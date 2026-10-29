//! SuperCI's commands: what the dashboard's pages do, from a terminal or a coding agent. A command that changes
//! something fills in the form its page has and hands it to the dashboard's own code (`Dashboard::act`), so the two
//! cannot differ; one that reads shows what the control plane says. Nothing is asked in the terminal (but a secret's
//! value, typed hidden by a person at one): what a person must do first (a sign-in, in the browser) is said, with the command for it, and the status is 3.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::dashboard::{day_of, job_start_lines, job_words, plane_json, read_keys, run_words, runs_of, with_hosts, Background, Dashboard, Task, AWS_ENDED, NOT_SIGNED_IN};
use crate::plane::Plane;
use crate::view::{PlaneView, DASHBOARD_VERSION};
use crate::store;

/// What help is made of: a resource by its name, its commands (a resource, an operation, then what it takes), and
/// what more `superci help <resource>` says. The shape is the Stripe CLI's: `superci <resource> <operation> [id]
/// [--param=value]`, the operations `list`, `retrieve`, `create`, `update` and `delete` wherever they fit. A cloud or a
/// code host is never a command of its own (there will be many of each): it is what an operation takes (`superci
/// runners create aws`, `superci accounts retrieve aws`, `superci connections create github OWNER`).
const HELP: &[(&str, &str, &str)] = &[
    ("dashboard", "  superci dashboard                 Opens your dashboard in the browser (on this machine only, while it runs).",
        "Set up or find SuperCI in your cloud, connect repositories and runner providers, and see your jobs. It opens signed in\nonce you have signed in before. Ctrl-C stops it; your runners keep working without it. With several workspaces it opens on the\none shown last, and the name at the top of its sidebar switches between them.\n\n  --no-browser    Prints its link without opening it."),
    ("login", "  superci login [CLOUD]             Signs in with a cloud in the browser (aws, cloudflare, modal), and ends.",
        "A person signs in once; SuperCI keeps that sign-in in its own folder (~/.superci, by workspace) and commands use it\nfrom then on.\nWithout a cloud, the first screen asks which. AWS ends a sign-in after twelve hours at most: looking still works\nafter that, and a change in AWS asks for `superci login aws` again. A sign-in adds an account beside the others:\n`superci login aws --add` signs in to one more AWS account; `superci login modal --add`, to one more Modal\nworkspace, whose runners are then a provider of their own (`superci runners create modal --account=WORKSPACE`).\n\n  --no-browser    Prints the link without opening it.\n  --add           One more AWS account or Modal workspace, beside those signed in to."),
    ("logout", "  superci logout                    Removes SuperCI's sign-ins from this machine (the workspace's).",
        "It also asks Cloudflare to end its sign-in, and the control plane to forget this machine's key. Your control plane\nand runners keep working."),
    ("status", "  superci status                    The control plane in use: where, its version, whether jobs can run.",
        "With --json: the control plane, and under \"status\" everything it says of itself (repositories, runner providers,\nlimits, the latest jobs)."),
    ("jobs", "  superci jobs list [--run=ID]\n  superci jobs retrieve ID [--bytes=N] [--out=FILE] [--tests]\n  superci jobs retrieve --run=ID [--job=NAME] [--out=FILE] [--tests]",
        "list      The latest jobs: each one's id, how it ended, where it ran, how long, its cost, and why it failed. --run=ID:\n          that run's jobs only.\nretrieve  One job: how its start went step by step (what AWS was asked and answered, when its machine booted, when\n          its runner listened), and the end of its log: GitHub's (read with your App's token) or GitLab's, as far back as --bytes\n          says (200000 unless given, four million at most). --out=FILE: its whole log into FILE, whatever its size. GitHub keeps a job's log once the job has ended.\n          --tests: its tests instead, from its whole log: the ones that failed, each with its file and error, and how many\n          passed, failed and were skipped (what Vitest, Jest, Playwright, pytest, cargo and Go say in their output)."),
    ("runs", "  superci runs list\n  superci runs retrieve ID [--tests]\n  superci runs create --workflow=FILE [--job=NAME] [--event=push|pull_request|workflow_dispatch] [--repo=OWNER/NAME] [--quiet] [--detach]\n  superci runs wait ID [--quiet]\n  superci runs cancel ID",
        "A run is a workflow's jobs together (on GitLab, a pipeline's).\n\nlist      The latest runs: each one's id, how it stands, how many of its jobs ran here, how long it took (from its\n          first job queued to its last one over), what its machines cost together, and the job that was over last:\n          the one the run waited for.\nretrieve  One run with its jobs (every attempt of it, when it was run again). --tests: each failed job's tests too,\n          from its whole log: the ones that failed, with their files and errors, and where its artifacts are.\ncreate    Runs a workflow's job from this computer, as GitHub would run it, on a machine as any job here: the working\n          tree as it is (what git tracks, and new files it does not ignore), with nothing pushed. --workflow=FILE runs its\n          jobs (each combination of a matrix a job of its own, each job once those it needs have ended); --job=NAME\n          runs that job and those it needs. --event=push|pull_request|workflow_dispatch (push unless given), with the\n          payload GitHub would send (`github.event`) made from the working tree: --label=NAME, as often as needed,\n          a pull request's labels (added: its action labeled); --input=NAME=VALUE, a manual start's inputs (the rest\n          their defaults); --event-payload=FILE, JSON put over the payload. And what GitHub would give it and cannot\n          here: --secret=NAME=VALUE and --var=NAME=VALUE, as often as needed. --repo=OWNER/NAME: the GitHub repository
          it is of, when the checkout's origin is not GitHub's (else its cache is its own, apart from GitHub's jobs').\n          It waits for the run and prints its jobs' lines, then each step's result and time and what its machines cost;\n          it fails when the run does.\n          --quiet: each step's result, and the last lines of one that failed, not every line (for a coding agent).\n          --detach: says the run's id and ends; `superci runs wait ID` follows it.\nwait      Follows a run from a computer to its end, as create does.\ncancel    Stops a run, as GitHub's Cancel does: its jobs not started end (one whose `if` holds after a cancel, as\n          `always()`, runs), and a running one is told to stop (its steps have five minutes), then its machine ends. A GitHub run is cancelled by GitHub, asked with your App's token.\n\nOnly the jobs SuperCI ran are counted: a run's jobs on other runners are not known here."),
    ("planes", "  superci planes list\n  superci planes create aws --region=us-east-1 [--account=ID] | cloudflare [--account=ID] | modal\n  superci planes update [ID]\n  superci planes move ID\n  superci planes allow [ID]\n  superci planes delete ID --confirm",
        "list      Every control plane found in the clouds signed in to. One is in use at a time.\ncreate    Puts a control plane in a cloud you are signed in to. Nothing else is needed first.\nupdate    Brings the control plane in use (or the one named) to this program's version.\nmove      Moves to another control plane you created: settings, history and runner providers go along, then\n          GitHub and GitLab are switched over. No job is lost.\nallow     Gives its AWS role what this version asks for (the dashboard's Control plane → Permissions → Allow).\ndelete    Deletes one that is not in use, with what was made for it in its cloud."),
    ("runners", "  superci runners list\n  superci runners create aws [--region=us-east-1] | cloudflare [--account=ID] | modal [--account=WORKSPACE]\n  superci runners update CLOUD [--max-jobs=N|none] [--monthly-usd=N|none]\n                               [--regions=a,b,c] [--networks=\"REGION subnet-… sg-… [private]\"|none]\n                               [--location=enam|weur|auto|…] [--image=https://…|none]\n  superci runners order CLOUD...\n  superci runners delete CLOUD --confirm [--only-here] [--stop-jobs]",
        "list      Runner providers in order, with their limits.\ncreate    Lets a cloud you are signed in to run jobs. --region: where AWS machines start, for a control plane that\n          is not in AWS itself. modal --account: a further Modal workspace signed in to (`superci login modal\n          --add`); its launcher is a provider of its own, modal:WORKSPACE, with its own limits and spend.\nupdate    A provider's settings: jobs at once, dollars a month (`none` lifts one), and where it starts machines:\n          --regions for aws and aws-on-demand (each its own; aws's spot machines start where a job costs least, the\n          first counted 15% cheaper; aws-on-demand's in order), --location for cloudflare. --networks (AWS's, a\n          network of your own per region; give it once per region) and --image (Cloudflare's) are the account's.\norder     aws (its spot machines), aws-on-demand, cloudflare, modal: the ones named, in that order; any not named is\n          taken out (its settings kept). A job goes to the first that can run it and is within its limits; when one\n          cannot start it, to the next.\ndelete    Takes it out and deletes what SuperCI made for it there. --only-here: forgets it without deleting its\n          part (for when you cannot sign in there). --stop-jobs: also when jobs are running on it."),
    ("labels", "  superci labels list\n  superci labels create NAME\n  superci labels delete NAME --confirm",
        "The labels workflows name in `runs-on`: a list, `superci` alone unless you add others. A workflow may name any\nof them (`runs-on: acmeci`, `acmeci/cpu=8`).\n\ncreate    Adds NAME to the list: 2 to 24 lowercase letters and digits. It becomes the one shown in examples.\ndelete    Stops answering to NAME: jobs that still name it wait for a runner that never comes. The last one stays."),
    ("name", "  superci name retrieve\n  superci name update NAME",
        "What your CI is called where people read it: the dashboard's header, the GitHub App's name for an organization\nadded from then on, and what a job's page says when it could not be run. SuperCI unless you name it.\n\nupdate    Calls it NAME (\"AcmeCI\", \"Acme CI\"): 2 to 24 letters, digits, spaces and dashes. `SuperCI` puts it back.\n          The command stays `superci`; what workflows name is `superci labels`."),
    ("machine", "  superci machine retrieve\n  superci machine update [--cpu=N] [--ram=GB] [--disk=GB] [--disk-speed=MIBS] [--disk-iops=N] [--arch=x64|arm64]\n                         [--os=linux|windows] [--image=ubuntu26|ubuntu24|NAME] [--gpu=any|NAME] [--family=m8a+r8a] [--scratch=ramdisk|localdisk]\n                         [--provider=aws|cloudflare|modal] [--spot=true|false] [--docker-cache=true|false]",
        "The machine `runs-on: superci` alone gets. A job's label says another with the same keys, as key=value pairs\nafter its name: `superci/cpu=8/arch=arm64`. What a label leaves out is the default machine's.\n\nEach flag is a key (see superci.dev/docs/labels). `--KEY=default` leaves one to each provider's standard again.\n\n--disk-speed  On AWS a disk's speed follows the machine's size (31.25 MiB a second for each CPU, up to 1,000; the\n              extra is in a job's price). 125 is what AWS includes at no extra price; a number sets it (125 to\n              2000).\n--spot        false: never a spot machine. --scratch: a job's files in memory or on a machine's own disk.\n--family      Only these AWS machine families. --provider: only this provider.\n--docker-cache  false: Docker as it comes, no images from the cache or kept for it; true: from the cache from a\n              job's start, even one whose last run used no images (Ubuntu 26.04 machines)."),
    ("limits", "  superci limits retrieve\n  superci limits update [--max-cpu=N] [--max-hours=N] [--monthly-usd=N|none] [--daily-usd=N|none]",
        "--monthly-usd The most the CI may spend on machines in a month, all providers together (no limit unless set).\n              At it no job starts until next month or a higher limit. A cloud account's own limit is\n              `superci accounts update aws --monthly-usd=N`; a provider's, `superci runners update`.\n--daily-usd   The same for a day (UTC): a day that runs away stops that day.\n--max-cpu     The largest machine a label may ask for (32 unless set). A label asking for more is refused.\n--max-hours   The longest a job may run: its machine is ended after this (6 unless set, as on GitHub's own runners;\n              120 at most). Whatever is set, a machine on Cloudflare lives 6 hours at most and one on Modal 24. A\n              workflow's timeout-minutes ends a job sooner."),
    ("public_repos", "  superci public_repos list\n  superci public_repos create OWNER/REPO\n  superci public_repos delete OWNER/REPO",
        "Public repositories allowed to run here. Jobs from public repositories are refused unless the repository is allowed.\nEven then, a fork's pull request and `pull_request_target` runs are refused."),
    ("runner_images", "  superci runner_images list\n  superci runner_images create NAME --reference=REGISTRY/IMAGE:TAG [--modal-secret=SECRET]\n  superci runner_images update NAME [--reference=REGISTRY/IMAGE:TAG] [--modal-secret=SECRET|--modal-secret=]\n  superci runner_images delete NAME",
        "Images of your own for jobs on Modal, with a repository's tools in them already: a job names one by its name in its\nlabel (`runs-on: superci/image=NAME`) and its sandbox starts from it in place of GitHub's runner image. Other providers\nrefuse such a job, saying why. A label is split at each /, so it says the name; the control plane keeps the address.\n\ncreate    NAME: 2 to 32 lowercase letters, digits and dashes (not beginning with ubuntu). --reference: where the image\n          is, as `docker pull` takes it. Make it FROM ghcr.io/actions/actions-runner: GitHub's runner\n          (/home/runner/run.sh) starts the job, and a job whose image has none fails, saying so. Modal keeps an\n          image by its address and does not fetch a tag pushed again: give each build a tag of its own, or its\n          digest (…@sha256:…). A public image needs nothing more. For a private registry, --modal-secret names a\n          Secret with REGISTRY_USERNAME and REGISTRY_PASSWORD (for ghcr.io, a token that reads packages), in each\n          Modal workspace that runs jobs (a further one, modal:WORKSPACE, reads its own); only its name is kept here.\nupdate    The same name, a new --reference (a new build's tag) or --modal-secret (empty: none), in one change:\n          a job that names it starts from the new image, and none finds the name missing meanwhile. What it does\n          not say stays as it is. A name there is not is refused.\ndelete    Jobs that still name it fail, saying so."),
    ("connections", "  superci connections list\n  superci connections create github OWNER [--host=https://github.example.com]\n  superci connections create gitlab --url=https://gitlab.com [--gitlab=ID|new]\n  superci connections delete github OWNER --confirm\n  superci connections delete gitlab [ID] --confirm",
        "Where jobs come from: the GitHub accounts and the GitLabs connected.\n\nlist      Each GitHub account with where its App is installed, and each GitLab.\ncreate    github: opens GitHub in the browser, where a person creates the App for OWNER (an organization, or your\n          own name) and chooses its repositories. GitHub offers that nowhere else. It ends when the App is\n          installed. --host: a GitHub Enterprise Server, or GitHub Enterprise Cloud with data residency.\n          gitlab: the token is read from SUPERCI_GITLAB_TOKEN (--token-env=NAME for another variable): a project,\n          group or personal access token with the scopes api, create_runner and manage_runner. It goes to your\n          control plane and is not kept on this machine. --gitlab=new: a further connection beside the first.\ndelete    github: a further organization; its App is uninstalled and forgotten (the first one goes with\n          `superci leave`). gitlab: its projects stop sending jobs here, and its token is forgotten."),
    ("deliveries", "  superci deliveries list\n  superci deliveries retrieve ID",
        "What GitHub says of the events it sent your control plane lately: for a job that never got a machine.\n\nlist      The newest hundred: when, what (a job queued, begun, ended), and how it was answered. A job that is in no\n          line was never sent by GitHub; the control plane finds such jobs by asking, within a few minutes.\nretrieve  One of them, with the job it was about, the address it was sent to and what was answered."),
    ("projects", "  superci projects list [--gitlab=ID]\n  superci projects update PROJECT_ID --enabled=true|false [--gitlab=ID]\n  superci projects update --all --enabled=true [--gitlab=ID]",
        "The projects a GitLab connection's token reaches (on GitHub, repositories are chosen where the App is installed).\n\nlist      Each with its id and whether its jobs come here.\nupdate    --enabled=true: the project's jobs with `tags: [superci]` run here from then on."),
    ("accounts", "  superci accounts list\n  superci accounts retrieve aws [--account=ID]\n  superci accounts update aws [--monthly-usd=N|none] [--daily-usd=N|none]\n                              [--aws-budget=none|emails|stops|lift] [--email=ADDRESS]",
        "list      The cloud accounts signed in to: each cloud, its account, and who is signed in.\nupdate    The most the AWS account may spend on machines in a month and in a day (UTC), its spot and on-demand\n          machines together (`none` lifts one). At a limit, jobs go to the providers of other accounts in the\n          order. The whole CI's limits are `superci limits update`.\n          --aws-budget: a budget in the AWS account itself for the monthly limit, made with your sign-in (the\n          control plane gets no permission for it). AWS counts the account's real bill, all of it and hours late.\n          emails: AWS writes to --email at 80% and at 100%. stops: that, and at 100% AWS refuses new machines to\n          the control plane until the next month. lift: takes that refusal off now. none: removes all of it.\nretrieve  What an AWS account allows of what SuperCI does there: the regions, spot and on-demand machines, how many\n          vCPUs it may run at once, and whether the image jobs run on is published there. It is asked with reads and\n          dry runs only: nothing is made, started or changed. An account in an AWS organization can be held to some\n          regions or kept from spot by the organization's policies, which nothing inside the account lifts; this\n          says so, with the policy and whose it is. --account: which, when signed in to several."),
    ("usage", "  superci usage retrieve [--period=day|week|month]",
        "What jobs used in the last 24 hours (day, unless given), 7 days or 30 days: how many ran, their machines' minutes and\nthe minutes GitHub or GitLab would bill (each job rounded up to a minute), what they cost, and what the code host's\nown runners of the same sizes would have charged."),
    ("artifacts", "  superci artifacts list --run=ID\n  superci artifacts retrieve NAME --run=ID [--out=DIR]",
        "What a run from a computer's jobs kept with upload-artifact, kept for a day. (A run of GitHub's keeps its artifacts at\nGitHub: `gh run download`.)\n\nlist      Each artifact's name and size.\nretrieve  Fetches one and unpacks it into DIR (./NAME unless given), as download-artifact would."),
    ("caches", "  superci caches retrieve\n  superci caches list [--repo=OWNER/NAME]\n  superci caches update [--enabled=true|false] [--size-gb=N] [--days=N] [--images=true|false]\n  superci caches delete --repo=OWNER/NAME [KEY] [--scope=REF] --confirm",
        "GitHub's cache for jobs on AWS, kept in a bucket of your control plane's in your account, in each region its machines\nrun in. Workflows do not change: actions/cache, the setup actions' caches and Docker's type=gha reach it as they reach\nGitHub's. A job reads and writes what GitHub would let it (a pull request writes only its own branch's).\n\nretrieve  Whether it is on, what it keeps, and each repository with what it keeps now.\nlist      Each repository with its entries and size; --repo: that repository's entries (key, branch, size, last use).\nupdate    --enabled=false: jobs use GitHub's own cache. --size-gb: what one repository keeps at most (25 unless set;\n          the least recently used go first). --days: how long an entry may go unused (7 unless set). An entry saved 30\n          days ago is saved anew whatever its use. --images=false: Docker pulls every image as it always has (they are\n          kept for each job's next run on Ubuntu 26.04 machines unless turned off).\ndelete    Takes a repository's entries out, all of them or KEY's (--scope: on that branch only, as refs/heads/main)."),
    ("secrets", "  superci secrets list\n  superci secrets create NAME [--repo=OWNER/NAME] [--branch=PATTERN] [--workflow=PATTERN] [--environment=NAME]\n  superci secrets update NAME [the same]\n  superci secrets delete NAME [the same] --confirm",
        "Secrets kept for runs from a computer (`superci runs create`), as a repository's secrets are kept for its runs on\nGitHub. A run gets the ones its workflows name (${{ secrets.NAME }}) and nothing else; one it names that is not kept is\nempty, as on GitHub, and said before its jobs start. Jobs GitHub sends keep getting GitHub's own secrets.\n\nThey are written with this computer's sign-in into your control plane's own cloud (SSM on AWS, the Worker's secrets\non Cloudflare, a Dict on Modal) and are never read back out: `list` says names, where each may be used, when it was\nset and last given to a run, and who started that run. A run key (`superci keys create NAME --runs`) starts runs\nthat get them; nothing but a computer signed in changes them.\n\ncreate    Keeps NAME (letters, digits and _; not GITHUB_…), its value typed hidden when asked in a terminal, else read from\n          stdin or the variable --value-env names, never from the command (`printf %s \"$NPM_TOKEN\" | superci secrets create NPM_TOKEN`).\n          4 KB at most. --repo, --branch, --workflow (a file's name or path) and --environment (a job's\n          environment:) narrow where it may be used; patterns are GitHub's (release/*). The same name may be kept\n          for several scopes: a job gets the narrowest (an environment's, then a repository's, a workflow's, a\n          branch's). A value given with the run (--secret=NAME=VALUE) wins over all of them.\nupdate    A new value for NAME as kept for that scope.\ndelete    Takes it out."),
    ("keys", "  superci keys list\n  superci keys create NAME [--days=30] [--runs]\n  superci keys delete NAME",
        "Read keys and run keys, for a coding agent or a script. (Each device that looks after the CI has a key of its own,\nits device key, listed here too.)\n\ncreate    Makes a read key and shows it once. With it and the control plane's address, anything that looks works with no\n          sign-in on that machine, and nothing can be changed:\n            SUPERCI_PLANE=https://… SUPERCI_KEY=superci_read_… superci jobs list\n          Only the key's SHA-256 is kept, in your control plane. It ends by itself (30 days unless --days says\n          otherwise, 366 at most).\n          With --runs, a run key: it also starts runs from a computer (`superci runs create`), and nothing else. Its jobs keep\n          to this control plane's limits, as every job does (`superci limits`). For a coding agent that should run the\n          tests without pushing and without your sign-in.\ndelete    Ends one at once: a read or run key by its name, a computer key by its DEVICE_KEY_… name."),
    ("images", "  superci images list [--account=ID]\n  superci images create REGION [--from=REGION:IMAGE] [--account=ID]\n  superci images update REGION [--account=ID]\n  superci images delete REGION --confirm [--account=ID]",
        "The image AWS machines start from (GitHub's own runner image, published for AWS by RunsOn in most regions).\n\nlist      Each region the AWS account can use: whether the image is published there, or your own copy is.\ncreate    Copies the image into a region it is not published in, so that jobs can start there. Where to copy\n          from is read from its maker's public list; --from names a region and an image instead. AWS takes about\n          ten minutes. The copy is kept in your account's storage there (about $1.50 a month).\nupdate    Copies the newest build in, where your copy is of an older one (its maker builds the image anew every\n          week or two; `list` says when one is out). Jobs start from the older copy until the new one is\n          there; the older one is then removed.\ndelete    Removes your copy from a region, with its storage."),
    ("workspaces", "  superci workspaces list\n  superci workspaces create NAME\n  superci workspaces update ID --id=NEW\n  superci workspaces update ID --plane=PLANE\n  superci workspaces delete ID --confirm",
        "A workspace has its own sign-ins to its clouds, and through them its own control plane, runners and repositories.\nNothing of one is seen from another. There is one (`default`) until you make more. With several, a command says\nwhich, by its id: --workspace=ID, or SUPERCI_WORKSPACE=ID for a whole terminal. A command that names none stops\nand lists them.\n\ncreate    Makes an empty one called NAME (AcmeCI, Build Farm), which its CI is called too. Its id is made from the\n          name (acmeci, build-farm). Then `superci login --workspace=ID`.\nupdate    Gives one another id. What it is called is `superci name update`.\ndelete    Signs it out and forgets it on this machine. Nothing in its clouds is deleted: its control plane and\n          runners keep working (`superci leave --workspace=ID --confirm` stops them first)."),
    ("leave", "  superci leave --confirm           Stops using SuperCI: what it made in your clouds is deleted.",
        "GitHub and GitLab stop sending jobs, then the control plane is deleted with what was made for it, and with it the\nones that serve nothing. Another one that serves a code host of its own is left as it is: `--plane=ID` stops that\none by itself. The GitHub App itself is deleted on GitHub; change `runs-on: superci` back in your workflows first.\nWith several workspaces it takes --workspace=ID on the command itself, never from SUPERCI_WORKSPACE."),
];

const FLAGS: &str = "  --param=value      What an operation takes (also `--param value`). An id or a name comes before the flags.
  --workspace=ID     Which workspace, when several are kept on this machine (see `superci help workspaces`).
  --json             One JSON object: what was done or read, or \"error\" with \"needs\" (the command a person runs first).
  --dry-run          Checks a change and says what it would do, without doing it.
  --confirm          Needed by what deletes something. Nothing is asked in the terminal but a secret's value.

Status: 0 done · 1 failed · 2 not a command, or not a whole one · 3 a person is needed first (see \"needs\").";

/// `superci help`, `superci help RESOURCE`, `superci RESOURCE --help`, and a resource alone.
pub fn help(topic: &str) -> String {
    match HELP.iter().find(|(word, ..)| *word == topic) {
        Some((_, usage, more)) => format!("{usage}\n\n{more}\n\n{FLAGS}"),
        None => {
            let group = |words: &[&str]| HELP.iter().filter(|(w, ..)| words.contains(w)).map(|(_, usage, _)| *usage).collect::<Vec<_>>().join("\n");
            format!("SuperCI — GitHub Actions and GitLab CI jobs on your own clouds

  superci <resource> <operation> [id] [--param=value]

{}

Jobs:
{}

Control plane:
{}

Runners:
{}

Repositories:
{}

Coding agents and scripts:
{}

Workspaces:
{}

{}

  superci help RESOURCE             More about one (also: superci RESOURCE --help).

{FLAGS}
SuperCI keeps its own sign-ins in ~/.superci (SUPERCI_HOME to put it elsewhere), by workspace, and reads no other
tool's. On a machine with no browser, a sign-in can be given by name: SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_MODAL_TOKEN_ID and
SUPERCI_MODAL_TOKEN_SECRET.", group(&["dashboard", "login", "logout", "status"]), group(&["jobs", "runs", "secrets", "artifacts", "usage"]), group(&["planes"]), group(&["runners", "accounts", "images", "runner_images", "labels", "name", "machine", "limits", "caches"]),
                group(&["connections", "deliveries", "projects", "public_repos"]), group(&["keys"]), group(&["workspaces"]), group(&["leave"]))
        }
    }
}

/// How a command did not do its thing: it was not a whole command (its resource's help follows), it would delete
/// something and was not told to, or it failed.
#[derive(Debug, PartialEq)]
/// A command that failed: not a whole command (its help follows), a change not confirmed, or one that failed, with what
/// it read on the way when there is that (a run that ended in failure, with its jobs), for `--json`.
pub enum Fail { Usage(String), Unconfirmed(String), Failed(String), Ended(String, Value) }

impl From<String> for Fail { fn from(e: String) -> Self { Fail::Failed(e) } }
impl From<&str> for Fail { fn from(e: &str) -> Self { Fail::Failed(e.to_string()) } }

impl Fail {
    pub fn said(&self) -> &str { match self { Fail::Usage(e) | Fail::Unconfirmed(e) | Fail::Failed(e) | Fail::Ended(e, _) => e } }
}

type Result<T> = std::result::Result<T, Fail>;

fn usage<T>(say: impl Into<String>) -> Result<T> { Err(Fail::Usage(say.into())) }

/// A run followed to its end: done when it succeeded, else a failure with what was read of it.
fn ended(done: Done) -> Result<Done> { if done.data["ok"] == false { Err(Fail::Ended(done.said.join("\n"), done.data)) } else { Ok(done) } }

/// What was typed: words, and flags with their values (`--region us-east-1`, `--region=us-east-1`; a flag given
/// twice keeps both).
pub struct Args { pub words: Vec<String>, flags: HashMap<String, Vec<String>> }

/// Flags that stand alone; every other flag takes the word after it.
const SWITCHES: [&str; 14] = ["json", "confirm", "dry-run", "no-browser", "all", "only-here", "stop-jobs", "help", "version", "add", "runs", "quiet", "detach", "tests"];
const VALUES: [&str; 60] = ["reference", "modal-secret", "label", "input", "event-payload", "docker-cache", "branch", "environment", "value-env", "images", "header", "extra-disk", "user-data", "period", "run", "out", "repo", "scope", "size-gb", "secret", "var", "workflow", "job", "event", "disk-speed", "workspace", "id", "from", "daily-usd", "aws-budget", "email", "enabled", "spot", "disk-iops", "gpu", "family", "scratch", "provider", "max-hours", "days", "bytes", "region", "account", "plane", "host", "url", "gitlab", "max-jobs", "monthly-usd", "regions", "networks", "location", "image", "cpu", "ram", "disk", "arch", "os", "max-cpu", "token-env"];

impl Args {
    pub fn parse(raw: &[String]) -> Result<Args> {
        let (mut words, mut flags) = (vec![], HashMap::<String, Vec<String>>::new());
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                if a == "-h" { flags.entry("help".into()).or_default(); } else if a == "-V" { flags.entry("version".into()).or_default(); }
                else if a.starts_with('-') && a.len() > 1 { return usage(format!("{a} is not a flag of superci")) } else { words.push(a.clone()) }
                continue
            };
            let (name, given) = match flag.split_once('=') { Some((n, v)) => (n, Some(v.to_string())), None => (flag, None) };
            if SWITCHES.contains(&name) { flags.entry(name.into()).or_default(); }
            else if VALUES.contains(&name) {
                let value = match given { Some(v) => v, None => match it.next() { Some(v) => v.clone(), None => return usage(format!("--{name} needs a value")) } };
                flags.entry(name.into()).or_default().push(value);
            } else { return usage(format!("--{name} is not a flag of superci")) }
        }
        Ok(Args { words, flags })
    }
    pub fn has(&self, flag: &str) -> bool { self.flags.contains_key(flag) }
    fn get(&self, flag: &str) -> Option<&str> { self.flags.get(flag).and_then(|v| v.last()).map(String::as_str) }
    fn all(&self, flag: &str) -> Vec<&str> { self.flags.get(flag).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default() }
    pub fn word(&self, i: usize) -> &str { self.words.get(i).map(String::as_str).unwrap_or("") }
}

