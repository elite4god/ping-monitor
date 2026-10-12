#!/bin/sh
set -e
cd "$(dirname "$0")/.."
command -v cargo-deb >/dev/null 2>&1 || cargo install cargo-deb
cargo deb
echo
ls -1 target/debian/*.deb
