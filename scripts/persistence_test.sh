#!/bin/bash
# Boots NovaOS twice from the same disk image. The kernel keeps a boot counter
# and a log on the disk, so the second boot proves that what the first one
# wrote really reached the disk and survives a restart.
#
# usage: persistence_test.sh [novaos.iso] [disk.img]

ISO=${1:-novaos.iso}
DISK=${2:-build/disk.img}
MKFS=${3:-tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs}
dir=$(mktemp -d)
trap 'kill $QEMU_PID 2>/dev/null; rm -rf "$dir"' EXIT
cp "$DISK" "$dir/disk.img"

boot() { # log-file
    qemu-system-x86_64 -cdrom "$ISO" -m 512M -display none -no-reboot \
        -drive "file=$dir/disk.img,if=none,id=disk0,format=raw" \
        -device virtio-blk-pci,drive=disk0,disable-modern=on \
        -serial "file:$1" >/dev/null 2>&1 &
    QEMU_PID=$!
    # Wait until every boot-time test has finished and the shell is waiting for
    # input: the disk is idle then. (NovaFS has no journal, so stopping the machine
    # in the middle of a write can leave the volume inconsistent; see below.)
    for _ in $(seq 1 1800); do
        grep -aq "NovaOS shell" "$1" 2>/dev/null && break
        grep -aq "KERNEL PANIC" "$1" 2>/dev/null && break
        sleep 0.1
    done
    sleep 1
    kill $QEMU_PID 2>/dev/null
    wait $QEMU_PID 2>/dev/null
}

boot "$dir/first.log"
boot "$dir/second.log"

first=$(grep -a "disk: boot #" "$dir/first.log")
second=$(grep -a "disk: boot #" "$dir/second.log")
echo "first boot : $first"
echo "second boot: $second"

# The image the kernel left behind (QEMU was killed, not shut down) must still be
# a perfectly consistent volume when mounted by the host-side tool.
if [ -x "$MKFS" ]; then
    "$MKFS" --check "$dir/disk.img" || { echo "PERSISTENCE TEST FAILED: image inconsistent"; exit 1; }
fi

if echo "$first" | grep -q "boot #1 (log has 1 lines)" \
    && echo "$second" | grep -q "boot #2 (log has 2 lines)"; then
    echo "PERSISTENCE TEST PASSED: the second boot saw the first boot's writes"
    exit 0
fi
echo "PERSISTENCE TEST FAILED"
tail -5 "$dir/second.log"
exit 1