/// What a command did or read: lines for a person, and the same as data.
pub struct Done { pub said: Vec<String>, pub data: Value }

/// How a command marks a line a job wrote among its steps: printed as it is, with nothing added.
pub const JOB_LINE: &str = "\u{1}";

impl Done {
    /// (Each caller has a line to say: a command that changes something says what changed.)
    fn said(line: impl Into<String>) -> Done { let line = line.into(); Done { data: json!({ "ok": true, "said": line }), said: vec![line] } }
}

/// The command a person runs first, when that is what an error asks for.
pub fn needs(error: &str) -> Option<&'static str> {
    if error == NOT_SIGNED_IN { return Some("superci login") }
    let has = |words: &[&str]| words.iter().any(|w| error.contains(w));
    if error == AWS_ENDED || has(&["Sign in with AWS", "Your AWS sign-in has ended", "ExpiredToken", "RequestExpired"]) { return Some("superci login aws") }
    if has(&["Sign in with Cloudflare", "Cloudflare sign-in expired", "Cloudflare sign-in has expired"]) { return Some("superci login cloudflare") }
    if has(&["Sign in with Modal", "Modal sign-in is no longer accepted", "Modal no longer accepts"]) { return Some("superci login modal") }
    if has(&["Sign in where this control plane runs", "sign in where it runs", "needs SuperCI's own sign-in"]) { return Some("superci login") }
    None
}

const CLOUDS: [&str; 3] = ["aws", "cloudflare", "modal"];
const NO_PLANE: &str = "There is no control plane yet. Set one up: `superci planes create aws --region=us-east-1` (or cloudflare, or modal).";

const ONLY_READS: &str = "SUPERCI_KEY only reads. A change needs SuperCI's own sign-in on this machine: `superci login`.";

thread_local! {
    /// The workspace this command is in (see `enter`). None: nothing is kept for it (no home folder, or a key that
    /// only reads, given by name, is all it uses).
    static IN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// One workspace, or several with none named.
enum Which { One(Option<String>), Several(Vec<String>) }

/// Which workspace a command is in: `--workspace=NAME`, then SUPERCI_WORKSPACE (not for `flag_only`: what deletes
/// everything takes it from the command itself); with neither, the only one. A name that is none of those kept here
/// stops the command and never falls through to another.
fn which_workspace(args: &Args, flag_only: bool) -> Result<Which> {
    let Some(root) = store::root() else { return Ok(Which::One(None)) };
    let var = std::env::var("SUPERCI_WORKSPACE").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).filter(|_| !flag_only);
    let (named, by) = match args.get("workspace").map(str::trim).filter(|w| !w.is_empty()) { Some(w) => (Some(w.to_string()), "--workspace"), None => (var, "SUPERCI_WORKSPACE") };
    match store::which(&root, named.as_deref()) {
        Ok(name) if named.is_none() || store::kept_here(&root, &name) => Ok(Which::One(Some(name))),
        Ok(name) => {
            let all = store::workspaces_in(&root);
            let here = if all.is_empty() { "the only one is default".to_string() } else { format!("here: {}", all.join(", ")) };
            Err(Fail::Failed(format!("{by} names the workspace {name}, and none is called that on this machine ({here}). Nothing was done. `superci workspaces list` lists them.")))
        }
        Err(all) => Ok(Which::Several(all)),
    }
}

fn several_workspaces(all: &[String], flag_only: bool) -> Fail {
    Fail::Failed(format!("{} workspaces are kept on this machine: {}. Say which: --workspace=ID{}. Nothing was done.", all.len(), all.join(", "), if flag_only { " (on this command itself)" } else { " (or SUPERCI_WORKSPACE=ID)" }))
}

/// Puts a command in its workspace, before it does anything. With several and none named it stops and lists them,
/// unless a key that only reads was given by name: that reads its control plane and uses nothing of a workspace.
fn enter(args: &Args) -> Result<()> {
    let flag_only = args.word(0) == "leave";
    let name = match which_workspace(args, flag_only)? {
        Which::One(name) => name,
        Which::Several(_) if given_key().is_some() => None,
        Which::Several(all) => return Err(several_workspaces(&all, flag_only)),
    };
    IN.with(|w| *w.borrow_mut() = name);
    Ok(())
}

fn signed_in() -> Dashboard { Dashboard::signed_in(IN.with(|w| w.borrow().clone()).and_then(|w| store::Store::of(&w))) }

/// A dashboard ready for a change: signed in, its clouds looked in, the control plane in use chosen.
fn ready() -> Result<Dashboard> {
    let mut d = signed_in();
    if d.signed_in_as().is_none() && given_key().is_some() { return Err(ONLY_READS.into()) }
    d.ready()?;
    named_is_in_use(&d)?;
    Ok(d)
}

fn in_use(d: &Dashboard) -> Result<Plane> { Ok(d.plane_in_use().ok_or(NO_PLANE)?) }

thread_local! {
    /// The control plane a command was told to act on (`--plane=ID`, or `SUPERCI_PLANE` with no key), and where that
    /// was said. It is the one the command acts on (`view::name`); checked again where the command is about to act,
    /// so that a name that matches nothing stops it and never falls through to another control plane.
    static NAMED: std::cell::RefCell<Option<(String, &'static str)>> = const { std::cell::RefCell::new(None) };
}

/// Notes what this command names (see `NAMED`).
fn name_plane(args: &Args) {
    let var = |n: &str| std::env::var(n).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let named = match args.get("plane").map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some((p.to_string(), "--plane")),
        // With a key the variable is the address that key reads (see `given_key`); alone it names the control plane.
        None if var("SUPERCI_KEY").is_none() => var("SUPERCI_PLANE").map(|p| (p, "SUPERCI_PLANE")),
        None => None,
    };
    crate::view::name(named.as_ref().map(|(p, _)| p.as_str()));
    NAMED.with(|n| *n.borrow_mut() = named);
}

/// Stops a command whose named control plane (its id, or its address) is not the one it would act on.
fn named_is(id: &str, url: &str, place: &str) -> Result<()> {
    let Some((named, by)) = NAMED.with(|n| n.borrow().clone()) else { return Ok(()) };
    if named == id || named.trim_end_matches('/') == url.trim_end_matches('/') { return Ok(()) }
    Err(Fail::Failed(format!("{by} names the control plane {named}, which is not among the control planes here (this would have acted on {id}, {place}). Nothing was done. `superci planes list` lists them.")))
}

fn named_is_in_use(d: &Dashboard) -> Result<()> {
    match d.plane_in_use() { Some(p) => named_is(p.plane_id(), p.url(), &p.place()), None => Ok(()) }
}

/// A dashboard ready to change a setting of the control plane in use (its limits, its order, its keys): as `ready`,
/// without looking through the clouds again when that control plane is known.
fn ready_to_set() -> Result<Dashboard> { let mut d = signed_in(); to_set(&mut d)?; Ok(d) }

/// The same, of a dashboard that has read already. A key that only reads is no sign-in, and is said to be none.
fn to_set(d: &mut Dashboard) -> Result<()> {
    if d.signed_in_as().is_none() && given_key().is_some() { return Err(ONLY_READS.into()) }
    d.ready_for_settings()?;
    named_is_in_use(d)
}

/// A key that only reads, given by name with its control plane's address (SUPERCI_PLANE, SUPERCI_KEY): what a coding
/// agent or a script is handed in place of the sign-ins.
fn given_key() -> Option<(String, String)> {
    let var = |n: &str| std::env::var(n).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Some((var("SUPERCI_PLANE")?.trim_end_matches('/').to_string(), var("SUPERCI_KEY")?))
}

/// What a command that looks reads: the control plane, and the key it is read with. With a key given by name: that
/// control plane, with no sign-in. Else the one in use, with this machine's key.
/// A job that failed: its runner ran it and it failed, or it never ran.
fn failed_job(j: &Value) -> bool { j["conclusion"] == "failure" || ["failed", "swept"].contains(&j["state"].as_str().unwrap_or_default()) }

/// What a job's whole log says of its tests (`test_results`), and where its artifacts are: a run from a computer's
/// by name (`superci artifacts retrieve`), a code host's at its run's page.
fn job_tests(v: &PlaneView, key: &str, j: &Value) -> Value {
    let mut tests = match crate::dashboard::whole_log(v.plane.url(), key, j) {
        Ok(log) => serde_json::to_value(crate::test_results::read(&String::from_utf8_lossy(&log))).unwrap_or_default(),
        Err(e) => return json!({ "error": e }),
    };
    // (A failed job none of whose tests is read as failed: what failed is elsewhere in its log.)
    if failed_job(j) && tests["failed"].as_u64().unwrap_or(0) == 0 {
        // (The steps that failed, by name, where the control plane says: a type-check, say.)
        let gl = if j["provider"] == "gitlab" { format!("&gl={}", j["gitlab"].as_str().unwrap_or_default()) } else { String::new() };
        let failed: Vec<String> = crate::cloudflare::plane_get(v.plane.url(), key, &format!("/job/log?id={}&steps=1&bytes=1{gl}", j["job_id"])).ok()
            .and_then(|g| g["steps"].as_array().cloned()).unwrap_or_default().iter()
            .filter(|s| s["result"] == "failed" || s["conclusion"] == "failure").filter_map(|s| s["name"].as_str().map(str::to_string)).collect();
        let at = if failed.is_empty() { "a step besides the tests, or a test runner whose lines are not read".to_string() } else { format!("failed: {}", failed.join(", ")) };
        tests["note"] = format!("The job failed, but no test in its log is read as failed ({at}). Its whole log: `superci jobs retrieve {} --out=job.log`", j["job_id"]).into();
        tests["failed_steps"] = failed.into();
    }
    if j["provider"] == "local" {
        let run = j["run_id"].as_u64().unwrap_or_default();
        if let Ok(got) = crate::cloudflare::plane_get(v.plane.url(), key, &format!("/runs/{run}/artifacts")) {
            let names: Vec<Value> = got["artifacts"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str().map(|n| json!({ "name": n, "fetch": format!("superci artifacts retrieve {n} --run={run}") }))).collect();
            if !names.is_empty() { tests["artifacts"] = names.into() }
        }
    } else if j["provider"].as_str().unwrap_or_default().is_empty() && j["run_id"].as_u64().is_some_and(|r| r > 0) {
        tests["artifacts_at"] = format!("https://github.com/{}/actions/runs/{}#artifacts", j["repo"].as_str().unwrap_or_default(), j["run_id"]).into();
    }
    tests
}

