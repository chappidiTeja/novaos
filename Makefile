LIMINE   ?= $(HOME)/os/limine
KERNEL   := target/x86_64-unknown-none/release/kernel
ISO      := novaos.iso

.PHONY: all kernel iso run test clean

all: iso

kernel:
	cargo build --release

$(LIMINE)/limine:
	$(MAKE) -C $(LIMINE)

iso: kernel $(LIMINE)/limine
	rm -rf iso_root
	mkdir -p iso_root/boot/limine iso_root/EFI/BOOT
	cp $(KERNEL) iso_root/boot/kernel
	cp limine.conf $(LIMINE)/limine-bios.sys $(LIMINE)/limine-bios-cd.bin $(LIMINE)/limine-uefi-cd.bin iso_root/boot/limine/
	cp $(LIMINE)/BOOTX64.EFI $(LIMINE)/BOOTIA32.EFI iso_root/EFI/BOOT/
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table -hfsplus \
		-apm-block-size 2048 --efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		iso_root -o $(ISO)
	$(LIMINE)/limine bios-install $(ISO)

# Interactive: serial output goes to this terminal. Quit with Ctrl-A then X.
run: iso
	qemu-system-x86_64 -cdrom $(ISO) -m 512M -serial stdio -display none -no-reboot

# Non-interactive: boot for a few seconds, then check the serial log.
test: iso
	-timeout 10 qemu-system-x86_64 -cdrom $(ISO) -m 512M -serial file:serial.log -display none -no-reboot
	@cat serial.log
	@grep -q "BREAKPOINT" serial.log && grep -q "\[ok\] timer" serial.log \
		&& grep -q "\[ok\] pmm self-test" serial.log && grep -q "\[ok\] vmm self-test" serial.log \
		&& grep -q "\[ok\] heap self-test" serial.log \
		&& grep -q "\[ok\] scheduler self-test" serial.log \
		&& ! grep -q "KERNEL PANIC" serial.log && echo "TEST PASSED" || (echo "TEST FAILED"; exit 1)

clean:
	cargo clean
	rm -rf iso_root $(ISO) serial.log
