#!/usr/bin/env bash
# Bootstrap the self-hosted compiler in compiler/:
#
#   stage 0  the Rust compiler (cargo build)
#   stage 1  compiler/ built by stage 0
#   stage 2  compiler/ built by stage 1
#   stage 3  compiler/ built by stage 2
#
# Stages 2 and 3 are built by compilers made from the same source, so they
# must produce identical LLVM IR: the compiler has reached a fixed point.
# The resulting compiler is build/azula.
#
#   ./bootstrap.sh           bootstrap and check the fixed point
#   ./bootstrap.sh --test    also run tests/run.sh with every stage
set -euo pipefail
cd "$(dirname "$0")"

run_tests=0
if [ "${1:-}" = "--test" ]; then
    run_tests=1
fi

mkdir -p build
export AZULA_STDLIB="$PWD/stdlib"

echo "stage 0: building the Rust compiler"
cargo build --quiet
stage0=target/debug/azula

echo "stage 1: compiling compiler/ with stage 0"
"$stage0" build compiler/main.azl
mv compiler/main build/stage1

echo "stage 2: compiling compiler/ with stage 1"
build/stage1 build compiler/main.azl -o build/stage2 --emit-llvm

echo "stage 3: compiling compiler/ with stage 2"
build/stage2 build compiler/main.azl -o build/stage3 --emit-llvm

if cmp -s build/stage2.ll build/stage3.ll; then
    echo "fixed point reached: stage 2 and stage 3 generate identical LLVM IR ($(wc -l < build/stage3.ll) lines)"
else
    echo "error: stage 2 and stage 3 generate different LLVM IR"
    diff build/stage2.ll build/stage3.ll | head -20
    exit 1
fi
cp build/stage3 build/azula

if [ "$run_tests" = "1" ]; then
    for compiler in "$stage0" build/stage1 build/stage2 build/stage3; do
        echo "tests with $compiler:"
        tests/run.sh "$compiler"
    done
    echo "error tests with build/azula:"
    tests/run_errors.sh build/azula
    echo "examples with both compilers:"
    tests/run_examples.sh "$stage0" build/azula
    echo "garbage collector tests with build/azula:"
    tests/run_gc.sh build/azula
fi
