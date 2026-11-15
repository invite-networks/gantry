#!/bin/sh
# Builds the Debian package: release binaries, man page and shell
# completions. Needs cargo-deb (cargo install cargo-deb). Run on Linux.
set -eu
cd "$(dirname "$0")/.."
cargo build --release --locked
out=target/package
mkdir -p "$out"
bin=target/release/gantry
"$bin" internal man > "$out/gantry.1"
"$bin" completion bash > "$out/gantry.bash"
"$bin" completion zsh > "$out/_gantry"
"$bin" completion fish > "$out/gantry.fish"
cargo deb --no-build "$@"
