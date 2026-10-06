#!/bin/sh
# Publishes what ./scripts/npm-pack.sh packed: the programs first, the launcher last (it names them). A package whose
# version is on npm already is passed by, so a publish that stopped halfway is finished by running this again.
# From a computer: `npm login` as an owner of the npm organization `superci` (the @superci scope).
# From GitHub (.github/workflows/publish.yml): no login, npm trusts the workflow (trusted publishing).
#   ./scripts/npm-publish.sh            # publish
#   ./scripts/npm-publish.sh --dry-run  # say what would be published
set -e
cd "$(dirname "$0")/.."
ls target/npm/superci-cli-*-*-[0-9]*.tgz target/npm/superci-cli-[0-9]*.tgz >/dev/null 2>&1 || { echo "run ./scripts/npm-pack.sh first" >&2; exit 1; }
for tgz in target/npm/superci-cli-*-*-[0-9]*.tgz target/npm/superci-cli-[0-9]*.tgz; do
  spec=$(tar -xzOf "$tgz" package/package.json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const p=JSON.parse(s);console.log(p.name+"@"+p.version)})')
  if [ -n "$(npm view "$spec" version 2>/dev/null)" ]; then echo "==> $spec is on npm already"; continue; fi
  echo "==> $spec"
  # npm runs attached to the terminal: it may ask for a one-time password. It may also not show a version it has
  # just taken; being told so (in its own log) is not a failure.
  if ! npm publish "$tgz" --access public "$@"; then
    log=$(ls -t "${npm_config_cache:-$HOME/.npm}"/_logs/*-debug-0.log 2>/dev/null | head -1)
    [ -n "$log" ] && grep -q "cannot publish over the previously published" "$log" || exit 1
    echo "==> $spec is on npm already"
  fi
done
