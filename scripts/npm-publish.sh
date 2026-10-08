#!/bin/sh
# Publishes what ./scripts/npm-pack.sh packed: the programs first, the launcher last (it names them). A package whose
# version is on npm already is passed by, so a publish that stopped halfway is finished by running this again.
# Published is not yet downloadable: npm lists a new version at once and serves its file some minutes later (0.10.7:
# nine minutes), the launcher's own too. So nothing is `latest` before it downloads: the launcher is published only
# once every program's file downloads, and under the tag `next`; once its own file downloads, `latest` is moved to
# it. Until that last step `npx @superci/cli` goes on giving the version before, and a release that stops anywhere
# leaves it so. Moving the tag from GitHub needs "Allow npm dist-tag" on @superci/cli's trusted publisher.
# From a computer: `npm login` as an owner of the npm organization `superci` (the @superci scope).
# From GitHub (.github/workflows/publish.yml): no login, npm trusts the workflow (trusted publishing).
#   ./scripts/npm-publish.sh            # publish
#   ./scripts/npm-publish.sh --dry-run  # say what would be published
set -e
cd "$(dirname "$0")/.."
ls target/npm/superci-cli-*-*-[0-9]*.tgz target/npm/superci-cli-[0-9]*.tgz >/dev/null 2>&1 || { echo "run ./scripts/npm-pack.sh first" >&2; exit 1; }

named() { tar -xzOf "$1" package/package.json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const p=JSON.parse(s);console.log(p.name+"@"+p.version)})'; }

publish() {
  file=$1; shift
  spec=$(named "$file")
  if [ -n "$(npm view "$spec" version 2>/dev/null)" ]; then echo "==> $spec is on npm already"; return; fi
  echo "==> $spec"
  # npm runs attached to the terminal: it may ask for a one-time password. It may also not show a version it has
  # just taken; being told so (in its own log) is not a failure.
  if ! npm publish "$file" --access public "$@"; then
    log=$(ls -t "${npm_config_cache:-$HOME/.npm}"/_logs/*-debug-0.log 2>/dev/null | head -1)
    [ -n "$log" ] && grep -q "cannot publish over the previously published" "$log" || exit 1
    echo "==> $spec is on npm already"
  fi
}

# Waits until npm lists each version and its file downloads (15 minutes at most).
downloadable() {
  node - "$@" <<'NODE'
const waiting = new Set(process.argv.slice(2)), started = Date.now();
const seconds = () => Math.round((Date.now() - started) / 1000);
async function there(spec) {
  const at = spec.lastIndexOf("@"), name = spec.slice(0, at), version = spec.slice(at + 1);
  const listed = await fetch(`https://registry.npmjs.org/${name.replace("/", "%2F")}?t=${Date.now()}`, { headers: { "cache-control": "no-cache" } });
  const file = listed.ok && (await listed.json()).versions?.[version]?.dist?.tarball;
  return Boolean(file) && (await fetch(file, { method: "HEAD" })).ok;
}
while (waiting.size) {
  for (const spec of [...waiting]) {
    if (await there(spec).catch(() => false)) { waiting.delete(spec); console.log(`==> ${spec} downloads, after ${seconds()} s`); }
  }
  if (!waiting.size) break;
  if (seconds() > 900) { console.error(`still not downloadable after ${seconds()} s: ${[...waiting].join(", ")}`); process.exit(1); }
  await new Promise((r) => setTimeout(r, 10000));
}
NODE
}

programs=""
for tgz in target/npm/superci-cli-*-*-[0-9]*.tgz; do
  publish "$tgz" "$@"
  programs="$programs $(named "$tgz")"
done
launcher=$(ls target/npm/superci-cli-[0-9]*.tgz)
if [ "${1:-}" = "--dry-run" ]; then publish "$launcher" "$@"; exit 0; fi
# shellcheck disable=SC2086
downloadable $programs
publish "$launcher" --tag next "$@"
released=$(named "$launcher")
downloadable "$released"
if [ "$(npm view "${released%@*}" dist-tags.latest 2>/dev/null)" = "${released##*@}" ]; then echo "==> $released is latest already"; exit 0; fi
if ! npm dist-tag add "$released" latest; then
  echo "==> $released is published as \`next\`, and \`latest\` could not be moved to it. Nothing is broken: \`npx @superci/cli\` still gives the version before." >&2
  echo "    From GitHub: on npmjs.com, @superci/cli → Settings → Trusted publisher, tick \"Allow npm dist-tag\", then run this again." >&2
  echo "    From a computer: npm dist-tag add $released latest" >&2
  exit 1
fi
echo "==> $released is latest"
