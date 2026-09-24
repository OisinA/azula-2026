#!/usr/bin/env bash
# Build and run every program in examples/ with the Rust compiler and the
# self-hosted compiler, checking both succeed and print the same output.
#
#   tests/run_examples.sh [rust-compiler] [self-hosted-compiler]
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE0="$(realpath "${1:-$ROOT/target/debug/azula}")"
SELF_HOSTED="$(realpath "${2:-$ROOT/build/azula}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -r "$ROOT/examples" "$WORK/examples"

pass=0
fail=0
cd "$WORK/examples"
for src in *.azl modules/main.azl; do
    name="${src%.azl}"
    if ! "$STAGE0" build "$src" > "$WORK/log" 2>&1 || ! mv "$name" "$WORK/a.out"; then
        echo "FAIL $src (Rust compiler)"; sed 's/^/    /' "$WORK/log" | head -10
        fail=$((fail + 1)); continue
    fi
    if ! "$SELF_HOSTED" build "$src" -o "$WORK/b.out" > "$WORK/log" 2>&1; then
        echo "FAIL $src (self-hosted compiler)"; sed 's/^/    /' "$WORK/log" | head -10
        fail=$((fail + 1)); continue
    fi
    if ! expected="$(timeout 30 "$WORK/a.out" 2>&1)"; then
        echo "FAIL $src (Rust-compiled program failed)"; fail=$((fail + 1)); continue
    fi
    actual="$(timeout 30 "$WORK/b.out" 2>&1)"
    if [ "$expected" != "$actual" ]; then
        echo "FAIL $src (outputs differ)"
        diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") | head -10 | sed 's/^/    /'
        fail=$((fail + 1)); continue
    fi
    pass=$((pass + 1))
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
