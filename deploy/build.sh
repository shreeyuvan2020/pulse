#!/bin/sh
# Builds the Pulse server as a static musl binary (so it does not care which
# glibc the Box has) and fetches a portable Postgres next to it.
set -eu
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cd "$(dirname "$0")/.."
rustup target add x86_64-unknown-linux-musl
cargo build --release -p pulse-server --target x86_64-unknown-linux-musl
mkdir -p deploy/bin
cp target/x86_64-unknown-linux-musl/release/pulse-server deploy/bin/pulse-server
sh deploy/fetch-postgres.sh
