#!/usr/bin/env bash
# Run the garbage collector tests in tests/gc with the self-hosted compiler,
# once normally and once collecting every GC_STRESS allocations (default 10),
# comparing output against the .out files. Tests named *_nostress only run
# normally (they allocate too much to collect that often).
#
#   tests/run_gc.sh [path/to/azula]        (defaults to build/azula)
#   UPDATE=1 tests/run_gc.sh               (regenerate the .out files)
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPILER="$(realpath "${1:-$ROOT/build/azula}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# macOS has no timeout(1): fall back to perl's alarm
if ! command -v timeout > /dev/null; then
    timeout() { perl -e 'alarm shift; exec @ARGV' "$@"; }
fi

pass=0
fail=0
for src in "$ROOT"/tests/gc/*.azl; do
    name="$(basename "$src" .azl)"
    expected="${src%.azl}.out"
    if ! "$COMPILER" build "$src" -o "$WORK/$name" > "$WORK/log" 2>&1; then
        echo "FAIL $name (compile error)"; sed 's/^/    /' "$WORK/log" | head -10
        fail=$((fail + 1)); continue
    fi
    modes="normal"
    if [[ "$name" != *_nostress ]]; then
        modes="normal stress"
    fi
    for mode in $modes; do
        if [ "$mode" = "stress" ]; then
            actual="$(AZULA_GC_STRESS="${GC_STRESS:-10}" timeout 300 "$WORK/$name" 2>&1)"
        else
            actual="$(timeout 120 "$WORK/$name" 2>&1)"
        fi
        if [ "${UPDATE:-0}" = "1" ] && [ "$mode" = "normal" ]; then
            printf '%s\n' "$actual" > "$expected"
        fi
        if [ "$actual" == "$(cat "$expected" 2>/dev/null)" ]; then
            pass=$((pass + 1))
        else
            echo "FAIL $name ($mode)"
            diff <(printf '%s\n' "$actual") "$expected" | head -10 | sed 's/^/    /'
            fail=$((fail + 1))
        fi
    done
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
