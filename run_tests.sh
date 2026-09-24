#!/usr/bin/env bash
# Unit tests for the Rust compiler, then a full bootstrap of the self-hosted
# compiler with the program tests run against every stage.
set -e
cd "$(dirname "$0")"
cargo test --workspace
./bootstrap.sh --test
