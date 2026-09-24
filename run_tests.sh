#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"
cargo test --workspace
cargo build
tests/run.sh target/debug/azula
