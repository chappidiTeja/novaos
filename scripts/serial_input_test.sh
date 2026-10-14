#!/bin/bash
# Boots NovaOS with its serial port wired to named pipes, waits for the console
# demo's prompt, types a line as if from a terminal, and checks that the process
# running inside the OS received it. This exercises the UART receive interrupt,
# the console buffer, and a blocking read(0) in user mode, end to end.
#
# usage: serial_input_test.sh [path/to/novaos.iso]

ISO=${1:-novaos.iso}
LINE=${2:-hello nova}
dir=$(mktemp -d)
log="$dir/out.log"
trap 'kill $QEMU_PID $CAT_PID 2>/dev/null; rm -rf "$dir"' EXIT

mkfifo "$dir/chan.in" "$dir/chan.out"
qemu-system-x86_64 -cdrom "$ISO" -m 512M -display none -no-reboot \
    -serial "pipe:$dir/chan" >/dev/null 2>&1 &
QEMU_PID=$!
cat "$dir/chan.out" >"$log" &
CAT_PID=$!

wait_for() { # pattern, seconds
    for _ in $(seq 1 $(($2 * 10))); do
        grep -aq "$1" "$log" && return 0
        sleep 0.1
    done
    return 1
}

if ! wait_for "type a line and press Enter" 90; then
    echo "SERIAL INPUT TEST FAILED: the prompt never appeared"
    tail -5 "$log"
    exit 1
fi

# A terminal sends carriage return for the Enter key.
printf '%s\r' "$LINE" >"$dir/chan.in"

if wait_for "you typed: $LINE" 20; then
    echo "SERIAL INPUT TEST PASSED: the OS received \"$LINE\" over the serial port"
    grep -a "you typed" "$log"
    exit 0
fi
echo "SERIAL INPUT TEST FAILED: no echo of the typed line"
tail -5 "$log"
exit 1
