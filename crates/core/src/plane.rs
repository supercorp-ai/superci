//! A control plane: the part that lives in the user's own cloud account (Cloudflare, AWS or Modal). It is headless: setup
//! and changes happen in the dashboard on the user's machine, which gives the control plane its GitHub App credentials and
//! AWS connect token as the runtime's encrypted secrets. Online, a control plane answers only:
//! - `POST /webhook`: the App's `workflow_job` events (signed with the App's webhook secret);
//! - `GET /.well-known/openid-configuration`, `/.well-known/jwks.json`: its OpenID Connect identity (public keys only),
//!   which the connected AWS account trusts;
//! - `POST /aws/callback`: the dashboard reporting the AWS role it made, with a one-time token (closes once used);
//! - `GET /health`: whether GitHub and AWS are connected (yes/no only), for the setup page.
//! Each job labelled for it runs on a spot machine in the connected AWS account.
use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::aws::{self, Credentials, Launch};
use crate::crypto::{safe_eq, sha256_hex, verify_hub_signature, SigningKeyStore};
use crate::github::{self, App, Owner};
use crate::gitlab::{self, GitLab};
use crate::io::{Containers, get_json, put_json, Clock, Http, Request, Response, Result, Store, Timer, Work};
use crate::spec::{aws_instance_types, Capacity, Size, Spec};

pub const AUDIENCE: &str = "superci";
const MAX_JOB_MINUTES: u32 = 70;

/// What the runtime gives a control plane: its id and label, and (once set up on the user's machine) the App and AWS connect
/// token from its secrets.
pub struct Config {
    pub plane_id: String,
    pub label: String,
    pub app: Option<App>,
    /// GitHub Apps for further organizations (a private App belongs to one account, so each has its own); `app` is
    /// the first one made.
    pub more_apps: Vec<App>,
    /// GitLab, when connected from the dashboard (jobs from its projects' webhooks).
    pub gitlab: Option<GitLab>,
    /// Further GitLab connections (another GitLab, or another account's token); `gitlab` is the first one made.
    pub more_gitlabs: Vec<GitLab>,
    pub aws_region: Option<String>,
    pub aws_connect_token: Option<String>,
    /// The AWS regions machines go to, in order, set from the dashboard: the next one when a region has no spot
    /// capacity (or quota) left. None set: the connection's region.
    pub aws_regions: Vec<String>,
    /// Networks of the account's own, by region, set from the dashboard: machines there start in these subnets and
    /// security groups, not in the control plane's own network.
    pub aws_networks: HashMap<String, aws::GivenNetwork>,
    /// Where Cloudflare's containers start: a Durable Object location hint ("enam", "weur"…), or "auto" (Cloudflare
    /// chooses). Set from the dashboard; eastern North America (near GitHub and GitLab) when unset.
    pub cloudflare_location: String,
    /// Where GitHub's full image is published for Cloudflare's containers (see `docker::start`); none: the
    /// container's own small image.
    pub cloudflare_image: Option<String>,
    /// Set by each dashboard session on your machine (through the cloud's API): keys to read `/status`, each with
    /// the time it expires (ms).
    pub dashboard_keys: Vec<(u64, String)>,
    /// Keys that only read (`superci keys create`, for a coding agent or a script that should look and not change):
    /// each with the time it expires (ms), its name, and the SHA-256 of the key (the key itself is not kept here).
    /// Set through the cloud's API like the dashboard's, as READ_KEY_<expires, unix seconds>_<NAME>.
    pub read_keys: Vec<(u64, String, String)>,
    /// A one-time token for moving to another control plane, set by the dashboard through the cloud's API (so only
    /// someone who controls this cloud account can ask): the old one hands over its settings with it, the new one
    /// takes its history and GitLab's project runners with it.
    pub move_token: Option<String>,
    /// Runner agents in other clouds (Cloudflare containers, Modal sandboxes), set from the dashboard.
    pub agents: Vec<Agent>,
    /// Where jobs run, set from the dashboard (workflows only say `runs-on: <label>`).
    pub routing: Routing,
    /// Machines started by this control plane itself: containers (a Cloudflare control plane with its container
    /// application) or sandboxes (a Modal control plane), in `own_cloud`.
    pub containers: bool,
    pub own_cloud: String,
    /// When the control plane runs in AWS: its own account and the region for machines, and the runtime's credentials
    /// (no role to assume, no OpenID Connect).
    pub aws_own: Option<(String, String)>,
    /// A control plane in AWS whose AWS runners were removed (`AWS_RUNNERS=off`): it starts no machines there.
    pub aws_runners_off: bool,
    pub aws_own_creds: Option<Credentials>,
    pub image_owner: String,
    pub image_name: String,
    pub instance_types: Vec<String>,
    /// The default machine (`runs-on: superci`), set from the dashboard; a job's label may ask for another.
    pub machine: Spec,
}

/// This control plane's version (SuperCI's): the dashboard offers an update when it is older than the dashboard.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

impl Config {
    pub fn new(plane_id: String) -> Self {
        Config {
            plane_id,
            label: "superci".into(),
            own_cloud: "cloudflare".into(),
            app: None,
            gitlab: None,
            more_gitlabs: Vec::new(),
            more_apps: Vec::new(),
            aws_region: None,
            aws_connect_token: None,
            aws_regions: Vec::new(),
            aws_networks: HashMap::new(),
            cloudflare_location: "enam".into(),
            cloudflare_image: None,
            dashboard_keys: Vec::new(),
            read_keys: Vec::new(),
            move_token: None,
            agents: Vec::new(),
            routing: Routing::default(),
            containers: false,
            aws_own: None,
            aws_runners_off: false,
            aws_own_creds: None,
            // RunsOn's public GitHub-compatible Ubuntu 24.04 image: its snapshot is warm, so machines start in seconds.
            image_owner: "135269210855".into(),
            image_name: "runs-on-v2.2-ubuntu24-full-x64-*".into(),
            instance_types: vec!["c8a.xlarge".into(), "c7a.xlarge".into(), "m7a.xlarge".into()],
            machine: Spec::default(),
        }
    }

    pub fn owner(&self) -> Option<Owner> {
        self.app.as_ref().map(|a| Owner { org: a.owner_is_org, login: a.owner.clone() })
    }

    /// Every GitHub App of this control plane: the first, then one for each further organization.
    pub fn apps(&self) -> impl Iterator<Item = &App> { self.app.iter().chain(self.more_apps.iter()) }

    /// Every GitLab connection of this control plane: the first, then each further one.
    pub fn gitlabs(&self) -> impl Iterator<Item = &GitLab> { self.gitlab.iter().chain(self.more_gitlabs.iter()) }
}

/// What a runtime keeps between requests while it is alive (credentials, tokens, the image id).
#[derive(Default)]
pub struct Cache {
    installed: RefCell<Option<(bool, u64)>>,
    creds: RefCell<Option<Credentials>>,
    tokens: RefCell<HashMap<u64, (String, u64)>>,
    /// The machine image per region and architecture, and until when it is taken as current.
    image: RefCell<HashMap<String, (String, u64)>>,
    /// What the GitHub App asks for and the GitLab token's scopes, and until when they are taken as current.
    app_permissions: RefCell<HashMap<u64, (serde_json::Value, u64)>>,
    gitlab_scopes: RefCell<HashMap<String, (Vec<String>, u64)>>,
    /// The public repositories the App is installed on, and until when they are taken as current.
    public_repos: RefCell<Option<(Vec<String>, u64)>>,
    /// The control plane's own network per AWS region (None: not made there), and until when it is taken as current.
    network: RefCell<HashMap<String, (Option<aws::Network>, u64)>>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Identity { signing: SigningKeyStore }

#[derive(Serialize, Deserialize, Clone)]
pub struct AwsConnection { pub account_id: String, pub region: String, pub role_arn: String, pub connected: bool, pub error: Option<String> }

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Job {
    pub job_id: u64, pub run_id: u64, pub repo: String, pub state: String, pub at_ms: u64,
    pub runner: Option<String>, pub runner_id: Option<u64>, pub installation_id: u64,
    /// Where it ran ("aws", "cloudflare", "modal"), the machine's id there, and its kind.
    pub cloud: String, pub machine_id: Option<String>, pub machine_type: Option<String>,
    pub error: Option<String>, pub seen_in_progress: bool,
    /// The label it asked for (`superci`, `superci-8cpu-arm64`): what machine, and what its runner registers as.
    #[serde(default)] pub label: String,
    /// What its machine costs per hour (estimated from published prices; AWS: the spot price at launch), and when
    /// the job started running and when its machine ended (ms). The machine is paid from launch (`at_ms`) to end.
    #[serde(default)] pub usd_per_hour: Option<f64>,
    /// Its price while its runner waits for the job, when lower (Cloudflare bills CPU only as it is used).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub waiting_usd_per_hour: Option<f64>,
    #[serde(default)] pub started_ms: Option<u64>,
    #[serde(default)] pub ended_ms: Option<u64>,
    /// When its machine was last asked for (ms), and how many times it has been placed again because one never began it.
    #[serde(default)] pub launched_ms: Option<u64>,
    #[serde(default)] pub retries: u32,
    /// Its machine's CPUs (what GitHub's runner of that size would have cost is compared by it).
    #[serde(default)] pub cpu: Option<u32>,
    /// Where it comes from: "gitlab", or GitHub (empty). A GitLab job's project (`repo` is its path) and tags.
    #[serde(default, skip_serializing_if = "String::is_empty")] pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub project_id: Option<u64>,
    /// Which GitLab connection it came through (see `GitLab::id`); nothing: the first.
    #[serde(default, skip_serializing_if = "String::is_empty")] pub gitlab: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub tags: Vec<String>,
    /// Its name and its workflow's (GitLab: its stage), for people to tell jobs apart.
    #[serde(default, skip_serializing_if = "String::is_empty")] pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")] pub workflow: String,
    /// Added to its runner's name when its first runner went on to run another job (a new one cannot take its name).
    #[serde(default, skip_serializing_if = "String::is_empty")] pub runner_suffix: String,
    /// The AWS region its machine is in (one of the connection's regions), its zone there, and its disk (GB).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub disk_gb: Option<u32>,
    /// Its machine has no public address (a private subnet of the account's own: none is billed).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub private: bool,
    /// What its machine cost, settled once it ended: at the prices the cloud billed for its time ("prices"), or as the
    /// cloud measured it ("measured"). Until then, `usd_per_hour` over its time is the estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cost_from: Option<String>,
    /// What its AWS machine sent (GB, CloudWatch's count, read a little after it ended; -1: AWS would not say); its
    /// cost is in `cost_usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub sent_gb: Option<f64>,
    /// Nothing here could run it: a runner was started that failed it at once on GitHub, saying why (`fail_fast`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub failed_fast: bool,
    /// Its job ended on another job's runner while its own machine had not begun anything: the machine is kept a
    /// while for that other job (still queued, its runner taken), and no new one is started for this one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub over: bool,
    /// The job its machine is for now, when its own job went to another job's runner: that other job, still queued
    /// with its runner taken. If this machine never comes up, a new one is started while that job still waits.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub for_job: Option<u64>,
    /// Why it failed, while a failing runner (see `fail_fast`) is still to be started for it.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub fail_pending: Option<String>,
    /// The GitHub App its event came through (one per organization); none: the first.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub app_id: Option<u64>,
    /// Its AWS machine is an on-demand one (the place `AWS_ON_DEMAND`): as its label asks, or as the order has it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub on_demand: bool,
    /// Not on a spot machine this time, if another place in the order can run it: run again after AWS took its spot
    /// machine back, or a try that ran out of time with every region refusing a spot machine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub no_spot: bool,
    /// The places where starting its machine failed: its next try starts with the others (cleared once one starts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub failed_at: Vec<String>,
    /// What each region said when it had no spot machine for it, and the job went down the order (or waits).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub spot_refused: Option<String>,
    /// What its spot machine says its notice with (see `aws::runner_user_data_for`), and whether AWS took it back.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub notice: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub interrupted: bool,
    /// The job its interrupted machine was running, to be run again once its run has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub rerun: Option<Rerun>,
}

/// Where a job is kept: GitLab's job ids have their own space.
/// A job to run again after a spot interruption: GitHub's job (the one the machine was running, which may be another
/// with the same label), and how that stands ("pending", "asked", or why not).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Rerun { pub job_id: u64, pub run_id: u64, pub repo: String, pub name: String, pub state: String, pub at_ms: u64 }

/// How long one request may go on trying AWS's regions for a machine before the rest is left for the job's next try.
const LAUNCH_BUDGET_MS: u64 = 15_000;

/// Spot is paused for half an hour once AWS took back two machines within a quarter of an hour.
const SPOT_PAUSE: (usize, u64, u64) = (2, 15 * 60_000, 30 * 60_000);

/// Where a job to be run again off spot machines is noted until it arrives: by its repository, run and name (GitHub gives the
/// new attempt's job a new id).
fn rerun_key(repo: &str, run_id: u64, name: &str) -> String { format!("rerun:{repo}:{run_id}:{name}") }

/// How a failing runner's name ends (see `fail_fast`); a job's own runner's never does.
const FAILING: &str = "-x";
fn failing_runner(name: &str) -> bool { name.ends_with(FAILING) }

/// Where the time of a GitLab connection's last event is kept.
fn last_event_key(connection: &str) -> String { if connection.is_empty() { "gitlab:last_event".into() } else { format!("gitlab:last_event:{connection}") } }

/// An image address as the dashboard may set it: https, and nothing a shell would read as more than a word.
pub fn image_address(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('/');
    (s.starts_with("https://") && s.len() <= 300 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b))).then(|| s.to_string())
}

pub fn job_key(j: &Job) -> String { if j.provider == "gitlab" { format!("job:gl{}", gitlab::local(&j.gitlab, j.job_id)) } else { format!("job:{}", j.job_id) } }

/// Published prices, per hour, for the machines other clouds started before jobs were sized (jobs recorded without a
/// price): a Cloudflare container of 2 vCPU, 8 GiB, 16 GB (CPU counted as fully busy, so an upper bound) and a Modal
/// sandbox of 2 CPU cores and 8 GiB (sandbox rates).
pub const CLOUDFLARE_CONTAINER_USD_PER_HOUR: f64 = (2.0 * 0.000020 + 8.0 * 0.0000025 + 16.0 * 0.00000007) * 3600.0;
pub const MODAL_SANDBOX_USD_PER_HOUR: f64 = (2.0 * 0.00003942 + 8.0 * 0.00000667) * 3600.0;

/// A Cloudflare container of this size, per hour, at most: memory and disk are billed as provisioned, CPU only while
/// used (counted here as always busy).
pub fn cloudflare_usd_per_hour(size: Size) -> f64 { (size.cpu as f64 * 0.000020 + size.ram_gb as f64 * 0.0000025 + size.disk_gb as f64 * 0.00000007) * 3600.0 }
/// What Cloudflare bills for a container's measured usage (its usage analytics: CPU seconds used, memory and disk
/// byte-seconds provisioned, bytes sent), at list prices: the same numbers Cloudflare's own usage page shows.
pub fn cloudflare_measured_usd(cpu_secs: f64, memory_byte_secs: f64, disk_byte_secs: f64, tx_bytes: f64) -> f64 {
    cpu_secs * 0.000020 + memory_byte_secs / 1_073_741_824.0 * 0.0000025 + disk_byte_secs / 1e9 * 0.00000007 + tx_bytes / 1e9 * 0.025
}

/// What Cloudflare bills for a month of an account's containers, after the usage Workers Paid includes each month
/// (25 GiB-hours of memory, 375 vCPU-minutes, 200 GB-hours of disk, 1 TB sent from North America and Europe).
pub fn cloudflare_billed_usd(cpu_secs: f64, memory_byte_secs: f64, disk_byte_secs: f64, tx_bytes: f64) -> f64 {
    let over = |used: f64, included: f64| (used - included).max(0.0);
    cloudflare_measured_usd(over(cpu_secs, 375.0 * 60.0), over(memory_byte_secs, 25.0 * 3600.0 * 1_073_741_824.0), over(disk_byte_secs, 200.0 * 3600.0 * 1e9), over(tx_bytes, 1e12))
}

/// A Cloudflare container's price while its runner waits: memory and disk (CPU is billed as used, and it idles).
pub fn cloudflare_waiting_usd_per_hour(size: Size) -> f64 { (size.ram_gb as f64 * 0.0000025 + size.disk_gb as f64 * 0.00000007) * 3600.0 }