/// A job's tests as a person reads them: the counts, then each failure with the first lines of its error.
fn tests_lines(t: &Value, indent: &str) -> Vec<String> {
    if let Some(e) = t["error"].as_str() { return vec![format!("{indent}Its log could not be read: {e}")] }
    let runners: Vec<&str> = t["runners"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
    // (Its artifacts are said either way.)
    let mut out = vec![if runners.is_empty() { format!("{indent}No test results in its log (none of Vitest, Jest, Playwright, pytest, cargo or Go said any).") }
        else { format!("{indent}{}", totals_line(t)) }];
    for f in t["failures"].as_array().into_iter().flatten() {
        out.push(format!("{indent}✗ {}{}", f["name"].as_str().unwrap_or_default(), f["file"].as_str().map(|p| format!("  ({p})")).unwrap_or_default()));
        for l in f["error"].as_str().unwrap_or_default().lines() { out.push(format!("{indent}    {l}")) }
    }
    out.splice(1..1, process_lines(t, indent));
    out.extend(ran_lines(t, indent));
    out.extend(expected_lines(t, indent));
    for n in t["notes"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) { out.push(format!("{indent}{n}")) }
    if let Some(note) = t["note"].as_str() { out.push(format!("{indent}{note}")) }
    for a in t["artifacts"].as_array().into_iter().flatten() { out.push(format!("{indent}Artifact {}: `{}`", a["name"].as_str().unwrap_or_default(), a["fetch"].as_str().unwrap_or_default())) }
    if let Some(at) = t["artifacts_at"].as_str() { out.push(format!("{indent}Its artifacts: {at}")) }
    out
}

/// Where a job's log came from and how old its last line is, after it: a running job's log comes in blocks, minutes
/// behind at times ("…its last line 21:18:46 UTC, 12 minutes ago; the job is still running").
pub fn log_said(got: &Value, now: u64) -> Vec<String> {
    let Some(source) = got["source"].as_str() else { return vec![] };
    let last = got["last_line_at"].as_str().and_then(|t| superci_core::aws::parse_iso_ms(&format!("{t}Z")).map(|ms| (t, ms)));
    let age = last.map(|(t, ms)| { let ago = now.saturating_sub(ms) / 1000; format!("; its last line {} UTC, {}", t.get(11..).unwrap_or(t), if ago < 90 { format!("{ago} s ago") } else { format!("{} minutes ago", ago / 60) }) }).unwrap_or_default();
    let running = if got["running"] == true { "; the job is still running (a running job's log comes in blocks, minutes behind at times: `superci jobs retrieve` again for more)" } else { "" };
    vec![format!("(The log is {source}{age}{running}.)")]
}

/// What a job's tests came to, in a line: "Tests: 0 failed, 65 passed, 2 skipped (playwright), of the passed 3 failed
/// as expected, 1 flaky" (those marked to fail, `test.fail()`, are counted among the passed, as their runner counts them).
pub fn totals_line(t: &Value) -> String {
    let n = |k: &str| t[k].as_u64().unwrap_or(0);
    let runners: Vec<&str> = t["runners"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
    let mut extra: Vec<String> = vec![];
    if n("failed_as_expected") > 0 { extra.push(format!("{} failed as expected", n("failed_as_expected"))) }
    if n("flaky") > 0 { extra.push(format!("{} flaky", n("flaky"))) }
    format!("Tests: {} failed, {} passed{} ({}){}", n("failed"), n("passed"), if n("skipped") > 0 { format!(", {} skipped", n("skipped")) } else { String::new() }, runners.join(", "),
        if extra.is_empty() { String::new() } else { format!(", of the passed {}", extra.join(", ")) })
}

/// Each process's own counts, where several ran tests at once (`[slot 2] …`).
pub fn process_lines(t: &Value, indent: &str) -> Vec<String> {
    t["by_process"].as_array().into_iter().flatten().map(|p| {
        let n = |k: &str| p[k].as_u64().unwrap_or(0);
        let mut words = vec![format!("{} failed", n("failed")), format!("{} passed", n("passed"))];
        if n("skipped") > 0 { words.push(format!("{} skipped", n("skipped"))) }
        if n("failed_as_expected") > 0 { words.push(format!("{} failed as expected", n("failed_as_expected"))) }
        if n("flaky") > 0 { words.push(format!("{} flaky", n("flaky"))) }
        format!("{indent}  {} {}", p["mark"].as_str().unwrap_or_default(), words.join(", "))
    }).collect()
}

/// The tests that failed as they were marked to (`test.fail()`), each named where the list of what ran does not show
/// them, and what that means: their runner prints no error for them, and passes one that failed for another reason.
pub fn expected_lines(t: &Value, indent: &str) -> Vec<String> {
    let tests = t["failed_as_expected_tests"].as_array().cloned().unwrap_or_default();
    if tests.is_empty() { return vec![] }
    let mut out = vec![];
    let ran = t["ran"].as_array().map(|r| r.len()).unwrap_or(0);
    if ran > 30 {
        out.push(format!("{indent}Failed as expected:"));
        out.extend(tests.iter().map(|r| format!("{indent}  ✓ {}{}", r["name"].as_str().unwrap_or_default(), r["file"].as_str().map(|f| format!("  ({f})")).unwrap_or_default())));
    }
    out.push(format!("{indent}(Marked to fail, as test.fail(): Playwright prints no error for these, and passes one that failed for another reason too.)"));
    out
}

/// The tests that ran, each with how it came out, when there are few enough to read (30 at most; else the counts say it).
pub fn ran_lines(t: &Value, indent: &str) -> Vec<String> {
    let ran = t["ran"].as_array().cloned().unwrap_or_default();
    if ran.is_empty() || ran.len() > 30 { return vec![] }
    let mut out = vec![format!("{indent}What ran:")];
    out.extend(ran.iter().map(|r| {
        let mark = match r["outcome"].as_str().unwrap_or_default() { "passed" => "✓", "failed" => "✗", "skipped" => "–", "flaky" => "✓ (flaky)", "failed as expected" => "✓ (failed as expected)", _ => "·" };
        format!("{indent}  {mark} {}{}", r["name"].as_str().unwrap_or_default(), r["file"].as_str().map(|f| format!("  ({f})")).unwrap_or_default())
    }));
    out
}

fn look(d: &mut Dashboard) -> Result<(PlaneView, String)> {
    let Some((url, key)) = given_key() else {
        let v = d.current()?.ok_or(NO_PLANE)?;
        named_is(v.plane.plane_id(), v.plane.url(), &v.plane.place())?;
        return Ok((v, d.key().to_string()))
    };
    // (Or one served on this computer, as tests serve one.)
    if !url.starts_with("https://") && !url.starts_with("http://127.0.0.1:") { return usage("SUPERCI_PLANE is the control plane's address, starting with https:// (`superci status` says it).") }
    let health = crate::cloudflare::health(&url).ok_or("The control plane at SUPERCI_PLANE did not answer.")?;
    let v = crate::view::plane_view(&Plane::Seen { url, plane_id: health["plane"].as_str().unwrap_or_default().to_string() }, Some(&key));
    if v.status.is_none() { return Err("The control plane did not take SUPERCI_KEY: it has ended, was revoked, or is another control plane's (read keys need control plane 0.11.0 or newer).".into()) }
    named_is(v.plane.plane_id(), v.plane.url(), "the one SUPERCI_PLANE and SUPERCI_KEY read")?;
    Ok((v, key))
}

fn view(d: &mut Dashboard) -> Result<PlaneView> { Ok(look(d)?.0) }

/// What deletes something needs --confirm (said with what it would do); --dry-run needs none.
fn confirmed(args: &Args, what: &str) -> Result<()> { if args.has("confirm") || args.has("dry-run") { Ok(()) } else { Err(Fail::Unconfirmed(format!("This would {what}. Add --confirm to do it, or --dry-run to check it first."))) } }

/// A flag that is true or false (`--enabled=true`). None: not given.
fn truth(args: &Args, flag: &str) -> Result<Option<bool>> {
    match args.get(flag) { None => Ok(None), Some("true") => Ok(Some(true)), Some("false") => Ok(Some(false)), Some(other) => usage(format!("--{flag} is true or false, not {other}.")) }
}

fn field(name: &'static str, value: impl Into<String>) -> (&'static str, String) { (name, value.into()) }

/// With --dry-run: what the command would do, after everything that can be checked without changing anything was
/// (the flags, the sign-in, the control plane). The form it would send is in the data, without any token.
fn dry(args: &Args, would: &str, path: &str, fields: &[(&str, String)]) -> Option<Done> {
    if !args.has("dry-run") { return None }
    let form: serde_json::Map<String, Value> = fields.iter().map(|(k, v)| (k.to_string(), if *k == "token" { "(given)".into() } else { v.clone().into() })).collect();
    Some(Done { said: vec![format!("Would {would}. Nothing was changed.")], data: json!({ "ok": true, "dry_run": true, "would": would, "form": { "to": path, "fields": form } }) })
}

/// Does what the page's form does, or with --dry-run returns from the command saying what it would do.
macro_rules! apply {
    ($d:expr, $args:expr, $would:expr, $path:expr, $fields:expr) => {{
        let (would, path, fields): (String, &str, &[(&str, String)]) = ($would.into(), $path, $fields);
        if let Some(done) = dry($args, &would, path, fields) { return Ok(done) }
        $d.act(path, fields)?
    }};
}

/// Runs a command that is not the dashboard itself. `steps` is told each step of a long one as it begins.
pub fn run(args: &Args, steps: &mut dyn FnMut(&str)) -> Result<Done> {
    let w = |i: usize| args.word(i);
    if w(0) == "workspaces" { return workspaces(args) }
    enter(args)?;
    name_plane(args);
    match (w(0), w(1)) {
        ("status", "") => {
            let mut d = signed_in();
            let (signed_in, view) = match given_key() {
                Some(_) => ("with a read key (SUPERCI_KEY)".to_string(), Some(look(&mut d)?.0)),
                None => { let v = d.current()?; if let Some(v) = &v { named_is(v.plane.plane_id(), v.plane.url(), &v.plane.place())? } (d.signed_in_as().unwrap_or_else(|| "nowhere now (AWS's sign-in has ended; this is read with this machine's key)".into()), v) }
            };
            let mut v = match view { Some(v) => json!({ "signed_in": signed_in, "control_plane": plane_json(&v), "status": v.status }), None => json!({ "signed_in": signed_in, "control_plane": null }) };
            // Which workspace this is, said to a person only when there are several.
            if let Some(name) = IN.with(|w| w.borrow().clone()) {
                v["workspace"] = name.into();
                v["workspaces"] = store::root().map(|r| store::workspaces_in(&r).len()).unwrap_or(0).into();
            }
            Ok(Done { said: status_lines(&v), data: v })
        }
        ("jobs", "list") => jobs(args.get("run")),
        ("jobs", "retrieve") => {
            let id = w(2);
            // Or by its run and its name (`--run=ID --job=NAME`; the name may be left out when the run has one job), for
            // a script that started the run.
            let by_run = args.get("run").filter(|_| id.is_empty());
            if by_run.is_none() && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit())) { return usage("Say which: `superci jobs retrieve ID`, or `--run=ID --job=NAME` (ids are in `superci jobs list`).") }
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            let j = match by_run {
                Some(run) => {
                    let of_run: Vec<Value> = v.jobs().into_iter().filter(|j| j["run_id"].as_u64().is_some_and(|n| n.to_string() == run)).collect();
                    let names = || of_run.iter().filter_map(|j| j["name"].as_str()).collect::<Vec<_>>().join(", ");
                    match args.get("job") {
                        Some(name) => of_run.iter().find(|j| j["name"] == name).cloned().ok_or_else(|| if of_run.is_empty() { format!("No jobs of run {run} among the latest this control plane keeps (ids are in `superci runs list`).") } else { format!("Run {run} has no job {name} (its jobs: {}).", names()) })?,
                        None if of_run.len() == 1 => of_run[0].clone(),
                        None if of_run.is_empty() => return Err(format!("No jobs of run {run} among the latest this control plane keeps (ids are in `superci runs list`).").into()),
                        None => return usage(format!("Run {run} has several jobs: say which with --job=NAME ({}).", names())),
                    }
                }
                None => v.jobs().into_iter().find(|j| j["job_id"].as_u64().is_some_and(|n| n.to_string() == id)).ok_or("That job is not among the latest this control plane keeps (ids are in `superci jobs list`).")?,
            };
            let words = job_words(&j);
            // Its whole log into a file, whatever its size.
            if let Some(out) = args.get("out") {
                let log = crate::dashboard::whole_log(v.plane.url(), &key, &j)?;
                std::fs::write(out, &log).map_err(|e| format!("{out}: {e}"))?;
                let so_far = ["launching", "launched", "running", "waiting"].contains(&j["state"].as_str().unwrap_or_default());
                return Ok(Done { said: vec![format!("{} ({}, {} lines) is in {out}.", if so_far { "Its log so far (the job is still running)" } else { "Its whole log" }, crate::dashboard::size_words(log.len() as u64), log.iter().filter(|b| **b == b'\n').count())], data: json!({ "job": words, "out": out, "bytes": log.len() }) })
            }
            // Its tests, from its whole log (see `test_results`), instead of the end of its log.
            if args.has("tests") {
                let tests = job_tests(&v, &key, &j);
                let mut said = vec![job_line(&words), words["link"].as_str().unwrap_or_default().to_string()];
                said.extend(tests_lines(&tests, "  "));
                return Ok(Done { said, data: json!({ "job": words, "tests": tests }) })
            }
            // (By its run: the job found's own id.)
            let job_id = j["job_id"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| id.to_string());
            let mut path = format!("/job/log?id={job_id}&bytes={}", args.get("bytes").unwrap_or("200000"));
            if j["provider"] == "gitlab" { path += &format!("&gl={}", j["gitlab"].as_str().unwrap_or_default()) }
            let got = crate::cloudflare::plane_get(v.plane.url(), &key, &path)
                .map_err(|e| if e.contains("404") { "Reading a job's log needs control plane 0.11.0 or newer: `superci planes update`.".to_string() } else { e })?;
            let mut said = vec![job_line(&words), words["link"].as_str().unwrap_or_default().to_string()];
            // How its start went: what AWS was asked, and what its machine said of its own part.
            if let Some(choice) = words["choice"].as_str() { said.push(format!("{choice}.")) }
            if let Some(images) = words["images"].as_str() { said.push(format!("Docker images: {images}.")) }
            said.extend(job_start_lines(&j));
            match got["log"].as_str() {
                Some(log) => {
                    said.push(if got["truncated"] == true { format!("The end of its log ({} bytes in all):", got["bytes"]) } else { "Its log:".to_string() });
                    said.push(log.trim_end().to_string());
                    said.extend(log_said(&got, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)));
                }
                None => said.push(format!("No log: {}.", got["error"].as_str().unwrap_or("the control plane gave none").trim_end_matches('.'))),
            }
            Ok(Done { said, data: json!({ "job": words, "log": got["log"], "truncated": got["truncated"], "log_error": got["error"] }) })
        }
        ("runs", "create") => {
            let Some(workflow) = args.get("workflow") else { return usage("Say which workflow: `superci runs create --workflow=.github/workflows/ci.yml` (and --job=NAME when it has several jobs).") };
            let pairs = |flag: &str| args.all(flag).iter().map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or(format!("--{flag} is NAME=VALUE"))).collect::<std::result::Result<Vec<_>, String>>();
            let (secrets, vars) = match (pairs("secret"), pairs("var")) { (Ok(s), Ok(v)) => (s, v), (Err(e), _) | (_, Err(e)) => return usage(e) };
            // With a run key (SUPERCI_PLANE, SUPERCI_KEY), no sign-in is needed; else this computer's key
            // with the control plane it is kept for (each control plane is handed a key of its own, made from it). A
            // cloud sign-in is needed only to find a control plane, or to hand it the key again: a run needs neither,
            // so it goes on when AWS has ended its sign-in or another account is signed in.
            let (url, key) = match given_key() {
                Some((url, key)) => (url, key),
                None => { let mut d = signed_in(); let (v, key) = look(&mut d)?; (v.plane.url().to_string(), crate::cloudflare::plane_key(&key, v.plane.url())) }
            };
            // A value with what a shell reads as commands (`|`, `;`, `&`, a backquote, `$(`): said, as a workflow that puts
            // it in a `run:` line (or inside its own quotes) runs them. (A secret by its name only.)
            let shelly = |v: &str| ["|", ";", "&", "`", "$(", "\n"].iter().any(|c| v.contains(c));
            for (k, v) in vars.iter().filter(|(_, v)| shelly(v)) { steps(&format!("{}Note: --var {k}'s value has characters a shell runs ({}): a step that puts it in a `run:` line runs them; pass it through `env:` and quote it there.", JOB_LINE, ["|", ";", "&", "`", "$("].iter().filter(|c| v.contains(*c)).copied().collect::<Vec<_>>().join(" "))) }
            for (k, _) in secrets.iter().filter(|(_, v)| shelly(v)) { steps(&format!("{}Note: --secret {k}'s value has characters a shell runs: a step that puts it in a `run:` line runs them; pass it through `env:` and quote it there.", JOB_LINE)) }
            let how = crate::local_run::Follow { quiet: args.has("quiet"), detach: args.has("detach"), dry_run: args.has("dry-run") };
            let inputs = match pairs("input") { Ok(i) => i, Err(e) => return usage(e) };
            // (The GitHub repository it is of, for a checkout whose origin does not say: its cache, its secrets' scope.)
            let repo = args.get("repo").map(str::to_string);
            if let Some(r) = &repo { if !r.split_once('/').is_some_and(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/') && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))) { return usage("--repo is a GitHub repository, OWNER/NAME.") } }
            let event = crate::local_run::Event { name: args.get("event").unwrap_or("push").to_string(), labels: args.all("label").iter().map(|l| l.to_string()).collect(), inputs, payload: args.get("event-payload").map(str::to_string), repo };
            ended(crate::local_run::create(&url, &key, workflow, args.get("job"), &event, &secrets, &vars, how, steps)?)
        }
        ("runs", "cancel") => {
            let id = w(2);
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return usage("Say which: `superci runs cancel ID` (ids are in `superci runs list`).") }
            let (url, key) = match given_key() {
                Some((url, key)) => (url, key),
                None => { let mut d = signed_in(); let (v, key) = look(&mut d)?; (v.plane.url().to_string(), crate::cloudflare::plane_key(&key, v.plane.url())) }
            };
            if args.has("dry-run") { return Ok(Done { said: vec![format!("Would cancel run {id}: its jobs not started end (unless their `if` holds after a cancel), and a running one is told to stop. Nothing was changed.")], data: json!({ "dry_run": true, "would": format!("cancel run {id}") }) }) }
            let got = crate::cloudflare::plane_post(&url, &key, &format!("/runs/{id}/cancel"), &json!({})).map_err(|e| if e.contains("404") { format!("No run {id} here (a newer control plane cancels runs: `superci planes update`; ids are in `superci runs list`).") } else { e })?;
            if let Some(e) = got["error"].as_str() { return Err(e.to_string().into()) }
            let said = if got["github"] == true { format!("GitHub cancels run {id}: its running jobs stop, and their machines end with them.") }
                else if got["over"] == true { format!("Run {id} was over already.") }
                else { format!("Cancelled run {id}: its jobs not started end (as on GitHub, one whose `if` holds after a cancel, as `always()`, still runs); a running one is told to stop (its steps have five minutes), then its machine ends. `superci runs wait {id}` follows it.") };
            Ok(Done { said: vec![said], data: got })
        }
        ("runs", "wait") => {
            let (url, key) = match given_key() {
                Some((url, key)) => (url, key),
                None => { let mut d = signed_in(); let (v, key) = look(&mut d)?; (v.plane.url().to_string(), crate::cloudflare::plane_key(&key, v.plane.url())) }
            };
            ended(crate::local_run::wait(&url, &key, &w(2), crate::local_run::Follow { quiet: args.has("quiet"), ..Default::default() }, steps)?)
        }
        ("usage", "retrieve") => {
            let period = args.get("period").unwrap_or("day");
            let Some(&(_, _, _)) = superci_core::plane::USAGE_PERIODS.iter().find(|(p, _, _)| *p == period) else { return usage("--period is day, week or month.") };
            let mut d = signed_in();
            let (v, _) = look(&mut d)?;
            let u = v.status.as_ref().map(|s| s["usage"][period].clone()).filter(|u| u.is_object()).ok_or("The control plane does not count usage yet: update it (`superci planes update`).")?;
            let (n, f) = (|k: &str| u[k].as_u64().unwrap_or(0), |k: &str| u[k].as_f64().unwrap_or(0.0));
            let span = match period { "week" => "7 days", "month" => "30 days", _ => "24 hours" };
            let (spent, theirs) = (f("usd"), f("host_usd"));
            let mut said = vec![format!("The last {span}: {} jobs, {} ran{}.", n("jobs"), n("ran"), if n("not_ran") > 0 { format!(", {} did not", n("not_ran")) } else { String::new() }),
                format!("{:.1} machine minutes; {} minutes as GitHub or GitLab bills them (each job rounded up).", f("machine_minutes"), n("billed_minutes")),
                format!("Spent ${spent:.2}; the same jobs on the code host's own runners: ${theirs:.2}{}.", if theirs > spent { format!(" (${:.2} saved, {:.0}% less)", theirs - spent, (1.0 - spent / theirs) * 100.0) } else { String::new() })];
            if let Some(m) = u["wait_median_ms"].as_u64() { said.push(format!("Median wait for a machine: {} s.", (m + 500) / 1000)) }
            Ok(Done { said, data: json!({ "period": period, "usage": u }) })
        }
        ("artifacts", operation @ ("list" | "retrieve")) => {
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            crate::local_run::new_enough(v.version.as_deref())?;
            let key = crate::cloudflare::plane_key(&key, v.plane.url());
            let run = args.get("run").unwrap_or_default();
            if run.is_empty() || !run.bytes().all(|b| b.is_ascii_digit()) { return usage("Say which run: --run=ID (ids are in `superci runs list`).") }
            if operation == "retrieve" && w(2).is_empty() { return usage("Say which artifact: `superci artifacts retrieve NAME --run=ID` (names are in `superci artifacts list --run=ID`).") }
            if operation == "list" { Ok(crate::local_run::artifacts(v.plane.url(), &key, run)?) } else { Ok(crate::local_run::artifact(v.plane.url(), &key, run, &w(2), args.get("out"))?) }
        }
        ("runs", "list") => {
            let mut d = signed_in();
            let runs: Vec<Value> = runs(&view(&mut d)?).iter().map(run_words).collect();
            let said = runs.iter().take(40).map(run_line).collect::<Vec<_>>();
            Ok(Done { said: if said.is_empty() { vec!["No runs yet.".into()] } else { said }, data: json!({ "runs": runs }) })
        }
        ("runs", "retrieve") => {
            let id = w(2);
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return usage("Say which: `superci runs retrieve ID` (ids are in `superci runs list`).") }
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            let of: Vec<Value> = runs(&v).iter().filter(|r| r["id"].as_u64().is_some_and(|n| n.to_string() == id)).map(run_words).collect();
            if of.is_empty() { return Err("That run is not among the latest this control plane keeps (ids are in `superci runs list`).".into()) }
            let jobs = v.jobs();
            let (mut said, mut data) = (vec![], vec![]);
            // (Each run found has its jobs: a run is made of them.)
            of.iter().rev().for_each(|r| {
                let raw: Vec<&Value> = jobs.iter().filter(|j| j["run_id"] == r["id"] && j["attempt"].as_u64().filter(|a| *a > 0) == r["attempt"].as_u64()).collect();
                let mut its: Vec<Value> = raw.iter().map(|j| job_words(j)).collect();
                said.push(run_line(r));
                said.push(r["link"].as_str().unwrap_or_default().to_string());
                raw.iter().zip(its.iter_mut()).for_each(|(j, words)| {
                    said.push(format!("  {}", job_line(words)));
                    // Its tests, from its whole log: a passed job's too (it may have run fewer than asked, or none),
                    // whatever ran it.
                    if args.has("tests") && (failed_job(j) || j["machine_id"].is_string() || j["conclusion"].is_string()) {
                        let tests = job_tests(&v, &key, j);
                        said.extend(tests_lines(&tests, "    "));
                        words["tests"] = tests;
                    }
                });
                let mut r = r.clone();
                r["job_list"] = its.into();
                data.push(r);
            });
            Ok(Done { said, data: json!({ "runs": data }) })
        }
        ("caches", operation @ ("retrieve" | "list" | "update" | "delete")) => {
            let pick = |d: &Dashboard| -> Result<(String, String)> { Ok(match given_key() { Some(k) => k, None => { let p = in_use(d)?; (p.url().to_string(), d.key().to_string()) } }) };
            if operation == "update" {
                if !args.has("enabled") && !args.has("size-gb") && !args.has("days") && !args.has("images") { return usage("Say what to change: `superci caches update --size-gb=50` (see `superci help caches`).") }
                let mut d = signed_in();
                to_set(&mut d)?;
                in_use(&d)?;
                if let Some(e) = args.get("enabled") {
                    let on = match e { "true" => true, "false" => false, _ => return usage("--enabled is true or false.") };
                    apply!(d, args, if on { "keep jobs' caches in your AWS account".to_string() } else { "leave jobs to GitHub's own cache".to_string() }, "/routing", &[field("action", "cache_on"), field("on", if on { "on" } else { "off" })]);
                }
                if let Some(gb) = args.get("size-gb") { apply!(d, args, format!("let each repository keep {gb} GB in the cache"), "/routing", &[field("action", "cache_size"), field("size_gb", gb)]); }
                if let Some(days) = args.get("days") { apply!(d, args, format!("let a cache entry go unused for {days} days"), "/routing", &[field("action", "cache_days"), field("days", days)]); }
                if let Some(l) = args.get("images") {
                    let on = match l { "true" => true, "false" => false, _ => return usage("--images is true or false.") };
                    apply!(d, args, if on { "keep the Docker images jobs pull, for their next jobs (Ubuntu 26.04 machines)".to_string() } else { "let Docker pull every image as it always has".to_string() }, "/routing", &[field("action", "cache_images"), field("on", if on { "on" } else { "off" })]);
                }
            }
            if operation == "delete" {
                let Some(repo) = args.get("repo") else { return usage("Say whose: `superci caches delete --repo=OWNER/NAME [KEY] --confirm`.") };
                let key = w(2);
                let would = if key.is_empty() { format!("take every cache entry of {repo} out (jobs then save them anew)") } else { format!("take the cache entry {key} of {repo} out") };
                confirmed(args, &would)?;
                let mut fields = vec![field("repo", repo)];
                if !key.is_empty() { fields.push(field("key", key)) }
                if let Some(scope) = args.get("scope") { fields.push(field("scope", scope)) }
                if let Some(done) = dry(args, &would, "/caches/delete", &fields) { return Ok(done) }
                // Asked of the control plane with this computer's key (a key that only reads may not): no cloud
                // sign-in is needed for it.
                let mut d = signed_in();
                if d.signed_in_as().is_none() && given_key().is_some() { return Err(ONLY_READS.into()) }
                let (v, kept) = look(&mut d)?;
                let body: serde_json::Map<String, Value> = fields.iter().map(|(k, v)| (k.to_string(), Value::String(v.clone()))).collect();
                crate::cloudflare::plane_post(v.plane.url(), &crate::cloudflare::plane_key(&kept, v.plane.url()), "/caches/delete", &Value::Object(body))?;
                return Ok(Done::said(if key.is_empty() { format!("{repo}'s cache entries are out.") } else { format!("{key} is out of {repo}'s cache.") }))
            }
            let d = signed_in();
            let (url, key) = pick(&d)?;
            let repo = args.get("repo").filter(|_| operation == "list");
            let got = crate::cloudflare::plane_get(&url, &key, &match repo { Some(r) => format!("/caches?repo={}", url::form_urlencoded::byte_serialize(r.as_bytes()).collect::<String>()), None => "/caches".into() })
                .map_err(|e| if e.contains(" 404") { "This control plane keeps no cache yet: update it (`superci planes update`).".to_string() } else { e })?;
            let now = got["now_ms"].as_u64().unwrap_or(0);
            let ago = |ms: u64| { let m = now.saturating_sub(ms) / 60_000; match m { 0 => "just now".to_string(), 1..=59 => format!("{m} min ago"), 60..=2879 => format!("{} h ago", m / 60), _ => format!("{} days ago", m / 1440) } };
            let size = |b: u64| if b >= 1_000_000_000 { format!("{:.1} GB", b as f64 / 1e9) } else { format!("{} MB", b.div_ceil(1_000_000)) };
            let mut said = vec![];
            if repo.is_none() {
                said.push(if got["on"] == true { format!("The cache is on: each repository keeps {} GB at most, and an entry goes after {} days unused ({} at most since it was saved).", got["size_gb"], got["days"], got["max_age_days"]) } else { "The cache is off: jobs use GitHub's own cache (`superci caches update --enabled=true`).".into() });
                if got["on"] == true {
                    let i = &got["images"];
                    said.push(match (i["on"].as_bool(), i["layers"].as_u64().unwrap_or(0)) {
                        (Some(true), 0) => "Docker images: kept for each job's next run (Ubuntu 26.04 machines); none yet.".to_string(),
                        (Some(true), n) => format!("Docker images: kept for each job's next run (Ubuntu 26.04 machines): {n} layers now, {}.", size(i["bytes"].as_u64().unwrap_or(0))),
                        (Some(false), _) => "Docker images: not kept (`superci caches update --images=true`).".to_string(),
                        (None, _) => String::new(),
                    });
                    said.retain(|l| !l.is_empty());
                }
                let repos = got["repos"].as_array().cloned().unwrap_or_default();
                if repos.is_empty() { said.push("Nothing kept yet.".into()) }
                for r in &repos { said.push(format!("{:<40} {:>5} entries {:>9}   used {}", r["name"].as_str().or(r["id"].as_str()).unwrap_or_default(), r["entries"], size(r["bytes"].as_u64().unwrap_or(0)), ago(r["used_ms"].as_u64().unwrap_or(0)))) }
            } else {
                let entries = got["entries"].as_array().cloned().unwrap_or_default();
                if got["repo"].is_null() { said.push(format!("{} keeps nothing in the cache.", repo.unwrap_or_default())) }
                for e in &entries { said.push(format!("{:<56} {:<28} {:>9}   used {}", e["key"].as_str().unwrap_or_default(), e["scope"].as_str().unwrap_or_default(), size(e["bytes"].as_u64().unwrap_or(0)), ago(e["used_ms"].as_u64().unwrap_or(0)))) }
            }
            Ok(Done { said, data: got })
        }
        ("secrets", "list") => {
            let mut d = signed_in();
            in_use(&d)?;
            let got = d.kept_secrets_said()?;
            let kept = got["secrets"].as_array().cloned().unwrap_or_default();
            let now = got["now_ms"].as_u64().unwrap_or(0);
            let ago = |ms: u64| { let m = now.saturating_sub(ms) / 60_000; match m { 0 => "just now".to_string(), 1..=59 => format!("{m} min ago"), 60..=2879 => format!("{} h ago", m / 60), _ => format!("{} days ago", m / 1440) } };
            let mut said = vec![];
            if kept.is_empty() { said.push("No secrets kept. A run from a computer gets the ones it names: `printf %s \"$NPM_TOKEN\" | superci secrets create NPM_TOKEN`.".to_string()) }
            for s in &kept {
                let used = match s["used"]["at_ms"].as_u64() {
                    Some(at) => format!("used {} by {}", ago(at), s["used"]["by"].as_str().map(|b| if s["used"]["run_key"] == true { format!("the key {}", b.to_lowercase()) } else { b.to_string() }).unwrap_or_else(|| "a computer".into())),
                    None => "not used yet".into(),
                };
                said.push(format!("{:<28} {:<44} set {}, {used}", s["name"].as_str().unwrap_or_default(), s["said"].as_str().unwrap_or_default(), ago(s["updated_ms"].as_u64().unwrap_or(0))));
            }
            let data: Vec<Value> = kept.iter().map(|s| json!({ "name": s["name"], "scope": s["scope"], "updated_ms": s["updated_ms"], "used": s["used"] })).collect();
            Ok(Done { said, data: json!({ "secrets": data }) })
        }
        ("secrets", "create") | ("secrets", "update") | ("secrets", "delete") => {
            let op = w(1);
            if w(2).is_empty() { return usage(format!("Give its name: `superci secrets {op} NPM_TOKEN`.")) }
            let name = superci_core::secrets::valid_name(w(2))?;
            let scope = superci_core::secrets::valid_scope(args.get("repo"), args.get("branch"), args.get("workflow"), args.get("environment"))?;
            if op == "delete" {
                confirmed(args, &format!("take the secret {name} ({}) out", scope.said()))?;
                let mut d = ready_to_set()?;
                in_use(&d)?;
                if let Some(done) = dry(args, &format!("take the secret {name} ({}) out", scope.said()), "secrets", &[]) { return Ok(done) }
                d.forget_secret(&name, &scope)?;
                return Ok(Done { said: vec![format!("{name} ({}) is taken out: runs from a computer no longer get it.", scope.said())], data: json!({ "ok": true, "name": name, "scope": scope }) })
            }
            // The value: never in the command itself (it would stay in the shell's history), but from a variable or stdin.
            let value = match args.get("value-env") {
                Some(var) => std::env::var(var).map_err(|_| format!("{var} is not set."))?,
                // A person at a terminal: asked for it, typed hidden (never in the command, never in the shell's history).
                None if std::io::IsTerminal::is_terminal(&std::io::stdin()) => rpassword::prompt_password(format!("{name}'s value (typed hidden; Enter when done): ")).map_err(|e| format!("The value could not be read: {e}"))?,
                None => { let mut v = String::new(); std::io::Read::read_to_string(&mut std::io::stdin(), &mut v).map_err(|e| e.to_string())?; v.strip_suffix('\n').map(|x| x.strip_suffix('\r').unwrap_or(x).to_string()).unwrap_or(v) }
            };
            superci_core::secrets::valid_value(&value)?;
            let mut d = ready_to_set()?;
            in_use(&d)?;
            let would = format!("{} the secret {name} for {}", if op == "create" { "keep" } else { "give a new value to" }, scope.said());
            if let Some(done) = dry(args, &would, "secrets", &[]) { return Ok(done) }
            d.keep_secret(&name, &scope, &value, op == "create")?;
            Ok(Done { said: vec![format!("{name} is kept for {}: a run from a computer that names it (${{{{ secrets.{name} }}}}) gets it. It is in your control plane's cloud only, and is never shown again.", scope.said())], data: json!({ "ok": true, "name": name, "scope": scope }) })
        }
        ("keys", "list") => {
            let mut d = signed_in();
            let v = d.current()?.ok_or(NO_PLANE)?;
            let status = v.status.clone().unwrap_or_default();
            let runs_of = |name: &str| status["read_keys"].as_array().into_iter().flatten().find(|k| k["name"] == name).map(|k| k["runs"].clone()).filter(|r| !r.is_null());
            let keys: Vec<Value> = read_keys(&status).into_iter().map(|(name, until)| { let mut k = json!({ "name": name.to_lowercase(), "until": day_of(until / 1000) }); if let Some(r) = runs_of(&name) { k["runs"] = r } k }).collect();
            let said = if keys.is_empty() { vec!["No read or run keys. Make one for a coding agent or a script: `superci keys create agent` (with --runs, a run key).".to_string()] }
                else { keys.iter().map(|k| match k.get("runs") {
                    Some(_) => format!("{:<24} run key: reads and starts runs from a computer, until {}", k["name"].as_str().unwrap_or_default(), k["until"].as_str().unwrap_or_default()),
                    None => format!("{:<24} read key, until {}", k["name"].as_str().unwrap_or_default(), k["until"].as_str().unwrap_or_default()),
                }).collect() };
            // Each computer's that looks after this CI (its dashboard), when the control plane can say.
            let mine = d.status_key_name();
            let devices: Vec<Value> = crate::cloudflare::plane_get(v.plane.url(), &d.status_key_value(), "/keys").ok().and_then(|k| k["devices"].as_array().cloned()).unwrap_or_default().into_iter()
                .map(|c| json!({ "name": c["name"], "device": c["device"], "this_device": c["name"].as_str() == Some(mine.as_str()), "last_used_ms": c["last_ms"], "until_ms": c["until_ms"] })).collect();
            let mut said = said;
            if !devices.is_empty() {
                said.push(String::new());
                said.push("Device keys: each device that looks after it (`superci keys delete NAME` takes one out):".into());
                said.extend(devices.iter().map(|c| format!("{:<34} {}{}", c["name"].as_str().unwrap_or_default(), c["device"].as_str().filter(|n| !n.is_empty()).unwrap_or("a device"), if c["this_device"] == true { " (this one)" } else { "" })));
            }
            Ok(Done { said, data: json!({ "keys": keys, "devices": devices }) })
        }
        ("keys", "create") => {
            if w(2).is_empty() { return usage("Give it a name: `superci keys create agent`.") }
            let Ok(days) = args.get("days").unwrap_or("30").parse::<u64>() else { return usage("--days is a number of days (30 unless given).") };
            // A key that may also start runs from a computer (--runs), within the control plane's own limits.
            if args.has("max-cpu") || args.has("daily-usd") { return usage("A key has no limits of its own: its jobs keep to the control plane's (`superci limits update --max-cpu=64 --daily-usd=50`).") }
            let runs = args.has("runs");
            let mut d = ready_to_set()?;
            let plane = in_use(&d)?;
            let what = if runs { "(a run key: it reads and starts runs from a computer)" } else { "(a read key: it only reads)" };
            if let Some(done) = dry(args, &format!("make a key named {} {what}, good for {days} days", w(2)), "keys", &[]) { return Ok(done) }
            let (key, until) = d.create_key(w(2), days, runs)?;
            if runs {
                return Ok(Done { said: vec![format!("A run key, until {}: it reads, and starts runs from a computer, within this control plane's limits (`superci limits retrieve`). It is shown this once:", day_of(until)), String::new(),
                    format!("  SUPERCI_PLANE={} SUPERCI_KEY={key}", plane.url()), String::new(), "With these two set, `superci runs create --workflow=…` works with no sign-in, and nothing else can be changed.".to_string()],
                    data: json!({ "ok": true, "name": w(2).to_lowercase(), "until": day_of(until), "runs": true, "env": { "SUPERCI_PLANE": plane.url(), "SUPERCI_KEY": key } }) })
            }
            Ok(Done { said: vec![format!("A read key, until {}. It is shown this once:", day_of(until)), String::new(), format!("  SUPERCI_PLANE={} SUPERCI_KEY={key}", plane.url()), String::new(),
                "With these two set, whatever looks works with no sign-in (`superci status`, `superci jobs list`, `superci runners list`…), and nothing can be changed.".to_string()],
                data: json!({ "ok": true, "name": w(2).to_lowercase(), "until": day_of(until), "env": { "SUPERCI_PLANE": plane.url(), "SUPERCI_KEY": key } }) })
        }
        ("keys", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci keys delete NAME` (names are in `superci keys list`).") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            if let Some(done) = dry(args, &format!("end the key named {}", w(2)), "keys", &[]) { return Ok(done) }
            // A computer's (its dashboard's), or one that only reads.
            if w(2).starts_with("DEVICE_KEY_") {
                d.drop_device_key(w(2))?;
                return Ok(Done::said(format!("The device key {} is taken out: that device's dashboard no longer reaches this CI until it opens again with its sign-in.", w(2))))
            }
            d.revoke_key(w(2))?;
            Ok(Done::said(format!("The key {} no longer reads.", w(2).to_lowercase())))
        }
        // In a debug build only, to see (and record, with SUPERCI_RECORD) what a real account answers:
        // `superci debug ec2 REGION ACTION Key=Value…`, `superci debug iam ACTION Key=Value…`, or
        // `superci debug json REGION SERVICE TARGET '{…}'`. A SERVICE
        // with a dot in it is the whole host (budgets.amazonaws.com), a TARGET that starts with a slash is a path
        // (/listRegions), and one that ends in @1.0 is asked in that JSON version.
        // (What a stopped deploy made, removed as "Choose another place" removes it: `superci debug abandon REGION PLANE`.)
        #[cfg(debug_assertions)]
        ("debug", "abandon") => {
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let left = crate::aws_plane::abandon(&creds, w(2), w(3));
            Ok(Done::said(if left.is_empty() { "nothing left".to_string() } else { format!("left: {}", left.join("; ")) }))
        }
        // (The machines' network of the control plane in use in one region, deleted as removing AWS deletes it, and
        // nothing else: `superci debug networks REGION`.)
        #[cfg(debug_assertions)]
        ("debug", "networks") => {
            let mut d = signed_in();
            let plane_id = view(&mut d)?.plane.plane_id().to_string();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            if !superci_core::page::REGIONS.contains(&w(2)) { return usage("Say which region: `superci debug networks REGION`.") }
            crate::aws::delete_networks(&creds, &plane_id, &[w(2)])?;
            Ok(Done::said(format!("deleted in {}", w(2))))
        }
        // (And the AWS account's own budget, each step by itself: `superci debug budget found|set USD EMAIL [stop]|lift|remove`.)
        #[cfg(debug_assertions)]
        ("debug", "budget") => {
            let mut d = signed_in();
            let plane_id = view(&mut d)?.plane.plane_id().to_string();
            let (account, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let http = crate::aws::Blocking::new();
            let b = crate::aws_budget::Budgets { http: &http, creds: &creds, account: &account, plane_id: &plane_id, pause: &crate::aws::pause };
            let said = match w(2) {
                "set" => b.set(w(3).parse().map_err(|_| "set USD EMAIL [stop]")?, Some(w(4)).filter(|e| !e.is_empty()), w(5) == "stop").map(|_| "set".to_string()),
                "lift" => b.lift(w(3) == "again").map(|_| "lifted".to_string()),
                "remove" => b.remove().map(|_| "removed".to_string()),
                _ => b.found().map(|f| format!("{f:?}")),
            };
            let said = match said { Ok(s) => s, Err(e) => format!("refused or failed: {e}") };
            Ok(Done { data: json!({ "answer": said }), said: vec![said] })
        }
        // (One machine started as the control plane starts a job's, with this workspace's AWS sign-in, and ended at
        // once: what AWS answers each step, for the test kit (`SUPERCI_RECORD=FILE superci debug launch REGION [spot|
        // on-demand] [TYPE,TYPE]`).)
        #[cfg(debug_assertions)]
        ("debug", "launch") => {
            let mut d = signed_in();
            let plane_id = view(&mut d)?.plane.plane_id().to_string();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let http = crate::aws::Blocking::new();
            let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_millis() as u64).unwrap_or(0);
            let region = if w(2).is_empty() { "us-east-1" } else { w(2) };
            let spot = w(3) != "on-demand";
            let types: Vec<String> = if w(4).is_empty() { vec!["c8a.large".into(), "c7a.large".into(), "m7a.large".into()] } else { w(4).split(',').map(str::to_string).collect() };
            let config = superci_core::plane::Config::new(plane_id.clone());
            use superci_core::aws;
            use futures::executor::block_on;
            // (`--image=NAME-*`: another image of its owner's; `--user-data=FILE`: a start script of one's own, the machine
            // left to it (it ends itself), for a bench experiment.)
            let image_name = args.get("image").unwrap_or(&config.image_name).to_string();
            let script = match args.get("user-data") { Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?, None => "#!/bin/bash\npoweroff\n".to_string() };
            let keep = args.has("user-data");
            let (image, image_gb) = block_on(aws::latest_image_sized(&http, region, &creds, &config.image_owner, &image_name, now()))?;
            let network = block_on(aws::find_network(&http, region, &creds, &plane_id, now()))?;
            let types = block_on(aws::offered_types(&http, region, &creds, &types, now())).unwrap_or(types);
            let tags = vec![("Name".to_string(), "superci-probe".to_string()), ("superci-plane".to_string(), plane_id.clone()), ("superci-job".to_string(), "1".to_string()), ("superci-repo".to_string(), "acme/app".to_string())];
            let launch = aws::Launch { region, network: network.as_ref(), image: &image, types: &types, user_data: &script, disk_gb: args.get("disk").and_then(|d| d.parse().ok()).unwrap_or(0).max(image_gb).max(20), disk_mibs: 125, disk_iops: 3000, tags: &tags, spot, os: "linux",
                clock: None, skip: &[], avoid: &[], timer: None, jitter_ms: 0, fleet: spot, store_disk: args.get("extra-disk").and_then(|v| v.split_once(':')).and_then(|(g, m)| Some((g.parse().ok()?, m.parse().ok()?))) };
            let mut told = aws::Told::default();
            let started = block_on(aws::run_instance(&http, &creds, &launch, now(), &mut told));
            if let Some(name) = told.template_left.take() { let _ = block_on(aws::delete_template(&http, region, &creds, &name, now())); }
            let said = match &started {
                Ok(m) if keep => format!("started {} ({} in {}) from {image}, left to its script", m.id, m.kind, m.zone),
                Ok(m) => { let ended = block_on(aws::terminate_instance(&http, region, &creds, &m.id, now())); format!("started {} ({} in {}), ended: {ended:?}; asked {:?}", m.id, m.kind, m.zone, told.asked) }
                Err(e) => format!("not started: {e}; refused {:?}", told.refused),
            };
            Ok(Done { data: json!({ "answer": said }), said: vec![said] })
        }
        // (Links signed with this workspace's AWS sign-in, an hour long, for paths in a bucket: `superci debug presign
        // METHOD REGION BUCKET PREFIX PIECES` writes {path: link} for index.json, meta.tar.zst, layers.sqsh and p/000000…
        // to stdout, for trying things on a machine that has no sign-in.)
        #[cfg(debug_assertions)]
        ("debug", "presign") => {
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_millis() as u64).unwrap_or(0);
            let (method, region, bucket, prefix) = (w(2), w(3), w(4), w(5).trim_matches('/'));
            let pieces: u32 = w(6).parse().map_err(|_| "PIECES is a number")?;
            let mut paths: Vec<String> = ["index.json", "hot.json", "meta.tar.zst", "layers.sqsh", "image-reader", "baseline.txt"].iter().map(|s| s.to_string()).collect();
            paths.extend((0..pieces).map(|i| format!("p/{i:06}")));
            let out = paths.into_iter().map(|p| superci_core::aws::presign(method, &superci_core::aws::s3_url(bucket, region, &format!("{prefix}/{p}")), &creds, region, 3600, now).map(|link| (p, Value::from(link))))
                .collect::<std::result::Result<serde_json::Map<String, Value>, String>>()?;
            println!("{}", serde_json::Value::Object(out));
            Ok(Done { said: vec![], data: json!({}) })
        }
        // (A signed call to an AWS service that speaks REST, S3 among them: `superci debug rest REGION SERVICE METHOD URL
        // [FILE]`, FILE being the body.)
        #[cfg(debug_assertions)]
        ("debug", "rest") => {
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let body = if w(6).is_empty() { vec![] } else { std::fs::read(w(6)).map_err(|e| format!("{}: {e}", w(6)))? };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_millis() as u64).unwrap_or(0);
            let mut headers = vec![("x-amz-content-sha256".to_string(), superci_core::crypto::sha256_hex(&body))];
            // (`--header=NAME:VALUE`, signed with the rest: `X-Amz-Invocation-Type:Event`, say.)
            for h in args.all("header") { if let Some((k, v)) = h.split_once(':') { headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string())) } }
            let signed = superci_core::aws::sigv4(w(4), w(5), &headers, &body, &creds, w(2), w(3), &superci_core::aws::amz_date(now))?;
            let mut r = superci_core::io::Request::new(w(4), w(5)).with_body(body);
            r.headers = signed;
            let res = futures::executor::block_on(superci_core::io::Http::send(&crate::aws::Blocking::new(), r))?;
            let said = format!("{} {}", res.status, res.body_text());
            Ok(Done { data: json!({ "answer": said }), said: vec![said] })
        }
        // (One call to Cloudflare's API with this workspace's sign-in: `superci debug cloudflare METHOD PATH [FILE]`.)
        #[cfg(debug_assertions)]
        ("debug", "cloudflare") => {
            let mut d = signed_in();
            // (Renewed as every command renews it, and kept: Cloudflare takes a renewal token once.)
            if d.cf.is_none() { return Err("Sign in with Cloudflare first.".into()) }
            let cf = d.cloudflare()?;
            let body = if w(4).is_empty() { None } else { Some(std::fs::read(w(4)).map_err(|e| format!("{}: {e}", w(4)))?) };
            // (Its analytics, `POST /graphql FILE` with {"query", "variables"}: answered in GraphQL's own shape.)
            let got = if w(3) == "/graphql" {
                let asked: Value = serde_json::from_slice(&body.unwrap_or_default()).map_err(|e| format!("{}: {e}", w(4)))?;
                cf.graphql(asked["query"].as_str().unwrap_or_default(), asked["variables"].clone())?
            } else { cf.debug_call(w(2), w(3), body)? };
            Ok(Done { said: vec![got.to_string()], data: got })
        }
        // (GitHub's runner taking one job from SuperCI's own service, run here: `superci debug runner-service FILE JOB
        // ROOT PORT JITFILE [--secret=N=V] [--var=N=V]`, ROOT being where the runner's calls go, e.g. http://localhost:8080/runner/rig.)
        #[cfg(debug_assertions)]
        ("debug", "runner-service") => {
            let pairs = |flag: &str| args.all(flag).iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
            let said = crate::runner_local::serve(w(2), w(3), w(5).parse().unwrap_or(8099), w(4), &pairs("secret"), &pairs("var"), w(6))?;
            Ok(Done { data: json!({ "answer": said }), said: vec![said] })
        }
        #[cfg(debug_assertions)]
        ("debug", kind @ ("ec2" | "json" | "iam")) => {
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_millis() as u64).unwrap_or(0);
            let http = crate::aws::Blocking::new();
            let answer = if kind == "iam" {
                let params: serde_json::Map<String, Value> = args.words[3.min(args.words.len())..].iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), Value::from(v))).collect();
                futures::executor::block_on(superci_core::aws::iam(&http, &creds, w(2), Value::Object(params), now))
            } else if kind == "ec2" {
                let params: serde_json::Map<String, Value> = args.words[4.min(args.words.len())..].iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), Value::from(v))).collect();
                futures::executor::block_on(superci_core::aws::ec2(&http, w(2), &creds, w(3), Value::Object(params), now))
            } else {
                let body: Value = serde_json::from_str(w(5)).map_err(|e| e.to_string())?;
                let service = w(3).split('.').next().unwrap_or_default().to_string();
                let host = if w(3).contains('.') { w(3).to_string() } else { format!("{}.{}.amazonaws.com", w(3), w(2)) };
                let (target, kind) = match w(4).strip_suffix("@1.0") { Some(t) => (t, "application/x-amz-json-1.0"), None => (w(4), "application/x-amz-json-1.1") };
                let (path, target) = if target.starts_with('/') { (target, None) } else { ("/", Some(target)) };
                futures::executor::block_on(superci_core::aws::json_call(&http, "POST", &format!("https://{host}{path}"), w(2), &service, target, if target.is_some() { kind } else { "application/json" }, Some(&body), &creds, now)).map(|v| v.to_string())
            };
            let said = match answer { Ok(a) => a, Err(e) => format!("refused or failed: {e}") };
            Ok(Done { data: json!({ "answer": said }), said: vec![said] })
        }
        ("accounts", "list") => {
            let d = signed_in();
            let mut accounts: Vec<Value> = d.aws_accounts_signed_in().into_iter().map(|(id, who)| json!({ "cloud": "aws", "id": id, "signed_in_as": who })).collect();
            accounts.extend(d.other_accounts_signed_in().into_iter().map(|(cloud, id)| json!({ "cloud": cloud, "id": id })));
            let said = if accounts.is_empty() { vec![NOT_SIGNED_IN.to_string()] } else { accounts.iter().map(|a| format!("{:<11} {}{}", a["cloud"].as_str().unwrap_or_default(), a["id"].as_str().unwrap_or_default(), a["signed_in_as"].as_str().map(|w| format!("  {w}")).unwrap_or_default()).trim_end().to_string()).collect() };
            Ok(Done { said, data: json!({ "accounts": accounts }) })
        }
        // An account's limit for a month, its providers together (AWS: spot and on-demand machines).
        ("accounts", "update") => {
            if w(2) != "aws" { return usage("Say which cloud's: `superci accounts update aws --monthly-usd=200`. Another provider's limit is its own: `superci runners update CLOUD --monthly-usd=N`.") }
            if !["monthly-usd", "daily-usd", "aws-budget"].iter().any(|f| args.has(f)) { return usage("Say what to change: `superci accounts update aws --monthly-usd=200` (see `superci help accounts`).") }
            let budget = args.get("aws-budget");
            if budget.is_some_and(|b| !["none", "emails", "stops", "lift"].contains(&b)) { return usage("--aws-budget is none, emails, stops or lift (see `superci help accounts`).") }
            let mut d = signed_in();
            to_set(&mut d)?;
            in_use(&d)?;
            let now = view(&mut d)?.routing();
            let (month, day) = (given_dollars(args, "monthly-usd", now.account_usd.get("aws").copied())?, given_dollars(args, "daily-usd", now.account_daily_usd.get("aws").copied())?);
            let mut fields = vec![field("action", "account_usd"), field("account", "aws"), field("usd", month.clone()), field("daily", day.clone())];
            if let Some(b) = budget { fields.push(field("budget", b)) }
            if let Some(email) = args.get("email") { fields.push(field("email", email)) }
            let in_aws = match budget { Some("none") => "; no budget in AWS itself", Some("emails") => "; AWS emails at 80% and 100% of it", Some("stops") => "; AWS emails, and stops new machines at 100% of it", Some("lift") => "; AWS's rule taken off the control plane's role", _ => "" };
            apply!(d, args, format!("let the AWS account spend on machines {} a month and {} a day (spot and on-demand together){in_aws}", or_no_limit(&month), or_no_limit(&day)), "/routing", &fields);
            let v = view(&mut d)?;
            let routing = v.routing();
            let (spent, cap, today, day_cap) = (v.spend_account("aws"), routing.account_usd.get("aws").copied(), v.spend_today_account("aws"), routing.account_daily_usd.get("aws").copied());
            let said_budget = match &routing.aws_budget { None => "No budget in AWS itself.", Some(b) if b.stop => "In AWS itself: emails at 80% and 100% of the month's limit, and no new machines at 100%.", Some(_) => "In AWS itself: emails at 80% and 100% of the month's limit." };
            Ok(Done { said: vec![month_line("The AWS account", spent, cap), day_line(today, day_cap), said_budget.to_string()],
                data: json!({ "ok": true, "cloud": "aws", "monthly_usd": cap, "spent_usd": spent, "daily_usd": day_cap, "spent_today_usd": today, "aws_budget": routing.aws_budget.as_ref().map(|b| if b.stop { "stops" } else { "emails" }) }) })
        }
        ("accounts", "retrieve") => {
            if w(2) != "aws" { return usage("Say which cloud's: `superci accounts retrieve aws` (the look is at AWS accounts; other clouds have nothing to look for yet).") }
            let mut d = signed_in();
            let (account, who, look, creds) = d.look_at_aws(args.get("account").unwrap_or_default())?;
            let mut said = aws_look_lines(&account, &who, &look);
            // (And what SuperCI lets it spend, when its control plane is the one in use here.)
            let plane = d.current().ok().flatten().filter(|v| v.status.as_ref().is_some_and(|s| s["aws"]["account_id"].as_str() == Some(account.as_str())));
            let mut limits = plane.as_ref().map(|v| { let (lines, data) = other_limits(v); said.push(String::new()); said.extend(lines.into_iter().filter(|l| !l.contains("superci runners update"))); data["aws_account"].clone() });
            // Its budget in AWS counts AWS's own bill, hours behind SuperCI's estimate: its emails follow that count.
            if let (Some(v), Some(l)) = (plane.as_ref(), limits.as_mut().filter(|l| !l["aws_budget"].is_null())) {
                if let Ok(Some(found)) = crate::aws_budget::with(&creds, &account, v.plane.plane_id(), |b| b.found()) {
                    said.push(format!("  AWS has counted ${:.2} of its {} so far: its own bill, hours behind SuperCI's estimate. Its emails go at 80% and 100% of that count.", found.spent, superci_core::plane::dollars(found.amount)));
                    l["aws_counted_usd"] = json!(found.spent);
                }
            }
            Ok(Done { said, data: json!({ "cloud": "aws", "id": account, "signed_in_as": who, "kind": look.kind(), "allowed_regions": look.allowed_regions(), "organization": look.organization().map(|r| json!({ "management_account": r.management_account(), "policy": r.policy_id() })), "allows": look, "limits": limits }) })
        }
        ("images", "list") => {
            let mut d = signed_in();
            let (account, _, look, creds) = d.look_at_aws(args.get("account").unwrap_or_default())?;
            let mut rows = vec![];
            for (region, there) in &look.images {
                // (A copy that a newer one replaced goes, now that the newer one is there.)
                let tidied = crate::aws::tidy_copies(&creds, region).unwrap_or(0);
                let mut copies = crate::aws::copied_images(&creds, region).unwrap_or_default();
                copies.sort_by(|a, b| crate::aws::build_of(&b.name).cmp(crate::aws::build_of(&a.name)));
                // Its maker builds the image anew every week or two; a copy stays the build it was made of.
                let behind = if copies.is_empty() { None } else { crate::aws::newest_build().and_then(|newest| crate::aws::behind(&copies, &newest).map(|have| (have.to_string(), newest))) };
                let state = match (copies.first(), there) {
                    (Some(c), _) if c.state == "available" => format!("your copy {} ({}){}{}", c.id, c.name,
                        match &behind { Some((have, newest)) => format!(": the build of {}; one of {} is out, `superci images update {region}` copies it in", crate::aws::build_day(have), crate::aws::build_day(newest)), None => String::new() },
                        if tidied > 0 { format!(" (the copy before it is removed)") } else { String::new() }),
                    (Some(c), _) => format!("your copy {} is being made ({})", c.id, c.state),
                    (None, Some(true)) => "published".to_string(),
                    (None, Some(false)) => format!("none: `superci images create {region}` copies it in"),
                    (None, None) => "not known".to_string(),
                };
                rows.push(json!({ "region": region, "published": copies.is_empty() && *there == Some(true), "copies": copies, "newer_build": behind.as_ref().map(|(_, newest)| newest), "said": state }));
            }
            let said = if rows.is_empty() { vec!["This account can use none of the regions SuperCI works in.".to_string()] } else { rows.iter().map(|r| format!("{:<15} {}", r["region"].as_str().unwrap_or_default(), r["said"].as_str().unwrap_or_default())).collect() };
            Ok(Done { said, data: json!({ "account": account, "images": rows }) })
        }
        ("images", "create") => {
            let region = w(2);
            if !superci_core::page::REGIONS.contains(&region) { return usage("Say where: `superci images create REGION` (a region the image jobs run on is not in: `superci images list`).") }
            let from = match args.get("from").map(|f| f.split_once(':')) { None => None, Some(Some((r, i))) if superci_core::aws::aws_id(i, "ami-") => Some((r, i)), Some(_) => return usage("--from is a region and an image: --from=eu-west-1:ami-0123456789abcdef0.") };
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            // One copy is enough: a second would only be paid for.
            if let Some(copy) = crate::aws::copied_images(&creds, region)?.first() { return Err(format!("{region} has your copy of the image already: {} ({}). `superci images delete {region} --confirm` removes it.", copy.id, copy.state).into()) }
            // Where it would come from is read first (the maker's public list): a dry run says it too.
            let source = crate::aws::copy_source_for(region, from)?;
            let (from_region, from_image) = (source.1.clone(), source.2.clone());
            if let Some(done) = dry(args, &format!("copy the image jobs run on into {region}, from {from_region} ({from_image})"), "images", &[]) { return Ok(done) }
            let id = crate::aws::copy_image_into(&creds, region, &source)?;
            Ok(Done { said: vec![format!("Copying the image jobs run on into {region}, from {from_region} ({from_image}): {id}."), "AWS takes about ten minutes; `superci images list` shows when it is there.".to_string(),
                format!("It is kept in your account's storage there (about $1.50 a month). `superci images delete {region} --confirm` removes it.")], data: json!({ "ok": true, "region": region, "image": id, "from": { "region": from_region, "image": from_image } }) })
        }
        // A newer build of the image, copied in where the account holds a copy of an older one.
        ("images", "update") => {
            let region = w(2);
            if !superci_core::page::REGIONS.contains(&region) { return usage("Say where: `superci images update REGION` (a region your copy of the image is in: `superci images list`).") }
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            let copies = crate::aws::copied_images(&creds, region)?;
            if copies.is_empty() { return Err(format!("{region} has no copy of yours. `superci images create {region}` copies the image in, where it is not published.").into()) }
            if let Some(coming) = copies.iter().find(|c| c.state == "pending") { return Err(format!("A copy is on its way into {region} already: {} ({}).", coming.id, coming.name).into()) }
            let newest = crate::aws::newest_build().ok_or("The list of runner images could not be read from GitHub.")?;
            let Some(have) = crate::aws::behind(&copies, &newest).map(str::to_string) else { return Ok(Done { said: vec![format!("Your copy in {region} is of the newest build ({}).", crate::aws::build_day(&newest))], data: json!({ "ok": true, "region": region, "newest": true }) }) };
            let source = crate::aws::copy_source_for(region, None)?;
            let (from_region, from_image) = (source.1.clone(), source.2.clone());
            if let Some(done) = dry(args, &format!("copy the build of {} into {region}, from {from_region} ({from_image}), in place of your copy of {}", crate::aws::build_day(&newest), crate::aws::build_day(&have)), "images", &[]) { return Ok(done) }
            let id = crate::aws::copy_image_into(&creds, region, &source)?;
            Ok(Done { said: vec![format!("Copying the build of {} into {region}, from {from_region} ({from_image}): {id}.", crate::aws::build_day(&newest)),
                "AWS takes about ten minutes. Jobs start from your older copy until it is there; then from the new one.".to_string(),
                "The older copy is removed once the new one is there: at the next `superci images list`, or by the dashboard.".to_string()], data: json!({ "ok": true, "region": region, "image": id, "from": { "region": from_region, "image": from_image } }) })
        }
        ("images", "delete") => {
            let region = w(2);
            if !superci_core::page::REGIONS.contains(&region) { return usage("Say where: `superci images delete REGION --confirm`.") }
            let would = format!("remove your copy of the image jobs run on from {region}: no job can start there after");
            confirmed(args, &would)?;
            let mut d = signed_in();
            let (_, creds) = d.aws_account(args.get("account").unwrap_or_default())?;
            if let Some(done) = dry(args, &would, "images", &[]) { return Ok(done) }
            let removed = crate::aws::delete_copied_images(&creds, region)?;
            Ok(Done::said(if removed == 0 { format!("There was no copy of yours in {region}.") } else { format!("Removed your copy of the image from {region}, with its storage.") }))
        }
        ("runners", "list") => runners_of(&mut signed_in()),
        ("connections", "list") => connections(),
        ("deliveries", operation @ ("list" | "retrieve")) => {
            let id = w(2);
            if operation == "retrieve" && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit())) { return usage("Say which: `superci deliveries retrieve ID` (ids are in `superci deliveries list`).") }
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            let got = crate::cloudflare::plane_get(v.plane.url(), &key, &if operation == "retrieve" { format!("/github/deliveries?id={id}") } else { "/github/deliveries".to_string() })
                .map_err(|e| if e.contains("404") { "This needs control plane 0.11.2 or newer: `superci planes update`.".to_string() } else { e })?;
            let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
            let mut said = vec![];
            for app in got.as_array().into_iter().flatten() {
                let list = &app["deliveries"];
                if let Some(e) = list["error"].as_str() { said.push(format!("{}: GitHub would not say ({e})", s(&app["app"]))); continue }
                if operation == "retrieve" {
                    said.push(format!("{} {} {} · answered {} {} · job {} {} {}", s(&list["delivered_at"]), s(&list["event"]), s(&list["action"]), list["status_code"], s(&list["status"]), list["job"], s(&list["job_name"]), list["labels"]));
                    said.push(format!("sent to {}", s(&list["url"])));
                    if let Some(a) = list["answer"].as_str().filter(|a| !a.is_empty()) { said.push(format!("answer: {a}")) }
                } else {
                    for x in list.as_array().into_iter().flatten() {
                        said.push(format!("{}  {:<13} {:<12} {} {}{}  {}", s(&x["delivered_at"]).replace('T', " ").chars().take(19).collect::<String>(), s(&x["event"]), s(&x["action"]), x["status_code"], s(&x["status"]).chars().take(60).collect::<String>(), if x["redelivery"] == true { "  (sent again)" } else { "" }, x["id"]));
                    }
                }
            }
            if said.is_empty() { said.push("GitHub is not connected, or it has sent nothing lately.".into()) }
            Ok(Done { said, data: got })
        }
        ("planes", "list") => planes(),
        ("logout", "") => { let said = signed_in().logout()?; Ok(Done { data: json!({ "ok": true, "done": said }), said }) }

        ("planes", "create") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return usage("Say where: `superci planes create aws --region=us-east-1`, `superci planes create cloudflare` or `superci planes create modal`.") }
            let region = match (cloud, args.get("region")) { ("aws", None) => return usage("Say where: --region=us-east-1 (any AWS region the dashboard's Control plane page lists)."), (_, r) => r.unwrap_or_default().to_string() };
            let mut d = ready()?;
            let (fields, place) = match cloud {
                "aws" => (std::iter::once(field("region", region.clone())).chain(args.get("account").map(|a| field("account", a))).collect(), format!("AWS, in {region}")),
                "cloudflare" => {
                    let accounts = d.cloudflare_accounts();
                    let account = match (args.get("account"), accounts) {
                        (Some(a), _) => a.to_string(),
                        (None, [(only, _)]) => only.clone(),
                        (None, []) => return Err(d.ended_said("Sign in with Cloudflare first.".into()).into()),
                        (None, several) => return usage(format!("Say which account: --account=ID. Yours: {}.", several.iter().map(|(id, name)| format!("{id} ({name})")).collect::<Vec<_>>().join(", "))),
                    };
                    (vec![field("account", account.clone())], format!("Cloudflare, in account {account}"))
                }
                _ => (vec![], "Modal".to_string()),
            };
            apply!(d, args, format!("deploy a control plane to {place}"), &format!("/plane/{cloud}"), &fields);
            Ok(Done::said(d.wait(Background::Deploy, steps)?))
        }
        ("planes", "update") => {
            let mut d = ready()?;
            let plane = in_use(&d)?;
            let which = if w(2).is_empty() { format!("in use ({})", plane.place()) } else { w(2).to_string() };
            // (One on this version already is deployed again: what this version has beside it is made where missing.)
            let now = d.current().ok().flatten().and_then(|v| v.version);
            let would = match now.as_deref() {
                Some(v) if v == DASHBOARD_VERSION => format!("deploy the control plane {which} again: it is on {v} already; what this version has beside it is made where missing"),
                Some(v) => format!("update the control plane {which} from {v} to {DASHBOARD_VERSION}"),
                None => format!("update the control plane {which} to {DASHBOARD_VERSION}"),
            };
            apply!(d, args, would, "/plane/update", &[field("plane", w(2))]);
            Ok(Done::said(d.wait(Background::Update, steps)?))
        }
        ("planes", "move") => {
            if w(2).is_empty() { return usage("Say where to: `superci planes move ID` (ids are in `superci planes list`).") }
            let mut d = ready()?;
            apply!(d, args, format!("move to the control plane {}: settings, history and runner providers go along, then GitHub and GitLab are switched over", w(2)), "/plane/move", &[field("plane", w(2))]);
            Ok(Done::said(d.wait(Background::Move, steps)?))
        }
        ("planes", "allow") => {
            let mut d = ready()?;
            let plane = if w(2).is_empty() { in_use(&d)?.plane_id().to_string() } else { w(2).to_string() };
            // (A dry run reads what its role lacks now, and says it.)
            if args.has("dry-run") {
                let lacks = d.role_lacks(&plane)?;
                let said = if lacks.is_empty() { format!("The AWS role of the control plane {plane} has what this version asks for: nothing would change.") }
                    else { format!("Would give the AWS role of the control plane {plane} what it lacks: {}.", lacks.iter().map(|n| format!("{} (since {})", n.what, n.since)).collect::<Vec<_>>().join("; ")) };
                return Ok(Done { said: vec![said.clone()], data: json!({ "dry_run": true, "would": said, "lacks": lacks.iter().map(|n| json!({ "id": n.id, "what": n.what, "since": n.since })).collect::<Vec<_>>() }) })
            }
            apply!(d, args, format!("give the AWS role of the control plane {plane} what this version asks for"), "/permissions/aws", &[field("plane", plane.clone())]);
            Ok(Done::said("Its AWS role has what this version asks for."))
        }
        ("planes", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci planes delete ID --confirm` (ids are in `superci planes list`).") }
            let would = format!("delete the control plane {} and what was made for it in its cloud", w(2));
            confirmed(args, &would)?;
            let mut d = ready()?;
            Ok(Done::said(apply!(d, args, would, "/plane/delete", &[field("plane", w(2))])))
        }
        ("leave", "") => {
            // `--plane=ID` says which one is stopped (the others go on). What deletes takes its control plane from
            // the command itself, never from SUPERCI_PLANE: without the flag it is the only live one, or it stops.
            let named = args.get("plane").map(str::trim).filter(|p| !p.is_empty()).map(str::to_string);
            if named.is_none() { NAMED.with(|n| n.borrow_mut().take()); crate::view::name(None) }
            let would = match &named {
                Some(id) => format!("stop the control plane {id}: GitHub and GitLab stop sending it jobs, and it is deleted with what was made for it"),
                None => "stop SuperCI: GitHub and GitLab stop sending jobs, and the control plane is deleted with what was made for it, with the ones that serve nothing".to_string(),
            };
            confirmed(args, &would)?;
            let mut d = ready()?;
            in_use(&d)?;
            let mut fields = vec![field("confirm", "superci")];
            if let Some(id) = &named { fields.push(field("plane", id)) }
            Ok(Done::said(apply!(d, args, would, "/plane/leave", &fields)))
        }

        ("runners", "create") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return usage("Say which: `superci runners create aws`, `cloudflare` or `modal`.") }
            let mut d = ready()?;
            let plane = in_use(&d)?;
            // As the Add runners page does: a control plane starts machines in its own cloud itself; elsewhere it is
            // connected (AWS: a role it may assume; Cloudflare, Modal: a small launcher there).
            // Modal: `--account=WORKSPACE` is a further workspace signed in to (`superci login modal --add`), whose
            // launcher is a provider of its own (`modal:WORKSPACE`).
            let workspace = args.get("account").filter(|_| cloud == "modal").map(str::trim).filter(|w| !w.is_empty());
            let (path, fields) = match (cloud, &plane) {
                ("aws", Plane::Aws { .. }) => ("/runners/aws-own", vec![]),
                ("aws", _) => match args.get("region") { Some(r) => ("/aws/connect", vec![field("region", r)]), None => return usage("Say where its machines start: `superci runners create aws --region=us-east-1`.") },
                ("modal", Plane::Modal { workspace: own, .. }) if workspace.is_none_or(|w| w == own) => ("/runners/modal-own", vec![]),
                ("modal", _) => ("/runners/modal", workspace.map(|w| vec![field("account", w)]).unwrap_or_default()),
                _ => ("/runners/cloudflare", args.get("account").map(|a| vec![field("account", a)]).unwrap_or_default()),
            };
            // (Done, it says nothing of its own.)
            let cloud = match workspace { Some(w) if path == "/runners/modal" => format!("modal:{w}"), _ => cloud.to_string() };
            apply!(d, args, format!("let {cloud} run jobs for the control plane in use ({})", plane.place()), path, &fields);
            Ok(Done::said(format!("{cloud} runs jobs now.")))
        }
        ("runners", "delete") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) && !cloud.starts_with("modal:") { return usage("Say which: `superci runners delete aws --confirm` (or cloudflare, modal, modal:WORKSPACE).") }
            let would = format!("remove {cloud} as a runner provider{}", if args.has("only-here") { ", leaving what SuperCI made for it there" } else { " and delete what SuperCI made for it there" });
            confirmed(args, &would)?;
            let mut d = ready()?;
            let plane = in_use(&d)?;
            let mut fields = vec![field("plane", plane.plane_id()), field("cloud", cloud)];
            if args.has("only-here") { fields.push(field("only", "on")) }
            if args.has("stop-jobs") { fields.push(field("stop", "on")) }
            Ok(Done::said(apply!(d, args, would, "/runners/remove", &fields)))
        }
        ("runners", "order") => {
            if args.words.len() < 3 { return usage("Say the order: `superci runners order aws aws-on-demand cloudflare`.") }
            let mut d = signed_in();
            let now = view(&mut d)?.order();
            // The places named, in that order; every other one is taken out of the order (its settings kept).
            let order: Vec<String> = args.words[2..].to_vec();
            if let Some(unknown) = order.iter().find(|c| !now.iter().any(|p| p.cloud == **c)) { return usage(format!("{unknown} is not one of your runner providers ({}).", now.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(", "))) }
            let out: Vec<String> = now.iter().map(|p| p.cloud.clone()).filter(|c| !order.contains(c)).collect();
            let names: Vec<String> = (0..order.len()).map(|i| format!("cloud_{i}")).chain((0..out.len()).map(|i| format!("out_{i}"))).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "order")];
            fields.extend(names.iter().zip(order.iter().chain(&out)).map(|(name, cloud)| (name.as_str(), cloud.clone())));
            to_set(&mut d)?;
            apply!(d, args, format!("set the order to {}{}", order.join(", "), if out.is_empty() { String::new() } else { format!(" (taking out {})", out.join(", ")) }), "/routing", &fields);
            let mut said = vec![format!("Order: {}.", order.join(", "))];
            if !out.is_empty() { said.push(format!("Taken out: {}.", out.join(", "))) }
            Ok(Done { said, data: json!({ "ok": true, "order": order, "taken_out": out }) })
        }
        ("runners", "update") => {
            let cloud = w(2);
            let mut d = signed_in();
            let order = view(&mut d)?.order();
            let Some(pool) = order.iter().find(|p| p.cloud == cloud) else { return usage(format!("Say which: `superci runners update CLOUD …`, one of {}.", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(", "))) };
            if args.has("enabled") { return usage(format!("The order says what is used: `superci runners order …` without {cloud} takes it out, with it puts it back.")) }
            if args.has("regions") && superci_core::plane::account_of(cloud) != "aws" { return usage("--regions is for aws and aws-on-demand.") }
            // The form's fields as they stand, with what was given in their place (the page sends them all).
            let limit = |flag: &str, now: Option<String>| match args.get(flag) { Some("none") => String::new(), Some(v) => v.to_string(), None => now.unwrap_or_default() };
            let regions: Vec<String> = args.get("regions").map(|r| r.split(',').map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect()).unwrap_or_default();
            let names: Vec<String> = (0..regions.len()).map(|i| format!("region_{i}")).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "pool"), field("cloud", cloud), field("current", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(",")),
                field("max", limit("max-jobs", pool.max_jobs.map(|n| n.to_string()))), field("usd", limit("monthly-usd", pool.monthly_usd.map(|n| n.to_string())))];
            for (name, region) in names.iter().zip(&regions) { fields.push((name, region.clone())) }
            if args.has("networks") { fields.push(field("networks", args.all("networks").into_iter().filter(|n| *n != "none").collect::<Vec<_>>().join("\n"))) }
            if let Some(location) = args.get("location") { fields.push(field("location", location)) }
            if let Some(image) = args.get("image") { fields.push(field("image", if image == "none" { "" } else { image })) }
            let given: Vec<String> = ["max-jobs", "monthly-usd", "regions", "networks", "location", "image"].iter().filter(|f| args.has(f)).map(|f| format!("--{f}={}", args.get(f).unwrap_or_default())).collect();
            if given.is_empty() { return usage(format!("Say what to change: `superci runners update {cloud} --max-jobs=20` (see `superci help runners`).")) }
            to_set(&mut d)?;
            apply!(d, args, format!("update {cloud}: {}", given.join(" ")), "/routing", &fields);
            runners_of(&mut d)
        }
        ("labels", "list") => {
            let labels = view(&mut signed_in())?.labels();
            Ok(Done { said: labels.iter().enumerate().map(|(i, l)| if i == 0 && labels.len() > 1 { format!("{l}  (shown in examples)") } else { l.clone() }).collect(), data: json!({ "labels": labels }) })
        }
        ("labels", operation @ ("create" | "delete")) => {
            if w(2).is_empty() { return usage(format!("Say which: `superci labels {operation} NAME`.")) }
            let would = if operation == "create" { format!("add {} to the labels workflows may name; the ones before keep working", w(2)) } else { format!("stop answering to the label {}: jobs that still name it would wait for a runner that never comes", w(2)) };
            if operation == "delete" { confirmed(args, &would)? }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, would, "/labels", &[field("action", if operation == "create" { "add" } else { "remove" }), field("label", w(2))]);
            let labels = view(&mut d)?.labels();
            Ok(Done { said: vec![format!("It answers to: {}.", labels.join(", ")), format!("In a workflow: `runs-on: {}`.", labels.first().cloned().unwrap_or_default())], data: json!({ "ok": true, "labels": labels }) })
        }
        ("name", "retrieve") => {
            let name = view(&mut signed_in())?.name();
            Ok(Done { said: vec![name.clone()], data: json!({ "name": name }) })
        }
        ("name", "update") => {
            if w(2).is_empty() { return usage("Say what to call it: `superci name update AcmeCI`.") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, format!("call it {}", w(2)), "/name", &[field("name", w(2))]);
            let name = view(&mut d)?.name();
            Ok(Done { said: vec![format!("It is called {name}.")], data: json!({ "ok": true, "name": name }) })
        }
        ("machine", operation @ ("retrieve" | "update")) => {
            let mut d = signed_in();
            let now = view(&mut d)?.machine();
            if operation == "update" {
                use superci_core::spec::{Spec, KEYS};
                if !KEYS.iter().any(|key| args.has(key)) { return usage("Say what to change: `superci machine update --cpu=4 --ram=16` (see `superci help machine`).") }
                // Each key a label has, by the same name and the same rules: what is given is set (`default` leaves
                // it to each provider's standard again), the rest stays as it is.
                let mut want = now.pairs();
                KEYS.iter().filter_map(|key| args.get(key).map(|value| (*key, value))).for_each(|(key, value)| {
                    want.retain(|(k, _)| *k != key);
                    if value != "default" { want.push((key, value.to_string())) }
                });
                let mut machine = Spec::default();
                for (key, value) in &want { if let Err(e) = machine.set(key, value) { return usage(e) } }
                if let Err(e) = machine.check() { return usage(e) }
                to_set(&mut d)?;
                let would = if want.is_empty() { "each provider's standard".to_string() } else { machine.label("").trim_start_matches('/').replace('/', ", ") };
                apply!(d, args, format!("give `runs-on: superci` this machine: {would}"), "/machine", &want);
            }
            let seen = view(&mut d)?;
            let (machine, label) = (seen.machine(), seen.label());
            Ok(Done { said: vec![format!("`runs-on: {label}` gets: {}.", machine_words(&serde_json::to_value(&machine).unwrap_or_default()))], data: json!({ "ok": true, "machine": machine }) })
        }
        ("limits", operation @ ("retrieve" | "update")) => {
            let mut d = signed_in();
            if operation == "update" {
                if !args.has("max-cpu") && !args.has("max-hours") && !args.has("monthly-usd") && !args.has("daily-usd") { return usage("Say what to change: `superci limits update --monthly-usd=200` (see `superci help limits`).") }
                to_set(&mut d)?;
                in_use(&d)?;
                if let Some(max) = args.get("max-cpu") { apply!(d, args, format!("let a label ask for up to {max} CPUs"), "/routing", &[field("action", "max_cpu"), field("max_cpu", max)]); }
                if let Some(hours) = args.get("max-hours") { apply!(d, args, format!("let a job run for up to {hours} hours"), "/routing", &[field("action", "max_hours"), field("max_hours", hours)]); }
                if args.has("monthly-usd") || args.has("daily-usd") {
                    // Both as they stand, with what was given in its place (the page sends them both).
                    let now = view(&mut d)?.routing();
                    let (month, day) = (given_dollars(args, "monthly-usd", now.monthly_usd)?, given_dollars(args, "daily-usd", now.daily_usd)?);
                    apply!(d, args, format!("let the CI spend on machines {} a month and {} a day", or_no_limit(&month), or_no_limit(&day)), "/routing", &[field("action", "monthly_usd"), field("usd", month), field("daily", day)]);
                }
            }
            let v = view(&mut d)?;
            let routing = v.routing();
            let (cpu, hours, spent, today) = (routing.max_cpu.unwrap_or(superci_core::plane::MAX_CPU), routing.max_minutes.unwrap_or(superci_core::plane::MAX_JOB_MINUTES) / 60, v.spend_total(), v.spend_today_total());
            // The CI's own, then every other limit spending counts against (a job is refused at the first one reached).
            let (others, other_data) = other_limits(&v);
            let mut said = vec![format!("{} (`superci limits update --monthly-usd=N --daily-usd=N`)", month_line("The CI", spent, routing.monthly_usd).trim_end_matches('.')), day_line(today, routing.daily_usd)];
            said.extend(others);
            said.extend([format!("A label may ask for up to {cpu} CPUs."), format!("A job may run for up to {hours} hours (6 at most on Cloudflare, 24 on Modal).")]);
            Ok(Done { said, data: json!({ "ok": true, "max_cpu": cpu, "max_hours": hours, "monthly_usd": routing.monthly_usd, "spent_usd": spent, "daily_usd": routing.daily_usd, "spent_today_usd": today,
                "aws_account": other_data["aws_account"], "providers": other_data["providers"] }) })
        }
        ("public_repos", "list") => {
            let allowed = view(&mut signed_in())?.routing().public_repos;
            Ok(Done { said: if allowed.is_empty() { vec!["No public repository is allowed to run here. Allow one: `superci public_repos create OWNER/REPO`.".into()] } else { allowed.clone() }, data: json!({ "public_repos": allowed }) })
        }
        ("runner_images", "list") => {
            let images = view(&mut signed_in())?.routing().images;
            let said = if images.is_empty() { vec!["No image of your own. Add one: `superci runner_images create NAME --reference=REGISTRY/IMAGE:TAG`.".into()] }
                else { images.iter().map(|(name, i)| format!("{name}  {}{}", i.reference, i.modal_secret.as_ref().map(|s| format!("  (Modal Secret {s})")).unwrap_or_default())).collect() };
            Ok(Done { said, data: json!({ "runner_images": images }) })
        }
        ("runner_images", "create") => {
            let Some(reference) = args.get("reference").filter(|_| !w(2).is_empty()) else { return usage("Say which and where: `superci runner_images create NAME --reference=REGISTRY/IMAGE:TAG`.") };
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, format!("let jobs on Modal start from {reference} when their label says image={}", w(2)), "/routing",
                &[field("action", "image_add"), field("name", w(2)), field("reference", reference), field("modal_secret", args.get("modal-secret").unwrap_or_default())]);
            Ok(Done::said(format!("Jobs on Modal start from {reference} when their label says image={} (runs-on: superci/image={}).", w(2), w(2))))
        }
        // One write of the routing (see the dashboard's `image_update`): never a delete and a create, between which a
        // job naming it would fail as an unknown image.
        ("runner_images", "update") => {
            let (to, secret_to) = (args.get("reference"), args.get("modal-secret"));
            if w(2).is_empty() || (to.is_none() && secret_to.is_none()) { return usage("Say which and what: `superci runner_images update NAME --reference=REGISTRY/IMAGE:TAG` (or --modal-secret=SECRET, empty for none).") }
            if to.is_some_and(|r| !superci_core::plane::valid_image_reference(r)) { return usage("Check the image's address: as `docker pull` takes it, ghcr.io/acme/ci:2026-10, or by its digest (ghcr.io/acme/ci@sha256:…).") }
            if secret_to.is_some_and(|s| !s.is_empty() && !superci_core::plane::valid_modal_secret(s)) { return usage("Check the Modal Secret's name: letters, digits, dashes, underscores and dots, as Modal names a Secret.") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            let Some(had) = view(&mut d)?.routing().images.remove(w(2)) else { return Err(Fail::Failed(crate::dashboard::no_image(w(2)))) };
            let reference = to.unwrap_or(&had.reference).to_string();
            let secret = secret_to.map(str::to_string).or(had.modal_secret).filter(|s| !s.is_empty());
            let with = secret.as_ref().map(|s| format!(" (Modal Secret {s})")).unwrap_or_default();
            let mut fields = vec![field("action", "image_update"), field("name", w(2))];
            if let Some(r) = to { fields.push(field("reference", r)) }
            if let Some(s) = secret_to { fields.push(field("modal_secret", s)) }
            apply!(d, args, format!("let jobs on Modal whose label says image={} start from {reference}{with}, in place of {}", w(2), had.reference), "/routing", &fields[..]);
            Ok(Done::said(format!("Jobs on Modal start from {reference}{with} when their label says image={} (runs-on: superci/image={}).", w(2), w(2))))
        }
        ("runner_images", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci runner_images delete NAME`.") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, format!("forget the image {}: jobs that name it fail", w(2)), "/routing", &[field("action", "image_remove"), field("name", w(2))]);
            Ok(Done::said(format!("{} is no longer an image here.", w(2))))
        }
        ("public_repos", operation @ ("create" | "delete")) => {
            let action = if operation == "create" { "add" } else { "remove" };
            if w(2).is_empty() { return usage(format!("Say which: `superci public_repos {operation} OWNER/REPO`.")) }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, if action == "add" { format!("let the public repository {} run here", w(2)) } else { format!("stop the public repository {} from running here", w(2)) }, "/routing", &[field("action", format!("public_{action}")), field("repo", w(2))]);
            Ok(Done::said(if action == "add" { format!("{} may run here (never a fork's pull request).", w(2)) } else { format!("{} no longer runs here.", w(2)) }))
        }

        ("connections", "delete") if w(2) == "github" => {
            if w(3).is_empty() { return usage("Say which: `superci connections delete github OWNER --confirm`.") }
            let would = format!("uninstall the GitHub App of {}: its jobs then wait for runners that never come", w(3));
            confirmed(args, &would)?;
            let mut d = ready()?;
            let apps = view(&mut d)?.status.as_ref().and_then(|s| s["apps"].as_array().cloned()).unwrap_or_default();
            let at = apps.iter().position(|a| a["owner"].as_str().is_some_and(|o| o.eq_ignore_ascii_case(w(3)))).ok_or_else(|| format!("{} is not connected here.", w(3)))?;
            // (The first one connected is the control plane's own: it goes with the control plane.)
            if at == 0 { return Err(format!("{}'s App is the first this control plane was connected with: it stays while the control plane does (`superci leave` stops SuperCI). Organizations added after it are removed this way.", w(3)).into()) }
            let app = &apps[at];
            Ok(Done::said(apply!(d, args, would, "/github/remove", &[field("app", app["id"].as_u64().unwrap_or_default().to_string())])))
        }
        ("connections", "create") if w(2) == "gitlab" => {
            let name = args.get("token-env").unwrap_or("SUPERCI_GITLAB_TOKEN");
            let Some(token) = std::env::var(name).ok().filter(|t| !t.trim().is_empty()) else {
                return usage(format!("Give the GitLab token by name: {name} (a project, group or personal access token with the scopes api, create_runner and manage_runner). It goes to your control plane, and is not kept on this machine."))
            };
            let url = args.get("url").unwrap_or("https://gitlab.com");
            let mut d = ready()?;
            in_use(&d)?;
            // (Done, it says nothing of its own.)
            apply!(d, args, format!("connect {url} with the token in {name}"), "/gitlab/connect", &[field("url", url), field("token", token), field("which", args.get("gitlab").unwrap_or_default())]);
            Ok(Done::said("GitLab is connected. Its projects send jobs here once switched on: `superci projects list`, then `superci projects update PROJECT_ID --enabled=true`."))
        }
        ("projects", "list") => {
            let mut d = signed_in();
            let (view, key) = look(&mut d)?;
            let v = crate::cloudflare::plane_get(view.plane.url(), &key, &format!("/gitlab/projects?g={}", args.get("gitlab").unwrap_or_default()))?;
            let said = v["projects"].as_array().into_iter().flatten().map(|p| format!("{:>10}  {}  {}", p["id"].as_u64().unwrap_or_default(), if p["enabled"] == true { "on " } else { "off" }, p["path"].as_str().unwrap_or_default())).collect::<Vec<_>>();
            Ok(Done { said: if said.is_empty() { vec!["No projects this token can reach.".into()] } else { said }, data: v })
        }
        ("projects", "update") => {
            let Some(enabled) = truth(args, "enabled")? else { return usage("Say which way: `superci projects update PROJECT_ID --enabled=true` (or false).") };
            let mut fields = vec![field("gitlab", args.get("gitlab").unwrap_or_default())];
            let would = match (enabled, args.has("all"), w(2)) {
                (true, true, _) => { fields.push(field("all", "on")); "send every project's jobs here".to_string() }
                (false, true, _) => return usage("Projects are switched off one at a time: `superci projects update PROJECT_ID --enabled=false`."),
                (_, _, "") => return usage("Say which: `superci projects update PROJECT_ID --enabled=true` (ids are in `superci projects list`)."),
                (_, _, id) => { fields.push(field("id", id)); fields.push(field("enabled", if enabled { "true" } else { "false" })); format!("{} project {id}'s jobs here", if enabled { "send" } else { "stop sending" }) }
            };
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, would, "/gitlab/project", &fields);
            Ok(Done::said(if enabled { "Its jobs come here now (tags: superci)." } else { "Its jobs no longer come here." }))
        }
        ("connections", "delete") if w(2) == "gitlab" => {
            let would = "disconnect that GitLab: its projects stop sending jobs here and its token is forgotten";
            confirmed(args, would)?;
            let mut d = ready()?;
            in_use(&d)?;
            apply!(d, args, would, "/gitlab/disconnect", &[field("gitlab", w(3))]);
            Ok(Done::said("GitLab is disconnected."))
        }
        // (`superci connections create github OWNER` opens the browser: see `in_browser`.)
        ("connections", operation @ ("create" | "delete")) => usage(if operation == "create" { "Say which: `superci connections create github OWNER`, or `superci connections create gitlab --url=https://gitlab.com`." } else { "Say which: `superci connections delete github OWNER --confirm`, or `superci connections delete gitlab [ID] --confirm`." }),
        (word, operation) => usage(match HELP.iter().find(|(w, ..)| *w == word) {
            Some(_) if operation.is_empty() => format!("`superci {word}` takes an operation."),
            Some(_) => format!("`{operation}` is not an operation of `superci {word}`."),
            None if word.is_empty() => String::new(),
            None => format!("`{word}` is not a command of superci."),
        }),
    }
}

