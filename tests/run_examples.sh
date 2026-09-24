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

# The web server runs until it's stopped: start each build on a free port,
# send it some requests and compare what comes back
http_transcript() {
    local log="$WORK/server.log"
    "$1" 0 > "$log" 2>&1 &
    local pid=$!
    local port=""
    for _ in $(seq 50); do
        port="$(grep -o 'localhost:[0-9]*' "$log" | cut -d: -f2)"
        [ -n "$port" ] && break
        sleep 0.1
    done
    if [ -n "$port" ]; then
        local url="http://localhost:$port"
        curl -s "$url/"; curl -s "$url/"
        curl -s "$url/hello/Azula"
        curl -s -A test-agent "$url/agent"
        curl -s -d 'posted body' "$url/echo"; echo
        curl -si "$url/missing" | head -1
        curl -s -X DELETE "$url/"
    fi
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    sed "s/localhost:[0-9]*/localhost:PORT/" "$log"
}
if command -v curl > /dev/null; then
    src=http_server/main.azl
    if ! "$STAGE0" build "$src" > "$WORK/log" 2>&1 || ! mv http_server/main "$WORK/a.out"; then
        echo "FAIL $src (Rust compiler)"; sed 's/^/    /' "$WORK/log" | head -10
        fail=$((fail + 1))
    elif ! "$SELF_HOSTED" build "$src" -o "$WORK/b.out" > "$WORK/log" 2>&1; then
        echo "FAIL $src (self-hosted compiler)"; sed 's/^/    /' "$WORK/log" | head -10
        fail=$((fail + 1))
    else
        expected="$(http_transcript "$WORK/a.out")"
        actual="$(http_transcript "$WORK/b.out")"
        if [ "$expected" != "$actual" ] || [[ "$expected" != *"Hello, Azula!"* ]]; then
            echo "FAIL $src (responses differ or are wrong)"
            diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") | head -10 | sed 's/^/    /'
            fail=$((fail + 1))
        else
            pass=$((pass + 1))
        fi
    fi
else
    echo "skipping http_server/main.azl (needs curl)"
fi

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
