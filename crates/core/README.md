# superci-core

The core of a [SuperCI](https://superci.dev) control plane: it hears of GitHub Actions and GitLab CI jobs and gives each a fresh machine in your own AWS, Cloudflare or Modal account. It knows nothing of where it runs; a thin runtime for each place (a Lambda function, a Cloudflare Worker, a Modal app) gives it storage, time and HTTP.

You most likely want the program instead:

```sh
cargo install superci
```

Source and documentation: [supercorp-ai/superci](https://github.com/supercorp-ai/superci), [superci.dev/docs](https://superci.dev/docs).