/// A limit in dollars a month as a command gives it: an amount above zero, or `none` (empty: no limit).
fn dollars(given: &str) -> Result<String> {
    if given == "none" { return Ok(String::new()) }
    match given.parse::<f64>() { Ok(n) if n > 0.0 && n.is_finite() => Ok(given.to_string()), _ => Err("A limit is dollars a month (`--monthly-usd=200`), or `none`.".into()) }
}

/// A limit a flag gives (`none` lifts it), or the one that stands when the flag was not given: as the page's field.
fn given_dollars(args: &Args, flag: &str, now: Option<f64>) -> Result<String> {
    match args.get(flag) { Some(given) => dollars(given), None => Ok(now.map(|n| n.to_string()).unwrap_or_default()) }
}

/// A limit as a sentence has it: `$200`, or `without limit`.
fn or_no_limit(usd: &str) -> String { if usd.is_empty() { "without limit".to_string() } else { format!("up to ${usd}") } }

/// The other limits spending counts against, each with the command that changes it: the AWS account's for a month and
/// a day (its providers together) with its budget in AWS itself, and each provider's own for a month. With what each
/// has spent; as data too.
fn other_limits(v: &crate::view::PlaneView) -> (Vec<String>, Value) {
    let routing = v.routing();
    let mut said = vec![];
    let aws = v.status.as_ref().is_some_and(|s| !s["aws"].is_null()) || routing.account_usd.contains_key("aws");
    let (month, day) = (routing.account_usd.get("aws").copied(), routing.account_daily_usd.get("aws").copied());
    if aws {
        said.push(format!("{} (`superci accounts update aws --monthly-usd=N --daily-usd=N`)", month_line("The AWS account", v.spend_account("aws"), month).trim_end_matches('.')));
        if day.is_some() { said.push(format!("  {}", day_line(v.spend_today_account("aws"), day))) }
        said.push(match &routing.aws_budget {
            None => "No budget in AWS itself, so AWS sends no email at 80% or 100% (`superci accounts update aws --aws-budget=emails --email=ADDRESS` adds one).".to_string(),
            Some(b) if b.stop => "In AWS itself: a budget that emails at 80% and 100% of the account's monthly limit, and stops new machines at 100%.".to_string(),
            Some(_) => "In AWS itself: a budget that emails at 80% and 100% of the account's monthly limit.".to_string(),
        });
    }
    let pools: Vec<Value> = routing.order.iter().filter_map(|p| p.monthly_usd.map(|cap| {
        said.push(format!("{} spent ${:.2} this month, of {} allowed (`superci runners update {} --monthly-usd=N`).", superci_core::plane::cloud_name(&p.cloud), v.spend(&p.cloud), superci_core::plane::dollars(cap), p.cloud));
        json!({ "cloud": p.cloud, "monthly_usd": cap, "spent_usd": v.spend(&p.cloud) })
    })).collect();
    let data = json!({ "aws_account": aws.then(|| json!({ "monthly_usd": month, "daily_usd": day, "spent_usd": v.spend_account("aws"), "spent_today_usd": v.spend_today_account("aws"),
        "aws_budget": routing.aws_budget.as_ref().map(|b| if b.stop { "stops" } else { "emails" }) })), "providers": pools });
    (said, data)
}

