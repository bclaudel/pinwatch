#!/bin/sh
set -eu
# Run inside `nix develop`; rust-toolchain.toml remains the version authority.
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release --bins
