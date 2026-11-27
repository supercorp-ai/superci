#!/usr/bin/env bash
# Runs scripts/image-check.yml, one job per kind of image, on the control plane of a workspace whose control plane runs
# the build being released (`superci planes update` with it first). A release moves the image pins only after every
# job here passed. Each job is a small spot machine for two or three minutes; the GPU jobs are on T4s.
#
#   scripts/image-check.sh SUPERCI WORKSPACE OWNER/REPO [JOB...]
#
# SUPERCI: the superci binary to run (the release's build). OWNER/REPO: a repository of the workspace's code host, so
# the jobs' cache is the CI's own. Without JOBs, all of them, at once.
set -euo pipefail
superci=$1 workspace=$2 repo=$3; shift 3
here=$(cd "$(dirname "$0")" && pwd)
jobs=("$@")
[ ${#jobs[@]} -gt 0 ] || jobs=(ubuntu24-x64 ubuntu24-arm64 ubuntu26-x64 ubuntu26-arm64 windows gpu-ubuntu24 gpu-ubuntu26)

# A checkout of its own: a run from a computer sends the working tree, and this one is only the workflow.
tree=$(mktemp -d)
trap 'rm -rf "$tree"' EXIT
mkdir -p "$tree/.github/workflows"
cp "$here/image-check.yml" "$tree/.github/workflows/image-check.yml"
git -C "$tree" init -q && git -C "$tree" add . && git -C "$tree" -c user.name=check -c user.email=check@localhost commit -qm check

failed=()
for job in "${jobs[@]}"; do
  (cd "$tree" && "$superci" runs create --workspace="$workspace" --workflow=.github/workflows/image-check.yml --job="$job" \
    --event=workflow_dispatch --repo="$repo" --quiet) > "$tree/$job.log" 2>&1 &
  pids+=("$!")
done
for i in "${!jobs[@]}"; do
  if wait "${pids[$i]}"; then echo "passed  ${jobs[$i]}"; else echo "FAILED  ${jobs[$i]}"; failed+=("${jobs[$i]}"); tail -40 "$tree/${jobs[$i]}.log"; fi
done
[ ${#failed[@]} -eq 0 ] || { echo "Failed: ${failed[*]}"; exit 1; }
# Every kind passed: the builds now pinned noted as checked, for the release to see (commit it).
if [ ${#jobs[@]} -eq 7 ]; then
  sed -n 's/^pub const IMAGE_RELEASE: &str = "\(.*\)";/\1/p' "$here/../crates/core/src/aws.rs" > "$here/image-check.passed"
  echo "All passed: scripts/image-check.passed now names $(cat "$here/image-check.passed")."
fi
