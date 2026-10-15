LIMINE   ?= $(HOME)/os/limine
KERNEL   := target/x86_64-unknown-none/release/kernel
ISO      := novaos.iso
TEST_ISO := novaos-test.iso
DESKTOP_ISO := novaos-desktop.iso

USER_OUT  := user/target/x86_64-unknown-none/release
USER_ELF  := build/hello.elf
# Programs that go to /bin on the disk. `hello` is also embedded in the kernel for its tests.
USER_BINS := hello sh ls cat echo mkdir rm rmdir touch wc grep cp gfxtest desktop term gdemo ifconfig ping nslookup nettest fetch httpd nc cksum ps kill sleep printenv
MKFS      := tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs
DISK      := build/disk.img

# The OS disk: a virtio block device, legacy interface only.
QEMU_DISK = -drive file=$(1),if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0,disable-modern=on

# A network card on QEMU's built-in user-mode network (DHCP gives 10.0.2.15, gateway 10.0.2.2).
QEMU_NET = -netdev user,id=net0 -device virtio-net-pci,netdev=net0,disable-modern=on

.PHONY: regress test-net test-net-stack all user kernel iso mkfs rootfs disk run test test-shell test-screen test-screen-selftest test-gfx test-user-libs test-desktop test-desktop-boot run-desktop test-persistence test-crash test-fs clean

all: iso

# User programs are a separate workspace; the kernel embeds `hello` for its tests.
# RUSTFLAGS replaces (rather than adds to) the kernel's rustflags from .cargo/config.toml.
user:
	mkdir -p build
	cd user && RUSTFLAGS="-Crelocation-model=static -Clink-arg=-T$(CURDIR)/user/linker.ld -Clink-arg=-zmax-page-size=0x1000" \
		cargo build --release --target x86_64-unknown-none
	cp $(USER_OUT)/hello $(USER_ELF)

kernel: user
	cargo build --release

$(LIMINE)/limine:
	$(MAKE) -C $(LIMINE)

# Build one bootable image: $(1) = image file, $(2) = Limine config to put on it.
define build_iso
	rm -rf iso_root
	mkdir -p iso_root/boot/limine iso_root/EFI/BOOT
	cp $(KERNEL) iso_root/boot/kernel
	cp $(2) iso_root/boot/limine/limine.conf
	cp $(LIMINE)/limine-bios.sys $(LIMINE)/limine-bios-cd.bin $(LIMINE)/limine-uefi-cd.bin iso_root/boot/limine/
	cp $(LIMINE)/BOOTX64.EFI $(LIMINE)/BOOTIA32.EFI iso_root/EFI/BOOT/
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table -hfsplus \
		-apm-block-size 2048 --efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		iso_root -o $(1)
	$(LIMINE)/limine bios-install $(1)
endef

# novaos.iso boots straight into the shell; novaos-test.iso first runs every self-test.
iso: kernel $(LIMINE)/limine
	$(call build_iso,$(ISO),limine.conf)
	$(call build_iso,$(TEST_ISO),limine-test.conf)
	$(call build_iso,$(DESKTOP_ISO),limine-desktop.conf)

# The disk builder runs on the build machine and shares the kernel's filesystem code.
mkfs:
	cd tools/mkfs && RUSTFLAGS="" cargo build --release --target x86_64-unknown-linux-gnu

# Stage the files that go on the disk: static ones from rootfs/, plus built programs
# and a generated test file.
rootfs: user
	rm -rf build/rootfs
	mkdir -p build/rootfs/bin build/rootfs/tmp build/rootfs/var build/rootfs/test build/rootfs/etc build/rootfs/docs
	cp -r rootfs/. build/rootfs/
	find build/rootfs -type f \( -name '*.txt' -o -name '*.html' -o -name '*.css' -o -name motd \) -exec sed -i 's/\r$$//' {} +
	for b in $(USER_BINS); do cp $(USER_OUT)/$$b build/rootfs/bin/$$b; done
	python3 -c "import sys; sys.stdout.buffer.write(bytes((i ^ (i>>8) ^ (i>>16)) & 255 for i in range(1572864)))" > build/rootfs/test/pattern.bin

disk: mkfs rootfs
	$(MKFS) $(DISK) build/rootfs --size-mib 64

# Interactive: serial output goes to this terminal. Quit with Ctrl-A then X.
# Changes made to the disk persist in build/disk-run.img between runs.
run: iso disk
	test -f build/disk-run.img || cp $(DISK) build/disk-run.img
	qemu-system-x86_64 -cdrom $(ISO) -m 512M $(call QEMU_DISK,build/disk-run.img) $(QEMU_NET) -serial stdio -display none -no-reboot

