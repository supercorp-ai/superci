#!/bin/sh
# Builds SuperCI for every system it ships for and packs the npm packages: a launcher and one package
# per system holding the program. The launcher is `@superci/cli` (npm refuses the plain name `superci`: too close to
# an existing package). Nothing is published: the packages land in target/npm/ as .tgz files.
#   ./scripts/npm-pack.sh            # build everything, then pack
#   ./scripts/npm-pack.sh --no-build # pack what is already built
# Needs what ./build.sh needs, plus the Rust targets x86_64-apple-darwin and x86_64-pc-windows-gnu, node and npm.
set -e
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "${1:-}" != "--no-build" ]; then
  # What the program carries inside it (the control planes, the runner agent), and the program for this Mac.
  ./build.sh
  cargo build --release --target aarch64-apple-darwin -p superci
  cargo build --release --target x86_64-apple-darwin -p superci
  cargo zigbuild --release --target aarch64-unknown-linux-musl -p superci
  cargo zigbuild --release --target x86_64-unknown-linux-musl -p superci
  cargo zigbuild --release --target x86_64-pc-windows-gnu -p superci
fi
out=target/npm
rm -rf "$out" && mkdir -p "$out"
node - "$version" "$out" <<'NODE'
const fs = require("node:fs"), path = require("node:path");
const [version, out] = process.argv.slice(2);
const { targets } = JSON.parse(fs.readFileSync("npm/targets.json", "utf8"));
const main = JSON.parse(fs.readFileSync("npm/package.json", "utf8"));
main.version = version;
for (const t of targets) {
  const dir = path.join(out, t.package.replace("@superci/", ""));
  fs.mkdirSync(dir, { recursive: true });
  const file = t.os === "win32" ? "superci.exe" : "superci";
  const program = path.join("target", t.rustTarget, "release", file);
  if (!fs.existsSync(program)) throw new Error(`${program} is not built`);
  fs.copyFileSync(program, path.join(dir, file));
  fs.chmodSync(path.join(dir, file), 0o755);
  fs.copyFileSync("LICENSE", path.join(dir, "LICENSE"));
  fs.writeFileSync(path.join(dir, "README.md"), `# ${t.package}\n\nSuperCI's program for ${t.os} on ${t.cpu}. Install \`@superci/cli\` instead: it picks the right one.\n\nhttps://superci.dev\n`);
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({
    name: t.package, version, description: `SuperCI's program for ${t.os} on ${t.cpu}`, license: main.license,
    repository: main.repository, homepage: main.homepage, os: [t.os], cpu: [t.cpu, ...(t.alsoCpu || [])], files: [file, "LICENSE", "README.md"],
    publishConfig: { access: "public" },
  }, null, 2) + "\n");
  main.optionalDependencies[t.package] = version;
}
const dir = path.join(out, "cli");
fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
fs.copyFileSync("npm/bin/superci.js", path.join(dir, "bin", "superci.js"));
fs.chmodSync(path.join(dir, "bin", "superci.js"), 0o755);
fs.copyFileSync("npm/README.md", path.join(dir, "README.md"));
fs.copyFileSync("LICENSE", path.join(dir, "LICENSE"));
fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify(main, null, 2) + "\n");
NODE
for dir in "$out"/*/; do (cd "$dir" && npm pack --silent --pack-destination .. >/dev/null); done
ls -l "$out"/*.tgz | awk '{print $5, $9}'
echo "packed SuperCI $version"
