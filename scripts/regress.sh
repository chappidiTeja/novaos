#!/bin/bash
# Run every test target in turn and print one line per target plus a verdict.
#   scripts/regress.sh               all of them
#   scripts/regress.sh test-fs test  just these
# Full logs go to build/regress/<target>.log. Takes 10-20 minutes for everything.
# Run it detached on a slow machine:  nohup scripts/regress.sh > build/regress.result 2>&1 &

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cd "$(dirname "$0")/.." || exit 1
mkdir -p build/regress

ALL="test-fs test-net-stack test-user-libs test test-shell test-persistence test-net test-screen test-screen-selftest test-gfx test-desktop test-desktop-boot"
targets=${*:-$ALL}

echo "regression run started $(date '+%F %T')"
failed=0
for t in $targets; do
    start=$(date +%s)
    make "$t" > "build/regress/$t.log" 2>&1
    status=$?
    secs=$(( $(date +%s) - start ))
    if [ $status -eq 0 ]; then
        printf '  PASS  %-22s %4d s\n' "$t" "$secs"
    else
        printf '  FAIL  %-22s %4d s   (see build/regress/%s.log)\n' "$t" "$secs" "$t"
        grep -a -E '^FAIL|FAILED|panicked|^error' "build/regress/$t.log" | head -5 | sed 's/^/        /'
        failed=$((failed + 1))
    fi
done
if [ $failed -eq 0 ]; then
    echo "REGRESSION PASSED"
else
    echo "REGRESSION FAILED: $failed target(s)"
    exit 1
fi
