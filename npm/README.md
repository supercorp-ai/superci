# SuperCI

Your GitHub Actions and GitLab CI jobs, on fresh machines in your own AWS, Cloudflare or Modal account.

```sh
npx @superci/cli dashboard
```

That opens your dashboard, on your own computer. Sign in with your cloud, put a control plane there with one click, connect a repository, and change one line in a workflow:

```yaml
runs-on: superci
```

- Documentation: https://superci.dev/docs
- Source: https://github.com/supercorp-ai/superci

This package is a small launcher (its command is `superci`). The program itself comes in a package for your system (macOS, Linux or Windows; arm64 or x64), installed with it.

MIT licensed.
