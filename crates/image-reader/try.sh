#!/bin/bash
# Tries the reader for real, in a Linux container (Docker, or colima on a Mac): a small image is packed, served from
# the container itself, shown by the reader and mounted; then read whole, with a changed piece, another index, a
# small cache, and a full disk. With --start: also what starts a runner inside a published image (a small Ubuntu with
# a user runner stands in for GitHub's). Nothing leaves this machine.
# Needs: Docker, zig with cargo-zigbuild, squashfs-tools (mksquashfs, sqfstar).
set -euo pipefail
cd "$(dirname "$0")/../.."
case "$(docker info --format '{{.Architecture}}')" in aarch64|arm64) target=aarch64-unknown-linux-musl ;; *) target=x86_64-unknown-linux-musl ;; esac
cargo zigbuild --release --target $target -p image-reader
cargo build --release -p image-reader
# A directory Docker can see (colima shares the home directory).
t="${TRY_DIR:-$HOME/.cache/image-reader-try}"
rm -rf "$t" && mkdir -p "$t/tree/bin" "$t/tree/data"
cp target/$target/release/image-reader "$t/reader"
head -c 30000000 /dev/urandom > "$t/tree/data/big.bin"
for i in 1 2 3 4 5 6 7 8; do head -c 700000 /dev/urandom > "$t/tree/bin/tool$i"; done
echo hello > "$t/tree/hello.txt"
(cd "$t/tree" && find . -type f | sort | xargs shasum -a 256 > ../expected.txt)
mksquashfs "$t/tree" "$t/small.sqsh" -comp zstd -b 131072 -noappend -quiet -no-progress > /dev/null
echo '[3,1,0]' > "$t/hot.json"
target/release/image-reader pack "$t/small.sqsh" "$t/out" --piece 1048576 --hot "$t/hot.json" --reader "$t/reader"
cp crates/image-reader/tests/reader.sh "$t/test.sh"
docker run --rm --privileged -v "$t:/t:ro" python:3.12-slim bash /t/test.sh
[ "${1:-}" = "--start" ] || exit 0

docker rm -f image-reader-try-root > /dev/null 2>&1 || true
docker run --name image-reader-try-root ubuntu:24.04 bash -c "apt-get update -qq > /dev/null && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo curl python3 > /dev/null 2>&1 && useradd -m -u 1001 runner && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner && printf 'ImageOS=inside-image\nFOO=\"a b\"\n1BAD=x\nBAD NAME=y\nPATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n' > /etc/environment"
docker export image-reader-try-root | sqfstar -comp zstd -b 131072 -no-progress -quiet "$t/root.sqsh" > /dev/null
docker commit image-reader-try-root image-reader-try > /dev/null
target/release/image-reader pack "$t/root.sqsh" "$t/rootout" --piece 1048576 --reader "$t/reader"
# The start script as the control plane hands it to a container (its two parts, from core's docker.rs).
python3 -c '
import re, sys
s = open("crates/core/src/docker.rs").read()
part = lambda name: re.search(r"const %s: &str = r#\"(.*?)\"#;" % name, s, re.S).group(1)
open(sys.argv[1], "w").write(part("FULL_IMAGE") + part("PLAIN"))
' "$t/start.sh"
cp crates/image-reader/tests/start.sh "$t/start-test.sh"
docker volume rm image-reader-try > /dev/null 2>&1 || true
docker run --rm --privileged -v image-reader-try:/mnt/image -v "$t:/t:ro" image-reader-try bash /t/start-test.sh
docker volume rm image-reader-try > /dev/null 2>&1 || true
docker rm -f image-reader-try-root > /dev/null 2>&1 || true
