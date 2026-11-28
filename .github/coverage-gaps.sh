#!/bin/sh
# What the latest measured run reaches, and each source file's conditions still to show, by decision, in gaps/ (kept
# with the run, to read without measuring again). For the Coverage workflow.
set -u
npx -y supercov@4.29.18 runs latest
npx -y supercov@4.29.18 runs latest gaps
npx -y supercov@4.29.18 runs latest file crates/cli/src/dashboard.rs --group decision --sort missing --limit 80
mkdir -p gaps
# (Every source file's, named by its path: crates/core/src/aws.rs is core-aws.txt.)
for f in crates/cli/src/*.rs crates/core/src/*.rs; do
  n=$(echo "$f" | sed -e 's|^crates/||' -e 's|/src/|-|' -e 's|\.rs$||')
  npx -y supercov@4.29.18 runs latest file "$f" --group decision --sort missing --limit 3000 > "gaps/$n.txt" || true
done
npx -y supercov@4.29.18 runs latest files --limit 0 > gaps/files.txt || true
