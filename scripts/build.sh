#!/bin/sh
set -e

if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
else
    curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable
    . "$HOME/.cargo/env"
    rustup target add wasm32-unknown-unknown
    cargo install -q worker-build
fi

worker-build --release
