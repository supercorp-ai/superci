#!/bin/sh
# Makes this commit carry VERSION for a release, and refuses when something a release needs is not there yet:
#   ./scripts/release-prepare.sh 0.20.5
# It sets the workspace's version and the CLI's exact pins of its own crates, dates the changelog's entry for it
# ("## 0.20.5" becomes "## 0.20.5 — 2026-11-28"), updates Cargo.lock, and checks that the image builds pinned passed
# the live image check (scripts/image-check.passed). Nothing is committed or published: the next steps are said.
set -eu
cd "$(dirname "$0")/.."
version=${1:?say the version: ./scripts/release-prepare.sh 0.20.5}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "$version is not a version (0.20.5)" >&2; exit 2; }
[ -z "$(git status --porcelain)" ] || { echo "Uncommitted changes here: commit or put them away first." >&2; exit 1; }
was=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ "$was" != "$version" ] || { echo "This commit carries $version already." >&2; exit 1; }
grep -Eq "^## $version( |\$)" CHANGELOG.md || { echo "CHANGELOG.md has no entry for $version (## $version)." >&2; exit 1; }
pinned=$(sed -n 's/^pub const IMAGE_RELEASE: &str = "\(.*\)";/\1/p' crates/core/src/aws.rs)
grep -qx "$pinned" scripts/image-check.passed 2>/dev/null || { echo "The pinned image builds ($pinned) have not passed scripts/image-check.sh." >&2; exit 1; }
# The version, and the CLI's pins of its own crates (exact: they are published together).
sed -i.bak "s/^version = \"$was\"/version = \"$version\"/" Cargo.toml
sed -i.bak "s/version = \"=$was\"/version = \"=$version\"/g" crates/cli/Cargo.toml
rm -f Cargo.toml.bak crates/cli/Cargo.toml.bak
# The changelog's entry dated, unless it is.
grep -Eq "^## $version — " CHANGELOG.md || { sed -i.bak "s/^## $version\$/## $version — $(date -u +%Y-%m-%d)/" CHANGELOG.md && rm -f CHANGELOG.md.bak; }
cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
[ "$(grep -c "\"=$version\"" crates/cli/Cargo.toml)" = 4 ] || { echo "The CLI's pins were not all moved: see crates/cli/Cargo.toml." >&2; exit 1; }
echo "This commit now carries $version ($was before). Next: commit, let Test and Coverage pass, merge into main, then run"
echo "  gh workflow run publish.yml --ref main -f version=$version -f dry_run=true"
echo "and, once that passed, the same without dry_run."
