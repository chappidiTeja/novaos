#!/bin/bash
# End-to-end test of NovaOS through its shell. Boots the OS with the serial port wired to
# named pipes, types commands (and raw keystrokes) as a user would, and checks the output.
# It exercises: serial input interrupts, the console, the line editor, the shell language
# (variables, expansions, control flow, functions, wildcards, redirections, scripts), job
# control and signals (Control-C, kill), spawn/waitpid, programs loaded from disk, pipes,
# the file syscalls, and finally the disk image the whole session left behind.
#
# usage: shell_test.sh [novaos.iso] [disk.img] [mkfs-tool]

ISO=${1:-novaos.iso}
DISK=${2:-build/disk.img}
MKFS=${3:-tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs}
here=$(cd "$(dirname "$0")" && pwd)
. "$here/shelllib.sh"
trap 'stop_os; rm -rf "$dir"' EXIT

boot_os "$ISO" "$DISK"
if ! wait_for_prompts 1 120; then
    echo "SHELL TEST FAILED: the shell never started"
    tail -8 "$log"
    exit 1
fi
boot_seconds=$(awk -v a="$boot_started" -v b="$(date +%s.%N)" 'BEGIN { printf "%.1f", b - a }')
echo "boot to shell prompt: ${boot_seconds} s (QEMU software emulation, no KVM)"
grep -aq "self-tests enabled" "$log" && fail "the normal boot entry ran the self-tests"
grep -aq "NovaOS shell. Type .help. for commands." "$log" || fail "no banner"