/// Cloudflare runs this many jobs at once unless the dashboard says otherwise (its containers have no cap of their own).
pub const CLOUDFLARE_JOBS_AT_ONCE: u32 = 20;

/// A Modal sandbox of this size, per hour (sandbox rates).
/// A Modal sandbox of `cpu` vCPUs (half as many of Modal's physical cores) and `ram_gb`, per hour, at Modal's sandbox
/// rates: $0.00003942 a core-second, $0.00000667 a GiB-second (held to its size, it is billed exactly that).
pub fn modal_usd_per_hour(cpu: u32, ram_gb: u32) -> f64 { (cpu as f64 / 2.0 * 0.00003942 + ram_gb as f64 * 0.00000667) * 3600.0 }

/// A Modal GPU's price per hour (Modal's per-second list prices; A100: the 40 GB one).
pub fn modal_gpu_usd_per_hour(gpu: Option<&str>) -> f64 {
    let per_second = match gpu { Some("t4") => 0.000164, Some("l4") => 0.000222, Some("a10g") => 0.000306, Some("l40s") => 0.000542, Some("a100") => 0.000583,
        Some("h100") => 0.001097, Some("h200") => 0.001261, Some("b200") => 0.001736, _ => 0.0 };
    per_second * 3600.0
}

/// A container or sandbox as the dashboard names it: "4cpu-16gb", "2cpu-8gb-t4".
fn machine_name(size: Size) -> String { format!("{}cpu-{}gb{}", size.cpu, size.ram_gb, size.gpu.map(|g| format!("-{g}")).unwrap_or_default()) }

/// How a try ends when starting the machine failed at more than one place (followed by what each said).
const SEVERAL: &str = "no provider could start its machine: ";

/// A place refusing what a job asks, as it would any job asking the same (no hiccup): no GPU machines allowed in the
/// AWS account, or a runner agent refusing the request (Modal: no GPUs without a payment method).
fn refuses_alike(e: &str) -> bool { e.starts_with(NO_GPU_QUOTA) || e.contains(" runner agent: 400 ") || e.starts_with("Modal refused: ") }

/// How a try ends when AWS has no spot machine for a job and nothing else in the order can run it (followed by what
/// each region said): the job waits, and is tried again, rather than failing.
const WAIT_FOR_SPOT: &str = "waiting: AWS has no spot machine for it now, and nothing else in the order can run it (";

/// How a job fails when AWS lets the account run no GPU machines (followed by the quota to raise).
const NO_GPU_QUOTA: &str = "AWS lets this account run no GPU machines yet: ask AWS for more in Service Quotas, ";

fn cloud_name(cloud: &str) -> &str {
    match cloud { "aws" => "AWS", AWS_ON_DEMAND => "AWS on-demand", "cloudflare" => "Cloudflare", "modal" => "Modal", c => c }
}

/// AWS's on-demand machines, as a place of their own in the order (`aws` there is its spot machines): when no region
/// has a spot machine for a job, the job goes to the next place in the order that can run it, which may be this one.
pub const AWS_ON_DEMAND: &str = "aws-on-demand";

/// The place in the order a job's machine counts toward (its limits and its spend).
fn place_of(j: &Job) -> &str { if j.cloud == "aws" && j.on_demand { AWS_ON_DEMAND } else { &j.cloud } }

/// A runner agent in another cloud: it starts one runner per request from this control plane, which it trusts the way
/// AWS does, by this control plane's signed tokens (issuer = the control plane's URL, audience = the agent's URL).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Agent { pub cloud: String, pub url: String }

/// Where jobs run. A job naming a cloud in its label (`superci-aws`) runs only there; else the first rule whose
/// repository pattern matches ("owner/repo", "owner/*" or "*") names its only cloud; else the places in `order` are tried
/// top to bottom: the first that can run the job's machine and is within its limits takes it. Without an order: the
/// default cloud, else every connected cloud (AWS, Cloudflare, agents).
///
/// AWS is two places: `aws`, its spot machines, and `AWS_ON_DEMAND`, its on-demand ones (right after `aws` unless the
/// order puts it elsewhere, or turns it off). A job goes to the next place that can run it when the one before cannot
/// start its machine: AWS has no spot machine for it, or starting it failed. With no spot machine and no other place,
/// it waits for one.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Routing {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<Pool>,
    /// Public repositories whose jobs may run here (patterns as in rules). Others' jobs are refused: on a public
    /// repository anyone can open a pull request. Even here, runs from forks and `pull_request_target` are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_repos: Vec<String>,
    /// The most CPUs a label may ask for (default `MAX_CPU`); memory at most 8 GB per CPU of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cpu: Option<u32>,
}

/// The largest machine a label may ask for unless the dashboard sets another: 32 CPUs.
pub const MAX_CPU: u32 = 32;
/// Jobs at once in a cloud unless the dashboard sets another.
pub const JOBS_AT_ONCE: u32 = 20;

fn matches_repo(pattern: &str, repo: &str) -> bool {
    pattern == "*" || pattern.eq_ignore_ascii_case(repo)
        || pattern.strip_suffix("/*").is_some_and(|owner| repo.split('/').next().is_some_and(|o| o.eq_ignore_ascii_case(owner)))
}

/// A place in the order, with its limits: jobs at once, and spend this month (USD, estimated as on the dashboard).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pool {
    pub cloud: String,
    /// Turned off in the dashboard: it takes no jobs (only `AWS_ON_DEMAND` can be, the others are removed instead).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub off: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_jobs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub monthly_usd: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rule { pub repo: String, pub cloud: String }

impl Routing {
    /// The cloud this policy names for a repository, if any.
    pub fn cloud_for(&self, repo: &str) -> Option<&str> { self.rule_for(repo).or(self.default.as_deref()) }

    /// The cloud a repository's rule names (an exception to the order), if any.
    pub fn rule_for(&self, repo: &str) -> Option<&str> {
        self.rules.iter().find(|r| matches_repo(&r.repo, repo)).map(|r| r.cloud.as_str())
    }

    /// A public repository allowed to run jobs here.
    pub fn allows_public(&self, repo: &str) -> bool { self.public_repos.iter().any(|p| matches_repo(p, repo)) }

    /// Why a machine is larger than allowed, if it is.
    pub fn too_large(&self, spec: &Spec) -> Option<String> {
        let max = self.max_cpu.unwrap_or(MAX_CPU);
        if spec.cpu.is_some_and(|c| c > max) { return Some(format!("it asks for {} CPUs; the most allowed is {max} (set in the dashboard)", spec.cpu.unwrap_or_default())) }
        if spec.ram_gb.is_some_and(|g| g > max * 8) { return Some(format!("it asks for {} GB of memory; the most allowed is {} GB (set in the dashboard)", spec.ram_gb.unwrap_or_default(), max * 8)) }
        None
    }

    /// The limits for a cloud, from its place in the order.
    pub fn pool(&self, cloud: &str) -> Option<&Pool> { self.order.iter().find(|p| p.cloud == cloud) }
}

enum Runner { Aws(AwsConnection), Agent(Agent), Own }

/// Where a job goes: a place now, later (every place that could run it is full, over its limit or offline), or nowhere
/// (no connected place can run its machine), each with why.
enum Placement { Run(Vec<(String, Runner)>), Wait(String), Fail(String) }

/// The start of this month (UTC), in ms.
pub fn month_start_ms(now_ms: u64) -> u64 {
    let days = now_ms / 86_400_000;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm), then back to the 1st of that month.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let day = doy - (153 * ((5 * doy + 2) / 153) + 2) / 5; // 0-based day of month
    (days as i64 - day) as u64 * 86_400_000
}

/// A job's estimated machine cost so far (USD).
pub fn job_usd(j: &Job, now_ms: u64) -> f64 {
    if let Some(settled) = j.cost_usd { return settled }
    let Some(per_hour) = j.usd_per_hour else { return 0.0 };
    // Paid from when the machine started, whether or not a job came: an AWS machine boots billed, a sandbox is billed
    // for what it holds, a container for its memory and disk (and CPU as used) while its runner waits.
    let from = j.launched_ms.or(j.started_ms).unwrap_or(j.at_ms);
    let end = j.ended_ms.unwrap_or(if active(j) { now_ms } else { from });
    let began = j.started_ms.unwrap_or(end).clamp(from, end.max(from));
    (began.saturating_sub(from) as f64 * j.waiting_usd_per_hour.unwrap_or(per_hour) + end.saturating_sub(began) as f64 * per_hour) / 3_600_000.0
}

/// GitHub's price per minute for its hosted Linux runner with this many CPUs (2 and fewer: the standard runner), x64 or
/// arm64 (GitHub's prices from January 2026; each job is rounded up to whole minutes).
pub fn github_usd_per_minute(cpu: u32, arm64: bool) -> f64 {
    if arm64 { return match cpu { 0..=2 => 0.005, 3..=4 => 0.008, 5..=8 => 0.014, 9..=16 => 0.026, 17..=32 => 0.050, _ => 0.098 } }
    match cpu { 0..=2 => 0.006, 3..=4 => 0.012, 5..=8 => 0.022, 9..=16 => 0.042, 17..=32 => 0.082, 33..=64 => 0.162, _ => 0.252 }
}

/// What a cloud's machines can be (the clouds this control plane starts machines in).
fn capacity_of(cloud: &str) -> Capacity {
    match cloud { "aws" => Capacity::aws(), "modal" => Capacity::modal(), _ => Capacity::cloudflare() }
}

fn active(j: &Job) -> bool { ["launching", "launched", "running"].contains(&j.state.as_str()) }

pub struct ControlPlane<'a> {
    pub store: &'a dyn Store,
    pub http: &'a dyn Http,
    pub clock: &'a dyn Clock,
    pub timer: &'a dyn Timer,
    pub config: &'a Config,
    pub cache: &'a Cache,
    /// The runtime's own containers, when it has them.
    pub containers: Option<&'a dyn Containers>,
}

/// A key that only reads, from the name and value it is stored under (`READ_KEY_<expires, unix seconds>_<NAME>`, the
/// key's SHA-256): when it expires (ms), its name, the hash. A removed one (emptied, where a store keeps names) is none.
pub fn read_key(name: &str, value: &str) -> Option<(u64, String, String)> {
    let (until, key_name) = name.strip_prefix("READ_KEY_")?.split_once('_')?;
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) { return None }
    Some((until.parse::<u64>().ok()? * 1000, key_name.to_string(), value.to_ascii_lowercase()))
}

fn nothing_here() -> Response {
    Response::text(404, "SuperCI plane: nothing here (set up and changed from the SuperCI dashboard on your machine)")
}

fn origin(url: &str) -> String {
    url::Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_default()
}

/// The role the dashboard makes in an AWS account for a control plane (`connect_runners`).
pub fn role_arn(account_id: &str, plane_id: &str) -> String {
    format!("arn:aws:iam::{account_id}:role/superci-plane-{plane_id}")
}

impl<'a> ControlPlane<'a> {
    /// The control plane's signing key: made once, kept in its storage; only the public half is ever served.
    async fn identity(&self) -> Result<Identity> {
        if let Some(i) = get_json::<Identity>(self.store, "identity").await? { return Ok(i); }
        let i = Identity { signing: SigningKeyStore::generate() };
        put_json(self.store, "identity", &i).await?;
        Ok(i)
    }

    pub async fn handle(&self, req: Request) -> Response {
        match self.route(req).await {
            Ok(r) => r,
            Err(e) => Response::text(500, &e.chars().take(300).collect::<String>()),
        }
    }