/// What was spent on machines today (UTC) and the day's limit, in a line.
fn day_line(spent: f64, cap: Option<f64>) -> String {
    match cap { Some(cap) => format!("Today (UTC): ${spent:.2} of {} allowed.", superci_core::plane::dollars(cap)), None => format!("Today (UTC): ${spent:.2}; no limit for a day is set.") }
}

/// What was spent on machines this month and the limit it counts against, in a line.
fn month_line(whose: &str, spent: f64, cap: Option<f64>) -> String {
    match cap { Some(cap) => format!("{whose} spent ${spent:.2} on machines this month, of {} allowed.", superci_core::plane::dollars(cap)), None => format!("{whose} spent ${spent:.2} on machines this month; no limit is set.") }
}

/// `superci dashboard`, `superci login [cloud]` and `superci connections create github`: the dashboard, or the dashboard opened
/// for the one thing a person does in the browser (it ends when that is done). None: not one of these.
pub fn in_browser(args: &Args) -> Option<Result<()>> {
    let open = !args.has("no-browser");
    if !matches!((args.word(0), args.word(1), args.word(2)), ("dashboard", "", _) | ("login", _, _) | ("connections", "create", "github")) { return None }
    // The dashboard serves every workspace: with several and none named it opens on the list of them.
    if args.word(0) == "dashboard" {
        return Some(match which_workspace(args, false) {
            Ok(Which::One(name)) => { IN.with(|w| *w.borrow_mut() = name); signed_in().serve(open, false).map_err(Fail::from) }
            Ok(Which::Several(_)) => Dashboard::signed_in(None).serve(open, true).map_err(Fail::from),
            Err(e) => Err(e),
        })
    }
    if let Err(e) = enter(args) { return Some(Err(e)) }
    match (args.word(0), args.word(1)) {
        ("login", cloud) => {
            let cloud: Option<&'static str> = match cloud {
                "" => None,
                c => match CLOUDS.iter().find(|k| **k == c) { Some(k) => Some(*k), None => return Some(usage("Say which: `superci login aws`, `cloudflare` or `modal`.")) },
            };
            let mut d = signed_in();
            // `--add`: one more AWS account, or Modal workspace, beside those signed in to (it replaces none).
            let add = args.has("add") && matches!(cloud, Some("aws" | "modal"));
            let there = match cloud { Some(c) => d.signed_in_as().is_some_and(|s| s.to_lowercase().contains(c)), None => d.signed_in_as().is_some() };
            if there && !add {
                println!("SuperCI is signed in on this computer: {}. `superci logout` removes it.{}", d.signed_in_as().unwrap_or_default(), match cloud { Some("aws") => " `superci login aws --add` signs in to another AWS account beside it.", Some("modal") => " `superci login modal --add` signs in to another Modal workspace beside it.", _ => "" });
                return Some(Ok(()))
            }
            d.adding = add;
            Some(d.for_task(Task::Login(cloud)).serve(open, false).map_err(Fail::from))
        }
        // (The rest: `superci connections create github OWNER`.)
        _ => Some((|| {
            let owner = args.word(3);
            if owner.is_empty() { return usage("Say whose repositories: `superci connections create github OWNER` (an organization, or your own GitHub name).") }
            let d = ready()?;
            let plane = in_use(&d)?;
            // GitHub shows the page that creates an organization's App to its owners only: what to send one.
            let note = crate::dashboard::owner_note(owner, &plane.place());
            if args.has("dry-run") { println!("Would open GitHub in the browser, for a person to create the App for {owner} and choose its repositories. Nothing was changed.\nOnly an owner of {owner} can create it. A note to send one:\n\n{note}"); return Ok(()) }
            println!("Only an owner of {owner} can create its App (GitHub shows anyone else a 404). Not an owner? `superci connections create github {owner} --dry-run` prints a note to send one.");
            d.for_task(Task::GitHub { login: owner.to_string(), host: args.get("host").unwrap_or_default().to_string(), started: false, done: false }).serve(open, false).map_err(Fail::from)
        })()),
    }
}

