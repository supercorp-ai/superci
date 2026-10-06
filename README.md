# SuperCI

Your GitHub Actions and GitLab CI jobs, on fresh machines in your own AWS, Cloudflare or Modal account.

```sh
npx @superci/cli
```

That opens your dashboard, on your own computer. Sign in with your cloud, put a control plane there with one click, connect a repository, and change one line in a workflow:

```yaml
jobs:
  test:
    runs-on: superci
```

Each job gets a machine of its own, of the size its label asks for (`superci-16cpu`, `superci-arm64`, `superci-gpu`, `superci-windows`), and the machine is gone when the job ends. You pay your cloud's prices; there is no service of ours in between.

Documentation: [superci.dev/docs](https://superci.dev/docs).

## How it is put together

- **The dashboard** (`crates/cli`) runs on your computer and stores nothing. It deploys and updates everything else with your own sign-ins.
- **The control plane** (`crates/core`, with a thin runtime for each place it can live: `crates/plane-cloudflare`, `crates/plane-aws`, `crates/plane-modal`) runs in your cloud account. GitHub and GitLab tell it when a job is waiting, and it starts a machine for it at the first of your providers that can run it.
- **Runner agents** (`crates/runners-cloudflare`, and a Modal app in `crates/cli/src/modal_agent.py`) start containers for a control plane that lives in another cloud.
- **The image reader** (`crates/image-reader`) lets a Cloudflare container run jobs inside GitHub's full runner image without holding it on its disk.

How it is built and where its trust lines are: [SECURITY.md](SECURITY.md). What changed in each version: [CHANGELOG.md](CHANGELOG.md).

## Build from source

```sh
./build.sh        # the control planes and agents the program carries inside it, then the program
cargo test --workspace
target/release/superci
```

`build.sh` needs Rust, [worker-build](https://crates.io/crates/worker-build), zig with [cargo-zigbuild](https://crates.io/crates/cargo-zigbuild), and `zip`.

The npm packages are packed with `./scripts/npm-pack.sh` (the program for macOS and Linux, arm64 and x64, and a launcher). A release is the workflow **Publish to npm** (`.github/workflows/publish.yml`), run by hand with the version: it builds, tests, publishes through npm's trusted publishing, and tags the release.

## License

MIT. See [LICENSE](LICENSE).
