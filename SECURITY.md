# Security

## Reporting a problem

Please report security problems privately, through GitHub's **Report a vulnerability** button on this repository's
Security tab, not in a public issue. Say what you found, how to reproduce it, and what it lets someone do. You will get
an answer within three working days, and a fix or a plan within two weeks. We credit reporters who want it.

Supported: the latest release. Control planes are updated from the dashboard (Control plane → Update).

## How SuperCI is built, and where the trust lines are

- **The dashboard** runs on your computer (`localhost:8976`). It opens with a random key in its URL, which becomes a
  cookie. It answers only on this computer's own address, and makes changes only on requests from its own pages.
- **SuperCI's sign-ins** are its own, kept on your computer in `~/.superci/sign-ins.json` (readable by you alone), so
  the dashboard opens signed in and commands such as `superci status` run without a browser. It reads no other
  tool's credentials: no AWS profile, no wrangler or Modal login, none of their environment variables. What is kept:
  - AWS: the browser sign-in. It can do what you can in that account, and AWS ends it after twelve hours at most.
  - Cloudflare: the browser sign-in, renewed by SuperCI until you end it. It reaches Workers and containers in the
    accounts you can reach; Cloudflare has nothing narrower.
  - Modal: a token for your workspace, until you delete it in Modal.
  - A key to read your control plane's status, renewed every thirty days.
  Anyone who can read that file can act as SuperCI does in those clouds. `superci logout` removes it, asks Cloudflare
  to end its sign-in, and asks your control plane to forget the key. Nothing of this is in your control plane or
  anywhere outside your computer.
- **The control plane** runs in your cloud account (a Cloudflare Worker, an AWS Lambda, or a Modal app). It is open
  to the internet, because GitHub and GitLab send it webhooks. Anything else needs one of these:
  - a GitHub signature (HMAC, compared in constant time);
  - the GitLab webhook secret;
  - a dashboard key (random, expiring, kept in the control plane's own secrets);
  - a key that only reads (`superci keys create`): it sees what the dashboard sees and a job's log, and changes
    nothing. The control plane keeps its SHA-256, not the key, and it expires;
  - a one-time move token.
  A job's log is fetched by the control plane when a key holder asks, with your GitHub App's token for that job's
  repository (or your GitLab token), and passed on. GitHub masks secrets in logs; what a job printed is otherwise
  what a key holder reads.
  It signs short-lived tokens (ES256), but only for its own runner agents and for AWS. It never signs claims a
  caller picks.
- **Runner agents** (Cloudflare, Modal) accept only tokens signed by the one control plane they were set up for. The
  token must be for the agent's own address and must not have expired.
- **Machines** run one job each and end when it ends. A machine also has its own time limit: six hours (GitHub's
  default limit for a job), enforced by
  the machine itself (AWS: a timer plus a scheduled shutdown, and the machine terminates on power-off) or by the cloud
  (Cloudflare: the runner's alarm; Modal: the sandbox timeout). This works even if the control plane is gone. The
  control plane also sweeps up machines whose job never came (after 10 minutes) and machines past the limit.

## Defaults that limit what a job can do

- **AWS machines** start in a network of the control plane's own in each region (a VPC tagged `superci-plane`),
  in a security group with no inbound rules. Nothing can reach a job's machine, jobs can't reach each other, and they
  can't reach your other machines' private addresses. The dashboard makes the network with your AWS sign-in (the
  control plane's role cannot make networks) and removes it with the control plane. Until it is made (Control plane →
  Permissions → Review), machines start in the region's default network.
  A region can be given a network of your own instead (Runners → AWS → Your own network): machines there start in
  your subnets with your security groups, and reach whatever that network reaches. What a job may reach is then
  yours to set, in those security groups. A network that cannot be found stops jobs there; they do not fall back
  to another network.

- **Public repositories:** their jobs are refused unless the repository is allowed in the dashboard (Workflows →
  Limits). Even then, runs from a fork's pull request and `pull_request_target` runs are refused. On organizations,
  GitHub's default runner group also keeps runners away from public repositories.
- **Machine size:** a label asking for more than 32 CPUs (or 8 GB of memory per allowed CPU) is refused. The limit
  can be changed in the dashboard.
- **Jobs at once:** 20 per provider unless set otherwise. Monthly budgets per provider count estimated spend,
  including the time a runner waits for its job. AWS's on-demand machines are a provider of their own in the order,
  with their own limits.

## Known limits

These are tracked and will change. Until then, keep them in mind:

- **GitLab** has no runner for exactly one job, as GitHub does. Each job gets a project runner of its own, paused as
  soon as it takes a job and removed when the job ends. A job that reads its runner's token can't take another job
  with it. GitLab runs a fork's merge-request pipelines in the fork's project, so those never reach your runners.
- **Credentials** given to a control plane are broad:
  - the GitHub App on a personal account needs `administration: write`;
  - a GitLab token has the `api` scope.
  Prefer an organization install, and a project or group access token for GitLab.
- **Spot machines** can be taken back by AWS mid-job. The job fails and is run again once its run has finished
  (GitHub cannot restart one job of a run that is still going), at the next provider in your order below AWS's spot
  machines: AWS on-demand, unless you moved it or turned it off. With no other provider that can run it, it runs on a
  spot machine again. A job that must not run twice (a deploy) should say `-ondemand` in its label. The machine says
  it was taken back with a link only it has, and a job runs as root on its machine, so a job can claim its own machine
  was taken back: it is then run once more, and two such claims in a quarter of an hour send new jobs past AWS's spot
  machines for half an hour. Nothing else follows from the link.
- **No spot machine, or a provider that cannot start a machine:** the job goes to the next provider in your order
  that can run it. By default that is AWS on-demand, at AWS's list price; each provider's monthly budget and jobs at
  once still apply. A job that lands on another provider runs in that provider's environment: from Cloudflare or
  Modal it does not reach a network of your own on AWS. To keep a job on one provider, name it in its label
  (`superci-aws`).
- **GitHub Enterprise Server:** the control plane runs in your cloud account, not inside your network. Your server
  has to accept the control plane's API calls and be able to send it job events, and runners have to reach your
  server. A server that only your network can reach will not work with a control plane on Cloudflare or Modal.
- **GitHub's full image on Cloudflare** (experimental) comes from an address you set. With an address that ends
  in its index's checksum (as `image-reader pack` names what it makes), everything read is what was published: the
  index is checked against the address, each piece and the reader program against the index. With any other
  address, the image is whatever that address serves over https at the time. Jobs run inside the image as on
  GitHub's machines; whoever can publish at the address decides what runs in them.
- **Costs:** a running job's cost is estimated from list prices. When it ends, it is settled at the prices AWS billed
  for it, or as Cloudflare metered it (read while the dashboard is signed in to Cloudflare). Modal's are its sandbox
  price for the exact size. None of this subtracts free allowances or credits on your accounts.