/// What an AWS account allows, in lines: what kind of account it is for SuperCI, its regions, spot and on-demand, and
/// how many vCPUs it may run.
pub fn aws_look_lines(account: &str, who: &str, look: &crate::aws::Look) -> Vec<String> {
    use crate::aws::{Answer, Kind};
    let whose = |a: &Answer| match a {
        Answer::Allowed => "allowed".to_string(),
        Answer::Refused(r) => format!("refused by {}", refused_by(r)),
        Answer::Unknown(e) => format!("not known (AWS answered: {})", e.chars().take(140).collect::<String>()),
    };
    let mut out = vec![format!("AWS account   {account}{}", if who.is_empty() { String::new() } else { format!(" · signed in as {who}") })];
    out.push(format!("Kind          {}", match look.kind() {
        Kind::Open => "open: nothing SuperCI does is refused".to_string(),
        Kind::Unanswered => format!("not known: AWS did not answer ({})", look.unanswered().unwrap_or_default().chars().take(140).collect::<String>()),
        Kind::Project { region, activated: false } => format!("an AWS project (AWS's new sign-up): it works in {region} only, and spot machines are refused"),
        Kind::Project { region, activated: true } => format!("an AWS project (AWS's new sign-up): it works in {region} only, with spot machines"),
        Kind::Restricted => "restricted: some regions or spot machines are refused".to_string(),
    }));
    let allowed = look.allowed_regions();
    let refused: Vec<&(String, Answer)> = look.regions.iter().filter(|(_, a)| !a.allowed()).collect();
    out.push(format!("Regions       {}", if allowed.is_empty() { "none allowed".to_string() } else { format!("allowed: {}", allowed.join(", ")) }));
    // Refusals alike are said once, with their regions.
    let mut said: Vec<(String, Vec<&str>)> = vec![];
    for (region, answer) in refused { let why = whose(answer); match said.iter_mut().find(|(w, _)| *w == why) { Some((_, regions)) => regions.push(region), None => said.push((why, vec![region])) } }
    for (why, regions) in said { out.push(format!("              {why}: {}", regions.join(", "))) }
    let at = look.asked_in.as_deref().map(|r| format!(" (asked in {r})")).unwrap_or_default();
    if let Some(spot) = &look.spot { out.push(format!("Spot          {}{at}", whose(spot))) }
    if let Some(on_demand) = &look.on_demand { out.push(format!("On-demand     {}{at}", whose(on_demand))) }
    let cpus = |n: Option<u32>| n.map(|n| format!("{n} vCPUs")).unwrap_or_else(|| "not known".into());
    if look.asked_in.is_some() { out.push(format!("At once       spot {} · on-demand {}", cpus(look.spot_cpus), cpus(look.on_demand_cpus))) }
    // Few enough to hold jobs back (a new account's): where to ask AWS for more.
    if let Some((cpus, link)) = look.low_limit() { out.push(format!("              {cpus} vCPUs is {} of the default size at a time. Ask AWS for more: {link}", if cpus < 8 { "one job".to_string() } else { format!("{} jobs", cpus / 4) })) }
    let without = look.regions_without_image();
    if !without.is_empty() { out.push(format!("Runner image  not in {}: jobs cannot start there until it is copied in", without.join(", "))) }
    if let Some(org) = &look.organization_id {
        let managed = look.management_account.as_deref().filter(|m| *m != account);
        out.push(format!("Organization  {org}{}", match managed { Some(m) => format!(", managed from account {m}: its policies are not this account's to read or change"), None => ", managed from this account".to_string() }));
    }
    out
}

