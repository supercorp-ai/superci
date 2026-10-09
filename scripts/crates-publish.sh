#!/bin/sh
# Publishes SuperCI to crates.io, so that `cargo install superci` builds the program: `superci-core` first, then
# `superci`, which carries what ./build.sh built (the control planes and the runner agent) in embedded/, since a
# crate is built with Rust alone. A version already there is passed by, so a publish that stopped halfway can be
# run again.
#   ./scripts/crates-publish.sh             # publish (the token: CARGO_REGISTRY_TOKEN, or `cargo login`)
#   ./scripts/crates-publish.sh --dry-run   # pack and build both as crates.io would get them; publish nothing
#   ./scripts/crates-publish.sh --embed     # only fill embedded/ from what is built
#   ./scripts/crates-publish.sh --embedded  # publish with embedded/ as it is (filled on another machine)
# Needs ./build.sh to have run, except with --embedded.
set -eu
cd "$(dirname "$0")/.."
# Said exactly, or nothing is done: anything this does not know is not taken for "publish".
[ $# -le 1 ] || { echo "one way at a time: --dry-run, --embed or --embedded" >&2; exit 2; }
case "${1:-}" in "" | --dry-run | --embed | --embedded) ;; *) echo "$1 is not a way to run this (--dry-run, --embed, --embedded)" >&2; exit 2 ;; esac
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
grep -q "superci-core = { path = \"../core\", version = \"=$version\" }" crates/cli/Cargo.toml \
  || { echo "crates/cli/Cargo.toml does not ask for superci-core =$version" >&2; exit 1; }

embedded=crates/cli/embedded
names="plane-cloudflare.js plane-cloudflare.wasm runners-cloudflare.js runners-cloudflare.wasm plane-aws.zip plane-modal CHANGELOG.md"
if [ "${1:-}" = "--embedded" ]; then
  for name in $names; do [ -f "$embedded/$name" ] || { echo "$embedded/$name is missing" >&2; exit 1; }; done
  cmp -s CHANGELOG.md "$embedded/CHANGELOG.md" || { echo "$embedded was filled from another commit (its changelog differs)" >&2; exit 1; }
else
  rm -rf "$embedded" && mkdir -p "$embedded"
  put() { [ -f "$1" ] || { echo "$1 is not built: run ./build.sh first" >&2; exit 1; }; cp "$1" "$embedded/$2"; }
  put crates/plane-cloudflare/build/index.js plane-cloudflare.js
  put crates/plane-cloudflare/build/index_bg.wasm plane-cloudflare.wasm
  put crates/runners-cloudflare/build/index.js runners-cloudflare.js
  put crates/runners-cloudflare/build/index_bg.wasm runners-cloudflare.wasm
  put crates/plane-aws/build/bootstrap.zip plane-aws.zip
  put crates/plane-modal/build/superci-plane plane-modal
  put CHANGELOG.md CHANGELOG.md
fi
[ "${1:-}" = "--embed" ] && exit 0

if [ "${1:-}" = "--dry-run" ]; then
  # Both packed and built from their packages alone, the second against the first as packed.
  cargo package -p superci-core -p superci --locked --allow-dirty
  ls -l target/package/*.crate
  exit 0
fi

there() { [ "$(curl -q -s -o /dev/null -w '%{http_code}' -A 'superci-release (https://github.com/supercorp-ai/superci)' "https://crates.io/api/v1/crates/$1/$version")" = 200 ]; }
for crate in superci-core superci; do
  if there "$crate"; then echo "$crate $version is on crates.io already"; continue; fi
  # cargo waits until the crate is in the index before it returns, so the next one finds it.
  cargo publish -p "$crate" --locked --allow-dirty
done
