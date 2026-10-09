#!/bin/sh
# Writes the Homebrew formula for a released version: the same files `npx @superci/cli` runs, from npm's registry,
# each with its checksum. It goes to the tap (supercorp-ai/homebrew-tap, Formula/superci.rb), so that
# `brew install supercorp-ai/tap/superci` installs that version.
#   ./scripts/brew-formula.sh [version] > Formula/superci.rb   # the version in Cargo.toml unless given
set -eu
cd "$(dirname "$0")/.."
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
part() {
  url="https://registry.npmjs.org/@superci/cli-$1/-/cli-$1-$version.tgz"
  curl -q -fsSL -o "$tmp/$1.tgz" "$url" || { echo "no @superci/cli-$1@$version on npm" >&2; exit 1; }
  sum=$(shasum -a 256 "$tmp/$1.tgz" 2> /dev/null || sha256sum "$tmp/$1.tgz")
  printf '    %s do\n      url "%s"\n      sha256 "%s"\n    end\n' "$2" "$url" "${sum%% *}"
}
cat <<FORMULA
# Written by scripts/brew-formula.sh in supercorp-ai/superci at each release. Not edited by hand.
class Superci < Formula
  desc "GitHub Actions and GitLab CI jobs on your own AWS, Cloudflare and Modal"
  homepage "https://superci.dev"
  license "MIT"

  on_macos do
$(part darwin-arm64 on_arm)

$(part darwin-x64 on_intel)
  end

  on_linux do
$(part linux-arm64 on_arm)

$(part linux-x64 on_intel)
  end

  def install
    # npm's archive holds the program under package/, which Homebrew takes away when it unpacks.
    bin.install "superci"
  end

  test do
    assert_equal "superci #{version}", shell_output("#{bin}/superci --version").strip
  end
end
FORMULA