/// Whose rule a refusal is, in words.
fn refused_by(r: &superci_core::aws::Refusal) -> String {
    use superci_core::aws::By;
    let policy = r.policy_id().map(|p| format!(" {p}")).unwrap_or_default();
    match r.by {
        By::Organization => format!("your AWS organization's policy{policy}{}", r.management_account().map(|a| format!(" (management account {a})")).unwrap_or_default()),
        By::OrganizationResources => format!("your AWS organization's resource policy{policy}"),
        By::Boundary => "the permissions boundary of the role signed in as".to_string(),
        By::Identity => "the permissions of the role signed in as".to_string(),
        By::Session => "this sign-in's session policy".to_string(),
        By::Resource => "the resource's own policy".to_string(),
        By::Unsaid => "AWS, which did not say whose rule it is".to_string(),
    }
}

fn status_lines(v: &Value) -> Vec<String> {
    let mut out = vec![format!("Signed in     {}", v["signed_in"].as_str().unwrap_or_default())];
    if v["workspaces"].as_u64().unwrap_or(0) > 1 { out.insert(0, format!("Workspace     {}", v["workspace"].as_str().unwrap_or_default())) }
    let p = &v["control_plane"];
    if p.is_null() { out.push(format!("Control plane none found in the clouds signed in to. {}", NO_PLANE.trim_start_matches("There is no control plane yet. "))); return out }
    let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
    out.push(format!("Control plane {} · {}{}", s(&p["where"]), p["version"].as_str().unwrap_or("not answering"), p["update_to"].as_str().map(|to| format!(" (update to {to}: `superci planes update`)")).unwrap_or_default()));
    out.push(format!("Address       {}", s(&p["url"])));
    out.push(format!("Jobs can run  {}", if p["ready"] == true { "yes" } else { "not yet: it needs repositories (`superci connections create github OWNER`) and a runner provider (`superci runners create aws`)" }));
    let st = &v["status"];
    if st.is_null() { out.push("Its jobs could not be read just now (it did not answer with this machine's key yet). Try again in a moment.".into()); return out }
    let jobs = st["jobs"].as_array().cloned().unwrap_or_default();
    let count = |states: &[&str]| jobs.iter().filter(|j| states.contains(&j["state"].as_str().unwrap_or_default())).count();
    out.push(format!("Jobs          {} listed · {} running · {} starting · {} waiting", jobs.len(), count(&["running"]), count(&["launching", "launched"]), count(&["waiting"])));
    out
}

/// `superci workspaces …`: the workspaces kept on this machine. Each is read from its own folder; no cloud is asked.
fn workspaces(args: &Args) -> Result<Done> {
    let w = |i: usize| args.word(i);
    let root = store::root().ok_or("SuperCI keeps nothing on this machine (it has no home folder).")?;
    let id = w(2).trim().to_lowercase();
    let known = |id: &str| if store::workspaces_in(&root).iter().any(|n| n == id) { Ok(()) } else { Err(Fail::Failed(format!("There is no workspace with the id {id} on this machine (`superci workspaces list`)."))) };
    match w(1) {
        "list" => {
            let all = store::listed(&root, None);
            // Another one that looks after the same CI (a duplicate).
            let twin = |l: &store::Listed| all.iter().find(|o| o.id != l.id && !l.plane_id.is_empty() && o.plane_id == l.plane_id).map(|o| o.id.clone());
            let line = |l: &store::Listed| {
                let mut parts: Vec<String> = vec![];
                if !l.name.is_empty() { parts.push(l.name.clone()) }
                parts.extend(l.hosts.iter().map(|(kind, name)| format!("{} {name}", if kind == "gitlab" { "GitLab" } else { "GitHub" })));
                if !l.place.is_empty() { parts.push(l.place.clone()) }
                if !l.signed_in { parts.push(if l.place.is_empty() { "nothing set up yet".into() } else { "signed out".into() }) }
                if let Some(twin) = twin(l) { parts.push(format!("same CI as {twin}")) }
                format!("{:<24} {}", l.id, parts.join(" · "))
            };
            let said = if all.is_empty() { vec!["None yet: the first one (default) is made when you sign in (`superci login`).".to_string()] } else { all.iter().map(line).collect() };
            let data: Vec<Value> = all.iter().map(|l| json!({ "id": l.id, "name": l.name, "hosts": l.hosts.iter().map(|(kind, name)| json!({ "kind": kind, "name": name })).collect::<Vec<_>>(), "control_plane": l.place, "plane_id": l.plane_id, "same_as": twin(l), "signed_in": l.signed_in })).collect();
            Ok(Done { said, data: json!({ "workspaces": data }) })
        }
        "create" => {
            // Its name, as typed (several words are one name); its id is made from it.
            let name = args.words[2.min(args.words.len())..].join(" ");
            if name.is_empty() { return usage("Give it a name: `superci workspaces create AcmeCI`.") }
            // (A name that is valid always gives an id.)
            let id = superci_core::spec::valid_name(&name).and_then(|_| store::id_from(&name))?;
            if root.join("workspaces").join(&id).exists() { return Err(format!("There is a workspace with the id {id} already. Give this one another name.").into()) }
            if let Some(done) = dry(args, &format!("make an empty workspace called {name}, with the id {id}"), "workspaces", &[]) { return Ok(done) }
            store::Store::under(&root, &id).set_about(&store::About { name: name.clone(), ..Default::default() })?;
            Ok(Done { said: vec![format!("The workspace {name} is made, with nothing in it yet. Its id is {id}."), format!("Sign in to its clouds: `superci login --workspace={id}`, or open it: `superci dashboard --workspace={id}`.")], data: json!({ "ok": true, "workspace": id, "name": name }) })
        }
        // A CI found in its signed-in clouds, taken as its own (while it has none).
        "update" if args.get("plane").is_some() => {
            if id.is_empty() { return usage("Say which: `superci workspaces update ID --plane=PLANE`.") }
            known(&id)?;
            let plane = args.get("plane").unwrap_or_default().trim().to_string();
            if let Some(done) = dry(args, &format!("have the workspace {id} look after the control plane {plane}"), "workspaces", &[]) { return Ok(done) }
            let mut d = Dashboard::signed_in(Some(store::Store::under(&root, &id)));
            d.all()?;
            let said = d.join(&plane).map_err(|(_, e)| format!("{e}. Its CIs: `superci planes list --workspace={id}`."))?;
            Ok(Done { said: vec![said], data: json!({ "ok": true, "workspace": id, "plane": plane }) })
        }
        "update" => {
            let Some(to) = args.get("id").map(|n| n.trim().to_lowercase()) else { return usage("Say its new id: `superci workspaces update ID --id=NEW` (or the CI it looks after: `--plane=PLANE`).") };
            if id.is_empty() { return usage("Say which: `superci workspaces update ID --id=NEW`.") }
            known(&id)?;
            store::valid_id(&to)?;
            if let Some(done) = dry(args, &format!("give the workspace {id} the id {to}"), "workspaces", &[]) { return Ok(done) }
            store::rename(&root, &id, &to)?;
            Ok(Done { said: vec![format!("The workspace {id} has the id {to} now.")], data: json!({ "ok": true, "workspace": to }) })
        }
        "delete" => {
            if id.is_empty() { return usage("Say which: `superci workspaces delete ID --confirm`.") }
            let would = format!("sign the workspace {id} out and forget it on this machine; nothing in its clouds is deleted");
            confirmed(args, &would)?;
            known(&id)?;
            if let Some(done) = dry(args, &would, "workspaces", &[]) { return Ok(done) }
            let store = store::Store::under(&root, &id);
            // As `superci logout` does there first (its key leaves its control plane, Cloudflare ends its sign-in).
            let mut said = Dashboard::signed_in(Some(store.clone())).logout()?;
            said.retain(|line| line != "Nothing was kept on this machine.");
            store.forget()?;
            said.push(format!("The workspace {id} is forgotten on this machine. Nothing in its clouds was deleted."));
            Ok(Done { data: json!({ "ok": true, "done": said }), said })
        }
        "" => usage("`superci workspaces` takes an operation."),
        operation => usage(format!("`{operation}` is not an operation of `superci workspaces`.")),
    }
}

/// The latest jobs (`run`: that run's only).
fn jobs(run: Option<&str>) -> Result<Done> {
    if run.is_some_and(|r| r.is_empty() || !r.bytes().all(|b| b.is_ascii_digit())) { return usage("--run takes a run's id (they are in `superci runs list`).") }
    let mut d = signed_in();
    let jobs: Vec<Value> = view(&mut d)?.jobs().iter().filter(|j| run.is_none_or(|r| j["run_id"].as_u64().is_some_and(|n| n.to_string() == r))).map(job_words).collect();
    let said = jobs.iter().take(40).map(job_line).collect::<Vec<_>>();
    let none = match run { Some(r) => format!("No jobs of run {r} among the latest this control plane keeps (ids are in `superci runs list`)."), None => "No jobs yet.".into() };
    Ok(Done { said: if said.is_empty() { vec![none] } else { said }, data: json!({ "jobs": jobs }) })
}

/// A job in a line: its id, how it stands, what it is, and (its runner, how long, its cost, why it failed or waits).
/// The control plane's latest jobs by run, each linked to where its code host is.
fn runs(v: &crate::view::PlaneView) -> Vec<Value> {
    let jobs: Vec<Value> = v.jobs().iter().map(|j| with_hosts(v, j)).collect();
    runs_of(&jobs)
}

fn run_line(r: &Value) -> String {
    let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
    let count = r["jobs"].as_u64().unwrap_or_default();
    let tail: Vec<String> = [format!("{count} {}", if count == 1 { "job" } else { "jobs" }), s("took"), r["usd"].as_f64().map(|c| format!("${c:.3}")).unwrap_or_default(), s("last_said")].into_iter().filter(|t| !t.is_empty()).collect();
    format!("{:<12} {:<9} {} · {}  ({})", r["id"].as_u64().unwrap_or_default(), s("state"), s("run"), s("in"), tail.join(" · "))
}

fn job_line(j: &Value) -> String {
    let s = |k: &str| j[k].as_str().unwrap_or_default().to_string();
    let tail: Vec<String> = [s("runner"), s("took"), j["usd"].as_f64().map(|c| format!("${c:.3}")).unwrap_or_default(), s("why"), s("choice")].into_iter().filter(|t| !t.is_empty()).collect();
    // (A job a machine ran to its end is said by how the job itself ended, when that is known: passed, failed.)
    let stands = Some(s("ended")).filter(|e| !e.is_empty()).unwrap_or_else(|| s("state"));
    format!("{:<12} {:<9} {} · {}{}", j["id"].as_u64().unwrap_or_default(), stands, s("job"), s("in"), if tail.is_empty() { String::new() } else { format!("  ({})", tail.join(" · ")) })
}

fn machine_words(m: &Value) -> String {
    let machine: superci_core::spec::Spec = serde_json::from_value(m.clone()).unwrap_or_default();
    // (As a label would say it, for what a command can copy; and in words.)
    let said = machine.describe();
    if machine.pairs().is_empty() { "each provider's standard machine (Linux, x64)".into() } else { format!("{said} ({})", machine.label("").trim_start_matches('/').replace('/', ", ")) }
}

fn runners_of(d: &mut Dashboard) -> Result<Done> {
    let view = view(d)?;
    let st = view.status.clone().unwrap_or_default();
    let pools = view.order();
    let order: Vec<Value> = pools.iter().map(|p| json!({ "cloud": p.cloud, "taken_out": p.off, "max_jobs": p.max_jobs, "monthly_usd": p.monthly_usd,
        "regions": if superci_core::plane::account_of(&p.cloud) == "aws" { json!(view.pool_regions(p)) } else { Value::Null },
        "running": st["active"][&p.cloud].as_u64().unwrap_or(0), "spent_usd": st["spend"][&p.cloud].as_f64().unwrap_or(0.0) })).collect();
    let line = |p: &Value| [p["regions"].as_array().map(|r| format!("in {}", r.iter().filter_map(|r| r.as_str()).collect::<Vec<_>>().join(", "))).unwrap_or_default(),
        p["max_jobs"].as_u64().map(|n| format!("up to {n} jobs at once")).unwrap_or_default(), p["monthly_usd"].as_f64().map(|n| format!("up to ${n} a month")).unwrap_or_default()]
        .into_iter().filter(|w| !w.is_empty()).collect::<Vec<_>>().join("  ");
    let mut said: Vec<String> = order.iter().filter(|p| p["taken_out"] != true).enumerate().map(|(i, p)| format!("{}. {:<14} {}", i + 1, p["cloud"].as_str().unwrap_or_default(), line(p)).trim_end().to_string()).collect();
    let out: Vec<&str> = order.iter().filter(|p| p["taken_out"] == true).filter_map(|p| p["cloud"].as_str()).collect();
    if !out.is_empty() { said.push(format!("Taken out of the order: {} (`superci runners order` with it named puts it back).", out.join(", "))) }
    if said.is_empty() { said.push("No runner provider yet: `superci runners create aws` (or cloudflare, or modal).".into()) }
    let machine = serde_json::to_value(view.machine()).unwrap_or_default();
    // (By the label workflows name here: the first of the control plane's.)
    said.push(format!("`runs-on: {}` gets: {}.", view.label(), machine_words(&machine)));
    Ok(Done { said, data: json!({ "ok": true, "order": order, "machine": machine, "aws_regions": view.aws_regions(), "aws_networks": st["aws_networks"], "cloudflare_location": st["cloudflare_location"], "cloudflare_image": st["cloudflare_image"] }) })
}

/// Where jobs come from: the GitHub accounts connected (with where each one's App is installed) and the GitLabs.
fn connections() -> Result<Done> {
    let view = view(&mut signed_in())?;
    let st = view.status.clone().unwrap_or_default();
    let mut said = vec![];
    for a in st["apps"].as_array().into_iter().flatten() {
        let installed = st["installations"].as_array().into_iter().flatten().filter(|i| i["app"] == a["id"]).map(|i| format!("{} ({} repositories)", i["account"].as_str().unwrap_or_default(), i["repositories"].as_str().unwrap_or("selected"))).collect::<Vec<_>>();
        said.push(format!("github  {}  {}", a["owner"].as_str().unwrap_or_default(), if installed.is_empty() { "its App is not installed yet".to_string() } else { format!("installed on {}", installed.join(", ")) }));
    }
    said.extend(view.gitlabs().into_iter().map(|g| format!("gitlab  {}{}", g.1, if g.0.is_empty() { String::new() } else { format!("  ({})", g.0) })));
    if said.is_empty() { said.push("Nothing is connected: `superci connections create github OWNER`, or `superci connections create gitlab --url=https://gitlab.com` (its token in SUPERCI_GITLAB_TOKEN).".into()) }
    Ok(Done { said, data: json!({ "github": st["apps"], "installations": st["installations"], "gitlab": st["gitlabs"] }) })
}

