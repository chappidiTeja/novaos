LIMINE   ?= $(HOME)/os/limine
KERNEL   := target/x86_64-unknown-none/release/kernel
ISO      := novaos.iso
TEST_ISO := novaos-test.iso

USER_OUT  := user/target/x86_64-unknown-none/release
USER_ELF  := build/hello.elf
# Programs that go to /bin on the disk. `hello` is also embedded in the kernel for its tests.
USER_BINS := hello sh ls cat echo mkdir rm rmdir touch wc cp
MKFS      := tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs
DISK      := build/disk.img

# The OS disk: a virtio block device, legacy interface only.
QEMU_DISK = -drive file=$(1),if=none,id=disk0,format=raw -device virtio-blk-pci,drive=disk0,disable-modern=on

.PHONY: all user kernel iso mkfs rootfs disk run test test-shell test-persistence test-crash test-fs clean

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

# The disk builder runs on the build machine and shares the kernel's filesystem code.
mkfs:
	cd tools/mkfs && RUSTFLAGS="" cargo build --release --target x86_64-unknown-linux-gnu

# Stage the files that go on the disk: static ones from rootfs/, plus built programs
# and a generated test file.
rootfs: user
	rm -rf build/rootfs
	mkdir -p build/rootfs/bin build/rootfs/tmp build/rootfs/var build/rootfs/test build/rootfs/etc build/rootfs/docs
	cp -r rootfs/. build/rootfs/
	find build/rootfs -type f \( -name '*.txt' -o -name motd \) -exec sed -i 's/\r$$//' {} +
	for b in $(USER_BINS); do cp $(USER_OUT)/$$b build/rootfs/bin/$$b; done
	python3 -c "import sys; sys.stdout.buffer.write(bytes((i ^ (i>>8) ^ (i>>16)) & 255 for i in range(1572864)))" > build/rootfs/test/pattern.bin

disk: mkfs rootfs
	$(MKFS) $(DISK) build/rootfs --size-mib 64

# Interactive: serial output goes to this terminal. Quit with Ctrl-A then X.
# Changes made to the disk persist in build/disk-run.img between runs.
run: iso disk
	test -f build/disk-run.img || cp $(DISK) build/disk-run.img
	qemu-system-x86_64 -cdrom $(ISO) -m 512M $(call QEMU_DISK,build/disk-run.img) -serial stdio -display none -no-reboot

# Non-interactive: boot on a fresh copy of the disk, then check the serial log.
test: iso disk
	cp $(DISK) build/disk-test.img
	-timeout 40 qemu-system-x86_64 -cdrom $(TEST_ISO) -m 512M $(call QEMU_DISK,build/disk-test.img) -serial file:serial.log -display none -no-reboot
	@cat serial.log
	@grep -q "BREAKPOINT" serial.log && grep -q "\[ok\] timer" serial.log \
		&& grep -q "\[ok\] pmm self-test" serial.log && grep -q "\[ok\] vmm self-test" serial.log \
		&& grep -q "\[ok\] heap self-test" serial.log \
		&& grep -q "\[ok\] scheduler self-test" serial.log \
		&& grep -q "\[ok\] ipc self-test" serial.log \
		&& grep -q "\[ok\] disk self-test" serial.log \
		&& grep -q "hello from user mode" serial.log \
		&& grep -q "\[ok\] user-mode self-test" serial.log \
		&& grep -q "NovaOS shell" serial.log \
		&& ! grep -q "KERNEL PANIC" serial.log && echo "TEST PASSED" || (echo "TEST FAILED"; exit 1)

# Drives the shell over the serial port: runs commands, checks their output, and
# afterwards checks the disk image they left behind.
test-shell: iso disk
	bash scripts/shell_test.sh $(ISO) $(DISK) $(MKFS)

# Boots twice from one disk image: the second boot must see the first one's writes.
test-persistence: iso disk
	bash scripts/persistence_test.sh $(TEST_ISO) $(DISK) $(MKFS)

# Power-cut experiment: kills the machine at random moments and reports how often
# the disk survives (informational; NovaFS has no journal yet).
test-crash: iso disk
	bash scripts/crash_test.sh $(TEST_ISO) $(DISK) $(MKFS) 30 1.2 "disk self-test"

# The filesystem's own unit tests, on the build machine.
test-fs:
	cd fs && RUSTFLAGS="" cargo test --release --target x86_64-unknown-linux-gnu

clean:
	cargo clean
	cd user && cargo clean
	cd tools/mkfs && cargo clean
	cd fs && cargo clean
	rm -rf iso_root build $(ISO) $(TEST_ISO) serial.log
