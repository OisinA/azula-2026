#!/usr/bin/env bash
# Check that each program in tests/errors fails to compile with the error
# given in its first line (`// error: <expected text>`). Tests can import
# modules from tests/errors/lib.
#
#   tests/run_errors.sh [path/to/azula]    (defaults to build/azula)
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPILER="$(realpath "${1:-$ROOT/build/azula}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -r "$ROOT/tests/errors/lib" "$WORK/lib"

pass=0
fail=0
for src in "$ROOT"/tests/errors/*.azl; do
    name="$(basename "$src" .azl)"
    expected="$(head -1 "$src" | sed 's|^// error: ||')"
    cp "$src" "$WORK/$name.azl"
    if output="$(cd "$WORK" && "$COMPILER" build "$name.azl" 2>&1)"; then
        echo "FAIL $name (compiled successfully)"
        fail=$((fail + 1))
    elif [[ "$output" != *"$expected"* ]]; then
        echo "FAIL $name (expected \"$expected\")"
        printf '%s\n' "$output" | head -5 | sed 's/^/    /'
        fail=$((fail + 1))
    else
        pass=$((pass + 1))
    fi
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
