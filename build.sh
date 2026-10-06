#!/bin/sh
# Builds everything the SuperCI binary carries inside it, then the binary: the Cloudflare control plane and runner
# agent (WebAssembly), the AWS control plane (a Lambda package for arm64), the Modal control plane's program (Linux
# x86_64), and the CLI.
# Needs: Rust, worker-build (cargo install worker-build), zig with cargo-zigbuild, zip.
set -e
cd "$(dirname "$0")"
(cd crates/plane-cloudflare && worker-build --release)
(cd crates/runners-cloudflare && worker-build --release)
cargo zigbuild --release --target aarch64-unknown-linux-musl -p superci-plane-aws
mkdir -p crates/plane-aws/build
rm -f crates/plane-aws/build/bootstrap.zip
(cd target/aarch64-unknown-linux-musl/release && zip -q -j ../../../crates/plane-aws/build/bootstrap.zip bootstrap)
cargo zigbuild --release --target x86_64-unknown-linux-musl -p superci-plane-modal
mkdir -p crates/plane-modal/build
cp target/x86_64-unknown-linux-musl/release/superci-plane crates/plane-modal/build/superci-plane
# The image reader (Linux x86_64), published beside an image for Cloudflare's containers.
cargo zigbuild --release --target x86_64-unknown-linux-musl -p image-reader
cargo build --release -p superci
ls -l target/release/superci
