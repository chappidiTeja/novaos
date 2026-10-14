#!/bin/bash
# Power-cut experiment. Boots NovaOS and kills QEMU at a random moment (as pulling
# the plug would), then asks the host-side tool whether the disk is still a
# consistent NovaFS volume. Repeats and reports how often it was.
#
# NovaFS has no journal, so a cut in the middle of a multi-block update (creating
# a file, say) can leave the volume inconsistent. This script measures that; it
# does not fail the build. A journaled filesystem would score 100%.
#
# usage: crash_test.sh [novaos.iso] [disk.img] [mkfs-tool] [rounds] [max-seconds] [marker]
#
# Without a marker the cut comes 1..max-seconds after power-on, which mostly
# lands outside the short bursts of disk writes. With a marker (a line the
# kernel prints, e.g. "disk self-test") the cut comes 0..max-seconds after that
# line appears, so it can be aimed at a write-heavy phase.

ISO=${1:-novaos.iso}
DISK=${2:-build/disk.img}
MKFS=${3:-tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs}
ROUNDS=${4:-20}
MAXSEC=${5:-14}
MARKER=${6:-}
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

consistent=0
repaired=0
broken=0
for round in $(seq 1 "$ROUNDS"); do
    cp "$DISK" "$dir/disk.img"
    qemu-system-x86_64 -cdrom "$ISO" -m 512M -display none -no-reboot \
        -drive "file=$dir/disk.img,if=none,id=disk0,format=raw" \
        -device virtio-blk-pci,drive=disk0,disable-modern=on \
        -serial "file:$dir/serial.log" >/dev/null 2>&1 &
    pid=$!
    if [ -n "$MARKER" ]; then
        # Wait for the kernel to reach the marker, then cut after a random moment.
        for _ in $(seq 1 1200); do
            grep -aq "$MARKER" "$dir/serial.log" 2>/dev/null && break
            sleep 0.05
        done
        sleep "$(awk -v max="$MAXSEC" -v seed="$RANDOM$round" 'BEGIN { srand(seed); printf "%.2f", rand() * max }')"
    else
        sleep "$(awk -v max="$MAXSEC" -v seed="$RANDOM$round" 'BEGIN { srand(seed); printf "%.1f", 1 + rand() * (max - 1) }')"
    fi
    kill -9 "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    if "$MKFS" --check "$dir/disk.img" >"$dir/check.out" 2>&1; then
        if grep -q "repaired" "$dir/check.out"; then
            repaired=$((repaired + 1))
            echo "round $round: stale counters, repaired at mount"
        else
            consistent=$((consistent + 1))
        fi
    else
        broken=$((broken + 1))
        echo "round $round: $(head -1 "$dir/check.out" | sed "s#$dir/##")"
    fi
done
echo "power cuts: $consistent clean, $repaired needed only a counter repair at mount, $broken left the volume inconsistent (of $ROUNDS)"
