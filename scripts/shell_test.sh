#!/bin/bash
# End-to-end test of NovaOS through its shell. Boots the OS with the serial port
# wired to named pipes, types commands as a user would, and checks the output.
# It exercises: serial input interrupts, the console, spawn/waitpid, programs
# loaded from disk, pipes between processes, redirection, the file syscalls,
# and finally the disk image the whole session left behind.
#
# usage: shell_test.sh [novaos.iso] [disk.img] [mkfs-tool]

ISO=${1:-novaos.iso}
DISK=${2:-build/disk.img}
MKFS=${3:-tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs}
dir=$(mktemp -d)
log="$dir/out.log"
trap 'exec 3>&- 2>/dev/null; kill $QEMU_PID $CAT_PID 2>/dev/null; rm -rf "$dir"' EXIT

cp "$DISK" "$dir/disk.img"
mkfifo "$dir/chan.in" "$dir/chan.out"
qemu-system-x86_64 -cdrom "$ISO" -m 512M -display none -no-reboot \
    -drive "file=$dir/disk.img,if=none,id=disk0,format=raw" \
    -device virtio-blk-pci,drive=disk0,disable-modern=on \
    -serial "pipe:$dir/chan" >/dev/null 2>&1 &
QEMU_PID=$!
cat "$dir/chan.out" >"$log" &
CAT_PID=$!
exec 3>"$dir/chan.in"

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }

prompt_count() { grep -aEc 'nova:[^ ]*\$ ' "$log"; }

wait_for_prompts() { # number of prompts to wait for, seconds
    for _ in $(seq 1 $(($2 * 10))); do
        [ "$(prompt_count)" -ge "$1" ] && return 0
        grep -aq "KERNEL PANIC" "$log" && return 1
        sleep 0.1
    done
    return 1
}

# run COMMAND [EXPECTED-LINE...]: type the command, wait for the shell to come
# back, and require every expected line to appear exactly (as a whole line) in
# what the command printed. Lines starting with ~ are extended regular expressions.
run() {
    local command=$1
    shift
    local seen before output
    seen=$(prompt_count)
    before=$(stat -c %s "$log")
    printf '%s\r' "$command" >&3
    if ! wait_for_prompts $((seen + 1)) 20; then
        # The shell is stuck (a pipe that never ends, say); nothing after this
        # command can be trusted, so stop instead of timing out on every one.
        fail "'$command': the shell never came back"
        tail -5 "$log"
        echo "SHELL TEST FAILED: $failures problem(s); stopped at the first hang"
        exit 1
    fi
    output=$(tail -c +$((before + 1)) "$log" | tr -d '\r')
    local expected
    for expected in "$@"; do
        if [[ "$expected" == "~"* ]]; then
            echo "$output" | grep -aEq -- "${expected#\~}" || fail "'$command': no line matching /${expected#\~}/"
        else
            echo "$output" | grep -axF -- "$expected" >/dev/null || fail "'$command': missing line '$expected'"
        fi
    done
}

# run_absent COMMAND UNWANTED: like run, but the line must NOT appear.
run_absent() {
    local command=$1 unwanted=$2 seen before output
    seen=$(prompt_count)
    before=$(stat -c %s "$log")
    printf '%s\r' "$command" >&3
    wait_for_prompts $((seen + 1)) 60 || { fail "'$command': the shell never came back"; return; }
    output=$(tail -c +$((before + 1)) "$log" | tr -d '\r')
    echo "$output" | grep -axF -- "$unwanted" >/dev/null && fail "'$command': unexpected line '$unwanted'"
}

if ! wait_for_prompts 1 120; then
    echo "SHELL TEST FAILED: the shell never started"
    tail -8 "$log"
    exit 1
fi

# What the large pipeline should count, computed independently of the OS.
read -r PL PW PB < <(python3 -c "
data = bytes((i ^ (i>>8) ^ (i>>16)) & 255 for i in range(1572864))
print(data.count(b'\n'), len(data.split()), len(data))")

run "echo hello nova"                 "hello nova"
run "echo \"a   b\" 'c d'"            "a   b c d"
run "cat /hello.txt"                  "Hello from NovaFS!"
run "ls /"                            "bin/" "docs/" "etc/" "hello.txt" "test/" "tmp/" "var/"
run "ls -l /docs"                     "~^d +[0-9]+ notes/$" "~^- +[0-9]+ readme.txt$"
run "echo piped | wc"                 "1 1 6"
run "cat /hello.txt | cat | wc"       "1 3 19"
run "echo redirected > /tmp/r.txt"
run "cat /tmp/r.txt"                  "redirected"
run "echo more >> /tmp/r.txt"
run "cat /tmp/r.txt | wc"             "2 2 16"
run "wc < /tmp/r.txt"                 "2 2 16"
run "cat /test/pattern.bin | wc"      "$PL $PW $PB"
run "cat /nonexistent"                "cat: /nonexistent: No such file or directory" "[exit 1]"
run "nosuchcmd"                       "sh: nosuchcmd: command not found"
run "echo lost | nosuchcmd"           "sh: nosuchcmd: command not found"
run "mkdir /tmp/dd"
run "cd /tmp/dd"
run "pwd"                             "/tmp/dd"
run "touch a b"
run "ls"                              "a" "b"
run "cp /hello.txt c"
run "cat c"                           "Hello from NovaFS!"
run "cd .."
run "pwd"                             "/tmp"
run "rmdir dd"                        "rmdir: dd: Directory not empty"
run "rm dd/a dd/b dd/c"
run "rmdir dd"
run_absent "ls /tmp"                  "dd/"
run "rm /tmp/r.txt"
run "ls /tmp"
run "rm /tmp/r.txt"                   "rm: /tmp/r.txt: No such file or directory"

# Shut down QEMU abruptly, as a power cut would, and check what is on the disk.
exec 3>&-
kill $QEMU_PID 2>/dev/null
wait $QEMU_PID 2>/dev/null
if [ -x "$MKFS" ]; then
    "$MKFS" --check "$dir/disk.img" || fail "the disk image is inconsistent after the session"
fi

if [ "$failures" -eq 0 ]; then
    echo "SHELL TEST PASSED: all commands behaved, and the disk is consistent"
    exit 0
fi
echo "SHELL TEST FAILED: $failures problem(s)"
exit 1
