#!/bin/bash
# Helpers for tests that drive NovaOS's shell over the serial port. Source this file.
#
#   boot_os ISO DISK [extra qemu args...]   start the OS; sets $dir $log $QEMU_PID $CAT_PID, fd 3
#   wait_for_prompts N SECONDS              wait until the shell has printed N prompts
#   run "command" [expected...]             type a command, wait for the next prompt, check the
#                                           output: every expected line must appear as a whole line;
#                                           one starting with ~ is an extended regular expression
#   run_absent "command" "unwanted line"    like run, but the line must not appear
#   stop_os                                 kill QEMU and clean up (call from a trap)
#
# $failures counts problems. A command that never returns stops the whole test.
# Set QEMU_ACCEL="-enable-kvm -cpu host" to use KVM.

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }

boot_os() {
    local iso=$1 disk=$2
    shift 2
    dir=$(mktemp -d)
    log="$dir/out.log"
    cp "$disk" "$dir/disk.img"
    mkfifo "$dir/chan.in" "$dir/chan.out"
    boot_started=$(date +%s.%N)
    qemu-system-x86_64 $QEMU_ACCEL -cdrom "$iso" -m 512M -display none -no-reboot \
        -drive "file=$dir/disk.img,if=none,id=disk0,format=raw" \
        -device virtio-blk-pci,drive=disk0,disable-modern=on \
        "$@" \
        -serial "pipe:$dir/chan" >/dev/null 2>&1 &
    QEMU_PID=$!
    cat "$dir/chan.out" >"$log" &
    CAT_PID=$!
    exec 3>"$dir/chan.in"
}

stop_os() {
    exec 3>&- 2>/dev/null
    kill $QEMU_PID $CAT_PID 2>/dev/null
    wait $QEMU_PID $CAT_PID 2>/dev/null
}

prompt_count() { grep -aEc 'nova:[^ ]*\$ ' "$log"; }

wait_for_prompts() {
    for _ in $(seq 1 $(($2 * 10))); do
        [ "$(prompt_count)" -ge "$1" ] && return 0
        grep -aq "KERNEL PANIC" "$log" && return 1
        sleep 0.1
    done
    return 1
}

run() {
    local command=$1
    shift
    local seen before output
    seen=$(prompt_count)
    before=$(stat -c %s "$log")
    printf '%s\r' "$command" >&3
    if ! wait_for_prompts $((seen + 1)) "${CMD_TIMEOUT:-30}"; then
        fail "'$command': the shell never came back"
        tail -5 "$log"
        echo "TEST FAILED: $failures problem(s); stopped at the first hang"
        exit 1
    fi
    output=$(tail -c +$((before + 1)) "$log" | tr -d '\r')
    LAST_OUTPUT=$output
    local expected failed_before=$failures
    for expected in "$@"; do
        if [[ "$expected" == "~"* ]]; then
            echo "$output" | grep -aEq -- "${expected#\~}" || fail "'$command': no line matching /${expected#\~}/"
        else
            echo "$output" | grep -axF -- "$expected" >/dev/null || fail "'$command': missing line '$expected'"
        fi
    done
    if [ "$failures" -ne "$failed_before" ]; then
        echo "---- output of '$command':"
        echo "$output" | tail -${SHOW_OUTPUT:-25} | sed 's/^/  | /'
        echo "----"
    fi
}

run_absent() {
    local command=$1 unwanted=$2 seen before output
    seen=$(prompt_count)
    before=$(stat -c %s "$log")
    printf '%s\r' "$command" >&3
    wait_for_prompts $((seen + 1)) "${CMD_TIMEOUT:-30}" || { fail "'$command': the shell never came back"; return; }
    output=$(tail -c +$((before + 1)) "$log" | tr -d '\r')
    echo "$output" | grep -axF -- "$unwanted" >/dev/null && fail "'$command': unexpected line '$unwanted'"
}

# ---- commands that need something to happen on the host while they run -----------------------
#   begin "cmd"                  type the command but do not wait for it
#   wait_cmd_line REGEX SECONDS  wait until the command has printed a line matching REGEX
#   finish [SECONDS]             wait for the prompt; $LAST_OUTPUT is everything it printed
begin() {
    CMD_SEEN=$(prompt_count)
    CMD_BEFORE=$(stat -c %s "$log")
    CMD_TEXT=$1
    printf '%s\r' "$1" >&3
}

cmd_output() { tail -c +$((CMD_BEFORE + 1)) "$log" | tr -d '\r'; }

wait_cmd_line() {
    for _ in $(seq 1 $(($2 * 10))); do
        cmd_output | grep -aEq -- "$1" && return 0
        grep -aq "KERNEL PANIC" "$log" && return 1
        sleep 0.1
    done
    return 1
}

finish() {
    if ! wait_for_prompts $((CMD_SEEN + 1)) "${1:-30}"; then
        fail "'$CMD_TEXT': the shell never came back"
        cmd_output | tail -8
        echo "TEST FAILED: $failures problem(s); stopped at the first hang"
        exit 1
    fi
    LAST_OUTPUT=$(cmd_output)
}

# ---- typing raw keystrokes (to test the line editor) ----------------------------------------------
#   run_raw "keys" [expected...]   send the bytes exactly as given (end them with $'\r'), wait for the
#                                  shell's next prompt, check the output like `run`
#   send "bytes"                   send bytes without waiting for anything
run_raw() {
    local keys=$1
    shift
    local seen before output expected failed_before=$failures label
    label=$(printf '%q' "$keys")
    seen=$(prompt_count)
    before=$(stat -c %s "$log")
    printf '%s' "$keys" >&3
    if ! wait_for_prompts $((seen + 1)) "${CMD_TIMEOUT:-30}"; then
        fail "keys $label: the shell never came back"
        tail -5 "$log"
        echo "TEST FAILED: $failures problem(s); stopped at the first hang"
        exit 1
    fi
    output=$(tail -c +$((before + 1)) "$log" | tr -d '\r')
    LAST_OUTPUT=$output
    for expected in "$@"; do
        if [[ "$expected" == "~"* ]]; then
            echo "$output" | grep -aEq -- "${expected#\~}" || fail "keys $label: no line matching /${expected#\~}/"
        else
            echo "$output" | grep -axF -- "$expected" >/dev/null || fail "keys $label: missing line '$expected'"
        fi
    done
    if [ "$failures" -ne "$failed_before" ]; then
        echo "---- output of keys $label:"
        echo "$output" | tail -${SHOW_OUTPUT:-25} | sed 's/^/  | /'
        echo "----"
    fi
}

send() { printf '%s' "$1" >&3; }