# What the large pipeline should count, computed independently of the OS.
read -r PL PW PB < <(python3 -c "
data = bytes((i ^ (i>>8) ^ (i>>16)) & 255 for i in range(1572864))
print(data.count(b'\n'), len(data.split()), len(data))")

# ---- the basics, as before -----------------------------------------------------------------------
run "echo hello nova"                 "hello nova"
run "echo \"a   b\" 'c d'"            "a   b c d"
run "cat /hello.txt"                  "Hello from NovaFS!"
run "ls /"                            "bin/" "docs/" "etc/" "hello.txt" "test/" "tmp/" "var/" "www/"
run "ls -l /docs"                     "~^d +[0-9]+ notes/$" "~^- +[0-9]+ readme.txt$"
run "echo piped | wc"                 "1 1 6"
run "cat /hello.txt | cat | wc"       "1 3 19"
run "echo redirected > /tmp/r.txt"
run "cat /tmp/r.txt"                  "redirected"
run "echo more >> /tmp/r.txt"
run "cat /tmp/r.txt | wc"             "2 2 16"
run "wc < /tmp/r.txt"                 "2 2 16"
run "cat /test/pattern.bin | wc"      "$PL $PW $PB"
run "cat /nonexistent"                "cat: /nonexistent: No such file or directory"
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
run "rm /tmp/r.txt"                   "rm: /tmp/r.txt: No such file or directory"

# ---- variables and expansion -----------------------------------------------------------------------
run "X=hello"
run "echo \$X"                        "hello"
run "echo \"\${X}_world\""            "hello_world"
run "echo '\$X'"                      "\$X"
run "echo \${UNSET:-fallback}"        "fallback"
run "echo \${X:+set}"                 "set"
run "echo \${#X}"                     "5"
run "echo \$((6*7))"                  "42"
run "echo \$(( (1+2) * 3 - 4 ))"      "5"
run "echo \$(echo nested)"            "nested"
run "echo \"count: \$(ls /docs | wc)\"" "~^count: 2 +2 "
run "Y=\"two words\""
run "echo \$Y | wc"                   "1 2 10"
run "echo \"\$Y\" | wc"               "1 2 10"
run "cd /tmp"
run "echo \$PWD"                      "/tmp"
run "cd -"                            "/tmp"
run "echo \$PWD"                      "/tmp"
run "echo ~"                          "/"
run "echo \$?"                        "0"
run "false"
run "echo \$?"                        "1"
run "echo a; echo b"                  "a" "b"
run "true && echo yes"                "yes"
run "false || echo fallback"          "fallback"
run_absent "false && echo never"      "never"
run "! false && echo negated"         "negated"
run "echo 'it''s'"                    "its"
run "echo a\\ b | wc"                 "1 2 4"
run "echo \"line1
line2\" | wc"                         "2 2 12"

# ---- wildcards -------------------------------------------------------------------------------------------
run "echo /h*"                        "/hello.txt"
run "echo /d*"                        "/docs"
run "echo /b?n"                       "/bin"
run "echo /[bd]*s"                    "/docs"
run "echo /nomatch*"                  "/nomatch*"
run "echo '/h*'"                      "/h*"
run "echo /docs/*"                    "/docs/notes /docs/readme.txt"
run "touch /tmp/w1.t /tmp/w2.t /tmp/other"
run "echo /tmp/w*.t"                  "/tmp/w1.t /tmp/w2.t"
run "rm /tmp/w1.t /tmp/w2.t /tmp/other"

# ---- redirections ------------------------------------------------------------------------------------------
run "cat /nonexistent 2> /tmp/err.txt"
run "cat /tmp/err.txt"                "cat: /nonexistent: No such file or directory"
run "cat /nonexistent > /tmp/both.txt 2>&1"
run "cat /tmp/both.txt"               "cat: /nonexistent: No such file or directory"
run "echo one > /tmp/f; echo two >> /tmp/f; echo three >> /tmp/f"
run "cat /tmp/f"                      "one" "two" "three"
run "cat < /tmp/f | wc"               "3 3 14"
run "rm /tmp/f /tmp/err.txt /tmp/both.txt"

# ---- control flow ----------------------------------------------------------------------------------------------
run "if true; then echo yes; else echo no; fi"                       "yes"
run "if false; then echo yes; else echo no; fi"                      "no"
run "if test -f /hello.txt; then echo exists; fi"                    "exists"
run "if [ -d /docs ] && [ ! -d /hello.txt ]; then echo dirs; fi"     "dirs"
run "if false; then echo a; elif true; then echo b; else echo c; fi" "b"
run "for i in 1 2 3; do echo n\$i; done"                             "n1" "n2" "n3"
run "for f in /docs/*; do echo item \$f; done"                       "item /docs/notes" "item /docs/readme.txt"
run "i=0; while [ \$i -lt 3 ]; do echo w\$i; i=\$((i+1)); done"      "w0" "w1" "w2"
run "i=3; until [ \$i -eq 0 ]; do echo u\$i; i=\$((i-1)); done"      "u3" "u2" "u1"
run "for i in 1 2 3 4; do if [ \$i -eq 3 ]; then break; fi; echo b\$i; done" "b1" "b2"
run "for i in 1 2 3; do if [ \$i -eq 2 ]; then continue; fi; echo c\$i; done" "c1" "c3"
run "for a in x y; do for b in 1 2; do echo \$a\$b; done; done"      "x1" "x2" "y1" "y2"
run "greet() { echo hello \$1; }"
run "greet nova"                      "hello nova"
run "f() { return 3; }; f; echo \$?"  "3"
run "g() { echo args=\$# first=\$1 all=\$@; }; g a 'b c' d"          "args=3 first=a all=a b c d"
run "fact() { if [ \$1 -le 1 ]; then echo 1; else n=\$(fact \$((\$1-1))); echo \$((\$1*n)); fi; }; fact 5" "120"
run "read a b < /hello.txt; echo \$b"  "from NovaFS!"
run "echo x y z | read p q; echo \$p-\$q"                            "x-y z"
run "printf '%s-%d\\n' abc 42"        "abc-42"
run "printf '%05d|%-4s|%x\\n' 42 ab 255" "00042|ab  |ff"
run "[ 3 -lt 10 ] && echo less"       "less"
run "[ \"a\" = \"b\" ] || echo differ" "differ"
run "test -z \"\" && echo empty"      "empty"

# ---- environment -----------------------------------------------------------------------------------------------------
run "export FOO=bar"
run "printenv FOO"                    "bar"
run "FOO=baz printenv FOO"            "baz"
run "printenv FOO"                    "bar"
run "BAR=1 printenv BAR"              "1"
run_absent "printenv BAR"             "1"
run "unset FOO"
run "printenv FOO"
run "env | cat"                       "~^PATH=/bin$"
run "type echo"                       "echo is a shell builtin"
run "type ls"                         "ls is /bin/ls"
run "type nope"                       "sh: type: nope: not found"
run "alias ll='ls -l'"
run "ll /docs"                        "~^d +[0-9]+ notes/$"
run "history"                         "~^ +[0-9]+  echo hello nova$"

# ---- scripts -----------------------------------------------------------------------------------------------------------------
run "echo 'echo script \$0 \$1 \$2 count \$#' > /tmp/s.sh"
run "sh /tmp/s.sh a b"                "script /tmp/s.sh a b count 2"
run "echo '#!/bin/sh' > /tmp/t.sh; echo 'echo direct \$1' >> /tmp/t.sh"
run "/tmp/t.sh run"                   "direct run"
run "sh -c 'echo \$0 \$1' name arg"   "name arg"
run "echo 'echo in-source \$#' > /tmp/src.sh"
run ". /tmp/src.sh x y"               "in-source 2"
run "source /tmp/src.sh"              "in-source 0"
run "eval 'echo evaluated'"           "evaluated"
run "rm /tmp/s.sh /tmp/t.sh /tmp/src.sh"

# ---- syntax errors and continuation ------------------------------------------------------------------------------------------
run_raw $'if true; then\recho in-if\rfi\r'   "in-if"      # the shell asks for the rest with "> "
run_raw $'echo "multi\rline"\r'               "multi" "line"   # an open quote continues on the next line
run "echo )"                          "sh: syntax error: unexpected ')'"
run "fi"                              "sh: syntax error: unexpected 'fi'"
run "echo \$((1/0))"                  "sh: arithmetic: division by zero" "0"

# ---- grep ------------------------------------------------------------------------------------------------------------------------
run "echo 'apple pie' > /tmp/g1; echo 'Banana split' >> /tmp/g1; echo 'cherry tart' >> /tmp/g1; echo 'apple 42' >> /tmp/g1"
run "grep apple /tmp/g1"              "apple pie" "apple 42"
run_absent "grep apple /tmp/g1"       "cherry tart"
run "grep -n apple /tmp/g1"           "1:apple pie" "4:apple 42"
run "grep -c apple /tmp/g1"           "2"
run "grep -v apple /tmp/g1"           "Banana split" "cherry tart"
run_absent "grep -v apple /tmp/g1"    "apple pie"
run "grep banana /tmp/g1; echo status \$?"  "status 1"
run "grep -i banana /tmp/g1"          "Banana split"
run "grep '^c.*t\$' /tmp/g1"          "cherry tart"
run "grep '[0-9][0-9]' /tmp/g1"       "apple 42"
run "grep -E 'pie|tart' /tmp/g1"      "apple pie" "cherry tart"
run "grep 'pie|tart' /tmp/g1"         "apple pie" "cherry tart"
run "cat /tmp/g1 | grep split"        "Banana split"
run "grep -l apple /tmp/g1"           "/tmp/g1"
run "grep apple /tmp/g1 /tmp/g1"      "/tmp/g1:apple pie"
run "grep '[' /tmp/g1; echo status \$?"  "~bad pattern" "status 2"
run "grep apple /tmp/nonexistent; echo status \$?"  "status 2"
run "echo 'x y' > /tmp/g2; grep -r 'x y' /tmp"  "/tmp/g2:x y"
run "rm /tmp/g1 /tmp/g2"

# ---- the line editor ----------------------------------------------------------------------------------------------------------------
run_raw $'echo abx\x7fc\r'            "abc"                       # backspace
run_raw $'echo ac\x1b[Db\r'           "abc"                       # left arrow, insert
run_raw $'echo bc\x1b[H\x1b[Fd\r'     "bcd"                       # home, end
run_raw $'x\x01echo \r'               "x"                         # Ctrl-A (home), then type in front
run_raw $'echo one two three\x17\x17four\r' "one four"            # Ctrl-W twice
run_raw $'echo discarded\x15echo kept\r'    "kept"                # Ctrl-U
run_raw $'echo abcdef\x1b[D\x1b[D\x1b[D\x0b\r' "abc"              # Ctrl-K
run "echo first"                      "first"
run_raw $'\x1b[A\r'                   "first"                     # up arrow repeats the last command
run_raw $'echo second\r'              "second"
run_raw $'\x1b[A\x1b[A\r'             "first"                     # two ups
run_raw $'cat /hel\t\r'               "Hello from NovaFS!"        # Tab completes a file name
run_raw $'ec\t hi\r'                  "hi"                        # ... and a command
run_raw $'abc\x03'                                                 # Ctrl-C abandons the line
run "echo after-interrupt"            "after-interrupt"
run_raw $'cd /do\t\r'
run "pwd"                             "/docs"
run "cd /"

# ---- jobs and signals --------------------------------------------------------------------------------------------------------------------
run "sleep 1 &"                       "~^\[1\] [0-9]+$"
run "jobs"                            "~^\[1\]  Running  sleep 1$"
run "wait"
run "jobs"
run "sleep 100 &"                     "~^\[[0-9]+\] [0-9]+$"
run "ps"                              "~sleep$"
run "kill %2"
run "wait"
run_absent "ps"                       "~sleep"
# Control-C stops the foreground command, not the shell.
seen=$(prompt_count); before=$(stat -c %s "$log")
send $'sleep 100\r'
sleep 1.5
send $'\x03'
wait_for_prompts $((seen + 1)) 10 || fail "Control-C did not bring the prompt back"
run "echo \$?"                        "130"
run_absent "ps"                       "~sleep"
# ... all of a pipeline:
seen=$(prompt_count)
send $'sleep 100 | sleep 100\r'
sleep 1.5
send $'\x03'
wait_for_prompts $((seen + 1)) 10 || fail "Control-C did not stop a pipeline"
run_absent "ps"                       "~sleep"
# kill with a signal name and the exit status that results
run "sleep 100 &"                     "~^\[[0-9]+\] [0-9]+$"
run "kill -KILL %3"
run "wait"
run "kill -l"                         "HUP INT QUIT KILL USR1 USR2 ALRM TERM"
run "kill 999999"                     "sh: kill: 999999: No such process"
run "ps"                              "~^ +[0-9]+ +[0-9]+ [RSZ] .* sh$"

# ---- a program that crashes does not take the shell with it ------------------------------------------------------------------------------
run "echo still alive"                "still alive"

# Shut down QEMU abruptly, as a power cut would, and check what is on the disk.
stop_os
if [ -x "$MKFS" ]; then
    "$MKFS" --check "$dir/disk.img" || fail "the disk image is inconsistent after the session"
fi

if [ "$failures" -eq 0 ]; then
    echo "SHELL TEST PASSED: all commands behaved, and the disk is consistent"
    exit 0
fi
echo "SHELL TEST FAILED: $failures problem(s)"
exit 1
