#!/usr/bin/env bash
# Compile and run every program in tests/programs with the given compiler and
# compare its stdout against the matching .out file.
#
#   tests/run.sh [path/to/azula]        (defaults to target/debug/azula)
#   UPDATE=1 tests/run.sh               (regenerate the .out files)
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPILER="$(realpath "${1:-$ROOT/target/debug/azula}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
for src in "$ROOT"/tests/programs/*.azl; do
    name="$(basename "$src" .azl)"
    expected="${src%.azl}.out"
    cp "$src" "$WORK/$name.azl"
    if ! (cd "$WORK" && "$COMPILER" build "$name.azl" > "$WORK/$name.build.log" 2>&1); then
        echo "FAIL $name (compile error)"
        sed 's/^/    /' "$WORK/$name.build.log" | head -20
        fail=$((fail + 1))
        continue
    fi
    actual="$(cd "$WORK" && timeout 10 "./$name" 2>&1)"
    if [ "${UPDATE:-0}" = "1" ]; then
        printf '%s\n' "$actual" > "$expected"
    fi
    if [ "$actual" == "$(cat "$expected" 2>/dev/null)" ]; then
        pass=$((pass + 1))
    else
        echo "FAIL $name (output differs)"
        diff <(printf '%s\n' "$actual") "$expected" | head -20 | sed 's/^/    /'
        fail=$((fail + 1))
    fi
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