    async fn route(&self, req: Request) -> Result<Response> {
        let url = url::Url::parse(&req.url).map_err(|e| e.to_string())?;
        let plane_url = origin(&req.url);
        if get_json::<String>(self.store, "plane_url").await?.as_deref() != Some(plane_url.as_str()) { put_json(self.store, "plane_url", &plane_url).await? }
        match (req.method.as_str(), url.path()) {
            ("GET", "/.well-known/openid-configuration") => Ok(Response::json(&serde_json::json!({
                "issuer": plane_url, "jwks_uri": format!("{plane_url}/.well-known/jwks.json"), "response_types_supported": ["id_token"],
                "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["ES256"], "claims_supported": ["sub", "aud", "iss", "exp", "iat"] }))),
            ("GET", "/.well-known/jwks.json") => Ok(Response::json(&serde_json::json!({ "keys": [self.identity().await?.signing.public_jwk()?] }))),
            ("GET", "/health") => {
                let aws = self.aws_connection().await?.is_some_and(|a| a.connected);
                let runners = aws || !self.config.agents.is_empty() || self.own_containers();
                let moved_to = get_json::<String>(self.store, "moved_to").await?;
                let mut health = serde_json::json!({ "plane": self.config.plane_id, "label": self.config.label, "version": VERSION, "github": self.config.app.is_some(), "gitlab": self.config.gitlabs().next().is_some(), "installed": self.installed().await, "aws": aws, "runners": runners });
                // Moved: another control plane took over (the dashboard shows it as not in use). Standby: taking over,
                // not yet in use.
                if let Some(to) = moved_to { health["moved_to"] = to.into() }
                if self.store.get("standby").await?.is_some() { health["standby"] = true.into() }
                Ok(Response::json(&health))
            }
            ("GET", "/status") => self.status(&req).await,
            // Moved: events that still arrive here (sent just before the switch) go on to the new control plane.
            ("POST", "/webhook") | ("POST", "/gitlab/webhook") if self.store.get("moved_to").await?.is_some() => self.forward(&req, url.path()).await,
            ("POST", "/webhook") => self.webhook(&req, &plane_url).await,
            ("POST", "/gitlab/webhook") => self.gitlab_webhook(&req, &plane_url).await,
            ("POST", "/aws/callback") => self.aws_callback(&req, &plane_url).await,
            // A spot machine saying AWS is taking it back (only it has its job's link).
            ("POST", "/interrupted") => {
                let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
                // The job its machine is on now: a GitHub runner may have taken another than the one it was started
                // for (found by the runner's name); a GitLab machine's record is its own.
                let found = match q.get("gl") {
                    Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') => get_json::<Job>(self.store, &format!("job:gl{id}")).await?,
                    Some(_) => None,
                    None => self.by_runner(q.get("runner").map(String::as_str)).await?,
                };
                let Some(mut j) = found else { return Ok(nothing_here()) };
                if !j.notice.as_deref().is_some_and(|n| n.len() >= 16 && q.get("t").is_some_and(|t| safe_eq(t.as_bytes(), n.as_bytes()))) { return Ok(nothing_here()) }
                if !j.interrupted && active(&j) {
                    j.interrupted = true;
                    put_json(self.store, &job_key(&j), &j).await?;
                    // One record each (two machines may say so at once), counted by `spot_paused`.
                    let now = self.clock.now_ms();
                    put_json(self.store, &format!("spot:interrupted:{now}:{}", j.job_id), &now).await?;
                }
                Ok(Response::text(200, "ok"))
            }
            // GitLab's projects, for the dashboard: which send their jobs here; switching one on or off; after the
            // connection changes, every webhook here gets the new secret.
            ("GET", "/job/log") if self.reader(&req) => self.job_log(&url).await,
            ("GET", "/gitlab/projects") if self.reader(&req) => {
                // Of one connection (`g`: its name; nothing: the first).
                let which = url.query_pairs().find(|(k, _)| k == "g").map(|(_, v)| v.into_owned()).unwrap_or_default();
                let gl = self.gitlab_by(&which).ok_or("GitLab is not connected")?;
                let list = gitlab::projects(self.http, gl, &format!("{plane_url}/gitlab/webhook")).await?;
                // The last GitLab event that arrived, so the dashboard can say whether webhooks reach here.
                let last = get_json::<u64>(self.store, &last_event_key(&gl.id)).await?;
                Ok(Response::json(&serde_json::json!({ "projects": list, "last_event_ms": last })))
            }
            ("POST", "/gitlab/projects") if self.dashboard(&req) => {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let gl = self.gitlab_by(body["gitlab"].as_str().unwrap_or_default()).ok_or("GitLab is not connected")?;
                let hook = format!("{plane_url}/gitlab/webhook");
                let ids: Vec<(u64, bool)> = if body["refresh"] == true {
                    gitlab::projects(self.http, gl, &hook).await?.into_iter().filter(|p| p.enabled).map(|p| (p.id, true)).collect()
                } else if body["all_off"] == true {
                    gitlab::projects(self.http, gl, &hook).await?.into_iter().filter(|p| p.enabled).map(|p| (p.id, false)).collect()
                } else if body["all_on"] == true {
                    gitlab::projects(self.http, gl, &hook).await?.into_iter().filter(|p| !p.enabled).map(|p| (p.id, true)).collect()
                } else { vec![(body["id"].as_u64().ok_or("which project?")?, body["enabled"] == true)] };
                for (id, on) in &ids { gitlab::set_project(self.http, gl, *id, &hook, *on).await? }
                Ok(Response::json(&serde_json::json!({ "changed": ids.len() })))
            }
            // Moving to another control plane (see `move_out`, `move_in`, `claim`).
            ("POST", "/move/export") if self.dashboard(&req) => self.move_out(&req).await,
            ("POST", "/move/import") if self.dashboard(&req) => self.move_in(&req).await,
            ("POST", "/move/claim") if self.dashboard(&req) => self.claim(&req, &plane_url).await,
            ("POST", "/leave") if self.dashboard(&req) => self.leave(&plane_url).await,
            ("POST", "/move/away") if self.dashboard(&req) => {
                let to = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default()["to"].as_str().unwrap_or_default().to_string();
                if !to.starts_with("https://") { return Ok(Response::text(400, "where to?")) }
                put_json(self.store, "moved_to", &to).await?;
                Ok(Response::json(&serde_json::json!({ "moved_to": to })))
            }
            // The public repositories the App is installed on, for the dashboard to allow them one by one (read again
            // after five minutes).
            ("GET", "/github/public-repos") if self.dashboard(&req) => {
                let now = self.clock.now_ms();
                if let Some((repos, until)) = self.cache.public_repos.borrow().clone() { if now < until { return Ok(Response::json(&serde_json::json!({ "repos": repos }))) } }
                if self.config.app.is_none() { return Err("no GitHub App".into()) }
                // Each App by itself: one that GitHub no longer answers for (deleted there) does not hide the others'.
                let (mut repos, mut failed) = (vec![], vec![]);
                for app in self.config.apps() {
                    let of_app: Result<Vec<String>> = async {
                        let mut out = vec![];
                        for i in github::installations(self.http, app, now).await? {
                            let token = self.installation_token(app, i.id).await?;
                            out.extend(github::public_repositories(self.http, &app.api(), &token).await?);
                        }
                        Ok(out)
                    }.await;
                    match of_app { Ok(r) => repos.extend(r), Err(e) => failed.push(format!("{}: {}", app.owner, e.chars().take(200).collect::<String>())) }
                }
                if repos.is_empty() && !failed.is_empty() { return Err(failed.join("; ")) }
                repos.sort();
                if failed.is_empty() { *self.cache.public_repos.borrow_mut() = Some((repos.clone(), now + 300_000)); }
                Ok(Response::json(&serde_json::json!({ "repos": repos, "failed": failed })))
            }
            // The dashboard gave a cloud the permissions this version asks for: what it refused before is forgotten.
            // An organization removed: its App (one of the further ones) is uninstalled where it is installed. The
            // dashboard then drops its secret; the App itself is deleted on GitHub.
            ("POST", "/github/uninstall") if self.dashboard(&req) => {
                let id = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default()["app"].as_u64();
                let Some(app) = self.config.more_apps.iter().find(|a| Some(a.id) == id) else { return Ok(Response::text(404, "no such App here")) };
                let now = self.clock.now_ms();
                // An App already deleted on GitHub has nothing left to uninstall: said, not an error (the dashboard
                // drops its secret either way).
                let (mut installations, mut error) = (0, None);
                match github::installations(self.http, app, now).await {
                    Ok(list) => for i in list { match github::delete_installation(self.http, app, i.id, now).await { Ok(()) => installations += 1, Err(e) => error = Some(e) } },
                    Err(e) => error = Some(e),
                }
                Ok(Response::json(&serde_json::json!({ "github_installations": installations, "error": error })))
            }
            // AWS runners removed (the dashboard deletes the role and network with the account's sign-in): the connection
            // is forgotten, so no job goes there; adding AWS again connects anew.
            ("POST", "/aws/forget") if self.dashboard(&req) => {
                self.store.delete("aws").await?;
                self.cache.creds.replace(None);
                self.cache.network.borrow_mut().clear();
                self.cache.image.borrow_mut().clear();
                Ok(Response::json(&serde_json::json!({ "aws": false })))
            }
            ("POST", "/permissions/given") if self.dashboard(&req) => {
                let cloud = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default()["cloud"].as_str().unwrap_or_default().to_string();
                let mut cleared = 0;
                for (k, _) in self.store.list(&format!("denied:{cloud}:")).await? { self.store.delete(&k).await?; cleared += 1 }
                Ok(Response::json(&serde_json::json!({ "cleared": cleared })))
            }
            // Costs the dashboard read from a cloud's own metering (Cloudflare's usage analytics), kept with the jobs.
            ("POST", "/costs") if self.dashboard(&req) => {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let mut kept = 0;
                for (key, usd) in body["measured"].as_object().into_iter().flatten() {
                    let Some(usd) = usd.as_f64().filter(|u| u.is_finite() && (0.0..1000.0).contains(u)) else { continue };
                    if !key.starts_with("job:") { continue }
                    let Some(mut j) = get_json::<Job>(self.store, key).await? else { continue };
                    if j.cloud != "cloudflare" || active(&j) { continue }
                    j.cost_usd = Some(usd);
                    j.cost_from = Some("measured".into());
                    put_json(self.store, key, &j).await?;
                    kept += 1;
                }
                Ok(Response::json(&serde_json::json!({ "kept": kept })))
            }
            _ => Ok(nothing_here()),
        }
    }

    /// An event for the control plane this one moved to, passed on as it came (its signature still holds there).
    async fn forward(&self, req: &Request, path: &str) -> Result<Response> {
        let to = get_json::<String>(self.store, "moved_to").await?.ok_or("not moved")?;
        // Only events that really come from GitHub or GitLab (as here before the move).
        let genuine = if path == "/webhook" { self.config.apps().any(|a| verify_hub_signature(&a.webhook_secret, &req.body, req.header("x-hub-signature-256"))) }
            else { self.gitlab_signed(req).is_some() };
        if !genuine { return Ok(Response::text(401, "bad signature")) }
        let mut out = Request::new("POST", &format!("{to}{path}")).with_body(req.body.clone());
        for (k, v) in &req.headers {
            if k.starts_with("x-hub-") || k.starts_with("x-github-") || k.starts_with("x-gitlab-") || k == "content-type" || k == "user-agent" { out = out.with_header(k, v) }
        }
        let r = self.http.send(out).await?;
        Ok(Response::text(r.status, &r.body_text()))
    }

    /// Stopping SuperCI: GitLab's projects stop sending jobs here (their webhooks go) and the project runners it
    /// made are removed; GitHub's App is removed from where it is installed. What it did, for the dashboard.
    async fn leave(&self, plane_url: &str) -> Result<Response> {
        let now = self.clock.now_ms();
        let (mut projects, mut runners, mut installations) = (0, 0, 0);
        for gl in self.config.gitlabs() {
            let hook = format!("{plane_url}/gitlab/webhook");
            for p in gitlab::projects(self.http, gl, &hook).await?.into_iter().filter(|p| p.enabled) {
                gitlab::set_project(self.http, gl, p.id, &hook, false).await?;
                projects += 1;
            }
            // Its runners (each record says whose it is; ones from before there could be several: the first's).
            for (k, v) in self.store.list("glrunner:").await? {
                let v = serde_json::from_str::<serde_json::Value>(&v).unwrap_or_default();
                if v["gitlab"].as_str().unwrap_or_default() != gl.id { continue }
                if let Some(id) = v["id"].as_u64() { gitlab::delete_runner(self.http, gl, id).await?; runners += 1 }
                self.store.delete(&k).await?;
            }
        }
        // Each App by itself (one deleted on GitHub already does not keep the others installed).
        let mut failed = vec![];
        for app in self.config.apps() {
            match github::installations(self.http, app, now).await {
                Ok(list) => for i in list { match github::delete_installation(self.http, app, i.id, now).await { Ok(()) => installations += 1, Err(e) => failed.push(format!("{}: {e}", app.owner)) } },
                Err(e) => failed.push(format!("{}: {e}", app.owner)),
            }
        }
        Ok(Response::json(&serde_json::json!({ "gitlab_projects": projects, "gitlab_runners": runners, "github_installations": installations, "github_failed": failed })))
    }

    /// Whether a request carries this control plane's one-time move token (set through the cloud's API), unused.
    async fn move_token_ok(&self, req: &Request) -> Result<bool> {
        let (Some(want), Some(given)) = (&self.config.move_token, req.header("x-move-token")) else { return Ok(false) };
        if want.len() < 16 || !safe_eq(given.as_bytes(), want.as_bytes()) { return Ok(false) }
        // Each token is taken once.
        self.store.put_if_absent(&format!("move_token_used:{}", sha256_hex(want.as_bytes())), "1".into()).await
    }

    /// Moving away, step one: everything another control plane needs to take over, for the dashboard to hand it there
    /// (the App with its key, GitLab, the order and the default machine; GitLab's project runners and history). Only
    /// with the one-time move token.
    async fn move_out(&self, req: &Request) -> Result<Response> {
        if !self.move_token_ok(req).await? { return Ok(Response::text(403, "no move token, or one used already")) }
        let records = |prefix: &'static str| async move { self.store.list(prefix).await };
        Ok(Response::json(&serde_json::json!({
            "app": self.config.app, "more_apps": self.config.more_apps, "gitlab": self.config.gitlab, "more_gitlabs": self.config.more_gitlabs, "routing": self.config.routing, "machine": self.config.machine,
            // GitLab's project runners, and jobs that have finished (ones still running finish here).
            "state": records("glrunner:").await?.into_iter()
                .chain(records("job:").await?.into_iter().filter(|(_, v)| serde_json::from_str::<Job>(v).is_ok_and(|j| ["done", "failed", "cancelled", "swept", "orphan"].contains(&j.state.as_str()))))
                .collect::<Vec<_>>(),
        })))
    }

    /// Moving here: the GitLab project runners and history of the control plane before (what is here already stays).
    async fn move_in(&self, req: &Request) -> Result<Response> {
        if !self.move_token_ok(req).await? { return Ok(Response::text(403, "no move token, or one used already")) }
        let body: serde_json::Value = serde_json::from_slice(&req.body).map_err(|e| e.to_string())?;
        let mut taken = 0;
        for pair in body["state"].as_array().into_iter().flatten() {
            let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
            if !(k.starts_with("glrunner:") || k.starts_with("job:")) { continue }
            if self.store.put_if_absent(k, v.to_string()).await? { taken += 1 }
        }
        // Not in use until the switch (`claim`).
        self.store.put("standby", "1".into()).await?;
        Ok(Response::json(&serde_json::json!({ "taken": taken })))
    }

    /// Moving here, the switch: GitHub's App sends its jobs here, and GitLab projects that sent theirs to the control
    /// plane before (`from`) send them here instead.
    async fn claim(&self, req: &Request, plane_url: &str) -> Result<Response> {
        let from = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default()["from"].as_str().unwrap_or_default().trim_end_matches('/').to_string();
        let now = self.clock.now_ms();
        // Every App's webhook, each by itself; one that would not move is said (its organization's jobs still go to
        // the control plane before, which passes them on).
        let mut failed = vec![];
        for app in self.config.apps() {
            if let Err(e) = github::set_webhook_url(self.http, app, &format!("{plane_url}/webhook"), now).await { failed.push(format!("{}: {}", app.owner, e.chars().take(200).collect::<String>())) }
        }
        if !failed.is_empty() && failed.len() == self.config.apps().count() { return Err(failed.join("; ")) }
        let mut projects = 0;
        for gl in self.config.gitlabs().filter(|_| from.starts_with("https://")) {
            let (old, new) = (format!("{from}/gitlab/webhook"), format!("{plane_url}/gitlab/webhook"));
            for p in gitlab::projects(self.http, gl, &old).await?.into_iter().filter(|p| p.enabled) {
                gitlab::set_project(self.http, gl, p.id, &new, true).await?;
                gitlab::set_project(self.http, gl, p.id, &old, false).await?;
                projects += 1;
            }
        }
        // In use (again, when moving back to a control plane moved away from before).
        self.store.delete("moved_to").await?;
        self.store.delete("standby").await?;
        self.timer.wake_in(60_000).await?;
        Ok(Response::json(&serde_json::json!({ "github": self.config.app.is_some(), "github_apps": self.config.apps().count(), "github_failed": failed, "gitlab_projects": projects })))
    }

    /// Whether a request carries a dashboard session's key that has not expired.
    fn dashboard(&self, req: &Request) -> bool {
        let given = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        let now = self.clock.now_ms();
        self.config.dashboard_keys.iter().any(|(until, key)| *until > now && key.len() >= 16 && safe_eq(given.as_bytes(), key.as_bytes()))
    }

    /// Whether a request may read: a dashboard session's key, or a key that only reads and has not expired.
    fn reader(&self, req: &Request) -> bool {
        if self.dashboard(req) { return true }
        let given = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if given.len() < 16 { return false }
        let (hash, now) = (sha256_hex(given.as_bytes()), self.clock.now_ms());
        self.config.read_keys.iter().any(|(until, _, kept)| *until > now && safe_eq(hash.as_bytes(), kept.as_bytes()))
    }

    /// A job's log, for whoever may read: GitHub's for the job (with the App's token for its repository), or GitLab's
    /// trace. Its end (`bytes`: how much, a megabyte at most), since the end says why a job failed.
    async fn job_log(&self, url: &url::Url) -> Result<Response> {
        let q = |n: &str| url.query_pairs().find(|(k, _)| k == n).map(|(_, v)| v.into_owned());
        let id = q("id").filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit())).ok_or("which job?")?;
        // A GitLab job is kept under its connection's name and its id (`gl`: the name; nothing for the first).
        let key = match q("gl") { Some(g) if gitlab::valid_id(&g) || g.is_empty() => format!("job:gl{}", gitlab::local(&g, id.parse().unwrap_or_default())), Some(_) => return Err("which GitLab?".into()), None => format!("job:{id}") };
        let Some(j) = get_json::<Job>(self.store, &key).await? else { return Ok(Response::json(&serde_json::json!({ "error": "This control plane has no such job (it keeps the latest ones)." }))) };
        let fetched = if j.provider == "gitlab" {
            match (self.gitlab_of(&j), j.project_id) { (Some(gl), Some(project)) => gitlab::job_trace(self.http, gl, project, j.job_id).await, _ => Err("its GitLab is no longer connected".into()) }
        } else {
            match (self.app_of(&j), self.installation_token_for(&j).await) { (Some(app), Some(token)) => github::job_log(self.http, &app.api(), &token, &j.repo, j.job_id).await, _ => Err("GitHub gave no token for its repository (is the App still installed there?)".into()) }
        };
        let log = match fetched { Ok(l) => l, Err(e) => return Ok(Response::json(&serde_json::json!({ "error": e }))) };
        let want = q("bytes").and_then(|b| b.parse::<usize>().ok()).unwrap_or(200_000).clamp(1_000, 1_000_000);
        // From a line's start, when the cut falls inside one.
        let from = log.len().saturating_sub(want);
        let from = if from == 0 { 0 } else { log[from..].iter().position(|b| *b == b'\n').map(|n| from + n + 1).unwrap_or(from) };
        Ok(Response::json(&serde_json::json!({ "log": String::from_utf8_lossy(&log[from..]), "bytes": log.len(), "truncated": from > 0 })))
    }

    /// What the dashboard shows, for a key a dashboard session set and that has not expired (or one that only reads):
    /// the App and where it is installed, the AWS connection, and recent jobs. Without a key it is like any unknown path.
    async fn status(&self, req: &Request) -> Result<Response> {
        if !self.reader(req) { return Ok(nothing_here()); }
        // `light`: asked only to see a setting take (after each change): without what GitHub has to be asked for.
        let light = url::Url::parse(&req.url).is_ok_and(|u| u.query_pairs().any(|(k, v)| k == "light" && v == "1"));
        let mut installations = vec![];
        for app in self.config.apps().filter(|_| !light) {
            installations.extend(github::installations(self.http, app, self.clock.now_ms()).await.unwrap_or_default()
                .into_iter().map(|i| serde_json::json!({ "account": i.account, "repositories": i.selection, "id": i.id, "permissions": i.permissions, "app": app.id })));
        }
        // Each App (one per organization), with what it asks GitHub for.
        let mut apps = vec![];
        for app in self.config.apps() {
            apps.push(serde_json::json!({ "id": app.id, "slug": app.slug, "owner": app.owner, "org": app.owner_is_org, "host": app.host, "permissions": self.app_permissions(app).await }));
        }
        let mut gitlabs = vec![];
        for gl in self.config.gitlabs() { gitlabs.push(serde_json::json!({ "id": gl.id, "url": gl.url, "scopes": self.gitlab_scopes(gl).await })) }
        let mut jobs_all: Vec<Job> = self.store.list("job:").await?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
        jobs_all.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
        let jobs: Vec<Job> = jobs_all.iter().take(50).cloned().collect();
        Ok(Response::json(&serde_json::json!({
            "plane": self.config.plane_id, "label": self.config.label,
            "app": self.config.app.as_ref().map(|a| serde_json::json!({ "id": a.id, "slug": a.slug, "owner": a.owner, "org": a.owner_is_org, "host": a.host })),
            "apps": apps,
            "installations": installations,
            "aws": self.aws_connection().await?,
            "aws_regions": self.aws_connection().await?.map(|a| self.aws_regions(&a)).unwrap_or_default(),
            "aws_networks": self.config.aws_networks,
            "cloudflare_location": self.config.cloudflare_location,
            "cloudflare_image": self.config.cloudflare_image,
            "agents": self.config.agents,
            "containers": self.own_containers(),
            "own_cloud": self.config.own_cloud,
            "gitlab": self.config.gitlab.as_ref().map(|g| serde_json::json!({ "url": g.url })),
            // Every GitLab connection (the first, then each further one, by its name), with its token's scopes.
            "gitlabs": gitlabs,
            "routing": self.config.routing,
            "read_keys": if self.dashboard(req) { serde_json::json!(self.config.read_keys.iter().map(|(until, name, _)| serde_json::json!({ "name": name, "until_ms": until })).collect::<Vec<_>>()) } else { serde_json::Value::Null },
            "machine": self.config.machine,
            "spend": self.month_spend(&jobs_all).await,
            // Machines up now in each cloud, over every job (`jobs` is the newest fifty).
            "active": jobs_all.iter().filter(|j| active(j) && !j.cloud.is_empty()).fold(HashMap::<String, u32>::new(), |mut m, j| { *m.entry(place_of(j).to_string()).or_default() += 1; m }),
            // What the clouds and code hosts allow it (see permissions.rs): what the App asks for (each installation's
            // accepted permissions are in `installations`), the GitLab token's scopes, and permissions refused lately.
            "permissions": {
                "github_app": match &self.config.app { Some(app) => self.app_permissions(app).await, None => None },
                "gitlab_scopes": match &self.config.gitlab { Some(gl) => self.gitlab_scopes(gl).await, None => None },
                "denied": self.store.list("denied:").await?.into_iter().filter_map(|(k, v)| Some((k.strip_prefix("denied:")?.to_string(), serde_json::from_str::<serde_json::Value>(&v).ok()?))).collect::<serde_json::Map<_, _>>(),
            },
            "jobs": jobs,
        })))
    }

    /// What the App asks GitHub for (asked again after ten minutes).
    async fn app_permissions(&self, app: &App) -> Option<serde_json::Value> {
        let now = self.clock.now_ms();
        if let Some((p, until)) = self.cache.app_permissions.borrow().get(&app.id).cloned() { if now < until { return Some(p) } }
        let p = github::app_permissions(self.http, app, now).await.ok()?;
        self.cache.app_permissions.borrow_mut().insert(app.id, (p.clone(), now + 600_000));
        Some(p)
    }

    /// The GitLab token's scopes (asked again after ten minutes).
    async fn gitlab_scopes(&self, gl: &GitLab) -> Option<Vec<String>> {
        let now = self.clock.now_ms();
        if let Some((s, until)) = self.cache.gitlab_scopes.borrow().get(&gl.id).cloned() { if now < until { return Some(s) } }
        let s = gitlab::token_scopes(self.http, gl).await.ok()?;
        self.cache.gitlab_scopes.borrow_mut().insert(gl.id.clone(), (s.clone(), now + 600_000));
        Some(s)
    }

    /// A GitLab connection by its name (nothing: the first), and the one a job came through.
    fn gitlab_by(&self, id: &str) -> Option<&GitLab> { self.config.gitlabs().find(|g| g.id == id) }
    fn gitlab_of(&self, j: &Job) -> Option<&GitLab> { self.gitlab_by(&j.gitlab) }

    /// The connection whose webhook secret a request carries (each has its own).
    fn gitlab_signed(&self, req: &Request) -> Option<&GitLab> {
        let given = req.header("x-gitlab-token")?;
        self.config.gitlabs().find(|g| g.hook_secret.len() >= 16 && safe_eq(given.as_bytes(), g.hook_secret.as_bytes()))
    }

    /// Notes whether a cloud refused one of its permissions (`permissions.rs`) for a call just made: refused, it is kept
    /// (when, and what the cloud said) for the dashboard; allowed again, it is cleared.
    async fn note_permission<T>(&self, cloud: &str, id: &str, result: &Result<T>) {
        let key = format!("denied:{cloud}:{id}");
        match result {
            Err(e) if ["AccessDenied", "UnauthorizedOperation", "not authorized"].iter().any(|d| e.contains(d)) => {
                let _ = put_json(self.store, &key, &serde_json::json!({ "at_ms": self.clock.now_ms(), "error": e.chars().take(300).collect::<String>() })).await;
            }
            Ok(_) => { if self.store.get(&key).await.ok().flatten().is_some() { let _ = self.store.delete(&key).await; } }
            Err(_) => {}
        }
    }

    /// Spend this month per place in the order (USD, estimated), for the order's limits and the dashboard.
    async fn month_spend(&self, jobs: &[Job]) -> HashMap<String, f64> {
        let now = self.clock.now_ms();
        let since = month_start_ms(now);
        let mut spend = HashMap::new();
        for j in jobs.iter().filter(|j| j.at_ms >= since) { *spend.entry(place_of(j).to_string()).or_insert(0.0) += job_usd(j, now) }
        spend
    }

    /// A job whose machine never began it: its runner registration is withdrawn and it is placed again (elsewhere, or
    /// waiting for room).
    async fn again(&self, plane_url: &str, key: &str, j: &mut Job, why: &str) -> Result<()> {
        self.withdraw_runner(j).await;
        if let (Some(id), Some(runner)) = (&j.machine_id, &j.runner) { let _ = self.store.delete(&format!("assign:{id}:{runner}")).await; }
        j.runner = None;
        j.runner_id = None;
        j.machine_id = None;
        j.machine_type = None;
        // What its machine said (a spot machine taken back before it began anything) goes with the machine.
        (j.notice, j.interrupted) = (None, false);
        j.cloud = String::new();
        j.state = "waiting".into();
        j.error = Some(format!("waiting: {why}"));
        put_json(self.store, key, &*j).await?;
        self.start(plane_url, j).await
    }

    /// The GitHub App a job's event came through (jobs from before there could be several: the first), and the
    /// account its runners register with.
    fn app_of(&self, j: &Job) -> Option<&App> {
        match j.app_id { Some(id) => self.config.apps().find(|a| a.id == id), None => self.config.app.as_ref() }
    }

    fn owner_of(app: &App) -> Owner { Owner { org: app.owner_is_org, login: app.owner.clone() } }

    /// An installation token for a job's repository (None without the App, or if GitHub would not give one).
    async fn installation_token_for(&self, j: &Job) -> Option<String> {
        self.installation_token(self.app_of(j)?, j.installation_id).await.ok()
    }

    /// Whether the App is installed anywhere (asked with its own key, at most once a minute).
    async fn installed(&self) -> bool {
        if self.config.app.is_none() { return false }
        let now = self.clock.now_ms();
        if let Some((yes, at)) = *self.cache.installed.borrow() { if now.saturating_sub(at) < 60_000 { return yes; } }
        let mut yes = false;
        for app in self.config.apps() { yes |= github::installations(self.http, app, now).await.map(|i| !i.is_empty()).unwrap_or(false) }
        self.cache.installed.replace(Some((yes, now)));
        yes
    }

    /// Connected when the control plane can assume the role and see the machine image.
    async fn verify_aws(&self, plane_url: &str, mut a: AwsConnection) -> Result<AwsConnection> {
        self.cache.creds.replace(None);
        let check = async {
            let creds = self.aws_creds(plane_url, &a).await?;
            aws::latest_image(self.http, &a.region, &creds, &self.config.image_owner, &self.config.image_name, self.clock.now_ms()).await
        }.await;
        a.connected = check.is_ok();
        a.error = check.err().map(|e| e.chars().take(300).collect());
        put_json(self.store, "aws", &a).await?;
        Ok(a)
    }

    /// The dashboard's report of the AWS role it made: with the connect token it set, for this control plane's own role,
    /// in the region it chose; each token is accepted once.
    async fn aws_callback(&self, req: &Request, plane_url: &str) -> Result<Response> {
        let (Some(token), Some(region)) = (&self.config.aws_connect_token, &self.config.aws_region) else { return Ok(Response::text(409, "no AWS connection in progress")) };
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let s = |k: &str| body[k].as_str().unwrap_or_default().to_string();
        if token.len() < 16 || !safe_eq(s("token").as_bytes(), token.as_bytes()) { return Ok(Response::text(403, "not this control plane's stack")); }
        let used = sha256_hex(token.as_bytes());
        if get_json::<String>(self.store, "aws_token_used").await?.as_deref() == Some(used.as_str()) { return Ok(Response::text(409, "this connect token was used already")); }
        let account_id = s("accountId");
        if account_id.len() != 12 || !account_id.bytes().all(|b| b.is_ascii_digit()) || s("roleArn") != role_arn(&account_id, &self.config.plane_id) || s("region") != *region {
            return Ok(Response::text(400, "unexpected account, role or region"));
        }
        let a = self.verify_aws(plane_url, AwsConnection { role_arn: role_arn(&account_id, &self.config.plane_id), account_id, region: region.clone(), connected: false, error: None }).await?;
        if a.connected { put_json(self.store, "aws_token_used", &used).await?; }
        Ok(Response::json(&serde_json::json!({ "connected": a.connected, "error": a.error })))
    }

    /// Credentials for the connected role: a token this control plane signs as its own identity provider, exchanged with STS.
    async fn aws_creds(&self, plane_url: &str, a: &AwsConnection) -> Result<Credentials> {
        if a.role_arn.is_empty() { return self.config.aws_own_creds.clone().ok_or_else(|| "no AWS credentials in this runtime".to_string()); }
        let now = self.clock.now_ms();
        if let Some(c) = self.cache.creds.borrow().as_ref() { if c.expires_at_ms > now + 300_000 { return Ok(c.clone()); } }
        let signing = self.identity().await?.signing;
        let secs = now / 1000;
        let token = signing.jwt(&serde_json::json!({ "iss": plane_url, "sub": format!("plane:{}", self.config.plane_id), "aud": AUDIENCE, "iat": secs, "exp": secs + 300 }))?;
        let creds = aws::assume_role_with_web_identity(self.http, &a.region, &a.role_arn, &token, &format!("superci-{}", self.config.plane_id), now).await?;
        self.cache.creds.replace(Some(creds.clone()));
        Ok(creds)
    }

    async fn installation_token(&self, app: &App, installation_id: u64) -> Result<String> {
        let now = self.clock.now_ms();
        if let Some((t, until)) = self.cache.tokens.borrow().get(&installation_id) { if *until > now { return Ok(t.clone()); } }
        let t = github::installation_token(self.http, app, installation_id, now).await?;
        self.cache.tokens.borrow_mut().insert(installation_id, (t.clone(), now + 45 * 60_000));
        Ok(t)
    }

    async fn webhook(&self, req: &Request, plane_url: &str) -> Result<Response> {
        if self.config.app.is_none() { return Ok(Response::text(401, "no app")) }
        // Signed by one of this control plane's Apps (each organization has its own): the event is that one's.
        let Some(app) = self.config.apps().find(|a| verify_hub_signature(&a.webhook_secret, &req.body, req.header("x-hub-signature-256"))) else { return Ok(Response::text(401, "bad signature")) };
        if req.header("x-github-event") != Some("workflow_job") { return Ok(Response::text(202, "ignored")); }
        let payload: serde_json::Value = serde_json::from_slice(&req.body).map_err(|e| e.to_string())?;
        // The sweep keeps running after the first webhook (it asks for failed deliveries again).
        self.timer.wake_in(300_000).await?;
        let Some(ev) = github::our_job(&payload, &self.config.label, &app.owner) else { return Ok(Response::text(202, "not ours")) };
        match ev.action.as_str() {
            "queued" => self.launch(plane_url, &ev, app).await?,
            "in_progress" => self.started(plane_url, ev.job_id, ev.runner_name.as_deref()).await?,
            "completed" => self.completed(plane_url, &ev).await?,
            _ => {}
        }
        Ok(Response::text(202, "ok"))
    }

    /// A GitLab project's job event: a pending job with this control plane's label in its tags gets a machine; later
    /// events say when it began and ended (the machine ends by itself after one job).
    async fn gitlab_webhook(&self, req: &Request, plane_url: &str) -> Result<Response> {
        if self.config.gitlabs().next().is_none() { return Ok(nothing_here()) }
        // Which connection it is from: the one whose secret it carries.
        let Some(gl) = self.gitlab_signed(req) else { return Ok(Response::text(401, "bad token")) };
        put_json(self.store, &last_event_key(&gl.id), &self.clock.now_ms()).await?;
        let Some(ev) = gitlab::job_event(&req.body) else { return Ok(Response::text(200, "ignored")) };
        self.timer.wake_in(300_000).await?;
        let key = format!("job:gl{}", gitlab::local(&gl.id, ev.job_id));
        // A project's path, told apart across connections (for the note that a job is run again).
        let project = if gl.id.is_empty() { ev.project.clone() } else { format!("{}:{}", gl.id, ev.project) };
        match ev.status.as_str() {
            "pending" => {
                if self.store.get(&key).await?.is_some() { return Ok(Response::text(200, "known")) }
                let (tags, status) = gitlab::job(self.http, gl, ev.project_id, ev.job_id).await?;
                if status != "pending" { return Ok(Response::text(200, "not pending")) }
                let ours: Vec<&String> = tags.iter().filter(|t| Spec::parse(t, &self.config.label).is_some()).collect();
                let [label] = ours.as_slice() else { return Ok(Response::text(200, "not ours")) };
                let mut job = Job { job_id: ev.job_id, run_id: ev.pipeline_id, repo: ev.project.clone(), state: "launching".into(), at_ms: self.clock.now_ms(), label: label.to_string(),
                    provider: "gitlab".into(), gitlab: gl.id.clone(), project_id: Some(ev.project_id), tags: tags.clone(), name: ev.name.clone(), workflow: ev.stage.clone(), ..Default::default() };
                // Claimed once, however many deliveries arrive.
                if !self.store.put_if_absent(&key, serde_json::to_string(&job).map_err(|e| e.to_string())?).await? { return Ok(Response::text(200, "known")) }
                // Run again after AWS took its spot machine back: not on a spot machine this time.
                let again = rerun_key(&project, ev.pipeline_id, &ev.name);
                if self.store.get(&again).await?.is_some() {
                    job.no_spot = true;
                    self.store.delete(&again).await?;
                }
                self.start(plane_url, &mut job).await?;
            }
            "running" | "success" | "failed" | "canceled" | "skipped" => {
                // The machine it runs on is its runner's, and a runner takes any job with its tags: the record that
                // follows the machine is the one of the job the runner was made for (as with GitHub's runners).
                let made_for = match ev.runner_id { Some(r) => get_json::<serde_json::Value>(self.store, &format!("glrunner:{}", gitlab::local(&gl.id, r))).await?.map(|v| (r, v["job"].as_u64())), None => None };
                let machine_key = made_for.and_then(|(_, job)| job).map(|id| format!("job:gl{}", gitlab::local(&gl.id, id))).unwrap_or(key.clone());
                let now = self.clock.now_ms();
                if ev.status == "running" {
                    // The runner that took it takes no other (a job that read its token gets nothing more with it).
                    let Some((r, _)) = made_for else { return Ok(Response::text(200, "not on a runner of ours")) };
                    let _ = gitlab::pause_runner(self.http, gl, r).await;
                    if let Some(mut j) = get_json::<Job>(self.store, &machine_key).await? {
                        // Another job than the runner's own: this job's machine, still coming, is for the runner's
                        // job now (or for whichever job the runner's machine was for).
                        if machine_key != key {
                            if let Some(mut own) = get_json::<Job>(self.store, &key).await?.filter(|o| active(o) && !o.seen_in_progress) {
                                own.for_job = Some(j.for_job.unwrap_or(j.job_id));
                                put_json(self.store, &key, &own).await?;
                            }
                        }
                        j.for_job = None;
                        j.state = "running".into();
                        j.seen_in_progress = true;
                        j.started_ms.get_or_insert(now);
                        put_json(self.store, &machine_key, &j).await?;
                    }
                    return Ok(Response::text(200, "ok"));
                }
                if let Some((r, _)) = made_for {
                    // Its machine ends by itself after one job; its runner is removed.
                    if gitlab::delete_runner(self.http, gl, r).await.is_ok() { self.store.delete(&format!("glrunner:{}", gitlab::local(&gl.id, r))).await?; }
                    if let Some(mut j) = get_json::<Job>(self.store, &machine_key).await? {
                        j.state = match ev.status.as_str() { "success" => "done", "failed" => "failed", _ => "cancelled" }.into();
                        j.ended_ms.get_or_insert(now);
                        // AWS took its spot machine back: the job it was running is run again (GitLab makes a new
                        // job of it at once), at the next place in the order below AWS's spot machines.
                        if j.interrupted && ev.status == "failed" && j.rerun.is_none() {
                            put_json(self.store, &rerun_key(&project, ev.pipeline_id, &ev.name), &now).await?;
                            let asked = gitlab::retry_job(self.http, gl, ev.project_id, ev.job_id).await;
                            if asked.is_err() { self.store.delete(&rerun_key(&project, ev.pipeline_id, &ev.name)).await?; }
                            j.error = Some(match &asked { Ok(()) => "AWS took back its spot machine; run again, off spot machines".to_string(), Err(e) => format!("AWS took back its spot machine; not run again: {e}").chars().take(300).collect() });
                            j.rerun = Some(Rerun { job_id: ev.job_id, run_id: ev.pipeline_id, repo: ev.project.clone(), name: ev.name.clone(), state: if asked.is_ok() { "asked".into() } else { "not run again".into() }, at_ms: now });
                        }
                        self.settle(plane_url, &mut j).await;
                        put_json(self.store, &machine_key, &j).await?;
                    }
                }
                // Its own record, when it ran elsewhere or nowhere: cancelled while it waited for room, or its machine
                // left idle (ended by the sweep, unless it takes another job first).
                if machine_key != key || made_for.is_none() {
                    if let Some(mut own) = get_json::<Job>(self.store, &key).await? {
                        if own.state == "waiting" {
                            own.state = "cancelled".into();
                            own.ended_ms = Some(now);
                            put_json(self.store, &key, &own).await?;
                        } else if own.state == "launched" && !own.seen_in_progress {
                            match made_for.and_then(|(_, job)| job) {
                                Some(runners) => { own.over = true; own.for_job.get_or_insert(runners); }
                                None if own.over => {}
                                None => own.state = "orphan".into(),
                            }
                            put_json(self.store, &key, &own).await?;
                            self.timer.wake_in(60_000).await?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(Response::text(200, "ok"))
    }

    /// A GitLab runner of the job's own (GitLab has none for exactly one job): its token goes to the machine only; the
    /// runner is kept by id until it is removed, after its job or once its machine's time is up.
    async fn gitlab_runner(&self, gl: &GitLab, job: &mut Job) -> Result<String> {
        let mut tags = job.tags.clone();
        tags.sort();
        let (id, token) = gitlab::create_runner(self.http, gl, job.project_id.ok_or("a GitLab job without its project")?, &tags, &self.config.plane_id, job.job_id).await?;
        put_json(self.store, &format!("glrunner:{}", gitlab::local(&gl.id, id)), &serde_json::json!({ "id": id, "job": job.job_id, "gitlab": gl.id, "at_ms": self.clock.now_ms() })).await?;
        job.runner_id = Some(id);
        Ok(token)
    }

    /// Withdraws a job's runner registration, on GitHub or GitLab.
    async fn withdraw_runner(&self, j: &Job) {
        let Some(rid) = j.runner_id else { return };
        if j.provider == "gitlab" {
            if let Some(gl) = self.gitlab_of(j) { if gitlab::delete_runner(self.http, gl, rid).await.is_ok() { let _ = self.store.delete(&format!("glrunner:{}", gitlab::local(&gl.id, rid))).await; } }
        } else if let Some(app) = self.app_of(j) {
            if let Ok(t) = self.installation_token(app, j.installation_id).await { let _ = github::delete_runner(self.http, &app.api(), &t, &Self::owner_of(app), &j.repo, rid).await; }
            if let Some(name) = &j.runner { let _ = self.store.delete(&format!("runner:{name}")).await; }
        }
    }

    /// GitLab runners past their machine's time (and ones from before each job had its own, kept per tag set): removed.
    async fn sweep_gitlab_runners(&self, now: u64) -> Result<()> {
        if self.config.gitlabs().next().is_none() { return Ok(()) }
        for (key, v) in self.store.list("glrunner:").await? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&v) else { continue };
            if v["at_ms"].as_u64().is_some_and(|at| now.saturating_sub(at) < (MAX_JOB_MINUTES as u64 + 10) * 60_000) { continue }
            // Removed at the connection it was made through; one whose connection is gone is only forgotten here.
            if let (Some(id), Some(gl)) = (v["id"].as_u64(), self.gitlab_by(v["gitlab"].as_str().unwrap_or_default())) { if gitlab::delete_runner(self.http, gl, id).await.is_err() { continue } }
            self.store.delete(&key).await?;
        }
        Ok(())
    }

    /// The AWS account machines run in: this control plane's own when it runs in AWS, else the one connected by role.
    async fn aws_connection(&self) -> Result<Option<AwsConnection>> {
        if let Some((account_id, region)) = &self.config.aws_own {
            if self.config.aws_runners_off { return Ok(None) }
            return Ok(Some(AwsConnection { account_id: account_id.clone(), region: region.clone(), role_arn: String::new(), connected: true, error: None }));
        }
        get_json::<AwsConnection>(self.store, "aws").await
    }

    /// The regions machines go to in AWS, in order (the dashboard's list, else the connection's region).
    fn aws_regions(&self, a: &AwsConnection) -> Vec<String> {
        if self.config.aws_regions.is_empty() { vec![a.region.clone()] } else { self.config.aws_regions.clone() }
    }

    /// The current machine image in a region for a machine (Linux x64 or arm64, Linux with NVIDIA's drivers for GPUs,
    /// Windows), with its disk's size (looked up at most every six hours).
    async fn image(&self, region: &str, spec: &Spec, creds: &Credentials, now: u64) -> Result<(String, u32)> {
        let base = &self.config.image_name;
        let pattern = if spec.os() == "windows" { base.replace("-ubuntu24-full-", "-windows25-full-") }
            else if spec.gpu.is_some() { base.replace("-ubuntu24-full-", "-ubuntu24-gpu-") }
            else if spec.arch() == "arm64" { base.replace("-x64-", "-arm64-") } else { base.clone() };
        let key = format!("{region}/{pattern}");
        if let Some((id, _)) = self.cache.image.borrow().get(&key).filter(|(_, until)| *until > now) { if let Some((id, gb)) = id.split_once(' ') { return Ok((id.to_string(), gb.parse().unwrap_or(0))) } }
        let (id, gb) = aws::latest_image_sized(self.http, region, creds, &self.config.image_owner, &pattern, now).await?;
        self.cache.image.borrow_mut().insert(key, (format!("{id} {gb}"), now + 6 * 3_600_000));
        Ok((id, gb))
    }

    /// Whether spot is paused now: AWS took back two machines within a quarter of an hour, less than half an hour ago.
    async fn spot_paused(&self, now: u64) -> bool {
        let mut times: Vec<u64> = self.store.list("spot:interrupted:").await.unwrap_or_default().into_iter().filter_map(|(_, v)| v.parse().ok()).collect();
        times.sort();
        let (count, within, pause) = SPOT_PAUSE;
        times.windows(count).any(|w| w[count - 1].saturating_sub(w[0]) <= within && now.saturating_sub(w[count - 1]) < pause)
    }

    /// A job whose spot machine AWS took back, its run finished: run again (GitHub makes a new attempt of that job and
    /// of those that need it), noted so the new attempt is not on a spot machine again. Whether it is settled.
    async fn run_again(&self, j: &mut Job, now: u64) -> bool {
        let Some(mut r) = j.rerun.clone().filter(|r| r.state == "pending") else { return true };
        let settled = async {
            if now.saturating_sub(r.at_ms) > 6 * 3_600_000 { return Some("not run again: its run was still going six hours later".to_string()) }
            // GitHub not answering now (a token, the run's status): asked again the next minute.
            let token = self.installation_token_for(j).await?;
            let api = self.app_of(j)?.api();
            match github::run_status(self.http, &api, &token, &r.repo, r.run_id).await {
                Ok(s) if s == "completed" => {}
                Err(e) if e.contains(" 404 ") => return Some("not run again: its run is gone".into()),
                _ => return None,
            }
            // The other jobs of this run that wait to be run again: GitHub takes one request per attempt, so they
            // are asked for together (the run's failed jobs). Each is asked for once, whoever gets here first.
            let others: Vec<Rerun> = self.store.list("job:").await.unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_str::<Job>(&v).ok()).filter_map(|o| o.rerun)
                .filter(|o| o.state == "pending" && o.repo == r.repo && o.run_id == r.run_id && o.job_id != r.job_id).collect();
            let claim = |id: u64| format!("rerun-asked:{id}");
            match self.store.put_if_absent(&claim(r.job_id), now.to_string()).await { Ok(true) => {} Ok(false) => return Some("asked".into()), Err(_) => return None }
            for o in &others { let _ = self.store.put_if_absent(&claim(o.job_id), now.to_string()).await; }
            for name in std::iter::once(&r.name).chain(others.iter().map(|o| &o.name)) { let _ = put_json(self.store, &rerun_key(&r.repo, r.run_id, name), &now).await; }
            let asked = if others.is_empty() { github::rerun_job(self.http, &api, &token, &r.repo, r.job_id).await } else { github::rerun_failed_jobs(self.http, &api, &token, &r.repo, r.run_id).await };
            let Err(e) = asked else { return Some("asked".into()) };
            for name in std::iter::once(&r.name).chain(others.iter().map(|o| &o.name)) { let _ = self.store.delete(&rerun_key(&r.repo, r.run_id, name)).await; }
            // Refused for good (the App may not, or GitHub will not run it again): said. Anything else: tried again.
            if e.contains("Resource not accessible") { return Some("not run again: GitHub's App may not (Actions: write; see Control plane → Permissions)".into()) }
            if [" 403 ", " 404 ", " 409 ", " 422 "].iter().any(|c| e.contains(c)) { return Some(format!("not run again: {e}")) }
            for id in std::iter::once(r.job_id).chain(others.iter().map(|o| o.job_id)) { let _ = self.store.delete(&claim(id)).await; }
            None
        }.await;
        let Some(state) = settled else { return false };
        j.error = Some(if state == "asked" { "AWS took back its spot machine; run again, off spot machines".into() } else { format!("AWS took back its spot machine; {state}") });
        r.state = state;
        j.rerun = Some(r);
        true
    }

    /// Whether other jobs' GPU machines are up in AWS now (then a quota refusal means it is full, not that it is 0).
    async fn gpu_machines_up(&self, job_id: u64) -> bool {
        let jobs: Vec<Job> = self.store.list("job:").await.unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
        jobs.iter().any(|j| j.job_id != job_id && j.cloud == "aws" && active(j) && Spec::parse(&j.label, &self.config.label).and_then(|s| s.ok()).is_some_and(|s| s.gpu.is_some()))
    }

    /// Its own network in an AWS region (made by the dashboard), looked up at most every ten minutes; without one
    /// (not made yet, or a role from before it could read networks), machines start in the region's default network.
    /// A region with a network of the account's own set in the dashboard uses that one (looked up the same way), or
    /// fails saying what is wrong with it: its machines never start anywhere else. AWS not answering is an error too
    /// (the job is tried again), not a reason to use the default network.
    async fn network(&self, region: &str, creds: &Credentials, now: u64) -> Result<Option<aws::Network>> {
        // Kept by region and by what is set for it (a setting changed in the dashboard is used at once).
        let key = match self.config.aws_networks.get(region) { Some(g) => format!("{region} {}", serde_json::to_string(g).unwrap_or_default()), None => region.to_string() };
        if let Some((n, _)) = self.cache.network.borrow().get(&key).filter(|(_, until)| *until > now) { return Ok(n.clone()) }
        let found = match self.config.aws_networks.get(region) {
            Some(given) => aws::given_network(self.http, region, creds, given, now).await.map(Some),
            None => aws::find_network(self.http, region, creds, &self.config.plane_id, now).await,
        };
        self.note_permission("aws", "ReadNetwork", &found).await;
        let n = match found {
            Ok(n) => n,
            Err(e) if !self.config.aws_networks.contains_key(region) && ["AccessDenied", "UnauthorizedOperation", "not authorized"].iter().any(|d| e.contains(d)) => None,
            Err(e) => return Err(e),
        };
        self.cache.network.borrow_mut().insert(key, (n.clone(), now + if n.is_some() { 600_000 } else { 120_000 }));
        Ok(n)
    }

    /// Whether this control plane starts Cloudflare containers itself.
    fn own_containers(&self) -> bool { self.config.containers && self.containers.is_some() }

    /// Where a job's machine may go, by the order and limits set in the dashboard (see `Routing`): the places that can
    /// run it now, first to last. The job goes to the first; to the next when that one cannot start its machine.
    /// `no_spot`: AWS's spot machines are passed over when another place can run it (see `Job::no_spot`, `spot_paused`).
    async fn place(&self, repo: &str, spec: &Spec, gitlab: bool, no_spot: bool, failed_at: &[String]) -> Result<Placement> {
        let aws = self.aws_connection().await?.filter(|a| a.connected);
        let routing = &self.config.routing;
        let mut connected = vec![];
        if aws.is_some() { connected.extend(["aws".to_string(), AWS_ON_DEMAND.to_string()]) }
        if self.own_containers() { connected.push(self.config.own_cloud.clone()) }
        for a in &self.config.agents { if !connected.contains(&a.cloud) { connected.push(a.cloud.clone()) } }
        // The order as set; places connected since then come after it (AWS's on-demand machines: right after its spot ones).
        let mut order: Vec<String> = routing.order.iter().map(|p| p.cloud.clone()).collect();
        for c in &connected {
            if order.contains(c) { continue }
            match order.iter().position(|o| o == "aws") { Some(at) if c == AWS_ON_DEMAND => order.insert(at + 1, c.clone()), _ => order.push(c.clone()) }
        }
        // A label or a rule naming AWS means both of its places, as the order has them.
        let named = |c: &str| -> Vec<String> { if c == "aws" { order.iter().filter(|o| *o == "aws" || *o == AWS_ON_DEMAND).cloned().collect() } else { vec![c.to_string()] } };
        let candidates: Vec<String> = if let Some(c) = &spec.cloud { let v = named(c); if v.is_empty() { vec![c.clone()] } else { v } }
            else if let Some(c) = routing.rule_for(repo) { let v = named(c); if v.is_empty() { vec![c.to_string()] } else { v } }
            else if !routing.order.is_empty() { order.clone() }
            else if let Some(d) = &routing.default { let v = named(d); if v.is_empty() { vec![d.clone()] } else { v } }
            else { connected };
        if candidates.is_empty() { return Ok(Placement::Fail("no cloud connected for jobs: connect one in the SuperCI dashboard".into())) }
        let jobs: Vec<Job> = self.store.list("job:").await?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
        let spend = self.month_spend(&jobs).await;
        let (mut later, mut never, mut places) = (vec![], vec![], vec![]);
        for cloud in &candidates {
            let name = cloud_name(cloud);
            let runner = match cloud.as_str() {
                "aws" | AWS_ON_DEMAND => aws.clone().map(|a| (Capacity::aws(), Runner::Aws(a))),
                c if c == self.config.own_cloud && self.own_containers() => Some((capacity_of(c), Runner::Own)),
                c => self.config.agents.iter().find(|a| a.cloud == c).map(|a| (if c == "cloudflare" { Capacity::cloudflare() } else { Capacity::modal() }, Runner::Agent(a.clone()))),
            };
            // Limits from the order: turned off, jobs at once, spend this month.
            let pool = routing.pool(cloud);
            if pool.is_some_and(|p| p.off) { never.push(format!("{name} is turned off")); continue }
            if let Some(cap) = pool.and_then(|p| p.monthly_usd) {
                if spend.get(cloud).copied().unwrap_or(0.0) >= cap { never.push(format!("{name} reached its ${cap:.0} a month")); continue }
            }
            if let Some(max) = pool.and_then(|p| p.max_jobs).or(Some(if cloud == "cloudflare" { CLOUDFLARE_JOBS_AT_ONCE } else { JOBS_AT_ONCE })) {
                if jobs.iter().filter(|j| place_of(j) == cloud && active(j)).count() as u32 >= max { later.push(format!("{name} runs {max} at once")); continue }
            }
            // A label that names this provider, and it is not added here: said as it is.
            let Some((capacity, runner)) = runner else {
                if spec.cloud.is_some() { return Ok(Placement::Fail(format!("its label needs {} runners, which this control plane has not added", cloud_name(spec.cloud.as_deref().unwrap_or(cloud))))) }
                never.push(format!("{name} is not connected"));
                continue
            };
            if gitlab && cloud == "modal" { never.push("Modal runs no Docker (GitLab jobs run in Docker)".into()); continue }
            if gitlab && spec.os() == "windows" { never.push(format!("{name} runs no GitLab jobs on Windows")); continue }
            if let Some(why) = capacity.refuses(spec) { never.push(format!("{name} runs {why}")); continue }
            // Its label asks for an on-demand machine: not AWS's spot ones.
            if cloud == "aws" && spec.on_demand { never.push("AWS spot machines are not on-demand".into()); continue }
            places.push((cloud.clone(), runner));
        }
        // Spot is passed over only when something else can take the job: with nothing else, a spot machine it is.
        if no_spot && places.iter().any(|(c, _)| c != "aws") { places.retain(|(c, _)| c != "aws") }
        // Places that failed this job before come after those that have not (still in order among themselves).
        places.sort_by_key(|(c, _)| failed_at.contains(c));
        Ok(if !places.is_empty() { Placement::Run(places) }
            else if !later.is_empty() { Placement::Wait(format!("waiting: {}", later.join("; "))) }
            else { Placement::Fail(format!("nowhere to run {}: {}", spec.describe(), never.join("; "))) })
    }

    /// One call to a runner agent, with a token this control plane signs for it alone.
    async fn agent_call(&self, plane_url: &str, a: &Agent, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let secs = self.clock.now_ms() / 1000;
        let token = self.identity().await?.signing.jwt(&serde_json::json!({ "iss": plane_url, "sub": format!("plane:{}", self.config.plane_id), "aud": a.url, "iat": secs, "exp": secs + 120 }))?;
        let req = Request::new("POST", &format!("{}{path}", a.url)).with_header("authorization", &format!("Bearer {token}"))
            .with_header("content-type", "application/json").with_body(body.to_string());
        let r = self.http.send(req).await?;
        if r.status >= 300 {
            // What it said (FastAPI's `detail`), else its answer as it came.
            let said = serde_json::from_slice::<serde_json::Value>(&r.body).ok().and_then(|v| v["detail"].as_str().map(str::to_string)).unwrap_or_else(|| r.body_text());
            return Err(format!("{} runner agent: {} {}", a.cloud, r.status, said.chars().take(300).collect::<String>()));
        }
        serde_json::from_slice(&r.body).map_err(|e| format!("{} runner agent: {e}", a.cloud))
    }

    /// Ends the machine that ran (or would have run) a job, wherever it is.
    /// An AWS machine's cost once it ended, at the prices AWS billed: its zone's spot prices over its time (or its
    /// on-demand price), its disk and its public address. Left as an estimate if AWS cannot say.
    async fn settle(&self, plane_url: &str, j: &mut Job) {
        if j.cloud != "aws" || j.cost_usd.is_some() { return }
        let (Some(region), Some(kind), Some(end)) = (j.region.clone(), j.machine_type.clone(), j.ended_ms) else { return };
        let Ok(Some(a)) = self.aws_connection().await else { return };
        let Ok(creds) = self.aws_creds(plane_url, &a).await else { return };
        let (from, now) = (j.launched_ms.unwrap_or(j.at_ms), self.clock.now_ms());
        let hours = end.saturating_sub(from).max(60_000) as f64 / 3_600_000.0;
        let spec = Spec::parse(&j.label, &self.config.label).and_then(|s| s.ok()).map(|s| s.or(&self.config.machine)).unwrap_or_default();
        let machine = match j.zone.as_deref() {
            Some(zone) if !(spec.on_demand || j.on_demand) => aws::spot_cost(self.http, &region, zone, &creds, &kind, spec.os(), from, end, now).await,
            _ => { let p = aws::on_demand_price(self.http, &creds, &region, &kind, spec.os(), now).await; self.note_permission("aws", "Prices", &p).await; p.map(|p| p * hours) }
        };
        let disk = aws::disk_price(self.http, &creds, &region, now).await;
        self.note_permission("aws", "Prices", &disk).await;
        let gb_month = disk.unwrap_or_else(|_| aws::disk_gb_month(&region));
        if let Ok(machine) = machine {
            j.cost_usd = Some(machine + aws::extras_usd_per_hour(gb_month, j.disk_gb.unwrap_or(60), !j.private) * hours);
            j.cost_from = Some("prices".into());
        }
    }

    /// Adds what an AWS machine sent to its cost, at AWS's price for data sent out (an upper bound: CloudWatch counts
    /// traffic within the region too). Whether it could.
    async fn settle_sent(&self, plane_url: &str, j: &mut Job) -> bool {
        let (Some(region), Some(id), Some(end)) = (j.region.clone(), j.machine_id.clone(), j.ended_ms) else { return false };
        let Ok(Some(a)) = self.aws_connection().await else { return false };
        let Ok(creds) = self.aws_creds(plane_url, &a).await else { return false };
        let sent = aws::bytes_sent(self.http, &region, &creds, &id, j.launched_ms.unwrap_or(j.at_ms), end, self.clock.now_ms()).await;
        self.note_permission("aws", "MachineTraffic", &sent).await;
        let bytes = match sent {
            Ok(b) => b,
            // A role from before it could read CloudWatch: not known (-1), not asked again.
            Err(e) if e.contains("AccessDenied") => { j.sent_gb = Some(-1.0); return true }
            Err(_) => return false,
        };
        let gb = bytes / 1e9;
        j.sent_gb = Some(gb);
        j.cost_usd = Some(j.cost_usd.unwrap_or(0.0) + gb * aws::DATA_OUT_USD_PER_GB);
        true
    }

    async fn stop_machine(&self, plane_url: &str, j: &Job) {
        let Some(id) = &j.machine_id else { return };
        if j.cloud == "aws" {
            if let Ok(Some(a)) = self.aws_connection().await {
                let region = j.region.clone().unwrap_or(a.region.clone());
                if let Ok(c) = self.aws_creds(plane_url, &a).await { let _ = aws::terminate_instance(self.http, &region, &c, id, self.clock.now_ms()).await; }
            }
        } else if j.cloud == self.config.own_cloud && self.own_containers() {
            if let Some(c) = self.containers { let _ = c.stop(id).await; }
        } else if let Some(agent) = self.config.agents.iter().find(|a| a.cloud == j.cloud) {
            let _ = self.agent_call(plane_url, agent, "/stop", serde_json::json!({ "id": id })).await;
        }
    }

    /// A queued job: claimed once, then started where the order says (or left waiting for room).
    async fn launch(&self, plane_url: &str, ev: &github::JobEvent, app: &App) -> Result<()> {
        let key = format!("job:{}", ev.job_id);
        let mut job = Job { job_id: ev.job_id, run_id: ev.run_id, repo: ev.repo.clone(), state: "launching".into(), at_ms: self.clock.now_ms(), installation_id: ev.installation_id, label: ev.label.clone(),
            name: ev.name.clone(), workflow: ev.workflow.clone(), app_id: Some(app.id), ..Default::default() };
        // Claimed once, however many deliveries of the event arrive at once. A claim that never got past launching (the
        // runtime restarted mid-way) is taken over by a later delivery.
        if !self.store.put_if_absent(&key, serde_json::to_string(&job).map_err(|e| e.to_string())?).await? {
            let stuck = get_json::<Job>(self.store, &key).await?.is_some_and(|j| j.state == "launching" && job.at_ms.saturating_sub(j.at_ms) > 90_000);
            if !stuck { return Ok(()); }
            put_json(self.store, &key, &job).await?;
        }
        // A job run again after AWS took its spot machine back: not on a spot machine this time.
        let again = rerun_key(&ev.repo, ev.run_id, &ev.name);
        if self.store.get(&again).await?.is_some() {
            job.no_spot = true;
            self.store.delete(&again).await?;
            put_json(self.store, &key, &job).await?;
        }
        // A public repository: anyone can open a pull request there. Its jobs run only if the dashboard allows the
        // repository, and never for a run from outside it (a fork's pull request, pull_request_target). In doubt, not.
        if ev.public {
            let refused = if !self.config.routing.allows_public(&ev.repo) {
                Some("a public repository: allow it in the dashboard (Workflows) for its jobs to run on your clouds".to_string())
            } else {
                let outside = match self.installation_token(app, ev.installation_id).await {
                    Ok(token) => github::run_from_outside(self.http, &app.api(), &token, &ev.repo, ev.run_id).await,
                    Err(e) => Err(e),
                };
                match outside {
                    Ok(None) => None,
                    Ok(Some(what)) => Some(format!("{what} in a public repository: not run on your clouds")),
                    Err(e) => Some(format!("could not check where its run came from ({e}): not run on your clouds")),
                }
            };
            if let Some(why) = refused {
                job.state = "failed".into();
                job.error = Some(why);
                job.ended_ms = Some(job.at_ms);
                return put_json(self.store, &key, &job).await;
            }
        }
        self.start(plane_url, &mut job).await
    }

    /// One machine for a job, of the size the label asks for, in the first place in the order that can run it now, with
    /// its work: a just-in-time GitHub runner registration (under the job's own label), or a GitLab project runner for
    /// the job's tags.
    async fn start(&self, plane_url: &str, job: &mut Job) -> Result<()> {
        let key = job_key(job);
        let gitlab = job.provider == "gitlab";
        let label = if job.label.is_empty() { self.config.label.clone() } else { job.label.clone() };
        let spec = match Spec::parse(&label, &self.config.label) {
            Some(Ok(s)) => Ok(s.or(&self.config.machine)),
            Some(Err(e)) => Err(e),
            None => Err(format!("{label} is not this control plane's label")),
        };
        // Where it goes is decided again each time.
        job.cloud = String::new();
        // Whether it failed for what its label asks (not for its repository, nor for a cloud's hiccup): then any job with
        // that label fails here alike, and a failing runner can take it (see `fail_fast`).
        let mut intrinsic = false;
        let result: Result<()> = async {
            let spec = match spec { Ok(s) => s, Err(e) => { intrinsic = true; return Err(e) } };
            if let Some(why) = self.config.routing.too_large(&spec) { intrinsic = true; return Err(why) }
            let now = self.clock.now_ms();
            let places = match self.place(&job.repo, &spec, gitlab, job.no_spot || self.spot_paused(now).await, &job.failed_at).await? {
                Placement::Run(p) => p,
                Placement::Wait(why) => {
                    job.state = "waiting".into();
                    job.error = Some(why);
                    put_json(self.store, &key, &*job).await?;
                    return self.timer.wake_in(20_000).await;
                }
                Placement::Fail(why) => { intrinsic = spec.cloud.is_some() || self.config.routing.rule_for(&job.repo).is_none(); return Err(why) }
            };
            let cloud_of = |r: &Runner| match r { Runner::Aws(_) => "aws".to_string(), Runner::Agent(a) => a.cloud.clone(), Runner::Own => self.config.own_cloud.clone() };
            job.state = "launching".into();
            job.error = None;
            job.cloud = cloud_of(&places[0].1);
            job.on_demand = places[0].0 == AWS_ON_DEMAND;
            let name = format!("superci-{}-{}{}{}", self.config.plane_id, if gitlab { "gl" } else { "" }, if gitlab { gitlab::local(&job.gitlab, job.job_id) } else { job.job_id.to_string() }, job.runner_suffix);
            let work = if gitlab {
                let gl = self.gitlab_of(job).ok_or("its GitLab is not connected any more")?;
                let token = self.gitlab_runner(gl, job).await?;
                job.runner = Some(name.clone());
                put_json(self.store, &key, &*job).await?;
                Work::GitLab { url: gl.url.clone(), token }
            } else {
                let app = self.app_of(job).ok_or("no GitHub App")?;
                let owner = Self::owner_of(app);
                let token = self.installation_token(app, job.installation_id).await?;
                // Where GitHub's own runners check out code (Windows: the runner's own folder).
                let folder = if spec.os() == "windows" { "_work" } else { "/home/runner/work" };
                let (jit, runner_id) = github::jit_config(self.http, &app.api(), &token, &owner, &job.repo, &name, &[&label], folder).await?;
                job.runner = Some(name.clone());
                job.runner_id = Some(runner_id);
                put_json(self.store, &key, &*job).await?;
                put_json(self.store, &format!("runner:{name}"), &job.job_id).await?;
                Work::GitHub { jit }
            };
            // The places that can run it, in order. The job goes to the next when this one cannot start its machine now:
            // AWS has no spot machine for it, or starting it failed (a provider's hiccup, or its refusal).
            enum Tried { Started(String, String), NoRoom, OutOfTime }
            let (mut started, began) = (None, self.clock.now_ms());
            // What each region said when it had no room, and whether only for the account's limits (quota).
            let (mut last, mut quota_only, mut answers) = (String::from("no AWS region"), true, Vec::<String>::new());
            let (mut tried_spot, mut tried_on_demand, mut out_of_time) = (false, false, false);
            // The same, of on-demand machines alone: whether a region refused one, and only for the account's limits.
            let (mut on_demand_refused, mut on_demand_quota_only) = (false, true);
            // What each place said where starting the machine failed.
            let mut failures: Vec<(String, String)> = vec![];
            for (at, (place, runner)) in places.into_iter().enumerate() {
                job.cloud = cloud_of(&runner);
                job.on_demand = place == AWS_ON_DEMAND;
                // What only the place before had (a spot machine's notice, Cloudflare's price while waiting).
                job.notice = None;
                job.waiting_usd_per_hour = None;
                // A runtime answers one request only so long (AWS Lambda: 30 seconds): the rest is left for the job's
                // next try.
                if at > 0 && self.clock.now_ms().saturating_sub(began) > LAUNCH_BUDGET_MS { out_of_time = true; break }
                let tried: Result<Tried> = match runner {
                    Runner::Aws(a) => async {
                        let on_demand = place == AWS_ON_DEMAND;
                        if on_demand { tried_on_demand = true } else { tried_spot = true }
                        let creds = self.aws_creds(plane_url, &a).await?;
                        let tags = vec![("Name".to_string(), name.clone()), ("superci-plane".into(), self.config.plane_id.clone()), ("superci-job".into(), job.job_id.to_string()), ("superci-repo".into(), job.repo.clone())];
                        // The standard machine keeps the configured types; a sized one gets the types that fit it.
                        let types = if spec.cpu.is_none() && spec.ram_gb.is_none() && spec.arch() == "x64" && spec.gpu.is_none() { self.config.instance_types.clone() } else { aws_instance_types(&spec) };
                        let mut disk_gb = spec.disk_gb.unwrap_or(60);
                        job.cpu = Some(Capacity::aws().fit(&spec).map(|s| s.cpu).unwrap_or(4));
                        // A spot machine says when AWS takes it back, with a link only it has.
                        job.notice = (!on_demand).then(|| crate::crypto::random_token(18));
                        let notice = job.notice.as_ref().map(|t| if gitlab { format!("{plane_url}/interrupted?gl={}&t={t}", gitlab::local(&job.gitlab, job.job_id)) } else { format!("{plane_url}/interrupted?runner={name}&t={t}") });
                        let user_data = aws::runner_user_data_for(&work, MAX_JOB_MINUTES, spec.os(), notice.as_deref())?;
                        let mut launched = None;
                        // The regions in order: the next when this one has no room (capacity or quota) for any of the types.
                        for region in &self.aws_regions(&a) {
                            if self.clock.now_ms().saturating_sub(began) > LAUNCH_BUDGET_MS { return Ok(Tried::OutOfTime) }
                            let said = |what: &str| format!("{region}{}: {what}", if on_demand { " on-demand" } else { "" });
                            let (image, image_gb) = match self.image(region, &spec, &creds, now).await { Ok(i) => i, Err(e) => { last = said(&e); continue } };
                            // Never smaller than its image's disk (Windows' is 100 GB).
                            disk_gb = disk_gb.max(image_gb);
                            let network = self.network(region, &creds, now).await?;
                            let machine = aws::run_instance(self.http, &creds, &Launch { region, network: network.as_ref(), image: &image, types: &types, user_data: &user_data, disk_gb, tags: &tags, spot: !on_demand, os: spec.os() }, now).await;
                            self.note_permission("aws", "LaunchOnlyTaggedMachines", &machine).await;
                            match machine {
                                Ok(m) => { launched = Some((region.clone(), m, network.is_some_and(|n| n.private))); break }
                                Err(e) if aws::out_of_room(&e) => {
                                    let quota = ["VcpuLimitExceeded", "MaxSpotInstanceCountExceeded"].iter().any(|c| e.starts_with(c));
                                    quota_only &= quota;
                                    if on_demand { on_demand_refused = true; on_demand_quota_only &= quota }
                                    answers.push(said(e.split(':').next().unwrap_or(&e)));
                                    last = said(&e);
                                }
                                // A network that is no longer there (removed, made again): looked up anew next try.
                                Err(e) => { if e.starts_with("InvalidSubnet") || e.starts_with("InvalidGroup") || e.starts_with("InvalidSecurityGroup") { self.cache.network.borrow_mut().clear(); } return Err(e) }
                            }
                        }
                        let Some((region, machine, private)) = launched else {
                            // No region has room: what the regions said goes with the job, down the order.
                            if !answers.is_empty() { job.spot_refused = Some(answers.join("; ").chars().take(300).collect()) }
                            return Ok(Tried::NoRoom)
                        };
                        if !on_demand { job.spot_refused = None }
                        job.private = private;
                        // Spot: its zone's price now. On-demand: AWS's list price. Settled at the prices in effect when it ends.
                        let price = if on_demand { let p = aws::on_demand_price(self.http, &creds, &region, &machine.kind, spec.os(), now).await; self.note_permission("aws", "Prices", &p).await; p.ok() }
                            else { aws::spot_price(self.http, &region, Some(&machine.zone), &creds, &machine.kind, spec.os(), now).await.ok() };
                        let cpu = job.cpu.unwrap_or(4);
                        job.usd_per_hour = Some(price.unwrap_or_else(|| aws::on_demand_usd_per_hour(cpu, spec.ram_gb.unwrap_or(cpu * 4))) + aws::extras_usd_per_hour(aws::disk_gb_month(&region), disk_gb, !private));
                        job.region = Some(region);
                        job.zone = Some(machine.zone).filter(|z| !z.is_empty());
                        job.disk_gb = Some(disk_gb);
                        Ok(Tried::Started(machine.id, machine.kind))
                    }.await,
                    Runner::Own => async {
                        // Sized for the job (placement already checked it fits).
                        let size = capacity_of(&self.config.own_cloud).fit(&spec)?;
                        job.cpu = Some(size.cpu);
                        job.usd_per_hour = Some(if self.config.own_cloud == "modal" { modal_usd_per_hour(size.cpu, size.ram_gb) + modal_gpu_usd_per_hour(size.gpu) } else { cloudflare_usd_per_hour(size) });
                        if self.config.own_cloud == "cloudflare" { job.waiting_usd_per_hour = Some(cloudflare_waiting_usd_per_hour(size)) }
                        Ok(Tried::Started(self.containers.ok_or("no containers here")?.start(&name, &work, MAX_JOB_MINUTES, size).await?, machine_name(size)))
                    }.await,
                    Runner::Agent(a) => async {
                        let size = if a.cloud == "cloudflare" { Capacity::cloudflare() } else { Capacity::modal() }.fit(&spec)?;
                        job.cpu = Some(size.cpu);
                        job.usd_per_hour = match a.cloud.as_str() { "cloudflare" => Some(cloudflare_usd_per_hour(size)), "modal" => Some(modal_usd_per_hour(size.cpu, size.ram_gb) + modal_gpu_usd_per_hour(size.gpu)), _ => None };
                        if a.cloud == "cloudflare" { job.waiting_usd_per_hour = Some(cloudflare_waiting_usd_per_hour(size)) }
                        let mut body = serde_json::json!({ "name": name, "job": job.job_id, "repo": job.repo, "max_minutes": MAX_JOB_MINUTES, "cpu": size.cpu, "ram_gb": size.ram_gb, "disk_gb": size.disk_gb, "location": self.config.cloudflare_location });
                        if a.cloud == "cloudflare" { if let Some(i) = &self.config.cloudflare_image { body["image_url"] = i.clone().into() } }
                        if let Some(g) = size.gpu { body["gpu"] = g.to_uppercase().into() }
                        for (k, v) in work.json().as_object().into_iter().flatten() { body[k] = v.clone() }
                        let v = self.agent_call(plane_url, &a, "/launch", body).await?;
                        Ok(Tried::Started(v["id"].as_str().ok_or("runner agent answered without an id")?.to_string(), v["kind"].as_str().unwrap_or_default().to_string()))
                    }.await,
                };
                match tried {
                    Ok(Tried::Started(id, kind)) => { started = Some((id, kind)); break }
                    Ok(Tried::NoRoom) => {}
                    Ok(Tried::OutOfTime) => { out_of_time = true; break }
                    // Remembered with the job: its next try starts with the places that have not failed it.
                    Err(e) => {
                        if !job.failed_at.contains(&place) { job.failed_at.push(place.clone()) }
                        // AWS failing alike for its spot and its on-demand machines (its network, its permissions) is said once.
                        if !(place == AWS_ON_DEMAND && failures.iter().any(|(p, said)| p == "aws" && *said == e)) { failures.push((place, e)) }
                    }
                }
            }
            // Out of time with spot machines refused: the next try starts below them.
            if out_of_time {
                if !answers.is_empty() { job.no_spot = true; job.spot_refused = Some(answers.join("; ").chars().take(300).collect()) }
                last = format!("{last} (out of time for more tries)");
            }
            let (machine_id, machine_type) = match started {
                Some(m) => { job.failed_at.clear(); m }
                None => {
                    // An account allowed no GPU machines (AWS's quota for them starts at 0): no use waiting. Seen when
                    // every refusal was for the account's limits, or every on-demand one was (spot may have said
                    // something else: a type a zone does not have).
                    let no_quota = quota_only || (on_demand_refused && on_demand_quota_only);
                    if spec.gpu.is_some() && no_quota && !answers.is_empty() && !self.gpu_machines_up(job.job_id).await {
                        let (spot, od) = ("“All G and VT Spot Instance Requests” (https://console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-3819A6DF)", "“Running On-Demand G and VT instances” (https://console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-DB2E81BA)");
                        let why = format!("{NO_GPU_QUOTA}{}{}", match (tried_spot && quota_only, tried_on_demand) { (true, true) => format!("{spot} or {od}"), (true, false) => spot.to_string(), _ => od.to_string() }, " in each region it uses");
                        if failures.is_empty() { return Err(why) }
                        failures.push(("aws".into(), why));
                        answers.clear();
                    }
                    return Err(match failures.len() {
                        0 if out_of_time => last,
                        // Only spot machines were asked for, no region has one, and nothing else in the order can run
                        // it: it waits for one (see `WAIT_FOR_SPOT`).
                        0 if tried_spot && !tried_on_demand && !answers.is_empty() => format!("{WAIT_FOR_SPOT}{}", answers.join("; ")),
                        0 if answers.len() > 1 => format!("AWS had no room for it: {}", answers.join("; ")),
                        0 => last,
                        // One place, and it failed: what it said, as it said it.
                        1 if answers.is_empty() && !out_of_time => failures.remove(0).1,
                        _ => {
                            // Every place refused what the job asks (none a hiccup): it fails at once (see below).
                            if answers.is_empty() && !out_of_time && failures.iter().all(|(_, e)| refuses_alike(e)) { intrinsic = true; job.cloud = String::new() }
                            let mut said = vec![];
                            if !answers.is_empty() { said.push(format!("AWS had no room ({})", answers.join("; "))) }
                            said.extend(failures.iter().map(|(place, e)| format!("{}: {e}", cloud_name(place))));
                            if out_of_time { said.push("out of time for more tries".into()) }
                            format!("{SEVERAL}{}", said.join("; "))
                        }
                    })
                }
            };
            job.state = "launched".into();
            job.launched_ms = Some(self.clock.now_ms());
            job.machine_id = Some(machine_id);
            job.machine_type = Some(machine_type);
            put_json(self.store, &key, &*job).await?;
            self.timer.wake_in(60_000).await
        }.await;
        if let Err(e) = result {
            self.withdraw_runner(job).await;
            job.runner_id = None;
            // No GPU machines allowed in the AWS account, or a runner agent refusing what was asked (Modal: no GPUs
            // without a payment method): every job asking for it fails alike, at once.
            if !e.starts_with(SEVERAL) && refuses_alike(&e) { intrinsic = true; job.cloud = String::new() }
            // No spot machine anywhere and no other place for it: it waits for one, however long (as for a place that is full).
            if e.starts_with(WAIT_FOR_SPOT) {
                job.state = "waiting".into();
                job.error = Some(format!("{})", e.chars().take(280).collect::<String>()));
                put_json(self.store, &key, &*job).await?;
                return self.timer.wake_in(30_000).await;
            }
            // A place was picked and starting the machine failed (Cloudflare "temporarily unavailable", a spot shortage):
            // tried again, twice; GitHub would otherwise keep the job queued for a day. No place for it: failed at once.
            if !job.cloud.is_empty() && job.retries < 2 {
                job.retries += 1;
                job.state = "waiting".into();
                job.error = Some(format!("waiting: starting its machine failed, trying again ({})", e.chars().take(200).collect::<String>()));
                put_json(self.store, &key, &*job).await?;
                return self.timer.wake_in(20_000).await;
            }
            job.state = "failed".into();
            job.error = Some(e.chars().take(300).collect());
            job.ended_ms.get_or_insert(self.clock.now_ms());
            put_json(self.store, &key, &*job).await?;
            // It fails now at GitHub too, saying why, instead of waiting there a day: what its label asks that nothing
            // here can run, or a machine that would not start, three times. GitHub gives the failing runner any job with
            // the label, so not for what only its repository is refused (another repository's job could take it).
            if intrinsic || !job.cloud.is_empty() { self.fail_fast(plane_url, job, &e).await; }
        }
        Ok(())
    }

    /// A job nothing here can run (what its label asks: a machine no place has, like macOS, or a place not added): a
    /// small runner takes it and fails it before any step, saying why, rather than GitHub keeping it queued for
    /// a day. Only for what a label asks: GitHub gives a runner any job with its label, and every job with that label
    /// fails here alike. It runs where it costs least and starts soonest (Cloudflare, Modal, AWS), for ten minutes at
    /// most. A GitLab job is cancelled instead (GitLab can cancel one job).
    async fn fail_fast(&self, plane_url: &str, job: &mut Job, why: &str) {
        if job.provider == "gitlab" {
            if let (Some(gl), Some(project)) = (self.gitlab_of(job), job.project_id) { let _ = gitlab::cancel_job(self.http, gl, project, job.job_id).await; }
            return;
        }
        // Where it may run, cheapest and soonest first (no runner is registered at GitHub when there is no place for it).
        enum Place { Own, Agent(Agent), Aws(AwsConnection) }
        let mut places = vec![];
        if self.own_containers() { places.push(Place::Own) }
        for c in ["cloudflare", "modal"] { if let Some(a) = self.config.agents.iter().find(|a| a.cloud == c) { places.push(Place::Agent(a.clone())) } }
        if let Ok(Some(a)) = self.aws_connection().await.map(|a| a.filter(|a| a.connected)) { places.push(Place::Aws(a)) }
        if places.is_empty() { return }
        let Some(app) = self.app_of(job) else { return };
        let owner = Self::owner_of(app);
        let Ok(token) = self.installation_token(app, job.installation_id).await else { return };
        let label = if job.label.is_empty() { self.config.label.clone() } else { job.label.clone() };
        let name = format!("superci-{}-{}{FAILING}", self.config.plane_id, job.job_id);
        let Ok((jit, runner_id)) = github::jit_config(self.http, &app.api(), &token, &owner, &job.repo, &name, &[&label], "/home/runner/work").await else { return self.fail_later(job, why).await };
        // Known by its name, as a job's runner is: it may take another job with the label (see `failed_by_runner`).
        let _ = put_json(self.store, &format!("runner:{name}"), &job.job_id).await;
        // macOS (no provider here runs it yet): what to do, in so many words.
        let mac = Spec::parse(&label, &self.config.label).and_then(|s| s.ok()).is_some_and(|s| s.os() == "macos") && why.starts_with("nowhere to run");
        let line = if mac { "::error title=SuperCI could not run this job::SuperCI runs no macOS jobs yet. Use runs-on: macos-latest for GitHub's own.".to_string() }
            else { format!("::error title=SuperCI could not run this job::{}. Change its runs-on, or add a place that can run it in the SuperCI dashboard.", why.replace(['\r', '\n'], " ").trim_end_matches('.')) };
        let work = Work::Fail { jit, why: crate::crypto::b64(line.as_bytes()) };
        let size = Size { cpu: 1, ram_gb: 4, disk_gb: 8, gpu: None };
        // The first place that starts it (a container that would not start, a cloud's hiccup: the next).
        for place in places {
            let started: Result<()> = async {
                match place {
                    Place::Own => self.containers.ok_or("no containers")?.start(&name, &work, 10, size).await.map(|_| ()),
                    Place::Agent(a) => {
                        let mut body = serde_json::json!({ "name": name, "job": job.job_id, "repo": job.repo, "max_minutes": 10, "cpu": 1, "ram_gb": 4, "disk_gb": 8, "location": self.config.cloudflare_location });
                        for (k, v) in work.json().as_object().into_iter().flatten() { body[k] = v.clone() }
                        self.agent_call(plane_url, &a, "/launch", body).await.map(|_| ())
                    }
                    Place::Aws(a) => {
                        let creds = self.aws_creds(plane_url, &a).await?;
                        let region = self.aws_regions(&a).into_iter().next().ok_or("no AWS region")?;
                        let (image, image_gb) = self.image(&region, &Spec::default(), &creds, self.clock.now_ms()).await?;
                        let tags = vec![("Name".to_string(), name.clone()), ("superci-plane".into(), self.config.plane_id.clone()), ("superci-job".into(), job.job_id.to_string())];
                        let types = ["t3a.small".to_string(), "t3.small".into(), "m7a.large".into()];
                        let network = self.network(&region, &creds, self.clock.now_ms()).await?;
                        aws::run_instance(self.http, &creds, &Launch { region: &region, network: network.as_ref(), image: &image, types: &types, user_data: &aws::runner_user_data(&work, 10)?, disk_gb: image_gb.max(30), tags: &tags, spot: true, os: "linux" }, self.clock.now_ms()).await.map(|_| ())
                    }
                }
            }.await;
            if started.is_ok() {
                job.failed_fast = true;
                job.fail_pending = None;
                let _ = put_json(self.store, &job_key(job), &*job).await;
                return;
            }
        }
        // Nowhere would start it: its registration withdrawn, and tried again from the sweep.
        let _ = github::delete_runner(self.http, &app.api(), &token, &owner, &job.repo, runner_id).await;
        let _ = self.store.delete(&format!("runner:{name}")).await;
        self.fail_later(job, why).await
    }

    /// A failing runner that could not be started now: the sweep tries again, each minute for half an hour, while
    /// GitHub still has the job queued.
    async fn fail_later(&self, job: &mut Job, why: &str) {
        job.fail_pending = Some(why.to_string());
        let _ = put_json(self.store, &job_key(job), &*job).await;
        let _ = self.timer.wake_in(60_000).await;
    }

    /// A failing runner ended a job. Its own: nothing more to do. Another with the same label (GitHub gives a runner
    /// any job with its label): that one failed for the first one's reason, said on it. Its machine, if one is coming,
    /// takes the first job, still queued (see `started`); if none is, the first job gets a failing runner again.
    async fn failed_by_runner(&self, runner: &str, job_id: u64) -> Result<()> {
        let first = self.by_runner(Some(runner)).await?;
        self.store.delete(&format!("runner:{runner}")).await?;
        let Some(mut first) = first.filter(|f| f.job_id != job_id) else { return Ok(()) };
        let Some(mut taken) = get_json::<Job>(self.store, &format!("job:{job_id}")).await? else { return Ok(()) };
        // One that was failing here too (its own failing runner takes the first), or one whose machine is busy with
        // another job (its record is that machine's).
        if !(active(&taken) || taken.state == "waiting") || taken.seen_in_progress { return Ok(()) }
        let why = first.error.clone().unwrap_or_default();
        taken.error = Some(format!("failed by the runner started to fail {} ({why})", if first.name.is_empty() { "another job" } else { first.name.as_str() }).chars().take(300).collect());
        taken.failed_fast = true;
        taken.for_job = Some(first.job_id);
        if taken.state == "waiting" {
            taken.for_job = None;
            taken.state = "failed".into();
            taken.ended_ms = Some(self.clock.now_ms());
            first.failed_fast = false;
            first.fail_pending = Some(why);
            put_json(self.store, &job_key(&first), &first).await?;
        }
        put_json(self.store, &format!("job:{job_id}"), &taken).await?;
        self.timer.wake_in(60_000).await
    }

    async fn by_runner(&self, runner: Option<&str>) -> Result<Option<Job>> {
        let Some(name) = runner else { return Ok(None) };
        let Some(job_id) = get_json::<u64>(self.store, &format!("runner:{name}")).await? else { return Ok(None) };
        get_json::<Job>(self.store, &format!("job:{job_id}")).await
    }

    /// A job began on a runner. GitHub gives a waiting job to any idle runner with its label, so a runner may begin a
    /// job other than the one it was started for. When that other job has no machine of its own coming any more (its
    /// machine failed or was swept), this runner becomes its machine, and the job it was started for gets a new one;
    /// otherwise the two simply swap (the other job's machine takes this one's job).
    async fn started(&self, plane_url: &str, job_id: u64, runner: Option<&str>) -> Result<()> {
        // A failing runner (see `fail_fast`) is no machine of a job's: what it took is settled when it ends.
        if runner.is_some_and(failing_runner) { return Ok(()) }
        if let (Some(mut j), Some(name)) = (self.by_runner(runner).await?, runner) {
            if j.job_id != job_id {
                if let Some(mut other) = get_json::<Job>(self.store, &format!("job:{job_id}")).await? {
                    if !["launching", "launched", "running"].contains(&other.state.as_str()) {
                        let now = self.clock.now_ms();
                        other.runner = j.runner.clone();
                        other.runner_id = j.runner_id;
                        other.machine_id = j.machine_id.clone();
                        other.machine_type = j.machine_type.clone();
                        other.cloud = j.cloud.clone();
                        other.region = j.region.clone();
                        other.cpu = j.cpu;
                        other.usd_per_hour = j.usd_per_hour;
                        // The machine as it is: where it is, what it costs and since when, and what it says if AWS
                        // takes it back.
                        (other.zone, other.disk_gb, other.waiting_usd_per_hour, other.launched_ms) = (j.zone.take(), j.disk_gb.take(), j.waiting_usd_per_hour.take(), j.launched_ms);
                        (other.on_demand, other.notice, other.interrupted, other.private, other.for_job) = (j.on_demand, j.notice.take(), j.interrupted, j.private, None);
                        (other.cost_usd, other.cost_from, other.sent_gb, other.fail_pending, other.failed_fast) = (None, None, None, None, false);
                        other.state = "running".into();
                        other.seen_in_progress = true;
                        other.error = None;
                        other.started_ms = Some(now);
                        other.ended_ms = None;
                        put_json(self.store, &format!("job:{}", other.job_id), &other).await?;
                        put_json(self.store, &format!("runner:{name}"), &other.job_id).await?;
                        // The job this runner was for: a machine of its own, again.
                        let key = job_key(&j);
                        (j.runner, j.runner_id, j.machine_id, j.machine_type) = (None, None, None, None);
                        (j.interrupted, j.for_job) = (false, None);
                        j.cloud = String::new();
                        // A job that is over (cancelled before a runner took it, or failed by another's failing
                        // runner) needs no machine again.
                        if j.state == "orphan" || j.failed_fast || j.over {
                            j.state = if j.state == "orphan" { "cancelled" } else if j.failed_fast { "failed" } else { "done" }.into();
                            j.ended_ms.get_or_insert(now);
                            return put_json(self.store, &key, &j).await;
                        }
                        j.state = "launching".into();
                        j.error = None;
                        j.launched_ms = Some(now);
                        j.runner_suffix = format!("-b{}", now % 100_000);
                        put_json(self.store, &key, &j).await?;
                        return self.start(plane_url, &mut j).await;
                    }
                }
            }
        }
        if let Some(mut j) = self.by_runner(runner).await? {
            // It began another job than its own, whose machine is still coming: that machine is for this runner's
            // job now (or for whichever job this machine was for).
            if j.job_id != job_id {
                if let Some(mut other) = get_json::<Job>(self.store, &format!("job:{job_id}")).await?.filter(|o| active(o) && !o.seen_in_progress) {
                    other.for_job = Some(j.for_job.unwrap_or(j.job_id));
                    put_json(self.store, &format!("job:{job_id}"), &other).await?;
                }
            }
            j.for_job = None;
            j.seen_in_progress = true;
            // A machine left idle by a cancelled job (an orphan) that took a job before the sweep came is busy too.
            if j.state == "launched" || j.state == "orphan" { j.state = "running".into(); }
            j.started_ms.get_or_insert(self.clock.now_ms());
            put_json(self.store, &format!("job:{}", j.job_id), &j).await?;
        }
        Ok(())
    }

    /// A finished job ends the machine that ran it, found by the runner's name (GitHub may hand a job to any idle runner
    /// with the label). A job cancelled before any runner took it leaves its own machine idle: marked for the sweep.
    async fn completed(&self, plane_url: &str, ev: &github::JobEvent) -> Result<()> {
        let (job_id, runner) = (ev.job_id, ev.runner_name.as_deref());
        if let Some(name) = runner.filter(|r| failing_runner(r)) { return self.failed_by_runner(name, job_id).await }
        let mut on_anothers = None;
        if let Some(mut j) = self.by_runner(runner).await? {
            if j.job_id != job_id { on_anothers = Some(j.for_job.unwrap_or(j.job_id)) }
            self.stop_machine(plane_url, &j).await;
            if let Some(name) = runner { let _ = self.store.delete(&format!("runner:{name}")).await; }
            j.state = "done".into();
            j.ended_ms.get_or_insert(self.clock.now_ms());
            // AWS took its spot machine back: the job it was running is run again once its run has finished (GitHub
            // runs nothing again before), on an on-demand machine. Not one that passed anyway (the notice came as
            // it ended).
            if j.interrupted && j.rerun.is_none() && !matches!(ev.conclusion.as_deref(), Some("success" | "skipped" | "neutral")) {
                j.state = "failed".into();
                j.error = Some("AWS took back its spot machine; the job is run again, off spot machines once its run has finished".into());
                j.rerun = Some(Rerun { job_id, run_id: ev.run_id, repo: ev.repo.clone(), name: ev.name.clone(), state: "pending".into(), at_ms: self.clock.now_ms() });
                self.timer.wake_in(60_000).await?;
            }
            self.settle(plane_url, &mut j).await;
            put_json(self.store, &format!("job:{}", j.job_id), &j).await?;
            if j.job_id == job_id { return Ok(()); }
        }
        if let Some(mut own) = get_json::<Job>(self.store, &format!("job:{job_id}")).await? {
            // Cancelled while waiting for room: nothing was started.
            if own.state == "waiting" {
                own.state = "cancelled".into();
                own.ended_ms = Some(self.clock.now_ms());
                put_json(self.store, &format!("job:{job_id}"), &own).await?;
                return Ok(());
            }
            if own.state == "launched" && !own.seen_in_progress {
                // It ran on another job's runner: that job, still queued, is what its own machine is for now. It
                // ran nowhere (cancelled before a runner took it): its machine is ended by the sweep.
                // (The same event delivered again finds the runner's name forgotten: what was noted stays.)
                match on_anothers {
                    Some(waiting) => { own.over = true; own.for_job.get_or_insert(waiting); }
                    None if own.over => {}
                    None => own.state = "orphan".into(),
                }
                put_json(self.store, &format!("job:{job_id}"), &own).await?;
                self.timer.wake_in(60_000).await?;
            }
        }
        Ok(())
    }

    /// Every minute while machines are up: ends machines whose runner never got a job (10 min), orphans, and machines
    /// past the job time bound (they also power themselves off).
    pub async fn alarm(&self) -> Result<()> {
        let now = self.clock.now_ms();
        let plane_url = get_json::<String>(self.store, "plane_url").await?.unwrap_or_default();
        let (mut active, mut waiting) = (0, 0);
        for (key, v) in self.store.list("job:").await? {
            let Ok(mut j) = serde_json::from_str::<Job>(&v) else { continue };
            // History is kept two months (this month's spend and the last's).
            if !["launching", "launched", "running", "waiting"].contains(&j.state.as_str()) && now.saturating_sub(j.ended_ms.unwrap_or(j.at_ms)) > 62 * 86_400_000 {
                self.store.delete(&key).await?;
                continue;
            }
            // Waiting for room: try again (GitHub keeps a job queued for up to a day).
            if j.state == "waiting" {
                if now.saturating_sub(j.at_ms) > 24 * 3_600_000 { j.state = "failed".into(); j.error = Some("waited a day for room".into()); put_json(self.store, &key, &j).await?; continue }
                self.start(&plane_url, &mut j).await?;
                if j.state == "waiting" { waiting += 1 }
                continue;
            }
            // A start cut off midway (the runtime stopped while it waited on a cloud): placed again, twice at most.
            if j.state == "launching" {
                if now.saturating_sub(j.launched_ms.unwrap_or(j.at_ms)) < 120_000 { active += 1; continue }
                self.stop_machine(&plane_url, &j).await;
                if j.retries >= 2 {
                    j.state = "failed".into();
                    j.error = Some("its machine's start was cut off, three times".into());
                    j.ended_ms.get_or_insert(now);
                    put_json(self.store, &key, &j).await?;
                } else {
                    j.retries += 1;
                    j.launched_ms = Some(now);
                    self.again(&plane_url, &key, &mut j, "its machine's start was cut off; trying again").await?;
                    active += 1;
                }
                continue;
            }
            // A job whose spot machine AWS took back: run again once its run has finished.
            if j.rerun.as_ref().is_some_and(|r| r.state == "pending") {
                if self.run_again(&mut j, now).await { put_json(self.store, &key, &j).await? } else { active += 1 }
                continue;
            }
            // A failed job GitHub still has queued, its failing runner not started yet: tried again, for half an hour.
            if let (Some(why), false) = (j.fail_pending.clone(), j.provider == "gitlab") {
                if now.saturating_sub(j.ended_ms.unwrap_or(j.at_ms)) > 30 * 60_000 { j.fail_pending = None; put_json(self.store, &key, &j).await?; continue }
                let queued = match (self.app_of(&j), self.installation_token_for(&j).await) {
                    (Some(app), Some(t)) => github::job_status(self.http, &app.api(), &t, &j.repo, j.job_id).await.map(|s| s == "queued").unwrap_or(true),
                    _ => false,
                };
                if queued { self.fail_fast(&plane_url, &mut j, &why).await; active += 1 } else { j.fail_pending = None; put_json(self.store, &key, &j).await? }
                continue;
            }
            // An AWS machine that ended a while ago: what it sent, once CloudWatch has counted it, added to its cost.
            if j.cloud == "aws" && j.cost_from.is_some() && j.sent_gb.is_none() && j.ended_ms.is_some_and(|e| now > e + 12 * 60_000 && now < e + 2 * 86_400_000) {
                if self.settle_sent(&plane_url, &mut j).await { put_json(self.store, &key, &j).await?; }
                continue;
            }
            if !["launched", "running", "orphan"].contains(&j.state.as_str()) { continue; }
            // A machine that never began its job (a container that did not start, a spot machine that did not boot):
            // placed again, twice at most, then the job fails with why (GitHub would otherwise keep it queued for a day).
            if j.state == "launched" && !j.seen_in_progress {
                let wait = if j.cloud == "aws" { 8 * 60_000 } else { 3 * 60_000 };
                if now.saturating_sub(j.launched_ms.unwrap_or(j.at_ms)) > wait {
                    let place = cloud_name(&j.cloud).to_string();
                    self.stop_machine(&plane_url, &j).await;
                    // Only while GitHub still has the job queued (another runner may have taken it, or it was
                    // cancelled, and the event never came).
                    // The job its machine is for: its own, or the one whose runner took its own (still queued, then, with
                    // no machine coming but this one).
                    let wanted = j.for_job.unwrap_or(j.job_id);
                    let gone = ((j.failed_fast || j.over) && j.for_job.is_none()) || if j.provider == "gitlab" {
                        match (self.gitlab_of(&j), j.project_id) { (Some(gl), Some(p)) => gitlab::job(self.http, gl, p, wanted).await.is_ok_and(|(_, s)| s != "pending"), _ => false }
                    } else {
                        match (self.app_of(&j), self.installation_token_for(&j).await) { (Some(app), Some(t)) => github::job_status(self.http, &app.api(), &t, &j.repo, wanted).await.is_ok_and(|s| s != "queued"), _ => false }
                    };
                    if gone {
                        self.withdraw_runner(&j).await;
                        j.state = "swept".into();
                        j.ended_ms.get_or_insert(now);
                        self.settle(&plane_url, &mut j).await;
                        put_json(self.store, &key, &j).await?;
                        continue;
                    }
                    if j.retries >= 2 {
                        self.withdraw_runner(&j).await;
                        j.state = "failed".into();
                        j.error = Some(format!("{place} did not start a machine for it, three times"));
                        j.ended_ms.get_or_insert(now);
                        put_json(self.store, &key, &j).await?;
                    } else {
                        j.retries += 1;
                        self.again(&plane_url, &key, &mut j, &format!("{place} did not start its machine; trying again")).await?;
                        if j.state == "waiting" { waiting += 1 } else { active += 1 }
                    }
                    continue;
                }
            }
            // Its machine's age (a job may have waited long for room before it got one).
            let age = now.saturating_sub(j.launched_ms.unwrap_or(j.at_ms));
            let stale = j.state == "orphan" || (!j.seen_in_progress && age > 10 * 60_000) || age > (MAX_JOB_MINUTES as u64 + 5) * 60_000;
            if !stale { active += 1; continue; }
            self.stop_machine(&plane_url, &j).await;
            self.withdraw_runner(&j).await;
            j.state = "swept".into();
            j.ended_ms.get_or_insert(now);
            self.settle(&plane_url, &mut j).await;
            put_json(self.store, &key, &j).await?;
        }
        self.sweep_gitlab_runners(now).await?;
        // Notes that only matter for a while: spot interruptions (counted for the pause), jobs asked to run again and
        // not arrived.
        for (prefix, keep) in [("spot:interrupted:", SPOT_PAUSE.1 + SPOT_PAUSE.2), ("rerun:", 86_400_000), ("rerun-asked:", 7 * 86_400_000)] {
            for (k, v) in self.store.list(prefix).await? { if v.trim_matches('"').parse::<u64>().is_ok_and(|at| now.saturating_sub(at) > keep) { self.store.delete(&k).await? } }
        }
        self.redeliver_failed().await;
        // Every 20 seconds while jobs wait for room; every minute while machines are up; every five otherwise (redeliveries).
        self.timer.wake_in(if waiting > 0 { 20_000 } else if active > 0 { 60_000 } else { 300_000 }).await?;
        Ok(())
    }

    /// A webhook that failed while the runtime restarted (a secret changed, a new version) would leave its job waiting
    /// forever: GitHub does not retry. So failed deliveries from the last 30 minutes are asked for again, once each.
    async fn redeliver_failed(&self) {
        let now = self.clock.now_ms();
        for app in self.config.apps() {
            let Ok(failed) = github::failed_deliveries(self.http, app, now.saturating_sub(30 * 60_000), now).await else { continue };
            for (id, guid) in failed {
                let key = format!("redelivered:{guid}");
                if matches!(self.store.put_if_absent(&key, now.to_string()).await, Ok(true)) { let _ = github::redeliver(self.http, app, id, now).await; }
            }
        }
    }
}
