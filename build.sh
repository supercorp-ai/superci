#!/bin/sh
# Builds everything the SuperCI binary carries inside it, then the binary: the Cloudflare control plane and runner
# agent (WebAssembly), the AWS control plane (a Lambda package for arm64, with mkfs.erofs for its converter), the
# Modal control plane's program (Linux x86_64), the cache program for machines (Linux x86_64 and arm64), and the CLI.
# Needs: Rust, worker-build (cargo install worker-build), zig with cargo-zigbuild, zip.
set -e
cd "$(dirname "$0")"
(cd crates/plane-cloudflare && worker-build --release)
(cd crates/runners-cloudflare && worker-build --release)
cargo zigbuild --release --target aarch64-unknown-linux-musl -p superci-plane-aws
mkdir -p crates/plane-aws/build
rm -f crates/plane-aws/build/bootstrap.zip
# With mkfs.erofs beside it, for the layer store's converter (the same package as a second function).
scripts/mkfs-erofs/build.sh target/mkfs-erofs/out
zip -q -j crates/plane-aws/build/bootstrap.zip target/aarch64-unknown-linux-musl/release/bootstrap target/mkfs-erofs/out/mkfs.erofs target/mkfs-erofs/out/mkfs.erofs.LICENSES
cargo zigbuild --release --target x86_64-unknown-linux-musl -p superci-plane-modal
mkdir -p crates/plane-modal/build
cp target/x86_64-unknown-linux-musl/release/superci-plane crates/plane-modal/build/superci-plane
# The cache program for AWS machines (Linux, both architectures, and Windows), put in each region's cache bucket.
cargo zigbuild --release --target x86_64-unknown-linux-musl -p superci-cache
cargo zigbuild --release --target aarch64-unknown-linux-musl -p superci-cache
cargo zigbuild --release --target x86_64-pc-windows-gnu -p superci-cache
cargo zigbuild --release --target x86_64-unknown-linux-musl -p superci-layers
cargo zigbuild --release --target aarch64-unknown-linux-musl -p superci-layers
# The image reader (Linux x86_64), published beside an image for Cloudflare's containers.
cargo zigbuild --release --target x86_64-unknown-linux-musl -p image-reader
# (SUPERCI_PROGRAMS_ONLY=1: what the program carries alone, for a release that builds the program for each system on
# its own machine.)
[ "${SUPERCI_PROGRAMS_ONLY:-}" = 1 ] && exit 0
cargo build --release -p superci
ls -l target/release/superci