fn planes() -> Result<Done> {
    let mut d = signed_in();
    let (views, used): (Vec<PlaneView>, usize) = d.all()?;
    // One live: it is the one in use. Several: each is live, and none is "the" one (commands name theirs).
    let several = views.iter().filter(|v| v.serves_its_own()).count() > 1;
    let list: Vec<Value> = views.iter().enumerate().map(|(i, v)| { let mut p = plane_json(v); p["in_use"] = (!several && i == used).into(); p["live"] = v.serves_its_own().into(); p }).collect();
    let mut said: Vec<String> = list.iter().map(|p| format!("{}  {:<44} {}{}", p["id"].as_str().unwrap_or_default(), p["where"].as_str().unwrap_or_default(), p["version"].as_str().unwrap_or("not answering"), if p["in_use"] == true { "  in use" } else if several && p["live"] == true { "  live" } else { "" })).collect();
    // Other CIs in the same clouds: listed apart, never this workspace's (a workspace looks after one CI).
    let found: Vec<Value> = d.found().into_iter().map(|(p, name, owners)| json!({ "id": p.plane_id(), "cloud": p.cloud(), "where": p.place(), "url": p.url(), "name": name, "owners": owners })).collect();
    if said.is_empty() && found.is_empty() { said.push(format!("No control plane in the clouds signed in to. {}", NO_PLANE.trim_start_matches("There is no control plane yet. "))) }
    if !found.is_empty() {
        said.push(if list.is_empty() { "Found in your signed-in clouds (`superci workspaces update ID --plane=PLANE` uses one in a workspace):".to_string() } else { "Other CIs in your signed-in clouds, not this workspace's:".to_string() });
        said.extend(found.iter().map(|f| format!("{}  {:<44} {}", f["id"].as_str().unwrap_or_default(), f["where"].as_str().unwrap_or_default(), f["name"].as_str().unwrap_or("not named"))));
    }
    // A sign-in that is over: said once, beside what the others reach.
    let notes = d.ended_notes();
    said.extend(notes.iter().cloned());
    let mut data = json!({ "control_planes": list, "found": found });
    if !notes.is_empty() { data["notes"] = json!(notes) }
    Ok(Done { said, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logs_source_and_the_age_of_its_last_line_are_said() {
        let now = superci_core::aws::parse_iso_ms("2026-10-16T21:30:46Z").unwrap();
        let got = json!({ "source": "GitHub's, then what its machine sent after it", "last_line_at": "2026-10-16T21:18:46", "running": true });
        assert_eq!(log_said(&got, now), ["(The log is GitHub's, then what its machine sent after it; its last line 21:18:46 UTC, 12 minutes ago; the job is still running (a running job's log comes in blocks, minutes behind at times: `superci jobs retrieve` again for more).)"]);
        assert_eq!(log_said(&json!({ "source": "GitHub's", "last_line_at": "2026-10-16T21:30:16" }), now), ["(The log is GitHub's; its last line 21:30:16 UTC, 30 s ago.)"]);
        assert!(log_said(&json!({}), now).is_empty(), "an older control plane says no source");
    }

    #[test]
    fn each_slots_counts_and_each_test_that_failed_as_expected_are_said() {
        let mut log = String::new();
        for slot in 1..=2 { log.push_str(&format!("[slot {slot}] Running 40 tests using 2 workers\n")); for i in 0..40 { log.push_str(&format!("[slot {slot}]   {}  {} [c] › e2e/s{slot}.spec.ts:{}:1 › t{i} (1s)\n", if slot == 2 && i == 39 { '✘' } else { '✓' }, i + 1, i + 1)) } log.push_str(&format!("[slot {slot}]   {} passed (9s)\n", if slot == 2 { 39 } else { 40 })); if slot == 2 { log.push_str("[slot 2]   1 failed\n") } }
        let t = serde_json::to_value(crate::test_results::read(&log)).unwrap();
        assert_eq!(process_lines(&t, ""), ["  [slot 1] 0 failed, 40 passed", "  [slot 2] 1 failed, 39 passed, 1 failed as expected"]);
        let said = expected_lines(&t, "");
        assert_eq!(said[..2], ["Failed as expected:".to_string(), "  ✓ [c] t39  (e2e/s2.spec.ts:40:1)".to_string()], "named: the list of what ran is too long to show");
        assert!(said[2].contains("Playwright prints no error for these"));
        assert!(tests_lines(&t, "")[1].starts_with("  [slot 1]"), "each slot's counts right after the totals");
    }

    fn args(line: &str) -> Args { Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).unwrap() }

    /// A limit a month is dollars or `none`; what was spent is said against it.
    #[test]
    fn a_monthly_limit_is_dollars_or_none() {
        assert_eq!((dollars("200").unwrap(), dollars("12.5").unwrap(), dollars("none").unwrap()), ("200".to_string(), "12.5".to_string(), String::new()));
        for bad in ["0", "-5", "lots", "", "inf"] { assert!(dollars(bad).is_err(), "{bad}") }
        assert_eq!(month_line("The CI", 14.224, Some(100.0)), "The CI spent $14.22 on machines this month, of $100 allowed.");
        assert_eq!(month_line("The AWS account", 0.0, None), "The AWS account spent $0.00 on machines this month; no limit is set.");
    }

    #[test]
    fn a_name_that_is_none_of_the_control_planes_here_stops_the_command() {
        let (id, url, place) = ("aaaaaaaaaaaa", "https://a.lambda-url.us-east-1.on.aws", "AWS · 111111111111 · us-east-1");
        // Nothing named: as before.
        name_plane(&args("status"));
        assert!(named_is(id, url, place).is_ok());
        // Named by its id or by its address: the one in use passes, another stops with what was named and where.
        name_plane(&args("runners update aws --max-jobs=5 --plane=aaaaaaaaaaaa"));
        assert!(named_is(id, url, place).is_ok());
        name_plane(&args("status --plane=https://a.lambda-url.us-east-1.on.aws/"));
        assert!(named_is(id, url, place).is_ok());
        name_plane(&args("runners update aws --max-jobs=5 --plane=bbbbbbbbbbbb"));
        let Err(Fail::Failed(said)) = named_is(id, url, place) else { panic!("another control plane named") };
        assert_eq!(said, "--plane names the control plane bbbbbbbbbbbb, which is not among the control planes here (this would have acted on aaaaaaaaaaaa, AWS · 111111111111 · us-east-1). Nothing was done. `superci planes list` lists them.");
        name_plane(&args("status"));
    }

    #[test]
    fn words_and_flags() {
        let a = args("runners update aws --max-jobs 5 --monthly-usd=200 --networks a --networks b --confirm");
        assert_eq!(a.words, ["runners", "update", "aws"]);
        assert!(a.has("confirm") && !a.has("json") && a.get("max-jobs") == Some("5") && a.get("monthly-usd") == Some("200") && a.all("networks") == ["a", "b"]);
        let parse = |line: &str| Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).err();
        assert_eq!(parse("status --jsno").unwrap(), Fail::Usage("--jsno is not a flag of superci".into()));
        assert_eq!(parse("planes create aws --region").unwrap(), Fail::Usage("--region needs a value".into()));
        assert!(args("-h").has("help") && args("-V").has("version"));
        assert_eq!(truth(&args("runners update aws --enabled=false"), "enabled"), Ok(Some(false)));
        assert_eq!(truth(&args("runners update aws"), "enabled"), Ok(None));
        assert!(matches!(truth(&args("runners update aws --enabled off"), "enabled"), Err(Fail::Usage(e)) if e == "--enabled is true or false, not off."));
    }

    #[test]
    fn help_for_all_and_for_each() {
        let all = help("");
        // Every resource's commands are in the list of all, and each has help of its own.
        for (word, usage, _) in HELP {
            assert!(all.contains(usage), "{word} is in the list");
            let one = help(word);
            assert!(one.starts_with(usage) && one.contains("--json") && !one.contains("Runners:"), "{word} has its own help");
        }
        assert!(all.starts_with("SuperCI —") && all.contains("superci <resource> <operation> [id] [--param=value]") && all.contains("superci help RESOURCE") && all.contains("SUPERCI_HOME") && all.contains("--workspace=ID") && help("workspaces").contains("SUPERCI_WORKSPACE=ID"));
        assert!(help("runners").contains("aws-on-demand") && help("connections").contains("SUPERCI_GITLAB_TOKEN") && help("connections").contains("GitHub offers that nowhere else"));
        // Every command `run` and `in_browser` know is in some resource's lines.
        for command in ["superci dashboard", "superci login", "superci logout", "superci status", "superci jobs list", "superci jobs retrieve ID", "superci runs list", "superci runs retrieve ID", "superci keys list", "superci keys create NAME", "superci keys delete NAME",
            "superci runners list", "superci runners create", "superci runners update CLOUD", "superci runners order", "superci runners delete CLOUD", "superci labels list", "superci labels create NAME", "superci labels delete NAME", "superci machine retrieve", "superci machine update", "superci limits retrieve", "superci limits update",
            "superci planes list", "superci planes create", "superci planes update", "superci planes move ID", "superci planes allow", "superci planes delete ID", "superci leave --confirm", "superci connections list", "superci connections create github OWNER", 
            "superci deliveries list", "superci deliveries retrieve ID", "superci connections create gitlab", "superci connections delete github OWNER", "superci connections delete gitlab", "superci projects list", "superci projects update", "superci public_repos list", "superci public_repos create", "superci public_repos delete",
            "superci runner_images list", "superci runner_images create NAME", "superci runner_images update NAME", "superci runner_images delete NAME", "superci accounts list", "superci accounts retrieve aws", "superci deliveries list", "superci projects update PROJECT_ID",
            "superci workspaces list", "superci workspaces create NAME", "superci workspaces update ID --id=NEW", "superci workspaces delete ID --confirm"] {
            assert!(all.contains(command), "{command}");
        }
    }

    #[test]
    fn what_a_person_is_needed_for() {
        assert_eq!(needs(NOT_SIGNED_IN), Some("superci login"));
        assert_eq!(needs(AWS_ENDED), Some("superci login aws"));
        assert_eq!(needs("Sign in with AWS first."), Some("superci login aws"));
        assert_eq!(needs("Sign in with Cloudflare first"), Some("superci login cloudflare"));
        assert_eq!(needs("the Cloudflare sign-in expired: sign in again"), Some("superci login cloudflare"));
        assert_eq!(needs("Sign in with Modal first: Its runners come along with the move."), Some("superci login modal"));
        assert_eq!(needs("3 jobs are running on AWS: Remove it when they finish, or stop them as you remove it."), None);
    }

    /// With SuperCI signed in nowhere, every command says so and asks for the sign-in; one that deletes asks for
    /// --confirm before anything else; half a command is said to be half, and what is none, none.
    #[test]
    fn nothing_runs_unasked_or_signed_out() {
        let dir = std::env::temp_dir().join(format!("superci-commands-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        std::env::set_var("SUPERCI_HOME", &dir);
        for name in ["SUPERCI_GITLAB_TOKEN", "SUPERCI_KEY", "SUPERCI_PLANE"] { std::env::remove_var(name) }
        let run = |line: &str| run(&args(line), &mut |_| {}).map(|d| d.said).unwrap_err();
        let signed_out = Fail::Failed(NOT_SIGNED_IN.into());
        for line in ["status", "jobs list", "jobs retrieve 7", "runs list", "runs retrieve 7", "deliveries list", "deliveries retrieve 7", "keys list", "keys create agent", "keys delete agent", "runners list", "labels list", "labels create acmeci", "name retrieve", "name update AcmeCI", "connections list", "planes list", "public_repos list", "machine retrieve", "machine update --cpu=8",
            "planes update", "planes create modal", "planes create aws --region=us-east-1", "runners create aws", "runners order aws", "runners update aws --max-jobs=3", "limits retrieve", "limits update --max-cpu=8", "limits update --max-hours 12",
            "public_repos create acme/site", "public_repos delete acme/site", "projects list", "projects update 7 --enabled=true", "projects update --all --enabled=true", "planes allow", "planes move abcdef123456",
            "planes update --dry-run", "runners create cloudflare --dry-run", "runner_images list", "runner_images create rust-ci --reference=ghcr.io/acme/ci:1", "runner_images update rust-ci --reference=ghcr.io/acme/ci:2", "runner_images update rust-ci --modal-secret=", "runner_images delete rust-ci"] {
            assert_eq!(run(line), signed_out, "{line}");
        }
        for line in ["planes delete abcdef123456", "leave", "runners delete aws", "labels delete superci", "connections delete github acme", "connections delete gitlab"] {
            assert!(matches!(run(line), Fail::Unconfirmed(e) if e.starts_with("This would ") && e.ends_with("Add --confirm to do it, or --dry-run to check it first.")), "{line}");
            assert_eq!(run(&format!("{line} --confirm")), signed_out, "{line}");
            assert_eq!(run(&format!("{line} --dry-run")), signed_out, "{line}: a dry run checks the sign-in too");
        }
        for (line, says) in [("runners create gcp", "Say which: `superci runners create aws`"), ("planes create", "Say where: `superci planes create aws"), ("planes create aws", "Say where: --region"), ("planes move", "Say where to:"), ("planes delete", "Say which:"),
            ("runners order", "Say the order:"), ("public_repos create", "Say which:"), ("runner_images create rust-ci", "Say which and where:"), ("runner_images create --reference=ghcr.io/acme/ci:1", "Say which and where:"), ("runner_images delete", "Say which:"), ("runner_images update rust-ci", "Say which and what:"), ("runner_images update --reference=ghcr.io/acme/ci:2", "Say which and what:"),
            ("runner_images update rust-ci --reference=https://ghcr.io/acme/ci", "Check the image's address:"), ("runner_images update rust-ci --modal-secret=a!secret", "Check the Modal Secret's name:"), ("connections create gitlab", "Give the GitLab token by name: SUPERCI_GITLAB_TOKEN"), ("connections create gitlab --token-env MY_TOKEN", "Give the GitLab token by name: MY_TOKEN"),
            ("jobs retrieve", "Say which: `superci jobs retrieve ID`"), ("runs retrieve", "Say which: `superci runs retrieve ID`"), ("jobs retrieve seven", "Say which: `superci jobs retrieve ID`"), ("keys create", "Give it a name:"), ("keys delete", "Say which:"), ("name update", "Say what to call it:"), ("keys create agent --days soon", "--days is a number"),
            ("projects update 7", "Say which way:"), ("projects update --all --enabled=false", "Projects are switched off one at a time"),
            ("planes", "`superci planes` takes an operation."), ("limits", "`superci limits` takes an operation."), ("runners frob", "`frob` is not an operation of `superci runners`."), ("limits set", "`set` is not an operation of `superci limits`."),
            ("aws", "`aws` is not a command of superci."), ("accounts", "`superci accounts` takes an operation."), ("accounts frob", "`frob` is not an operation of `superci accounts`."), ("accounts retrieve", "Say which cloud's:"), ("accounts retrieve modal", "Say which cloud's:"),
            ("deliveries", "`superci deliveries` takes an operation."), ("projects frob", "`frob` is not an operation of `superci projects`."),
            ("connections create", "Say which: `superci connections create github OWNER`"), ("connections create bitbucket x", "Say which: `superci connections create github OWNER`"), ("connections delete", "Say which: `superci connections delete github OWNER --confirm`"), ("connections delete github", "Say which: `superci connections delete github OWNER --confirm`."),
            ("github list", "`github` is not a command of superci."), ("gitlab create", "`gitlab` is not a command of superci."), ("github_deliveries list", "`github_deliveries` is not a command of superci."), ("frobnicate", "`frobnicate` is not a command of superci.")] {
            assert!(matches!(run(line), Fail::Usage(e) if e.starts_with(says)), "{line}");
        }
        assert_eq!(super::run(&args("logout"), &mut |_| {}).unwrap().said, ["Nothing was kept on this machine."]);
        // A cloud is what an operation takes, never a command of its own.
        assert_eq!(super::run(&args("accounts list"), &mut |_| {}).unwrap().said, [NOT_SIGNED_IN]);
        assert_eq!(run("accounts retrieve aws"), Fail::Failed("Sign in with AWS first.".into()));
        assert_eq!(needs("Sign in with AWS first."), Some("superci login aws"));
        // What opens the browser asks for the same first.
        assert!(matches!(in_browser(&args("login gcp")), Some(Err(Fail::Usage(_)))) && matches!(in_browser(&args("connections create github")), Some(Err(Fail::Usage(_)))));
        assert_eq!(in_browser(&args("connections create github acme")).unwrap().unwrap_err(), signed_out);
        assert!(in_browser(&args("status")).is_none() && in_browser(&args("connections list")).is_none() && in_browser(&args("connections create gitlab")).is_none());
        // A key that only reads, given by name, is no sign-in: a change says so, and names what is needed.
        std::env::set_var("SUPERCI_PLANE", "https://plane.example");
        std::env::set_var("SUPERCI_KEY", "superci_read_x");
        assert_eq!(run("runners create aws"), Fail::Failed(ONLY_READS.into()));
        assert_eq!(needs(ONLY_READS), Some("superci login"));
        std::env::set_var("SUPERCI_PLANE", "plane.example");
        assert!(matches!(run("status"), Fail::Usage(e) if e.starts_with("SUPERCI_PLANE is the control plane's address")));
        for name in ["SUPERCI_KEY", "SUPERCI_PLANE"] { std::env::remove_var(name) }
        // Workspaces. None yet: the first one is there without being made, and another name is none.
        std::env::remove_var("SUPERCI_WORKSPACE");
        let ok = |line: &str| super::run(&args(line), &mut |_| {}).unwrap().said;
        assert_eq!(ok("workspaces list"), ["None yet: the first one (default) is made when you sign in (`superci login`)."]);
        assert_eq!(run("status --workspace=acme"), Fail::Failed("--workspace names the workspace acme, and none is called that on this machine (the only one is default). Nothing was done. `superci workspaces list` lists them.".into()));
        assert_eq!(run("status --workspace=default"), signed_out);
        // One: nothing needs naming.
        assert_eq!(ok("workspaces create Acme")[0], "The workspace Acme is made, with nothing in it yet. Its id is acme.");
        assert_eq!(run("status"), signed_out);
        assert_eq!(run("workspaces create acme"), Fail::Failed("There is a workspace with the id acme already. Give this one another name.".into()));
        // Two: a command says which, or stops and lists them. Nothing picks one.
        assert_eq!(ok("workspaces create Globex CI")[0], "The workspace Globex CI is made, with nothing in it yet. Its id is globex-ci.");
        ok("workspaces update globex-ci --id=globex");
        let which = Fail::Failed("2 workspaces are kept on this machine: acme, globex. Say which: --workspace=ID (or SUPERCI_WORKSPACE=ID). Nothing was done.".into());
        for line in ["status", "jobs list", "planes list", "runners update aws --max-jobs=3", "logout", "planes delete abcdef123456 --confirm", "name update AcmeCI"] { assert_eq!(run(line), which, "{line}") }
        assert_eq!(in_browser(&args("login aws")).unwrap().unwrap_err(), which);
        assert_eq!(in_browser(&args("connections create github acme")).unwrap().unwrap_err(), which);
        assert_eq!(run("status --workspace=globex"), signed_out);
        std::env::set_var("SUPERCI_WORKSPACE", "acme");
        assert_eq!(run("status"), signed_out);
        // What stops everything takes its workspace from the command itself, never from the terminal's variable.
        assert_eq!(run("leave --confirm"), Fail::Failed("2 workspaces are kept on this machine: acme, globex. Say which: --workspace=ID (on this command itself). Nothing was done.".into()));
        assert_eq!(run("leave --confirm --workspace=acme"), signed_out);
        // A name that is none of them stops the command: it never falls through to another.
        std::env::set_var("SUPERCI_WORKSPACE", "initech");
        assert_eq!(run("status"), Fail::Failed("SUPERCI_WORKSPACE names the workspace initech, and none is called that on this machine (here: acme, globex). Nothing was done. `superci workspaces list` lists them.".into()));
        assert_eq!(run("status --workspace=acme"), signed_out, "the flag comes before the variable");
        std::env::remove_var("SUPERCI_WORKSPACE");
        // The list, a new name, and forgetting one (which needs --confirm and a name that is one of them).
        assert_eq!(ok("workspaces list"), [format!("{:<24} Acme · nothing set up yet", "acme"), format!("{:<24} Globex CI · nothing set up yet", "globex")]);
        assert_eq!(ok("workspaces update globex --id=initech"), ["The workspace globex has the id initech now."]);
        assert_eq!(run("workspaces update acme --id=initech"), Fail::Failed("There is a workspace with the id initech already.".into()));
        assert!(matches!(run("workspaces delete initech"), Fail::Unconfirmed(e) if e.starts_with("This would sign the workspace initech out and forget it on this machine")));
        assert_eq!(run("workspaces delete nope --confirm"), Fail::Failed("There is no workspace with the id nope on this machine (`superci workspaces list`).".into()));
        assert_eq!(ok("workspaces delete initech --dry-run"), ["Would sign the workspace initech out and forget it on this machine; nothing in its clouds is deleted. Nothing was changed."]);
        assert_eq!(ok("workspaces delete initech --confirm"), ["The workspace initech is forgotten on this machine. Nothing in its clouds was deleted."]);
        assert_eq!(run("status"), signed_out, "one is left: nothing needs naming again");
        for (line, says) in [("workspaces", "`superci workspaces` takes an operation."), ("workspaces create", "Give it a name:"), ("workspaces create 9", "A name is 2 to 24 letters"), ("workspaces update acme", "Say its new id:"), ("workspaces delete", "Say which:"), ("workspaces frob", "`frob` is not an operation of `superci workspaces`.")] {
            assert!(matches!(run(line), Fail::Usage(e) | Fail::Failed(e) if e.starts_with(says)), "{line}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whose_rule_a_refusal_is_said_for_each_kind_of_policy() {
        use superci_core::aws::{By, Refusal};
        let r = |by: By, policy: &str| refused_by(&Refusal { action: "ec2:RunInstances".into(), by, policy: policy.into() });
        assert_eq!(r(By::Organization, "arn:aws:organizations::222222222222:policy/o-1/service_control_policy/p-abc"), "your AWS organization's policy p-abc (management account 222222222222)");
        assert_eq!(r(By::Organization, ""), "your AWS organization's policy");
        assert_eq!(r(By::OrganizationResources, "arn:aws:organizations::222222222222:policy/o-1/resource_control_policy/p-rcp"), "your AWS organization's resource policy p-rcp");
        assert_eq!(r(By::Boundary, ""), "the permissions boundary of the role signed in as");
        assert_eq!(r(By::Identity, ""), "the permissions of the role signed in as");
        assert_eq!(r(By::Session, ""), "this sign-in's session policy");
        assert_eq!(r(By::Resource, ""), "the resource's own policy");
        assert_eq!(r(By::Unsaid, ""), "AWS, which did not say whose rule it is");
    }

    #[test]
    fn the_tests_that_ran_are_listed_with_each_outcome_and_none_or_too_many_are_not() {
        let t = json!({ "ran": [{ "name": "a", "outcome": "passed", "file": "a.spec.ts" }, { "name": "b", "outcome": "failed" }, { "name": "c", "outcome": "skipped" }, { "name": "d", "outcome": "flaky" },
            { "name": "e", "outcome": "failed as expected" }, { "name": "f", "outcome": "interrupted" }], "failed_as_expected_tests": [{ "name": "e" }] });
        assert_eq!(ran_lines(&t, ""), ["What ran:", "  ✓ a  (a.spec.ts)", "  ✗ b", "  – c", "  ✓ (flaky) d", "  ✓ (failed as expected) e", "  · f"]);
        assert!(ran_lines(&json!({ "ran": [] }), "").is_empty() && ran_lines(&json!({}), "").is_empty());
        assert!(ran_lines(&json!({ "ran": vec![json!({ "name": "x" }); 31] }), "").is_empty(), "too many to read: the counts say it");
        // Failed as expected, among few that ran: not named again, only what it means.
        let said = expected_lines(&t, "");
        assert!(said.len() == 1 && said[0].starts_with("(Marked to fail"), "{said:?}");
        assert!(expected_lines(&json!({}), "").is_empty());
        // A log that could not be read, and one with no runner's results in it.
        assert_eq!(tests_lines(&json!({ "error": "gone" }), "  "), ["  Its log could not be read: gone"]);
        let none = tests_lines(&json!({ "runners": [], "notes": ["a note"], "note": "the job failed", "artifacts": [{ "name": "report", "fetch": "superci artifacts retrieve report --run=1" }], "artifacts_at": "https://github.com/acme/app/actions/runs/1#artifacts" }), "");
        assert_eq!(none, ["No test results in its log (none of Vitest, Jest, Playwright, pytest, cargo or Go said any).", "a note", "the job failed", "Artifact report: `superci artifacts retrieve report --run=1`", "Its artifacts: https://github.com/acme/app/actions/runs/1#artifacts"]);
        // Each process's counts, with what each had beside passing and failing.
        assert_eq!(process_lines(&json!({ "by_process": [{ "mark": "[slot 1]", "failed": 0, "passed": 2, "skipped": 1, "flaky": 1 }] }), ""), ["  [slot 1] 0 failed, 2 passed, 1 skipped, 1 flaky"]);
        assert_eq!(totals_line(&json!({ "failed": 1, "passed": 3, "skipped": 2, "flaky": 1, "failed_as_expected": 1, "runners": ["playwright"] })), "Tests: 1 failed, 3 passed, 2 skipped (playwright), of the passed 1 failed as expected, 1 flaky");
    }

    #[test]
    fn nothing_typed_is_no_words_and_no_flags() {
        let a = Args::parse(&[]).unwrap();
        assert!(a.words.is_empty() && !a.has("json") && a.word(0).is_empty());
        assert_eq!(truth(&args("projects update 1 --enabled=true"), "enabled"), Ok(Some(true)));
    }

    #[test]
    fn a_dry_run_says_what_it_would_do() {
        let a = args("connections create gitlab --dry-run");
        let fields = [field("url", "https://gitlab.com"), field("token", "glpat-secret")];
        let done = dry(&a, "connect https://gitlab.com with the token in SUPERCI_GITLAB_TOKEN", "/gitlab/connect", &fields).unwrap();
        assert_eq!(done.said, ["Would connect https://gitlab.com with the token in SUPERCI_GITLAB_TOKEN. Nothing was changed."]);
        assert!(done.data["dry_run"] == true && done.data["form"]["to"] == "/gitlab/connect" && done.data["form"]["fields"]["token"] == "(given)" && !done.data.to_string().contains("glpat-secret"));
        assert!(dry(&args("connections create gitlab"), "x", "/gitlab/connect", &fields).is_none());
    }
}
