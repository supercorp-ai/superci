# Changelog

What changed in each version of SuperCI. The dashboard shows the entries your control plane does not have yet.

## 0.13.0 — unreleased
- Plugins: what your control plane does beside running jobs. Each is off until you switch it on, and none needs a line in a workflow, so a job's file stays what it would be on GitHub's own runners. `superci plugins list`, `superci plugins update NAME --enabled=true`.
- The first plugin, `output`: what each job's tests leave on its machine is kept. Once the job's last step has ended, the machine sends Playwright's report, traces, videos and `error-context.md` files, Cypress's videos and screenshots, pytest's list of what failed, coverage files and any JUnit XML written during the job to a private bucket in your AWS account, where they stay 30 days. `superci files list --job=ID` names them and `superci files retrieve --job=ID` downloads them, with a key that only reads too, so a coding agent can fetch a failed test's context. Nothing here can fail a job. Needs a control plane in AWS; GitHub jobs on Linux machines in AWS so far.

## 0.12.0 — 2026-10-09
- Labels of your own. Your control plane answers to a list of labels, `superci` alone to begin with: add one on Workflows → Labels, or with `superci labels create soroci`, and workflows can name it in `runs-on` (`runs-on: soroci`, `soroci-8cpu-arm64`). Every label in the list works at once, so workflows change one at a time and nothing waits the day you add one; remove a label when nothing names it any more. The one added last is the one shown in examples. Two to twenty-four lowercase letters and digits.
- A name of your own. Workflows → Name, or `superci name update SoroCI`: the dashboard's header and tab say it, a GitHub App made for a further organization starts with it (SoroCI acme …), GitLab's runners are described with it, and a job that could not be run says "SoroCI could not run this job". `SuperCI` puts it back. The command stays `superci`.
- A move to another control plane takes the labels and the name along.
- Installs without Node too, as the same program: `brew install supercorp-ai/tap/superci`, `curl -fsSL https://superci.dev/install.sh | sh` (macOS and Linux; checked against npm's checksum, put in `~/.local/bin`), or `cargo install superci`. Then `superci dashboard`.

## 0.11.2 — 2026-10-09
- A job that GitHub never told your control plane of now gets its machine all the same. GitHub sends each event once, and can lose one (seen: a job queued four seconds after a move changed the App's address was never sent anywhere, so it waited until cancelled). The control plane now asks GitHub which jobs wait for its runners: every five minutes for the repositories that had a job in the last day, and every sweep in the quarter of an hour after a move.
- After `superci planes move`, commands read the control plane moved to at once (before: the one moved away from, until `superci planes list` or the dashboard had looked again).
- `superci github_deliveries list` and `retrieve ID` show what GitHub says of the events it sent your control plane: when, what, how each was answered, and for one, which job, which address and what was answered. For a job that never got a machine.

## 0.11.1 — 2026-10-08
- `superci planes create`, with a control plane in use already: the new one no longer takes its place in what SuperCI keeps on your computer. In 0.11.0, commands run afterwards went to the new, empty control plane until the dashboard was opened again.
- After AWS has ended a sign-in that was SuperCI's only one, the dashboard and the commands that look go on working at the next start too (in 0.11.0 only until the program ended), and a change says that the sign-in has ended and to run `superci login aws`.
- A move to another control plane waits until the new one says every runner provider it was given is there, before GitHub and GitLab are switched over. Before, a job that arrived in the first minute could fail saying a provider was not added (seen with Cloudflare's containers).
- A command that would delete something and was not given `--confirm` says only that, without its resource's help after it.
- `superci login` says what it is waiting for and that it is done, without a line for each request of the browser.

## 0.11.0 — 2026-10-08
- SuperCI stays signed in. Its sign-ins to your clouds are kept in its own folder on your computer (`~/.superci`, readable by you alone), so the dashboard opens where you left it. An AWS sign-in still ends after twelve hours at most, as AWS has it; Cloudflare's no longer ends after an hour.
- Everything the dashboard does is also a command, for scripts and coding agents, in the shape of Stripe's CLI: `superci <resource> <operation> [id] [--param=value]`, with `list`, `retrieve`, `create`, `update` and `delete` wherever they fit. The resources: `jobs`, `planes` (also `move` and `allow`), `runners` (also `order`), `machine`, `limits`, `public_repos`, `github`, `gitlab`, `gitlab_projects` and `keys`; beside them `superci status`, `login`, `logout`, `dashboard` and `leave`. A command runs the same code as its page. `--json` prints one object; `--dry-run` checks a change and says what it would do; nothing is ever asked in the terminal; what would delete something needs `--confirm`; when a person is needed first (a sign-in), the answer names the command for it and the status is 3.
- The dashboard opens with `superci dashboard` (`npx @superci/cli dashboard`). `superci` alone lists the commands, and `superci help RESOURCE` (or `superci RESOURCE --help`) says more about one.
- `superci login [aws|cloudflare|modal]` signs in, in the browser, and ends; `superci logout` removes the sign-ins from this computer, asks Cloudflare to end its one, and has the control plane forget this computer's key. The dashboard's sidebar has the same under More → Sign out.
- After an AWS sign-in has ended, the dashboard still opens on your control plane and shows its jobs and settings (read with this computer's key), with a line saying the sign-in ended and a button to sign in again. Before, it would have shown the first screen as if nothing were known.
- SuperCI reads no other tool's credentials. `CLOUDFLARE_API_TOKEN`, `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` are no longer picked up; on a machine with no browser a sign-in is given by name: `SUPERCI_CLOUDFLARE_TOKEN`, `SUPERCI_MODAL_TOKEN_ID`, `SUPERCI_MODAL_TOKEN_SECRET`.
- The key the dashboard reads a control plane with lasts thirty days and is reused between runs (before: a new one, twelve hours, at every start).
- Keys that only read, for a coding agent or a script that should look and not change: `superci keys create agent` shows one once; with `SUPERCI_PLANE` and `SUPERCI_KEY` set, whatever looks (`superci status`, `superci jobs list`, `superci jobs retrieve ID`…) works with no sign-in on that machine, and nothing can be changed. Your control plane keeps only the key's SHA-256; a key ends by itself (30 days unless said otherwise) or with `superci keys delete`.
- `superci jobs retrieve ID` shows one job with the end of its log: GitHub's, read by your control plane with your App's token, or GitLab's. So why a job failed can be read without opening the code host.
- A change is seen at once on a control plane in AWS (before: up to fifteen seconds later, while it went on with the settings it had read). The dashboard and commands no longer wait there after each change, and a change made right after another starts from it.
- A job may run for six hours, as on GitHub's own runners (before: 70 minutes). Limits has a new setting, Longest job (`superci limits update --max-hours=N`), for another length: up to five days on AWS (the longest GitHub lets a job run on a runner of your own), a day on Modal; Cloudflare keeps a container six hours at most. A job's machine is ended at that limit, also when the control plane is gone.
- A release becomes what `npx @superci/cli` gives only once every one of its files downloads from npm, the launcher's too (npm lists a version minutes before it serves its file; for those minutes a new install failed).
- The dashboard has a new mark: four petals, as on superci.dev, in place of the two arrows.

## 0.10.7 — 2026-10-06
- `npx @superci/cli` works on Windows (x64, and arm64 through Windows' own x64 support). A release tries the package on both before it is published.
- Every push and pull request to the repository is built and tested on GitHub.

## 0.10.6 — 2026-10-06
- SuperCI is released by its repository on GitHub: one workflow builds the program for macOS and Linux (Apple silicon, Intel, arm64, x64), runs the tests, tries it through `npx`, and publishes `@superci/cli` to npm, which trusts that workflow (trusted publishing). No person's npm login or token is part of a release.
- Nothing changes in the program or in a control plane.

## 0.10.5 — 2026-10-06
- Docker works in jobs on AWS: service containers, container jobs and `docker` steps. The machine image left Docker to root, so these failed; the runner's user is now let in before the runner starts, and Docker starts beside it.
- A GPU job in an AWS account that is allowed no GPU machines fails at once saying which allowance to raise, also when spot machines were refused for another reason (before: after three tries, with "AWS had no room").
- Both found in the first runs on real clouds.

## 0.10.4 — 2026-10-06
- Back from creating and installing the GitHub App, the dashboard reads your control plane fresh and keeps looking until it says the App is installed (a control plane takes a few seconds to see what was just stored). Before, Overview could go on saying "Connect repositories" after GitHub was connected.
- A page shown from what the dashboard read a moment ago looks again once that has been read anew, so it does not stay on what was true before.

## 0.10.3 — 2026-10-06
- Set up on Overview does nothing in place: each step says where it stands in a line and has one button to the page where it is done (Set up control plane, Connect repositories, Add runners, Open Workflows).
- Deploying a control plane has its own page, Setting up your control plane, with its steps; when it is done you are back on Overview. Overview says a deploy is running in one line, with a link there.
- The deploy's steps say what each one is (its permissions, its storage, its check for waiting jobs and leftover machines every 2 minutes on AWS).
- Runners: the AWS on-demand row says "one per job, never interrupted". Like every machine, an on-demand one is ended when its job ends.

## 0.10.2 — 2026-10-05
- Before there is a control plane, Overview is the page it will be: Set up with its first step open (one button, Set up control plane) and the steps after it in one row, then the day's figures and the latest jobs, still empty.
- An Overview with no jobs yet shows the same shapes at zero instead of one line.
- Dark mode is darker: near-black with neutral greys and white text, as on supercov.com.

## 0.10.1 — 2026-10-05
- When AWS has no spot machine for a job, the order of your runner providers decides what happens: the job goes to the next provider in the list that can run it. AWS on-demand is a row of its own there (Runners), right under AWS until you move it, with its own limits (jobs at once, a monthly budget) and a switch to turn it off.
- The same when a provider cannot start a job's machine for any other reason (Cloudflare or Modal failing, AWS refusing the request, a provider refusing what the job asks, like GPUs): the job goes to the next provider that can run it instead of trying the same one again. When none can, each one's answer is said; the job is tried again twice, starting with the providers that have not failed it, then fails.
- With nothing below that can run the job (on-demand off, no other provider that fits), it waits for a spot machine instead of failing.
- A job run again after AWS took its spot machine back, and jobs started while spot is paused after several were taken, also go to the next provider below AWS's spot machines (before: always on-demand).

## 0.10.0 — 2026-10-04
- The name is SuperCI. The program is `superci`, the label is `runs-on: superci` (with sizes: `superci-8cpu`, `superci-gpu`, …), and what it makes in your clouds is named after it: control planes `superci-plane-<id>`, runner agents `superci-runners-<id>`, the GitHub App `SuperCI <owner>`, the tag `superci-plane`.

## 0.9.37 — 2026-10-04
- Several GitLabs on one control plane: Repositories → Another GitLab adds another server, or another account's token on the same one, beside the first. Each is a row with its own settings (its projects, a new token, Disconnect) and its own line under Permissions; their jobs run on the same runners. Jobs and runners of different GitLabs are kept apart even when their numbers are the same.
- A GitLab connection's token can be replaced in its settings (New token) without disconnecting: the projects turned on stay on.
- A move takes every GitLab connection along.

## 0.9.36 — 2026-10-04
- Control plane → Permissions reads at a glance: each row says in a few words how it stands, and when there is something to do, one Review button opens its steps (what is needed and why, then the one button or link that does it).
- "Change region" beside AWS is a button, like the others in its row.
- Connecting GitHub (the first organization or another) and GitLab are dialogs of three steps: where it is (the public one, or your own, which asks its address), whose jobs or which token, then the button that does it. Their rows have one button each.
- AWS's sign-in page answering "400 Bad Request" (it does when the browser holds an AWS console sign-in that has ended; AWS's own `aws login` has the same fault): back in the dashboard, a notice says the two ways on: sign in to the AWS console in another tab and try again, or "Sign out of AWS and try again", which clears the old sign-in and goes on to the sign-in.
- Signing in with AWS from a Review dialog comes back to that dialog, with everything to allow.
- Signed in to another AWS account than the one a control plane's role is in: Permissions says which is which.
- An AWS sign-in that has run out (they last twelve hours at most) is forgotten and asked for again, not shown as an error on every action.
- After an update, while its cloud still answers with the version before (Cloudflare can take a few minutes with containers), the dashboard says "Restarting" and looks again by itself, with no Update button meanwhile.

## 0.9.35 — 2026-10-04
- Fixes found by a second review, of the three versions before:
  - A job whose runner was taken by another job gets a machine even if that other job's machine never comes up (before, it could stay queued).
  - Out of time while spot is refused: the job's next try goes straight to on-demand.
  - A job's end delivered twice by GitHub no longer ends the machine kept for the job that waits; a machine left idle by a cancelled job that takes another job is not ended under it.
  - A network of your own set or changed in the dashboard is used by the next job, not minutes later.
  - Two organizations of the same name on github.com and on a GitHub Enterprise Server are kept apart.
- Several jobs of one run taken back by AWS are run again together with that run's other failed jobs (GitHub takes one request for a run's new attempt).

## 0.9.34 — 2026-10-04
- GitHub Enterprise: an organization on your own GitHub Enterprise Server (3.10 or newer), or on GitHub Enterprise Cloud with data residency (yours.ghe.com), connects like one on github.com. When making its App, say where your GitHub is (Repositories → the field beside the name; empty: github.com). Its App, its jobs' runners and every link then use that server. One control plane can serve organizations on github.com and on your server at once.
- What it needs: the server can reach the control plane (it sends job events there), the control plane can reach the server's API, and runners can reach the server (on AWS, a network of your own does that for a server inside your network).
- A Cloudflare control plane hands its settings to its Durable Object in pieces, so many organizations' Apps fit.

## 0.9.33 — 2026-10-04
- GitHub's full image on Cloudflare (experimental) is checked: every piece against its SHA-256 in the image's index; the index against the address, when the address ends in the index's checksum; the reader program against the index. A piece that is not what the index says is never used.
- An image that cannot be loaded fails the job at once saying why, and what was set up is taken down (before, the job ran in the small image, without the tools it expected).
- The reader survives what goes wrong: a piece that cannot be fetched fails only the reads of it, and is tried again when read again; its cache gives way when the job fills the disk (the oldest pieces are dropped, and fetched again if read again).
- `image-reader pack <image file> <directory>` makes what to upload: the pieces, their checksums, the hot list, the reader, in a directory named by the index's checksum. `crates/image-reader/try.sh` tries all of it for real in a local Linux container.
- The image's environment (/etc/environment) is read as names and values, never run.

## 0.9.32 — 2026-10-04
- No room for a spot machine in any region: the job gets an on-demand one (the regions in order again) rather than waiting; its machine says "on-demand" in the job list.
- Spot machines taken back: Windows machines and GitLab jobs are handled too. A Windows machine ends the job's process so it fails at once; a GitLab job is run again by GitLab at once, on an on-demand machine. Several jobs of one GitHub run taken back are run again with one request.
- Your own network on AWS: Runners → AWS → Your own network takes a line a region (subnets, security groups, `private` for subnets without public addresses). Machines there start in your network, so jobs reach what it reaches and leave by its NAT. A network that is wrong is said, and no machine starts anywhere else. No new permission.
- Fixes found by a review of the last versions:
  - A job that passed just as AWS gave notice is not run again; a notice follows the machine to the job its runner took; re-runs survive GitHub not answering.
  - A failing runner that takes another job is said on that job, and that job's machine takes the first.
  - GitLab: a runner that takes another job's job no longer has its busy machine taken for one that never began; a job cancelled before a runner took it has its machine ended.
  - A machine that never began a job GitHub or GitLab no longer has waiting is ended, with none started again; a machine's age counts from its start, not from when its job arrived.
  - A network that cannot be read is an error (the job is tried again), never a reason to use AWS's default network.
  - Removing AWS deletes what lets the control plane start machines first, then its machines and network; a sign-in that ended is said before anything changes.
  - Adding an organization never replaces the first one's App on a guess; removing one forgets it even if GitHub will not uninstall its App; a move takes every App and waits for each.
  - `-windows` and GPU labels no longer take a default machine's arm64 with them.

## 0.9.31 — 2026-10-04
- Several GitHub organizations on one control plane: Repositories → Another organization makes a private App in that organization too (a private App belongs to one account), kept beside the first; its jobs run on the same runners, with the same limits. Each organization is a row (Install, Manage, Remove), and Control plane → Permissions says for each what its App asks for and what was accepted. Removing one uninstalls its App there; the App itself is deleted on GitHub.
- A move takes every organization's App along.

## 0.9.30 — 2026-10-04
- Cloudflare (experimental): jobs can run inside GitHub's full runner image, which a container's 20 GB disk cannot hold. Runners → Cloudflare → GitHub's full image takes the address the image is published at; the container shows it as one file and fetches only what a job reads (several pieces at once, the commonly used ones first), with a writable layer on its own disk. GitHub's runner starts inside it as on GitHub's machines (same tools, tool cache and environment). Empty: the small image, as before; if the image cannot be reached, the small image is used.
- New program `image-reader` (Linux): shows an image kept as small pieces at a public address as one local file.

## 0.9.29 — 2026-10-04
- AWS taking back a spot machine mid-job: the machine tells the control plane and stops its runner, so the job fails at once (GitHub otherwise waits many minutes for a runner it no longer hears from). Once the run has finished, that job is run again as a new attempt (with the jobs that need it), on an on-demand machine. GitHub's App needs Actions (write) for that: Control plane → Permissions says how; without it the job says it was not run again, and why.
- Two spot machines taken back within a quarter of an hour: jobs start on on-demand machines for half an hour.

## 0.9.28 — 2026-10-03
- AWS spot machines try each zone of a region, cheapest first, also in the region's default network (before, AWS picked one zone, and a full zone meant the next region).
- A job AWS had no room for says what each region answered; "no GPU quota" only when every region refused for quota.

## 0.9.27 — 2026-10-03
- Workflows → Switch workflows: a prompt to paste into your coding agent (Claude, Codex, Copilot, Gemini) that moves a repository's jobs to SuperCI: every workflow, or one workflow first. Short prompts: what to change, what stays on GitHub (with why), one pull request (or GitLab merge request) with a table of the changes.

## 0.9.26 — 2026-10-03
- Signing in with Modal: Modal's page does not come back to the dashboard by itself, so while it waits every page looks again and notices the approval; Control plane → Permissions says to approve in Modal's tab and close it.

## 0.9.25 — 2026-10-03
- Runners → a provider's settings → Remove: jobs stop going there at once, then what SuperCI made for it in that cloud is deleted with your sign-in there (AWS: its machines, their network and the control plane's role; Modal or Cloudflare: the runner agent). The control plane and your sign-ins stay; adding it back is Add provider. Not signed in there: sign in from the dialog, or remove it from SuperCI only (what stays is listed). Jobs running there are stopped, after saying so.
- A control plane in AWS can remove its AWS runners too (its own role keeps its control-plane policy) and add them back.
- Control plane → Permissions: Modal's runners work without a sign-in; it says so.

## 0.9.24 — 2026-10-03
- The runner that fails a job on GitHub (see 0.9.15) is tried on each place in turn, and again each minute for half an hour while GitHub still has the job queued; before, a container that would not start left the job queued for a day.
- Overview: jobs from computers (from before they were set aside) are named so.

## 0.9.23 — 2026-10-03
- A job whose machine would not start, three times, now fails on GitHub saying why, instead of staying queued there for a day.
- A cloud refusing what a job asks (Modal: GPUs need a payment method on the workspace) fails it at once with Modal's own words.
- AWS GPU jobs say which quota to raise even when a later region has no GPU image.

## 0.9.22 — 2026-10-03
- GPUs: `runs-on: superci-gpu` gets one NVIDIA GPU (the least costly: T4), or name one: `superci-l4`, `-a10g`, `-l40s` on AWS or Modal, `-a100`, `-h100`, `-h200`, `-b200` on Modal. AWS uses RunsOn's image with NVIDIA's drivers; Modal's sandboxes get the GPU, priced per second.
- Windows: `runs-on: superci-windows` runs on AWS (Windows Server 2025, x64), spot by default, priced at Windows' rates.
- An AWS account that allows no GPU machines (AWS's quota for them starts at 0) fails GPU jobs at once, saying which quota to raise, instead of trying for a day.
- The machine picker (Workflows) offers Windows and GPUs.

## 0.9.21 — 2026-10-03
- AWS machines start in a network of their own per control plane and region, with nothing let in: jobs can't reach each other or your other machines in AWS. The dashboard makes it when AWS is connected, when a region is added, with an update, or with Control plane → Permissions → Review; it is removed with the control plane. Until then, machines use the region's default network as before.
- AWS spot machines start in the region's cheapest zone for their type, then the next when it has no room.
- GitLab: each job gets a runner of its own, paused once it takes its job and removed after (runners were shared by jobs with the same tags). Runners from before are removed.

## 0.9.20 — 2026-10-03
- Your own computers are coming soon: until jobs on them can be kept apart from the computer as reliably as on a cloud, they can't be added. The computer agent (`superci join`) and its routes are gone; a macOS job fails at once, pointing to GitHub's macos-latest.

## 0.9.19 — 2026-10-02
- A macOS VM's runner works in a folder beside itself (macOS has no /home/runner).

## 0.9.18 — 2026-10-02
- Your Mac runs macOS jobs in a VM of its own per job (Tart, on Apple's virtualization; Cirrus Labs' plain macOS image), deleted after the job, so nothing a job does stays on the Mac. Two run at once at most. Jobs wait while the Mac fetches the image (once, about 27 GB).
- Running jobs on a computer itself (as you, with your files) is off unless turned on for that computer.
- A computer's jobs run in a folder without spaces in its path (GitHub's runner does not quote a step's script).

## 0.9.17 — 2026-10-02
- `superci-macos` means Apple silicon (arm64), as GitHub's macos-latest does; `superci-macos-x64` asks for an Intel Mac.
- A macOS job with no Mac added fails saying so, and how to add one (or to use GitHub's macos-latest).

## 0.9.16 — 2026-10-02
- Workflows → Limits lists the public repositories GitHub's App is installed on (jobs come only from those), each allowed with one click, instead of typing names.

## 0.9.15 — 2026-10-02
- A job nothing here can run (a machine no place has, like macOS with no Mac added; one larger than allowed; a place not added) fails at once on GitHub, saying why, instead of waiting a day: a small runner takes it and fails it before any step. A GitLab job like it is cancelled.
- A job waiting for a computer that went offline (the only place that can run it) fails after five minutes, the same way.

## 0.9.14 — 2026-10-02
- Control plane → Permissions: for AWS, GitHub and GitLab, whether they allow what this version asks for, or what is missing, why, and how to give it. AWS's role is read while signed in to AWS and given what it lacks with Review → Allow; not signed in, what AWS refused lately is shown. GitHub: the App's and each installation's permissions, with links to give them. GitLab: the token's scopes.
- An update lists the permissions it also asks for (sidebar and Changelog), and gives AWS's what it lacks when signed in to AWS.
- AWS's permissions are made from one list in the code, each with what it allows, why and since when; the CloudFormation template (no longer used) is gone.

## 0.9.13 — 2026-10-02
- Each provider on Runners shows this month as its cloud bills it: Cloudflare's containers after the usage Workers Paid includes, Modal's apps after the plan's credits (from Modal's own billing), AWS at the prices it billed.
- An AWS machine's cost includes what it sent (CloudWatch's count, at AWS's price for data sent out), added a few minutes after it ends.
- A Modal control plane no longer keeps a container up around the clock: its sweep's container goes as soon as it is done (it cost about $5 a month idle).

## 0.9.12 — 2026-10-02
- Costs are settled when a job ends. AWS: at the prices AWS billed for its time — its zone's spot price for each hour (or the on-demand list price), its disk at the region's gp3 price, and its public address ($0.005/h) — a minute at least. Cloudflare: as Cloudflare metered its container, read by the dashboard while signed in to Cloudflare. Until then a cost is marked as estimated (≈).
- Modal sandboxes are sized in CPUs as GitHub counts them (two to one of Modal's cores) and held to what they ask for, so each is billed exactly its size.
- The comparison with GitHub uses GitHub's arm64 prices for arm64 jobs; job costs show to the hundredth of a cent.

## 0.9.11 — 2026-10-02
- Jobs from public repositories are refused unless the repository is allowed (Workflows → Limits); runs from a fork's pull request and `pull_request_target` runs are refused even then.
- A label asking for a machine larger than allowed (32 CPUs unless set otherwise in Workflows → Limits) is refused; every cloud runs at most 20 jobs at once unless set otherwise.
- Monthly budgets count on-demand AWS machines (estimated from their size) and the time a runner waits for its job; a Cloudflare container's wait counts its memory and disk (CPU is billed as used).
- An AWS machine powers itself off at its time bound twice over (a timer and a scheduled shutdown), so a job ending the timer no longer keeps it up.
- A control plane that moved forwards only events signed by GitHub or GitLab; job history older than two months is removed.
- The dashboard answers only on this computer's own address and makes changes only from its own pages.

## 0.9.10 — 2026-10-02
- Moving to an AWS or Modal control plane switches over without waiting for it to read its settings again (it reads them fresh for a move's steps); a move brings Cloudflare's Location along.

## 0.9.9 — 2026-10-02
- Changes saved for a Cloudflare control plane take effect in seconds (its Worker hands the Durable Object the settings as they are now; the Durable Object kept the ones it started with for up to half a minute).

## 0.9.8 — 2026-10-02
- A runner that takes a job other than its own (GitHub gives a waiting job to any runner with the label) no longer leaves its own job waiting forever when the other job's machine had failed: the job it was started for gets a new machine.

## 0.9.7 — 2026-10-02
- Cloudflare has a Location in its settings: where its containers start (eastern North America by default, near GitHub and GitLab; another area; or Automatic, where Cloudflare chooses).
- Region and location lists mark the one closest to the code hosts you connected (GitHub, gitlab.com).

## 0.9.6 — 2026-10-01
- A Cloudflare control plane takes a new dashboard session's key at once (its Worker reads the secrets fresh and vouches for the key; the Durable Object kept the secrets it started with for about 15 seconds).
- After a move or an update the page reads afresh, so the one in use shows at once; a move says where it went.
- AWS's regions are a list in its settings: drag to reorder, add one, remove one.
- Saving a dialog shows Saving… on its button until it is done; a control plane that does not answer for a moment (a Worker restarting after a setting changed) keeps its last view, so its runners do not seem to vanish.
- Reordering runner providers saves in place ("Saving the order…", then "Order saved"); the page does not reload.

## 0.9.5 — 2026-10-01
- A faster first look: the dashboard searches every cloud at once and hands each control plane its key at once; an AWS or Modal control plane takes a new dashboard key at once (it read its settings every 15 or 10 seconds before).
- An AWS control plane runs with 1024 MB (it had 256 MB, about a seventh of a CPU): it starts and answers several times faster. Update sets it on one set up before.
- No more flicker while a page waits: it is redrawn only when something changed.
- Overview has no control plane chip; Workflows' default machine is one card, and Choose a machine… picks another.

## 0.9.4 — 2026-10-01
- A job whose label names a runner provider this control plane has not added says so plainly ("its label needs Cloudflare runners, which this control plane has not added").
- Workflows: the default machine first; then the steps for GitHub Actions or GitLab CI; another size from Make a label…, a dialog.
- States read as words with a capital ("Connected", "Ready"); the ⋯ menu no longer nudges its row.

## 0.9.3 — 2026-10-01
- Moving is non-destructive and runs in the background, its steps in its row. Your runner providers come along (Cloudflare's containers, Modal's sandboxes, AWS), and so does job history; nothing is removed from the old control plane, so you can move back. Move here first says what comes along and what needs a sign-in.
- The switch is safe: the new control plane waits on standby until GitHub and GitLab are pointed at it, and the old one passes on any event that still reaches it.
- Delete a control plane that is not in use (its Worker, function or app and storage, and the runner agents made for it); Stop using SuperCI lets GitHub and GitLab go and deletes everything from your clouds.
- Control plane is one list: the one in use, others to move to, and each cloud without one with Set up (its progress in its row). Delete and Stop using SuperCI are in each one's menu (⋯).
- Overview: no "Needs a look" list (each job's row says what went wrong); GitHub's price behind a small mark; a quiet line when there are no jobs yet; pages no longer flash "Reading your control plane" when one is slow to answer.

## 0.9.2 — 2026-10-01
- One control plane is in use at a time. Another (in another cloud or account) is somewhere to move to: Move here brings your GitHub App and its installations, GitLab and its projects' webhooks, the order of your runner providers, the default machine and your computers; jobs running at the time finish where they are.
- Every control plane answers to the same label (`runs-on: superci`), so workflows do not change on a move. A cloud account holds one control plane.
- A job whose machine's start was cut off midway (the control plane stopped while a cloud answered) is placed again after two minutes, twice at most; before, it stayed queued until GitHub redelivered its event.
- Your computers follow a move by themselves. Runner providers tied to a control plane (AWS, Cloudflare, Modal) are added again on the new one; Runners says which.

## 0.9.1 — 2026-10-01
- Jobs keep their names and their workflow's (GitLab: their stage), so lists say "test · app · ci" rather than numbers.
- Overview, rethought: jobs in a table (name, runner in words, time, cost, how it ended); what needs a look grouped by what happened, in plain words; where jobs ran only when more than one provider ran them. The control plane is one quiet chip with its own icon.
- Cloudflare's errors are recorded by their message.

## 0.9.0 — 2026-10-01
- AWS machines can go to several regions in order: when one has no spot capacity (or quota) left, the next. Adding AWS sets US East (N. Virginia), closest to GitHub with the most spot capacity, then Ohio and Oregon; change them in AWS's settings on Runners.
- Adding AWS says what your account may run there (its spot CPU quota, as jobs of 4 CPU) and links to AWS's page to ask for more when it is low. Regions show their full names.
- Workflows, in the sidebar: how jobs ask for your runners, for the code hosts you connected, with the default machine and the picker for another.
- The control plane and What's new are pages of their own (Settings before); Overview says which line is your control plane.

## 0.8.1 — 2026-10-01
- A machine that fails to start (Cloudflare's "temporarily unavailable", a spot shortage) is tried again, twice, instead of failing the job; a start always gets a fresh container.
- Overview, after supercov's report: the control plane and your label on one quiet line; Jobs (how many got a machine, the median wait for one, what they cost against GitHub's runners, what needs a look, the latest); where jobs ran.
- Every page waits in the shape of its own cards, the loader inside the first.
- Overview's measures: jobs in the last day with a chart by hour (those that ran, those that did not), the median wait for a machine, and spend next to GitHub's price for the same jobs. Pages' headers no longer float.
- Repositories: a Use it card switching between GitHub Actions and GitLab CI, with the steps for each; GitLab's projects and connection are its own settings page (Settings on its row).
- GitLab projects say when GitLab paused or disabled their webhook, and when the last event from GitLab arrived; Turn on all.

## 0.8.0 — 2026-10-01
- GitHub jobs with `services:` and `container:` (and `docker://` steps) run on Cloudflare: Docker starts when a job first uses it, and containers share the machine's network (Cloudflare has no bridge networks). Service names and `job.services.<name>.ports` work as on GitHub; two services on one port would collide.
- Runner pools are called runner providers (Add a provider).

## 0.7.0 — 2026-10-01
- GitLab CI, next to GitHub: connect GitLab with a token (made on GitLab in one click, with the scopes ticked), then turn projects on. Jobs whose `tags:` name your label (`tags: [superci-8cpu]`) run on your runners; GitLab's own runners keep the rest.
- GitLab jobs run in Docker, as on GitLab's own runners: on AWS spot machines, Cloudflare containers (services on `localhost`) and your computers (macOS jobs on the Mac itself). Modal's sandboxes have no Docker, so they skip GitLab jobs and say why.
- One project runner per set of tags is made once and reused (GitLab has no runner for one job); each machine takes one job and ends.
- Repositories, in the sidebar: where jobs come from, GitHub and GitLab in one list (GitLab's connect form opens in its row, its projects below), and how a workflow asks for your runners on both. Overview's second step offers either.

## 0.6.0 — 2026-10-01
- Modal as a control plane: a web endpoint in your Modal workspace, its state and settings in Modal Dicts, a sweep every minute. Sign in with Modal on the connect screen and deploy (about a minute).
- A Modal control plane starts Modal sandboxes for jobs itself (Add runners → Modal → Add), sized per job.
- `/health` also says the control plane's runs-on label.

## 0.5.2 — 2026-10-01
- Cloudflare containers run until their job ends: under per-container scheduling Cloudflare stopped a container soon after its Durable Object went idle, which would have cut longer jobs short.
- Spend counts only what machines were paid for: a container from when its job began (one that never began cost nothing), an AWS machine from its launch.
- GitHub's price for the same jobs follows each job's size (2-core $0.006 a minute, 4-core $0.012, and so on) and shows as a small line under what was spent.

## 0.5.1 — 2026-10-01
- A machine that never begins its job (a container that does not start, a spot machine that does not boot) is tried again, twice, then the job fails saying why, instead of staying queued.
- A Cloudflare container that cannot start says so as the job's error.

## 0.5.0 — 2026-10-01
- Cloudflare containers are sized per job (Cloudflare's per-container scheduling): 1 to 4 CPU, up to 12 GB and 20 GB disk. The standard is the largest, 4 CPU and 12 GB, since Cloudflare bills CPU only while it is used.
- Memory a label leaves out is each pool's usual amount: 4 GB per CPU, up to what the pool has (12 GB on Cloudflare). The machine picker shows what each pool gives.
- Cloudflare containers start in eastern North America, near GitHub, and run 20 jobs at once unless a pool's settings say otherwise.
- Updating a control plane waits while jobs run on Cloudflare (the update gives its containers a new setup).

## 0.4.0 — 2026-10-01
- The default machine has its own line and dialog: what plain `runs-on: superci` gets, and any part a label leaves out.
- The machine picker starts at Default for everything and names only what you pick, so `superci-4cpu` always means 4 CPU, whatever the default.
- A label that asks for CPU but not memory gets 4 GB per CPU, not the default machine's memory.

## 0.3.0 — 2026-10-01
- Runner pools: each cloud, and each of your computers, is a pool of its own. Drag pools to reorder them; each has its settings (jobs at once, a monthly budget; a computer is removed there).
- Machines: pick CPU, memory, disk, architecture and system to get the `runs-on` label, and see which pools can run it. Use as default sets what plain `runs-on: superci` gets.
- Rules by repository are gone from the page: a workflow picks a pool in its label instead (`runs-on: superci-aws`).

## 0.2.0 — 2026-10-01
- Machine sizes in the label, in any order: `runs-on: superci-8cpu`, `-32gb`, `-100disk`, `-arm64`, `-macos`, `-ondemand`, or a place (`aws`, `cloudflare`, `modal`, `machine`).
- A default machine for `runs-on: superci`, set on the Runners page.
- Places in order, each with jobs at once and dollars a month. When every place is full, jobs wait for one.
- Your own computers: run `superci join` with a link from the dashboard. Linux jobs run in Docker, macOS jobs on the Mac.
- A computer that goes offline: jobs it had not begun move to the next place.
- The dashboard says when your control plane needs an update, and updates it.

## 0.1.0 — 2026-09-29
- A control plane in your own Cloudflare or AWS account, set up from the dashboard.
- Runners in Cloudflare containers, AWS spot machines and Modal sandboxes.
- A GitHub App made and installed from the dashboard.