# The same, but straight into the graphical desktop, in a window (needs a machine
# with a display; the build server has none).
run-desktop: iso disk
	test -f build/disk-run.img || cp $(DISK) build/disk-run.img
	qemu-system-x86_64 -cdrom $(DESKTOP_ISO) -m 512M $(call QEMU_DISK,build/disk-run.img) -serial stdio -display gtk -no-reboot

# Non-interactive: boot on a fresh copy of the disk, then check the serial log.
test: iso disk
	cp $(DISK) build/disk-test.img
	-timeout 60 qemu-system-x86_64 -cdrom $(TEST_ISO) -m 512M $(call QEMU_DISK,build/disk-test.img) $(QEMU_NET) -serial file:serial.log -display none -no-reboot
	@cat serial.log
	@grep -q "BREAKPOINT" serial.log && grep -q "\[ok\] timer" serial.log \
		&& grep -q "\[ok\] pmm self-test" serial.log && grep -q "\[ok\] vmm self-test" serial.log \
		&& grep -q "\[ok\] heap self-test" serial.log \
		&& grep -q "\[ok\] scheduler self-test" serial.log \
		&& grep -q "\[ok\] ipc self-test" serial.log \
		&& grep -q "\[ok\] disk self-test" serial.log \
		&& grep -q "\[ok\] net self-test" serial.log \
		&& grep -q "hello from user mode" serial.log \
		&& grep -q "\[ok\] user-mode self-test" serial.log \
		&& grep -q "\[ok\] gfx: screen released after exit and after a crash; a second run leaks nothing" serial.log \
		&& grep -q "NovaOS shell" serial.log \
		&& ! grep -q "KERNEL PANIC" serial.log && echo "TEST PASSED" || (echo "TEST FAILED"; exit 1)

# Drives the shell over the serial port: runs commands, checks their output, and
# afterwards checks the disk image they left behind.
test-shell: iso disk
	bash scripts/shell_test.sh $(ISO) $(DISK) $(MKFS)

# Boots twice from one disk image: the second boot must see the first one's writes.
# Takes real screenshots of the running OS and checks every pixel (see scripts/screen_test.py).
# The second form boots the self-test image, whose long output scrolls the screen many times.
test-screen: iso disk
	python3 scripts/screen_test.py $(ISO) $(DISK) build/screens

test-screen-selftest: iso disk
	python3 scripts/screen_test.py $(TEST_ISO) $(DISK) build/screens-selftest

# The mouse, keyboard routing and screen mapping against the real emulated devices
# (QEMU monitor: mouse_move, mouse_button, sendkey, screendump).
test-gfx: iso disk
	python3 scripts/gfx_test.py $(ISO) $(DISK) build/gfx-screens

test-persistence: iso disk
	bash scripts/persistence_test.sh $(TEST_ISO) $(DISK) $(MKFS)

# Power-cut experiment: kills the machine at random moments and reports how often
# the disk survives (informational; NovaFS has no journal yet).
test-crash: iso disk
	bash scripts/crash_test.sh $(TEST_ISO) $(DISK) $(MKFS) 30 1.2 "disk self-test"

# The graphical desktop end to end: starts it, drives the emulated mouse and keyboard,
# and compares whole screenshots with an independently built picture.
test-desktop: iso disk
	python3 scripts/desktop_test.py $(ISO) $(DISK) build/desktop-screens

# The desktop boot image goes straight to the desktop, with no shell started first.
test-desktop-boot: iso disk
	python3 scripts/desktop_test.py --boot $(DESKTOP_ISO) $(DISK) build/desktop-screens

# Unit tests of the user-space libraries (shared types, drawing, window manager),
# run on the build machine.
test-user-libs:
	cd user && RUSTFLAGS="" cargo test --release --target x86_64-unknown-linux-gnu -p novaabi -p novaheap -p novashell -p novaregex -p gfx -p wm

# The filesystem's own unit tests, on the build machine.
test-fs:
	cd fs && RUSTFLAGS="" cargo test --release --target x86_64-unknown-linux-gnu

clean:
	cargo clean
	cd user && cargo clean
	cd tools/mkfs && cargo clean
	cd fs && cargo clean
	rm -rf iso_root build $(ISO) $(TEST_ISO) serial.log

# The network stack's own unit tests, on the build machine.
test-net-stack:
	cd net && RUSTFLAGS="" cargo test --release --target x86_64-unknown-linux-gnu

# The network end to end: boots with a card on QEMU's user network, runs ping, nslookup and the
# socket tests, with a UDP echo server and a DNS server running on the host (scripts/net_test.sh).
test-net: iso disk
	bash scripts/net_test.sh $(ISO) $(DISK)

# Every test target in turn, one verdict line each (10-20 minutes).
regress:
	bash scripts/regress.sh
