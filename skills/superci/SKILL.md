---
name: superci
description: Move a repository's GitHub Actions or GitLab CI jobs onto SuperCI runners (a fresh machine per job in the user's own AWS, Cloudflare or Modal account), from GitHub's hosted or larger runners, Blacksmith, Depot, Namespace, WarpBuild, Ubicloud, BuildJet, RunsOn or actuated. Use when asked to switch CI runners, cut the CI bill, run jobs in the user's own cloud, or replace a runner vendor; also to check why a SuperCI job did not start.
---

# Moving CI jobs to SuperCI

SuperCI runs each job on a fresh machine in the user's own cloud account. A job moves by changing its `runs-on` line. The CLI is `npx @superci/cli` (or `superci` when installed); every command takes `--json`, never asks a question, and exits 3 with a `needs` field when a person must act first.

## 1. Check SuperCI is set up

```sh
npx @superci/cli status --json        # control plane, runners, connections
npx @superci/cli labels list --json   # the label names workflows may use
```

With several workspaces on the machine, the command says so: ask the user which and add `--workspace=ID`. Use the first label from `labels list` everywhere below (`superci` unless the user made their own). Exit status 3 means a person is needed: tell the user the `needs` command and stop. Only a person can sign in to a cloud, create the GitHub App (`superci connections create github OWNER` opens GitHub's pages), or make a GitLab token. With nothing set up, tell them to run `npx @superci/cli dashboard`.

## 2. Find every runner

Search `.github/workflows/` (and `.gitlab-ci.yml` with its `include:` files) for `runs-on`, matrix values that feed it, reusable workflows and composite actions. Change matrix values rather than `runs-on: ${{ matrix.os }}`, and a reusable workflow where it is defined. One label per job, not a list.

## 3. Map each label

Keep the job's size as `key=value` pairs after the label. Memory follows CPUs at 4 GB each; add `ram=` only when the old machine had more.

| Before | After |
|---|---|
| `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04` | `superci` |
| `ubuntu-24.04-arm`, `ubuntu-22.04-arm` | `superci/arch=arm64` |
| GitHub larger runner with N cores (name chosen by its owner, e.g. `ubuntu-24.04-16core`) | `superci/cpu=N` |
| `windows-latest`, `windows-2025` | `superci/os=windows` |
| `blacksmith-Nvcpu-ubuntu-2404`, `buildjet-Nvcpu-ubuntu-2204` (`-arm`) | `superci/cpu=N` (`/arch=arm64`) |
| `depot-ubuntu-24.04` (`-arm`) (`-N`) | `superci/cpu=2`, or `superci/cpu=N` (`/arch=arm64`) |
| `warp-ubuntu-latest-x64-Nx`, `warp-ubuntu-2404-arm64-Nx` | `superci/cpu=N` (`/arch=arm64`) |
| `ubicloud-standard-N` (`-arm`) | `superci/cpu=N` (`/arch=arm64`) |
| `actuated-Ncpu-Mgb`, `actuated-arm64-Ncpu-Mgb` | `superci/cpu=N/ram=M` (`/arch=arm64`) |
| `runs-on=${{ github.run_id }}/cpu=N/...` or `runner=Ncpu-linux-x64` | `superci/cpu=N`; `family=` and `spot=false` carry over as they are |
| `namespace-profile-NAME` | The profile's size, from the user or `nsc`: `16x32` is `superci/cpu=16/ram=32` |
| A GPU job | `superci/gpu=any`, or `gpu=l4` and so on |

https://superci.dev/docs/labels.md lists every key (`disk=`, `image=ubuntu26`, `provider=aws`, ...). A label SuperCI does not understand fails the job at once, saying why.

## 4. Replace vendor actions

| Before | After |
|---|---|
| `useblacksmith/cache`, `buildjet/cache`, `WarpBuilds/cache`, `ubicloud/cache`, `runs-on/cache` | `actions/cache`, same inputs |
| `useblacksmith/setup-node`, `buildjet/setup-node` | `actions/setup-node`, same inputs |
| `namespacelabs/nscloud-cache-action` | `actions/cache` with the same paths and a `key` |
| `useblacksmith/setup-docker-builder`, `namespacelabs/nscloud-setup-buildx-action` | `docker/setup-buildx-action` |
| `useblacksmith/build-push-action` | `docker/build-push-action` with `cache-from: type=gha` and `cache-to: type=gha,mode=max` |
| `useblacksmith/stickydisk` | No equivalent: `actions/cache` for the same path, and say so in the pull request |
| `depot/build-push-action` | Leave it if the user keeps Depot for Docker builds (it builds on Depot's machines from any runner); otherwise `docker/build-push-action` |

On AWS, `actions/cache` and Docker's `type=gha` reach a cache in the user's own account with no change.

## 5. What stays where it is

Leave these jobs alone, each with a `# stays on GitHub: <why>` (or `# stays on GitLab: <why>`) comment:

- macOS jobs;
- npm publishing with provenance (it needs a GitHub-hosted runner);
- jobs that need KVM or nested virtualisation;
- on a public repository, fork pull requests and `pull_request_target`, which SuperCI refuses: `runs-on: ${{ github.event.pull_request.head.repo.fork && 'ubuntu-latest' || 'superci' }}`. A public repository also has to be allowed first (`superci public_repos create OWNER/REPO`).

For GitLab, tag jobs `superci` (replacing `saas-linux-*`, or once in `default: tags`): `superci/cpu=4` for medium, `superci/cpu=8` for large, `superci/cpu=16` for xlarge, `superci/arch=arm64` for arm64. Keep `image:` and `services:`. GitLab jobs run in Docker, so not on Modal.

## 6. Try it before pushing

```sh
npx @superci/cli runs create --workflow=.github/workflows/ci.yml --quiet
```

That runs the working tree as it is on SuperCI's machines, with nothing pushed, and prints each step's result and the end of a failed one. A job that fails only on the new runner goes back to its old label, with the reason in the pull request.

To switch back without a commit later, name a variable in the workflow: `runs-on: ${{ vars.CI_RUNS_ON || 'ubuntu-latest' }}`, with `CI_RUNS_ON` set to the SuperCI label.

## 7. Open one pull request

One pull request (one merge request on GitLab) with a before → after table of every job's runner, the jobs that stayed and why, and the vendor actions replaced. After it merges, `npx @superci/cli jobs list --json` shows each job's machine and cost.

## When a job never starts

- `npx @superci/cli labels list`: the job's label must be one of these.
- `npx @superci/cli connections list --json`: the GitHub App must be installed on the repository.
- `npx @superci/cli deliveries list`: what GitHub says it sent to the control plane.
- `npx @superci/cli jobs retrieve JOB_ID`: how one job's start went, step by step.

Docs: https://superci.dev/docs (each page also as Markdown, e.g. https://superci.dev/docs/labels.md).
