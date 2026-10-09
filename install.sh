#!/bin/sh
# Installs SuperCI's program without Node: the same file `npx @superci/cli` runs, fetched from npm's registry and
# checked against the checksum the registry gives for it.
#
#   curl -fsSL https://superci.dev/install.sh | sh
#
# SUPERCI_VERSION       the version to install (the latest unless set)
# SUPERCI_INSTALL_DIR   where the program goes (~/.local/bin unless set)
#
# macOS and Linux, arm64 and x64. On Windows: `npx @superci/cli`.
set -eu

say() { printf '%s\n' "$*"; }
fail() { printf 'superci: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" > /dev/null 2>&1; }

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) fail "this installer is for macOS and Linux. Elsewhere: npx @superci/cli" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) cpu=arm64 ;;
  x86_64 | amd64) cpu=x64 ;;
  *) fail "no SuperCI is built for $(uname -m). See https://superci.dev/docs/getting-started" ;;
esac

if have curl; then
  get() { curl -fsSL "$1"; }
  save() { curl -fsSL -o "$2" "$1"; }
elif have wget; then
  get() { wget -qO- "$1"; }
  save() { wget -qO "$2" "$1"; }
else
  fail "curl or wget is needed"
fi
have tar || fail "tar is needed"

registry=https://registry.npmjs.org
package="cli-$os-$cpu"
# One value out of the registry's answer (one JSON object on one line), without a JSON tool.
field() { sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p" | head -1; }

version="${SUPERCI_VERSION:-}"
if [ -z "$version" ]; then
  version=$(get "$registry/@superci/cli/latest" | field version) || true
  [ -n "$version" ] || fail "npm's registry did not say which version is the latest"
fi
case "$version" in *[!0-9A-Za-z.+-]* | "") fail "$version is not a version" ;; esac

about=$(get "$registry/@superci/$package/$version") || fail "npm's registry has no SuperCI $version for $os on $cpu"
integrity=$(printf '%s' "$about" | field integrity)
case "$integrity" in sha512-*) want="${integrity#sha512-}" ;; *) fail "npm's registry gave no checksum for SuperCI $version" ;; esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
save "$registry/@superci/$package/-/$package-$version.tgz" "$tmp/superci.tgz" || fail "could not download SuperCI $version"

# The checksum the registry gives is SHA-512 in base64.
if have openssl; then
  got=$(openssl dgst -sha512 -binary "$tmp/superci.tgz" | openssl base64 -A)
elif have sha512sum && have base64 && have od; then
  got=$(sha512sum "$tmp/superci.tgz" | cut -d' ' -f1)
  want=$(printf '%s' "$want" | base64 -d | od -An -v -tx1 | tr -d ' \n')
else
  fail "openssl (or sha512sum and base64) is needed to check the download"
fi
[ "$got" = "$want" ] || fail "the download does not match its checksum; nothing was installed"

tar -xzf "$tmp/superci.tgz" -C "$tmp" package/superci
dir="${SUPERCI_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dir"
chmod 755 "$tmp/package/superci"
# Moved into place in one step, so a program that is running is not written over halfway.
mv -f "$tmp/package/superci" "$dir/.superci.$$"
mv -f "$dir/.superci.$$" "$dir/superci"

say "SuperCI $version is installed: $dir/superci"
case ":$PATH:" in
  *":$dir:"*) say "Open the dashboard: superci dashboard" ;;
  *)
    say "$dir is not on your PATH. Add it (in ~/.zshrc or ~/.bashrc):"
    say "  export PATH=\"$dir:\$PATH\""
    say "Then open the dashboard: superci dashboard"
    ;;
esac
