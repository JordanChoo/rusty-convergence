#!/bin/sh
set -e

curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable
. "$HOME/.cargo/env"
rustup target add wasm32-unknown-unknown
cargo install -q worker-build
worker-build --release
