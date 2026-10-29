//! The dashboard, served only on this machine and only while `superci dashboard` runs. You sign in with your clouds in the
//! browser; it lists every control plane it finds there (or sets one up), each control plane's jobs, the machines running them, and the
//! clouds they run on; it creates a control plane's GitHub App (GitHub redirects back here; the App's key goes straight into the
//! control plane's secrets) and connects AWS. It keeps SuperCI's own sign-ins in SuperCI's folder on this machine (store.rs), so it
//! opens signed in and commands run without it; closing it changes nothing.
//!
//! It listens on localhost:8976 (the one return address Cloudflare's browser sign-in allows). The page is opened with a
//! random key in its URL, which becomes a same-site cookie: other websites and processes cannot drive it. It changes a
//! control plane only through the cloud's own API with your sign-in, and reads a control plane's jobs with a key it writes into the
//! control plane's secrets (kept with the sign-ins, renewed every thirty days).
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use superci_core::crypto::{random_token, safe_eq};
use superci_core::github::{self, app_manifest, manifest_target, Owner};
use superci_core::plane::AUDIENCE;
use superci_core::io::Response;
use superci_core::plane::{AwsBudget, Pool, Routing, Rule, AWS_ON_DEMAND, CLOUDFLARE_LOCATIONS as CF_LOCATIONS, MAX_CPU, MAX_JOB_MINUTES};
use superci_core::spec::{Capacity, Spec};
use superci_core::page::{document, esc, manifest_form, nearby_regions, region_name, region_short, REGIONS};

use crate::aws;
use crate::aws_plane;
use crate::cloudflare::{self, Cloudflare, oauth_port};
use crate::logos::{icon, logo};
use crate::modal;
use crate::plane::Plane;
use crate::store::{Kept, Key, Store};
use crate::view::{self, PlaneView, DASHBOARD_VERSION};
use crate::Result;

pub struct Dashboard {
    key: String,
    base: String,
    /// Every control plane found in your signed-in clouds, and which one the page shows.
    planes: Vec<Plane>,
    selected: usize,
    looked_cf: bool,
    looked_aws: bool,
    looked_modal: bool,
    /// This session's key for reading control planes' `/status`, the secret it is kept in (named with when it expires),
    /// and the control planes it has been written to.
    status_key: String,
    status_secret: String,
    keyed: HashSet<String>,
    pub(crate) cf: Option<cloudflare::Session>,
    /// A sign-in given by name for this run (SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_MODAL_TOKEN_ID): used, never kept.
    cf_given: bool,
    modal_given: bool,
    /// SuperCI's folder on this machine, where its sign-ins are kept (none: nothing is kept), what was last written
    /// there, the control plane in use as kept (for commands, before any cloud was asked), and whether writing failed.
    store: Option<Store>,
    /// Its name always, the first's too: what the addresses it hands out for coming back carry, since a request that
    /// names no workspace is refused once several are kept (see `back_url`).
    named: String,
    /// What the workspace is called and looks like in a list (kept beside its sign-ins), and when GitHub was last
    /// asked for its organization's picture.
    about: crate::store::About,
    picture_asked_ms: u64,
    picture_owner: String,
    kept_as: String,
    kept_plane: Option<Plane>,
    /// Control planes found in the signed-in clouds that are not this workspace's (another CI in the same account, a
    /// teammate's): offered, never taken in by themselves (a workspace looks after one CI; see `join`).
    found: Vec<Plane>,
    /// What each one found says of itself before it is used here (its name, whose jobs it runs, its label, version).
    found_about: HashMap<String, FoundAbout>,
    /// Whose jobs each control plane runs as last written into it (`OWNERS`), by its id: written again when that changes.
    owners_written: HashMap<String, String>,
    /// The control planes told this session which computer its key is on (`/keys/hello`), by their id.
    hello_sent: HashSet<String>,
    keep_failed: bool,
    /// A kept AWS sign-in turned out to have ended (AWS ends one after twelve hours at most).
    aws_ended: bool,
    /// The same of the kept Cloudflare sign-in (Cloudflare no longer renews it) and of Modal's (Modal no longer
    /// takes its token): forgotten, and what the other sign-ins reach goes on (see `cf_over`).
    cf_ended: bool,
    modal_ended: bool,
    /// The clouds that did not answer when last looked in ("Modal: …"): said on the pages, asked again on the next.
    unanswered: Vec<String>,
    /// A sign-in to AWS came back in this run (what `superci login aws --add` waits for), and whether one is awaited
    /// although an account is signed in to already.
    aws_came: bool,
    pub adding: bool,
    /// The other AWS accounts signed in to. `aws` is the one in hand: the account of the control plane a page or a
    /// command acts on (see `hold_account`), so everything that writes to AWS writes with the right one.
    aws_others: Vec<aws::Session>,
    /// The dashboard opened by a command for the one thing a person must do in a browser (see `Task`): once it is
    /// done, the page says so and the program ends.
    task: Option<Task>,
    /// A command is acting, in a page's place (`act`): what a page's action would say in the dashboard's terminal is
    /// left to the command.
    commanding: bool,
    /// The control plane kept from the last run, listed before any cloud was asked (its id): looking in its own
    /// cloud then says whether it is still there.
    preloaded: Option<String>,
    /// Said once at the top of the next page (signed out).
    notice: Option<String>,
    cf_pending: Option<cloudflare::Pending>,
    cf_accounts: Vec<(String, String)>,
    aws: Option<aws::Session>,
    /// Which GitLab connection the GitLab page shows (its name; nothing: the first).
    gitlab_shown: String,
    /// The job its page shows (`?p=job&id=…`, and `gl=…` for a GitLab job of a further connection).
    job_shown: (String, String),
    /// The run its page shows (`?p=run&repo=…&id=…&attempt=…`).
    run_shown: (String, String, String),
    aws_pending: Option<aws::Pending>,
    /// When the sign-in with AWS under way was started (see `aws_stuck`).
    aws_asked_ms: u64,
    modal: Option<modal::Session>,
    /// The other Modal workspaces signed in to (`superci login modal --add`): each one runs jobs through a launcher of
    /// its own, as a place of its own in the order (`modal:<workspace>`). `modal` stays the first: the one a Modal
    /// control plane and its settings are written with. Whether one came back in this run (what `--add` waits for).
    modal_others: Vec<modal::Session>,
    modal_came: bool,
    modal_pending: Option<modal::Pending>,
    /// The page a sign-in started from, to come back to (`?p=…`).
    return_to: String,
    /// "Nowhere yet": the connect screen asks where SuperCI should live, and the deploy card offers to put it there.
    show_setup: bool,
    /// An App being made on GitHub: the state it comes back with, whose it is, and where that GitHub is (none: github.com).
    manifest_state: Option<(String, Owner, Option<String>)>,
    /// The control plane a GitHub App is being created for: GitHub comes back with no word of it.
    github_for: Option<String>,
    /// A control plane being deployed in the background (the page shows its steps), or one that stopped.
    deploying: Arc<Mutex<Option<Deploy>>>,
    /// A deploy just finished: the page that showed it goes to Overview, once.
    deployed: Arc<Mutex<bool>>,
    /// The control plane that was just set up (its pages are where the person goes on, beside any others).
    deployed_to: Arc<Mutex<Option<String>>>,
    /// A move to another control plane, under way in the background (its steps), or how it ended.
    moving: Arc<Mutex<Option<Move>>>,
    /// An update of a control plane, under way in the background (its steps), or how it ended.
    updating: Arc<Mutex<Option<Update>>>,
    /// Spot CPU quotas per region for the AWS account signed in (read from AWS, kept a few minutes while this runs).
    quotas: HashMap<String, Result<u32>>,
    quotas_for: Option<(String, u64)>,
    /// What each AWS account signed in to allows (see `aws::Look`), and when it was asked: looked at where a control
    /// plane or runners are set up, again after ten minutes.
    aws_looks: HashMap<String, (u64, aws::Look)>,
    /// The regions AWS is copying the image jobs run on into, by account (asked with the look).
    aws_copying: HashMap<String, Vec<String>>,
    /// The AWS accounts being looked at again beside the pages just now (see `look_all`).
    aws_looking: std::collections::HashSet<String>,
    /// Where an AWS account's own copy of the runner image is of an older build than its maker's newest, by account:
    /// the region, the build held, the newest, and whether the newest is being copied in.
    aws_behind: HashMap<String, Vec<(String, String, String, bool)>>,
    /// When the rule of an AWS account's budget was last taken off a control plane's role here (by its id): what
    /// the control plane noted before that is no longer so.
    budget_lifted: HashMap<String, u64>,
    /// Each control plane's last status read in this session: a slow answer (a Lambda starting) shows the last one
    /// rather than the page waiting again. Kept only while the dashboard runs.
    seen: HashMap<String, serde_json::Value>,
    /// What the last change on the Control plane page did, said once there.
    flash: Option<String>,
    /// The control planes as last read (when), shown at once while they are read again in the background; any change
    /// made here clears it, so a page after a change is always read fresh.
    views: Option<(Vec<PlaneView>, u64)>,
    refreshing: bool,
    /// A GitHub App was just made here (until when this is waited for): until the control plane says it is installed,
    /// pages are read fresh and look again by themselves. GitHub's pages do not say when the person is done there, and
    /// a control plane takes a few seconds to see what was just stored.
    github_expected: Option<u64>,
    /// When Cloudflare's metering was last read for finished jobs (see `measure`), and whether it is being read now.
    measured_at: u64,
    measuring: bool,
    /// This month as the clouds bill it (read with `measure`): Cloudflare's containers in the signed-in accounts (metered,
    /// billed after included usage), and Modal's workspace.
    cf_month: Option<(f64, f64)>,
    modal_month: Option<modal::Month>,
    /// Each control plane's AWS runner role, as read while signed in to its account: what it lacks of what this version
    /// asks for (or why it could not be read), and when it was read.
    aws_missing: Arc<Mutex<HashMap<String, (std::result::Result<Vec<&'static superci_core::permissions::Need>, String>, u64)>>>,
    /// Control planes that have answered in this session, and when each was first asked: one not heard from yet is
    /// loading (for half a minute), not "not set up".
    answered: HashSet<String>,
    first_asked: HashMap<String, u64>,
    /// Each control plane's last full view and when: one that did not answer just now (a Worker restarting after a
    /// setting changed) shows it, for a minute, instead of looking like it lost its runners.
    last_good: HashMap<String, (PlaneView, u64)>,
}


/// What a command needs a person for, in the browser: a sign-in (with any cloud, or with one), or making a GitHub App
/// for an account and choosing its repositories (GitHub offers that only on its own pages).
pub enum Task {
    Login(Option<&'static str>),
    GitHub { login: String, host: String, started: bool, done: bool },
}

/// An update: of which control plane, its steps, the one it is at, and how it ended.
struct Update { plane: String, steps: &'static [&'static str], at: usize, result: Option<Result<()>>, ended_ms: u64,
    /// What the update left out, said with its result (a part in a cloud whose sign-in is over); nothing: all of it.
    note: String }

impl Update {
    /// Uploaded, and its control plane still answers with the version before: its cloud is switching it over (with
    /// containers that can take a few minutes). Not asked to update again meanwhile; after ten minutes, it is.
    fn restarting(&self, v: &PlaneView) -> bool {
        self.plane == v.plane.plane_id() && matches!(self.result, Some(Ok(()))) && v.outdated() && now_ms() < self.ended_ms + 10 * 60_000
    }
}

const UPDATE_CLOUDFLARE: [&str; 3] = ["Uploading the new version", "Giving it its address", "Waiting for the new version"];
const UPDATE_AWS: [&str; 6] = ["Updating its role", "Checking its table", "Uploading the new functions (the control plane and its Docker image converter)", "Checking its address", "Checking its schedule", "Waiting for the new version"];
const UPDATE_MODAL: [&str; 3] = ["Building the new version", "Deploying it", "Waiting for the new version"];

/// What an update deploys, with the sign-in of the cloud its control plane runs in.
enum Deploying {
    Cloudflare { account_id: String, script: String, label: String },
    Aws { creds: superci_core::aws::Credentials, account_id: String, region: String, plane_id: String, label: String },
    Modal { session: modal::Session, plane_id: String, label: String },
}

/// A move: to which control plane, the step it is at, and how it ended (with what did not come along).
struct Move { to: String, at: usize, result: Option<Result<Vec<String>>>, ended_ms: u64 }

const MOVE_STEPS: [&str; 4] = ["Copying your settings and history", "Bringing your runner providers", "Switching GitHub and GitLab over", "Checking it answers"];

/// Writes control planes' secrets from a background thread, with the sign-ins it was given.
/// A control plane found in a signed-in cloud, as it says of itself to that cloud's sign-in (its name and whose jobs
/// it runs, kept where the account's sign-in reads them: AWS's) and to anyone (its label and version).
#[derive(Clone, Default)]
struct FoundAbout { name: Option<String>, owners: Option<String>, label: Option<String>, version: Option<String>,
    /// Whether GitHub or GitLab sends it jobs (it is a CI), as it answered; one that does not is a place set up and never used.
    serves: bool }

/// What this computer is called, as its owner named it (macOS's computer name), else its host name.
fn computer_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let run = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|n| !n.is_empty());
        run("scutil", &["--get", "ComputerName"]).or_else(|| run("hostname", &[])).unwrap_or_default().chars().take(64).collect()
    }).clone()
}

/// Whose jobs a control plane runs, as kept in `OWNERS` (`github:acme,gitlab:gitlab.com`), in words.
fn owners_said(owners: &str) -> String {
    owners.split(',').filter_map(|o| o.split_once(':')).map(|(kind, name)| format!("{} {name}", if kind == "gitlab" { "GitLab" } else { "GitHub" })).collect::<Vec<_>>().join(", ")
}

struct Writer { cf: Option<cloudflare::Cloudflare>, aws: Option<superci_core::aws::Credentials>, modal: Option<modal::Session>, own: Own }

/// Whose the sign-ins kept are: the AWS account, the Cloudflare accounts the login reaches (none: not asked yet), the
/// Modal workspace.
#[derive(Clone, Default)]
struct Own { aws: Option<String>, cf: Vec<String>, modal: Option<String> }

impl Own {
    /// A control plane is changed only with the sign-in of the account it lives in: with another account's the cloud
    /// would take the change somewhere else, or refuse it, and nothing here would know.
    fn check(&self, plane: &Plane) -> Result<()> {
        let other = |cloud: &str, its: &str, ours: &str| Err(format!("This control plane is in {cloud} {its}, and SuperCI is signed in to {ours}. Sign in to {its} to change it. Nothing has changed."));
        match plane {
            Plane::Aws { account_id, .. } => match &self.aws { Some(a) if a != account_id => other("AWS account", account_id, a), _ => Ok(()) },
            Plane::Modal { workspace, .. } => match &self.modal { Some(w) if w != workspace => other("the Modal workspace", workspace, w), _ => Ok(()) },
            Plane::Cloudflare { account_id, account_name, .. } if !self.cf.is_empty() && !self.cf.contains(account_id) => {
                Err(format!("This control plane is in the Cloudflare account {account_name}, which the Cloudflare sign-in kept does not reach. Sign in to it to change it. Nothing has changed."))
            }
            _ => Ok(()),
        }
    }
}

impl Writer {
    fn put(&self, plane: &Plane, name: &str, value: &str) -> Result<()> {
        self.own.check(plane)?;
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cf.as_ref().ok_or("Sign in with Cloudflare first.")?.put_secret(account_id, script, name, value),
            Plane::Aws { region, plane_id, .. } => aws_plane::put_secret(self.aws.as_ref().ok_or("Sign in with AWS first.")?, region, plane_id, name, value),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, value),
            Plane::Seen { .. } => Err("Sign in where this control plane runs first.".into()),
        }
    }

    /// The names of a control plane's settings, where its cloud lists them (Modal's Dict is not listed: none).
    fn names(&self, plane: &Plane) -> Vec<String> {
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cf.as_ref().and_then(|c| c.secret_names(account_id, script).ok()).unwrap_or_default(),
            Plane::Aws { region, plane_id, .. } => self.aws.as_ref().and_then(|c| aws_plane::secret_names(c, region, plane_id).ok()).unwrap_or_default(),
            _ => vec![],
        }
    }

    /// Removes a setting (Modal's is emptied: its settings are entries of one Dict).
    fn drop(&self, plane: &Plane, name: &str) -> Result<()> {
        self.own.check(plane)?;
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cf.as_ref().ok_or("Sign in with Cloudflare first.")?.delete_secret(account_id, script, name),
            Plane::Aws { region, plane_id, .. } => aws_plane::delete_secret(self.aws.as_ref().ok_or("Sign in with AWS first.")?, region, plane_id, name),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, ""),
            Plane::Seen { .. } => Err("Sign in where this control plane runs first.".into()),
        }
    }
}

/// What a move brings along of the runner providers: each provider the control plane in use has, and whether it can
/// come (an error says what is needed first, or why it cannot).
#[derive(Debug, PartialEq)]
enum Carry { Comes(&'static str), NeedsSignIn(&'static str), Cannot(&'static str, &'static str) }

/// A control plane on its way: which of its steps has started, and how it ended.
struct Deploy {
    cloud: &'static str,
    /// Where: the Cloudflare account's name, or the AWS account and region.
    place: String,
    steps: &'static [&'static str],
    at: usize,
    /// The form that started it, to try again with.
    form: Vec<(&'static str, String)>,
    result: Option<std::result::Result<Plane, String>>,
}

thread_local! {
    /// The workspace the request in hand is of (see `respond`): what a page's frame says is kept by workspace.
    static HERE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// A workspace's own notes for its pages' frame (see `side`, `called` and `pictured`).
#[cfg(not(test))]
fn frame(which: usize) -> &'static Mutex<String> {
    type Notes = [&'static Mutex<String>; 3];
    static ALL: Mutex<Vec<(String, Notes)>> = Mutex::new(Vec::new());
    let name = HERE.with(|h| h.borrow().clone());
    let mut all = lock(&ALL);
    if let Some((_, notes)) = all.iter().find(|(n, _)| *n == name) { return notes[which] }
    let note = || -> &'static Mutex<String> { Box::leak(Box::new(Mutex::new(String::new()))) };
    let notes = [note(), note(), note()];
    all.push((name, notes));
    notes[which]
}

/// The sidebar's update card as last drawn, so each page's frame has it at once (asking the control plane comes after).
#[cfg(not(test))]
fn side() -> &'static Mutex<String> { frame(0) }
/// Tests run side by side: each its own.
#[cfg(test)]
fn side() -> &'static Mutex<String> { thread_local!(static SIDE: &'static Mutex<String> = Box::leak(Box::new(Mutex::new(String::new())))); SIDE.with(|s| *s) }

/// What the control plane in use is called, as last seen: the page's frame is drawn before it is asked, and says
/// this until the page's live part says otherwise.
#[cfg(not(test))]
fn called() -> &'static Mutex<String> { frame(1) }
#[cfg(test)]
fn called() -> &'static Mutex<String> { thread_local!(static CALLED: &'static Mutex<String> = Box::leak(Box::new(Mutex::new(String::new())))); CALLED.with(|s| *s) }
/// The workspace's picture (its GitHub organization's), as last known: the frame's mark at the top of the sidebar.
#[cfg(not(test))]
fn pictured() -> &'static Mutex<String> { frame(2) }
#[cfg(test)]
fn pictured() -> &'static Mutex<String> { thread_local!(static PICTURED: &'static Mutex<String> = Box::leak(Box::new(Mutex::new(String::new())))); PICTURED.with(|s| *s) }
fn called_now() -> String { let c = lock(called()); if c.is_empty() { "SuperCI".to_string() } else { c.clone() } }

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }

thread_local! {
    /// The last page that said one thing (its status, heading and text): what a command, which asked in a browser's
    /// place, says in turn (see `Dashboard::act`).
    static SAID: std::cell::RefCell<Option<(u16, String, String)>> = const { std::cell::RefCell::new(None) };
}

/// A page that says one thing.
fn message(status: u16, heading: &str, text: &str) -> Response {
    SAID.with(|s| *s.borrow_mut() = Some((status, heading.to_string(), text.to_string())));
    superci_core::page::message(status, heading, text)
}

/// A request; `last` is the connect screen's "Last used" cloud (a cookie of its own, only a cloud's name).
struct Req { method: String, path: String, query: Vec<(String, String)>, cookie: Option<String>, last: Option<String>, body: Vec<u8>, host: String, origin: Option<String>, fetch_site: Option<String> }

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(&*stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let (mut length, mut cookies, mut last, mut host, mut origin, mut fetch_site) = (0usize, String::new(), None, String::new(), None, None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" { break }
        let (name, value) = h.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "host" => host = value.trim().to_ascii_lowercase(),
            "origin" => origin = Some(value.trim().to_ascii_lowercase()),
            "sec-fetch-site" => fetch_site = Some(value.trim().to_ascii_lowercase()),
            "cookie" => {
                let find = |name: &str| value.split(';').map(str::trim).find_map(|c| c.strip_prefix(name)).map(str::to_string);
                cookies = value.to_string();
                last = find("superci_last=").filter(|c| CLOUDS.contains(&c.as_str()));
            }
            _ => {}
        }
    }
    let mut body = vec![0u8; length.min(64 * 1024)];
    reader.read_exact(&mut body).ok()?;
    let url = url::Url::parse(&format!("http://local{target}")).ok()?;
    // This dashboard's own cookie: named for its port, since a browser keeps cookies per computer name and not per
    // port (two dashboards would take each other's).
    let name = format!("{}=", key_cookie(&host));
    let cookie = cookies.split(';').map(str::trim).find_map(|c| c.strip_prefix(name.as_str())).map(str::to_string);
    Some(Req { method, path: url.path().to_string(), query: url.query_pairs().into_owned().collect(), cookie, last, body, host, origin, fetch_site })
}

/// The name of the cookie that holds a dashboard's key: `superci_local` on the usual port, with the port after it
/// on another.
fn key_cookie(host: &str) -> String {
    match host.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) { Some(p) if p != oauth_port() => format!("superci_local_{p}"), _ => "superci_local".into() }
}

/// What listens on a port, on both loopback addresses ("localhost" can mean either to a browser). Taken on either, the
/// port is taken (none): a browser's "localhost" could reach the other program there. (An address this computer has
/// not, IPv6's where it is off: the other is enough.)
fn listen_on(p: u16) -> Vec<TcpListener> {
    let mut bound = vec![];
    let free = [format!("127.0.0.1:{p}"), format!("[::1]:{p}")].iter().all(|addr| match TcpListener::bind(addr) { Ok(l) => { bound.push(l); true } Err(e) => e.kind() != std::io::ErrorKind::AddrInUse });
    if free { bound } else { vec![] }
}

fn write_response(stream: &mut TcpStream, r: &Response) {
    let mut head = format!("HTTP/1.1 {} X\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n", r.status, r.body.len());
    for (k, v) in &r.headers { head.push_str(&format!("{k}: {v}\r\n")) }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
}

/// With SUPERCI_TIMING set: how long a step of reading the clouds took, in the terminal.
fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let start = std::time::Instant::now();
    let out = f();
    if std::env::var_os("SUPERCI_TIMING").is_some() { eprintln!("  {:>6} ms  {what}", start.elapsed().as_millis()) }
    out
}

/// The dashboard's time. In a debug build, `SUPERCI_LATER` names a file holding how many milliseconds a test moved it
/// on: what it does after minutes is tested without waiting them.
fn now_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    #[cfg(debug_assertions)]
    if let Some(file) = std::env::var_os("SUPERCI_LATER") { return now + std::fs::read_to_string(file).ok().and_then(|ms| ms.trim().parse::<u64>().ok()).unwrap_or(0) }
    now
}

/// The keys that only read, as a control plane's status lists them: each one's name and when it ends (ms).
pub fn read_keys(status: &serde_json::Value) -> Vec<(String, u64)> {
    status["read_keys"].as_array().into_iter().flatten().filter_map(|k| Some((k["name"].as_str()?.to_string(), k["until_ms"].as_u64()?))).collect()
}

/// A day, as 2026-10-07 (UTC), from unix seconds: when a key ends, as commands say it.
pub fn day_of(unix: u64) -> String { day(unix) }

/// SuperCI's own settings as JSON: plain fields and maps with string keys, which serialize whatever they hold.
fn json_text(v: &impl serde::Serialize) -> String { serde_json::to_string(v).unwrap_or_default() }
fn json_value(v: &impl serde::Serialize) -> serde_json::Value { serde_json::to_value(v).unwrap_or_default() }

/// What a cloud's search on a thread of its own found; one that stopped (it panicked) is said as that.
fn searched<T>(h: std::thread::ScopedJoinHandle<'_, Result<T>>, cloud: &str) -> Result<T> { h.join().unwrap_or_else(|_| Err(format!("{cloud}'s search stopped"))) }

/// What a command waits for after starting it (see `Dashboard::wait`).
#[derive(Clone, Copy)]
pub enum Background { Deploy, Update, Move }

/// How a job stands, as its row says it (a pill's tone and word). One a machine ran to its end is said by how the
/// job itself ended, as its code host said: Passed, or Failed when its own steps failed. Done, when that was not
/// said (a job from before it was kept). Failed is also a job SuperCI could not run or finish; its row says why.
fn job_ended(j: &serde_json::Value) -> (&'static str, &'static str) {
    match (j["state"].as_str().unwrap_or_default(), j["conclusion"].as_str()) {
        ("done", Some("success")) => ("good", "Passed"),
        ("done", Some("failure" | "timed_out" | "startup_failure")) => ("bad", "Failed"),
        ("done", Some("cancelled")) => ("", "Cancelled"),
        ("done", Some("skipped" | "neutral")) => ("", "Skipped"),
        ("done", _) => ("good", "Done"),
        ("failed" | "swept", _) => ("bad", "Failed"),
        // (An orphan is a job cancelled while its machine came up: that machine is ended unless it takes another job.)
        ("cancelled" | "orphan", _) => ("", "Cancelled"),
        ("running", _) => ("accent", "Running"),
        ("waiting", _) => ("open", "Waiting"),
        _ => ("open", "Starting"),
    }
}

/// A job as commands say it: how it stands, what it is (its name, repository and workflow), the runner it got, how
/// long it ran, what it cost, and why (when it failed or waits): the words the Jobs page has.
pub fn job_words(j: &serde_json::Value) -> serde_json::Value {
    let state = match j["state"].as_str().unwrap_or_default() { "done" => "done", "failed" | "swept" => "failed", "cancelled" | "orphan" => "cancelled", "running" => "running", "waiting" => "waiting", _ => "starting" };
    let (title, sub) = job_names(j);
    let why = if ["failed", "waiting"].contains(&state) { plain_reason(j) } else { off_spot(j).map(|(why, _)| why).unwrap_or_default() };
    // (How the job itself ended, where a machine ran it to its end: `passed`, `failed`, …; apart from `state`.)
    let ended = Some(job_ended(j).1.to_lowercase()).filter(|_| state == "done" && j["conclusion"].is_string());
    serde_json::json!({ "id": j["job_id"], "state": state, "ended": ended, "job": title, "in": sub, "runner": runner_text(j).1, "disk": job_disk(j), "choice": choice_said(j), "images": images_said(j), "start": job_start(j), "cloud": j["cloud"], "took": duration(j), "usd": job_cost(j).filter(|c| *c > 0.0), "why": why, "link": job_link(j), "at_ms": j["at_ms"] })
}

/// A run as commands say it: how it stands, what it is, how many of its jobs ran here, how long it took (ms, from its
/// first job queued to its last one over), what it cost, and the job that was over last.
pub fn run_words(r: &serde_json::Value) -> serde_json::Value {
    let (title, sub) = run_names(r);
    serde_json::json!({ "id": r["id"], "attempt": r["attempt"], "state": r["state"], "run": title, "in": sub, "jobs": r["jobs"], "job_ids": r["job_ids"], "took": r["took_ms"].as_u64().map(span), "took_ms": r["took_ms"],
        "usd": r["usd"], "last": r["last"], "last_said": Some(run_last(r)).filter(|l| !l.is_empty()), "link": r["link"], "at_ms": r["at_ms"] })
}

/// A control plane as commands say it.
pub fn plane_json(v: &PlaneView) -> serde_json::Value {
    serde_json::json!({ "id": v.plane.plane_id(), "cloud": v.plane.cloud(), "where": v.plane.place(), "url": v.plane.url(), "label": v.label(),
        "online": v.online, "version": v.version, "update_to": if v.outdated() { Some(DASHBOARD_VERSION) } else { None }, "ready": v.ready() })
}

/// A page's words without its markup: tags gone (a list's items one after another), entities as their characters.
fn plain(html: &str) -> String {
    let (mut out, mut tag) = (String::new(), None::<String>);
    for c in html.chars() {
        match (&mut tag, c) {
            (None, '<') => tag = Some(String::new()),
            (Some(t), '>') => { if t.starts_with("li") || t.starts_with("/p") || t.starts_with("/li") || t.starts_with("ul") { out.push(' ') } tag = None }
            (Some(t), c) => t.push(c),
            (None, c) => out.push(c),
        }
    }
    let out = out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What `superci status` says when SuperCI is signed in nowhere, and when its only sign-in was AWS's and has ended.
pub const NOT_SIGNED_IN: &str = "SuperCI is not signed in on this computer. Run `superci login` (it opens your browser).";
pub const AWS_ENDED: &str = "SuperCI's AWS sign-in has ended (AWS ends one after twelve hours at most). Run `superci login aws` to sign in again.";
/// The same of the other clouds' sign-ins, for what needs one of them.
pub const CF_ENDED: &str = "SuperCI's Cloudflare sign-in has expired. Run `superci login cloudflare` to sign in again.";
pub const MODAL_ENDED: &str = "Modal no longer accepts SuperCI's sign-in. Run `superci login modal` to sign in again.";

/// A day, as 2026-10-07 (UTC), from unix seconds.
fn day(unix: u64) -> String {
    let z = unix as i64 / 86_400 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + if m <= 2 { 1 } else { 0 })
}

/// When a key ends, from the name it is stored under in a control plane (`DEVICE_KEY_<unix seconds>_<random>`).
fn key_until(name: &str) -> u64 { name.strip_prefix("DEVICE_KEY_").and_then(|r| r.split('_').next().unwrap_or_default().parse().ok()).unwrap_or(0) }

/// The version from which control planes read their devices' keys as DEVICE_KEY_… and their launchers as LAUNCHERS.
/// One from before reads DASHBOARD_KEY_… and AGENTS: this device's key is handed to it under the old name too, until
/// its update moves them (see `renames`).
const NAMES_SINCE: &str = "0.19.13";

/// Which device a key just handed is on, for the CI's Keys (a command's key as a dashboard's): said once the control
/// plane takes it, a moment after it is written.
fn say_device(url: &str, key: &str) {
    tries(3, 1000, || cloudflare::plane_post(url, key, "/keys/hello", &serde_json::json!({ "device": computer_name() })).ok());
}

/// Whether the control plane at this address is from before `NAMES_SINCE`.
fn before_names(url: &str) -> bool { cloudflare::health(url).and_then(|h| h["version"].as_str().map(str::to_string)).is_some_and(|v| view::older(Some(&v), NAMES_SINCE)) }

/// This device's key under the name a control plane from before `NAMES_SINCE` reads.
fn old_key_name(name: &str) -> String { name.replacen("DEVICE_KEY_", "DASHBOARD_KEY_", 1) }

/// What updating a control plane from before `NAMES_SINCE` moves, from what it says (its status) and the names of its
/// settings: its launchers, written as LAUNCHERS before the new version is deployed (so that it never runs without
/// them); and, once that runs, AGENTS and every DASHBOARD_KEY_… taken out (each device's dashboard hands its key
/// anew as DEVICE_KEY_…).
fn renames(status: &serde_json::Value, names: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let puts = status["agents"].as_array().filter(|a| !a.is_empty()).map(|a| vec![("LAUNCHERS".to_string(), serde_json::Value::Array(a.clone()).to_string())]).unwrap_or_default();
    let drops = names.iter().filter(|n| *n == "AGENTS" || n.starts_with("DASHBOARD_KEY_")).cloned().collect();
    (puts, drops)
}

fn ago(at_ms: u64) -> String {
    let s = now_ms().saturating_sub(at_ms) / 1000;
    match s { 0..=59 => "just now".into(), 60..=3599 => format!("{}m ago", s / 60), 3600..=86399 => format!("{}h ago", s / 3600), _ => format!("{}d ago", s / 86400) }
}

fn region_options() -> String { region_select_options("us-east-1", &HashMap::new(), &closest(None), None) }

/// The same, of the regions an account can use: the first of them chosen (N. Virginia when it is one).
fn region_options_among(usable: &[&str]) -> String {
    let best = closest(None);
    usable.iter().enumerate().map(|(i, r)| format!(r#"<option value="{r}"{}>{} · {r}{}</option>"#, if i == 0 { " selected" } else { "" }, region_name(r), if *r == "us-east-1" { format!(" · {best}") } else { String::new() })).collect()
}

/// Why one place is best for runners: where the code hosts connected are (GitHub's Actions and gitlab.com both run
/// in the eastern US). A GitLab of your own is wherever it is: not counted.
fn closest(v: Option<&PlaneView>) -> String {
    let gh = v.is_none_or(|v| v.github);
    let gl = v.and_then(|v| v.gitlab_url()).is_some_and(|u| u.contains("://gitlab.com"));
    match (gh, gl) { (true, true) => "closest to GitHub and GitLab", (false, true) => "closest to GitLab", _ => "closest to GitHub" }.into()
}

/// Regions by their full names, with what this account may run there when known.
fn region_select_options(selected: &str, quotas: &HashMap<String, Result<u32>>, best: &str, look: Option<&aws::Look>) -> String {
    // The one chosen to begin with: the one asked for, or the first the account can use when it cannot use that.
    let selected = if region_unusable(look, selected).is_some() { REGIONS.iter().find(|r| region_unusable(look, r).is_none()).copied().unwrap_or(selected) } else { selected };
    REGIONS.iter().map(|r| {
        if let Some(why) = region_unusable(look, r) { return format!(r#"<option value="{r}" disabled>{} · {r} · {why}</option>"#, region_name(r)) }
        let quota = match quotas.get(*r) { Some(Ok(n)) => format!(" · up to {n} CPUs"), _ => String::new() };
        let best = if *r == "us-east-1" { format!(" · {best}") } else { String::new() };
        format!(r#"<option value="{r}"{}>{} · {r}{best}{quota}</option>"#, if *r == selected { " selected" } else { "" }, region_name(r))
    }).collect()
}

/// Why the AWS account in hand cannot run jobs in a region, in a few words (None: it can, or it has not been looked
/// at): its organization or its sign-in does not allow the region, or the image jobs run on is not there. A region
/// AWS gave no clear answer for (an error that is no refusal, as while a policy changes) is not taken as allowed,
/// unless AWS answered for none (it could not be reached: nothing is known of any).
fn region_unusable(look: Option<&aws::Look>, region: &str) -> Option<&'static str> {
    let look = look?;
    match look.regions.iter().find(|(r, _)| r == region) {
        Some((_, aws::Answer::Refused(r))) => Some(if matches!(r.by, superci_core::aws::By::Organization | superci_core::aws::By::OrganizationResources) { "not allowed by your AWS organization" } else { "not allowed for this sign-in" }),
        Some((_, aws::Answer::Unknown(_))) if look.unanswered().is_none() => Some("AWS did not say: Refresh under Control plane"),
        _ if look.regions_without_image().contains(&region) => Some("copy the runner image in first"),
        _ => None,
    }
}

/// An AWS refusal in plain words: whose rule it is, and what can be done about it. None: the error is something
/// else, and is said as it is.
pub fn plain_refusal(e: &str) -> Option<String> {
    use superci_core::aws::By;
    let r = superci_core::aws::refusal(e)?;
    // (A region's errors start with its name: "us-east-1: UnauthorizedOperation: …".)
    let region = e.split(": ").next().filter(|r| REGIONS.contains(r)).map(|r| format!(" in {} ({r})", region_short(r))).unwrap_or_default();
    let what = if r.action.is_empty() { "this".to_string() } else { format!("this ({})", r.action) };
    let policy = r.policy_id().map(|p| format!(" Its policy is {p}{}.", r.management_account().map(|a| format!(", managed from account {a}")).unwrap_or_default())).unwrap_or_default();
    Some(match r.by {
        By::Organization => format!("AWS refuses {what}{region} for the whole account: the AWS organization the account is in does not allow it, and nothing inside the account lifts that.{policy} `superci accounts retrieve aws` shows what the account allows."),
        By::OrganizationResources => format!("AWS refuses {what}{region}: a resource policy of the AWS organization the account is in does not allow it, and nothing inside the account lifts that.{policy}"),
        By::Identity => format!("The AWS role or user you signed in as may not do {what}{region}. Sign in with one that administers the account: `superci login aws`."),
        By::Boundary => format!("The AWS role or user you signed in as has a permissions boundary that does not allow {what}{region}. Sign in with one that administers the account: `superci login aws`."),
        By::Session => format!("This AWS sign-in's session does not allow {what}{region}. Sign in again: `superci login aws`."),
        By::Resource | By::Unsaid => return None,
    })
}

/// What an account's spot quota in a region means in jobs, and where to ask for more when it is low.
fn quota_text(region: &str, cpus: u32) -> String {
    let more = format!(r#" <a href="{}" target="_blank" rel="noopener">Request more from AWS ↗</a>"#, esc(&superci_core::aws::spot_quota_link(region)));
    match cpus {
        0..=3 => format!("Your account may run only {cpus} spot CPUs at once there, too few for a 4-CPU job.{more}"),
        4..=15 => format!("Your account may run {cpus} spot CPUs at once there ({} of 4 CPU).{more}", jobs_word(cpus / 4)),
        _ => format!("Your account may run up to {cpus} spot CPUs at once there ({} of 4 CPU).", jobs_word(cpus / 4)),
    }
}

/// A Cloudflare container's memory in GB, from its size as a job records it ("4cpu-12gb"); none for another.
fn container_gb(machine_type: &str) -> f64 { machine_type.split('-').nth(1).and_then(|m| m.strip_suffix("gb")).and_then(|m| m.parse().ok()).unwrap_or(0.0) }

fn jobs_word(n: u32) -> String { if n == 1 { "1 job".into() } else { format!("{n} jobs") } }

/// The mark: four broad petals, one solid shape (the same outline as on superci.dev).
const MARK: &str = concat!(r#"<svg class="mark" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d=""#, superci_core::mark_path!(), r#""/></svg>"#);

fn row(dot: &str, name: &str, sub: &str, end: &str) -> String {
    format!(r#"<div class="row"><span class="dot {dot}"></span><span><span class="row-name">{name}</span>{}</span><span class="row-end">{end}</span></div>"#,
        if sub.is_empty() { String::new() } else { format!(r#"<span class="row-sub">{sub}</span>"#) })
}

/// A state, as a label: "Connected", "Ready" (written with a capital first letter wherever it comes from).
fn pill(kind: &str, text: &str) -> String {
    let mut c = text.chars();
    let text = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    format!(r#"<span class="pill {kind}"><i></i>{}</span>"#, esc(&text))
}

/// A job's machine cost (estimated, the control plane's own estimate): its price per hour over the time it was paid for.
fn job_cost(j: &serde_json::Value) -> Option<f64> {
    j["machine_id"].as_str()?;
    let mut job: superci_core::plane::Job = serde_json::from_value(j.clone()).ok()?;
    // Jobs recorded before prices were: the price of the machines started then.
    if job.usd_per_hour.is_none() {
        job.usd_per_hour = match superci_core::plane::kind_of(&job.cloud) { "cloudflare" => Some(superci_core::plane::CLOUDFLARE_CONTAINER_USD_PER_HOUR), "modal" => Some(superci_core::plane::MODAL_SANDBOX_USD_PER_HOUR), _ => None };
        job.started_ms = job.started_ms.or(Some(job.at_ms));
    }
    // A Cloudflare container while its runner waits: memory and disk only (CPU is billed as used).
    if job.cloud == "cloudflare" && job.waiting_usd_per_hour.is_none() {
        job.waiting_usd_per_hour = job.usd_per_hour.map(|p| (p - job.cpu.unwrap_or(4) as f64 * 0.000020 * 3600.0).max(0.0));
    }
    job.usd_per_hour?;
    Some(superci_core::plane::job_usd(&job, now_ms()))
}

/// What the same job would have cost on GitHub's hosted runner of its size (from when it began, in whole minutes).
fn github_cost(j: &serde_json::Value) -> Option<f64> {
    let (start, end) = (j["started_ms"].as_u64()?, j["ended_ms"].as_u64()?);
    let cpu = j["cpu"].as_u64().unwrap_or(2) as u32;
    let arm64 = j["label"].as_str().is_some_and(|l| l.split('-').any(|p| p == "arm64"));
    Some((end.saturating_sub(start) as f64 / 60_000.0).ceil().max(1.0) * superci_core::plane::github_usd_per_minute(cpu, arm64))
}

/// A provider's month, as its cloud bills it when that is known: what SuperCI's jobs cost there, and what is
/// billed after the cloud's included usage or credits (Cloudflare's Workers Paid allowance, Modal's plan credits).
/// With a budget set, its figure ("$a of $b") already says the spend: only what is billed is added.
fn month_text(cloud: &str, spent: f64, show_spent: bool, cf_month: Option<(f64, f64)>, modal_month: Option<&modal::Month>) -> String {
    let (text, title) = match (cloud, cf_month, modal_month) {
        ("cloudflare", Some((metered, billed)), _) => {
            let share = if metered > 0.0 { billed * (spent / metered).min(1.0) } else { 0.0 };
            (if billed <= 0.0 { format!("{} this month · included", usd(spent)) } else { format!("{} this month · {} billed", usd(spent), usd(share)) },
                format!("Your Cloudflare account's containers this month: {} metered, {} billed after the usage Workers Paid includes", usd(metered), usd(billed)))
        }
        ("modal", _, Some(m)) => {
            let share = if m.metered > 0.0 { m.billed * (m.superci / m.metered).min(1.0) } else { 0.0 };
            (if m.billed <= 0.0 { format!("{} this month · credits", usd(m.superci)) } else { format!("{} this month · {} billed", usd(m.superci), usd(share)) },
                format!("As Modal bills it: SuperCI's apps {} of the workspace's {} metered; {} billed after credits", usd(m.superci), usd(m.metered), usd(m.billed)))
        }
        ("aws", _, _) => (format!("{} this month", usd(spent)), "At the prices AWS billed for each machine, its disk, address and data sent".to_string()),
        _ if spent > 0.0 => (format!("{} this month", usd(spent)), "Estimated from list prices".to_string()),
        _ => return String::new(),
    };
    let text = if show_spent { text } else { text.split(" · ").nth(1).map(str::to_string).unwrap_or_default() };
    if text.is_empty() { return String::new() }
    format!(r#"<span class="lim month" title="{}">{}</span>"#, esc(&title), esc(&text))
}

/// A job's cost, saying where it comes from: as the cloud metered it, at the prices it billed, or estimated (≈) until
/// the job ends and its cost is settled.
fn cost_text(j: &serde_json::Value, c: f64) -> String {
    match j["cost_from"].as_str() {
        Some("measured") => format!(r#"<span title="As {} metered it">{}</span>"#, if j["cloud"] == "cloudflare" { "Cloudflare" } else { "the cloud" }, usd_fine(c)),
        Some("prices") => format!(r#"<span title="At the prices AWS billed for its time, disk and address">{}</span>"#, usd_fine(c)),
        _ => {
            let when = match j["cloud"].as_str() {
                Some("cloudflare") => "Estimated until Cloudflare's metering has it: a few minutes after it ends, read while signed in to Cloudflare",
                Some("aws") => "Estimated until it ends; then settled at the prices AWS billed",
                _ => "Estimated from list prices",
            };
            format!(r#"<span class="est" title="{when}">≈{}</span>"#, usd_fine(c))
        }
    }
}

/// A job's cost to the hundredth of a cent under a dollar ($0.0123, $0.03, $0.50).
fn usd_fine(v: f64) -> String {
    if v >= 1.0 { return usd(v) }
    let s = format!("{:.4}", v.max(0.0));
    let t = s.trim_end_matches('0');
    format!("${}", if t.len() < s.len() - 2 { &s[..s.len() - 2] } else { t })
}

fn usd(v: f64) -> String { let v = v.max(0.0) + 0.0; if v > 0.0 && v < 0.01 { format!("${v:.4}") } else { format!("${v:.2}") } }

fn duration(j: &serde_json::Value) -> String {
    let (Some(start), Some(end)) = (j["started_ms"].as_u64(), j["ended_ms"].as_u64()) else { return String::new() };
    let s = end.saturating_sub(start) / 1000;
    if s < 60 { format!("{s}s") } else if s % 60 == 0 || s >= 600 { format!("{}m", s / 60) } else { format!("{}m {}s", s / 60, s % 60) }
}

/// Jobs as a table: what the job is (its name, repository and workflow), the runner it got, how long it ran, what it
/// cost, how it ended, and when. Each row opens the job on its code host.
fn job_table(jobs: &[serde_json::Value]) -> String {
    let rows = jobs.iter().map(|j| {
        let s = |k: &str| j[k].as_str().unwrap_or_default().to_string();
        let state = s("state");
        let (kind, word) = job_ended(j);
        let (title, sub) = job_names(j);
        let host = if j["provider"] == "gitlab" { logo("gitlab", 14) } else { logo("github", 14) };
        let (runner_logo, runner) = runner_text(j);
        let runner = if runner.is_empty() { r#"<span class="faint">—</span>"#.to_string() } else { format!("{}<span>{}</span>", logo(&runner_logo, 16), esc(&runner)) };
        let took = duration(j);
        let cost = job_cost(j).filter(|c| *c > 0.0).map(|c| cost_text(j, c)).unwrap_or_default();
        let why = if (kind == "bad" || state == "waiting") && !plain_reason(j).is_empty() { format!(r#"<small class="jt-why {kind}">{}</small>"#, esc(&plain_reason(j))) }
            else { off_spot(j).map(|(why, _)| format!(r#"<small class="jt-why open">{}</small>"#, esc(&why)))
                // (Or that its machine was not the first choice: quietly, it is no fault.)
                .or_else(|| choice_said(j).map(|said| format!(r#"<small class="jt-why held">{}</small>"#, esc(&said)))).unwrap_or_default() };
        format!(r#"<a class="jt-row" href="{}"><span class="jt-job"><strong>{}</strong><small>{host}{}</small>{why}</span><span class="jt-runner">{runner}</span><span class="jt-num">{took}</span><span class="jt-num">{cost}</span><span class="jt-state {kind}"><i></i>{word}</span><span class="jt-when">{}</span></a>"#,
            esc(&job_page_link(j)), esc(&title), esc(&sub), ago(j["at_ms"].as_u64().unwrap_or_default()))
    }).collect::<String>();
    format!(r#"<div class="jt"><div class="jt-head"><span>Job</span><span>Runner</span><span class="jt-num">Time</span><span class="jt-num">Cost</span><span>Status</span><span class="jt-when"></span></div>{rows}</div>"#)
}

/// A span of time in words: "45s", "6m 12s", "25m".
fn span(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 { format!("{s}s") } else if s % 60 == 0 || s >= 600 { format!("{}m", s / 60) } else { format!("{}m {}s", s / 60, s % 60) }
}

/// Jobs by the run they are of (a workflow run on GitHub, a pipeline on GitLab; an attempt of one apart, where the
/// control plane kept which), newest first. Each with how it stands, how long it took from its first job queued to
/// its last one over, what its machines cost together, and the job that was over last: the one the run waited for.
/// Only the jobs SuperCI ran are counted: a run's jobs on other runners are not known here.
pub fn runs_of(jobs: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let key = |j: &serde_json::Value| (j["repo"].as_str().unwrap_or_default().to_string(), j["run_id"].as_u64().unwrap_or_default(), j["attempt"].as_u64().unwrap_or_default());
    let mut order: Vec<(String, u64, u64)> = vec![];
    let mut by: HashMap<(String, u64, u64), Vec<&serde_json::Value>> = HashMap::new();
    for j in jobs.iter().filter(|j| j["run_id"].as_u64().is_some_and(|r| r > 0)) {
        let k = key(j);
        if !by.contains_key(&k) { order.push(k.clone()) }
        by.entry(k).or_default().push(j);
    }
    let now = now_ms();
    let mut runs: Vec<serde_json::Value> = order.into_iter().map(|k| {
        let rows = &by[&k];
        let n = |j: &serde_json::Value, f: &str| j[f].as_u64();
        let words: Vec<&str> = rows.iter().map(|j| job_ended(j).1).collect();
        let going = words.iter().any(|w| ["Running", "Waiting", "Starting"].contains(w));
        let state = if going { "running" } else if words.contains(&"Failed") { "failed" } else if words.contains(&"Cancelled") { "cancelled" }
            else if words.iter().all(|w| ["Passed", "Skipped"].contains(w)) { "passed" } else { "done" };
        let at = rows.iter().filter_map(|j| n(j, "at_ms")).min().unwrap_or_default();
        // (A job that never got a machine has no end of its own: the run is over when its last machine was.)
        let end = if going { None } else { rows.iter().filter_map(|j| n(j, "ended_ms")).max() };
        let took_ms = match end { Some(end) => Some(end.saturating_sub(at)), None if going => Some(now.saturating_sub(at)), None => None };
        let usd: f64 = rows.iter().filter_map(|j| job_cost(j)).sum::<f64>() + 0.0;
        // The one over last, with how long it waited for a machine and how long it ran.
        let last = rows.iter().filter(|j| n(j, "ended_ms").is_some() && n(j, "started_ms").is_some()).max_by_key(|j| n(j, "ended_ms")).map(|j| {
            let (queued, started, ended) = (n(j, "at_ms").unwrap_or_default(), n(j, "started_ms").unwrap_or_default(), n(j, "ended_ms").unwrap_or_default());
            serde_json::json!({ "id": j["job_id"], "job": job_names(j).0, "waited_ms": started.saturating_sub(queued), "ran_ms": ended.saturating_sub(started) })
        });
        let mut ids: Vec<u64> = rows.iter().filter_map(|j| n(j, "job_id")).collect();
        ids.sort_unstable(); ids.dedup();
        let first = rows[0];
        let link = if first["provider"] == "local" { String::new() } else if first["provider"] == "gitlab" { format!("{}/{}/-/pipelines/{}", first["gitlab_url"].as_str().unwrap_or("https://gitlab.com"), k.0, k.1) }
            else { format!("{}/{}/actions/runs/{}{}", first["github_web"].as_str().unwrap_or("https://github.com"), k.0, k.1, if k.2 > 1 { format!("/attempts/{}", k.2) } else { String::new() }) };
        serde_json::json!({ "id": k.1, "attempt": Some(k.2).filter(|a| *a > 0), "repo": k.0, "workflow": first["workflow"], "provider": first["provider"], "state": state, "jobs": ids.len(), "job_ids": ids,
            "took_ms": took_ms, "usd": Some(usd).filter(|c| *c > 0.0), "last": last, "link": link, "at_ms": at })
    }).collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r["at_ms"].as_u64().unwrap_or_default()));
    runs
}

/// What is said of the job a run waited for: "Last to end: E2E (1/2), 6m 12s after 1m 5s waiting for a machine".
pub fn run_last(r: &serde_json::Value) -> String {
    if r["jobs"].as_u64().unwrap_or_default() < 2 || !r["last"].is_object() { return String::new() }
    let (waited, ran) = (r["last"]["waited_ms"].as_u64().unwrap_or_default(), r["last"]["ran_ms"].as_u64().unwrap_or_default());
    format!("Last to end: {}, {}{}", r["last"]["job"].as_str().unwrap_or_default(), span(ran), if waited >= 20_000 { format!(" after {} waiting for a machine", span(waited)) } else { String::new() })
}

/// A run's name and the line under it: its workflow, with its repository (and which attempt, after the first).
pub fn run_names(r: &serde_json::Value) -> (String, String) {
    let repo = r["repo"].as_str().unwrap_or_default();
    let short = repo.rsplit('/').next().unwrap_or(repo).to_string();
    let again = r["attempt"].as_u64().filter(|a| *a > 1).map(|a| format!(" · attempt {a}")).unwrap_or_default();
    // (A GitLab job keeps its stage where a GitHub job keeps its workflow: a pipeline goes by its number.)
    if r["provider"] == "gitlab" { return (format!("Pipeline {}", r["id"].as_u64().unwrap_or_default()), format!("{short}{again}")) }
    match r["workflow"].as_str().filter(|w| !w.is_empty()) { Some(w) => (w.to_string(), format!("{short}{again}")), None => (short, format!("{}{again}", repo.split('/').next().unwrap_or_default())) }
}

/// Runs as a table, like the jobs': what the run is, how many jobs of it ran here, how long it took, what it cost,
/// how it stands, and when. Each row opens the run's page here.
fn run_table(runs: &[serde_json::Value]) -> String {
    let rows = runs.iter().map(|r| {
        let (kind, word) = match r["state"].as_str().unwrap_or_default() { "passed" => ("good", "Passed"), "failed" => ("bad", "Failed"), "cancelled" => ("", "Cancelled"), "running" => ("accent", "Running"), _ => ("good", "Done") };
        let (title, sub) = run_names(r);
        let host = if r["provider"] == "gitlab" { logo("gitlab", 14) } else { logo("github", 14) };
        let last = Some(run_last(r)).filter(|l| !l.is_empty()).map(|l| format!(r#"<small class="jt-why held">{}</small>"#, esc(&l))).unwrap_or_default();
        let count = r["jobs"].as_u64().unwrap_or_default();
        format!(r#"<a class="jt-row" href="{}"><span class="jt-job"><strong>{}</strong><small>{host}{}</small>{last}</span><span class="jt-runner count">{count} {}</span><span class="jt-num">{}</span><span class="jt-num">{}</span><span class="jt-state {kind}"><i></i>{word}</span><span class="jt-when">{}</span></a>"#,
            esc(&run_page_link(r)), esc(&title), esc(&sub), if count == 1 { "job" } else { "jobs" }, r["took_ms"].as_u64().map(span).unwrap_or_default(), r["usd"].as_f64().map(usd_fine).unwrap_or_default(), ago(r["at_ms"].as_u64().unwrap_or_default()))
    }).collect::<String>();
    format!(r#"<div class="jt"><div class="jt-head"><span>Run</span><span>Jobs here</span><span class="jt-num">Time</span><span class="jt-num">Cost</span><span>Status</span><span class="jt-when"></span></div>{rows}</div>"#)
}

/// A job's name for people, and the line under it: "four" with "superci-bench · sizes" (jobs from before names were
/// kept: the repository).
fn job_names(j: &serde_json::Value) -> (String, String) {
    let repo = j["repo"].as_str().unwrap_or_default();
    let short = repo.rsplit('/').next().unwrap_or(repo).to_string();
    let name = j["name"].as_str().unwrap_or_default();
    let workflow = j["workflow"].as_str().unwrap_or_default();
    if name.is_empty() { return (short, repo.split('/').next().unwrap_or_default().to_string()) }
    (name.to_string(), if workflow.is_empty() { short } else { format!("{short} · {workflow}") })
}

/// Seconds in words, for a start's steps: "0.4 s", "36 s", "1m 12s".
fn secs(ms: u64) -> String { if ms < 9_950 { format!("{:.1} s", ms as f64 / 1000.0) } else if ms < 60_000 { format!("{} s", (ms + 500) / 1000) } else { span(ms) } }

/// What AWS was asked for a job's machine, in a few words: "no spot m8a.8xlarge in 6 zones; spot m7a.8xlarge in us-east-1a",
/// or "no spot m8a.8xlarge; spot r8a.8xlarge in us-east-1d, AWS's choice of 14 pools".
fn asks_said(j: &serde_json::Value) -> String {
    let asks = j["asks"].as_array().cloned().unwrap_or_default();
    let mut said: Vec<String> = vec![];
    let mut i = 0;
    while i < asks.len() {
        let a = &asks[i];
        let (kind, spot, what) = (a["kind"].as_str().unwrap_or_default(), a["spot"] == true, a["said"].as_str().unwrap_or_default());
        let market = if spot { "spot " } else { "on-demand " };
        // One request for any of several pools (see `Ask::pools`): what AWS chose among them, or that none had one.
        if let Some(pools) = a["pools"].as_u64().filter(|p| *p > 0) {
            said.push(match what {
                "started" => format!("{market}{kind}{}, AWS's choice of {pools} pools", a["zone"].as_str().filter(|z| !z.is_empty()).map(|z| format!(" in {z}")).unwrap_or_default()),
                "MaxSpotInstanceCountExceeded" | "VcpuLimitExceeded" => format!("your account's limit refused {pools} other pools"),
                "InsufficientInstanceCapacity" => format!("no spot machine in {pools} other pools"),
                other => format!("{other} for {pools} other pools"),
            });
            i += 1; continue
        }
        if what == "started" { said.push(format!("{market}{kind}{}", a["zone"].as_str().filter(|z| !z.is_empty()).map(|z| format!(" in {z}")).unwrap_or_default())); i += 1; continue }
        // The same answer for the same type, zone after zone: said once.
        let alike = asks[i..].iter().take_while(|b| b["kind"] == a["kind"] && b["spot"] == a["spot"] && b["said"] == a["said"]).count();
        let why = match what { "InsufficientInstanceCapacity" => "no", "MaxSpotInstanceCountExceeded" | "VcpuLimitExceeded" => "your account's limit refused", "RequestLimitExceeded" => "AWS took no more requests for", other => other };
        said.push(format!("{why} {market}{kind}{}", if alike > 1 { format!(" in {alike} zones") } else { String::new() }));
        i += alike;
    }
    said.join("; ")
}

/// The machine types AWS had no spot machine of before the one a job got, when it got another: its machine was not
/// the first choice (an older or a larger type, which changes how long the job takes and what it costs).
fn passed_over(j: &serde_json::Value) -> Vec<String> {
    let got = j["machine_type"].as_str().unwrap_or_default();
    let mut kinds: Vec<String> = vec![];
    let mut said: Vec<String> = vec![];
    for a in j["asks"].as_array().into_iter().flatten() {
        let kind = a["kind"].as_str().unwrap_or_default();
        if a["spot"] == true && a["said"] == "InsufficientInstanceCapacity" && kind != got && !kinds.iter().any(|k| k == kind) {
            kinds.push(kind.to_string());
            // (Where, and at what price, when the control plane kept them: why a pricier one was taken.)
            let zone = a["zone"].as_str().filter(|z| !z.is_empty()).map(|z| format!(" in {z}")).unwrap_or_default();
            let usd = a["usd"].as_f64().filter(|u| *u > 0.0).map(|u| format!(" (${u:.3} an hour)")).unwrap_or_default();
            said.push(format!("{kind}{zone}{usd}"));
        }
    }
    if got.is_empty() || j["on_demand"] == true { vec![] } else { said }
}

/// What a job had from the cache's Docker images and what it pulled, as its machine said after it: "103 layers from
/// the cache (2.7 GB); 12 pulled (300 MB), kept for the next job". None: it said nothing (not a machine that keeps
/// images, or one from before), or it used no images.
pub fn images_said(j: &serde_json::Value) -> Option<String> {
    let i = &j["images"];
    if let Some(e) = i["error"].as_str() { return Some(format!("not kept ({e})")) }
    let n = |k: &str| i[k].as_u64().unwrap_or(0);
    let layers = |c: u64| if c == 1 { "1 layer".to_string() } else { format!("{c} layers") };
    let from = (n("served") > 0).then(|| format!("{} from the cache ({})", layers(n("served")), size_words(n("served_bytes"))));
    // (What the next job has from it: what was uploaded, and what the cache had already. Pulled and neither: said.)
    let kept = match (n("uploaded") + n("had"), n("pulled")) {
        // (Said as the job ended: its machine is keeping them.)
        _ if i["keeping"] == true => "being kept for the next job".to_string(),
        (0, _) => "none kept for the next job".to_string(),
        (k, all) if k >= all => "kept for the next job".to_string(),
        (k, _) => format!("{k} of them kept for the next job ({})", size_words(n("uploaded_bytes") + n("had_bytes"))),
    };
    // Why the rest was not kept, when the machine said.
    let mut why = vec![];
    if n("missing") > 0 { why.push(format!("{} with no whole file on the machine ({})", layers(n("missing")), size_words(n("missing_bytes")))) }
    if n("refused") > 0 { why.push(format!("{} the cache would not take", layers(n("refused")))) }
    if n("failed") > 0 { why.push(format!("{} not uploaded ({})", layers(n("failed")), i["note"].as_str().filter(|s| !s.is_empty()).unwrap_or("failed"))) }
    let not_kept = if why.is_empty() { String::new() } else { format!(": {}", why.join(", ")) };
    let pulled = (n("pulled") > 0).then(|| format!("{} pulled ({}), {kept}{not_kept}", layers(n("pulled")), size_words(n("pulled_bytes"))));
    // (The store not switched in on its machine: everything was pulled, and why is said.)
    let off = i["off"].as_str().filter(|s| !s.is_empty()).map(|w| format!("the cache was not set up on its machine ({w})"));
    let pulled = match (pulled, off) { (Some(p), Some(o)) => Some(format!("{p}; {o}")), (None, Some(o)) => Some(o), (p, None) => p };
    match (from, pulled) { (None, None) => None, (Some(a), None) | (None, Some(a)) => Some(a), (Some(a), Some(b)) => Some(format!("{a}; {b}")) }
}

/// The same as a job's note: "Not the first choice: AWS had no spot m8a.8xlarge in us-east-1a ($0.135 an hour)".
pub fn choice_said(j: &serde_json::Value) -> Option<String> {
    let kinds = passed_over(j);
    let list = match kinds.as_slice() { [] => return None, [one] => one.clone(), [most @ .., last] => format!("{} or {last}", most.join(", ")) };
    Some(format!("Not the first choice: AWS had no spot {list}"))
}

/// How a job's start went, step by step (see `Job::asks` and `MachineStart` in the control plane): from GitHub
/// making the job to the job running, with how long each step took (ms). What the control plane kept is all that is
/// said: a job from before, or a machine that could not say its part, has fewer steps.
pub fn job_start(j: &serde_json::Value) -> serde_json::Value {
    let n = |v: &serde_json::Value| v.as_u64().filter(|t| *t > 0);
    let (Some(heard), started) = (n(&j["at_ms"]), n(&j["started_ms"])) else { return serde_json::Value::Null };
    let mut steps: Vec<(String, u64, String)> = vec![];
    let mut step = |what: &str, from: u64, to: u64, more: String| if to >= from && to - from < 86_400_000 { steps.push((what.to_string(), to - from, more)) };
    let queued = n(&j["queued_ms"]).filter(|q| *q <= heard);
    // (A run from a computer's job is the control plane's own: no GitHub tells of it or gives it out.)
    let local = j["provider"] == "local";
    if let Some(q) = queued.filter(|_| !local) { step("GitHub told SuperCI of the job", q, heard, String::new()) }
    let Some(launched) = n(&j["launched_ms"]).filter(|_| j["machine_id"].is_string()) else {
        return serde_json::json!({ "steps": steps.iter().map(|(what, ms, more)| serde_json::json!({ "what": what, "ms": ms, "more": more })).collect::<Vec<_>>(), "asked": asks_said(j) })
    };
    let asked = asks_said(j);
    // (A wait for room under the account's spot limit first: said apart, as SuperCI's, not AWS's.)
    let asked_at = match n(&j["spot_waited_ms"]) { Some(w) if heard + w <= launched => { step("waited for room under your AWS spot limit", heard, heard + w, String::new()); heard + w } _ => heard };
    step(if j["cloud"] == "aws" { "AWS gave a machine" } else { "its machine was asked for" }, asked_at, launched, asked.clone());
    let m = &j["machine"];
    match (n(&m["script_ms"]), m["boot_s"].as_f64()) {
        (Some(script), Some(boot)) => {
            let kernel = script.saturating_sub((boot * 1000.0) as u64);
            step("the machine was made and switched on", launched, kernel.max(launched), String::new());
            step("it booted", kernel, script, String::new());
            let (read, ready, listening) = (n(&m["read_ms"]), n(&m["ready_ms"]), n(&m["listening_ms"]));
            if let Some(ready) = ready {
                // (GitHub's address for the runner refusing it at first is waited for here, beside the reads.)
                let refused = m["github_wait_s"].as_u64().map(|s| format!("GitHub refused the first connection; reachable after {s} s")).unwrap_or_default();
                match read { Some(read) => { step("its disk and Docker were set up", script, read, String::new()); step("the runner's files were read from the image", read, ready, refused) }, None => step("its start-up script ran", script, ready, String::new()) }
                match (listening, started) {
                    (Some(l), Some(s)) if local => { step("the runner started and reached the control plane", ready, l, String::new()); step("the control plane gave it the job", l, s.max(l), String::new()) }
                    (Some(l), Some(s)) => { step("the runner started and reached GitHub", ready, l, String::new()); step("GitHub gave it the job", l, s.max(l), String::new()) }
                    (Some(l), None) if local => step("the runner started and reached the control plane", ready, l, String::new()),
                    (Some(l), None) => step("the runner started and reached GitHub", ready, l, String::new()),
                    (None, Some(s)) => step("the runner started and took the job", ready, s.max(ready), String::new()),
                    _ => {}
                }
            }
        }
        _ => if let Some(s) = started { step("its machine booted and its runner took the job", launched, s, String::new()) },
    }
    let total = started.map(|s| s.saturating_sub(queued.unwrap_or(heard)));
    let mut start = serde_json::json!({ "ms": total, "steps": steps.iter().map(|(what, ms, more)| serde_json::json!({ "what": what, "ms": ms, "more": more })).collect::<Vec<_>>(), "asked": asked });
    // What its runner logged while it was slow to reach GitHub (see `MachineStart::log`).
    if let Some(log) = m["log"].as_str().filter(|l| !l.is_empty()) { start["runner_log"] = log.into() }
    start
}

/// The same for a person: a line for the whole, and one for each step.
pub fn job_start_lines(j: &serde_json::Value) -> Vec<String> {
    let start = job_start(j);
    let steps = start["steps"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() { return vec![] }
    let mut out = vec![match start["ms"].as_u64() { Some(ms) => format!("Its start: {} from queued to running", secs(ms)), None => "Its start, so far:".to_string() }];
    out.extend(steps.iter().map(|s| {
        let more = s["more"].as_str().filter(|m| !m.is_empty()).map(|m| format!(" ({m})")).unwrap_or_default();
        format!("  {:>7}  {}{more}", secs(s["ms"].as_u64().unwrap_or_default()), s["what"].as_str().unwrap_or_default())
    }));
    // A runner that was slow to reach GitHub (a run from a computer's: this control plane): what it logged meanwhile,
    // as its machine sent it.
    if let Some(log) = start["runner_log"].as_str() {
        out.extend(runner_log_lines(log, j["provider"] == "local"));
    }
    out
}

/// What a slow runner logged while it reached GitHub (`local`: this control plane), as its machine sent it: its waits,
/// warnings and errors (a fixed wait of 30 seconds is its retry after a request that failed).
pub fn runner_log_lines(log: &str, local: bool) -> Vec<String> {
    let mut out = vec![format!("What its runner logged while it was slow to reach {} (its waits, warnings and errors):", if local { "the control plane" } else { "GitHub" })];
    out.extend(log.lines().map(|l| format!("  {l}")));
    out
}

/// A job's disk on AWS, for commands: its size, its speed (what AWS includes when none was kept: 125 MiB a second,
/// 3,000 IOPS; the IOPS a speed needs when only the speed was), and whether its files were in memory or on a local
/// disk. Nothing for a job with no disk kept (another cloud's, or one that never got a machine).
fn job_disk(j: &serde_json::Value) -> serde_json::Value {
    let Some(gb) = j["disk_gb"].as_u64().filter(|_| j["cloud"] == "aws") else { return serde_json::Value::Null };
    let mibs = j["disk_mibs"].as_u64().unwrap_or(superci_core::spec::DISK_MIBS_INCLUDED as u64);
    let iops = j["disk_iops"].as_u64().unwrap_or((mibs * 4).max(superci_core::spec::DISK_IOPS_INCLUDED as u64));
    serde_json::json!({ "gb": gb, "mibs": mibs, "iops": iops, "ramdisk": j["ramdisk"] == true, "local_disk": j["local_disk"] == true })
}

/// The runner a job got, in words: its provider's logo and its size ("4 CPU · 12 GB"), or the AWS machine's type.
fn runner_text(j: &serde_json::Value) -> (String, String) {
    let cloud = j["cloud"].as_str().unwrap_or_default();
    let kind = j["machine_type"].as_str().unwrap_or_default();
    match cloud {
        "" => (String::new(), String::new()),
        // From before computers were set aside (coming soon).
        "machine" => ("machine".into(), "Your computer".into()),
        // An on-demand machine says so (its label asked, or the order sent it there).
        // (Its disk too: its size when it is not the standard 60 GB, its speed when above what AWS includes (it is in
        // the job's price), and where the job's files went when its label asked for memory or a local disk.)
        "aws" => ("aws".into(), format!("{}{}{}{}", match j["cpu"].as_u64() { Some(c) if !kind.is_empty() => format!("{c} CPU · {kind}"), _ => kind.to_string() },
            match (j["disk_gb"].as_u64(), j["disk_mibs"].as_u64()) { _ if kind.is_empty() => String::new(), (Some(60) | None, Some(s)) => format!(" · {s} MiB/s disk"), (Some(d), Some(s)) => format!(" · {d} GB disk at {s} MiB/s"), (Some(d), None) if d != 60 => format!(" · {d} GB disk"), _ => String::new() },
            if kind.is_empty() { "" } else if j["ramdisk"] == true { " · RAM disk" } else if j["local_disk"] == true { " · local disk" } else { "" },
            if j["on_demand"] == true && !kind.is_empty() { " · on-demand" } else { "" })),
        _ => {
            // "4cpu-12gb" as "4 CPU · 12 GB".
            let size = kind.split_once("cpu-").and_then(|(c, r)| Some(format!("{c} CPU · {} GB", r.strip_suffix("gb")?)));
            (cloud.to_string(), size.unwrap_or_else(|| cloud_name(cloud).to_string()))
        }
    }
}

/// An AWS job's machine for its page, each in words of its own: the machine ("c8a.xlarge · 4 CPU · RAM disk"), how it
/// was bought ("Spot", or "On-demand" and why), and where ("N. Virginia · us-east-1d"). None: not on an AWS machine
/// (yet).
fn aws_machine(j: &serde_json::Value) -> Option<(String, String, String)> {
    if j["cloud"] != "aws" || j["machine_type"].as_str().is_none_or(str::is_empty) { return None }
    // (The type first, then its CPUs and its disk as the jobs' list says them.)
    let (kind, text) = (j["machine_type"].as_str().unwrap_or_default(), runner_text(j).1);
    let machine = match j["cpu"].as_u64() {
        Some(c) => format!("{kind} · {c} CPU{}", text.strip_prefix(&format!("{c} CPU · {kind}")).unwrap_or_default().trim_end_matches(" · on-demand")),
        None => text.trim_end_matches(" · on-demand").to_string(),
    };
    let market = if j["on_demand"] == true {
        match off_spot(j) { Some((why, _)) => format!("On-demand · {}", why.strip_prefix("On-demand: ").unwrap_or(&why)), None => "On-demand".to_string() }
    } else if j["spot_again"] == true { "Spot · an on-demand machine did not fit your AWS account's on-demand limit".to_string() } else {
        // Why this one: its price, and the cheaper choice it was not, when that had no machine; and what was not asked
        // for at all, priced over an on-demand machine.
        let asks = j["asks"].as_array().cloned().unwrap_or_default();
        let usd = |a: &serde_json::Value| a["usd"].as_f64().filter(|u| *u > 0.0);
        let got = asks.iter().rev().find(|a| a["spot"] == true && a["said"] == "started").and_then(usd);
        let first = asks.iter().find(|a| a["spot"] == true && a["said"] != "started" && a["pools"].as_u64().unwrap_or(0) == 0 && usd(a).is_some());
        let mut said = vec!["Spot".to_string()];
        if let Some(u) = got { said.push(format!("${u:.4} an hour")) }
        if let (Some(f), Some(fu)) = (first, first.and_then(usd)) {
            if got.is_none_or(|g| fu < g) { said.push(format!("its first choice, {} in {} at ${fu:.4}, had no machine", f["kind"].as_str().unwrap_or_default(), f["zone"].as_str().unwrap_or_default())) }
        }
        let out: Vec<&str> = j["priced_out"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).collect();
        if !out.is_empty() { said.push(format!("not asked, priced over an on-demand machine: {}", out.join(", "))) }
        said.join(" · ")
    };
    let region = j["region"].as_str().unwrap_or_default();
    let place = match (region, j["zone"].as_str().filter(|z| !z.is_empty())) {
        ("", _) => String::new(),
        (r, Some(z)) => format!("{} · {z}", region_short(r)),
        (r, None) => format!("{} · {r}", region_short(r)),
    };
    Some((machine, market, place))
}

/// Why a job that asked for a spot machine ran on an on-demand one (which costs about twice as much): what AWS said
/// of spot, in plain words, and the region where the account's spot limit was reached, if that was it.
fn off_spot(j: &serde_json::Value) -> Option<(String, Option<String>)> {
    // (Or why one kept off spot machines is on one after all: its on-demand machine did not fit the account's limit.)
    if j["spot_again"] == true && j["on_demand"] != true && (j["cloud"] == "aws" || j["cloud"] == "") { return Some(("On a spot machine again: an on-demand one does not fit your AWS account's on-demand limit".into(), None)) }
    if j["on_demand"] != true || j["cloud"] != "aws" { return None }
    // Run again after AWS took its spot machine back: SuperCI's own choice, whatever spot machines there were.
    if j["after_reclaim"] == true { return Some(("On-demand: run again after AWS took back its spot machine".into(), None)) }
    let said = j["spot_refused"].as_str()?;
    // The account's AWS organization allows no spot machines: no limit to raise, and nothing of AWS's to wait for.
    if said.contains(superci_core::plane::SPOT_NOT_ALLOWED) { return Some(("On-demand: your AWS organization does not allow spot machines".into(), None)) }
    // AWS took no more requests for machines just then (many asked for at once, in an account AWS lets ask for few).
    if said.contains("RequestLimitExceeded") { return Some(("On-demand: AWS refused the spot request for its rate (many machines asked for at once)".into(), None)) }
    let limited = said.split("; ").find(|a| a.contains("MaxSpotInstanceCountExceeded")).and_then(|a| a.split(':').next());
    Some(match limited {
        Some(region) => (format!("On-demand: your AWS spot limit in {} was reached{}", region_short(region), if j["spot_ending"] == true { " (machines AWS was still ending count too)" } else { "" }), Some(region.to_string())),
        None => ("On-demand: AWS had no spot machine for it".into(), None),
    })
}

/// One line above the jobs when some of the last day's ran on on-demand machines because the account's spot limit
/// was reached: how many, and where to ask AWS for more.
fn spot_limit_notice(jobs: &[serde_json::Value]) -> String {
    let since = now_ms().saturating_sub(86_400_000);
    let hit: Vec<String> = jobs.iter().filter(|j| j["at_ms"].as_u64().is_some_and(|at| at > since)).filter_map(|j| off_spot(j)?.1).collect();
    let Some(region) = hit.first() else { return String::new() };
    format!(r#"<div class="notice warn"><span>{} in the last day ran on on-demand machines, at about twice the price: your AWS account's spot limit in {} was reached. <a href="{}" target="_blank" rel="noopener">Request more from AWS ↗</a></span></div>"#,
        jobs_word(hit.len() as u32), esc(&region_short(region)), esc(&superci_core::aws::spot_quota_link(region)))
}

/// Why a job did not run (or waits), in plain words: what the control plane recorded, without its machinery.
fn plain_reason(j: &serde_json::Value) -> String {
    let state = j["state"].as_str().unwrap_or_default();
    let place = cloud_name(j["cloud"].as_str().unwrap_or_default()).to_string();
    let Some(e) = j["error"].as_str() else {
        return match state {
            "swept" => "Its machine never started the job".into(),
            "orphan" => "Cancelled while its machine came up; the machine is ended unless it takes another job".into(),
            "waiting" => "Waiting for a free runner".into(),
            // It ran: its steps say how it went (a failed one in its log).
            "done" | "running" => String::new(),
            _ => "It did not start".into(),
        }
    };
    let e = e.trim_start_matches("waiting: ");
    if e.contains("cannot be kept running") { return format!("{} could not keep its container running", if place.is_empty() { "Cloudflare".into() } else { place }) }
    if e.contains("did not start") && e.contains("container") { return "Cloudflare did not start its container".into() }
    // The first clause, without wrappers (`runner container: 502 …`, `JsValue(…)`).
    let e = e.split("JsValue(").next().unwrap_or(e).trim_end_matches([':', ' ']);
    let e = e.strip_prefix("runner container: ").unwrap_or(e);
    let e = e.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
    let mut out: String = e.chars().take(110).collect();
    if e.chars().count() > 110 { out.push('…') }
    let mut c = out.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

impl Dashboard {
    pub fn new() -> Self {
        // For a machine with no browser: a Cloudflare API token given to SuperCI by name instead of signing in
        // (Cloudflare's and Modal's own variables are not read).
        let cf = std::env::var("SUPERCI_CLOUDFLARE_TOKEN").ok().filter(|t| !t.trim().is_empty()).map(|t| cloudflare::Session::from_token(&t));
        let modal = modal::Session::from_env();
        Dashboard { key: random_token(24), base: format!("http://localhost:{}", oauth_port()), planes: vec![], selected: 0, looked_cf: false, looked_aws: false, looked_modal: false,
            status_key: folder_key(), status_secret: format!("DEVICE_KEY_{}_{}", now_ms() / 1000 + 12 * 3600, superci_core::crypto::random_id(6).to_uppercase()), keyed: HashSet::new(), cf_given: cf.is_some(), modal_given: modal.is_some(), store: None, named: String::new(), about: Default::default(), picture_asked_ms: 0, picture_owner: String::new(), kept_as: String::new(), kept_plane: None, found: vec![], found_about: HashMap::new(), owners_written: HashMap::new(), hello_sent: HashSet::new(), keep_failed: false, aws_ended: false, cf_ended: false, modal_ended: false, unanswered: vec![], aws_came: false, adding: false, aws_others: vec![], task: None, commanding: false, preloaded: None, notice: None, cf, cf_pending: None, cf_accounts: vec![], aws: None, aws_pending: None, aws_asked_ms: 0, gitlab_shown: String::new(), job_shown: Default::default(), run_shown: Default::default(), modal, modal_others: vec![], modal_came: false, modal_pending: None, return_to: String::new(), show_setup: false,
            manifest_state: None, github_for: None, github_expected: None, deploying: Arc::new(Mutex::new(None)), deployed: Arc::new(Mutex::new(false)), deployed_to: Arc::new(Mutex::new(None)), moving: Arc::new(Mutex::new(None)), updating: Arc::new(Mutex::new(None)), quotas: HashMap::new(), quotas_for: None, aws_looks: HashMap::new(), aws_copying: HashMap::new(), aws_looking: Default::default(), aws_behind: HashMap::new(), budget_lifted: HashMap::new(), seen: HashMap::new(), flash: None, views: None, refreshing: false, measured_at: 0, measuring: false, cf_month: None, modal_month: None, aws_missing: Arc::new(Mutex::new(HashMap::new())), answered: HashSet::new(), first_asked: HashMap::new(), last_good: HashMap::new() }
    }

    /// A dashboard that remembers: with SuperCI's own sign-ins from its folder, kept there again as they change. A
    /// sign-in given by name for this run comes first.
    pub fn signed_in(store: Option<Store>) -> Self {
        let mut d = Dashboard::new();
        let Some(store) = store else { return d };
        let kept = store.read();
        if d.cf.is_none() { d.cf = kept.cloudflare }
        // Every AWS account kept; in hand, the kept control plane's own (else the first).
        let mut accounts: Vec<aws::Session> = kept.aws.into_iter().chain(kept.aws_more).collect();
        let its = match &kept.plane { Some(Plane::Aws { account_id, .. }) => accounts.iter().position(|a| &a.account_id == account_id), _ => None };
        if !accounts.is_empty() { d.aws = Some(accounts.remove(its.unwrap_or(0))) }
        d.aws_others = accounts;
        if d.modal.is_none() { d.modal = kept.modal }
        let first = d.modal.as_ref().map(|m| m.workspace.clone());
        d.modal_others = kept.modal_more.into_iter().filter(|m| Some(&m.workspace) != first.as_ref()).collect();
        d.aws_ended = kept.aws_ended && d.aws.is_none();
        (d.cf_ended, d.modal_ended) = (kept.cloudflare_ended && d.cf.is_none(), kept.modal_ended && d.modal.is_none());
        // The key the control planes were handed, while it lasts another day; else a new one, good for thirty days.
        match kept.key.filter(|k| key_until(&k.name) > now_ms() / 1000 + 86_400) {
            // (One from before each control plane had a key of its own goes on working until it can be replaced
            // everywhere: see `renew_folder_key`.)
            Some(k) => { d.status_key = k.value; d.status_secret = k.name; d.keyed = k.planes.into_iter().collect() }
            None => d.status_secret = format!("DEVICE_KEY_{}_{}", now_ms() / 1000 + 30 * 86_400, superci_core::crypto::random_id(6).to_uppercase()),
        }
        // The control plane in use is listed at once: its pages are read with the kept key, whether or not its
        // cloud's sign-in still stands (AWS ends one after twelve hours).
        if let Some(p) = &kept.plane { d.planes.push(p.clone()); d.preloaded = Some(p.plane_id().to_string()) }
        for p in kept.planes_more { if !d.planes.iter().any(|x| x.plane_id() == p.plane_id()) { d.planes.push(p) } }
        d.kept_plane = kept.plane;
        d.about = store.about();
        d.store = Some(store);
        d.kept_as = serde_json::to_string(&d.kept()).unwrap_or_default();
        d
    }

    /// A dashboard opened for one thing a person does in the browser.
    pub fn for_task(mut self, task: Task) -> Self { self.task = Some(task); self }

    /// The port this dashboard is served on.
    fn port(&self) -> u16 { self.base.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(oauth_port()) }

    fn signed_in_with(&self, cloud: &str) -> bool { match cloud { "aws" => self.aws.is_some(), "cloudflare" => self.cf.is_some(), _ => self.modal.is_some() } }

    /// The task is done: what to say in the terminal.
    fn task_done(&self) -> Option<String> {
        let kept = || self.store.as_ref().map(|s| s.path().display().to_string()).unwrap_or_else(|| "nothing (no home folder)".into());
        match self.task.as_ref()? {
            Task::Login(Some("aws")) if self.adding && !self.aws_came => None,
            Task::Login(Some("modal")) if self.adding && !self.modal_came => None,
            Task::Login(cloud) if cloud.is_none_or(|c| self.signed_in_with(c)) => self.signed_in_as().map(|now| format!("Signed in: {now}. Kept in {}; `superci logout` removes it.", kept())),
            Task::GitHub { login, done: true, .. } => Some(format!("GitHub is connected for {login}: its App is installed. Jobs with `runs-on: superci` in its repositories now come here.")),
            _ => None,
        }
    }

    /// The task's last page: what was done (its cloud's or GitHub's mark, checked), in a line, and nothing to press.
    fn done_page(&self) -> Response {
        let (mark, heading, text) = match &self.task {
            Some(Task::GitHub { login, .. }) => ("github", "GitHub is connected".to_string(), format!("Jobs with runs-on: superci in {login}'s repositories now run on your runners.")),
            Some(Task::Login(cloud)) => {
                // The cloud asked for; with any: the one signed in to (several: none named).
                let signed: Vec<&str> = ["aws", "cloudflare", "modal"].into_iter().filter(|c| self.signed_in_with(c)).collect();
                let one = cloud.or(match signed.as_slice() { [only] => Some(*only), _ => None });
                (one.unwrap_or("superci"), one.map(|c| format!("Signed in with {}", provider_name(c))).unwrap_or_else(|| "Signed in".into()), "SuperCI stays signed in on this computer until you sign out.".to_string())
            }
            None => ("superci", "Done".to_string(), String::new()),
        };
        let mark = if mark == "superci" { MARK.to_string() } else { logo(mark, 44) };
        document(200, "SuperCI", &format!(r#"<div class="done"><div><span class="done-mark">{mark}<span class="done-check"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12.5 4.2 4.2L19 7"/></svg></span></span><h1>{}</h1><p>{}</p><p class="done-foot">You can close this tab.</p></div></div>"#, esc(&heading), esc(&text)), None)
    }

    /// `superci login aws`, before AWS: one button to it. Back here with the sign-in unfinished: signed out of AWS
    /// first, or the same again. (Shown again from the browser's kept copy, it asks anew which of the two it is.)
    fn aws_login_page(&self) -> Response {
        let stuck = !self.aws_stuck().is_empty();
        let (heading, text) = if stuck { ("Your AWS sign-in is not finished", "If AWS said “400 Bad Request”, an old AWS sign-in in this browser is in the way.") } else { ("Sign in with AWS", "Where AWS lists sessions, choose the account your CI runs in, not an organization's management account. The sign-in lasts as long as that session: twelve hours from when it began.") };
        // (Which workspace it is for: with several on this computer, nothing that changes one goes without saying.)
        let w = esc(&self.workspace());
        let go = |fresh: bool, class: &str, label: &str| format!(r#"<form method="post" action="/aws/signin"><input type="hidden" name="w" value="{w}"><input type="hidden" name="next">{}<button class="{class}">{label}</button></form>"#, if fresh { r#"<input type="hidden" name="fresh" value="on">"# } else { "" });
        let buttons = if stuck { format!("{}{}", go(true, "button", "Sign out of AWS and try again"), go(false, "button secondary", "Try again")) } else { go(false, "button", "Continue to AWS") };
        document(200, "SuperCI", &format!(r#"<div class="done"><div><span class="done-mark">{}</span><h1>{}</h1><p>{}</p><div class="done-go">{buttons}</div></div></div><script>addEventListener('pageshow',function(e){{if(e.persisted)location.reload()}})</script>"#, logo("aws", 44), esc(heading), esc(text)), None)
    }

    /// What the task's tab shows at the dashboard's address, in the dashboard's place.
    fn task_page(&mut self) -> Option<Response> {
        if self.task_done().is_some() { return Some(self.done_page()) }
        // Modal's page does not come back with the sign-in: it is asked for here until it is approved there.
        if matches!(self.task, Some(Task::Login(Some("modal")))) {
            if let Some(pending) = self.modal_pending.clone() {
                if let Ok(Some(session)) = modal::wait(&pending, 1.0) {
                    self.came_modal(session);
                    return Some(self.done_page())
                }
                return Some(back_to("Approve it in Modal's tab", "This tab goes on by itself once you have.", &self.back_url("")))
            }
        }
        // AWS's sign-in can end on AWS's own "400 Bad Request" (see `aws_stuck`), and Back from there has to find a
        // page here: so its sign-in starts from one with a button (a page that goes on by itself is one a browser's
        // Back skips), and that page says the way on once a sign-in was started and has not come back.
        if matches!(self.task, Some(Task::Login(Some("aws")))) { return Some(self.aws_login_page()) }
        match self.task.as_mut()? {
            // A sign-in with Cloudflare or Modal goes straight to it; with any, the dashboard's own first screen asks which.
            Task::Login(Some(cloud)) => Some(Response::redirect(&format!("/connect/{cloud}"))),
            Task::Login(None) => None,
            // Back here before GitHub was finished: the page waits, and looks again by itself.
            Task::GitHub { started: true, .. } => Some(document(200, "SuperCI", &format!(r#"<div class="wait"><div>{MARK}<h1>Finish on GitHub</h1><p>Create the App there and choose its repositories.</p><div class="spinner"></div></div></div>"#), Some(3))),
            Task::GitHub { login, host, started, .. } => {
                *started = true;
                let fields = [("login", login.clone()), ("host", host.clone())];
                let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields.iter().map(|(k, v)| (*k, v.as_str()))).finish().into_bytes();
                let req = Req { method: "POST".into(), path: "/github/start".into(), query: vec![], cookie: None, last: None, body, host: String::new(), origin: None, fetch_site: None };
                Some(self.handle(&req).unwrap_or_else(|e| message(500, "GitHub could not be started", &esc(&e))))
            }
        }
    }

    /// For a command that changes something: signed in, the clouds looked in, and the control plane in use chosen, as a
    /// page's first load does.
    pub fn ready(&mut self) -> Result<()> { self.look(true) }

    /// As `ready`; `one`: stops when several control planes are live and none is named (a listing passes false).
    fn look(&mut self, one: bool) -> Result<()> {
        self.signed_in_as().ok_or(self.nowhere())?;
        self.discover()?;
        if self.signed_in_as().is_none() { return Err(self.nowhere().into()) }
        let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
        if let Some(live) = view::several(&views).filter(|_| one) { return Err(view::name_one(&live)) }
        // (Each view is of the control plane at its own place in the list.)
        if !views.is_empty() { self.select(view::in_use(&views)) }
        self.keep();
        Ok(())
    }

    /// Why nothing is signed in to, for a command that needs a sign-in: one that ended says which and how to sign in
    /// again. (Asked only while nothing is signed in to: none of them was made again.)
    fn nowhere(&self) -> &'static str {
        if self.aws_ended { AWS_ENDED } else if self.cf_ended { CF_ENDED } else if self.modal_ended { MODAL_ENDED } else { NOT_SIGNED_IN }
    }

    /// Whether a sign-in kept here turned out to be over (asked only while nothing is signed in to).
    fn any_ended(&self) -> bool { self.aws_ended || self.cf_ended || self.modal_ended }

    /// The Cloudflare sign-in ended and was not made again (a sign-in made since leaves `cf_ended` as it was).
    fn cf_still_ended(&self) -> bool { self.cf_ended && self.cf.is_none() }

    /// The same of Modal's.
    fn modal_still_ended(&self) -> bool { self.modal_ended && self.modal.is_none() }

    /// The sign-ins that are over and not made again, for a command to say once beside what it did: what they
    /// reach was not looked at.
    pub fn ended_notes(&self) -> Vec<String> {
        let mut notes = vec![];
        if self.cf_still_ended() { notes.push("The Cloudflare sign-in has expired: what is in Cloudflare was not looked at. `superci login cloudflare` signs in again.".to_string()) }
        if self.modal_still_ended() { notes.push("Modal no longer accepts the sign-in: what is in Modal was not looked at. `superci login modal` signs in again.".to_string()) }
        for u in &self.unanswered { let (cloud, why) = u.split_once(": ").unwrap_or((u, "")); notes.push(format!("{cloud} did not answer ({why}): what is there was not looked at.")) }
        notes
    }

    /// The kept Cloudflare sign-in is over (Cloudflare said so): forgotten, and said from here on where Cloudflare
    /// is needed. (One given by name for this run is not SuperCI's to forget: its error is said as it is, and this is
    /// never asked of it.)
    fn cf_over(&mut self) {
        (self.cf, self.cf_ended, self.views) = (None, true, None);
        self.cf_accounts.clear();
    }

    /// A change that needs a cloud, after that cloud ended the sign-in kept here: said as that; after it did not
    /// answer, with what it said (else as it was said).
    pub(crate) fn ended_said(&self, e: String) -> String {
        let (cf, modal) = (self.cf_still_ended(), self.modal_still_ended());
        if self.aws_ended && e.contains("Sign in with AWS") { AWS_ENDED.to_string() } else if (cf && e.contains("Sign in with Cloudflare")) || (!self.cf_given && cloudflare::over(&e)) { CF_ENDED.to_string() }
            else if (modal && e.contains("Sign in with Modal")) || (!self.modal_given && e.contains(modal::ENDED)) { MODAL_ENDED.to_string() }
            else if let Some((cloud, why)) = self.unanswered.iter().filter_map(|u| u.split_once(": ")).find(|(cloud, _)| e.contains(&format!("Sign in with {cloud}"))) { format!("{cloud} did not answer: {why}") }
            else { e }
    }

    /// A Modal sign-in that came back. With `--add`, one to another workspace goes beside the first (the same
    /// workspace's again replaces its token); else it is the first.
    fn came_modal(&mut self, session: modal::Session) {
        let first = self.modal.as_ref().map(|m| m.workspace.clone());
        self.modal_others.retain(|m| m.workspace != session.workspace);
        if self.adding && first.as_ref().is_some_and(|w| *w != session.workspace) { self.modal_others.push(session) } else { self.modal = Some(session) }
        (self.modal_pending, self.looked_modal, self.modal_came) = (None, false, true);
    }

    /// The Modal sign-in a place in the order is of: `modal` the first's, `modal:<workspace>` that workspace's.
    fn modal_for(&self, cloud: &str) -> Option<&modal::Session> {
        match cloud.split_once(':') { Some((_, w)) => self.modal.iter().chain(&self.modal_others).find(|m| m.workspace == w), None => self.modal.as_ref() }
    }

    /// Deploys the jobs' Modal Functions (one per image) in the workspace of a Modal place in Function mode.
    fn deploy_jobs(&self, plane: &Plane, cloud: &str, images: &std::collections::BTreeMap<String, superci_core::plane::RunnerImage>) -> Result<()> {
        let session = self.modal_for(cloud).ok_or_else(|| match cloud.split_once(':') { Some((_, w)) => format!("Sign in with Modal to the workspace {w} first: `superci login modal --add`."), None => "Sign in with Modal first.".to_string() })?;
        eprintln!("Deploying the jobs' Modal Functions in {} (the first time builds their images there)…", session.workspace);
        modal::deploy_jobs(session, plane.plane_id(), images)
    }

    /// The same of Modal's.
    fn modal_over(&mut self) {
        (self.modal, self.modal_ended, self.views) = (None, true, None);
    }

    /// For a command that changes a setting of the control plane in use: when that one is known from the last run and
    /// its cloud is signed in to, nothing more is looked for (finding control planes asks a cloud in every region);
    /// else as `ready`.
    pub fn ready_for_settings(&mut self) -> Result<()> {
        match self.planes.get(self.selected).map(|p| p.cloud()) {
            Some(cloud) if self.preloaded.is_some() && self.signed_in_with(cloud) => Ok(()),
            _ => self.ready(),
        }
    }

    /// Does what a form on the dashboard's pages does, with the same fields: the same code runs (`handle`), so a
    /// command and the page can never differ. What it said: a page that says one thing, or the note a page shows once.
    pub fn act(&mut self, path: &str, fields: &[(&str, String)]) -> Result<String> {
        SAID.with(|s| s.borrow_mut().take());
        self.commanding = true;
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields.iter().map(|(k, v)| (*k, v.as_str()))).finish().into_bytes();
        let req = Req { method: "POST".into(), path: path.into(), query: vec![], cookie: None, last: None, body, host: String::new(), origin: None, fetch_site: None };
        self.views = None;
        let answer = self.handle(&req);
        self.keep();
        let ended = |e: String| self.ended_said(e);
        let answer = answer.map_err(ended)?;
        let said = SAID.with(|s| s.borrow_mut().take()).map(|(_, heading, text)| { let text = plain(&text); if text.is_empty() { heading } else { format!("{}: {text}", heading.trim_end_matches('.')) } });
        match (answer.status, said) {
            (300..=399, _) => Ok(self.flash.take().unwrap_or_default()),
            // (A page that refuses says why: `message` is how every one does.)
            (status, said) if status >= 400 => Err(ended(said.unwrap_or(format!("refused ({status})")))),
            (_, said) => Ok(said.unwrap_or_default()),
        }
    }

    /// Waits for what a command started in the background (a deploy, an update, a move), saying each step as it
    /// begins. A deploy's control plane joins the list, in use, with this machine's key.
    pub fn wait(&mut self, what: Background, say: &mut dyn FnMut(&str)) -> Result<String> {
        let mut said = 0;
        loop {
            let (steps, at, ended): (Vec<String>, usize, Option<Result<String>>) = match what {
                Background::Deploy => { let g = lock(&self.deploying); let d = g.as_ref().ok_or("no deploy was started")?;
                    (d.steps.iter().map(|s| s.to_string()).collect(), d.at, d.result.as_ref().map(|r| r.as_ref().map(|p| format!("The control plane is running: {} ({})", p.url(), p.place())).map_err(|e| plain_refusal(e).unwrap_or_else(|| e.clone())))) }
                Background::Update => { let g = lock(&self.updating); let u = g.as_ref().ok_or("no update was started")?;
                    (u.steps.iter().map(|s| s.to_string()).collect(), u.at, u.result.as_ref().map(|r| r.as_ref().map(|_| format!("Updated to {DASHBOARD_VERSION}.{}{}", if u.note.is_empty() { "" } else { " " }, u.note)).map_err(|e| e.clone()))) }
                Background::Move => { let g = lock(&self.moving); let m = g.as_ref().ok_or("no move was started")?;
                    (MOVE_STEPS.iter().map(|s| s.to_string()).collect(), m.at, m.result.as_ref().map(|r| r.as_ref().map(|left| if left.is_empty() { "Moved.".to_string() } else { format!("Moved. Did not come along: {}.", left.join("; ")) }).map_err(|e| e.clone()))) }
            };
            // Each step as it begins; all of them once it has ended well.
            let upto = if ended.as_ref().is_some_and(|e| e.is_ok()) { steps.len() } else { (at + 1).min(steps.len()) };
            while said < upto { say(&steps[said]); said += 1 }
            if let Some(ended) = ended {
                if matches!(what, Background::Deploy) && ended.is_ok() { self.adopt_deploy(); self.key_planes(); }
                // The one in use is the one kept: after a deploy still the one before (unless this is the first),
                // after a move the one moved to.
                if !matches!(what, Background::Update) && ended.is_ok() {
                    let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
                    // (A view each of the control planes listed: the one in use is among them.)
                    self.selected = views.get(view::in_use(&views)).and_then(|v| self.planes.iter().position(|p| p.plane_id() == v.plane.plane_id())).unwrap_or(self.selected);
                }
                self.views = None;
                self.keep();
                return ended
            }
            aws::pause(400);
        }
    }

    /// The control plane in use, read for a command: with the kept key when its control plane has it (no cloud is
    /// asked), else after looking in the clouds signed in to. None: signed in, and no control plane found.
    pub fn current(&mut self) -> Result<Option<PlaneView>> {
        // Signed in nowhere: the control plane kept here is still reached with this computer's key (a sign-in that
        // ended, or was made elsewhere, leaves it as it was).
        let only_reading = self.signed_in_as().is_none();
        let reading_kept = match (only_reading, self.kept_plane.clone()) {
            (false, _) => None,
            (true, Some(p)) if self.any_ended() || !self.keyed.is_empty() => Some(p),
            _ => return Err(NOT_SIGNED_IN.into()),
        };
        // The one named, when it is known here; with none named, the kept one only when no other is known (with
        // several known, which of them are live is asked first, below).
        let known = match view::named() {
            Some(n) => self.planes.iter().find(|p| view::is(p, &n)).cloned(),
            None if self.planes.len() <= 1 || only_reading => self.kept_plane.clone(),
            None => None,
        };
        let kept = known.filter(|p| self.keyed.contains(p.plane_id()));
        // (One that says it moved away is no longer the one in use: found again below.)
        if let Some(v) = kept.as_ref().map(|p| view::plane_view(p, Some(&self.status_key))).filter(|v| v.status.is_some() && (v.moved_to.is_none() || only_reading)) {
            // (The one named is the one a change then acts on: selected, as one found again below is. Else a change
            // to it after another was in use is refused as naming none here.)
            if let Some(i) = self.planes.iter().position(|p| p.plane_id() == v.plane.plane_id()) { self.select(i) }
            return Ok(Some(v))
        }
        // Nothing to find it again with: as it answers now (not at all, or without taking the key any more).
        if let Some(p) = reading_kept {
            let v = view::plane_view(&p, Some(&self.status_key));
            if let Some(why) = self.key_refused(&v) { return Err(why) }
            return Ok(Some(v))
        }
        // Not known yet, or its key is gone there: found again (and handed the key) with the sign-ins.
        self.keyed.clear();
        self.discover()?;
        if self.planes.is_empty() { return if self.signed_in_as().is_none() { Err(self.nowhere().into()) } else { Ok(None) } }
        let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
        if let Some(live) = view::several(&views) { return Err(view::name_one(&live)) }
        let i = view::in_use(&views);
        let p = views[i].plane.clone();
        self.select(i);
        let mut v = view::plane_view(&p, Some(&self.status_key));
        // Just handed the key: its whole status, once it takes it (a few tries, for some twenty seconds: one slower than
        // that to answer is not asked again and again).
        if v.online && v.status.is_none() && self.keyed.contains(p.plane_id()) {
            let began = std::time::Instant::now();
            v.status = (0..8).take_while(|_| began.elapsed() < Duration::from_secs(20)).find_map(|_| { aws::pause(1500); cloudflare::status(p.url(), &self.status_key) })
        }
        self.keep();
        if let Some(why) = self.key_refused(&v) { return Err(why) }
        Ok(Some(v))
    }

    /// A control plane that answers and does not take this computer's key: why, and what to do (never "no runs").
    /// One from before `NAMES_SINCE` reads the key under its old name, which only a sign-in can hand it.
    fn key_refused(&self, v: &PlaneView) -> Option<String> {
        if !v.online || v.status.is_some() { return None }
        let said = cloudflare::refused(v.plane.url(), &self.status_key)?;
        // (Online, it said a version: one from before versions were said reads as the first.)
        if view::older(v.version.as_deref(), NAMES_SINCE) {
            return Some(format!("The control plane runs SuperCI {}, which reads this computer's key under its old name. Sign in again where it runs (`superci login`) so it is handed that way, or update the control plane (`superci planes update`).", v.version.as_deref().unwrap_or("of an older version")))
        }
        if said.starts_with(superci_core::plane::KEY_REFUSED) && view::older(Some(DASHBOARD_VERSION), v.version.as_deref().unwrap_or_default()) {
            return Some(format!("The control plane runs SuperCI {}, newer than this SuperCI ({DASHBOARD_VERSION}), and does not take its key: update SuperCI (npx @superci/cli@latest).", v.version.as_deref().unwrap_or_default()))
        }
        Some("The control plane does not take this computer's key (it expired or was replaced). Sign in again where it runs (`superci login`) to hand it a new one.".to_string())
    }

    /// Every control plane found in the clouds signed in to, the one in use marked.
    pub fn all(&mut self) -> Result<(Vec<PlaneView>, usize)> {
        self.look(false)?;
        let key = self.status_key.clone();
        let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, Some(&key))).collect();
        let used = view::in_use(&views);
        Ok((views, used))
    }

    #[cfg(test)]
    fn read_only_of_test(&self) -> bool { self.signed_in_as().is_some() || !self.planes.is_empty() }

    /// The key commands read the control plane with (after `current`).
    pub fn key(&self) -> &str { &self.status_key }

    /// The control plane in use (after `ready`), and the Cloudflare accounts the sign-in reaches.
    pub fn plane_in_use(&self) -> Option<Plane> { self.plane().ok() }
    pub fn cloudflare_accounts(&self) -> &[(String, String)] { &self.cf_accounts }

    /// Where SuperCI is signed in, in words (nothing: nowhere).
    pub fn signed_in_as(&self) -> Option<String> {
        let mut at = vec![];
        for a in self.aws_accounts() { at.push(format!("AWS account {}", a.account_id)) }
        if self.cf.is_some() { at.push(if self.cf_given { "Cloudflare (a token given for this run)".to_string() } else { "Cloudflare".to_string() }) }
        for m in self.modal.iter().chain(&self.modal_others) { at.push(format!("Modal workspace {}", m.workspace)) }
        if at.is_empty() { None } else { Some(at.join(", ")) }
    }

    /// A key that only reads, for a coding agent or a script that should look and not change: made here, its SHA-256
    /// written into the control plane's secrets (through the cloud's API, like every setting), and shown once. The
    /// key, and when it ends (unix seconds).
    pub fn create_key(&mut self, name: &str, days: u64, runs: bool) -> Result<(String, u64)> {
        let plane = self.plane()?;
        let name = name.to_ascii_uppercase();
        if !(1..=24).contains(&name.len()) || !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) { return Err("A key's name is letters and digits, 24 at most (agent, ci2).".into()) }
        if !(1..=366).contains(&days) { return Err("A key lasts from 1 to 366 days.".into()) }
        let seen = view::plane_view(&plane, Some(&self.status_key));
        if view::older(seen.version.as_deref(), "0.11.0") { return Err("Update the control plane first: read keys need control plane 0.11.0 or newer (`superci planes update`).".into()) }
        // (A key that may also start runs from a computer says so after its hash.)
        if runs && view::older(seen.version.as_deref(), "0.18.0") { return Err("Update the control plane first: keys that start runs need control plane 0.18.0 or newer (`superci planes update`).".into()) }
        // (One that answers and does not take this computer's key: why, rather than "did not answer".)
        let refused = self.key_refused(&seen);
        let status = seen.status.clone().or_else(|| status_soon(plane.url(), &self.status_key)).ok_or_else(|| refused.unwrap_or_else(|| "The control plane did not answer: try again in a few seconds".to_string()))?;
        if read_keys(&status).iter().any(|(n, _)| *n == name) { return Err(format!("There is a key named {} already: `superci keys delete {}` first.", name.to_lowercase(), name.to_lowercase())) }
        let (key, until) = (format!("superci_{}_{}", if runs { "run" } else { "read" }, random_token(32)), now_ms() / 1000 + days * 86_400);
        let hash = superci_core::crypto::sha256_hex(key.as_bytes());
        let value = if runs { format!("{hash}:runs") } else { hash };
        self.put_secret(&plane, &format!("READ_KEY_{until}_{name}"), &value)?;
        let n = name.clone();
        if !wait_for(plane.url(), &self.status_key, move |s| read_keys(s).iter().any(|(k, _)| *k == n)) { return Err("The control plane has not taken the key yet. It is written; look with `superci keys list` in a moment, and make it again if it is not there (the key itself was not shown).".into()) }
        Ok((key, until))
    }

    /// Ends a key that only reads: its entry leaves the control plane's secrets.
    pub fn revoke_key(&mut self, name: &str) -> Result<()> {
        let plane = self.plane()?;
        let name = name.to_ascii_uppercase();
        let status = status_soon(plane.url(), &self.status_key).ok_or("The control plane did not answer: try again in a few seconds")?;
        let (_, until_ms) = read_keys(&status).into_iter().find(|(n, _)| *n == name).ok_or_else(|| format!("There is no key named {}.", name.to_lowercase()))?;
        self.drop_secret(&plane, &format!("READ_KEY_{}_{name}", until_ms / 1000))?;
        let n = name.clone();
        wait_for(plane.url(), &self.status_key, move |s| !read_keys(s).iter().any(|(k, _)| *k == n));
        Ok(())
    }

    /// This session's key: its name (as kept in the control plane) and its value.
    pub fn status_key_name(&self) -> String { self.status_secret.clone() }
    pub fn status_key_value(&self) -> String { self.status_key.clone() }

    /// Another computer's key taken out of the CI in use (its dashboard stops reaching it); never this one's own.
    pub fn drop_device_key(&mut self, name: &str) -> Result<()> {
        if !name.starts_with("DEVICE_KEY_") || !name[11..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { return Err("That is not a device key (names are in `superci keys list`).".into()) }
        if name == self.status_secret { return Err("That is this computer's own key: `superci logout` takes it out.".into()) }
        let plane = self.plane()?;
        self.drop_secret(&plane, name)
    }

    /// The secrets the CI in use keeps for runs from a computer, as its control plane says them (names, scopes, last
    /// use; never a value).
    pub fn kept_secrets(&mut self) -> Result<Vec<serde_json::Value>> { Ok(self.kept_secrets_said()?["secrets"].as_array().cloned().unwrap_or_default()) }

    /// The same with the control plane's time (`now_ms`).
    pub fn kept_secrets_said(&mut self) -> Result<serde_json::Value> {
        let plane = self.plane()?;
        let seen = view::plane_view(&plane, Some(&self.status_key));
        if view::older(seen.version.as_deref(), "0.19.13") { return Err("Update the control plane first: secrets for runs from a computer need control plane 0.19.13 or newer (`superci planes update`).".into()) }
        cloudflare::plane_get(plane.url(), &self.status_key, "/secrets")
    }

    /// Keeps a secret for runs from a computer: a new one (`create`: refused when the name is kept with this scope
    /// already), or a new value for one kept (not `create`: refused when it is not). It is written into the control
    /// plane's own cloud with this computer's sign-in; nothing on this machine keeps it. Its name as kept.
    pub fn keep_secret(&mut self, name: &str, scope: &superci_core::secrets::Scope, value: &str, create: bool) -> Result<String> {
        use superci_core::secrets;
        let name = secrets::valid_name(name)?;
        secrets::valid_value(value)?;
        let plane = self.plane()?;
        let kept = self.kept_secrets()?;
        let setting = secrets::setting(&name, scope);
        let there = kept.iter().any(|s| s["setting"] == setting.as_str());
        if create && there { return Err(format!("{name} is kept for {} already: `superci secrets update {name}` gives it a new value.", scope.said())) }
        if !create && !there { return Err(format!("{name} is not kept for {}: `superci secrets create {name}` keeps it.", scope.said())) }
        if create && kept.len() >= 100 { return Err("A CI keeps 100 secrets at most.".into()) }
        let at = now_ms();
        let text = json_text(&secrets::Stored { name: name.clone(), value: value.to_string(), scope: scope.clone(), updated_ms: at });
        // (Cloudflare keeps 5 KB in a secret, and hands its control plane what a run is given with the request.)
        if matches!(plane, Plane::Cloudflare { .. }) {
            if text.len() > 5000 { return Err("On Cloudflare a secret holds about 4.8 KB at most.".into()) }
            let more: u64 = kept.iter().filter(|s| s["setting"] != setting.as_str()).count() as u64;
            if more >= 12 && create { return Err("A control plane on Cloudflare keeps 12 secrets at most (it is handed them with each run).".into()) }
        }
        self.put_secret(&plane, &setting, &text)?;
        let taken = || cloudflare::plane_get(plane.url(), &self.status_key, "/secrets").ok().filter(|seen| seen["secrets"].as_array().into_iter().flatten().any(|s| s["setting"] == setting.as_str() && s["updated_ms"].as_u64() == Some(at)));
        if tries(60, 500, taken).is_some() { return Ok(name) }
        Err(format!("{name} is written; the control plane has not taken it yet. Look with `superci secrets list` in a moment."))
    }

    /// A kept secret taken out (its name and scope as kept).
    pub fn forget_secret(&mut self, name: &str, scope: &superci_core::secrets::Scope) -> Result<String> {
        use superci_core::secrets;
        let name = secrets::valid_name(name)?;
        let plane = self.plane()?;
        let kept = self.kept_secrets()?;
        let setting = secrets::setting(&name, scope);
        if !kept.iter().any(|s| s["setting"] == setting.as_str()) {
            let others: Vec<&str> = kept.iter().filter(|s| s["name"] == name.as_str()).filter_map(|s| s["said"].as_str()).collect();
            return Err(match others.is_empty() { true => format!("{name} is not kept."), false => format!("{name} is not kept for {}; it is for: {}. Say which with --repo, --branch, --workflow and --environment.", scope.said(), others.join("; ")) })
        }
        self.drop_secret(&plane, &setting)?;
        // (One that does not answer has not said it is gone.)
        let gone = || cloudflare::plane_get(plane.url(), &self.status_key, "/secrets").ok().filter(|seen| !seen["secrets"].as_array().into_iter().flatten().any(|s| s["setting"] == setting.as_str()));
        if tries(60, 500, gone).is_some() { return Ok(name) }
        // (Never said as done while the control plane still gives it to runs.)
        Err(format!("{name} is taken out of the control plane's cloud; the control plane still has it. Look with `superci secrets list` in a moment."))
    }

    /// `superci logout`: what is kept on this machine is removed, after the clouds that can end a sign-in were asked
    /// to (Cloudflare), and the control plane in use was asked to forget this machine's key. What was done, in words.
    pub fn logout(mut self) -> Result<Vec<String>> { self.sign_out() }

    /// Signs SuperCI out on this machine (the dashboard's Sign out, and `superci logout`): see `logout`. Afterwards
    /// this dashboard knows no cloud and no control plane, as on a first start.
    fn sign_out(&mut self) -> Result<Vec<String>> {
        let Some(store) = self.store.clone() else { return Ok(vec!["Nothing is kept on this machine (it has no home folder).".into()]) };
        let mut said = vec![];
        if store.read().key.is_some() {
            // From every control plane that has it: the ones listed, and the kept one if it is not among them.
            let mut have: Vec<Plane> = self.planes.iter().filter(|p| !matches!(p, Plane::Seen { .. })).cloned().collect();
            if let Some(k) = self.kept_plane.clone().filter(|k| !have.iter().any(|p| p.plane_id() == k.plane_id())) { have.push(k) }
            have.retain(|p| self.keyed.contains(p.plane_id()));
            let writer = Writer { cf: if self.cf.is_some() { self.cf_client().ok() } else { None }, aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone(), own: self.own() };
            for p in have {
                if before_names(p.url()) { let _ = writer.drop(&p, &old_key_name(&self.status_secret)); }
                match writer.drop(&p, &self.status_secret) {
                    Ok(()) => said.push(format!("The control plane ({}) no longer takes this machine's key.", p.place())),
                    Err(_) => said.push(format!("The control plane ({}) still has this machine's key; it ends by itself by {}.", p.place(), day(key_until(&self.status_secret)))),
                }
            }
        }
        if let Some(refresh) = self.cf.as_ref().filter(|_| !self.cf_given).and_then(|s| s.refresh_token()) {
            said.push(if cloudflare::end_sign_in(refresh) { "Cloudflare ended SuperCI's sign-in.".into() } else { "Cloudflare did not confirm ending SuperCI's sign-in; it is removed here.".to_string() });
        }
        if (self.modal.is_some() && !self.modal_given) || !self.modal_others.is_empty() { said.push("SuperCI's Modal token is removed here; it stays listed in Modal (Settings → API tokens) until deleted there.".into()) }
        if self.aws.is_some() { said.push("SuperCI's AWS sign-in is removed here; AWS ends it by itself within twelve hours of when it was made.".into()) }
        said.push(if store.remove()? { format!("Removed {}.", store.path().display()) } else { "Nothing was kept on this machine.".into() });
        // As on a first start: no sign-in (but one given by name for this run), no control plane, a new key.
        if !self.cf_given { self.cf = None }
        if !self.modal_given { self.modal = None }
        self.modal_others.clear();
        self.aws = None;
        self.aws_others.clear();
        // (Signed out, the workspace is still called what it was; whose jobs it ran is not known any more.)
        let mut about = self.about.clone();
        (about.hosts, about.picture, about.picture_of) = (vec![], String::new(), String::new());
        self.set_about(about);
        (self.picture_owner, self.picture_asked_ms) = (String::new(), 0);
        (self.aws_ended, self.cf_ended, self.modal_ended, self.looked_cf, self.looked_aws, self.looked_modal) = (false, false, false, false, false, false);
        (self.planes, self.selected, self.kept_plane, self.preloaded, self.views) = (vec![], 0, None, None, None);
        (self.found, self.found_about) = (vec![], HashMap::new());
        self.cf_accounts.clear();
        self.keyed.clear();
        self.status_key = folder_key();
        self.status_secret = format!("DEVICE_KEY_{}_{}", now_ms() / 1000 + 30 * 86_400, superci_core::crypto::random_id(6).to_uppercase());
        self.kept_as = serde_json::to_string(&self.kept()).unwrap_or_default();
        Ok(said)
    }

    /// What is to be kept, as things are now.
    fn kept(&self) -> Kept {
        let mut planes: Vec<String> = self.keyed.iter().cloned().collect();
        planes.sort();
        Kept {
            cloudflare: if self.cf_given { None } else { self.cf.clone() },
            aws: self.aws_accounts().into_iter().next(),
            aws_more: self.aws_accounts().into_iter().skip(1).collect(),
            modal: if self.modal_given { None } else { self.modal.clone() },
            modal_more: self.modal_others.clone(),
            key: Some(Key { name: self.status_secret.clone(), value: self.status_key.clone(), planes }),
            plane: self.planes.get(self.selected).filter(|p| !matches!(p, Plane::Seen { .. })).cloned().or_else(|| self.kept_plane.clone()),
            planes_more: self.planes.iter().enumerate().filter(|(i, p)| *i != self.selected && !matches!(p, Plane::Seen { .. })).map(|(_, p)| p.clone()).collect(),
            aws_ended: self.aws_ended && self.aws.is_none() && self.aws_others.is_empty(),
            cloudflare_ended: self.cf_still_ended(),
            modal_ended: self.modal_still_ended(),
        }
    }

    /// Notes what the control plane in use is called and whose jobs it runs, once it has said, with the first GitHub
    /// organization's picture: kept beside the sign-ins, so that a list of workspaces can show this one without asking
    /// its control plane.
    fn note_seen(&mut self, views: &[PlaneView]) {
        let Some(v) = views.get(view::in_use(views)).filter(|v| v.status.is_some() && !matches!(v.plane, Plane::Seen { .. })) else { return };
        let st = v.status.clone().unwrap_or_default();
        let apps = status_apps(&st);
        let mut about = self.about.clone();
        about.hosts = apps.iter().filter_map(|a| a["owner"].as_str().map(|o| ("github".to_string(), o.to_string()))).collect();
        about.hosts.extend(v.gitlabs().into_iter().map(|(_, url, _)| ("gitlab".to_string(), url.trim_start_matches("https://").to_string())));
        // A control plane that has not been named says "SuperCI": the name the workspace was given stands until
        // the control plane has it (see `adopt_deploy`).
        if v.name() != "SuperCI" || about.name.is_empty() { about.name = v.name() }
        // Whose picture it shows: the first organization on github.com (see `picture_due`). Another one's goes.
        self.picture_owner = apps.iter().find(|a| a["host"].as_str().is_none_or(str::is_empty)).and_then(|a| a["owner"].as_str()).unwrap_or_default().to_string();
        if about.picture_of != self.picture_owner { (about.picture, about.picture_of) = (String::new(), String::new()) }
        // Which computer this session's key is on, said once a session (for the CI's Keys).
        if self.hello_sent.insert(v.plane.plane_id().to_string()) {
            let _ = cloudflare::plane_post(v.plane.url(), &self.status_key, "/keys/hello", &serde_json::json!({ "device": computer_name() }));
        }
        // Whose jobs it runs, kept in it where its account's sign-in reads it (before anything of this computer's
        // is in it: what a workspace picking among an account's CIs shows), once it changes.
        let owners = about.hosts.iter().map(|(kind, name)| format!("{kind}:{name}")).collect::<Vec<_>>().join(",");
        if !owners.is_empty() && self.owners_written.get(v.plane.plane_id()) != Some(&owners) {
            let writer = Writer { cf: if self.cf.is_some() { self.cf_client().ok() } else { None }, aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone(), own: self.own() };
            // (Tried once a session: without its cloud's sign-in here, the next session that has it writes it.)
            let _ = writer.put(&v.plane, "OWNERS", &owners);
            self.owners_written.insert(v.plane.plane_id().to_string(), owners);
        }
        self.set_about(about);
    }

    /// The organization whose picture GitHub is to be asked for now: when the dashboard starts, and once a day while
    /// it runs, so that a logo set on GitHub later shows here by itself (after a try that gave none and with none
    /// kept: ten minutes later). The picture kept stands until a new one comes.
    fn picture_due(&mut self) -> Option<String> {
        if self.picture_owner.is_empty() { return None }
        let wait = if self.about.picture.is_empty() { 600_000 } else { 24 * 3_600_000 };
        if self.picture_asked_ms != 0 && now_ms() < self.picture_asked_ms + wait { return None }
        self.picture_asked_ms = now_ms();
        Some(self.picture_owner.clone())
    }

    /// Keeps what the workspace is called and looks like, when it changed.
    fn set_about(&mut self, about: crate::store::About) {
        if about == self.about { return }
        self.about = about;
        if let Some(store) = &self.store { if let Err(e) = store.set_about(&self.about) { if !self.keep_failed { eprintln!("{e}"); self.keep_failed = true } } }
    }

    /// What this workspace's CI is called on its pages: the control plane's name; the workspace's own until the
    /// control plane has one, or there is one.
    fn called_as(&self, current: Option<&PlaneView>) -> String {
        match current.map(|v| v.name()) {
            Some(name) if name != "SuperCI" || self.about.name.is_empty() => name,
            _ if !self.about.name.is_empty() => self.about.name.clone(),
            _ => "SuperCI".to_string(),
        }
    }

    /// This workspace's id: its folder's name.
    fn workspace(&self) -> String { self.store.as_ref().map(|s| s.workspace()).unwrap_or_else(|| crate::store::FIRST.to_string()) }

    /// An address of this dashboard to come back to (`query`: nothing, or `?p=…`), naming this workspace where its
    /// addresses do.
    fn back_url(&self, query: &str) -> String {
        let query = query.trim_start_matches('/');
        let named = query.trim_start_matches('?').split('&').any(|kv| kv.starts_with("w="));
        let w = &self.named;
        if w.is_empty() || named { return format!("{}/{query}", self.base) }
        format!("{}/{query}{}w={w}", self.base, if query.is_empty() { "?" } else { "&" })
    }

    /// Whether a sign-in or an App that comes back from elsewhere (see `respond`) was started in this workspace.
    fn awaits(&self, req: &Req) -> bool {
        let state = req.query.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str()).unwrap_or("");
        let is = |s: &str| !state.is_empty() && safe_eq(state.as_bytes(), s.as_bytes());
        match req.path.as_str() {
            "/oauth/callback" => self.aws_pending.as_ref().is_some_and(|p| is(&p.state)) || self.cf_pending.as_ref().is_some_and(|p| is(&p.state)),
            "/github/callback" => self.manifest_state.as_ref().is_some_and(|(s, ..)| is(s)),
            "/github/installed" => self.github_for.is_some() || self.github_expected.is_some(),
            "/oauth/modal" => self.modal_pending.is_some(),
            _ => false,
        }
    }

    /// The other clouds signed in to, each with its account as far as the sign-in says it (Cloudflare's sign-in
    /// reaches accounts that only asking it lists, which this does not do; Modal's is for one workspace).
    pub fn other_accounts_signed_in(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![];
        if self.cf.is_some() { out.push(("cloudflare", if self.cf_given { "a token given for this run".to_string() } else { "signed in".to_string() })) }
        for m in self.modal.iter().chain(&self.modal_others) { out.push(("modal", m.workspace.clone())) }
        out
    }

    /// The AWS accounts signed in to, each with who is signed in.
    pub fn aws_accounts_signed_in(&self) -> Vec<(String, String)> { self.aws_accounts().into_iter().map(|a| (a.account_id, a.who)).collect() }

    /// What an AWS account signed in to allows (see `aws::Look`): the one named, or the only one; with the
    /// credentials it was looked at with.
    pub fn look_at_aws(&mut self, account: &str) -> Result<(String, String, aws::Look, superci_core::aws::Credentials)> {
        let (account, creds) = self.aws_account(account)?;
        let who = self.aws.as_ref().map(|a| a.who.clone()).unwrap_or_default();
        let look = aws::look_at(&creds);
        // AWS had ended the credentials before they said so: renewed and looked at again, or the sign-in is over.
        if look.unanswered().is_some_and(aws::expired) {
            self.aws_cut_short();
            // (Gone only when AWS ended its sign-in.)
            if self.aws.is_none() { return Err(AWS_ENDED.into()) }
            let (account, creds) = self.aws_account(&account)?;
            let look = aws::look_at(&creds);
            return Ok((account, who, look, creds))
        }
        Ok((account, who, look, creds))
    }

    /// An AWS account signed in to (the one named, or the only one), taken in hand, with its credentials.
    pub fn aws_account(&mut self, account: &str) -> Result<(String, superci_core::aws::Credentials)> {
        let all = self.aws_accounts();
        let account = match (account, all.as_slice()) {
            ("", [only]) => only.account_id.clone(),
            ("", []) => return Err(if self.aws_ended { AWS_ENDED.into() } else { "Sign in with AWS first.".to_string() }),
            ("", several) => return Err(format!("Say which account: {}.", several.iter().map(|a| a.account_id.clone()).collect::<Vec<_>>().join(", "))),
            (id, _) => id.to_string(),
        };
        if !self.hold_account(&account) { return Err(format!("Sign in with AWS first: SuperCI is not signed in to account {account}.")) }
        let creds = self.aws_creds()?;
        Ok((account, creds))
    }

    /// What the AWS account in hand allows, as last looked at (see `aws::Look`).
    fn aws_look(&self) -> Option<&aws::Look> { self.aws.as_ref().and_then(|a| self.aws_looks.get(&a.account_id)).map(|(_, look)| look) }

    /// The AWS account in hand, as a section of the pages where a control plane or runners are set up: what it
    /// allows of what SuperCI does there (its regions, spot and on-demand machines, how many CPUs at once, the image
    /// jobs run on), whose rule a refusal is, and where to ask AWS for more. For every account, restricted or not:
    /// what `superci accounts retrieve aws` says. Nothing until the account has been looked at.
    fn aws_account_section(&self) -> String { self.aws_account_section_for(None) }

    /// As `aws_account_section`, beside a control plane whose machines start in this account: each region it may use
    /// on a line of its own, with how the control plane's jobs use it (spot, on-demand, in which order), what AWS
    /// lets it run there at once, and whether the image jobs run on is there.
    fn aws_account_section_for(&self, v: Option<&PlaneView>) -> String {
        let (Some(account), Some(look)) = (self.aws.as_ref().map(|a| a.account_id.clone()), self.aws_look()) else { return String::new() };
        let names = |regions: &[&str]| regions.iter().map(|r| region_short(r)).collect::<Vec<_>>().join(", ");
        let whose = |r: &superci_core::aws::Refusal| match r.by {
            superci_core::aws::By::Organization | superci_core::aws::By::OrganizationResources => format!("By its AWS organization{}.", r.policy_id().map(|p| format!(" (policy {p})")).unwrap_or_default()),
            superci_core::aws::By::Unsaid => "AWS did not say by whose rule.".to_string(),
            _ => "By the permissions of the role or user signed in as.".to_string(),
        };
        let row = |what: &str, value: String, more: String, end: String| format!(r#"<div><dt>{what}</dt><dd>{value}{}</dd>{end}</div>"#, if more.is_empty() { String::new() } else { format!("<small>{more}</small>") });
        let (allowed, spot_refused) = (look.allowed_regions(), matches!(look.spot, Some(aws::Answer::Refused(_))));
        // What kind of account it is, in a line.
        let (kind, tone, says) = match look.kind() {
            aws::Kind::Open => ("Nothing refused", "good", "Nothing SuperCI does there is refused.".to_string()),
            aws::Kind::Unanswered => ("Not known", "open", format!("AWS did not answer when asked what this account allows ({}). Refresh asks again.", look.unanswered().unwrap_or_default().chars().take(120).collect::<String>())),
            aws::Kind::Project { region, activated: false } => ("AWS project", "open", format!("A project of AWS's new sign-up: AWS keeps it to {} and allows it no spot machines, so jobs run on on-demand machines, at about twice the price.", region_short(&region))),
            aws::Kind::Project { region, activated: true } if look.usable_regions().is_empty() => ("AWS project", "open", format!("A project of AWS's new sign-up: it works in {} only. The image jobs run on is not there yet: copying it in comes first.", region_short(&region))),
            aws::Kind::Project { region, activated: true } => ("AWS project", "good", format!("A project of AWS's new sign-up: it works in {} only. Everything SuperCI does works there.", region_short(&region))),
            // Kept to some regions by its organization, with everything else allowed: said as where it works, which
            // is no trouble. Anything else refused (spot machines, on-demand ones, or by the sign-in's own
            // permissions) is said as that.
            aws::Kind::Restricted => {
                let usable = look.usable_regions();
                let only_regions = !matches!(look.spot, Some(aws::Answer::Refused(_))) && !matches!(look.on_demand, Some(aws::Answer::Refused(_)))
                    && look.regions.iter().all(|(_, a)| match a { aws::Answer::Refused(_) => a.by_organization().is_some(), _ => true });
                if only_regions && !usable.is_empty() { ("Some regions", "good", format!("SuperCI works there in {}. Its AWS organization keeps it out of the other regions.", names(&usable))) }
                else { ("Restricted", "open", "Some of what SuperCI does there is refused: see below.".to_string()) }
            }
        };
        let mut rows = vec![];
        // Copying the image jobs run on into a region that lacks it: a press here (as `superci images create REGION`).
        let copy_into = |region: &str| format!(r#"<form method="post" action="/aws/image"><input type="hidden" name="account" value="{}"><input type="hidden" name="region" value="{}"><input type="hidden" name="next"><button class="button secondary sm">Copy into {}</button></form>"#, esc(&account), esc(region), esc(&region_short(region)));
        // How the control plane in use starts machines in this account, by region: each of its AWS places (spot,
        // on-demand) there: spot where a job costs least, the first favoured ("spot, favoured", "spot when cheaper");
        // on-demand in its order ("on-demand first", "on-demand when N. Virginia has none").
        let mut in_use: Vec<(String, Vec<String>)> = vec![];
        if let Some(v) = v.filter(|v| v.aws_account().is_some_and(|(a, _)| a == account)) {
            for p in v.order().iter().filter(|p| !p.off && superci_core::plane::account_of(&p.cloud) == "aws") {
                let what = if p.cloud == superci_core::plane::AWS_ON_DEMAND { "on-demand" } else { "spot" };
                let regions = v.pool_regions(p);
                // (Never none: a pool with none of its own goes to all of the control plane's, at least its own region.)
                regions.iter().enumerate().for_each(|(i, r)| {
                    // (Spot goes where a job costs least, the first favoured; on-demand in order.)
                    let said = match (regions.len(), i, what) { (1, ..) => what.to_string(), (_, 0, "spot") => "spot, favoured".to_string(), (_, _, "spot") => "spot when cheaper".to_string(),
                        (_, 0, _) => format!("{what} first"), _ => format!("{what} when {} has none", region_short(&regions[i - 1])) };
                    match in_use.iter_mut().find(|(x, _)| x == r) { Some((_, u)) => u.push(said), None => in_use.push((r.clone(), vec![said])) }
                });
            }
        }
        // What AWS lets it run on spot at once in a region, as last asked.
        let spot_cpus = |r: &str| match self.quotas.get(r) { Some(Ok(n)) => Some(*n), _ if look.asked_in.as_deref() == Some(r) => look.spot_cpus, _ => None };
        let without_image = look.regions_without_image();
        // Regions: where it may work, and whose rule keeps it from the others.
        let refused: Vec<&(String, aws::Answer)> = look.regions.iter().filter(|(_, a)| matches!(a, aws::Answer::Refused(_))).collect();
        let unsaid: Vec<&str> = if look.unanswered().is_some() { vec![] } else { look.regions.iter().filter(|(_, a)| matches!(a, aws::Answer::Unknown(_))).map(|(r, _)| r.as_str()).collect() };
        let value = if !in_use.is_empty() && !allowed.is_empty() {
            // The ones it uses first (in their order), then the others it may use.
            let mut listed: Vec<&str> = in_use.iter().map(|(r, _)| r.as_str()).collect();
            // (Every region allowed: the ones it uses, and any the image is missing from; the rest said in a word.)
            let others: Vec<&str> = allowed.iter().copied().filter(|r| !listed.contains(r) && (!refused.is_empty() || without_image.contains(r))).collect();
            listed.extend(others);
            let lines = listed.iter().map(|r| {
                let uses = in_use.iter().find(|(x, _)| x == r).map(|(_, u)| u.join(" · ")).unwrap_or_default();
                let (use_said, end) = if !allowed.contains(r) { (format!("{uses} · AWS refuses it now"), String::new()) }
                    else if without_image.contains(r) { (if uses.is_empty() { "Not used: the runner image is not here".to_string() } else { format!("{uses} · the runner image is not here") }, copy_into(r)) }
                    else if uses.is_empty() { ("Not used".to_string(), String::new()) } else { (uses, String::new()) };
                // (Never empty, and begun with a plain letter: each way above says something.)
                let use_said = format!("{}{}", use_said[..1].to_uppercase(), &use_said[1..]);
                let cpus = spot_cpus(r).filter(|_| allowed.contains(r) && !without_image.contains(r)).map(|n| format!("{n} spot CPUs")).unwrap_or_default();
                format!(r#"<li><span class="ar-name">{}<small>{r}</small></span><span class="ar-use">{}</span><span class="ar-cpu">{cpus}</span>{end}</li>"#, esc(&region_short(r)), esc(&use_said))
            }).collect::<String>();
            format!(r#"<ul class="acct-regions">{lines}</ul>"#)
        } else if allowed.is_empty() { "None of those SuperCI works in".to_string() } else if refused.is_empty() { format!("All {} SuperCI works in", allowed.len()) } else { esc(&names(&allowed)) };
        let mut more = match refused.first() { Some((_, aws::Answer::Refused(r))) => format!("The other {} are refused. {}", refused.len(), whose(r)),
            None if value.starts_with("<ul") && unsaid.is_empty() => format!("All {} SuperCI works in are allowed: add more on Runners.", allowed.len()), _ => String::new() };
        if !unsaid.is_empty() { more = format!("{more} AWS did not say for {}: Refresh asks again.", names(&unsaid)).trim().to_string() }
        // (Nothing has to be allowed for SuperCI to work: the guide is for whoever wants another region. Beside the
        // regions' lines, a link under them, which leaves the lines the whole width.)
        let guide = refused.iter().any(|(_, a)| a.by_organization().is_some());
        let listed = value.starts_with("<ul");
        let more = if guide && listed { format!(r#"{} <a href="{MORE_REGIONS_GUIDE}" target="_blank" rel="noopener">How to allow more ↗</a>"#, esc(&more)) } else { esc(&more) };
        rows.push(row("Regions", value, more,
            if guide && !listed { format!(r#"<a class="button secondary sm" href="{MORE_REGIONS_GUIDE}" target="_blank" rel="noopener">How to allow more ↗</a>"#) } else { String::new() }));
        // Spot and on-demand machines.
        let machines = |a: Option<&aws::Answer>, refused_means: &str| match a {
            Some(aws::Answer::Allowed) => ("Allowed".to_string(), String::new()),
            Some(aws::Answer::Refused(r)) => ("Not allowed".to_string(), esc(&format!("{} {refused_means}", whose(r)).trim().to_string())),
            _ => ("Not known".to_string(), String::new()),
        };
        let (spot, spot_more) = machines(look.spot.as_ref(), "Jobs run on on-demand machines, at about twice the price.");
        rows.push(row("Spot machines", spot, spot_more, String::new()));
        let (on_demand, on_demand_more) = machines(look.on_demand.as_ref(), "");
        rows.push(row("On-demand machines", on_demand, on_demand_more, String::new()));
        // How many CPUs at once, how many jobs that is, and where AWS raises it.
        if let (Some(region), true) = (look.asked_in.as_deref(), look.spot_cpus.is_some() || look.on_demand_cpus.is_some()) {
            let n = |cpus: Option<u32>| cpus.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            let (limit, code) = if spot_refused { (look.on_demand_cpus, superci_core::aws::ON_DEMAND_CPU_QUOTA) } else { (look.spot_cpus, superci_core::aws::SPOT_CPU_QUOTA) };
            let jobs = limit.map(|c| if c < 8 { "One job of the default size at a time".to_string() } else { format!("{} jobs of the default size at a time", c / 4) }).unwrap_or_default();
            // Spot and on-demand are raised apart: one button, each in its menu (only the one jobs can use, when the
            // other is refused).
            let on_demand_refused = matches!(look.on_demand, Some(aws::Answer::Refused(_)));
            let ask = if spot_refused || on_demand_refused {
                format!(r#"<a class="button secondary sm" href="{}" target="_blank" rel="noopener">Ask AWS for more ↗</a>"#, esc(&superci_core::aws::quota_link(region, code)))
            } else {
                let item = |what: &str, cpus: Option<u32>, code: &str| format!(r#"<a href="{}" target="_blank" rel="noopener">{what} CPUs ↗<small>{} now, in {}</small></a>"#,
                    esc(&superci_core::aws::quota_link(region, code)), n(cpus), region_short(region));
                format!(r#"<details class="menu"><summary class="button secondary sm">Ask AWS for more <svg class="chev" viewBox="0 0 24 24"><path d="M8 10l4 4 4-4"/></svg></summary><div class="menu-pop">{}{}</div></details>"#,
                    item("Spot", look.spot_cpus, superci_core::aws::SPOT_CPU_QUOTA), item("On-demand", look.on_demand_cpus, superci_core::aws::ON_DEMAND_CPU_QUOTA))
            };
            // Beside a control plane whose spot machines start in several regions: what they allow together.
            let spot_regions: Vec<&str> = in_use.iter().filter(|(_, u)| u.iter().any(|u| u.starts_with("spot"))).map(|(r, _)| r.as_str()).collect();
            let together: Vec<u32> = spot_regions.iter().filter_map(|r| spot_cpus(r)).collect();
            if !spot_refused && spot_regions.len() > 1 && together.len() == spot_regions.len() {
                let total: u32 = together.iter().sum();
                let item = |what: &str, r: &str, cpus: Option<u32>, code: &str| format!(r#"<a href="{}" target="_blank" rel="noopener">{what} CPUs in {} ↗<small>{} now</small></a>"#,
                    esc(&superci_core::aws::quota_link(r, code)), region_short(r), n(cpus));
                let items = spot_regions.iter().map(|r| item("Spot", r, spot_cpus(r), superci_core::aws::SPOT_CPU_QUOTA)).collect::<String>() + &item("On-demand", region, look.on_demand_cpus, superci_core::aws::ON_DEMAND_CPU_QUOTA);
                let ask = format!(r#"<details class="menu"><summary class="button secondary sm">Ask AWS for more <svg class="chev" viewBox="0 0 24 24"><path d="M8 10l4 4 4-4"/></svg></summary><div class="menu-pop">{items}</div></details>"#);
                rows.push(row("CPUs at once", format!("{total} spot · {} on-demand", n(look.on_demand_cpus)),
                    esc(&format!("{} jobs of the default size at a time on spot, across {}. On-demand: in {}.", total / 4, names(&spot_regions), region_short(region))), ask));
            } else {
                rows.push(row("CPUs at once", format!("{} on-demand · {} spot", n(look.on_demand_cpus), n(look.spot_cpus)), esc(&format!("{jobs}, in {}.", region_short(region))), ask));
            }
        }
        // The image jobs run on: where it is missing among the regions the account may use.
        let (usable, without) = (look.usable_regions(), look.regions_without_image());
        if !usable.is_empty() || !without.is_empty() {
            let copying = self.aws_copying.get(&account).cloned().unwrap_or_default();
            // The account's own copy, of an older build than its maker's newest: said, with copying the new one in.
            let behind = self.aws_behind.get(&account).and_then(|b| b.first()).cloned();
            let being_copied = |r: &&str| copying.iter().any(|c| c == *r);
            let (more, end) = match (&behind, usable.is_empty()) {
                // In none of its regions: nothing can run until it is copied in (setting up offers that first).
                (_, true) if without.iter().any(being_copied) => ("It is being copied in: about ten minutes.".to_string(), String::new()),
                (_, true) => (esc("No job can start there until it is copied in: about ten minutes, and about $1.50 a month for the copy's storage in your account."), String::new()),
                (Some((region, _, newest, true)), _) => (esc(&format!("The build of {} is being copied into {}: about ten minutes. Jobs start from your older copy until it is there.", aws::build_day(newest), region_short(region))), String::new()),
                (Some((region, have, newest, false)), _) => (esc(&format!("Your copy in {} is the build of {}. Its maker has one of {} out.", region_short(region), aws::build_day(have), aws::build_day(newest))),
                    format!(r#"<form method="post" action="/aws/image"><input type="hidden" name="account" value="{}"><input type="hidden" name="region" value="{}"><input type="hidden" name="newer" value="1"><button class="button secondary sm">Copy the new one in</button></form>"#, esc(&account), esc(region))),
                // Where jobs run it is there. A region the account may also use lacks it: nothing is wrong, and it
                // is copied in at a press by whoever wants jobs there too.
                (None, false) if !without.is_empty() => {
                    let (coming, missing): (Vec<&str>, Vec<&str>) = without.iter().copied().partition(|r| being_copied(&r));
                    let mut said = vec![];
                    if !coming.is_empty() { said.push(format!("It is being copied into {}: about ten minutes. Then {} can be added as a region on Runners.", names(&coming), if coming.len() == 1 { "it" } else { "they" })) }
                    if !missing.is_empty() { said.push(format!("Not in {}, which this account may also use. To run jobs there too, copy it in: about ten minutes, and about $1.50 a month for the copy's storage.", names(&missing))) }
                    (esc(&said.join(" ")), missing.iter().map(|r| copy_into(r)).collect::<String>())
                }
                (None, false) => (String::new(), String::new()),
            };
            let value = if usable.is_empty() { format!("Not in {}", esc(&names(&without))) } else if without.is_empty() && usable.len() > 1 { "In every region it may use".to_string() }
                else if usable.len() > 3 { format!("In {} of the {} regions it may use", usable.len(), allowed.len()) } else { format!("In {}", esc(&names(&usable))) };
            // (Beside a control plane, each region's line says where it is, with Copy where it is not: said here only
            // when there is more to say, or nowhere to run.)
            let said_by_regions = !in_use.is_empty() && behind.is_none() && !usable.is_empty() && !without.iter().any(being_copied);
            if !said_by_regions { rows.push(row("Runner image", value, more, if end.is_empty() { String::new() } else { format!(r#"<span class="acct-do">{end}</span>"#) })) }
        }
        if let (Some(org), Some(managed)) = (&look.organization_id, look.management_account.as_deref().filter(|m| *m != account)) {
            rows.push(row("Organization", esc(org), esc(&format!("Managed from account {managed}: its policies are not this account's to read or change.")), String::new()));
        }
        // Signed in to the organization's management account itself: said, since it is easily picked by mistake
        // where AWS lists sessions, and a CI is better kept out of the account that governs the others.
        if let (Some(org), true) = (&look.organization_id, look.management_account.as_deref() == Some(account.as_str())) {
            rows.push(row("Organization", esc(org), esc("The organization's management account. A CI is better kept in an account of its own."),
                r#"<form method="post" action="/aws/signin"><input type="hidden" name="next"><button class="button secondary sm">Another AWS account</button></form>"#.to_string()));
        }
        // Looked at again each time the page is opened (when the last look is older than twenty seconds), and at a
        // press: said while it is on its way, and the page looks again until it is in.
        let looking = self.aws_looking.contains(&account);
        let again = if looking { r#"<span class="waiting-for"><span class="mini-spin"></span>Asking AWS…</span>"#.to_string() }
            else { r#"<form method="post" action="/aws/look"><input type="hidden" name="next"><button class="button secondary sm" title="Asks AWS again what this account allows">Refresh</button></form>"#.to_string() };
        format!(r#"{}<h2 class="list-title" id="aws-account">AWS account</h2><div class="picks page"><div class="acct"><div class="acct-head">{}<span><strong>{}</strong><small>{}</small></span><span class="acct-do">{}{again}</span></div><dl class="acct-facts">{}</dl></div></div>"#,
            if looking { r#"<div data-refresh="2"></div>"# } else { "" }, logo("aws", 32), esc(&account), esc(&says), pill(tone, kind), rows.concat())
    }

    /// Every AWS account signed in to, in one order (by account) whichever is in hand.
    fn aws_accounts(&self) -> Vec<aws::Session> {
        let mut all: Vec<aws::Session> = self.aws.iter().chain(&self.aws_others).cloned().collect();
        all.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        all
    }

    /// Takes the sign-in to an AWS account in hand, when it is kept: what is done next for a control plane in that
    /// account, or for its runners there, is done with that account's sign-in and no other. Whether it is in hand.
    /// Whether this session is signed in where a control plane runs (what changing it, or updating it, needs).
    fn signed_in_at(&self, plane: &Plane) -> bool {
        match plane {
            Plane::Aws { account_id, .. } => self.aws.iter().chain(self.aws_others.iter()).any(|a| &a.account_id == account_id),
            Plane::Cloudflare { .. } => self.cf.is_some(),
            Plane::Modal { .. } => self.modal.is_some(),
            Plane::Seen { .. } => false,
        }
    }

    fn hold_account(&mut self, account: &str) -> bool {
        if self.aws.as_ref().is_some_and(|a| a.account_id == account) { return true }
        let Some(i) = self.aws_others.iter().position(|a| a.account_id == account) else { return false };
        let theirs = self.aws_others.remove(i);
        if let Some(was) = self.aws.replace(theirs) { self.aws_others.push(was) }
        // (What was asked about the account in hand, its quotas, is asked again for this one.)
        self.quotas_for = None;
        true
    }

    /// Makes one of the control planes listed the one that pages and changes act on, with its own account's sign-in.
    fn select(&mut self, i: usize) {
        self.selected = i;
        if let Some(Plane::Aws { account_id, .. }) = self.planes.get(i).cloned() { self.hold_account(&account_id); }
    }

    /// A sign-in to AWS that just came back: a new account is added beside the others, a known one is renewed.
    fn add_account(&mut self, session: aws::Session) {
        self.aws_others.retain(|a| a.account_id != session.account_id);
        if let Some(was) = self.aws.replace(session) { if self.aws.as_ref().is_some_and(|now| now.account_id != was.account_id) { self.aws_others.push(was) } }
        (self.aws_ended, self.looked_aws, self.aws_came) = (false, false, true);
    }

    /// What another process on the same folder wrote since this one last read or wrote the file (a `superci login`
    /// in a terminal while the dashboard runs, a second dashboard): taken, for each sign-in that this process has not
    /// changed itself meanwhile. Without it the next write here would put the old state back and undo that sign-in.
    fn take_in(&mut self) {
        let Some(store) = self.store.clone() else { return };
        let on_disk = store.read();
        let value = |k: &Kept| serde_json::to_value(k).unwrap_or_default();
        let (disk, last, mine) = (value(&on_disk), serde_json::from_str::<serde_json::Value>(&self.kept_as).unwrap_or_default(), value(&self.kept()));
        if disk == last { return }
        let theirs = |field: &str| mine[field] == last[field] && disk[field] != last[field];
        let mut took = false;
        // (The AWS accounts as one: which of them is first in the file says nothing.)
        let accounts = |v: &serde_json::Value| (v["aws"].clone(), v["aws_more"].clone());
        if (accounts(&mine) == accounts(&last) && accounts(&disk) != accounts(&last)) || (theirs("aws_ended") && self.aws_accounts().is_empty()) {
            let held = self.aws.as_ref().map(|a| a.account_id.clone());
            let mut all: Vec<aws::Session> = on_disk.aws.into_iter().chain(on_disk.aws_more).collect();
            let at = held.and_then(|h| all.iter().position(|a| a.account_id == h)).unwrap_or(0);
            self.aws = if all.is_empty() { None } else { Some(all.remove(at)) };
            (self.aws_others, self.aws_ended, self.looked_aws, took) = (all, on_disk.aws_ended, false, true);
        }
        if theirs("cloudflare") && !self.cf_given { (self.cf, self.cf_ended, self.looked_cf, took) = (on_disk.cloudflare, on_disk.cloudflare_ended, false, true); self.cf_accounts.clear(); }
        if theirs("modal") && !self.modal_given { (self.modal, self.modal_ended, self.looked_modal, took) = (on_disk.modal, on_disk.modal_ended, false, true) }
        if theirs("modal_more") { (self.modal_others, took) = (on_disk.modal_more, true) }
        if theirs("key") { if let Some(k) = on_disk.key { (self.status_key, self.status_secret, self.keyed, took) = (k.value, k.name, k.planes.into_iter().collect(), true) } }
        if took {
            self.views = None;
            // What was taken in is as the file has it now: a further change there before this one writes is taken in
            // too (it was taken for this one's own, and the write put it back as it was).
            let mut known = last.clone();
            let now = value(&self.kept());
            ["aws", "aws_more", "aws_ended", "cloudflare", "cloudflare_ended", "modal", "modal_more", "modal_ended", "key"].into_iter().filter(|f| now[*f] == disk[*f]).for_each(|field| {
                // (One the file leaves out, as it leaves out what is not there: left out too.)
                match (&disk[field], known.as_object_mut()) { (serde_json::Value::Null, Some(o)) => { o.remove(field); } (v, _) => known[field] = v.clone() }
            });
            // (A Kept's fields, some of them the file's: one again.)
            self.kept_as = serde_json::from_value::<crate::store::Kept>(known).map(|k| serde_json::to_string(&k).unwrap_or_default()).unwrap_or(self.kept_as.clone());
        }
    }

    /// Keeps the sign-ins as they are now (a new one, a renewed one, one that ended), when they changed. Signed in
    /// nowhere: nothing is kept.
    fn keep(&mut self) {
        let Some(store) = self.store.clone() else { return };
        self.take_in();
        let kept = self.kept();
        let text = serde_json::to_string(&kept).unwrap_or_default();
        if text == self.kept_as { return }
        let done = if kept.worth_keeping() { store.write(&kept) } else { store.remove().map(|_| ()) };
        match done {
            Ok(()) => self.kept_as = text,
            Err(e) => if !self.keep_failed { eprintln!("{e}"); self.keep_failed = true },
        }
    }

    /// Serves the dashboard until you quit it (or Ctrl-C), opening it in the browser: this workspace's pages, and
    /// every other workspace's behind the switcher. `several`: started with several workspaces and none named, so it
    /// opens on the list of them and this one is not served.
    pub fn serve(mut self, open_browser: bool, several: bool) -> Result<()> {
        // Port 8976, or the next free one when another SuperCI has it (or the one SUPERCI_PORT names). Only
        // Cloudflare's browser sign-in needs 8976 itself: it comes back to no other.
        let asked: Option<u16> = std::env::var("SUPERCI_PORT").ok().and_then(|p| p.parse().ok());
        let ports: Vec<u16> = match asked {
            Some(p) => vec![p],
            None if matches!(self.task, Some(Task::Login(Some("cloudflare")))) => vec![oauth_port()],
            None => (oauth_port()..oauth_port() + 10).collect(),
        };
        // The first of them free, and what listens there (none free: the last one tried, to say).
        let (port, bound) = ports.iter().map(|&p| (p, listen_on(p))).find(|(_, bound)| !bound.is_empty()).unwrap_or((ports[ports.len() - 1], vec![]));
        if bound.is_empty() {
            return Err(if port == oauth_port() && asked.is_none() { format!("Cloudflare's sign-in comes back to port {} only, and another program has it (another SuperCI, or `wrangler login`). Stop that one, or sign in to Cloudflare there.", oauth_port()) }
                else { format!("port {port} is in use (is another SuperCI running?)") })
        }
        if port != oauth_port() {
            self.base = format!("http://localhost:{port}");
            let shared = if std::env::var_os("SUPERCI_HOME").is_some_and(|v| !v.is_empty()) { "" } else { " Both use the same sign-ins (SUPERCI_HOME gives this one a folder of its own)." };
            if asked.is_none() { println!("Port {} is taken (another SuperCI?): this one is on {port}.{shared}", oauth_port()) }
        }
        let link = format!("{}/?k={}", self.base, self.key);
        println!("{}: {link}", match &self.task { Some(Task::Login(_)) => "Sign in with your cloud here (on this machine only)", Some(Task::GitHub { .. }) => "Create the GitHub App and choose its repositories here (on this machine only)", None => "SuperCI dashboard (on this machine only)" });
        if self.task.is_some() {
            if open_browser { open_link(&link) }
        } else if open_browser {
            println!("It is opening in your browser. Ctrl-C stops it; your runners keep working without it.");
            open_link(&link);
        } else {
            println!("Ctrl-C stops it; your runners keep working without it.");
        }
        // One thread per connection, each with a read timeout: a browser's idle spare connection cannot hold up a page.
        // The page frame needs no shared state, so it is drawn at once; the live part holds it only briefly.
        let for_task = self.task.is_some();
        let name = self.workspace();
        let hub = Arc::new(Hub { root: self.store.as_ref().and_then(|s| s.root()).or_else(crate::store::root), key: self.key.clone(), base: self.base.clone(),
            first: Mutex::new((!several).then(|| name.clone())), boards: Mutex::new(HashMap::new()), opened: Mutex::new(None) });
        self.named = name.clone();
        let shared = Arc::new(Mutex::new(self));
        if !several { lock(&hub.boards).insert(name, shared.clone()); }
        if for_task {
            // Done: kept, said, and (a moment later, so the tab gets its last page) ended.
            let shared = shared.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(300));
                // Modal's page does not come back here once the sign-in is approved (it stays on "API token
                // created"): Modal is asked from here until it is, whatever the tab does.
                let asked = { let d = lock(&shared); if matches!(d.task, Some(Task::Login(_))) { d.modal_pending.clone() } else { None } };
                if let Some(pending) = asked {
                    if let Ok(Some(session)) = modal::wait(&pending, 2.0) {
                        lock(&shared).came_modal(session);
                    }
                }
                let mut d = lock(&shared);
                if let Some(done) = d.task_done() {
                    d.keep();
                    println!("{done}");
                    drop(d);
                    // A page that looks again by itself (every three seconds) gets its last word too.
                    std::thread::sleep(Duration::from_millis(4000));
                    std::process::exit(0)
                }
            });
        }
        bound.into_iter().for_each(|listener| {
            let hub = hub.clone();
            std::thread::spawn(move || listener.incoming().flatten().for_each(|mut stream| {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let Some(req) = read_request(&mut stream) else { return };
                    write_response(&mut stream, &respond(&hub, &req));
                });
            }));
        });
        // (Each connection is answered on a thread of its own, as long as this program runs.)
        loop { std::thread::park() }
    }

    /// A client for the kept Cloudflare sign-in. Renewed by one SuperCI at a time: with the folder's lock held, the file
    /// is read again (another one may have just renewed it, and Cloudflare takes what renews a sign-in once: renewed
    /// twice, the sign-in ends), and what was renewed is written before the lock is let go.
    fn cf_client(&mut self) -> Result<Cloudflare> {
        if self.cf.as_ref().is_some_and(|s| s.due()) && !self.cf_given {
            if let Some(store) = self.store.clone() {
                let _held = store.renewing();
                self.take_in();
                let renewed = self.cf.as_mut().map(|s| s.client());
                self.keep();
                if let Some(r) = renewed { return r }
            }
        }
        self.cf.as_mut().ok_or_else(|| "Sign in with Cloudflare first.".to_string())?.client()
    }

    pub(crate) fn cloudflare(&mut self) -> Result<Cloudflare> {
        let ended = self.cf_ended;
        if self.cf.is_none() { return Err(if ended { CF_ENDED } else { "Sign in with Cloudflare first." }.into()) }
        let client = self.cf_client();
        // Cloudflare no longer renews the kept sign-in: forgotten, and said as that.
        match client { Err(e) if cloudflare::over(&e) && !self.cf_given => { self.cf_over(); Err(CF_ENDED.into()) } other => other }
    }

    fn plane(&self) -> Result<Plane> { self.planes.get(self.selected).cloned().ok_or_else(|| "no control plane yet".to_string()) }

    /// After a setting was written: waits for the control plane to show it. One that does not show it in twenty
    /// seconds is said as that, never as done (it may not have started again with it, or reads another place).
    fn shown(&mut self, plane: &Plane, done: impl Fn(&serde_json::Value) -> bool) -> Result<()> {
        self.views = None;
        if wait_for(plane.url(), &self.status_key, done) { return Ok(()) }
        Err("The change was written to the control plane's cloud, but the control plane did not show it within twenty seconds. Look again in a moment (`superci status`); if it still shows the old setting, the change did not take.".into())
    }

    /// Whose the sign-ins kept are (see `Own`).
    fn own(&self) -> Own {
        Own { aws: self.aws.as_ref().map(|a| a.account_id.clone()), cf: self.cf_accounts.iter().map(|(id, _)| id.clone()).collect(), modal: self.modal.as_ref().map(|m| m.workspace.clone()) }
    }

    /// Writes one of a control plane's secrets through its cloud's API (a Worker secret; an SSM parameter).
    fn put_secret(&mut self, plane: &Plane, name: &str, value: &str) -> Result<()> {
        self.own().check(plane)?;
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.put_secret(account_id, script, name, value),
            Plane::Aws { region, plane_id, .. } => aws_plane::put_secret(&self.aws_creds()?, region, plane_id, name, value),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, value),
            Plane::Seen { .. } => Err("This control plane is known only from AWS: sign in where it runs to change it.".into()),
        }
    }

    /// Removes a setting from the control plane (Modal's are emptied: its store keeps names).
    fn drop_secret(&mut self, plane: &Plane, name: &str) -> Result<()> {
        self.own().check(plane)?;
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.delete_secret(account_id, script, name),
            Plane::Aws { region, plane_id, .. } => aws_plane::delete_secret(&self.aws_creds()?, region, plane_id, name),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, ""),
            Plane::Seen { .. } => Err("This control plane is known only from AWS: sign in where it runs to change it.".into()),
        }
    }

    /// The AWS sign-ins due to be renewed, renewed by one SuperCI at a time, as Cloudflare's (see `cf_client`): with the
    /// folder's lock held the file is read again (another one may have just renewed them, and AWS takes a refresh token
    /// once: a second renewal with it is refused, and was taken for the sign-in's end), and what was renewed is written
    /// before the lock is let go.
    fn aws_renewed(&mut self) {
        let due = |d: &Dashboard| d.aws.iter().chain(d.aws_others.iter()).any(|s| s.due());
        if !due(self) { return }
        let Some(store) = self.store.clone() else { return };
        let _held = store.renewing();
        self.take_in();
        if !due(self) { return }
        if let Some(s) = self.aws.as_mut().filter(|s| s.due()) {
            if let Err(e) = s.credentials() { if e.starts_with(aws::ENDED) { (self.aws, self.aws_ended, self.views) = (None, true, None) } }
        }
        // (Another account's that AWS no longer renews: left out, as the one in hand is.)
        self.aws_others.retain_mut(|s| !s.due() || !s.credentials().is_err_and(|e| e.starts_with(aws::ENDED)));
        self.keep();
    }

    fn aws_creds(&mut self) -> Result<superci_core::aws::Credentials> {
        self.aws_renewed();
        match self.aws.as_mut().ok_or("Sign in with AWS first.")?.credentials() {
            // Over on AWS's side (they last twelve hours at most): forgotten here, so the pages ask for it again.
            Err(e) if e.starts_with(aws::ENDED) => { self.aws = None; self.aws_ended = true; self.views = None; Err(format!("{}. Sign in with AWS again.", aws::ENDED)) }
            r => r,
        }
    }

    /// AWS answered a call with "your credentials are over" before they said they were (see `aws::expired`): they
    /// are renewed now, or the sign-in is found ended, which the pages then say with Sign in again.
    fn aws_cut_short(&mut self) {
        if let Some(a) = self.aws.as_mut() { a.expire() }
        let _ = self.aws_creds();
    }

    /// A look at an AWS account is kept for its pages. Not one AWS did not answer: with the credentials over, the
    /// sign-in is seen to (above); otherwise the look before it stands, and AWS is asked again in a moment. Only
    /// with none before is it kept, to say that AWS did not answer.
    fn take_look(&mut self, account: String, look: aws::Look, copying: Vec<String>, behind: Vec<(String, String, String, bool)>) {
        if let Some(said) = look.unanswered().map(str::to_string) {
            if aws::expired(&said) { self.aws_looks.remove(&account); return self.aws_cut_short() }
            if let Some((at, _)) = self.aws_looks.get_mut(&account) { *at = now_ms(); return }
        }
        self.aws_copying.insert(account.clone(), copying);
        self.aws_behind.insert(account.clone(), behind);
        self.aws_looks.insert(account, (now_ms(), look));
    }

    /// Looks for control planes in the clouds you signed in to (once per sign-in), and lets the ones in Cloudflare accept this
    /// session's status key.
    fn discover(&mut self) -> Result<()> {
        // Every cloud signed in to and not looked in yet, at once.
        // (A sign-in that is over is no reason not to look where the others reach: it is forgotten and said, see
        // `cf_over`, and the control planes known there stay listed.)
        // A cloud that does not answer is said, and looked in again next time; the others are looked in meanwhile.
        self.unanswered.clear();
        let mut unanswered = vec![];
        let cf = if self.cf.is_some() && !self.looked_cf { match self.cloudflare() { Ok(cf) => Some(cf), Err(e) if e == CF_ENDED => None, Err(e) => { unanswered.push(format!("Cloudflare: {e}")); None } } } else { None };
        // Every AWS account signed in to: the one in hand, then the others.
        let mut aws: Vec<(superci_core::aws::Credentials, String)> = vec![];
        if !self.looked_aws {
            match self.aws.as_mut().map(|s| (s.credentials(), s.account_id.clone())) {
                Some((Ok(creds), account)) => aws.push((creds, account)),
                // Over on AWS's side (a sign-in lasts twelve hours at most, and one kept from yesterday has ended):
                // forgotten, so the pages ask for it again.
                Some((Err(e), _)) if e.starts_with(aws::ENDED) => { self.aws = None; self.aws_ended = true }
                Some((Err(e), _)) => unanswered.push(format!("AWS: {e}")),
                None => {}
            }
            let mut ended = vec![];
            for s in self.aws_others.iter_mut() {
                match s.credentials() {
                    Ok(creds) => aws.push((creds, s.account_id.clone())),
                    Err(e) if e.starts_with(aws::ENDED) => ended.push(s.account_id.clone()),
                    Err(e) => unanswered.push(format!("AWS: {e}")),
                }
            }
            self.aws_others.retain(|s| !ended.contains(&s.account_id));
        }
        let modal = if self.looked_modal { None } else { self.modal.clone() };
        // (Cloudflare is looked in again only after a sign-in changed, which forgets the accounts it reached: asked anew.)
        let (cf_found, aws_found, modal_found) = std::thread::scope(|s| {
            let c = cf.as_ref().map(|cf| s.spawn(move || -> Result<(Vec<(String, String)>, Vec<Plane>)> {
                let accounts = timed("Cloudflare: accounts", || cf.accounts())?;
                let planes = timed("Cloudflare: find control planes", || cf.find_planes(&accounts))?;
                Ok((accounts, planes))
            }));
            let a: Vec<_> = aws.iter().map(|(creds, _)| s.spawn(move || timed("AWS: find control planes", || aws::connected_planes(creds)))).collect();
            let m = modal.as_ref().map(|session| s.spawn(move || -> Result<Vec<Plane>> {
                let found = timed("Modal: find control planes", || modal::find_planes(session))?;
                Ok(std::thread::scope(|s| {
                    let hs: Vec<_> = found.into_iter().map(|(plane_id, url)| s.spawn(move || {
                        let label = cloudflare::health(&url).and_then(|h| h["label"].as_str().map(str::to_string)).unwrap_or_else(|| "superci".into());
                        Plane::Modal { workspace: session.workspace.clone(), url, plane_id, label }
                    })).collect();
                    hs.into_iter().filter_map(|h| h.join().ok()).collect()
                }))
            }));
            (c.map(|h| searched(h, "Cloudflare")), a.into_iter().map(|h| searched(h, "AWS")).collect::<Vec<_>>(), m.map(|h| searched(h, "Modal")))
        });
        // Where the kept control plane was looked for, and what was found there.
        let (mut looked, mut there): (Vec<String>, Vec<String>) = (vec![], vec![]);
        // Cloudflare took the token no more (ended there, where renewing it still worked a moment ago): the same.
        let cf_found = match cf_found { Some(Err(e)) if cloudflare::over(&e) && !self.cf_given => { self.cf_over(); None } found => found };
        let modal_found = match modal_found { Some(Err(e)) if e.contains(modal::ENDED) && !self.modal_given => { self.modal_over(); None } found => found };
        match cf_found {
            Some(Err(e)) => unanswered.push(format!("Cloudflare: {e}")),
            Some(Ok((accounts, planes))) => {
                looked.extend(accounts.iter().map(|(id, _)| format!("cloudflare:{id}")));
                there.extend(planes.iter().map(|p| p.plane_id().to_string()));
                self.cf_accounts = accounts;
                self.found.retain(|p| !matches!(p, Plane::Cloudflare { .. }));
                for h in planes {
                    match self.planes.iter().position(|x| x.plane_id() == h.plane_id()) { Some(i) => self.planes[i] = h, None => self.found.push(h) }
                }
                self.looked_cf = true;
            }
            None => {}
        }
        let asked_aws = !aws.is_empty();
        for (found, (_, account_id)) in aws_found.into_iter().zip(aws) {
            let found = match found { Ok(f) => f, Err(e) => { unanswered.push(format!("AWS: {e}")); continue } };
            looked.push(format!("aws:{account_id}"));
            self.found.retain(|p| !matches!(p, Plane::Aws { account_id: a, .. } if *a == account_id));
            for f in found {
                // (Only over HTTPS; and in a debug build, a test's control plane on this computer.)
                if !(f.url.starts_with("https://") || superci_core::io::local_test_address(&f.url)) { continue }
                if f.lambda.is_some() { there.push(f.plane_id.clone()) }
                let found = match f.lambda {
                    Some((region, label)) => Plane::Aws { account_id: account_id.clone(), region, url: f.url, plane_id: f.plane_id, label },
                    None => Plane::Seen { url: f.url, plane_id: f.plane_id },
                };
                // What runs here replaces what was only seen from here; otherwise the first sighting stays. One not
                // this workspace's is offered (one only seen from here, a control plane elsewhere, is not).
                match self.planes.iter().position(|p| p.plane_id() == found.plane_id()) {
                    Some(i) if matches!(self.planes[i], Plane::Seen { .. }) => self.planes[i] = found,
                    Some(_) => {}
                    None if !matches!(found, Plane::Seen { .. }) && !self.found.iter().any(|p| p.plane_id() == found.plane_id()) => self.found.push(found),
                    None => {}
                }
            }
        }
        // (Every account looked in, or looked in again next time.)
        if asked_aws && !unanswered.iter().any(|u| u.starts_with("AWS: ")) { self.looked_aws = true }
        match modal_found {
            Some(Err(e)) => unanswered.push(format!("Modal: {e}")),
            Some(Ok(found)) => {
                looked.extend(modal.as_ref().map(|m| format!("modal:{}", m.workspace)));
                self.found.retain(|p| !matches!(p, Plane::Modal { .. }));
                for found in found {
                    there.push(found.plane_id().to_string());
                    match self.planes.iter().position(|x| x.plane_id() == found.plane_id()) { Some(i) => self.planes[i] = found, None => self.found.push(found) }
                }
                self.looked_modal = true;
            }
            None => {}
        }
        // A control plane kept from the last run: looked for where it lives and not there any more, it is gone.
        let home = |p: &Plane| match p {
            Plane::Cloudflare { account_id, .. } => format!("cloudflare:{account_id}"),
            Plane::Aws { account_id, .. } => format!("aws:{account_id}"),
            Plane::Modal { workspace, .. } => format!("modal:{workspace}"),
            Plane::Seen { .. } => String::new(),
        };
        let gone: Vec<String> = self.planes.iter().filter(|p| looked.contains(&home(p)) && !there.iter().any(|id| id == p.plane_id())).map(|p| p.plane_id().to_string()).collect();
        if !gone.is_empty() {
            let shown = self.planes.get(self.selected).map(|p| p.plane_id().to_string());
            self.planes.retain(|p| !gone.iter().any(|g| g == p.plane_id()));
            if self.kept_plane.as_ref().is_some_and(|k| gone.iter().any(|g| g == k.plane_id())) { self.kept_plane = None }
            self.selected = shown.and_then(|id| self.planes.iter().position(|p| p.plane_id() == id)).unwrap_or(0);
            self.views = None;
        }
        if self.preloaded.as_ref().is_some_and(|id| self.planes.iter().find(|p| p.plane_id() == id).is_none_or(|p| looked.contains(&home(p)))) { self.preloaded = None }
        self.learn_found();
        self.key_planes();
        // Nothing to show but that: said as the page (else a note on each page, see `others_ended_notice`).
        if !unanswered.is_empty() && self.planes.is_empty() && self.found.is_empty() { return Err(unanswered.join("; ")) }
        self.unanswered = unanswered;
        Ok(())
    }

    /// A CI found in the signed-in clouds taken as this workspace's: only while it has none (a workspace looks after one
    /// CI; another is a workspace of its own). Its key is written into it as into any control plane. What is said, or
    /// why not (a status and words).
    pub fn join(&mut self, plane_id: &str) -> std::result::Result<String, (u16, String)> {
        let Some(i) = self.found.iter().position(|p| p.plane_id() == plane_id) else { return Err((404, format!("{plane_id} is not a control plane found in your signed-in clouds"))) };
        if self.planes.iter().any(|p| !matches!(p, Plane::Seen { .. })) { return Err((409, "This workspace looks after a CI already: make a workspace for the other one, sign in there and pick it".into())) }
        let p = self.found.remove(i);
        self.planes.retain(|x| x.plane_id() != p.plane_id());
        self.planes.push(p.clone());
        self.select(self.planes.len() - 1);
        self.kept_plane = Some(p.clone());
        self.views = None;
        self.key_planes();
        self.keep();
        Ok(format!("This workspace looks after {} now.", p.place()))
    }

    /// Another control plane in the same AWS account as `plane` called `name` (whatever the case): its id. None where
    /// none is, or the account cannot be read (a name is not refused for what was not seen).
    fn name_taken(&mut self, plane: &Plane, name: &str) -> Option<String> {
        let Plane::Aws { account_id, .. } = plane else { return None };
        let creds = self.aws.iter_mut().chain(self.aws_others.iter_mut()).find(|s| s.account_id == *account_id)?.credentials().ok()?;
        let others = aws::connected_planes(&creds).ok()?;
        others.into_iter().filter(|o| o.plane_id != plane.plane_id()).filter_map(|o| Some((o.plane_id, o.lambda?.0))).find(|(id, region)| aws_plane::about(&creds, region, id).ok().and_then(|(n, _)| n).is_some_and(|n| n.eq_ignore_ascii_case(name))).map(|(id, _)| id)
    }

    /// The control planes found in the signed-in clouds that are not this workspace's, with what each says of itself.
    pub fn found(&self) -> Vec<(Plane, Option<String>, Option<String>)> {
        self.found.iter().map(|p| { let a = self.found_about.get(p.plane_id()).cloned().unwrap_or_default(); (p.clone(), a.name, a.owners) }).collect()
    }

    /// What each control plane found and not yet asked says of itself (at once, each in its own thread).
    fn learn_found(&mut self) {
        let mut creds: Vec<(String, superci_core::aws::Credentials)> = vec![];
        for s in self.aws.iter_mut().chain(self.aws_others.iter_mut()) { if let Ok(c) = s.credentials() { creds.push((s.account_id.clone(), c)) } }
        let asked: Vec<Plane> = self.found.iter().filter(|p| !self.found_about.contains_key(p.plane_id())).cloned().collect();
        let learnt: Vec<(String, FoundAbout)> = std::thread::scope(|s| {
            let hs: Vec<_> = asked.iter().map(|p| { let creds = &creds; s.spawn(move || {
                let health = cloudflare::health(p.url()).unwrap_or_default();
                // (One not answering is taken as a CI: nothing of it is taken in by guess.)
                // (One standing by for a move is not in use yet, whatever it was given: a place, as `serves_its_own` has it.)
                let serves = health.is_null() || (health["standby"] != true && (health["github"] == true || health["gitlab"] == true || health["moved_to"].is_string()));
                let mut a = FoundAbout { label: health["label"].as_str().map(str::to_string), version: health["version"].as_str().map(str::to_string), serves, ..Default::default() };
                if let Plane::Aws { account_id, region, plane_id, .. } = p {
                    if let Some((_, c)) = creds.iter().find(|(id, _)| id == account_id) { if let Ok((name, owners)) = aws_plane::about(c, region, plane_id) { (a.name, a.owners) = (name, owners) } }
                }
                (p.plane_id().to_string(), a)
            }) }).collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        self.found_about.extend(learnt);
        // Beside a CI this workspace looks after, a place set up and never used (what a move goes to, a deploy that
        // stopped) is this workspace's, as before: moved to, or deleted, from here. Only a CI is another's.
        if self.planes.iter().any(|p| !matches!(p, Plane::Seen { .. })) {
            let unused: Vec<Plane> = self.found.iter().filter(|p| self.found_about.get(p.plane_id()).is_some_and(|a| !a.serves)).cloned().collect();
            for p in unused { self.found.retain(|f| f.plane_id() != p.plane_id()); self.planes.push(p) }
        }
    }

    /// The control planes found in the signed-in clouds that are not this workspace's, each with what it says of
    /// itself and its one action: use it here (while this workspace looks after none), or open the workspace on this
    /// computer that looks after it. Nothing when none was found.
    fn found_card(&self) -> String {
        if self.found.is_empty() { return String::new() }
        let me = self.store.as_ref().map(|s| s.workspace()).unwrap_or_default();
        let others: Vec<crate::store::Listed> = crate::store::root().map(|r| crate::store::listed(&r, None)).unwrap_or_default().into_iter().filter(|l| l.id != me && !l.plane_id.is_empty()).collect();
        let mine = self.planes.iter().any(|p| !matches!(p, Plane::Seen { .. }));
        let rows: String = self.found.iter().map(|p| {
            let a = self.found_about.get(p.plane_id()).cloned().unwrap_or_default();
            let title = a.name.clone().unwrap_or_else(|| format!("A CI ({})", p.plane_id()));
            let mut what = vec![p.place()];
            if let Some(o) = a.owners.as_deref().map(owners_said).filter(|o| !o.is_empty()) { what.push(o) }
            if let Some(l) = &a.label { what.push(format!("runs-on {l}")) }
            if let Some(v) = &a.version { what.push(v.clone()) }
            let end = match others.iter().find(|l| l.plane_id == p.plane_id()) {
                Some(l) => format!(r#"<a class="button secondary sm" href="/?w={}">Open {}</a>"#, esc(&l.id), esc(l.title())),
                None if !mine => row_form("/plane/join", &format!(r#"<input type="hidden" name="plane" value="{}">"#, esc(p.plane_id())), "Use this one"),
                None => r#"<span class="note">Another CI</span>"#.to_string(),
            };
            format!(r#"<div class="pick act">{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{end}</span></div>"#, logo(p.cloud(), 36), esc(&title), esc(&what.join(" · ")))
        }).collect();
        let (head, note) = if mine { ("Other CIs in your signed-in accounts", "A workspace looks after one CI. To look after one of these too, make a workspace for it (the switcher, New workspace…) and pick it there.") }
            else { ("CIs in your signed-in accounts", "Use one of these in this workspace (teammates each use it from their own computer), or set up a new one below.") };
        format!(r#"<h2 class="list-title">{head}</h2><p class="note" style="margin:-4px 0 14px">{note}</p><div class="picks page">{rows}</div>"#)
    }

    /// A key kept from before each control plane had one of its own is the same value in every control plane that
    /// has it. It is replaced once every control plane listed can be handed its own key (each written with its own
    /// account's sign-in); until then it goes on working, so that nothing becomes unreadable (a control plane whose
    /// AWS sign-in has ended is still read with it). Tried again at each look.
    fn renew_folder_key(&mut self) {
        if self.status_key.starts_with(cloudflare::FOLDER_KEY) { return }
        let planes: Vec<Plane> = self.planes.iter().filter(|p| !matches!(p, Plane::Seen { .. })).cloned().collect();
        if planes.is_empty() {
            if self.keyed.is_empty() { self.status_key = folder_key() }
            return;
        }
        let (new, name) = (folder_key(), format!("DEVICE_KEY_{}_{}", now_ms() / 1000 + 30 * 86_400, superci_core::crypto::random_id(6).to_uppercase()));
        let writer = Writer { cf: if self.cf.is_some() { self.cf_client().ok() } else { None }, aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone(), own: self.own() };
        let before: Vec<bool> = planes.iter().map(|p| before_names(p.url())).collect();
        if !planes.iter().zip(&before).all(|(p, b)| writer.put(p, &name, &cloudflare::plane_key(&new, p.url())).is_ok() && (!b || writer.put(p, &old_key_name(&name), &cloudflare::plane_key(&new, p.url())).is_ok())) { return }
        planes.iter().for_each(|p| say_device(p.url(), &new));
        // (The key it replaces ends by itself: a command still running with it, here or in another terminal, goes on.)
        self.status_secret = name;
        self.status_key = new;
        self.keyed = planes.iter().map(|p| p.plane_id().to_string()).collect();
    }

    /// Lets every control plane this dashboard can change accept this session's status key. Earlier sessions' keys that
    /// have expired are removed afterwards, in the background: the page does not wait for that.
    fn key_planes(&mut self) {
        self.renew_folder_key();
        let unkeyed = |d: &Dashboard| -> Vec<Plane> { d.planes.iter().filter(|p| !matches!(p, Plane::Seen { .. }) && !d.keyed.contains(p.plane_id())).cloned().collect() };
        if unkeyed(self).is_empty() { return }
        // (Its sign-ins renewed first: that takes the lock below too.)
        let writer = Writer { cf: if self.cf.is_some() { self.cf_client().ok() } else { None }, aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone(), own: self.own() };
        // One SuperCI at a time on this computer (commands run side by side): a key made meanwhile by another one, and
        // written, is taken instead of handing one more; this one's is written before the next goes on.
        let held = self.store.clone().and_then(|s| s.renewing());
        if self.keyed.is_empty() {
            if let Some(k) = self.store.as_ref().and_then(|s| s.read().key).filter(|k| k.name != self.status_secret && key_until(&k.name) > now_ms() / 1000 + 86_400) {
                (self.status_key, self.status_secret, self.keyed) = (k.value, k.name, k.planes.into_iter().collect());
            }
        }
        let unkeyed = unkeyed(self);
        if unkeyed.is_empty() { return self.keep() }
        // Handed to each at once.
        let (name, key) = (self.status_secret.clone(), self.status_key.clone());
        let done: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = unkeyed.iter().map(|p| { let (writer, name, key) = (&writer, &name, &key); s.spawn(move || timed(&format!("{}: hand it this session's key", p.place()), || {
                writer.put(p, name, &cloudflare::plane_key(key, p.url()))?;
                // (One not updated yet reads it under the old name.)
                if before_names(p.url()) { writer.put(p, &old_key_name(name), &cloudflare::plane_key(key, p.url()))? }
                say_device(p.url(), key);
                Ok::<(), String>(())
            }).ok().map(|_| p.plane_id().to_string())) }).collect();
            hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        });
        self.keyed.extend(done);
        self.keep();
        drop(held);
        let mut tidy: Vec<Box<dyn FnOnce() + Send>> = vec![];
        unkeyed.iter().for_each(|p| {
            // (And the keys an older SuperCI on another device hands under the old name, which nothing here reads.)
            let expired = |names: Vec<String>| { let now = now_ms() / 1000; names.into_iter().filter(move |n| n.strip_prefix("DEVICE_KEY_").or_else(|| n.strip_prefix("DASHBOARD_KEY_")).and_then(|r| r.split('_').next().unwrap_or_default().parse::<u64>().ok()).is_some_and(|until| until < now)) };
            match p.clone() {
                Plane::Cloudflare { account_id, script, .. } => if let Ok(cf) = self.cloudflare() {
                    tidy.push(Box::new(move || for old in expired(cf.secret_names(&account_id, &script).unwrap_or_default()) { let _ = cf.delete_secret(&account_id, &script, &old); }));
                },
                Plane::Aws { region, plane_id, .. } => if let Ok(creds) = self.aws_creds() {
                    tidy.push(Box::new(move || for old in expired(aws_plane::secret_names(&creds, &region, &plane_id).unwrap_or_default()) { let _ = aws_plane::delete_secret(&creds, &region, &plane_id, &old); }));
                },
                // Modal keeps expired keys; the control plane ignores them. (One only seen has no key: not among these.)
                _ => {}
            }
        });
        if !tidy.is_empty() { std::thread::spawn(move || tidy.into_iter().for_each(|job| job())); }
    }

    /// A deploy that finished: its control plane joins the list and is shown (discovery then gives it the key).
    fn adopt_deploy(&mut self) {
        let finished = { let mut g = lock(&self.deploying); if g.as_ref().is_some_and(|x| matches!(x.result, Some(Ok(_)))) { g.take() } else { None } };
        if let Some(Deploy { result: Some(Ok(p)), .. }) = finished {
            self.selected = match self.planes.iter().position(|x| x.plane_id() == p.plane_id()) { Some(i) => i, None => { self.planes.push(p); self.planes.len() - 1 } };
            self.show_setup = false;
            // The workspace's first control plane is called what the workspace was named (another one gets its
            // name with a move). Not done: it is named in Settings.
            if self.planes.len() == 1 && !self.about.name.is_empty() && self.about.name != "SuperCI" {
                let (plane, name) = (self.planes[0].clone(), self.about.name.clone());
                if let Plane::Aws { account_id, .. } = &plane { self.hold_account(account_id); }
                let _ = self.put_secret(&plane, "NAME", &name);
            }
            *lock(&self.deployed_to) = self.planes.get(self.selected).map(|p| p.plane_id().to_string());
            *lock(&self.deployed) = true;
        }
    }

    fn deploy_running(&self) -> bool { lock(&self.deploying).as_ref().is_some_and(|d| d.result.is_none()) }

    /// Runs a deploy on its own thread, recording each step it starts and how it ends.
    fn start_deploy(&self, cloud: &'static str, place: String, steps: &'static [&'static str], form: Vec<(&'static str, String)>,
        work: impl FnOnce(&dyn Fn(usize)) -> Result<Plane> + Send + 'static) {
        let state = self.deploying.clone();
        *lock(&state) = Some(Deploy { cloud, place, steps, at: 0, form, result: None });
        let quiet = self.commanding;
        if !quiet { eprintln!("Deploying a control plane to {}…", provider_name(cloud)); }
        std::thread::spawn(move || {
            // (Its state is set before this thread starts, and nothing takes it away until it has a result.)
            let step = |i: usize| lock(&state).as_mut().expect("a deploy under way").at = i;
            let result = work(&step);
            // Said in the dashboard's terminal; a command says how it ended itself.
            if !quiet { match &result { Ok(p) => eprintln!("The control plane is running: {}", p.url()), Err(e) => eprintln!("The deploy stopped: {e}") } }
            // A stop keeps the step it stopped at; a finish marks them all done.
            let mut d = lock(&state);
            let d = d.as_mut().expect("a deploy under way");
            if result.is_ok() { d.at = d.steps.len() }
            d.result = Some(result);
        });
    }

    fn handle(&mut self, req: &Req) -> Result<Response> {
        // (What a page or an action does with AWS uses credentials renewed one SuperCI at a time.)
        self.aws_renewed();
        let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
        // Back to the dashboard after a sign-in elsewhere: a same-site navigation, so the page's cookie goes along
        // (a plain redirect at the end of another site's redirect chain would count as cross-site and lose it).
        let home = self.back_url("");
        let back = |heading: &str, text: &str| back_to(heading, text, &home);
        let form: Vec<(String, String)> = url::form_urlencoded::parse(&req.body).into_owned().collect();
        let field = |n: &str| form.iter().find(|(k, _)| k == n).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
        let form_has = |n: &str| form.iter().any(|(k, _)| k == n);
        match (req.method.as_str(), req.path.as_str()) {

            // A run from a computer's artifact, as its file (a zip, as upload-artifact made it).
            ("GET", "/run/artifact") => {
                let (run, name) = (q("run"), q("name"));
                if run.is_empty() || !run.bytes().all(|b| b.is_ascii_digit()) || name.is_empty() { return Ok(message(400, "Which artifact?", "")) }
                let Some(v) = self.current()? else { return Ok(message(404, "No control plane", "")) };
                let bytes = artifact_bytes(v.plane.url(), &self.status_key, run, name)?;
                let file: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
                return Ok(Response::new(200, "application/zip", bytes).with_header("content-disposition", &format!("attachment; filename=\"{file}.zip\"")))
            }

            // A job's log as its page shows it (`/job/view`: in steps, see `log_view`), and as it is (`/job/raw`).
            ("GET", "/job/view") | ("GET", "/job/raw") => {
                let (id, gl) = (q("id"), q("gl"));
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || !gl.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') { return Ok(message(400, "Which job?", "")) }
                let Some(v) = self.current()? else { return Ok(message(404, "No control plane", "")) };
                let job = v.jobs().into_iter().find(|j| j["job_id"].as_u64().is_some_and(|n| n.to_string() == id) && (gl.is_empty() || j["gitlab"].as_str() == Some(gl)));
                let mut path = format!("/job/log?id={id}&bytes={}&steps=1", if req.path == "/job/raw" { 4_000_000 } else { 1_000_000 });
                if !gl.is_empty() || job.as_ref().is_some_and(|j| j["provider"] == "gitlab") { path += &format!("&gl={gl}") }
                // A GitHub job's whole log, from GitHub itself (any size).
                if req.path == "/job/raw" && job.as_ref().is_some_and(|j| j["provider"].as_str().unwrap_or_default().is_empty()) {
                    if let Ok(url) = cloudflare::plane_get(v.plane.url(), &self.status_key, &format!("/job/log?id={id}&link=1")).map(|g| g["url"].as_str().unwrap_or_default().to_string()) {
                        if url.starts_with("https://") { return Ok(Response::new(302, "text/plain", String::new()).with_header("location", &url)) }
                    }
                }
                // A finished job's page shows its whole log, whatever its size (GitHub's from GitHub, a run from a
                // computer's from the bucket): the control plane says its steps, the log comes whole. While it runs,
                // the last of it, as it comes.
                let ended = job.as_ref().filter(|j| !["launching", "launched", "running", "waiting"].contains(&j["state"].as_str().unwrap_or_default()));
                if req.path == "/job/view" {
                    if let Some(j) = ended {
                        // (A finished job's view, kept for its next opens: its log can be many MB, read from GitHub.)
                        let kept_as = format!("{}|{id}|{gl}", v.plane.url());
                        if let Some((_, kept)) = lock(&LOG_VIEWS).iter().find(|(k, _)| *k == kept_as) { return Ok(Response::new(200, "application/json", kept.clone())) }
                        if let Ok(whole) = whole_log(v.plane.url(), &self.status_key, j) {
                            let mut got = cloudflare::plane_get(v.plane.url(), &self.status_key, &path.replace("bytes=1000000", "bytes=1000")).unwrap_or_default();
                            got["log"] = String::from_utf8_lossy(&whole).into_owned().into();
                            got["truncated"] = false.into();
                            got["bytes"] = whole.len().into();
                            // Steps run side by side: each one's own log, from its run's (their lines are mixed in
                            // the job's). Without it, the lines by their times, not kept (asked again next time).
                            let mixed = j["provider"].as_str().unwrap_or_default().is_empty() && crate::log_view::side_by_side(&got["steps"]);
                            if mixed { if let Some(own) = step_logs(v.plane.url(), &self.status_key, j) { got["step_logs"] = own } }
                            let mut shown = crate::log_view::view(j, &got, false);
                            if mixed && got["step_logs"].is_null() { shown["mixed"] = true.into(); return Ok(Response::new(200, "application/json", shown.to_string())) }
                            let shown = shown.to_string();
                            let mut kept = lock(&LOG_VIEWS);
                            kept.retain(|(k, _)| *k != kept_as);
                            kept.push((kept_as, shown.clone()));
                            if kept.len() > 4 { kept.remove(0); }
                            return Ok(Response::new(200, "application/json", shown))
                        }
                    }
                }
                let got = cloudflare::plane_get(v.plane.url(), &self.status_key, &path).unwrap_or_else(|e| serde_json::json!({ "error": e }));
                if req.path == "/job/raw" {
                    // (A run from a computer's whole, from its pieces in the bucket.)
                    let whole = match job.as_ref().filter(|j| j["provider"] == "local") { Some(j) => whole_log(v.plane.url(), &self.status_key, j).ok(), None => None };
                    let body = whole.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| got["log"].as_str().unwrap_or_default().to_string());
                    return Ok(Response::new(200, "text/plain; charset=utf-8", body).with_header("content-disposition", &format!("attachment; filename=\"job-{id}.log\"")))
                }
                let job = job.unwrap_or_default();
                let going = ["launching", "launched", "running", "waiting"].contains(&job["state"].as_str().unwrap_or_default());
                let mut shown = crate::log_view::view(&job, &got, going);
                // (Cut by a control plane that gives no whole log yet: its update is what shows it.)
                shown["older"] = view::older(v.version.as_deref(), "0.19.1").into();
                return Ok(Response::new(200, "application/json", shown.to_string()))
            }

            // The connect screen's links: each remembers itself as the cloud picked last ("Last used"), in this browser only.
            ("GET", "/connect/cloudflare") | ("POST", "/cloudflare/signin") if self.port() != oauth_port() => Ok(message(409, "Cloudflare's sign-in needs port 8976",
                &format!("Cloudflare's sign-in comes back to http://localhost:{} only, and this dashboard is on port {}. Stop the SuperCI that has {} and start this one again, or sign in to Cloudflare there.", oauth_port(), self.port(), oauth_port()))),
            ("GET", "/connect/cloudflare") => {
                let (link, pending) = cloudflare::authorize();
                self.cf_pending = Some(pending);
                self.return_to.clear();
                Ok(leave(&link, "Cloudflare").with_header("set-cookie", &last_used_cookie("cloudflare")))
            }
            ("GET", "/connect/aws") => {
                let (link, pending) = aws::authorize("us-east-1", &format!("http://127.0.0.1:{}/oauth/callback", self.port()));
                self.aws_pending = Some(pending);
                self.aws_asked_ms = now_ms();
                self.return_to.clear();
                Ok(leave(&link, "AWS").with_header("set-cookie", &last_used_cookie("aws")))
            }
            ("GET", "/connect/modal") => {
                if self.task.is_some() { println!("Approve it in Modal's tab. Modal's page stays where it is afterwards: this ends by itself, and the tab can be closed.") }
                let pending = modal::authorize(&format!("{}/oauth/modal", self.base))?;
                let link = pending.web_url.clone();
                self.modal_pending = Some(pending);
                self.return_to.clear();
                Ok(leave(&link, "Modal").with_header("set-cookie", &last_used_cookie("modal")))
            }
            ("POST", "/plane/modal") => {
                if self.deploy_running() { return Ok(Response::redirect("/?p=plane")) }
                let session = self.modal.clone().ok_or("Sign in with Modal first.")?;
                // Every control plane answers to the same label (one is in use at a time; workflows do not change on a move).
                let label = "superci".to_string();
                let again = field("plane");
                let plane_id = if again.len() == 12 && again.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) { again } else { cloudflare::new_plane_id() };
                self.start_deploy("modal", format!("workspace {}", session.workspace), &modal::DEPLOY_STEPS, vec![("plane", plane_id.clone())], move |step| {
                    let url = modal::deploy_plane(&session, &plane_id, &label, step)?;
                    step(2);
                    wait_online(&url);
                    Ok(Plane::Modal { workspace: session.workspace.clone(), url, plane_id, label })
                });
                Ok(Response::redirect(if self.planes.is_empty() { "/?p=plane" } else { "/?p=planes" }))
            }
            // An AWS control plane whose AWS runners were removed: its role gets the machines' policy again, its network
            // is made, and it starts machines again.
            ("POST", "/runners/aws-own") => {
                let plane = self.plane()?;
                let Plane::Aws { account_id, region, .. } = &plane else { return Ok(message(400, "Not a control plane in AWS", "")) };
                if !self.hold_account(account_id) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {}.", esc(account_id)))) }
                let creds = self.aws_creds()?;
                // With its runners off its status lists no regions: its own, then (its network is made in each).
                let mut regions = status_soon(plane.url(), &self.status_key).map(|s| status_regions(&s)).unwrap_or_default();
                if regions.is_empty() { regions.push(region.clone()) }
                aws::give_runner_role(&creds, account_id, plane.plane_id(), &regions)?;
                self.put_secret(&plane, "AWS_RUNNERS", "on")?;
                wait_for(plane.url(), &self.status_key, |s| s["aws"]["connected"] == true);
                Ok(Response::redirect("/?p=runners"))
            }
            // Removing a runner provider: the control plane forgets it first (no job goes there from then), then what was
            // made for it in that cloud is deleted with your sign-in there. The control plane itself, and your sign-ins,
            // stay. "Only": forgotten here, its cloud's part left (listed), for when you cannot sign in there.
            ("POST", "/runners/remove") => {
                let plane = self.plane()?;
                // The control plane the dialog was shown for (another may be in use since).
                if form_has("plane") && field("plane") != plane.plane_id() { return Ok(message(409, "Another control plane is in use now", "Open Runners again and remove it there.")) }
                let cloud = field("cloud");
                if !["aws", "cloudflare", "modal"].contains(&cloud.as_str()) && !pool_name_ok(&cloud) { return Ok(message(400, "Unknown provider", "")) }
                let only = field("only") == "on";
                let Some(status) = status_soon(plane.url(), &self.status_key) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let name = cloud_name(&cloud).to_string();
                let running = running_on(&status, &cloud);
                if running > 0 && field("stop") != "on" {
                    return Ok(message(409, &format!("{running} job{} running on {name}", if running == 1 { " is" } else { "s are" }), "Remove it when they finish, or stop them as you remove it."))
                }
                let own = status["own_cloud"].as_str().unwrap_or(match &plane { Plane::Modal { .. } => "modal", _ => "cloudflare" }).to_string();
                let own_containers = own == cloud && status["containers"] == true;
                let account = match &plane { Plane::Aws { account_id, .. } => Some(account_id.clone()), _ => status["aws"]["account_id"].as_str().map(str::to_string) };
                // (AWS in no account it knows of, removed already: nothing there to sign in to or delete.)
                let nowhere = cloud == "aws" && account.is_none();
                let only = only || nowhere;
                // Signed in where its part is, before anything changes.
                if !only {
                    let signed_in = match cloud.as_str() {
                        "aws" => account.as_ref().is_some_and(|a| self.hold_account(a)),
                        _ if own_containers => true,
                        c if superci_core::plane::kind_of(c) == "modal" => self.modal_for(c).is_some(),
                        _ => self.cf.is_some(),
                    };
                    if !signed_in {
                        let whom = if cloud == "aws" { format!(" to account {}", account.as_deref().unwrap_or("")) } else { String::new() };
                        return Ok(message(403, &format!("Sign in with {name} first"), &format!("Sign in{} to delete what SuperCI made there, or remove it from SuperCI only.", esc(&whom))))
                    }
                }
                // What deleting its part needs, read now: a sign-in that ended is said before anything changes.
                let aws_creds = if cloud == "aws" && !only { Some(self.aws_creds()?) } else { None };
                let cf = if cloud == "cloudflare" && !only && !own_containers { Some(self.cloudflare()?) } else { None };
                // (Signed in to Modal, above, for its part.)
                let md = if superci_core::plane::kind_of(&cloud) == "modal" && !only && !own_containers { self.modal_for(&cloud).cloned() } else { None };
                // 1. Forgotten: out of the order (and any rule naming it), then its connection.
                let mut routing: Routing = serde_json::from_value(status["routing"].clone()).unwrap_or_default();
                routing.order.retain(|p| p.cloud != cloud && !(cloud == "aws" && p.cloud == AWS_ON_DEMAND));
                routing.rules.retain(|r| r.cloud != cloud);
                if routing.default.as_deref() == Some(cloud.as_str()) { routing.default = None }
                // (AWS's own budget for the account goes with AWS: see below.)
                let had_budget = cloud == "aws" && routing.aws_budget.take().is_some();
                self.put_secret(&plane, "ROUTING", &json_text(&routing))?;
                if cloud == "aws" {
                    if matches!(plane, Plane::Aws { .. }) { self.put_secret(&plane, "AWS_RUNNERS", "off")? } else { cloudflare::plane_post(plane.url(), &self.status_key, "/aws/forget", &serde_json::json!({}))?; }
                } else if own_containers {
                    self.put_secret(&plane, "CONTAINERS", "off")?;
                } else {
                    let mut launchers: Vec<superci_core::plane::Launcher> = serde_json::from_value(status["launchers"].clone()).unwrap_or_default();
                    launchers.retain(|a| a.cloud != cloud);
                    self.put_secret(&plane, "LAUNCHERS", &json_text(&launchers))?;
                }
                let c = cloud.clone();
                let forgotten = wait_for(plane.url(), &self.status_key, move |s| !provider_connected(s, &c));
                // 2. Its part in that cloud (its machines go with it).
                let id = plane.plane_id().to_string();
                let mut left = vec![];
                match (cloud.as_str(), only, &aws_creds, &cf, &md) {
                    (_, _, Some(creds), _, _) => {
                        // What lets the control plane start machines goes first: none can start after, whether or
                        // not it has taken the change yet. Then its machines and their network, once more if a
                        // machine started meanwhile.
                        let gone = match &plane {
                            Plane::Aws { region, .. } => aws_plane::delete_runner_policy(creds, region, &id),
                            _ => aws::disconnect_runners(creds, account.as_deref().unwrap_or_default(), plane.url(), &id),
                        };
                        if let Err(e) = gone { left.push(format!("its role ({e})")) }
                        // Its budget in AWS itself, with the rule and the role made for it, if there was one.
                        if had_budget { if let Err(e) = crate::aws_budget::with(creds, account.as_deref().unwrap_or_default(), &id, |b| b.remove()) { left.push(format!("its budget in AWS ({e})")) } }
                        if let Err(e) = aws::delete_networks(creds, &id, &REGIONS).or_else(|_| aws::delete_networks(creds, &id, &REGIONS)) { left.push(format!("its machines' network ({e}); remove AWS again to delete it")) }
                    }
                    // A control plane that has not taken the change yet may still start a runner there: its part is
                    // left until it has (nothing is half-deleted under a running job).
                    (_, false, ..) if !forgotten && !own_containers => left.push(format!("its part in {name}: the control plane had not taken the change yet. Add {name} again and remove it in a moment")),
                    ("aws", true, ..) if nowhere => {}
                    ("aws", true, ..) => left.push(format!("in AWS account {}: the role superci-plane-{id}{}, and its machines' network (none cost anything; add AWS again and remove it while signed in to delete them)",
                        account.as_deref().unwrap_or(""), if matches!(plane, Plane::Aws { .. }) { "'s machines policy" } else { " and its identity provider" })),
                    (_, _, _, _, Some(session)) => if let Err(e) = modal::delete_runners(session, &id) { left.push(format!("its Modal app superci-runners-{id} ({e})")) },
                    (_, _, _, Some(cf), _) => for (a, _) in self.cf_accounts.clone() { if let Err(e) = cf.delete_runners(&a, &id) { left.push(format!("its Cloudflare launcher ({e})")) } },
                    (_, _, _, None, None) if own_containers => {}
                    (c, ..) if superci_core::plane::kind_of(c) == "modal" => left.push(format!("the Modal app superci-runners-{id} in your workspace")),
                    _ => left.push(format!("the Cloudflare launcher superci-runners-{id}")),
                }
                self.views = None;
                self.flash = Some(format!("Removed {name}.{}", if left.is_empty() { String::new() } else { format!(" Left: {}.", left.join("; ")) }));
                Ok(Response::redirect("/?p=runners"))
            }
            // A Modal control plane starts sandboxes itself: switching them on is one setting.
            ("POST", "/runners/modal-own") => {
                let plane = self.plane()?;
                // (In Function mode: its Functions again, for the images there are now.)
                if let Some(r) = status_soon(plane.url(), &self.status_key).and_then(|s| serde_json::from_value::<Routing>(s["routing"].clone()).ok()).filter(|r| r.pool("modal").is_some_and(|p| p.function)) { self.deploy_jobs(&plane, "modal", &r.images)? }
                self.put_secret(&plane, "CONTAINERS", "on")?;
                wait_for(plane.url(), &self.status_key, |s| s["containers"] == true);
                Ok(Response::redirect("/?p=runners"))
            }
            // GitLab: a token (checked here, kept in the control plane's secrets), then projects switched on and off by
            // the control plane itself (it adds or removes their webhooks).
            // GitLab: a token (checked here, kept in the control plane's secrets). `which`: nothing for the first
            // connection (or the first again, with a new token), "new" for a further one, or a further one's name.
            ("POST", "/gitlab/connect") => {
                let plane = self.plane()?;
                let url = field("url").trim_end_matches('/').to_string();
                let token = field("token");
                if !url.starts_with("https://") || url.contains(' ') { return Ok(message(400, "GitLab's address starts with https://", "")) }
                let me = match gitlab_check(&url, &token) { Ok(me) => me, Err(e) => return Ok(message(400, "GitLab did not accept the token", &esc(&e))) };
                let seen = view::plane_view(&plane, Some(&self.status_key));
                let have = seen.gitlabs();
                let which = match field("which").as_str() {
                    "new" if have.is_empty() => String::new(),
                    "new" => {
                        if view::older(seen.version.as_deref(), "0.9.37") { return Ok(message(409, "Update the control plane first", "Another GitLab needs control plane 0.9.37 or newer (Control plane → Update).")) }
                        format!("g{}", superci_core::crypto::random_id(5).to_lowercase())
                    }
                    id if id.is_empty() || have.iter().any(|(h, ..)| h == id) => id.to_string(),
                    _ => return Ok(message(400, "That GitLab is not connected here", "")),
                };
                let before = have.iter().any(|(h, ..)| *h == which);
                let mut setting = serde_json::json!({ "url": url, "token": token, "hook_secret": random_token(24) });
                if !which.is_empty() { setting["id"] = which.clone().into() }
                self.put_secret(&plane, &if which.is_empty() { "GITLAB".to_string() } else { format!("GITLAB_{which}") }, &setting.to_string())?;
                let (w, u) = (which.clone(), url.clone());
                wait_for(plane.url(), &self.status_key, move |s| if w.is_empty() { s["gitlab"]["url"] == u.as_str() } else { s["gitlabs"].as_array().is_some_and(|l| l.iter().any(|g| g["id"] == w.as_str() && g["url"] == u.as_str())) });
                // Connected again: its webhooks get the new secret.
                if before { let _ = cloudflare::plane_post(plane.url(), &self.status_key, "/gitlab/projects", &serde_json::json!({ "refresh": true, "gitlab": which })); }
                eprintln!("GitLab connected as {me}");
                self.views = None;
                Ok(Response::redirect(&format!("/?p=gitlab&g={which}")))
            }
            ("POST", "/gitlab/project") => {
                let plane = self.plane()?;
                let which = field("gitlab");
                let mut change = if field("all") == "on" { serde_json::json!({ "all_on": true }) } else {
                    let id: u64 = field("id").parse().map_err(|_| "which project?")?;
                    serde_json::json!({ "id": id, "enabled": field("enabled") == "true" })
                };
                change["gitlab"] = which.clone().into();
                cloudflare::plane_post(plane.url(), &self.status_key, "/gitlab/projects", &change)?;
                Ok(Response::redirect(&format!("/?p=gitlab&g={}", safe_id(&which))))
            }
            // Its projects stop sending jobs here (their webhooks go), then its token is forgotten.
            ("POST", "/gitlab/disconnect") => {
                let plane = self.plane()?;
                let which = safe_id(&field("gitlab"));
                let _ = cloudflare::plane_post(plane.url(), &self.status_key, "/gitlab/projects", &serde_json::json!({ "all_off": true, "gitlab": which }));
                if which.is_empty() {
                    self.put_secret(&plane, "GITLAB", "off")?;
                    wait_for(plane.url(), &self.status_key, |s| s["gitlab"].is_null());
                } else {
                    self.drop_secret(&plane, &format!("GITLAB_{which}"))?;
                    let w = which.clone();
                    wait_for(plane.url(), &self.status_key, move |s| !s["gitlabs"].as_array().is_some_and(|l| l.iter().any(|g| g["id"] == w.as_str())));
                }
                self.views = None;
                Ok(Response::redirect("/?p=repos"))
            }
            ("POST", "/cloudflare/signin") => {
                self.return_to = safe_return(&field("next"));
                let (link, pending) = cloudflare::authorize();
                self.cf_pending = Some(pending);
                Ok(Response::redirect(&link))
            }
            ("GET", "/oauth/callback") if self.aws_pending.as_ref().is_some_and(|p| safe_eq(q("state").as_bytes(), p.state.as_bytes())) => {
                let pending = self.aws_pending.take().unwrap();
                if !q("error").is_empty() { return Ok(message(400, "AWS sign-in was not completed", &esc(q("error")))) }
                // AWS refused its last step (seen when the account was changed on its page midway): said plainly,
                // with a button that signs in again (signed out of AWS first). (This page is at 127.0.0.1, which a
                // browser holds apart from the dashboard's localhost: a link there, not a form.)
                let session = match aws::exchange(pending, q("code")) {
                    Ok(s) => s,
                    Err(e) => {
                        let said = e.split("\"message\":\"").nth(1).and_then(|m| m.split('"').next()).unwrap_or(&e).trim_end_matches('.').to_string();
                        return Ok(document(400, "SuperCI", &format!(r#"<div class="wait"><div>{MARK}<h1>AWS did not finish the sign-in</h1><p>It refused its last step ({}). This can happen when the account is changed on its page midway. Signing in again works.</p><p style="margin-top:18px"><a class="button primary" href="{}">Sign in again</a></p></div></div>"#,
                            esc(&said), esc(&self.back_url(&format!("aws/again?next={}", url::form_urlencoded::byte_serialize(self.return_to.as_bytes()).collect::<String>())))), None))
                    }
                };
                println!("Signed in to AWS account {}.", session.account_id);
                self.add_account(session);
                Ok(back_to("Signed in with AWS", "Looking for SuperCI in your account…", &self.back_url(&self.return_to)))
            }
            ("GET", "/oauth/callback") => {
                let fresh = self.cf_pending.as_ref().is_some_and(|p| safe_eq(q("state").as_bytes(), p.state.as_bytes()));
                if !fresh { return Ok(message(400, "This Cloudflare sign-in has expired", "Start again from the dashboard.")) }
                let pending = self.cf_pending.take().unwrap();
                if !q("error").is_empty() { return Ok(message(400, "Cloudflare sign-in was not completed", &esc(q("error")))) }
                self.cf = Some(cloudflare::exchange(pending, q("code"))?);
                println!("Signed in to Cloudflare.");
                self.cf_accounts.clear();
                self.looked_cf = false;
                Ok(back_to("Signed in with Cloudflare", "Looking for SuperCI in your accounts…", &self.back_url(&self.return_to)))
            }
            // Signing in again after AWS refused a sign-in's last step (its page's button): as the dashboard's Sign in,
            // signed out of AWS first.
            ("GET", "/aws/again") => {
                self.return_to = safe_return(q("next"));
                let (link, pending) = aws::authorize("us-east-1", &format!("http://127.0.0.1:{}/oauth/callback", self.port()));
                self.aws_pending = Some(pending);
                self.aws_asked_ms = now_ms();
                Ok(Response::redirect(&aws::signed_out_first(&link)))
            }
            ("POST", "/aws/signin") => {
                self.return_to = safe_return(&field("next"));
                // AWS's developer-tools sign-in allows only this return path, on any port of this computer (Cloudflare's
                // shares the path; `state` tells them apart).
                let (link, pending) = aws::authorize("us-east-1", &format!("http://127.0.0.1:{}/oauth/callback", self.port()));
                self.aws_pending = Some(pending);
                self.aws_asked_ms = now_ms();
                // Again after AWS's "400 Bad Request": signed out of AWS first (see `aws_stuck`).
                Ok(Response::redirect(&if field("fresh") == "on" { aws::signed_out_first(&link) } else { link }))
            }

            // Sign out (the sidebar's More menu): SuperCI's sign-ins leave this machine, as with `superci logout`.
            ("POST", "/signout") => {
                self.sign_out()?;
                lock(side()).clear();
                self.notice = Some("Signed out. SuperCI's sign-ins are removed from this computer; your runners keep working.".into());
                Ok(Response::redirect("/"))
            }

            // Deploying happens only here, from a Deploy button: connecting to a cloud never deploys anything. It runs in the
            // background; the page shows each step, and the control plane joins the list when it answers.
            ("POST", "/plane/cloudflare") => {
                if self.deploy_running() { return Ok(Response::redirect("/?p=plane")) }
                let account_id = field("account");
                let cf = self.cloudflare()?;
                if self.cf_accounts.is_empty() { self.cf_accounts = cf.accounts()? }
                let account_name = self.cf_accounts.iter().find(|(id, _)| *id == account_id).map(|(_, n)| n.clone()).ok_or("the sign-in cannot reach that account")?;
                // One control plane per account (another account, or another cloud, is where one moves to).
                if self.planes.iter().any(|h| matches!(h, Plane::Cloudflare { account_id: a, .. } if *a == account_id)) {
                    return Ok(message(409, "Your control plane is in this Cloudflare account already", "To move it, set one up in another account or cloud."))
                }
                let (script, label) = ("superci".to_string(), "superci".to_string());
                self.start_deploy("cloudflare", account_name.clone(), &cloudflare::DEPLOY_STEPS, vec![("account", account_id.clone())], move |step| {
                    let deployed = cf.deploy_with(&account_id, &script, &label, false, step)?;
                    step(2);
                    wait_online(&deployed.url);
                    Ok(Plane::Cloudflare { account_id, account_name, script, url: deployed.url, plane_id: deployed.plane_id, label })
                });
                Ok(Response::redirect(if self.planes.is_empty() { "/?p=plane" } else { "/?p=planes" }))
            }
            ("POST", "/plane/aws") => {
                if self.deploy_running() { return Ok(Response::redirect("/?p=plane")) }
                let region = field("region");
                if !REGIONS.contains(&region.as_str()) { return Ok(message(400, "Choose a region from the list", "")) }
                // In the account named (one of those signed in to), else the one in hand.
                if !field("account").is_empty() && !self.hold_account(&field("account")) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {}.", esc(&field("account"))))) }
                let account_id = self.aws.as_ref().ok_or("Sign in with AWS first.")?.account_id.clone();
                // (It is called what the workspace is: another CI in the account called that first gets a name of its own.)
                let named = self.about.name.clone();
                if !named.is_empty() { if let Some(other) = self.name_taken(&Plane::Aws { account_id: account_id.clone(), region: region.clone(), url: String::new(), plane_id: String::new(), label: String::new() }, &named) {
                    return Ok(message(409, "Another CI is called that", &format!("{} in this AWS account is called {}, as this workspace is. Rename this workspace (Settings) first: each CI in an account has a name of its own. Nothing was set up.", esc(&other), esc(&named))))
                } }
                // A region the account is known not to have (refused to it, or without the image jobs run on):
                // said before anything is made there.
                if let Some(look) = self.aws_look() {
                    if let Some((_, aws::Answer::Refused(r))) = look.regions.iter().find(|(name, _)| *name == region) {
                        let allowed = look.allowed_regions();
                        let whose = if r.by == superci_core::aws::By::Organization { "The AWS organization the account is in does not allow it there" } else { "The sign-in is not allowed there" };
                        return Ok(message(409, &format!("AWS account {account_id} cannot use {}", region_short(&region)), &format!("{whose}. {}", if allowed.is_empty() { "It allows none of the regions SuperCI works in.".to_string() } else { format!("It can use: {}.", allowed.iter().map(|r| format!("{} ({r})", region_short(r))).collect::<Vec<_>>().join(", ")) })))
                    }
                    if look.regions_without_image().contains(&region.as_str()) { return Ok(message(409, &format!("No job could start in {}", region_short(&region)), &format!("The image SuperCI's jobs run on is not in {region} yet. Choose another region."))) }
                }
                let label = "superci".to_string();
                // Trying again after a stop reuses what the first try created (same id, same names).
                let again = field("plane");
                let plane_id = if again.len() == 12 && again.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) { again } else { cloudflare::new_plane_id() };
                let creds = self.aws_creds()?;
                self.start_deploy("aws", format!("account {account_id} · {region}"), &aws_plane::DEPLOY_STEPS, vec![("region", region.clone()), ("plane", plane_id.clone())], move |step| {
                    // Before anything is made: the region is the account's to use, and has the image jobs run on.
                    aws::region_ready(&creds, &region)?;
                    let deployed = aws_plane::deploy_with(&creds, &account_id, &region, &plane_id, &label, true, step)?;
                    step(5);
                    wait_online(&deployed.url);
                    Ok(Plane::Aws { account_id, region, url: deployed.url, plane_id, label })
                });
                Ok(Response::redirect(if self.planes.is_empty() { "/?p=plane" } else { "/?p=planes" }))
            }
            // The image jobs run on, copied into a region of the account in hand that lacks it (see `aws::copy_image`).
            ("POST", "/aws/image") => {
                let region = field("region");
                if !REGIONS.contains(&region.as_str()) { return Ok(message(400, "Choose a region from the list", "")) }
                if !field("account").is_empty() && !self.hold_account(&field("account")) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {}.", esc(&field("account"))))) }
                let account = self.aws.as_ref().ok_or("Sign in with AWS first.")?.account_id.clone();
                let creds = self.aws_creds()?;
                // (One copy is enough: with one there or on its way, nothing more is asked for. But for a newer
                // build, asked for where the copy is of an older one: copied in beside it, which then goes.)
                let copies = aws::copied_images(&creds, &region)?;
                let newer = field("newer") == "1" && !copies.iter().any(|c| c.state == "pending") && aws::newest_build().is_some_and(|newest| aws::behind(&copies, &newest).is_some());
                if copies.is_empty() || newer {
                    let source = aws::copy_source_for(&region, None)?;
                    aws::copy_image_into(&creds, &region, &source)?;
                }
                if field("newer") == "1" {
                    if let Some(b) = self.aws_behind.get_mut(&account).and_then(|b| b.iter_mut().find(|b| b.0 == region)) { b.3 = true }
                    if let Some((at, _)) = self.aws_looks.get_mut(&account) { *at = 0 }
                    return Ok(Response::redirect("/?p=planes#aws-account"))
                }
                let copying = self.aws_copying.entry(account.clone()).or_default();
                if !copying.contains(&region) { copying.push(region) }
                // (Looked at again at the next page: when the copy is there, the region can be set up in.)
                if let Some((at, _)) = self.aws_looks.get_mut(&account) { *at = 0 }
                // (Back to the page it was asked from: the set-up page, or the account's section.)
                Ok(Response::redirect(&if field("next").is_empty() { "/?p=plane".to_string() } else { format!("/{}#aws-account", safe_return(&field("next"))) }))
            }
            // Refresh, in an AWS account's section: it is looked at again now (beside the page it is on).
            ("POST", "/aws/look") => {
                if let Some(account) = self.aws.as_ref().map(|a| a.account_id.clone()) { if let Some((at, _)) = self.aws_looks.get_mut(&account) { *at = 1 } }
                Ok(Response::redirect(&format!("/{}", safe_return(&field("next")))))
            }
            // A deploy that stopped is let go of: back to choosing where. What it had made in AWS is removed (its
            // role at the least: nothing is left behind in a place that will not be used); said if it could not be.
            ("POST", "/plane/start-over") => {
                let stopped = { let mut deploying = lock(&self.deploying); if deploying.as_ref().is_some_and(|d| matches!(d.result, Some(Err(_)))) { deploying.take() } else { None } };
                if let Some(d) = stopped.filter(|d| d.cloud == "aws") {
                    let value = |name: &str| d.form.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).unwrap_or_default();
                    let (region, plane, account) = (value("region"), value("plane"), d.place.strip_prefix("account ").and_then(|p| p.split(' ').next()).unwrap_or_default().to_string());
                    // (Its form always has both: an AWS deploy is started with its region and its id.)
                    if self.hold_account(&account) {
                        if let Ok(creds) = self.aws_creds() {
                            let left = aws_plane::abandon(&creds, &region, &plane);
                            println!("POST /plane/start-over → what the stopped deploy made in AWS account {account} is removed{}", if left.is_empty() { String::new() } else { format!(", but for {}", left.join("; ")) });
                            if !left.is_empty() { return Ok(message(200, "Something of the stopped deploy is still in AWS", &esc(&format!("Left in account {account}: {}. It costs nothing; setting up there again reuses it.", left.join("; "))))) }
                        }
                    }
                }
                Ok(Response::redirect("/?p=plane"))
            }
            // Moving to another control plane: the one in use hands over its settings (with a one-time token set
            // through its cloud's API), the new one takes them and points GitHub's App and GitLab's webhooks at
            // itself, and the old one says where it went.
            // Moving to another control plane, in the background (its steps show in its row): the one in use hands over
            // its settings and history (with a one-time token set through its cloud's API); the new one gets
            // the runner providers too; then the old one passes on whatever still reaches it, and the new one points
            // GitHub's App and GitLab's webhooks at itself. Until that switch nothing changes for jobs; after it, none is
            // lost; nothing is removed from the old one.
            ("POST", "/plane/move") => {
                if lock(&self.moving).as_ref().is_some_and(|m| m.result.is_none()) { return Ok(Response::redirect("/?p=planes")) }
                let to = self.planes.iter().find(|p| p.plane_id() == field("plane")).cloned().ok_or("That control plane is not known here")?;
                let key = self.status_key.clone();
                // A switch that did not finish (the old one passes jobs on meanwhile): finish it.
                if let Some(old) = self.planes.iter().find(|p| view::plane_view(p, None).moved_to.as_deref() == Some(to.url())).cloned() {
                    if view::plane_view(&to, None).standby {
                        cloudflare::plane_post(to.url(), &key, "/move/claim", &serde_json::json!({ "from": old.url() }))
                            .map_err(|e| format!("The new control plane could not take GitHub over yet ({e}). Jobs still reach it through the old one; try again in a minute."))?;
                        if let Some(m) = lock(&self.moving).as_mut() { m.result = Some(Ok(vec![])); m.ended_ms = now_ms() }
                        return Ok(Response::redirect("/?p=planes"))
                    }
                }
                let from = self.plane()?;
                if to.plane_id() == from.plane_id() { return Ok(Response::redirect("/?p=planes")) }
                let (from_view, to_view) = (view::plane_view(&from, Some(&key)), view::plane_view(&to, Some(&key)));
                // Only to a control plane that serves nothing yet, or back to the one this one moved from: a move
                // replaces the target's GitHub App and settings, and one that has its own is another CI.
                if !to_view.takes_a_move_from(from.url()) {
                    return Ok(message(409, "That control plane serves a code host of its own", "A move would replace its GitHub App and its settings. Move to one that serves nothing yet; to bring an organization there, add it on its Repositories page. Nothing has changed."))
                }
                let plan = self.carry_plan(&from_view, &to_view);
                if let Some(Carry::NeedsSignIn(p)) = plan.iter().find(|c| matches!(c, Carry::NeedsSignIn(_))) { return Ok(message(409, &format!("Sign in with {} first", provider_name(p)), "Its runners come along with the move.")) }
                let writer = Writer { cf: if self.cf.is_some() { Some(self.cf_client()?) } else { None }, aws: self.aws.as_mut().map(|s| s.credentials()).transpose()?, modal: self.modal.clone(), own: self.own() };
                let aws_account = self.aws.as_ref().map(|a| a.account_id.clone());
                let cf_account = match &from { Plane::Cloudflare { account_id, .. } => Some(account_id.clone()), _ => self.cf_accounts.first().map(|a| a.0.clone()) };
                let state = self.moving.clone();
                *lock(&state) = Some(Move { to: to.plane_id().to_string(), at: 0, result: None, ended_ms: 0 });
                std::thread::spawn(move || {
                    // (Its state is set before this thread starts, and nothing takes it away.)
                    let step = |n: usize| lock(&state).as_mut().expect("a move under way").at = n;
                    let result = move_plane(&writer, &key, &from, &to, from_view.status.unwrap_or_default(), &plan, cf_account, aws_account, &step);
                    let mut m = lock(&state);
                    let m = m.as_mut().expect("a move under way");
                    (m.result, m.ended_ms) = (Some(result), now_ms());
                });
                Ok(Response::redirect("/?p=planes"))
            }
            // Deleting a control plane not in use (never the one in use; never one a move is under way to).
            ("POST", "/plane/delete") => {
                // (This workspace's, or one found beside it that serves no code host: a place set up and never used.)
                let plane = self.planes.iter().chain(self.found.iter()).find(|p| p.plane_id() == field("plane")).cloned().ok_or("That control plane is not known here")?;
                let key = self.status_key.clone();
                // (One that moved away handed over: it is in use no more, whatever else is left here.)
                if self.plane().is_ok_and(|p| p.plane_id() == plane.plane_id()) && view::plane_view(&plane, None).moved_to.is_none() { return Ok(message(409, "This control plane is in use", "Move to another one first, or stop using SuperCI.")) }
                if lock(&self.moving).as_ref().is_some_and(|m| m.result.is_none() && m.to == plane.plane_id()) { return Ok(message(409, "A move to it is under way", "")) }
                // One that GitHub or GitLab sends jobs to is a CI of its own (another team's, reached with the same
                // sign-ins): never deleted from beside it. One that moved away, or stands by for a move, is not.
                if view::plane_view(&plane, None).serves_its_own() {
                    return Ok(message(409, "This control plane serves a code host", &format!("GitHub or GitLab sends jobs to it. `superci leave --plane={} --confirm` stops it and deletes it. Nothing was deleted.", esc(plane.plane_id()))))
                }
                let status = cloudflare::status(plane.url(), &key).unwrap_or_default();
                let running = status["jobs"].as_array().into_iter().flatten().filter(|j| ["launching", "launched", "running"].contains(&j["state"].as_str().unwrap_or_default())).count();
                if running > 0 { return Ok(message(409, &format!("{running} job{} still running there", if running == 1 { " is" } else { "s are" }), "Delete it when they finish.")) }
                let left = self.delete_plane(&plane, &status)?;
                self.flash = Some(format!("Deleted {}.{}", plane.place(), left.iter().map(|l| format!(" Left: {l}")).collect::<String>()));
                Ok(Response::redirect("/?p=planes"))
            }
            // A CI found in the signed-in clouds, taken as this workspace's: only while it has none (a workspace looks
            // after one CI; another is a workspace of its own). Its key is written into it as for any control plane.
            ("POST", "/plane/join") => match self.join(&field("plane")) {
                Ok(said) => { self.flash = Some(said); Ok(Response::redirect("/?p=planes")) }
                Err((status, title)) => Ok(message(status, &title, "Nothing has changed.")),
            },
            // Stopping SuperCI: GitHub and GitLab stop sending jobs, then the control plane is deleted with what was
            // made for it, and with it the ones that serve nothing (places it could have moved to, ones it moved
            // from). A control plane that serves a code host of its own is another CI: left as it is. The GitHub App
            // itself is deleted on GitHub (it has no way to delete itself).
            ("POST", "/plane/leave") => {
                if field("confirm").trim() != "superci" { return Ok(message(400, "Type superci to confirm", "")) }
                // The one in use, or the one named (another CI kept on this computer, stopped by itself).
                let used = match field("plane") {
                    named if named.is_empty() => self.plane()?,
                    named => self.planes.iter().find(|p| p.plane_id() == named).cloned().ok_or("That control plane is not known here")?,
                };
                // Before GitHub is let go of: what follows must be able to delete it.
                self.own().check(&used)?;
                let key = self.status_key.clone();
                let others: Vec<PlaneView> = self.planes.iter().filter(|p| p.plane_id() != used.plane_id()).map(|p| view::plane_view(p, None)).collect();
                let another_ci = others.iter().any(|o| o.serves_its_own());
                let status = cloudflare::status(used.url(), &key).unwrap_or_default();
                let app = status["app"].clone();
                cloudflare::plane_post(used.url(), &key, "/leave", &serde_json::json!({})).map_err(|e| format!("GitHub and GitLab could not be let go of yet ({e}); nothing was deleted."))?;
                let mut left = vec![];
                match self.delete_plane(&used, &status) { Ok(l) => left.extend(l), Err(e) => left.push(format!("{}: {e}", used.place())) }
                for o in others {
                    match o.goes_with(&used, another_ci) {
                        Ok(false) => {}
                        Err(why) => left.push(why),
                        Ok(true) => {
                            let st = cloudflare::status(o.plane.url(), &key).unwrap_or_default();
                            match self.delete_plane(&o.plane, &st) { Ok(l) => left.extend(l), Err(e) => left.push(format!("{}: {e}", o.plane.place())) }
                        }
                    }
                }
                self.views = None;
                let app_link = match (app["slug"].as_str(), app["owner"].as_str()) {
                    (Some(slug), Some(owner)) => format!(r#"<li>Delete the GitHub App <a href="{}" target="_blank" rel="noopener">{}</a> on GitHub (Advanced → Delete). It is uninstalled already.</li>"#,
                        esc(&format!("{}/advanced", app_settings_link(app["host"].as_str(), owner, slug, app["org"] == true))), esc(slug)),
                    _ => String::new(),
                };
                let more = left.iter().map(|l| format!("<li>{}</li>", esc(l))).collect::<String>();
                let (heading, said) = if another_ci { ("That control plane is stopped", format!("{} and what was made for it are deleted.", esc(&used.place()))) }
                    else { ("SuperCI is stopped", "Its control planes and what was made for them are deleted from your clouds.".to_string()) };
                // Workflows change back only where it served any: all of them when SuperCI is stopped, those of its
                // own repositories when another control plane goes on.
                let served = !app.is_null() || !status["gitlab"].is_null() || status["gitlabs"].as_array().is_some_and(|g| !g.is_empty());
                let back = if !served { "" }
                    else if another_ci { "<li>Change <code>runs-on</code> back in the workflows of the repositories it served (and <code>tags:</code> in GitLab).</li>" }
                    else { "<li>Change <code>runs-on: superci</code> back in your workflows (and <code>tags:</code> in GitLab).</li>" };
                Ok(message(200, heading, &format!(r#"{said}</p><ul class="checks" style="margin-top:12px">{app_link}{back}{more}</ul><p>"#)))
            }
            ("POST", "/plane/update") => {
                // Which: its id (or, from before, its place in the list); none: the one in use.
                let which = field("plane");
                // Updating another one does not make it the one that later pages and commands act on.
                let was = self.selected;
                if let Some(i) = self.planes.iter().position(|p| p.plane_id() == which).or_else(|| which.parse::<usize>().ok().filter(|i| *i < self.planes.len())) { self.select(i) }
                let plane = self.plane();
                self.selected = was;
                let plane = plane?;
                // Its cloud's sign-in ended (AWS ends one after 12 hours): said, with what to do.
                if !self.signed_in_at(&plane) && !matches!(plane, Plane::Seen { .. }) {
                    let name = provider_name(plane.cloud());
                    return Ok(message(403, &format!("Sign in with {name} to update it"), &format!("Updating changes the control plane in {name}: sign in there (the sign-in kept here has ended), then press Update again.")))
                }
                self.own().check(&plane)?;
                // What it runs now: whether its jobs' containers come along, and whether any would be cut short.
                let Some(status) = status_soon(plane.url(), &self.status_key) else {
                    // (One that answers and does not take this computer's key: why, and what to do.)
                    if let Some(why) = self.key_refused(&view::plane_view(&plane, Some(&self.status_key))) { return Ok(message(403, "The control plane does not take this computer's key", &esc(&why))) }
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let running = status["jobs"].as_array().into_iter().flatten().filter(|j| j["cloud"] == "cloudflare" && ["launching", "launched", "running"].contains(&j["state"].as_str().unwrap_or_default())).count();
                if running > 0 {
                    return Ok(message(409, &format!("{running} job{} running on Cloudflare", if running == 1 { " is" } else { "s are" }), "Update when they finish: the update gives Cloudflare's containers a new setup, which would stop them."))
                }
                if lock(&self.updating).as_ref().is_some_and(|u| u.result.is_none()) { return Ok(Response::redirect(&format!("/{}", safe_return(&field("next"))))) }
                // One from before `NAMES_SINCE` says its launchers as `agents`, and keeps them and its devices' keys under
                // the old names: the update moves them (see `renames`).
                let before = before_names(plane.url());
                let launchers = if before { status["agents"].clone() } else { status["launchers"].clone() };
                // In the background, its steps shown where its Update button was (the sidebar, the Control plane page).
                let cf = if matches!(plane, Plane::Cloudflare { .. }) || launchers.as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")) { self.cloudflare().ok() } else { None };
                // What deploys it, with its cloud's sign-in (a Modal control plane's is there: checked above).
                let (steps, deploying): (&'static [&'static str], Deploying) = match (plane.clone(), self.modal.clone()) {
                    (Plane::Cloudflare { account_id, script, label, .. }, _) => (&UPDATE_CLOUDFLARE, Deploying::Cloudflare { account_id, script, label }),
                    (Plane::Aws { account_id, region, plane_id, label, .. }, _) => (&UPDATE_AWS, Deploying::Aws { creds: self.aws_creds()?, account_id, region, plane_id, label }),
                    (Plane::Modal { plane_id, label, .. }, Some(session)) => (&UPDATE_MODAL, Deploying::Modal { session, plane_id, label }),
                    _ => return Ok(message(400, "Sign in where this control plane runs to update it", "")),
                };
                let cf_account = self.cf_accounts.first().map(|a| a.0.clone());
                let state = self.updating.clone();
                let containers = status["containers"] == true;
                let cf_launcher = launchers.as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare"));
                // Its launcher in Cloudflare is updated with it, when Cloudflare is signed in to: a sign-in
                // that is over stops nothing else, and is said with the result.
                let note = if cf_launcher && !matches!(plane, Plane::Cloudflare { .. }) && cf.is_none() { format!("Its Cloudflare launcher was not updated: {}", if self.cf_ended { "the Cloudflare sign-in has expired. Sign in again (`superci login cloudflare`) and update once more." } else { "Cloudflare is not signed in to here. Sign in (`superci login cloudflare`) and update once more." }) } else { String::new() };
                *lock(&state) = Some(Update { plane: plane.plane_id().to_string(), steps, at: 0, result: None, ended_ms: 0, note });
                // Signed in to the AWS account its machines run in: the update also gives its runner role what this
                // version asks for (an AWS control plane's own update does that already).
                if let (Some(account), false) = (status["aws"]["account_id"].as_str(), matches!(plane, Plane::Aws { .. })) { self.hold_account(account); }
                let aws_give = match (&self.aws, status["aws"]["account_id"].as_str()) {
                    (Some(a), Some(account)) if a.account_id == account && !matches!(plane, Plane::Aws { .. }) && status["aws"]["connected"] == true => Some((self.aws_creds()?, account.to_string())),
                    _ => None,
                };
                let regions = status_regions(&status);
                // An AWS control plane whose AWS runners were removed keeps them off.
                let aws_runners = !status["aws"].is_null();
                let (aws_missing, key) = (self.aws_missing.clone(), self.status_key.clone());
                let writer = Writer { cf: if self.cf.is_some() { self.cf_client().ok() } else { None }, aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone(), own: self.own() };
                let (puts, drops) = if before {
                    // (Modal's settings are not listed: its old names are taken out by name.)
                    let names = if matches!(plane, Plane::Modal { .. }) { vec!["AGENTS".to_string(), old_key_name(&self.status_secret)] } else { writer.names(&plane) };
                    renames(&status, &names)
                } else { (vec![], vec![]) };
                std::thread::spawn(move || {
                    // (Its state is set before this thread starts, and nothing takes it away.)
                    let step = |n: usize| lock(&state).as_mut().expect("an update under way").at = n;
                    let result = (|| -> Result<()> {
                        for (name, value) in &puts { writer.put(&plane, name, value)? }
                        let url = match &deploying {
                            Deploying::Cloudflare { account_id, script, label } => cf.as_ref().ok_or("Sign in with Cloudflare first.")?.deploy_with(account_id, script, label, containers, &step)?.url,
                            Deploying::Aws { creds, account_id, region, plane_id, label } => aws_plane::deploy_with(creds, account_id, region, plane_id, label, aws_runners, &step)?.url,
                            Deploying::Modal { session, plane_id, label } => modal::deploy_plane(session, plane_id, label, &step)?,
                        };
                        // A separate Cloudflare launcher (for a control plane elsewhere) is updated with it.
                        if let (true, Some(cf), Some(account)) = (cf_launcher && !matches!(plane, Plane::Cloudflare { .. }), &cf, &cf_account) { cf.deploy_runners(account, plane.url(), plane.plane_id())?; }
                        // An AWS control plane's machines get their own network in each of its regions too.
                        if let (Deploying::Aws { creds, .. }, true) = (&deploying, aws_runners) { aws::make_networks(creds, plane.plane_id(), &regions)?; }
                        if let Some((creds, account)) = &aws_give {
                            aws::give_runner_role(creds, account, plane.plane_id(), &regions)?;
                            let _ = cloudflare::plane_post(&url, &key, "/permissions/given", &serde_json::json!({ "cloud": "aws" }));
                            lock(&aws_missing).insert(plane.plane_id().to_string(), (Ok(vec![]), now_ms()));
                        }
                        // The old version keeps answering for a few seconds after a deploy: wait for this one.
                        step(steps.len() - 1);
                        wait_for_version(&url, DASHBOARD_VERSION);
                        // The old names, once the new version runs (it reads only the new ones).
                        for name in &drops { let _ = writer.drop(&plane, name); }
                        Ok(())
                    })();
                    if result.is_ok() { lock(side()).clear() }
                    let mut u = lock(&state);
                    let u = u.as_mut().expect("an update under way");
                    (u.result, u.ended_ms) = (Some(result), now_ms());
                });
                let next = safe_return(&field("next"));
                Ok(Response::redirect(&format!("/{}", if next.is_empty() { "?p=planes" } else { &next })))
            }
            ("POST", "/github/start") => {
                let plane = self.plane()?;
                let login = field("login");
                if !github::valid_login(&login) { return Ok(message(400, "That is not a GitHub name", "")) }
                // GitHub elsewhere (a GitHub Enterprise Server, or GitHub Enterprise Cloud with data residency).
                let host = match github::host_of(&field("host")) { Ok(h) => h, Err(e) => return Ok(message(400, "Check the address of your GitHub", &esc(&e))) };
                let owner = match github_owner(host.as_deref(), &login) { Ok(o) => o, Err(e) => return Ok(message(400, "Check the name", &esc(&e))) };
                // A further organization: only a control plane that knows several (an older one would not see its App).
                let seen = view::plane_view(&plane, Some(&self.status_key));
                if host.is_some() && view::older(seen.version.as_deref(), "0.9.34") {
                    return Ok(message(409, "Update the control plane first", "GitHub Enterprise needs control plane 0.9.34 or newer (Control plane → Update)."))
                }
                if seen.github && view::older(seen.version.as_deref(), "0.9.31") && !seen.app_owner().is_some_and(|o| o.eq_ignore_ascii_case(&owner.login)) {
                    return Ok(message(409, "Update the control plane first", "Adding another organization needs control plane 0.9.31 or newer (Control plane → Update)."))
                }
                let state = random_token(18);
                let mut manifest = app_manifest(plane.url(), &owner, &seen.name(), &seen.label());
                // GitHub comes back to this machine, not to the control plane: the code is exchanged here.
                manifest["redirect_url"] = format!("{}/github/callback", self.base).into();
                manifest["setup_url"] = format!("{}/github/installed", self.base).into();
                let target = manifest_target(host.as_deref(), &owner, &state);
                self.manifest_state = Some((state, owner, host));
                self.github_for = Some(plane.plane_id().to_string());
                Ok(manifest_form(&target, &manifest))
            }
            ("GET", "/github/callback") => {
                let fresh = self.manifest_state.as_ref().is_some_and(|(s, ..)| safe_eq(q("state").as_bytes(), s.as_bytes()));
                if !fresh { return Ok(message(400, "This App creation has expired", "Start again from the dashboard.")) }
                let host = self.manifest_state.as_ref().and_then(|(_, _, h)| h.clone());
                let app = convert_manifest_code(&github::api_base(host.as_deref()), host.as_deref(), q("code"))?;
                // For the control plane it was started for, whatever another tab shows meanwhile.
                if let Some(i) = self.github_for.as_ref().and_then(|id| self.planes.iter().position(|p| p.plane_id() == id)) { self.select(i) }
                // The App's key goes straight into the control plane's encrypted secrets; this machine does not keep it.
                let plane = self.plane()?;
                // A further organization (the control plane has an App of another account already): beside the first,
                // as a secret of its own.
                // Never from a guess: a control plane that does not say what it has keeps what it has (an App put
                // in the first one's place would cut its organization off).
                let not_kept = || message(503, "The control plane did not answer", "The App was made on GitHub but not kept: delete it there and try again in a moment.");
                let apps = match status_soon(plane.url(), &self.status_key) {
                    Some(s) => status_apps(&s),
                    None => match cloudflare::health(plane.url()) { Some(h) if h["github"] == false => vec![], _ => return Ok(not_kept()) },
                };
                // The same account as an App here already: in its place (the first, or a further one, whose old
                // secret goes). Another account: beside them.
                let same = apps.iter().position(|a| a["owner"].as_str().is_some_and(|o| o.eq_ignore_ascii_case(&app.owner)) && a["host"].as_str() == app.host.as_deref());
                let name = match same { Some(0) => "GITHUB_APP".to_string(), None if apps.is_empty() => "GITHUB_APP".to_string(), _ => format!("GITHUB_APP_{}", app.id) };
                self.put_secret(&plane, &name, &json_text(&app))?;
                if let Some(old) = same.filter(|n| *n > 0).and_then(|n| apps[n]["id"].as_u64()).filter(|old| *old != app.id) { let _ = self.drop_secret(&plane, &format!("GITHUB_APP_{old}")); }
                self.views = None;
                self.manifest_state = None;
                self.github_expected = Some(now_ms() + 600_000);
                Ok(Response::redirect(&github::install_link(app.host.as_deref(), &app.slug)))
            }
            // Back from installing the App on GitHub: what was read before is from before it.
            ("GET", "/github/installed") => {
                if let Some(Task::GitHub { done, .. }) = self.task.as_mut() { *done = true }
                self.views = None;
                self.github_expected.get_or_insert(now_ms() + 120_000);
                // Back to that control plane's pages (with several control planes, a page says which one it shows).
                match self.github_for.take().filter(|_| self.planes.len() > 1) {
                    Some(id) => Ok(back_to("Installed on GitHub", "Back to your dashboard…", &self.back_url(&format!("?p=repos&c={id}")))),
                    None => Ok(back("Installed on GitHub", "Back to your dashboard…")),
                }
            }
            // An organization removed (not the first one): its App is uninstalled there, then forgotten here. Its jobs
            // then wait for runners that never come: workflows there go back to GitHub's runners first.
            ("POST", "/github/remove") => {
                let plane = self.plane()?;
                let id: u64 = field("app").parse().map_err(|_| "Which organization?")?;
                let status = status_soon(plane.url(), &self.status_key).ok_or("The control plane did not answer: try again in a few seconds")?;
                let app = status["apps"].as_array().into_iter().flatten().skip(1).find(|a| a["id"].as_u64() == Some(id)).cloned().ok_or("That organization is not one added here")?;
                // Uninstalled where it can be (an App deleted on GitHub already cannot); forgotten here either way.
                let uninstalled = cloudflare::plane_post(plane.url(), &self.status_key, "/github/uninstall", &serde_json::json!({ "app": id }));
                let failed = match &uninstalled { Ok(v) => v["error"].as_str().map(str::to_string), Err(e) => Some(e.clone()) };
                self.drop_secret(&plane, &format!("GITHUB_APP_{id}"))?;
                wait_for(plane.url(), &self.status_key, move |s| !s["apps"].as_array().is_some_and(|a| a.iter().any(|a| a["id"].as_u64() == Some(id))));
                let (owner, slug) = (app["owner"].as_str().unwrap_or_default(), app["slug"].as_str().unwrap_or_default());
                let link = app_settings_link(app["host"].as_str(), owner, slug, app["org"] != false);
                self.flash = Some(match failed {
                    None => format!("Removed {owner}. Its App {slug} is uninstalled; delete the App itself on GitHub ({link})."),
                    Some(e) => format!("Removed {owner} from this control plane. GitHub would not uninstall its App {slug} ({}): uninstall and delete it on GitHub ({link}).", e.chars().take(120).collect::<String>()),
                });
                self.views = None;
                Ok(Response::redirect("/?p=repos"))
            }

            ("POST", "/aws/connect") => {
                let plane = self.plane()?;
                let region = field("region");
                if !REGIONS.contains(&region.as_str()) { return Ok(message(400, "Choose a region from the list", "")) }
                let session = self.aws.as_mut().ok_or("Sign in with AWS first")?;
                let creds = session.credentials()?;
                let account_id = session.account_id.clone();
                let role_arn = aws::connect_runners(&creds, &account_id, plane.url(), plane.plane_id(), AUDIENCE)?;
                // The control plane accepts the connection with a one-time token it holds.
                let token = random_token(24);
                self.put_secret(&plane, "AWS_CONNECT", &serde_json::json!({ "region": region, "token": token }).to_string())?;
                // Its nearest regions after it, for when it runs out of spot capacity (changed in AWS's settings).
                let regions: Vec<&str> = std::iter::once(region.as_str()).chain(nearby_regions(&region).iter().copied()).collect();
                // Its machines' own network in each (before it starts any; without, they would use the default one).
                aws::make_networks(&creds, plane.plane_id(), &regions.iter().map(|r| r.to_string()).collect::<Vec<_>>())?;
                self.put_secret(&plane, "AWS_REGIONS", &json_text(&regions))?;
                report_to_plane(plane.url(), &serde_json::json!({ "token": token, "accountId": account_id, "region": region, "roleArn": role_arn }))?;
                Ok(Response::redirect("/"))
            }
            ("POST", "/modal/signin") => {
                self.return_to = safe_return(&field("next"));
                let pending = modal::authorize(&format!("{}/oauth/modal", self.base))?;
                let link = pending.web_url.clone();
                self.modal_pending = Some(pending);
                Ok(Response::redirect(&link))
            }
            ("GET", "/oauth/modal") => Ok(back_to("Signed in with Modal", "Back to where you were…", &self.back_url(&self.return_to))),
            // `account`: a Modal workspace signed in to beside the first (`superci login modal --add`), whose launcher
            // is a place of its own in the order (`modal:<workspace>`); none, the first's (`modal`).
            ("POST", "/runners/modal") => {
                let plane = self.plane()?;
                let workspace = field("account");
                let session = if workspace.is_empty() { self.modal.clone().ok_or("Sign in with Modal first.")? }
                    else { self.modal_for(&format!("modal:{workspace}")).cloned().ok_or_else(|| format!("Sign in with Modal to the workspace {workspace} first: `superci login modal --add`."))? };
                let cloud = if self.modal.as_ref().is_some_and(|m| m.workspace == session.workspace) { "modal".to_string() } else { format!("modal:{}", session.workspace) };
                let Some(mut launchers) = status_soon(plane.url(), &self.status_key).and_then(|s| serde_json::from_value::<Vec<superci_core::plane::Launcher>>(s["launchers"].clone()).ok()) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let keys = plane_keys(plane.url())?;
                eprintln!("Deploying the Modal launcher (the first time builds a small image in your workspace)…");
                let url = modal::deploy_runners(&session, plane.url(), plane.plane_id(), &keys)?;
                launcher_up(&url);
                // (In Function mode: its Functions too, for the images there are now.)
                if let Some(r) = status_soon(plane.url(), &self.status_key).and_then(|s| serde_json::from_value::<Routing>(s["routing"].clone()).ok()).filter(|r| r.pool(&cloud).is_some_and(|p| p.function)) { self.deploy_jobs(&plane, &cloud, &r.images)? }
                launchers.retain(|a| a.cloud != cloud);
                launchers.push(superci_core::plane::Launcher { cloud: cloud.clone(), url });
                self.put_secret(&plane, "LAUNCHERS", &json_text(&launchers))?;
                wait_for(plane.url(), &self.status_key, move |s| s["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == cloud.as_str())));
                Ok(Response::redirect("/?p=runners"))
            }
            ("POST", "/runners/cloudflare") => {
                let plane = self.plane()?;
                let Some(status) = status_soon(plane.url(), &self.status_key) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let cf = self.cloudflare()?;
                match &plane {
                    // A Cloudflare control plane starts the containers itself: its own Worker gets the runner image and
                    // the container application (each container sized per job), then containers are switched on.
                    Plane::Cloudflare { account_id, script, label, .. } => {
                        cf.deploy(account_id, script, label, true)?;
                        self.put_secret(&plane, "CONTAINERS", "on")?;
                        // A separate Cloudflare launcher from before is no longer needed.
                        let mut launchers: Vec<superci_core::plane::Launcher> = serde_json::from_value(status["launchers"].clone()).unwrap_or_default();
                        if launchers.iter().any(|a| a.cloud == "cloudflare") {
                            launchers.retain(|a| a.cloud != "cloudflare");
                            self.put_secret(&plane, "LAUNCHERS", &json_text(&launchers))?;
                            let _ = cf.delete_runners(account_id, plane.plane_id());
                        }
                        wait_for(plane.url(), &self.status_key, |s| s["containers"] == true && !s["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")));
                    }
                    // A control plane elsewhere cannot start Cloudflare containers: a small launcher in Cloudflare does.
                    _ => {
                        let mut launchers: Vec<superci_core::plane::Launcher> = serde_json::from_value(status["launchers"].clone()).unwrap_or_default();
                        // Which Cloudflare account its runners go into: the only one the sign-in reaches, or the one
                        // named. Never just the first of several (it may be another company's).
                        let account = match (self.cf_accounts.as_slice(), field("account")) {
                            ([], _) => return Err("no Cloudflare account".into()),
                            (all, named) if !named.is_empty() => all.iter().find(|a| a.0 == named).map(|a| a.0.clone()).ok_or("The Cloudflare sign-in does not reach that account.")?,
                            ([only], _) => only.0.clone(),
                            (all, _) => return Ok(message(409, "Which Cloudflare account?", &format!("Your Cloudflare sign-in reaches {}: {}. Say which one the runners go into: `superci runners create cloudflare --account=ID`. Nothing has changed.",
                                all.len(), esc(&all.iter().map(|a| format!("{} ({})", a.1, a.0)).collect::<Vec<_>>().join(", "))))),
                        };
                        let url = cf.deploy_runners(&account, plane.url(), plane.plane_id())?;
                        launchers.retain(|a| a.cloud != "cloudflare");
                        launchers.push(superci_core::plane::Launcher { cloud: "cloudflare".into(), url });
                        self.put_secret(&plane, "LAUNCHERS", &json_text(&launchers))?;
                        wait_for(plane.url(), &self.status_key, |s| s["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")));
                    }
                }
                Ok(Response::redirect("/?p=runners"))
            }
            // The labels workflows name: a list, each answered to until removed (so no workflow has to change the day
            // one is added). One added goes first: the one shown in examples.
            ("POST", "/labels") => {
                let plane = self.plane()?;
                let seen = view::plane_view(&plane, Some(&self.status_key));
                // (Its version as it answers, before anything else of it is read: one that did not answer is not
                // taken for an old one.)
                if seen.version.is_some() && view::older(seen.version.as_deref(), "0.12.0") { return Ok(message(409, "Update the control plane first", "Labels can be added from control plane 0.12.0 on (Control plane → Update).")) }
                let Some(status) = seen.status.clone().or_else(|| status_soon(plane.url(), &self.status_key)) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let mut labels: Vec<String> = status["labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
                if labels.is_empty() { labels.push(plane.label().to_string()) }
                let label = field("label").to_ascii_lowercase();
                if field("action") == "remove" {
                    if !labels.contains(&label) { return Ok(message(400, "It does not answer to that label", "")) }
                    if labels.len() == 1 { return Ok(message(409, "This is its only label", "Add another first: a control plane answers to at least one.")) }
                    labels.retain(|l| *l != label);
                } else {
                    if label != "superci" { if let Err(e) = superci_core::spec::valid_label(&label) { return Ok(message(400, "That cannot be a label", &esc(&e))) } }
                    labels.retain(|l| *l != label);
                    labels.insert(0, label);
                }
                let want = json_value(&labels);
                self.put_secret(&plane, "LABELS", &want.to_string())?;
                self.planes[self.selected].set_label(&labels[0]);
                self.shown(&plane, |s| s["labels"] == want)?;
                Ok(Response::redirect("/?p=settings"))
            }
            // What it is called where people read it: the dashboard's header, the App's name for further
            // organizations, what it says on a job it could not run. SuperCI again by naming it that.
            // A key taken out of the CI: another computer's (its dashboard stops reaching it), or one that only reads.
            ("POST", "/secrets/add") | ("POST", "/secrets/remove") => {
                let scope = match superci_core::secrets::valid_scope(Some(&field("repo")), Some(&field("branch")), Some(&field("workflow")), Some(&field("environment"))) { Ok(s) => s, Err(e) => return Ok(message(400, "That cannot be its scope", &esc(&e))) };
                let add = req.path == "/secrets/add";
                let done = if add { self.keep_secret(&field("name"), &scope, &field("value"), true) } else { self.forget_secret(&field("name"), &scope) };
                match done {
                    Ok(_) => Ok(Response::redirect("/?p=settings")),
                    Err(e) => Ok(message(409, if add { "That secret was not added" } else { "That secret was not taken out" }, &format!("{} Nothing has changed.", esc(&e)))),
                }
            }
            ("POST", "/keys/remove") => {
                if !field("read").is_empty() { self.revoke_key(&field("read"))?; return Ok(Response::redirect("/?p=settings")) }
                match self.drop_device_key(&field("name")) {
                    Ok(()) => Ok(Response::redirect("/?p=settings")),
                    Err(e) => Ok(message(409, "That key was not taken out", &format!("{} Nothing has changed.", esc(&e)))),
                }
            }
            ("POST", "/name") => {
                let plane = self.plane()?;
                if view::older(view::plane_view(&plane, Some(&self.status_key)).version.as_deref(), "0.12.0") { return Ok(message(409, "Update the control plane first", "It can be named from control plane 0.12.0 on (Control plane → Update).")) }
                let name = field("name").trim().to_string();
                if let Err(e) = superci_core::spec::valid_name(&name) { return Ok(message(400, "That cannot be its name", &esc(&e))) }
                if let Some(other) = self.name_taken(&plane, &name) { return Ok(message(409, "Another CI is called that", &format!("{} in the same AWS account is called {}: each CI there has a name of its own, so a workspace picking among them can tell them apart. Nothing was changed.", esc(&other), esc(&name)))) }
                self.put_secret(&plane, "NAME", &name)?;
                self.shown(&plane, |s| s["name"] == name.as_str())?;
                let mut about = self.about.clone();
                about.name.clone_from(&name);
                self.set_about(about);
                Ok(Response::redirect("/?p=settings"))
            }
            // The default machine: what `runs-on: superci` alone gets.
            ("POST", "/machine") => {
                let plane = self.plane()?;
                // Each key a label has, by the same name and the same rules (see `Spec::set`); one left out, or said
                // as `default`, is left to each provider's standard. (The dialog's box for no spot machines too.)
                let mut spec = Spec::default();
                for (key, value) in superci_core::spec::KEYS.iter().map(|k| (*k, field(k))).filter(|(_, v)| !(v.is_empty() || v == "default")) {
                    if let Err(e) = spec.set(key, &value) { return Ok(message(400, "That machine is not understood", &esc(&e))) }
                }
                if field("nospot") == "on" { spec.spot = Some(false) }
                // (What is the standard anyway is not kept.)
                if spec.arch.as_deref() == Some("x64") { spec.arch = None }
                if spec.os.as_deref() == Some("linux") { spec.os = None }
                if spec.spot == Some(true) { spec.spot = None }
                if spec.arch.is_some() && spec.os.as_deref() == Some("windows") { return Ok(message(400, "Windows machines are x64", "Choose x64 for Windows, or Linux for arm64.")) }
                if let Err(e) = spec.check() { return Ok(message(400, "That machine does not go together", &esc(&e))) }
                let want = json_value(&spec);
                self.put_secret(&plane, "MACHINE", &want.to_string())?;
                self.shown(&plane, |s| s["machine"] == want)?;
                // (Back where it is set: Workflows.)
                Ok(Response::redirect("/?p=workflows"))
            }
            // Review → Allow: the control plane's AWS runner role gets what this version asks for.
            ("POST", "/permissions/aws") => {
                let id = field("plane");
                let plane = self.planes.iter().find(|p| p.plane_id() == id).cloned().ok_or("Which control plane?")?;
                let status = status_soon(plane.url(), &self.status_key).ok_or("The control plane did not answer: try again in a few seconds")?;
                let account = match &plane { Plane::Aws { account_id, .. } => account_id.clone(), _ => status["aws"]["account_id"].as_str().unwrap_or_default().to_string() };
                if !self.hold_account(&account) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {} to give its role these permissions.", esc(&account)))) }
                let creds = self.aws_creds()?;
                aws::give_runner_role(&creds, &account, &id, &status_regions(&status))?;
                let _ = cloudflare::plane_post(plane.url(), &self.status_key, "/permissions/given", &serde_json::json!({ "cloud": "aws" }));
                lock(&self.aws_missing).insert(id, (Ok(vec![]), now_ms()));
                self.views = None;
                Ok(Response::redirect("/?p=planes#permissions"))
            }
            ("POST", "/routing") => {
                let plane = self.plane()?;
                // Changed from what the control plane has now; never from a guess (that would drop its other rules).
                let status = status_soon(plane.url(), &self.status_key);
                let Some(mut routing) = status.as_ref().and_then(|s| serde_json::from_value::<Routing>(s["routing"].clone()).ok()) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                // A dollar amount, or none (empty, `none`, zero).
                let money = |name: &str| field(name).parse::<f64>().ok().filter(|n| *n > 0.0 && n.is_finite());
                let usd = || money("usd");
                let limits = ["monthly_usd", "account_usd"].contains(&field("action").as_str());
                if limits && !status.as_ref().is_some_and(|s| s["account_limits"] == true) {
                    return Ok(message(409, "Update the control plane first", "A limit for the whole CI or for a cloud account needs a newer control plane: update it on the Control plane page (or `superci planes update`), then set the limit."))
                }
                // (A limit for a day, and a budget in AWS itself, need a control plane that keeps them.)
                let newer = status.as_ref().is_some_and(|s| s["daily_limits"] == true);
                if limits && !newer && (money("daily").is_some() || !["", "none"].contains(&field("budget").as_str())) {
                    return Ok(message(409, "Update the control plane first", "A limit for a day, and a budget in AWS itself, need a newer control plane: update it on the Control plane page (or `superci planes update`), then set them."))
                }
                match field("action").as_str() {
                    // The whole CI's limit for a month, and a cloud account's (its providers together).
                    "monthly_usd" => { routing.monthly_usd = usd(); if newer { routing.daily_usd = money("daily") } }
                    "account_usd" => {
                        let account = field("account");
                        if account != "aws" { return Ok(message(400, "Which account?", "An account's limit is for a cloud with several providers: aws (its spot and on-demand machines). Another provider's limit is its own: its settings on Runners.")) }
                        let (was, limit) = (routing.account_usd.get(&account).copied(), usd());
                        match limit { Some(n) => { routing.account_usd.insert(account.clone(), n); } None => { routing.account_usd.remove(&account); } }
                        if newer { match money("daily") { Some(n) => { routing.account_daily_usd.insert(account.clone(), n); } None => { routing.account_daily_usd.remove(&account); } } }
                        // The account's own budget in AWS, for the same monthly limit: nothing, AWS's emails, or its
                        // emails and the rule that stops new machines at the limit (`lift` takes that rule off the
                        // role). Not said: as it is, kept in step with the limit. It is made, changed and removed
                        // with the sign-in of whoever asks; the control plane is only told what there is.
                        let had = routing.aws_budget.clone();
                        let (asked, lift) = (field("budget"), field("budget") == "lift");
                        let want = match asked.as_str() { "emails" => Some(AwsBudget { stop: false }), "stops" => Some(AwsBudget { stop: true }), "none" => None, "" | "lift" => had.clone(),
                            _ => return Ok(message(400, "What AWS does at the limit is none, emails or stops", "")) };
                        if lift && had.is_none() { return Ok(message(400, "There is no budget in AWS to lift", "")) }
                        if want.is_some() && limit.is_none() { return Ok(message(400, "A budget in AWS needs the monthly limit", "AWS's budget is for the account's monthly limit. Set a limit, or choose nothing in AWS.")) }
                        if want != had || lift || (want.is_some() && limit != was) {
                            let id = match &plane { Plane::Aws { account_id, .. } => account_id.clone(), _ => status.as_ref().and_then(|s| s["aws"]["account_id"].as_str()).unwrap_or_default().to_string() };
                            if !self.hold_account(&id) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {}: its budget in AWS is made and changed with your sign-in.", esc(&id)))) }
                            let creds = self.aws_creds()?;
                            let email = field("email").trim().to_string();
                            if !email.is_empty() && !(email.contains('@') && !email.contains(char::is_whitespace)) { return Ok(message(400, "That is no email address", "")) }
                            let done = crate::aws_budget::with(&creds, &id, plane.plane_id(), |b| match &want {
                                Some(_) if lift => b.lift(false),
                                Some(w) => b.set(limit.unwrap_or_default(), Some(email.as_str()).filter(|e| !e.is_empty()), w.stop),
                                None => b.remove(),
                            });
                            if let Err(e) = done { return Ok(message(502, "AWS did not take the budget", &esc(&plain_refusal(&e).unwrap_or(e)))) }
                            // (The rule is off the role now, whatever the control plane noted before.)
                            self.budget_lifted.insert(plane.plane_id().to_string(), now_ms());
                        }
                        routing.aws_budget = want;
                    }
                    "default" => routing.default = Some(field("cloud")).filter(|c| !c.is_empty()),
                    "add" => {
                        let repo = field("repo");
                        let ok = repo == "*" || { let mut p = repo.split('/'); matches!((p.next(), p.next(), p.next()), (Some(o), Some(r), None) if github::valid_login(o) && !r.is_empty()) };
                        if !ok { return Ok(message(400, "A repository is owner/name, owner/* or *", "")) }
                        routing.rules.retain(|r| r.repo != repo);
                        routing.rules.push(Rule { repo, cloud: field("cloud") });
                    }
                    "remove" => { let repo = field("repo"); routing.rules.retain(|r| r.repo != repo) }
                    // Limits: the largest machine a label may ask for; public repositories allowed to run here.
                    "max_cpu" => routing.max_cpu = field("max_cpu").parse().ok().filter(|n: &u32| *n >= 1 && *n <= 192 && *n != MAX_CPU),
                    // The longest a job may run, in hours: up to five days (what AWS's machines may live; Modal's
                    // and Cloudflare's end sooner whatever is set).
                    "max_hours" => {
                        let Some(hours) = field("max_hours").parse::<u32>().ok().filter(|h| (1..=120).contains(h)) else { return Ok(message(400, "From 1 to 120 hours", "Five days is the longest GitHub lets a job run on a runner of your own.")) };
                        routing.max_minutes = Some(hours * 60).filter(|m| *m != MAX_JOB_MINUTES);
                    }
                    "public_add" => {
                        let repo = field("repo").trim().to_string();
                        let ok = { let mut p = repo.split('/'); matches!((p.next(), p.next(), p.next()), (Some(o), Some(r), None) if github::valid_login(o) && !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.*".contains(&b))) };
                        if !ok { return Ok(message(400, "A repository is owner/name or owner/*", "")) }
                        if !routing.public_repos.iter().any(|r| r.eq_ignore_ascii_case(&repo)) { routing.public_repos.push(repo) }
                    }
                    "public_remove" => { let repo = field("repo"); routing.public_repos.retain(|r| *r != repo) }
                    // An image of the control plane's own, by the name a label gives it (`image=NAME`), or none by it.
                    // An update changes one there is, in this one write of the routing: a job that names it never
                    // meets a moment with no image by that name. What it does not say (its address, its Modal
                    // Secret) stays as the control plane has it now.
                    "image_add" | "image_update" => {
                        if !status.as_ref().is_some_and(|s| s["runner_images"] == true) { return Ok(message(409, "Update the control plane first", "Images of your own need a newer control plane: update it on the Control plane page (or `superci planes update`), then add one.")) }
                        let name = field("name").trim().to_string();
                        let had = if field("action") == "image_update" {
                            let Some(had) = routing.images.get(&name).cloned() else { return Ok(message(404, "No image by that name", &esc(&no_image(&name)))) };
                            Some(had)
                        } else { None };
                        let given = |n: &str, had: Option<String>| match had { Some(h) if !form_has(n) => h, _ => field(n).trim().to_string() };
                        let reference = given("reference", had.as_ref().map(|i| i.reference.clone()));
                        let secret = given("modal_secret", had.as_ref().map(|i| i.modal_secret.clone().unwrap_or_default()));
                        if let Err(e) = superci_core::spec::valid_image_name(&name) { return Ok(message(400, "Check the image's name", &esc(&e))) }
                        if !superci_core::plane::valid_image_reference(&reference) { return Ok(message(400, "Check the image's address", "As `docker pull` takes it: ghcr.io/acme/ci:2026-10, or by its digest (ghcr.io/acme/ci@sha256:…).")) }
                        if !secret.is_empty() && !superci_core::plane::valid_modal_secret(&secret) { return Ok(message(400, "Check the Modal Secret's name", "Letters, digits, dashes, underscores and dots, as Modal names a Secret.")) }
                        routing.images.insert(name, superci_core::plane::RunnerImage { reference, modal_secret: Some(secret).filter(|s| !s.is_empty()) });
                    }
                    "image_remove" => { routing.images.remove(field("name").trim()); }
                    // The cache: on or off, what a repository keeps, how long an entry may go unused.
                    "cache_on" => routing.cache.off = field("on") != "on",
                    "cache_images" => routing.cache.images_off = field("on") != "on",
                    "cache_size" => {
                        let Some(gb) = field("size_gb").parse::<u32>().ok().filter(|g| (1..=10_000).contains(g)) else { return Ok(message(400, "From 1 to 10,000 GB", "What one repository keeps in the cache at most.")) };
                        routing.cache.size_gb = Some(gb).filter(|g| *g != superci_core::cache::SIZE_GB);
                    }
                    "cache_days" => {
                        let max = superci_core::cache::MAX_AGE_DAYS;
                        let Some(days) = field("days").parse::<u32>().ok().filter(|d| (1..=max).contains(d)) else { return Ok(message(400, &format!("From 1 to {max} days"), &format!("An entry saved {max} days ago is saved anew whatever its use."))) };
                        routing.cache.days = Some(days).filter(|d| *d != superci_core::cache::UNUSED_DAYS);
                    }
                    // The whole set, as chosen in the dialog (each checked repository; patterns like owner/* kept).
                    "public_set" => {
                        let chosen: Vec<String> = form.iter().filter(|(k, _)| k == "repo").map(|(_, v)| v.trim().to_string()).collect();
                        let ok = |repo: &str| { let mut p = repo.split('/'); matches!((p.next(), p.next(), p.next()), (Some(o), Some(r), None) if github::valid_login(o) && !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))) };
                        if chosen.iter().any(|r| !ok(r)) { return Ok(message(400, "A repository is owner/name", "")) }
                        routing.public_repos.retain(|p| p.ends_with("/*"));
                        for r in chosen { if !routing.public_repos.iter().any(|p| p.eq_ignore_ascii_case(&r)) { routing.public_repos.push(r) } }
                    }
                    // A new order (dragged; or the CLI's): each place named in it is in the order, in that order, keeping its
                    // settings; one named as taken out (`out_N`) is taken out; one not named at all stays as it was, after them.
                    "order" => {
                        let mut order = vec![];
                        for cloud in (0..64).map(|i| field(&format!("cloud_{i}"))).take_while(|c| !c.is_empty()) {
                            if !pool_name_ok(&cloud) || order.iter().any(|p: &Pool| p.cloud == cloud) { return Ok(message(400, "Unknown provider", "")) }
                            let mut p = routing.pool(&cloud).cloned().unwrap_or(Pool::of(&cloud));
                            p.off = false;
                            order.push(p);
                        }
                        for cloud in (0..64).map(|i| field(&format!("out_{i}"))).take_while(|c| !c.is_empty()) {
                            if !pool_name_ok(&cloud) || order.iter().any(|p: &Pool| p.cloud == cloud) { return Ok(message(400, "Unknown provider", "")) }
                            let mut p = routing.pool(&cloud).cloned().unwrap_or(Pool::of(&cloud));
                            p.off = true;
                            order.push(p);
                        }
                        if order.iter().all(|p| p.off) { return Ok(message(400, "Keep one provider in the order", "With none, no job could start.")) }
                        for p in &routing.order { if !order.iter().any(|o| o.cloud == p.cloud) { order.push(p.clone()) } }
                        routing.order = order;
                        // The order replaces the one default cloud from before.
                        routing.default = None;
                    }
                    // A place taken out of the order (its settings kept, nothing deleted anywhere), or put back, at its end.
                    "take_out" | "put_back" => {
                        let cloud = field("cloud");
                        if !pool_name_ok(&cloud) { return Ok(message(400, "Unknown provider", "")) }
                        let mut order = order_as_shown(&routing, &field("current"));
                        let Some(at) = order.iter().position(|p| p.cloud == cloud) else { return Ok(message(400, "Unknown provider", "")) };
                        let mut p = order.remove(at);
                        p.off = field("action") == "take_out";
                        if p.off && order.iter().all(|o| o.off) { return Ok(message(400, "Keep one provider in the order", "With none, no job could start. Put another back first.")) }
                        // Back at the end of the order (before those taken out); out, among those taken out.
                        let end = order.iter().position(|o| o.off).unwrap_or(order.len());
                        order.insert(if p.off { order.len() } else { end }, p);
                        routing.order = order;
                        routing.default = None;
                    }
                    // A place's settings (jobs at once, a monthly budget, where it starts machines), or an account's
                    // (AWS's own network, Cloudflare's image). The order shown (`current`) is kept as it was.
                    "pool" | "account" => {
                        let cloud = field("cloud");
                        if !pool_name_ok(&cloud) { return Ok(message(400, "Unknown provider", "")) }
                        let account = superci_core::plane::account_of(&cloud).to_string();
                        let mut order = order_as_shown(&routing, &field("current"));
                        if !order.iter().any(|p| p.cloud == cloud) { order.push(Pool::of(&cloud)) }
                        // Checked whole before anything changes (a form refused halfway would leave half of it set).
                        let want_image = if cloud == "cloudflare" && form_has("image") {
                            let image = field("image").trim().to_string();
                            Some(if image.is_empty() { None } else { Some(superci_core::plane::image_address(&image).ok_or("The image's address must start with https:// and be a plain address")?) })
                        } else { None };
                        let want_networks = if account == "aws" && form_has("networks") { Some(match given_networks(&field("networks")) { Ok(n) => n, Err(e) => return Ok(message(400, "Check your networks", &esc(&e))) }) } else { None };
                        // AWS: networks of the account's own, by region. Looked up with the account's sign-in when
                        // there is one, so a wrong id is said here, not on the first job.
                        if let Some(networks) = want_networks {
                            let st = cloudflare::status(plane.url(), &self.status_key).unwrap_or_default();
                            let want = json_value(&networks);
                            if st["aws_networks"] != want && !(networks.is_empty() && st["aws_networks"].as_object().is_none_or(|n| n.is_empty())) {
                                let account = match &plane { Plane::Aws { account_id, .. } => Some(account_id.clone()), _ => st["aws"]["account_id"].as_str().map(str::to_string) };
                                if account.as_ref().is_some_and(|a| self.hold_account(a)) {
                                    let creds = self.aws_creds()?;
                                    let http = aws::Blocking::new();
                                    for (region, n) in &networks {
                                        if let Err(e) = futures::executor::block_on(superci_core::aws::given_network(&http, region, &creds, n, now_ms())) { return Ok(message(400, "Check your networks", &esc(&e))) }
                                    }
                                }
                                self.put_secret(&plane, "AWS_NETWORKS", &want.to_string())?;
                                self.shown(&plane, |s| s["aws_networks"] == want)?;
                            }
                        }
                        // An AWS place's regions, in order (the next when one has no room). The control plane is set up in
                        // all the regions of AWS's places (a network in each, where its machines are looked for): the
                        // other place's are kept as they are now, so changing one place's leaves the other's alone.
                        if field("action") == "pool" && account == "aws" && !field("region_0").is_empty() {
                            let asked: Vec<String> = (0..REGIONS.len()).map(|i| field(&format!("region_{i}"))).filter(|r| !r.is_empty()).collect();
                            if asked.iter().any(|r| !REGIONS.contains(&r.as_str())) { return Ok(message(400, "Choose a region from the list", "")) }
                            let mut regions: Vec<String> = vec![];
                            asked.into_iter().for_each(|r| if !regions.contains(&r) { regions.push(r) });
                            let st = cloudflare::status(plane.url(), &self.status_key).unwrap_or_default();
                            let have_all: Vec<String> = st["aws_regions"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
                            order.iter_mut().filter(|p| superci_core::plane::account_of(&p.cloud) == "aws").for_each(|p| {
                                if p.cloud == cloud { p.regions = regions.clone() } else if p.regions.is_empty() { p.regions = have_all.clone() }
                            });
                            let mut all: Vec<String> = vec![];
                            order.iter().filter(|p| superci_core::plane::account_of(&p.cloud) == "aws").flat_map(|p| &p.regions).for_each(|r| if !all.contains(r) { all.push(r.clone()) });
                            let want = json_value(&all);
                            if st["aws_regions"] != want {
                                // Signed in to its account: its own network in each new one, first (else Permissions asks for it).
                                let account_id = st["aws"]["account_id"].as_str().map(str::to_string);
                                let own: Vec<String> = all.iter().filter(|r| st["aws_networks"].get(r.as_str()).is_none() && !have_all.contains(r)).cloned().collect();
                                if account_id.as_ref().is_some_and(|a| self.hold_account(a)) && !own.is_empty() { aws::make_networks(&self.aws_creds()?, plane.plane_id(), &own)?; }
                                self.put_secret(&plane, "AWS_REGIONS", &want.to_string())?;
                                self.shown(&plane, |s| s["aws_regions"] == want)?;
                            }
                        }
                        // Cloudflare's location (where its containers start).
                        let location = field("location");
                        if field("action") == "pool" && cloud == "cloudflare" && CF_LOCATIONS.iter().any(|(k, _)| *k == location) {
                            let have = cloudflare::status(plane.url(), &self.status_key).and_then(|s| s["cloudflare_location"].as_str().map(str::to_string));
                            if have.as_deref() != Some(location.as_str()) {
                                self.put_secret(&plane, "CF_LOCATION", &location)?;
                                self.shown(&plane, |s| s["cloudflare_location"] == location.as_str())?;
                            }
                        }
                        // Cloudflare: GitHub's full image, by the address it is published at (empty: the small image).
                        if let Some(want) = want_image {
                            let have = cloudflare::status(plane.url(), &self.status_key).and_then(|s| s["cloudflare_image"].as_str().map(str::to_string));
                            if have != want {
                                self.put_secret(&plane, "CF_IMAGE", want.as_deref().unwrap_or("none"))?;
                                let w = want.clone();
                                self.shown(&plane, move |s| s["cloudflare_image"].as_str().map(str::to_string) == w)?;
                            }
                        }
                        // Modal: its jobs as Functions (preemptible, a third of a sandbox's price) or as sandboxes. The
                        // Functions are deployed first, in that workspace: no job asks for one that is not there.
                        if field("action") == "pool" && form_has("mode") {
                            let function = match field("mode").as_str() { "function" => true, "sandbox" => false, _ => return Ok(message(400, "A mode is function or sandbox", "")) };
                            if superci_core::plane::kind_of(&cloud) != "modal" { return Ok(message(400, "Only Modal has a mode", "Modal's jobs run as sandboxes or as Functions.")) }
                            if function && !status.as_ref().is_some_and(|s| s["function_mode"] == true) { return Ok(message(409, "Update the control plane first", "Function mode needs a newer control plane: update it on the Control plane page (or `superci planes update`), then set it.")) }
                            // (Deployed when it is switched on; after that, when images change, see below.)
                            if function && !order.iter().any(|p| p.cloud == cloud && p.function) { self.deploy_jobs(&plane, &cloud, &routing.images)? }
                            order.iter_mut().filter(|p| p.cloud == cloud).for_each(|p| p.function = function);
                        }
                        if field("action") == "pool" {
                            order.iter_mut().filter(|p| p.cloud == cloud).for_each(|p| {
                                p.max_jobs = field("max").parse().ok().filter(|n| *n > 0 && *n <= 999);
                                p.monthly_usd = field("usd").parse::<f64>().ok().filter(|n| *n > 0.0 && n.is_finite());
                            });
                        }
                        routing.order = order;
                        routing.default = None;
                    }
                    _ => return Ok(message(400, "Unknown change", "")),
                }
                // An image added or changed: each Modal workspace in Function mode gets its Function first.
                if ["image_add", "image_update"].contains(&field("action").as_str()) {
                    for p in routing.order.iter().filter(|p| p.function && superci_core::plane::kind_of(&p.cloud) == "modal") { self.deploy_jobs(&plane, &p.cloud, &routing.images)? }
                }
                let want = json_value(&routing);
                self.put_secret(&plane, "ROUTING", &want.to_string())?;
                self.shown(&plane, |s| s["routing"] == want)?;
                Ok(Response::redirect(if field("action").starts_with("public") || field("action").starts_with("cache") || field("action").starts_with("image") || ["max_cpu", "max_hours"].contains(&field("action").as_str()) { "/?p=settings" } else { "/?p=runners" }))
            }
            // A run cancelled (see `superci runs cancel`), then its page again.
            ("POST", "/run/cancel") => {
                let plane = self.plane()?;
                let id = field("id");
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return Ok(message(400, "Which run?", "")) }
                let got = cloudflare::plane_post(plane.url(), self.key(), &format!("/runs/{id}/cancel"), &serde_json::json!({}))?;
                if let Some(e) = got["error"].as_str() { return Ok(message(400, "The run was not cancelled", &esc(e))) }
                let repo: String = url::form_urlencoded::byte_serialize(field("repo").as_bytes()).collect();
                let attempt: u64 = field("attempt").parse().unwrap_or(0);
                Ok(Response::redirect(&format!("/?p=run&repo={repo}&id={id}&attempt={attempt}")))
            }
            // A repository's cache entries taken out (all of them, or one key's).
            ("POST", "/caches/delete") => {
                let plane = self.plane()?;
                let mut body = serde_json::json!({ "repo": field("repo") });
                if !field("key").is_empty() { body["key"] = field("key").into() }
                if !field("scope").is_empty() { body["scope"] = field("scope").into() }
                cloudflare::plane_post(plane.url(), self.key(), "/caches/delete", &body)?;
                Ok(Response::redirect("/?p=settings"))
            }

            _ => Ok(message(404, "Not found", "")),
        }
    }

    /// The page's frame at once (sidebar, title, placeholders); its live part comes from `section_html`, fetched by the
    /// page itself, and refreshes itself while something is on its way. Nothing is cached: every view is fresh.
    fn page(section: &str) -> Response {
        let section = section_name(section);
        let title = section_title(section);
        let skeleton = skeleton(section, title);
        // The live part is fetched by the page itself. Links and forms within the dashboard change only that part: the
        // page stays as it is (a thin bar at the top) until the next one is ready, so nothing flashes in between.
        let script = r#"<script>(function(){var main=document.getElementById('live'),timer;
var sections={plane:'planes','add-runners':'runners',gitlab:'repos',job:'jobs',run:'jobs'};
function qp(n){return new URLSearchParams(location.search).get(n)||''}
function cc(){return qp('c')}
function keep(u){['w','c'].forEach(function(n){if(!u.searchParams.has(n)){if(qp(n))u.searchParams.set(n,qp(n))}else if(!u.searchParams.get(n))u.searchParams.delete(n)});return u}
(function(){var u=new URL(location.href);if(u.searchParams.has('c')&&!u.searchParams.get('c')){u.searchParams.delete('c');history.replaceState(null,'',u.pathname+u.search+u.hash)}})();
function superciLogs(){main.querySelectorAll('[data-view]').forEach(function(el){if(el.superci)return;el.superci={open:{}};var s=el.querySelector('[data-log-search]'),t=el.querySelector('[data-log-times]');s.addEventListener('input',function(){superciLogFind(el)});t.addEventListener('click',function(){el.classList.toggle('times');t.classList.toggle('on')});superciLog(el)})}
function superciLog(el){if(!document.body.contains(el))return;var u=new URL(el.dataset.view,location.href);if(qp('w'))u.searchParams.set('w',qp('w'));if(cc())u.searchParams.set('c',cc());fetch(u.pathname+u.search,{credentials:'same-origin'}).then(function(r){return r.json()}).then(function(v){superciLogDraw(el,v);if(v.going)setTimeout(function(){superciLog(el)},2500)}).catch(function(){setTimeout(function(){superciLog(el)},5000)})}
function superciLogDraw(el,v){el.superci.v=v;var box=el.querySelector('[data-log-steps]'),st=el.superci,end=(window.innerHeight+window.scrollY)>=document.body.scrollHeight-80,sub=el.querySelector('[data-log-sub]'),b=[];if(v.errors)b.push(v.errors+(v.errors==1?' error':' errors'));if(v.warnings)b.push(v.warnings+(v.warnings==1?' warning':' warnings'));if(v.mixed)b.push('steps ran side by side: their lines are in the order they came until the run ends');if(v.truncated)b.push(v.older?'its start is not shown: update the control plane to see it whole':'its start is not shown: Raw log has it whole');sub.textContent=b.join(' · ');sub.className='card-sub'+(v.errors?' bad-text':'');var none=!v.steps.length;if(none){var w=v.error?v.error:(v.going?'No lines yet.':'No log.');box.innerHTML='<p class="note"></p>';box.firstChild.textContent=w;return}var n=box.querySelector(':scope>p.note');if(n)n.remove();var dur=function(ms){var s=Math.round(ms/1000);return s<60?s+'s':Math.floor(s/60)+'m '+(s%60)+'s'};v.steps.forEach(function(s,i){var d=box.children[i];if(!d||d.dataset.name!==s.name){d=document.createElement('details');d.dataset.name=s.name;d.innerHTML='<summary><i class="ls"></i><span class="lsn"></span><span class="lsb"></span><span class="lsm"></span></summary><div class="lines"></div>';d.shown=0;d.num=0;d.addEventListener('toggle',function(){if(d.prog){d.prog=false}else{st.open[d.step]=d.open}if(d.open)superciLogLines(el,d);if(d.classList.contains('lgroup'))superciLogFold(box)});if(box.children[i])box.insertBefore(d,box.children[i]);else box.appendChild(d)}d.step=i;d.className='lstep '+s.state+(s.child?' lchild':'')+(s.group!=null?' lgroup':'')+(d.classList.contains('nohit')?' nohit':'');d.querySelector('.lsn').textContent=s.name;var tg=function(x){return '<span class="tag">'+x+'</span>'},bh=(s.bg?tg('background'):'')+(s.group!=null?tg('parallel')+'<span class="lst">· '+s.group+(s.group==1?' step':' steps')+'</span>':'')+(s.control?tg(s.control=='wait-all'?'wait all':s.control)+(s.targets&&s.targets.length?'<span class="lst lsw"></span>':''):''),lb=d.querySelector('.lsb');if(lb.dataset.h!==bh){lb.innerHTML=bh;lb.dataset.h=bh;var lw=lb.querySelector('.lsw');if(lw)lw.textContent=s.targets.join(', ')}d.querySelector('.lsm').textContent=s.ms!=null?dur(s.ms):'';if(s.lines.length<d.shown){d.querySelector('.lines').innerHTML='';d.shown=0;d.num=0}if(d.open)superciLogLines(el,d);});while(box.children.length>v.steps.length)box.lastChild.remove();superciLogFold(box);if(end&&v.going&&box.querySelector('details.running[open]'))window.scrollTo(0,document.body.scrollHeight);superciLogFind(el)}
function superciLogLine(l,n,hit){return '<div class="ll'+(l[1]?' k'+l[1]:'')+(hit?' hit':'')+'"><span class="ln">'+n+'</span><span class="lt">'+l[0]+'</span><span class="lx">'+l[2]+'</span></div>'}
function superciLogFold(box){var shut=false;for(var c=box.firstElementChild;c;c=c.nextElementSibling){if(c.classList.contains('lgroup'))shut=!c.open;else if(c.classList.contains('lchild'))c.classList.toggle('lhid',shut);else shut=false}}
function superciLogLines(el,d,more){var st=el.superci,s=st.v&&st.v.steps[d.step];if(!s||st.q)return;var ls=d.querySelector('.lines'),old=ls.querySelector(':scope>.ll-more');if(old)old.remove();if(more)d.limit=(d.limit||2000)+4000;var lim=s.state=='running'?Infinity:(d.limit||(d.limit=2000)),hs=[],k=d.shown||0;d.num=d.num||0;for(;k<s.lines.length&&d.num<lim;k++){var l=s.lines[k];if(l[1]=='x')continue;d.num++;hs.push(superciLogLine(l,d.num))}d.shown=k;var c=ls.lastElementChild;if(c&&c.classList.contains('lc')&&c.childElementCount<200){var f=200-c.childElementCount;c.insertAdjacentHTML('beforeend',hs.splice(0,f).join(''))}var h='';for(var j=0;j<hs.length;j+=200)h+='<div class="lc">'+hs.slice(j,j+200).join('')+'</div>';if(h)ls.insertAdjacentHTML('beforeend',h);var left=s.lines.length-k;if(left>0){ls.insertAdjacentHTML('beforeend','<button type="button" class="chip ll-more">Show '+Math.min(left,4000).toLocaleString()+' more of '+left.toLocaleString()+' lines</button>');ls.querySelector(':scope>.ll-more').onclick=function(){superciLogLines(el,d,true)}}if(!s.lines.length&&s.state=='running'&&el.dataset.host=='github'&&!ls.firstChild)ls.innerHTML='<p class="note">GitHub gives its lines once the job has ended.</p>'}
function superciLogFind(el){var st=el.superci,q=el.querySelector('[data-log-search]').value.trim().toLowerCase(),was=st.q||'';if(was===q)return;st.q=q;el.classList.toggle('finding',!!q);el.querySelectorAll('details.lstep').forEach(function(d){var s=st.v&&st.v.steps[d.step],ls=d.querySelector('.lines');if(!s)return;if(!q){ls.innerHTML='';d.shown=0;d.num=0;d.classList.remove('nohit');if(d.open)superciLogLines(el,d);return}var h='',hits=0;for(var k=0,n=0;k<s.lines.length;k++){var l=s.lines[k];if(l[1]=='x')continue;n++;var p=l.p||(l.p=l[2].replace(/<[^>]*>/g,'').replace(/&lt;/g,'<').replace(/&gt;/g,'>').replace(/&quot;/g,'"').replace(/&#39;/g,"'").replace(/&amp;/g,'&').toLowerCase());if(p.indexOf(q)>=0){hits++;if(hits<=500)h+=superciLogLine(l,n,true)}}ls.innerHTML=h+(hits>500?'<p class="note">'+(hits-500).toLocaleString()+' more lines hold it: Raw log has them all.</p>':'');d.shown=s.lines.length;d.classList.toggle('nohit',!hits);if(hits&&!d.open){d.prog=true;d.open=true}})}
function mark(){var sw=main.querySelector('[data-switch]'),ss=document.getElementById('side-switch');if(sw&&ss){if(ss.innerHTML!==sw.innerHTML)ss.innerHTML=sw.innerHTML;sw.remove()}if(ss){ss.parentNode.classList.toggle('has',!!ss.innerHTML);var bs=ss.parentNode.querySelector('summary'),pic=ss.querySelector('a[aria-current=true]>img.logo'),bi=bs.querySelector('img.mark'),bv=bs.querySelector('svg.mark');if(pic){if(!bi){bi=document.createElement('img');bi.className='mark';bi.alt='';bs.insertBefore(bi,bv)}if(bi.getAttribute('src')!==pic.getAttribute('src'))bi.setAttribute('src',pic.getAttribute('src'));bv.setAttribute('hidden','')}else if(sw||ss.innerHTML){if(bi)bi.remove();bv.removeAttribute('hidden')}}document.querySelectorAll('a[href^="/"]').forEach(function(a){if(a.target)return;if(!a.dataset.to)a.dataset.to=a.getAttribute('href');var u=new URL(a.dataset.to,location.href);if(u.pathname==='/workspaces')return;if(!u.searchParams.has('w')&&qp('w'))u.searchParams.set('w',qp('w'));if(!u.searchParams.has('c')&&cc())u.searchParams.set('c',cc());a.setAttribute('href',u.pathname+u.search+u.hash)});document.body.classList.toggle('listing',!!main.querySelector('[data-list]'));var ic=document.getElementById('icon'),bm=document.querySelector('.brand img.mark');if(ic){if(!ic.dataset.own)ic.dataset.own=ic.getAttribute('href');var want=bm?bm.getAttribute('src'):ic.dataset.own;if(ic.getAttribute('href')!==want){var ni=ic.cloneNode();ni.setAttribute('href',want);if(bm)ni.removeAttribute('type');else ni.setAttribute('type','image/svg+xml');ic.parentNode.replaceChild(ni,ic)}}}
function frag(){var q=location.search;return '/'+(q?q+'&fragment=1':'?fragment=1')+(nav?'&went=1':'')}
function current(){var p=new URLSearchParams(location.search).get('p')||'overview';p=sections[p]||p;document.querySelectorAll('.side .nav a').forEach(function(a){a.setAttribute('aria-current',String(new URL(a.href).searchParams.get('p')===p))})}
var shown='',nav=false;
function show(h){var to=h.match(/data-go="([^"]+)"/);if(to){go(to[1]);return}document.body.classList.remove('busy');clearTimeout(timer);var ld=h.match(/data-loading="([^"]+)"/),cur=main.querySelector('[data-loading]'),again=h.match(/data-refresh="(\d+)"/);if(h===shown||(ld&&cur&&cur.getAttribute('data-loading')===ld[1])){if(again)timer=setTimeout(load,again[1]*1000);return}var busyEl=document.activeElement;if(!nav&&(main.querySelector('dialog[open]')||(busyEl&&main.contains(busyEl)&&/^(INPUT|TEXTAREA|SELECT)$/.test(busyEl.tagName)&&busyEl.type!=='checkbox'&&busyEl.type!=='radio'))){timer=setTimeout(load,2000);return}nav=false;shown=h;var kept={};main.querySelectorAll('[data-keep]').forEach(function(e){kept[e.getAttribute('data-keep')]=e});main.innerHTML=h;main.querySelectorAll('[data-keep]').forEach(function(e){var k=kept[e.getAttribute('data-keep')];if(k)e.replaceWith(k)});mark();superciLogs();main.querySelectorAll('form.picker').forEach(superciPick);if(location.hash){var hd=document.getElementById(location.hash.slice(1));if(hd){if(hd.tagName==='DETAILS')hd.open=true;hd.scrollIntoView({block:'center'})}}document.body.classList.toggle('gated',!!main.querySelector('[data-gated]'));var cl=main.querySelector('[data-called]');if(cl){var cn=cl.getAttribute('data-called'),cb=document.querySelector('.brand span');if(cb.textContent!==cn)cb.textContent=cn;if(document.title!==cn)document.title=cn}var u=main.querySelector('[data-side]'),su=document.getElementById('side-update'),nx=u?u.innerHTML:'';if(u)u.remove();if(su.innerHTML!==nx)su.innerHTML=nx;document.querySelectorAll('input[name=next]').forEach(function(i){i.value=location.search+(i.dataset.open&&location.search?'&open='+i.dataset.open:'')});var op=new URLSearchParams(location.search).get('open');if(op){var od=document.getElementById(op);if(od&&od.showModal)od.showModal();if(od||!main.querySelector('[data-refresh]')){var ou=new URL(location.href);ou.searchParams.delete('open');history.replaceState(null,'',ou.pathname+ou.search+ou.hash)}}clearTimeout(timer);var t=main.querySelector('[data-refresh]');if(t)timer=setTimeout(load,t.getAttribute('data-refresh')*1000)}
var loads=0,lastAt=0,shownAt='';
function stale(fresh){if(shownAt&&shownAt!==location.pathname+location.search+location.hash){history.replaceState(null,'',shownAt);current()}var b=document.getElementById('stale-bar'),at=new Date(lastAt).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit'});document.body.classList.add('stale');if(!b){b=document.createElement('div');b.id='stale-bar';b.className='stale-bar';b.setAttribute('role','status');main.insertBefore(b,main.firstChild)}b.innerHTML=fresh?'<i class="dot"></i><span><strong>A new dashboard is running.</strong> Open the link it printed in your terminal. This is what the last one showed at '+at+'.</span>':'<span class="mini-spin"></span><span><strong>The dashboard stopped.</strong> This is what it showed at '+at+'. Run <code>superci dashboard</code> again.</span>'}
function load(){var n=++loads;fetch(frag(),{credentials:'same-origin'}).then(function(r){if(r.status===403&&shown){document.body.classList.remove('busy');stale(true);return null}return r.text().then(function(t){if(!t.trim())throw new Error('cut off');return t})}).then(function(h){if(h!=null&&n===loads){var sb=document.getElementById('stale-bar');if(sb)sb.remove();document.body.classList.remove('stale');lastAt=Date.now();shownAt=location.pathname+location.search+location.hash;show(h)}}).catch(function(){document.body.classList.remove('busy');clearTimeout(timer);if(!shown){main.innerHTML='<div class="empty">The dashboard stopped. Run <code>superci dashboard</code> again.</div>'}else stale(false);timer=setTimeout(load,4000)})}
function go(url){nav=true;document.querySelectorAll('details.menu[open]').forEach(function(d){d.open=false});var u=keep(new URL(url,location.href));history.pushState(null,'',u.pathname+u.search+u.hash);current();clearTimeout(timer);document.body.classList.add('busy');window.scrollTo(0,0);load()}
document.addEventListener('click',function(e){var a=e.target.closest&&e.target.closest('a[href]');if(!a||a.target||e.defaultPrevented||e.metaKey||e.ctrlKey||e.shiftKey||e.altKey)return;var u=new URL(a.href,location.href);if(u.origin!==location.origin||u.pathname!=='/'||u.searchParams.has('k')||(u.search===location.search&&u.hash))return;e.preventDefault();go(a.dataset.to||u.href)});
document.addEventListener('submit',function(e){var f=e.target,act=f.getAttribute('action')||'';if(e.defaultPrevented||(f.getAttribute('method')||'').toLowerCase()!=='post'||act.charAt(0)!=='/')return;if(/signin$|^\/github\/start/.test(act)){['c','w'].forEach(function(n){if(qp(n)&&!f.querySelector('[name='+n+']')){var ci=document.createElement('input');ci.type='hidden';ci.name=n;ci.value=qp(n);f.appendChild(ci)}});return}e.preventDefault();var d=f.closest('dialog');document.body.classList.add('busy');var b=e.submitter||f.querySelector('button:not([type=button])');var body=new URLSearchParams(new FormData(f,e.submitter));var was=b&&b.innerHTML;if(b&&!b.disabled){b.disabled=true;b.innerHTML='<span class="mini-spin"></span>'+(b.classList.contains('danger')?'Working…':'Saving…');b.classList.add('working')}var qs=['w','c'].filter(qp).map(function(n){return n+'='+encodeURIComponent(qp(n))}).join('&');fetch(act+(qs?(act.indexOf('?')<0?'?':'&')+qs:''),{method:'POST',body:body,credentials:'same-origin'}).then(function(r){if(d&&d.open)d.close();if(!main.contains(f)){f.reset();if(b){b.disabled=false;b.innerHTML=was;b.classList.remove('working')}}var u=new URL(r.url);if(r.redirected&&u.origin===location.origin&&u.pathname==='/'){go(u.href);return}if(r.redirected){location.href=r.url;return}return r.text().then(function(t){document.open();document.write(t);document.close()})}).catch(function(){f.submit()})});
window.addEventListener('popstate',function(){nav=true;current();document.body.classList.add('busy');load()});
window.addEventListener('pageshow',function(e){if(e.persisted)load()});
mark();load()})();</script>"#;
        document(200, &called_now(), &format!(r#"<div class="layout">{}<main class="main" id="live">{skeleton}</main></div>{PAGE_JS}{script}"#, Self::sidebar(section)), None)
    }

    /// The live part of a page: control planes (and a pending Modal sign-in) asked now, in parallel, without holding
    /// the dashboard's state meanwhile.
    fn live(shared: &Arc<Mutex<Dashboard>>, section: &str, plane: Option<usize>, new_here: Option<bool>, last: Option<&str>) -> String {
        let board = || lock(shared);
        let (planes, key, pending, quota_creds, look_creds) = {
            let mut d = board();
            if let Some(i) = plane { if i < d.planes.len() { d.selected = i } }
            if let Some(n) = new_here { d.show_setup = n }
            // Sign-ins made elsewhere meanwhile (another terminal's `superci login`): this page has them already.
            d.take_in();
            d.adopt_deploy();
            // Finding control planes after a sign-in asks the cloud once; afterwards this is instant.
            // A sign-in that ran out (Cloudflare's lasts an hour): forgotten and asked for again, where the user is,
            // whether or not the other clouds answered.
            let refused = |e: &str| e.contains("(9109)") || e.contains("Invalid access token") || e.contains("sign-in expired");
            let failed = match d.discover() { Err(e) => Some(e), Ok(()) => d.unanswered.iter().find(|u| u.starts_with("Cloudflare: ") && refused(u)).cloned() };
            if let Some(e) = failed {
                if refused(&e) {
                    // (A token given in SUPERCI_CLOUDFLARE_TOKEN is no sign-in: signing in would not replace it.)
                    if d.cf_given {
                        return format!(r#"<div class="bar"><h1 class="page-title">Cloudflare refuses its token</h1></div><div class="cards"><section class="card">{}</section></div>"#,
                            empty_state(&logo("cloudflare", 40), "Cloudflare refuses the token in SUPERCI_CLOUDFLARE_TOKEN", "Give one it takes, or leave SUPERCI_CLOUDFLARE_TOKEN out and sign in with Cloudflare. Your runners keep working meanwhile.", ""))
                    }
                    d.cf = None;
                    d.looked_cf = false;
                    return format!(r#"<div class="bar"><h1 class="page-title">Sign in again</h1></div><div class="cards"><section class="card">{}</section></div>"#,
                        empty_state(&logo("cloudflare", 40), "Your Cloudflare sign-in has ended", "Cloudflare ended it, or it was ended there. Your runners keep working meanwhile.", &signin_button("cloudflare", "Sign in with Cloudflare", true)));
                }
                return format!(r#"<div class="bar"><h1 class="page-title">Could not reach your cloud</h1></div><div class="cards"><section class="card">{}</section></div>"#, empty_state(&icon("lock"), "Your cloud did not answer", &e, r#"<a class="button" href="">Try again</a>"#))
            }
            // The AWS pages say what the account may run in each region: asked again after ten minutes.
            let account = d.aws.as_ref().map(|a| a.account_id.clone());
            let stale = account.as_ref().is_some_and(|a| d.quotas_for.as_ref().is_none_or(|(b, at)| b != a || now_ms() > at + 600_000));
            let quota_creds = if stale && ["add-runners", "runners", "planes"].contains(&section) { d.aws_creds().ok().zip(account.clone()) } else { None };
            // Where a control plane or runners are set up: what the account in hand allows (its regions, spot).
            // Not looked at yet: the page waits for the look (there is nothing to show of the account before it).
            // Looked at more than twenty seconds ago: looked at again beside the page, each time such a page is
            // opened. The page shows what it has and looks again in a moment, so a change made in AWS shows within
            // seconds, and no page waits the three seconds a look takes.
            let on_page = ["plane", "planes", "add-runners", "runners"].contains(&section_name(section));
            let unlooked = account.as_ref().is_some_and(|a| !d.aws_looks.contains_key(a));
            if let Some(a) = account.as_ref().filter(|a| on_page && !d.aws_looking.contains(*a) && d.aws_looks.get(*a).is_some_and(|(at, _)| now_ms() > at + 20_000)) {
                if let Ok(creds) = d.aws_creds() {
                    d.aws_looking.insert(a.clone());
                    let (shared, account) = (shared.clone(), a.clone());
                    std::thread::spawn(move || {
                        let (look, copying, behind) = look_all(&creds);
                        let mut d = lock(&shared);
                        d.aws_looking.remove(&account);
                        d.take_look(account, look, copying, behind);
                    });
                }
            }
            let look_creds = if unlooked && on_page { d.aws_creds().ok().zip(account) } else { None };
            (d.planes.iter().map(|p| (p.clone(), d.keyed.contains(p.plane_id()))).collect::<Vec<_>>(), d.status_key.clone(), d.modal_pending.clone(), quota_creds, look_creds)
        };
        // Read a moment ago, and nothing changed since: shown at once, read again in the background. A page shown from
        // what was kept looks again in a moment (`look_again`), so it does not stay on what was true before.
        let mut look_again = false;
        let cached = {
            let mut d = board();
            let expecting = d.github_expected.is_some_and(|until| now_ms() < until);
            let same = d.views.as_ref().is_some_and(|(v, _)| v.iter().map(|v| v.plane.plane_id()).eq(planes.iter().map(|(p, _)| p.plane_id())));
            // A move or an update that ended after the reading changed what it shows: read again.
            let ended = lock(&d.moving).as_ref().map(|m| m.ended_ms).unwrap_or(0)
                .max(lock(&d.updating).as_ref().map(|u| u.ended_ms).unwrap_or(0));
            let same = same && d.views.as_ref().is_some_and(|(_, at)| *at > ended);
            // An update that is uploaded while the old version still answers: read anew at every look.
            let restarting = lock(&d.updating).as_ref().is_some_and(|u| d.views.as_ref().is_some_and(|(views, _)| views.iter().any(|v| u.restarting(v))));
            let same = same && !restarting;
            match d.views.clone() {
                Some((views, at)) if same && pending.is_none() && quota_creds.is_none() && look_creds.is_none() && !expecting && now_ms() < at + views_kept_ms() => {
                    look_again = now_ms() > at + 3_000;
                    if now_ms() > at + 3_000 && !d.refreshing {
                        d.refreshing = true;
                        let (shared, planes, key) = (shared.clone(), planes.clone(), key.clone());
                        std::thread::spawn(move || {
                            let views = read_views(&planes, &key);
                            let mut d = lock(&shared);
                            d.refreshing = false;
                            // A change made meanwhile cleared what was kept: this reading may be from before it.
                            let complete = views.iter().all(|v| !v.online || v.status.is_some() || !d.keyed.contains(v.plane.plane_id()));
                            if d.views.is_some() && complete { let views = d.remember(views); d.views = Some((views, now_ms())) }
                        });
                    }
                    Some(views)
                }
                _ => None,
            }
        };
        // (Read now, or what was kept: only a reading made now is kept, as of now.)
        let fresh = cached.is_none();
        let (views, modal, quotas, looked) = match cached { Some(v) => (v, None, HashMap::new(), None), None => std::thread::scope(|s| {
            let handles: Vec<_> = planes.iter().map(|(p, keyed)| { let key = key.clone(); s.spawn(move || view::plane_view(p, keyed.then_some(key.as_str()))) }).collect();
            let looking = look_creds.as_ref().map(|(creds, _)| s.spawn(move || {
                look_all(creds)
            }));
            let quotas: Vec<_> = quota_creds.iter().flat_map(|(creds, _)| REGIONS.iter().map(move |r| s.spawn(move || {
                let http = aws::Blocking::new();
                (r.to_string(), futures::executor::block_on(superci_core::aws::spot_cpu_quota(&http, r, creds, now_ms())))
            }))).collect();
            // A Modal sign-in in the browser: asked about briefly on each look until it is approved.
            let modal = pending.as_ref().map(|p| modal::wait(p, 1.0));
            (handles.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<PlaneView>>(), modal, quotas.into_iter().filter_map(|h| h.join().ok()).collect::<HashMap<_, _>>(), looking.and_then(|h| h.join().ok()))
        }) };
        // A control plane that moved says where: when that one is in a cloud this session is not signed in to, it is
        // still the one in use. It is shown (known by its address), and nothing is changed until signed in there.
        let mut views = views;
        let mut follow: Vec<String> = views.iter().filter_map(|v| v.moved_to.clone()).filter(|to| !views.iter().any(|w| w.plane.url() == to)).collect();
        follow.dedup();
        for to in follow {
            if let Some(h) = cloudflare::health(&to) {
                let seen = Plane::Seen { url: to.clone(), plane_id: h["plane"].as_str().unwrap_or_default().to_string() };
                views.push(view::plane_view(&seen, None));
            }
        }
        let mut d = board();
        for v in &views { if !d.planes.iter().any(|p| p.plane_id() == v.plane.plane_id()) { d.planes.push(v.plane.clone()) } }
        // Kept to show again only when complete: a control plane still taking this session's key is read again next time.
        let complete = views.iter().all(|v| !v.online || v.status.is_some() || !d.keyed.contains(v.plane.plane_id()));
        let views = d.remember(views);
        d.note_seen(&views);
        // Its organization's picture is asked for beside the page, never in its way. With none to show yet, the
        // page looks again in a moment and has it.
        if let Some(owner) = d.picture_due() {
            if d.about.picture.is_empty() { look_again = true }
            let shared = shared.clone();
            std::thread::spawn(move || {
                let Some(picture) = github_picture(&owner) else { return };
                let mut d = lock(&shared);
                // (Still that organization's workspace: it may have been signed out meanwhile.)
                if d.picture_owner != owner { return }
                let mut about = d.about.clone();
                (about.picture, about.picture_of) = (picture, owner);
                d.set_about(about);
            });
        }
        // A reading made now is kept, as of now. One shown from what was kept stays as old as it is (else a page
        // asked for every second or two would keep it young forever, and it would never be read again), and does
        // not replace one read in the background meanwhile.
        if fresh { d.views = if complete { Some((views.clone(), now_ms())) } else { None } }
        Dashboard::measure(shared, &mut d);
        d.check_permissions();
        // The page shows, and changes, the control plane in use.
        if let Some(i) = views.get(view::in_use(&views)).and_then(|v| d.planes.iter().position(|p| p.plane_id() == v.plane.plane_id())) { d.select(i) }
        if let Some((_, account)) = quota_creds { d.quotas = quotas; d.quotas_for = Some((account, now_ms())); }
        if let (Some((_, account)), Some((look, copying, behind))) = (look_creds, looked) { d.take_look(account, look, copying, behind) }
        if let Some(Ok(Some(session))) = modal { d.came_modal(session) }
        // The App is installed (or the wait is over): nothing more to wait for.
        if d.github_expected.is_some_and(|until| now_ms() >= until) || views.get(view::in_use(&views)).is_some_and(|v| v.github && v.installed) { d.github_expected = None }
        let said_once = d.notice.take().map(|n| format!(r#"<div class="notice"><span>{}</span></div>"#, esc(&n))).unwrap_or_default();
        let html = format!("{said_once}{}{}{}{}", d.aws_stuck(), d.aws_ended_notice(), d.others_ended_notice(), d.render(section, &views, last));
        let html = if look_again && !html.contains("data-refresh=") { format!(r#"<div data-refresh="2"></div>{html}"#) } else { html };
        // A change's note is said once: once a page showed it (a page still loading has no place for it, and keeps it).
        if let Some(f) = d.flash.clone() { if html.contains(&esc(&f)) { d.flash = None } }
        d.keep();
        html
    }

    /// A sign-in with AWS that was started and has not come back, for ten minutes: AWS's sign-in page answers
    /// "400 Bad Request" when the browser holds a console session that has ended (AWS's fault, the same with its own
    /// `aws login`), and leaves the person there. Back here, the way on: signed out of AWS first, then the sign-in.
    fn aws_stuck(&self) -> String {
        if self.aws.is_some() || self.aws_pending.is_none() || now_ms() > self.aws_asked_ms + 10 * 60_000 { return String::new() }
        // AWS's own page offers the first way (sign in as usual in another tab, then use that session); signing out
        // clears the old one.
        r#"<div class="notice warn"><span>Your AWS sign-in is not finished. If AWS said “400 Bad Request”, an old AWS sign-in in this browser is in the way: <a href="https://console.aws.amazon.com/" target="_blank" rel="noopener">sign in to the AWS console</a> in another tab and try again, or</span><form method="post" action="/aws/signin"><input type="hidden" name="next"><input type="hidden" name="fresh" value="on"><button class="button secondary sm">Sign out of AWS and try again</button></form></div>"#.to_string()
    }

    /// A kept AWS sign-in that has ended: the pages still show what the control plane says (read with the kept key);
    /// changing things in AWS needs the sign-in again.
    fn aws_ended_notice(&self) -> String {
        if !self.aws_ended || self.aws.is_some() || self.planes.is_empty() { return String::new() }
        format!(r#"<div class="notice warn"><span>Your AWS sign-in has ended (AWS ends one after 12 hours). You can look around; sign in again to change things in AWS.</span>{}</div>"#, signin_button("aws", "Sign in with AWS", false))
    }

    /// The same of Cloudflare's and Modal's sign-ins: the pages go on with what the other sign-ins reach.
    fn others_ended_notice(&self) -> String {
        let mut out = String::new();
        if self.cf_still_ended() { out.push_str(&format!(r#"<div class="notice warn"><span>Your Cloudflare sign-in has expired. Sign in again to see and change what is in Cloudflare.</span>{}</div>"#, signin_button("cloudflare", "Sign in with Cloudflare", false))) }
        if self.modal_still_ended() { out.push_str(&format!(r#"<div class="notice warn"><span>Modal no longer accepts your sign-in. Sign in again to see and change what is in Modal.</span>{}</div>"#, signin_button("modal", "Sign in with Modal", false))) }
        for u in &self.unanswered {
            let (cloud, why) = u.split_once(": ").unwrap_or((u, ""));
            out.push_str(&format!(r#"<div class="notice warn"><span>{} did not answer ({}). What is there is not shown; it is asked again on the next page.</span></div>"#, esc(cloud), esc(why)))
        }
        out
    }

    /// A page's live part, with the workspace switcher's rows (see `workspaces_menu`).
    fn render(&self, section: &str, views: &[PlaneView], last: Option<&str>) -> String {
        lock(pictured()).clone_from(&self.about.picture);
        format!("{}{}", self.render_page(section, views, last), self.workspaces_menu(views))
    }

    /// The switcher under the name at the top of the sidebar: the workspaces kept on this computer, each by its name
    /// with its code host's logo once one is connected, the one shown marked; under them, making a new one. Picking
    /// one goes to its own pages: nothing of one workspace is on another's.
    fn workspaces_menu(&self, views: &[PlaneView]) -> String {
        let Some(root) = self.store.as_ref().and_then(|s| s.root()) else { return String::new() };
        let me = self.workspace();
        let all = crate::store::listed(&root, Some(&me));
        // Two looking after the same CI (a duplicate): each says which other one it is the same as.
        let same: Vec<(String, String)> = all.iter().map(|l| (l.id.clone(), l.plane_id.clone())).collect();
        let rows = all.into_iter().map(|mut l| {
            let current = l.id == me;
            let twin = same.iter().find(|(id, p)| *id != l.id && !p.is_empty() && *p == l.plane_id).map(|(id, _)| id.clone());
            // This one as it is now.
            if current {
                // (One whose CI has not been named goes by its id.)
                let name = Some(self.called_as(views.get(view::in_use(views)))).filter(|n| n != "SuperCI").unwrap_or_default();
                (l.name, l.hosts, l.picture, l.signed_in) = (name, self.about.hosts.clone(), self.about.picture.clone(), self.signed_in_as().is_some());
                l.place = views.get(view::in_use(views)).map(|v| v.plane.place()).unwrap_or_default();
            }
            let sub = match &twin { Some(t) => format!("same CI as {t}"), None => workspace_sub(&l) };
            format!(r#"<a class="space" href="/?w={}&c=" aria-current="{current}">{}<span>{}</span><small>{}</small></a>"#, esc(&l.id), workspace_logo(&l, 28), esc(l.title()), esc(&sub))
        }).collect::<String>();
        format!(r##"<div data-switch hidden>{rows}<hr><a href="#" onclick="this.closest('details').open=false;document.getElementById('new-workspace').showModal();return false"><span>New workspace…</span></a></div>"##)
    }

    fn render_page(&self, section: &str, views: &[PlaneView], last: Option<&str>) -> String {
        let section = section_name(section);
        // No cloud signed in to: whether (and where) SuperCI runs is not known yet.
        if self.cf.is_none() && self.aws.is_none() && self.modal.is_none() && views.is_empty() {
            lock(side()).clear();
            lock(called()).clone_from(&self.called_as(None));
            let says = format!(r#"<div data-called="{}" hidden></div>"#, esc(&self.called_as(None)));
            // Settings is there before anything is set up: the workspace's id, and removing it.
            if section == "settings" { return format!("{}{says}", self.settings_page(None)) }
            // Back from Modal's sign-in: the page looks again until it is approved.
            let gate = format!("{}{says}", self.gate(section, last));
            return if self.modal_pending.is_some() { format!(r#"<div data-refresh="3"></div>{gate}"#) } else { gate }
        }
        // Signed in, no CI of its own yet, and CIs found in its accounts: which one this workspace looks after (or a
        // new one) comes first.
        if views.is_empty() && !self.found.is_empty() && !["plane", "planes", "settings", "changes"].contains(&section) {
            return format!(r#"<div class="bar"><h1 class="page-title">Which CI?</h1></div>{}<p class="note" style="margin-top:14px"><a href="/?p=plane">Or set up a new one</a></p>"#, self.found_card())
        }
        // Several control planes live and this page names none: the list of them, to open one. (Setting a new one
        // up belongs to none of them.) A page that names one shows that one.
        if section == "all" || (view::several(views).is_some() && section != "plane") {
            lock(side()).clear();
            return format!(r#"{}<div data-list hidden></div><div data-called="SuperCI" hidden></div>"#, self.all_page(views));
        }
        let current = views.get(view::in_use(views));
        // A control plane that has not picked up this session's key yet: look again in a moment.
        let settling = current.is_some_and(|v| v.online && v.status.is_none() && !matches!(v.plane, Plane::Seen { .. }));
        // Until then, pages that list what the control plane has would guess (and offer to add what exists): they wait.
        // Overview does not: its public state is enough, and its numbers and jobs fill in when the key is read.
        if settling && !["plane", "overview"].contains(&section) {
            let title = section_title(section);
            // Half a minute on, one that still does not take the key will not by itself (it is from before the key's
            // name, or the key expired): why, and what to do, said; looked at again now and then.
            let waited = current.is_some_and(|v| now_ms() >= self.first_asked.get(v.plane.plane_id()).copied().unwrap_or(u64::MAX - KEY_WAIT_MS) + key_wait_ms());
            let refused = current.filter(|_| waited).and_then(|v| self.key_refused(v)).map(|why| format!(r#"<section class="card"><div class="card-head"><h2>Its control plane does not take this computer's key</h2></div><p class="note bad-text">{}</p></section>"#, esc(&why)));
            // (Settings keeps what is this computer's own meanwhile: the workspace's id, signing out, removing it.)
            if section == "settings" {
                let local = self.settings_page(None).replacen(r#"<div class="cards">"#, &format!(r#"<div class="cards">{}"#, refused.as_deref().unwrap_or_default()), 1);
                return format!(r#"<div data-refresh="{}"></div>{local}"#, if refused.is_some() { 5 } else { 2 });
            }
            if let Some(card) = refused {
                return format!(r#"<div data-refresh="5"></div><div class="bar"><h1 class="page-title">{}</h1></div><div class="cards">{card}</div>"#, esc(title))
            }
            return format!(r#"<div data-refresh="2"></div>{}"#, skeleton(section, title));
        }
        // The control plane in use is in a cloud this session is not signed in to: its pages need that sign-in, asked
        // for as the first sign-in does (the page dimmed behind it).
        if let Some(v) = current.filter(|v| matches!(v.plane, Plane::Seen { .. })).filter(|_| !["planes", "plane", "changes"].contains(&section)) {
            let cloud = v.plane.cloud();
            let from = views.iter().find(|w| w.moved_to.as_deref() == Some(v.plane.url())).map(|w| format!(" It moved there from {}.", provider_name(w.plane.cloud()))).unwrap_or_default();
            let outline = format!(r#"<div class="gate-bg" aria-hidden="true"><h1 class="page-title">{}</h1><div class="cards"><section class="card"><div class="sk w40"></div><div class="sk"></div><div class="sk w70"></div></section><section class="card"><div class="sk w30"></div><div class="sk"></div><div class="sk"></div><div class="sk w70"></div></section></div></div>"#, section_title(section));
            return format!(r#"<div class="gate" data-gated>{outline}<div class="gate-front"><div class="gate-panel"><div><h2>Your control plane is in {name}</h2><p class="lede">{from} Sign in with {name} to see it: this dashboard sees only the clouds you sign in to.</p></div><div class="picks">{}</div><p class="gate-foot"><span class="mono">{}</span> · <a href="/?p=planes">Control plane</a></p></div></div></div>"#,
                pick(cloud, &format!("Approve in {name} sign-in", name = provider_name(cloud)), false), esc(v.plane.url().trim_start_matches("https://")), name = provider_name(cloud))
        }
        // One not heard from yet (it may be taking this session's key): loading, for half a minute.
        if let Some(v) = current.filter(|v| !v.online && !self.answered.contains(v.plane.plane_id()) && now_ms() < self.first_asked.get(v.plane.plane_id()).copied().unwrap_or(0) + key_wait_ms()) {
            let _ = v;
            return format!(r#"<div data-refresh="2"></div>{}"#, skeleton(section, section_title(section)));
        }
        // One that does not answer (after the wait above): its pages say so, as Overview does, rather than guess at
        // what it has (no runners, nothing connected); the page looks again by itself. (Control plane lists it as such;
        // Settings keeps the workspace's own, which need no control plane.)
        if let Some(v) = current.filter(|v| !v.online && !matches!(v.plane, Plane::Seen { .. })).filter(|_| !["plane", "planes", "overview", "changes", "all", "settings"].contains(&section)) {
            return format!(r#"<div data-refresh="5"></div><div class="bar"><h1 class="page-title">{}</h1></div><div class="cards"><section class="card">{}</section></div>"#, section_title(section),
                empty_state(&logo(v.plane.cloud(), 40), "Your control plane is not answering", &format!("{} did not answer just now. Jobs wait until it does; this page looks again by itself.", esc(v.plane.url().trim_start_matches("https://"))), r#"<a class="button secondary sm" href="/?p=planes">Control plane</a>"#))
        }
        let (html, waiting) = match section {
            // (A deploy that finished is shown no list: on to its pages, as `plane_page` says.)
            "plane" if !views.is_empty() && !*lock(&self.deployed) => (self.planes_page(views), self.deploy_running()),
            "plane" => (self.plane_page(), false),
            "runners" => (self.runners_page(current), false),
            // (With what the AWS account in hand allows, when that is not everything.)
            "add-runners" => (format!("{}{}", self.add_runners_page(current), self.aws_account_section()), self.modal_pending.is_some()),
            _ if self.modal_pending.is_some() && current.is_none() => (self.overview_page(current).0, true),
            "jobs" => (format!(r#"<div class="bar"><h1 class="page-title">Jobs</h1>{}</div><div class="cards">{}</div>"#, jobs_tabs(false), jobs_card(current, 200)), false),
            "job" => (self.job_page(current), false),
            "run" => (self.run_page(current), false),
            "runs" => (format!(r#"<div class="bar"><h1 class="page-title">Runs</h1>{}</div><div class="cards">{}</div>"#, jobs_tabs(true), runs_card(current)), false),
            "planes" => (self.planes_page(views), self.deploy_running()),
            "changes" => (changes_page(views, &views.get(view::in_use(views)).map(|v| self.aws_lacks(v)).unwrap_or_default()), false),
            "workflows" => (self.workflows_page(current), false),
            "settings" => (self.settings_page(current), false),
            "repos" => (self.repos_page(current), false),
            "gitlab" => (self.gitlab_page(current), false),
            _ => self.overview_page(current),
        };
        let updating = lock(&self.updating);
        // Shown while it is older, and also while its update runs (it does not answer for a moment while it restarts,
        // and answers with the new version before the update's last step) and for a little while after it ended.
        let its_update = |v: &PlaneView| updating.as_ref().is_some_and(|u| u.plane == v.plane.plane_id() && (u.result.is_none() || now_ms() < u.ended_ms + 20_000));
        let card = current.filter(|v| v.outdated() || its_update(v)).map(|v| side_update(v, updating.as_ref(), &self.aws_lacks(v), self.signed_in_at(&v.plane))).unwrap_or_default();
        // While an update runs, the page looks again every two seconds (its steps).
        // While Modal's sign-in waits, every page looks again: Modal's page does not come back here by itself, so the
        // dashboard notices the approval wherever you are.
        let waiting = waiting || updating.as_ref().is_some_and(|u| u.result.is_none()) || self.modal_pending.is_some() || self.github_expected.is_some_and(|until| now_ms() < until);
        drop(updating);
        *lock(side()) = card.clone();
        let name = self.called_as(current);
        *lock(called()) = name.clone();
        let html = format!(r#"{html}<div data-called="{}" hidden></div>"#, esc(&name));
        let html = if card.is_empty() { html } else { format!(r#"{html}<div data-side hidden>{card}</div>"#) };
        if waiting || settling { format!(r#"<div data-refresh="{}"></div>{html}"#, if settling { 2 } else { 5 }) } else { html }
    }

    fn sidebar(section: &str) -> String {
        let current = match section { "plane" => "planes", "add-runners" => "runners", "gitlab" => "repos", "jobs" | "job" | "run" => "runs", s => s };
        let link = |id: &str, label: &str, icon: &str| format!(r#"<a href="/?p={id}" aria-current="{}">{icon}<span>{label}</span></a>"#, current == id);
        let nav = format!(r#"<nav class="nav">{}{}{}{}{}</nav>"#, link("overview", "Overview", ICON_OVERVIEW), link("repos", "Repositories", ICON_REPOS), link("runners", "Runners", ICON_RUNNERS),
            link("workflows", "Workflows", ICON_WORKFLOWS), link("runs", "Runs", ICON_JOBS));
        // At the bottom: an update for the control plane, when it needs one (filled in by the page's live part), the
        // control plane itself, its settings (and signing SuperCI out on this computer), and what is new.
        let foot = format!(r#"<div class="side-foot"><div id="side-update">{}</div><nav class="nav">{}{}{}</nav></div>"#, lock(side()), link("planes", "Control plane", ICON_PLANE), link("settings", "Settings", ICON_SETTINGS), link("changes", "Changelog", ICON_NEWS));
        // The name at the top is the switcher between workspaces (its rows come with each page's live part, see
        // `workspaces_menu`).
        format!(r#"<aside class="side"><details class="menu brand brand-menu"><summary>{}<span>{}</span><svg class="chev" viewBox="0 0 24 24"><path d="M8 10l4 4 4-4"/></svg></summary><div class="menu-pop" id="side-switch"></div></details>{nav}{foot}</aside>{}"#, brand_mark(&lock(pictured())), esc(&called_now()), new_workspace_dialog())
    }

    /// Where a control plane runs: reachable from Settings to add another. The connect screen's list: a row per cloud with
    /// its one action (connect, then deploy), the ones not available yet folded under "More clouds".
    fn plane_page(&self) -> String {
        // A deploy has this page to itself while it runs (or once it stopped); when it finished, on to Overview.
        if std::mem::take(&mut *lock(&self.deployed)) {
            // On to its pages; with other control planes beside it, named in the address.
            return match lock(&self.deployed_to).take().filter(|_| self.planes.len() > 1) { Some(id) => format!(r#"<div data-go="/?c={id}"></div>"#), None => r#"<div data-go="/"></div>"#.to_string() }
        }
        if let Some(progress) = self.deploy_progress() {
            return format!(r#"<div class="bar"><h1 class="page-title">Setting up your control plane</h1></div><div class="cards">{progress}</div>"#)
        }
        let rows = self.plane_rows();
        let found = self.found_card();
        let copying = if self.aws.as_ref().and_then(|a| self.aws_copying.get(&a.account_id)).is_some_and(|c| !c.is_empty()) { r#"<div data-refresh="10"></div>"# } else { "" };
        let new_one = if found.is_empty() { String::new() } else { r#"<h2 class="list-title">A new one</h2>"#.to_string() };
        format!(r#"{copying}<div class="bar"><h1 class="page-title">Set up a control plane</h1></div><p class="note" style="margin-bottom:22px">The always-on part GitHub and GitLab send jobs to, in your own cloud account. Runners can be on other clouds too.</p>{found}{new_one}<div class="picks page">{rows}</div>{}"#, self.aws_account_section())
    }

    /// Where a control plane can be set up: a row per cloud with its one action (connect, then set up); an account
    /// that has one already is not offered again.
    fn plane_rows(&self) -> String {
        // (A Cloudflare account holds one: one found there, another CI's, takes it.)
        let taken: Vec<Plane> = self.planes.iter().chain(self.found.iter().filter(|p| matches!(p, Plane::Cloudflare { .. }))).cloned().collect();
        let rows = self.setup_actions(&taken).into_iter().map(|(cloud, what, action)| provider_row(cloud, &what, &action.unwrap_or_else(|| pill("good", "Has your control plane")))).collect::<String>();
        format!("{rows}{}", more_clouds(&PLANE_SOON))
    }

    /// Each cloud a control plane can run in: what it would be there, and its one action (connect, or set up in an
    /// account that has none; None: every account signed in to has one).
    fn setup_actions(&self, planes: &[Plane]) -> Vec<(&'static str, String, Option<String>)> {
        let signed = |who: Option<String>| who.map(|w| format!(" · {w}")).unwrap_or_default();
        let cf_free: Vec<(String, String)> = self.cf_accounts.iter().filter(|(id, _)| !planes.iter().any(|p| matches!(p, Plane::Cloudflare { account_id, .. } if account_id == id))).cloned().collect();
        let cloudflare = match &self.cf {
            Some(_) if cf_free.is_empty() && !self.cf_accounts.is_empty() => None,
            Some(_) => {
                let account = match cf_free.as_slice() {
                    [(id, _)] => format!(r#"<input type="hidden" name="account" value="{}">"#, esc(id)),
                    list => format!(r#"<select name="account" aria-label="Account">{}</select>"#, list.iter().map(|(id, n)| format!(r#"<option value="{}">{}</option>"#, esc(id), esc(n))).collect::<String>()),
                };
                Some(row_form("/plane/cloudflare", &account, "Set up"))
            }
            None => Some(connect_action("cloudflare")),
        };
        // AWS: the accounts signed in to that have none (set one up there); when each has one, another account
        // can be signed in to beside them.
        let aws_free: Vec<String> = self.aws_accounts().into_iter().map(|a| a.account_id).filter(|id| !planes.iter().any(|p| matches!(p, Plane::Aws { account_id, .. } if account_id == id))).collect();
        let aws = match (self.aws_accounts().is_empty(), aws_free.as_slice()) {
            (true, _) => Some(connect_action("aws")),
            (_, []) => Some(r#"<form method="post" action="/aws/signin"><input type="hidden" name="next"><button class="button secondary sm">Another AWS account</button></form>"#.to_string()),
            (_, free) => {
                let account = match free { [id] => format!(r#"<input type="hidden" name="account" value="{}">"#, esc(id)), list => format!(r#"<select name="account" aria-label="Account">{}</select>"#, list.iter().map(|id| format!(r#"<option value="{0}">account {0}</option>"#, esc(id))).collect::<String>()) };
                // The regions the account in hand can use (all of them until it has been looked at): one alone is
                // said, several are chosen among, none leaves nothing to set up.
                match self.aws_look().map(|l| l.usable_regions()) {
                    // Allowed somewhere, but the image jobs run on is not there: copied in first (one press, AWS
                    // takes about ten minutes), then set up.
                    Some(usable) if usable.is_empty() => {
                        let (without, copying) = (self.aws_look().map(|l| l.regions_without_image()).unwrap_or_default(), self.aws.as_ref().and_then(|a| self.aws_copying.get(&a.account_id)).cloned().unwrap_or_default());
                        Some(match without.first() {
                            Some(region) if copying.iter().any(|c| c == region) => format!(r#"<span class="waiting-for"><span class="mini-spin"></span>Copying the image into {}: about ten minutes</span>"#, esc(&region_short(region))),
                            Some(region) => format!(r#"<form method="post" action="/aws/image" class="aws-add">{account}<input type="hidden" name="region" value="{}"><span class="note">Needed before Set up: about ten minutes</span><button class="button primary sm">Copy the image into {1}</button></form>"#, esc(region), esc(&region_short(region))),
                            None => r#"<span class="note">No region to set it up in</span>"#.to_string(),
                        })
                    }
                    Some(usable) if usable.len() == 1 => Some(format!(r#"<form method="post" action="/plane/aws" class="aws-add">{account}<input type="hidden" name="region" value="{0}"><span class="note">{1}</span><button class="button primary sm">Set up</button></form>"#, esc(usable[0]), esc(&region_short(usable[0])))),
                    Some(usable) => Some(format!(r#"<form method="post" action="/plane/aws" class="aws-add">{account}<details class="change"><summary class="button secondary sm">Change region</summary><select name="region" aria-label="Region">{}</select></details><button class="button primary sm">Set up</button></form>"#, region_options_among(&usable))),
                    None => Some(format!(r#"<form method="post" action="/plane/aws" class="aws-add">{account}<details class="change"><summary class="button secondary sm">Change region</summary><select name="region" aria-label="Region">{}</select></details><button class="button primary sm">Set up</button></form>"#, region_options())),
                }
            }
        };
        let modal = match &self.modal {
            Some(m) if planes.iter().any(|p| matches!(p, Plane::Modal { workspace, .. } if *workspace == m.workspace)) => None,
            Some(_) => Some(row_form("/plane/modal", "", "Set up")),
            None => Some(connect_action("modal")),
        };
        vec![
            ("cloudflare", format!("A Worker with its own storage{}", signed(self.cf.as_ref().map(|_| cf_free.iter().map(|a| a.1.as_str()).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()))), cloudflare),
            ("aws", format!("A Lambda function with a table{}", signed(aws_free.first().cloned().or_else(|| self.aws.as_ref().map(|a| a.account_id.clone())).map(|a| match self.aws_look().map(|l| l.usable_regions()) { Some(usable) if !usable.is_empty() && !usable.contains(&"us-east-1") => format!("account {a} · {}", region_name(usable[0])), Some(usable) if usable.is_empty() => format!("account {a}"), _ => format!("account {a} · US East (N. Virginia)") }))), aws),
            ("modal", format!("A web endpoint with a Dict{}", signed(self.modal.as_ref().map(|m| format!("workspace {}", m.workspace)))), modal),
        ]
    }

    /// Before any cloud is connected: the page's outline, dimmed, under one question — where the control plane is (runners
    /// can be on any cloud; the control plane is in one). The cloud picked here last is marked "Last used" (from its own
    /// cookie: sign-ins for runners do not change it). Starting fresh is set apart below, as its own question.
    fn gate(&self, section: &str, last: Option<&str>) -> String {
        let pick = |provider: &str, sub: &str| pick(provider, sub, last == Some(provider));
        let title = section_title(section);
        let outline = format!(r#"<div class="gate-bg" aria-hidden="true"><h1 class="page-title">{title}</h1><div class="cards"><section class="card"><div class="sk w40"></div><div class="sk"></div><div class="sk w70"></div></section><section class="card"><div class="sk w30"></div><div class="sk"></div><div class="sk"></div><div class="sk w70"></div></section></div></div>"#);
        let panel = if self.show_setup {
            format!(r#"<a class="gate-back" href="/?p=overview&amp;new=0">← Back</a><div><h2>Where should your control plane live?</h2><p class="lede">It's the always-on part GitHub sends jobs to, in your own cloud account. Runners can be on other clouds too.</p></div>
<div class="picks">{}{}{}</div><p class="gate-foot">Nothing is created until you press Deploy.</p>"#,
                format!("{}{}", pick("cloudflare", "A Worker with its own storage"), pick("aws", "A Lambda function with a table")), pick("modal", "A web endpoint with a Dict"), more_clouds(&PLANE_SOON))
        } else {
            format!(r#"<div><h2>Where is your control plane?</h2><p class="lede">The always-on part GitHub sends jobs to. Runners can be on any cloud.</p></div>
<div class="picks">{}{}{}</div>
<div class="first"><div><strong>First time here?</strong><small>Set up a control plane in your own Cloudflare, AWS or Modal. About 5 minutes.</small></div><a class="button secondary" href="/?p=overview&amp;new=1">Set one up</a></div>"#,
                format!("{}{}", pick("cloudflare", "Approve “Wrangler”, Cloudflare's login"), pick("aws", "Approve in AWS sign-in")), pick("modal", "Approve in Modal's sign-in"), more_clouds(&PLANE_SOON))
        };
        format!(r#"<div class="gate" data-gated>{outline}<div class="gate-front"><div class="gate-panel">{panel}</div></div></div>"#)
    }

    /// A deploy on its way (each step as it starts, refreshing itself) or one that stopped (why, and trying again), as
    /// the card "Set up a control plane" shows in place of its list.
    fn deploy_progress(&self) -> Option<String> {
        let g = lock(&self.deploying);
        let d = g.as_ref()?;
        let name = provider_name(d.cloud);
        let (title, sub, running) = match &d.result {
            None => (format!("Deploying to {name}"), format!("{} · {}", esc(&d.place), match d.cloud { "aws" => "about a minute", "modal" => "about 2 minutes", _ => "about 20 seconds" }), true),
            Some(Err(e)) => (format!("The deploy to {name} stopped"), esc(&plain_refusal(e).unwrap_or_else(|| e.clone())), false),
            Some(Ok(_)) => return None,
        };
        let steps = d.steps.iter().enumerate().map(|(i, s)| {
            let state = if i < d.at { "done" } else if i == d.at { if running { "now" } else { "failed" } } else { "" };
            format!(r#"<li class="{state}"><span class="tick">{}</span>{}</li>"#, match state { "done" => icon("check"), "failed" => "!".to_string(), _ => String::new() }, esc(s))
        }).collect::<String>();
        let retry = if running { String::new() } else {
            // Again as it was (what the first try made is reused), or back to choosing where: another region, another cloud.
            format!(r#"<div class="deploy-do"><form method="post" action="/plane/{}">{}<button class="button primary">Try again</button></form><form method="post" action="/plane/start-over"><button class="button secondary">Choose another place</button></form><span class="note">Anything already created is reused.</span></div>"#,
                d.cloud, d.form.iter().map(|(k, v)| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v))).collect::<String>())
        };
        Some(format!(r#"{}<section class="card deploy{}"><div class="deploy-head">{}<div><h2>{}</h2><p class="note">{sub}</p></div></div><ol class="progress">{steps}</ol>{retry}</section>"#,
            if running { r#"<div data-refresh="1"></div>"# } else { "" }, if running { "" } else { " stopped" }, logo(d.cloud, 40), esc(&title)))
    }

    /// The deploy in a line, for the first step of Set up on Overview: it has its own page.
    fn deploy_line(&self) -> Option<String> {
        let g = lock(&self.deploying);
        let d = g.as_ref()?;
        let name = provider_name(d.cloud);
        match &d.result {
            None => Some(format!(r#"<div data-refresh="2"></div><h3>Set up a control plane {}</h3><p>Deploying to {name} · {}</p><div class="do"><a class="button secondary sm" href="/?p=plane">See its progress</a></div>"#, pill("open", "deploying"), esc(&d.place))),
            Some(Err(_)) => Some(format!(r#"<h3>Set up a control plane {}</h3><p>The deploy to {name} stopped.</p><div class="do"><a class="button primary sm" href="/?p=plane">See why</a></div>"#, pill("bad", "stopped"))),
            Some(Ok(_)) => None,
        }
    }

    /// Before there is a control plane: the Set up card with its first step open (where to put the control plane, or
    /// the deploy on its way), and the steps that follow shown locked, as they are once it is there (`setup_card`).
    fn first_setup_card(&self) -> String {
        let connected: Vec<&'static str> = [("cloudflare", self.cf.is_some()), ("aws", self.aws.is_some()), ("modal", self.modal.is_some())].into_iter().filter(|c| c.1).map(|(c, _)| c).collect();
        let first = match self.deploy_line() {
            Some(line) => line,
            // One button: where it goes is chosen on "Set up a control plane" (a row per cloud).
            None => {
                let names = connected.iter().map(|c| provider_name(c)).collect::<Vec<_>>().join(" or ");
                // (With no cloud signed in to, a page is the connect screen: here one is.)
                let about = if self.show_setup {
                    "The always-on part GitHub and GitLab send jobs to, in your own cloud account. Runners can be on other clouds too.".to_string()
                } else {
                    format!("No SuperCI in your {names} account yet, and nothing was created. Its control plane is the always-on part GitHub and GitLab send jobs to; runners can be on other clouds too.")
                };
                format!(r#"<h3>Set up a control plane</h3><p>{about}</p><div class="do"><a class="button primary" href="/?p=plane">Set up control plane</a></div>"#)
            }
        };
        // The steps after it, side by side and short: the page below already shows what they lead to.
        let then = |n: u8, title: &str, text: &str| format!(r#"<div class="then-step"><div class="num">{n}</div><div><h3>{title}</h3><p>{text}</p></div></div>"#);
        let steps = format!(r#"{}<div class="then">{}{}{}</div>"#, setup_step(1, "", String::new(), first),
            then(2, "Connect GitHub or GitLab", "Where your jobs come from."),
            then(3, "Add runners", "Where they run: AWS, Cloudflare, Modal."),
            then(4, "Change one line", "<code>runs-on: superci</code> in a workflow."));
        format!(r#"<section class="card" id="setup"><div class="card-head"><h2>Set up</h2>{}</div><div class="steps">{steps}</div></section>"#, pill("open", "4 steps"))
    }

    fn overview_page(&self, v: Option<&PlaneView>) -> (String, bool) {
        // Connected, and nothing found there: deploying is the first step of Set up (connecting never deploys), above
        // the page as it will be, still empty.
        let Some(v) = v else {
            return (format!(r#"<div class="bar"><h1 class="page-title">Overview</h1></div><div class="cards">{}{}</div>"#, self.first_setup_card(), overview_empty_cards("superci")), false);
        };
        // Not answering: said as it is (setting up again is not what it needs); the page looks again meanwhile.
        if !v.online {
            let cloud = v.plane.cloud();
            return (format!(r#"<div class="bar"><h1 class="page-title">Overview</h1></div><div class="cards"><section class="card">{}</section></div>"#,
                empty_state(&logo(cloud, 40), "Your control plane is not answering", &format!("{} did not answer just now. Jobs wait until it does; this page looks again by itself.", v.plane.url().trim_start_matches("https://")), r#"<a class="button secondary" href="/?p=planes">Control plane</a>"#)), true)
        }
        let jobs = v.jobs();
        let (setup, waiting) = self.setup_card(v);
        // Still reading it: the page's own loading shape (the same as before the first answer), looked at again.
        if v.status.is_none() && setup.is_empty() { return (skeleton("overview", "Overview"), true) }
        let jobs_card = if v.status.is_none() { overview_skeleton_cards() } else { overview_cards(v, &jobs, &usage_shown()) };
        (format!(r#"<div class="bar"><h1 class="page-title">Overview</h1></div><div class="cards">{setup}{jobs_card}</div>"#), waiting)
    }

    /// Setting up after the control plane: numbered steps; each says what it does, shows its one action, and stays locked
    /// until the step before is done. Gone when everything is ready. Also whether the page should refresh by itself.
    /// Only for a control plane that answers (Overview says one that does not as such, without this card).
    fn setup_card(&self, v: &PlaneView) -> (String, bool) {
        if v.ready() { return (String::new(), false) }
        let (step, locked) = (setup_step, setup_locked);

        // 1. The control plane.
        let plane = step(1, "done", format!("<h3>Control plane {}</h3>", pill("good", "running")),
            format!("<p>{} · <code>{}</code></p>", esc(&v.plane.place()), esc(v.plane.url().trim_start_matches("https://"))));

        // Each step says where it stands in a line and has one button, to the page where it is done: nothing is done
        // here.
        // 2. Where jobs come from: GitHub or GitLab (or both), on Repositories.
        let gh_done = v.github && v.installed;
        let connected: Vec<String> = [v.app_owner().filter(|_| gh_done).map(|o| format!("GitHub · {o}")), v.gitlab_url().map(|u| format!("GitLab · {}", u.trim_start_matches("https://")))].into_iter().flatten().collect();
        let github = if gh_done || v.gitlab {
            step(2, "done", format!("<h3>GitHub or GitLab {}</h3>", pill("good", "connected")), format!("<p>{}</p>", esc(&connected.join(", "))))
        } else if self.github_expected.is_some_and(|until| now_ms() < until) {
            // Just back from GitHub: the control plane has not said it yet. The page looks again by itself.
            step(2, "", "<h3>Connect GitHub or GitLab</h3>".into(), format!("<p>Where your jobs come from.</p>{GITHUB_ON_ITS_WAY}"))
        } else if v.github {
            // The App exists and is installed nowhere yet: that is done on GitHub, from Repositories.
            step(2, "", "<h3>Connect GitHub or GitLab</h3>".into(), r#"<p>Your GitHub App is made; it still has to be installed where your runners should take jobs.</p><div class="do"><a class="button primary" href="/?p=repos">Finish on Repositories</a></div>"#.to_string())
        } else {
            step(2, "", "<h3>Connect GitHub or GitLab</h3>".into(), r#"<p>Where your jobs come from; one is enough, both work.</p><div class="do"><a class="button primary" href="/?p=repos">Connect repositories</a></div>"#.to_string())
        };

        // 3. Runners.
        let logos = format!(r#"<span class="logos">{}{}{}</span>"#, logo("cloudflare", 22), logo("aws", 22), logo("modal", 22));
        let runners = if v.runners {
            step(3, "done", format!("<h3>Runners {}</h3>", pill("good", "added")), format!("<p>{}</p>", esc(&v.clouds().iter().map(|c| cloud_name(c)).collect::<Vec<_>>().join(", "))))
        } else {
            step(3, "", format!("<h3>Add runners {logos}</h3>"), r#"<p>Where jobs run: containers in Cloudflare, spot machines in AWS, sandboxes in Modal. Add one or more; the control plane decides per job.</p><div class="do"><a class="button primary" href="/?p=add-runners">Add runners</a></div>"#.to_string())
        };
        // 4. The one line a workflow needs, on Workflows.
        let label = esc(&v.label());
        // (Locked: once the steps before it are done the card is gone, and Overview's jobs card says the line.)
        let workflow = step(4, "locked", "<h3>Use it in a workflow</h3>".into(), format!("<p>Set <code>runs-on: {label}</code> in a workflow; nothing else changes.</p>{}", locked("GitHub or GitLab, and runners")));
        let waiting = v.github && !v.installed;
        (format!(r#"<section class="card" id="setup"><div class="card-head"><h2>Set up</h2>{}</div><div class="steps">{plane}{github}{runners}{workflow}</div></section>"#, pill("open", "4 steps")), waiting)
    }

    /// Every runner pool, one list: its provider, what it runs jobs on, where, and whether it is ready.
    fn runners_page(&self, v: Option<&PlaneView>) -> String {
        let add = r#"<a class="chip" href="/?p=add-runners">+ Add provider</a>"#;
        let Some(v) = v else {
            return format!(r#"<div class="bar"><h1 class="page-title">Runners</h1></div><div class="cards"><section class="card">{}</section></div>"#,
                empty_state(&icon("runners"), "No runners yet", "Runners connect to a control plane. Set one up first, then add runners here.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        };
        let body = if v.order().is_empty() {
            format!(r#"<section class="card">{}</section>"#, empty_state(&icon("runners"), "No runners yet", "Add containers in Cloudflare, spot machines in AWS or sandboxes in Modal. The control plane picks where each job runs.",
                &format!(r#"<span class="logos">{}{}{}</span><a class="button primary" href="/?p=add-runners">Add runners</a>"#, logo("cloudflare", 30), logo("aws", 30), logo("modal", 30))))
        } else {
            // Signed in where each provider's part is (removing it deletes that part).
            let account = aws_account(v);
            let further: Vec<String> = self.modal_others.iter().map(|m| format!("modal:{}", m.workspace)).collect();
            let signed_in: Vec<&str> = [("aws", self.aws.iter().chain(&self.aws_others).any(|a| Some(&a.account_id) == account.as_ref())), ("modal", self.modal.is_some()), ("cloudflare", self.cf.is_some())]
                .into_iter().filter(|(_, yes)| *yes).map(|(c, _)| c).chain(further.iter().map(String::as_str)).collect();
            format!("{}{}{}", pools_card(v, &self.quotas, self.cf_month, self.modal_month.as_ref(), self.aws_look()), clouds_card(v, &signed_in), spending_card(v, self.budget_lifted.get(v.plane.plane_id()).copied().unwrap_or(0)))
        };
        let note = match &self.flash { Some(f) => format!(r#"<div class="notice">{}<span>{}</span></div>"#, icon("check"), esc(f)), None => String::new() };
        format!(r#"<div class="bar"><h1 class="page-title">Runners</h1>{add}</div>{note}<div class="cards">{body}</div>"#)
    }

    /// The providers runners can come from: the connect screen's list, a row each with its state or its one step.
    fn add_runners_page(&self, v: Option<&PlaneView>) -> String {
        let Some(v) = v else {
            return format!(r#"<div class="bar"><h1 class="page-title">Add runners</h1></div><div class="cards"><section class="card">{}</section></div>"#,
                empty_state(&icon("lock"), "Set up a control plane first", "Runners connect to a control plane: it registers each job's runner with GitHub and starts the machine.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        };
        let launcher = |cloud: &str| v.launchers().into_iter().any(|a| a.cloud == cloud);
        let added = pill("good", "Added");
        let cloudflare = if launcher("cloudflare") || (matches!(v.plane, Plane::Cloudflare { .. }) && v.own_containers()) { added.clone() }
            else if self.cf.is_some() { row_form("/runners/cloudflare", "", "Add") } else { connect_action("cloudflare") };
        // AWS: a region picked for you (with why), what the account may run there, and one click; another region if needed.
        let aws = if v.aws { added.clone() }
            // A control plane in AWS whose AWS runners were removed: back in its own account.
            else if matches!(v.plane, Plane::Aws { .. }) { if self.aws.is_some() { row_form("/runners/aws-own", "", "Add") } else { connect_action("aws") } }
            else if self.aws.is_some() {
            format!(r#"<form method="post" action="/aws/connect" class="aws-add"><details class="change"><summary class="button secondary sm">Change region</summary><select name="region" aria-label="Region">{}</select></details><button class="button primary sm">Add</button></form>"#,
                region_select_options("us-east-1", &self.quotas, &closest(Some(v)), self.aws_look()))
        } else { connect_action("aws") };
        // A Modal control plane starts sandboxes itself; one elsewhere gets a small launcher in your Modal workspace.
        let own_modal = matches!(v.plane, Plane::Modal { .. });
        let modal = if launcher("modal") || (own_modal && v.own_containers()) { added } else if own_modal { row_form("/runners/modal-own", "", "Add") } else if self.modal.is_some() { row_form("/runners/modal", "", "Add") }
            else if self.modal_pending.is_some() { format!(r#"{}<form method="post" action="/modal/signin"><input type="hidden" name="next"><button class="button secondary sm">Open again</button></form>"#, pill("open", "Waiting for Modal")) }
            else { connect_action("modal") };
        let containers = if matches!(v.plane, Plane::Cloudflare { .. }) { "Containers, one per job, started by your control plane." } else { "Containers, one per job, through a small launcher in your Cloudflare." };
        let aws_what = match &self.aws {
            Some(a) if !v.aws => {
                let quota = match self.quotas.get("us-east-1") { Some(Ok(n)) => format!("<br>{}", quota_text("us-east-1", *n)), _ => String::new() };
                format!("Spot machines, one per job, in account {}: in {}, {}, with the most spot capacity; {} and {} when it runs out.{quota}",
                    esc(&a.account_id), region_name("us-east-1"), closest(Some(v)), region_short("us-east-2"), region_short("us-west-2"))
            }
            _ => "Spot machines, one per job, in your AWS account.".to_string(),
        };
        // Each further Modal workspace signed in to: a launcher there, a place of its own in the order.
        let further = self.modal_others.iter().map(|m| {
            let end = if launcher(&format!("modal:{}", m.workspace)) { pill("good", "Added") } else { row_form("/runners/modal", &format!(r#"<input type="hidden" name="account" value="{}">"#, esc(&m.workspace)), "Add") };
            provider_row("modal", &format!("Sandboxes, one per job, in the Modal workspace {}: a provider of its own, with its own limits.", m.workspace), &end)
        }).collect::<String>();
        let rows = format!("{}{}{}{further}{}", provider_row("cloudflare", containers, &cloudflare), provider_row_html("aws", &aws_what, &aws),
            provider_row("modal", "Sandboxes, one per job, in your Modal workspace. The first add builds a small image.", &modal), more_clouds(&RUNNERS_SOON));
        format!(r#"<div class="bar"><h1 class="page-title">Add a provider</h1><a class="chip" href="/?p=runners">Back to runners</a></div><p class="note" style="margin-bottom:22px">Where runners come from: each provider starts a fresh one for every job. Add one or more.</p><div class="picks page">{rows}</div>"#)
    }

    /// Where jobs come from: a row per code host (GitHub, GitLab; the rest folded as coming), each with its state and
    /// one step (GitLab's connect form opens in its row; connected, its settings are a page of their own). How workflows
    /// use them is Workflows.
    fn repos_page(&self, v: Option<&PlaneView>) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">Repositories</h1></div>"#;
        let Some(v) = v else {
            return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&icon("lock"), "Set up a control plane first", "GitHub and GitLab send their jobs to your control plane.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        };
        // GitHub: create the App, install it, then where it is. One App per organization (a private App belongs to one
        // account): the first, then any added; each a row, and a form to add another.
        let slug = v.app_slug().unwrap_or_default();
        let st = v.status.clone().unwrap_or_default();
        let apps = status_apps(&st);
        // Connecting is a dialog of three steps (`github_dialog`, `gitlab_dialog`); a row's button opens it.
        let opens = |id: &str, label: &str, primary: bool| format!(r#"<button type="button" class="button {} sm" onclick="document.getElementById('{id}').showModal()">{label}</button>"#, if primary { "primary" } else { "secondary" });
        let row = |title: &str, what: &str, end: &str| format!(r#"<div class="pick act">{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{end}</span></div>"#, logo("github", 36), esc(title), esc(what));
        // (Once it says the App is installed nothing is awaited any more: see `live`.)
        let on_its_way = self.github_expected.is_some_and(|until| now_ms() < until);
        let github = if on_its_way {
            // Just back from GitHub: said as that until the control plane has it (the page looks again by itself).
            row("GitHub", "A private App in your organization: it registers runners and reads job events, never your code", GITHUB_ON_ITS_WAY)
        } else if !v.github {
            format!("{}{}", row("GitHub", "A private App in your organization: it registers runners and reads job events, never your code", &opens("gh-add", "Connect", true)), github_dialog(false, &v.plane.place()))
        } else if apps.is_empty() {
            // Its details not read yet (or an older control plane): the one App, as its public state says.
            if v.installed { row("GitHub", &format!("Jobs from {}", v.app_owner().unwrap_or_default()), &pill("good", "connected")) }
            else { row("GitHub", &format!("The App {slug} exists; install it where your runners should take jobs"), &format!(r#"<a class="button primary sm" href="{}">Install</a>"#, esc(&github::install_link(v.app_host().as_deref(), &slug)))) }
        } else {
            let several = apps.len() > 1;
            let rows = apps.iter().enumerate().map(|(n, app)| {
                let (owner, slug, org) = (app["owner"].as_str().unwrap_or_default(), app["slug"].as_str().unwrap_or_default(), app["org"] != false);
                let installs: Vec<String> = st["installations"].as_array().into_iter().flatten().filter(|i| i["app"].is_null() || i["app"] == app["id"])
                    .map(|i| format!("{} ({})", i["account"].as_str().unwrap_or(""), if i["repositories"] == "all" { "all repositories" } else { "selected repositories" })).collect();
                let title = if several { format!("GitHub · {owner}") } else { "GitHub".to_string() };
                let manage = format!(r#"<a class="button secondary sm" href="{}" target="_blank" rel="noopener">Manage</a>"#, esc(&app_settings_link(app["host"].as_str(), owner, slug, org)));
                // An organization added after the first can be removed again.
                let plain: String = owner.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                let remove = if n == 0 { String::new() } else { format!(r#"<form method="post" action="/github/remove" onsubmit="return confirm('Remove {plain}? Its App is uninstalled there; its jobs asking for your label then wait for runners that never come.')"><input type="hidden" name="app" value="{}"><button class="button danger sm">Remove</button></form>"#, app["id"].as_u64().unwrap_or_default()) };
                if installs.is_empty() {
                    row(&title, &format!("The App {slug} exists; install it where your runners should take jobs"), &format!(r#"<a class="button primary sm" href="{}">Install</a>{remove}"#, esc(&github::install_link(app["host"].as_str(), slug))))
                } else {
                    row(&title, &format!("Jobs from {}", installs.join(", ")), &format!("{}{manage}{remove}", pill("good", "connected")))
                }
            }).collect::<String>();
            // One more can be added, on a control plane that knows several.
            let old = view::older(v.version.as_deref(), "0.9.31");
            let add = row("Another organization", if old { "Update the control plane first: several organizations need 0.9.31 or newer" } else { "Its own private App, on this control plane: its jobs run on the same runners" },
                &if old { String::new() } else { opens("gh-add", "Add", false) });
            format!("{rows}{add}{}", if old { String::new() } else { github_dialog(true, &v.plane.place()) })
        };
        // GitLab: a row for each connection (its projects and the connection are its own page), and one more to
        // add (another GitLab, or another account's token); none yet: Connect.
        let gls = v.gitlabs();
        let host = |u: &str| u.trim_start_matches("https://").to_string();
        let gitlab = if gls.is_empty() {
            format!(r#"<div class="pick act" id="gitlab">{}<span><strong>GitLab</strong><small>gitlab.com or your own GitLab: jobs from the projects you turn on</small></span><span class="pick-end">{}</span></div>{}"#,
                logo("gitlab", 36), opens("gl-add", "Connect", true), gitlab_dialog(false))
        } else {
            let rows = gls.iter().enumerate().map(|(n, (id, url, _))| format!(r#"<div class="pick act"{}>{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{}<a class="button secondary sm" href="/?p=gitlab&g={}">Settings</a></span></div>"#,
                if n == 0 { r#" id="gitlab""# } else { "" }, logo("gitlab", 36), if gls.len() > 1 { format!("GitLab · {}", esc(&host(url))) } else { "GitLab".to_string() },
                if gls.len() > 1 { "Jobs from the projects you turn on".to_string() } else { format!("{} · jobs from the projects you turn on", esc(&host(url))) }, pill("good", "connected"), esc(id))).collect::<String>();
            let old = view::older(v.version.as_deref(), "0.9.37");
            let add = format!(r#"<div class="pick act">{}<span><strong>Another GitLab</strong><small>{}</small></span><span class="pick-end">{}</span></div>"#, logo("gitlab", 36),
                if old { "Update the control plane first: several GitLabs need 0.9.37 or newer" } else { "Another server, or another account's token: its jobs run on the same runners" }, if old { String::new() } else { opens("gl-add", "Add", false) });
            format!("{rows}{add}{}", if old { String::new() } else { gitlab_dialog(true) })
        };
        let sources = format!(r#"<section class="card"><div class="card-head"><h2>Where jobs come from</h2></div><p class="note">Your code hosts send their jobs to your control plane; the ones that ask for your label run on your runners (<a href="/?p=workflows">in your workflows</a>).</p><div class="picks">{github}{gitlab}{}</div></section>"#, more_rows("More code hosts", &HOSTS_SOON));
        // (A change's note: an organization removed, with what is left to do on GitHub.)
        let note = match &self.flash { Some(f) => format!(r#"<div class="notice">{}<span>{}</span></div>"#, icon("check"), esc(f)), None => String::new() };
        format!(r#"{bar}{note}<div class="cards">{sources}</div>"#)
    }

    /// How workflows use your runners, for the code hosts connected (a switch when both are): the label, the steps
    /// for each host; then the machines: the default one, and a picker for the label of another.
    fn workflows_page(&self, v: Option<&PlaneView>) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">Workflows</h1></div>"#;
        let Some(v) = v else {
            return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&icon("lock"), "Set up a control plane first", "Workflows use your runners through its label.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        };
        let label = esc(&v.label());
        let gh_on = v.github;
        let gl_on = v.gitlab_url().is_some();
        let (default_card, picker) = machines_card(v);
        let step = |n: u8, body: String| format!(r#"<li><span class="tick">{n}</span><div>{body}</div></li>"#);
        let sizes = |note: &str| format!(r#"Another machine: add it to the label, like <code>{label}/cpu=8</code>, <code>{label}/arch=arm64</code> or <code>{label}/cpu=16/ram=64</code>.{note}<div class="step-do"><button type="button" class="button secondary sm" onclick="document.getElementById('label-picker').showModal()">Choose a machine…</button></div>"#);
        let gh_steps = [
            step(1, format!(r#"Set <code>runs-on</code> to your label:<pre class="snippet labels">jobs:
  test:
    <span class="hl">runs-on: {label}</span>
    steps:
      - uses: actions/checkout@v5</pre>"#)),
            step(2, sizes("")),
        ].concat();
        let gl_steps = [
            step(1, r#"Turn the project on in <a href="/?p=gitlab">GitLab settings</a>. That adds a webhook for its job events; nothing else in it changes."#.to_string()),
            step(2, format!(r#"Tag the job with your label, and give it an image (jobs run in Docker):<pre class="snippet labels">test:
  <span class="hl">tags: [{label}]</span>
  image: node:24
  script:
    - npm test</pre>"#)),
            step(3, sizes(r#"<p class="note step-note">GitLab jobs run on AWS and Cloudflare (Modal's sandboxes have no Docker). On Cloudflare, <code>services:</code> are on <code>localhost</code>.</p>"#)),
        ].concat();
        // Switching workflows with a coding agent: a prompt to copy (supercov's Improve, for runners).
        let switch = if gh_on || gl_on { agent_button("Switch workflows", "Switch workflows with a coding agent", "superciPrompt()") } else { String::new() };
        let prompt = if gh_on || gl_on { agent_prompt_dialog(v, gh_on, gl_on) } else { String::new() };
        let usage = match (gh_on, gl_on) {
            (true, true) => format!(r#"<section class="card"><div class="card-head"><h2>In your workflows</h2><div class="head-actions">{switch}<div class="seg" role="tablist"><input type="radio" name="host" id="use-gh" checked><label for="use-gh">{} GitHub Actions</label><input type="radio" name="host" id="use-gl"><label for="use-gl">{} GitLab CI</label></div></div></div><ol class="use-steps gh">{gh_steps}</ol><ol class="use-steps gl">{gl_steps}</ol></section>"#,
                logo("github", 16), logo("gitlab", 16)),
            (true, false) => format!(r#"<section class="card"><div class="card-head"><h2>{} In GitHub Actions</h2>{switch}</div><ol class="use-steps gh only">{gh_steps}</ol><p class="note">Using GitLab too? <a href="/?p=repos">Connect it on Repositories</a>.</p></section>"#, logo("github", 18)),
            (false, true) => format!(r#"<section class="card"><div class="card-head"><h2>{} In GitLab CI</h2>{switch}</div><ol class="use-steps gl only">{gl_steps}</ol><p class="note">Using GitHub too? <a href="/?p=repos">Connect it on Repositories</a>.</p></section>"#, logo("gitlab", 18)),
            (false, false) => format!(r#"<section class="card">{}</section>"#, empty_state(&icon("play"), "Connect GitHub or GitLab first", "Then this page shows how its workflows use your runners.", r#"<a class="button primary" href="/?p=repos">Repositories</a>"#)),
        };
        format!(r#"{bar}<div class="cards">{default_card}{usage}</div>{picker}{prompt}"#)
    }

    /// Every control plane known here, each with what it is called, the organizations it serves, where it lives and
    /// whether its own account is signed in to, and one button to its pages. Above them, setting up a new one.
    fn all_page(&self, views: &[PlaneView]) -> String {
        let own = self.own();
        // AWS: signed in to its account, whichever account is in hand. (One only seen from afar is not listed here.)
        let signed_in = |v: &PlaneView| match &v.plane {
            Plane::Aws { account_id, .. } => self.aws.iter().chain(&self.aws_others).any(|s| &s.account_id == account_id),
            Plane::Cloudflare { .. } => own.check(&v.plane).is_ok() && self.cf.is_some(),
            _ => own.check(&v.plane).is_ok() && self.modal.is_some(),
        };
        let open = |v: &PlaneView, label: &str| format!(r#"<a class="button secondary sm" href="/?c={}">{label}</a>"#, esc(v.plane.plane_id()));
        let mut rows = String::new();
        for v in views.iter().filter(|v| !matches!(v.plane, Plane::Seen { .. })) {
            let place = esc(&v.plane.place());
            let (title, sub, end) = if !v.online {
                (place.clone(), "Not answering".to_string(), format!("{}{}", pill("bad", "not answering"), open(v, "Open")))
            } else if let Some(to) = v.moved_to.as_ref() {
                (place.clone(), format!("Moved to {}", esc(to.trim_start_matches("https://"))), open(v, "Open"))
            } else if v.serves_its_own() {
                let state = if signed_in(v) { pill("good", "Signed in") } else { format!("{}{}", pill("open", "Not signed in"), signin_button(v.plane.cloud(), "Sign in", false)) };
                (esc(&v.name()), format!("{} · {place}", esc(&serves(v))), format!("{state}{}", open(v, "Open")))
            } else {
                ("A new control plane".to_string(), format!("Serves nothing yet · {place}"), open(v, "Open"))
            };
            rows.push_str(&format!(r#"<div class="pick act">{}<span><strong>{title}</strong><small>{sub}</small></span><span class="pick-end">{end}</span></div>"#, logo(v.plane.cloud(), 36)));
        }
        // One sentence says what the two words mean; organizations are added on a control plane's own Repositories page.
        format!(r#"<div class="bar"><h1 class="page-title">Control planes</h1><a class="button primary sm" href="/?p=plane&c=">New control plane</a></div><p class="note" style="margin:-4px 0 18px">A control plane is one CI: its own cloud account, name, runners and limits. Each runs the jobs of one or more organizations; add an organization on its Repositories page.</p><div class="cards"><section class="card"><div class="picks">{rows}</div></section></div>"#)
    }

    /// Settings: what the control plane is called and answers to, its limits, and this workspace on this computer
    /// (its name, SuperCI's sign-ins, and forgetting it when there are others).
    fn settings_page(&self, v: Option<&PlaneView>) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">Settings</h1></div>"#;
        let logo = |icon: &str| format!(r#"<span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{icon}</span>"#);
        let close = r#"<button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>"#;
        let who = self.signed_in_as().map(|w| format!("Signed in: {}.", esc(&w))).unwrap_or_else(|| "Not signed in to any cloud.".into());
        let mut rows = vec![];
        let mut dialogs = String::new();
        // The workspace: a folder of SuperCI's on this computer (none: nothing is kept here, and there is no other).
        let root = self.store.as_ref().and_then(|s| s.root());
        let name = esc(&self.workspace());
        let called = esc(&match self.called_as(v) { n if n == "SuperCI" => self.workspace(), n => n });
        if root.is_some() {
            rows.push(format!(r#"<div class="default-machine">{}<span class="dm-main"><strong>Workspace</strong><small>Its id: what <code>--workspace</code> takes in commands.</small></span><span class="dm-spec"><span class="lim">{name}</span></span><button type="button" class="button secondary sm" onclick="document.getElementById('rename-workspace').showModal()">Change</button></div>"#, logo(ICON_WORKSPACE)));
            dialogs.push_str(&format!(r#"<dialog class="dlg" id="rename-workspace" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Workspace id</strong><small>What <code>--workspace</code> takes in commands.</small></span>{close}</div><form method="post" action="/workspaces/rename" class="limit-do"><input type="hidden" name="w" value="{name}"><input name="id" value="{name}" aria-label="Its id" autofocus onfocus="this.select()" pattern="[a-z][a-z0-9\-]{{1,23}}" title="2 to 24 lowercase letters, digits and dashes" required spellcheck="false" autocomplete="off" autocapitalize="none"><button class="button primary sm">Save</button></form></div></dialog>"#, logo(ICON_WORKSPACE)));
        }
        // (Nothing to sign out of in a workspace that is signed in nowhere.)
        if self.signed_in_as().is_some() || self.aws_ended { rows.push(format!(r#"<div class="default-machine">{}<span class="dm-main"><strong>This device</strong><small>{who} Signing out removes this workspace's sign-ins from it; your runners keep working.</small></span><form method="post" action="/signout"><button class="button secondary sm">Sign out</button></form></div>"#, logo(ICON_SIGNOUT))) }
        // With other workspaces beside it, this one can be forgotten here.
        if root.is_some_and(|r| crate::store::listed(&r, Some(&self.workspace())).len() > 1) {
            rows.push(format!(r#"<div class="default-machine">{}<span class="dm-main"><strong>Remove this workspace</strong><small>Forgets {called} on this computer. Nothing in its clouds is deleted.</small></span><button type="button" class="button danger sm" onclick="document.getElementById('remove-workspace').showModal()">Remove</button></div>"#, logo(ICON_REMOVE)));
            dialogs.push_str(&format!(r#"<dialog class="dlg" id="remove-workspace" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Remove {called}?</strong><small>Its sign-ins are forgotten on this computer. Its control plane and runners keep working: nothing in its clouds is deleted.</small></span>{close}</div><form method="post" action="/workspaces/remove" class="limit-do"><input type="hidden" name="w" value="{name}"><button class="button danger sm">Remove from this computer</button></form></div></dialog>"#, logo(ICON_REMOVE)));
        }
        let here = format!(r#"<section class="card dm-card">{}</section>{dialogs}"#, rows.concat());
        let Some(v) = v else { return format!(r#"{bar}<div class="cards">{here}</div>"#) };
        // The public repositories GitHub's App is installed on (none when GitHub is not connected).
        let public = if v.github { cloudflare::plane_get(v.plane.url(), &self.status_key, "/github/public-repos").ok().and_then(|r| r["repos"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())) } else { Some(vec![]) };
        // What the cache keeps (a control plane from before has none to say).
        let caches = if v.aws { cloudflare::plane_get(v.plane.url(), &self.status_key, "/caches").ok() } else { None };
        let keys = cloudflare::plane_get(v.plane.url(), &self.status_key, "/keys").ok();
        // The secrets it keeps for runs from a computer (a control plane from before keeps none).
        let secrets = if view::older(v.version.as_deref(), "0.19.13") { None } else { cloudflare::plane_get(v.plane.url(), &self.status_key, "/secrets").ok() };
        format!(r#"{bar}<div class="cards">{}{}{}{}{}{here}</div>"#, called_card(v), limits_card(v, public), cache_card(v, caches.as_ref()), secrets_card(secrets.as_ref()), self.keys_card(keys.as_ref()))
    }

    /// The keys this CI takes: each computer's that looks after it (which one, when it was made and last used) and
    /// those that only read or start runs, each with Remove (this computer's own is taken out by signing out). Nothing
    /// from a control plane from before that cannot say.
    fn keys_card(&self, keys: Option<&serde_json::Value>) -> String {
        let Some(k) = keys else { return String::new() };
        let ago = |ms: Option<u64>| ms.map(|t| ago_words(now_ms().saturating_sub(t))).unwrap_or_else(|| "not yet".into());
        let left = |until: Option<u64>| until.map(|u| match u.saturating_sub(now_ms()) / 86_400_000 { 0 => "ends today".to_string(), 1 => "1 more day".into(), d => format!("{d} more days") }).unwrap_or_default();
        let mut rows = String::new();
        for c in k["devices"].as_array().into_iter().flatten() {
            let name = c["name"].as_str().unwrap_or_default();
            let mine = name == self.status_secret;
            let who = c["device"].as_str().filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| "A device".into());
            let made = c["until_ms"].as_u64().map(|u| ago_words(now_ms().saturating_sub(u.saturating_sub(30 * 86_400_000)))).unwrap_or_default();
            let end = if mine { pill("good", "This device") } else { format!(r#"<form method="post" action="/keys/remove" onsubmit="return confirm('Remove this device key? Its dashboard stops reaching this CI until it opens again with its sign-in.')"><input type="hidden" name="name" value="{}"><button class="button secondary sm">Remove</button></form>"#, esc(name)) };
            rows.push_str(&format!(r#"<div class="limit"><div class="limit-text"><strong>{}</strong><small>Device key · made {made} · last used {}</small></div><div class="limit-do">{end}</div></div>"#, esc(&who), ago(c["last_ms"].as_u64())));
        }
        for r in k["read"].as_array().into_iter().flatten() {
            let name = r["name"].as_str().unwrap_or_default();
            rows.push_str(&format!(r#"<div class="limit"><div class="limit-text"><strong>{}</strong><small>{} · {}</small></div><div class="limit-do"><form method="post" action="/keys/remove"><input type="hidden" name="read" value="{}"><button class="button secondary sm">Remove</button></form></div></div>"#,
                esc(&name.to_lowercase()), if r["runs"] == true { "Run key: reads and starts runs from a computer" } else { "Read key: reads only" }, left(r["until_ms"].as_u64()), esc(name)));
        }
        format!(r#"<section class="card"><div class="card-head"><h2>Keys</h2><span class="card-sub">Who reaches this CI: each device's key, and read and run keys made for coding agents and scripts. Anyone signed in to its cloud account can add their own.</span></div><div class="limits">{rows}</div></section>"#)
    }

    /// One run's page: how it stands, how long it took (its first job queued to its last over), what its machines cost,
    /// the job it waited for last, and its jobs here (each opening its own page). Its page at GitHub or GitLab is a
    /// line of it; a run from a computer has none.
    fn run_page(&self, v: Option<&PlaneView>) -> String {
        let back = r#"<a class="chip" href="/?p=runs">‹ Runs</a>"#;
        let Some(v) = v else { return format!(r#"<div class="bar"><h1 class="page-title">Run</h1>{back}</div>"#) };
        let (repo, id, attempt) = self.run_shown.clone();
        let jobs: Vec<serde_json::Value> = v.jobs().iter().map(|j| with_hosts(v, j)).collect();
        let runs = runs_of(&jobs);
        let found = runs.into_iter().find(|r| r["repo"].as_str() == Some(repo.as_str()) && r["id"].as_u64().is_some_and(|n| n.to_string() == id) && r["attempt"].as_u64().unwrap_or(0).to_string() == if attempt.is_empty() { "0" } else { attempt.as_str() });
        let Some(r) = found else {
            return format!(r#"<div class="bar"><h1 class="page-title">Run</h1>{back}</div><div class="cards">{}</div>"#, empty_state(&icon("play"), "Not among the latest runs", "The control plane keeps the latest jobs; this run's are not among them.", ""))
        };
        let (title, sub) = run_names(&r);
        let (kind, word) = match r["state"].as_str().unwrap_or_default() { "passed" => ("good", "Passed"), "failed" => ("bad", "Failed"), "cancelled" => ("", "Cancelled"), "running" => ("accent", "Running"), _ => ("good", "Done") };
        let ids: Vec<u64> = r["job_ids"].as_array().into_iter().flatten().filter_map(|i| i.as_u64()).collect();
        let mut its: Vec<serde_json::Value> = jobs.into_iter().filter(|j| j["repo"] == r["repo"] && j["run_id"] == r["id"] && j["job_id"].as_u64().is_some_and(|i| ids.contains(&i))
            && j["attempt"].as_u64().filter(|a| *a > 0) == r["attempt"].as_u64()).collect();
        its.sort_by_key(|j| j["at_ms"].as_u64().unwrap_or_default());
        let link = r["link"].as_str().unwrap_or_default().to_string();
        let at = if link.is_empty() { r#"<small class="faint">Run from a computer: it has no page at GitHub.</small>"#.to_string() }
            else { format!(r#"<a class="button secondary sm" href="{}" target="_blank" rel="noopener">{}On {} ›</a>"#, esc(&link), crate::logos::bare(if r["provider"] == "gitlab" { "gitlab" } else { "github" }, 15), if r["provider"] == "gitlab" { "GitLab" } else { "GitHub" }) };
        let row = |label: &str, value: String| if value.is_empty() { String::new() } else { format!(r#"<div class="limit"><span class="limit-text"><strong>{label}</strong></span><span class="limit-do">{value}</span></div>"#) };
        let facts = [
            row("Status", format!(r#"<span class="jt-state {kind}"><i></i>{word}</span>"#)),
            row("Jobs here", its.len().to_string()),
            row("Time", r["took_ms"].as_u64().map(span).unwrap_or_default()),
            row("Cost", r["usd"].as_f64().map(usd).unwrap_or_default()),
            row("Last to end", esc(run_last(&r).trim_start_matches("Last to end: "))),
        ].concat();
        let going = kind == "accent";
        // While it runs: Cancel, as GitHub's does (a run of GitHub's, cancelled by GitHub; one from a computer, here).
        let at = if going && r["provider"] != "gitlab" {
            format!(r#"{at}<form method="post" action="/run/cancel"><input type="hidden" name="id" value="{}"><input type="hidden" name="repo" value="{}"><input type="hidden" name="attempt" value="{}"><button class="button secondary sm">Cancel run</button></form>"#,
                r["id"], esc(r["repo"].as_str().unwrap_or_default()), r["attempt"].as_u64().unwrap_or(0))
        } else { at };
        // A run from a computer's artifacts (a run of GitHub's keeps its own at GitHub), each a download.
        let artifacts = if !link.is_empty() { String::new() } else {
            let listed = cloudflare::plane_get(v.plane.url(), &self.status_key, &format!("/runs/{}/artifacts", r["id"])).ok();
            let rows: String = listed.as_ref().and_then(|l| l["artifacts"].as_array()).into_iter().flatten().map(|a| {
                let name = a["name"].as_str().unwrap_or_default();
                format!(r#"<div class="limit"><span class="limit-text"><strong>{}</strong><small>{}</small></span><span class="limit-do"><a class="chip" href="/run/artifact?run={}&amp;name={}">Download</a></span></div>"#,
                    esc(name), size_words(a["size"].as_u64().unwrap_or(0)), r["id"], url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>())
            }).collect();
            if rows.is_empty() { String::new() } else { format!(r#"<section class="card"><div class="card-head"><h2>Its artifacts</h2><span class="card-sub">Kept a day</span></div><div class="limits">{rows}</div></section>"#) }
        };
        format!(r#"{}<div class="bar"><h1 class="page-title">{}</h1>{back}</div><div class="cards"><section class="card"><div class="card-head"><h2>{}</h2>{at}</div><div class="limits">{facts}</div></section>{artifacts}<section class="card"><div class="card-head"><h2>Its jobs</h2></div>{}<p class="note" style="margin:10px 8px 0">Its time is from its first job queued here to its last one over; jobs of it on other runners are not counted.</p></section></div>"#,
            if going { r#"<div data-refresh="4"></div>"# } else { "" }, esc(&title), esc(&sub), job_table(&its))
    }

    /// One job's page: what it is and how it stands, its machine and cost, how its start went, and the end of its log
    /// (looked at again every few seconds while it runs). Its page at GitHub or GitLab is a line of it; a run from a
    /// computer has none, so this is where it is followed.
    fn job_page(&self, v: Option<&PlaneView>) -> String {
        let back = r#"<a class="chip" href="/?p=jobs">‹ Jobs</a>"#;
        let Some(v) = v else { return format!(r#"<div class="bar"><h1 class="page-title">Job</h1>{back}</div>"#) };
        let (id, gl) = self.job_shown.clone();
        let found = v.jobs().into_iter().find(|j| j["job_id"].as_u64().is_some_and(|n| n.to_string() == id) && (gl.is_empty() || j["gitlab"].as_str() == Some(gl.as_str())));
        let Some(j) = found.map(|j| with_hosts(v, &j)) else {
            return format!(r#"<div class="bar"><h1 class="page-title">Job</h1>{back}</div><div class="cards">{}</div>"#, empty_state(&icon("jobs"), "Not among the latest jobs", "The control plane keeps the latest jobs; this one is not among them.", ""))
        };
        let (title, sub) = job_names(&j);
        let (kind, word) = job_ended(&j);
        let going = ["launching", "launched", "running", "waiting"].contains(&j["state"].as_str().unwrap_or_default());
        let (runner_logo, runner) = runner_text(&j);
        let row = |label: &str, value: String| if value.is_empty() { String::new() } else { format!(r#"<div class="limit"><span class="limit-text"><strong>{label}</strong></span><span class="limit-do">{value}</span></div>"#) };
        let link = job_link(&j);
        let at = if link.is_empty() { r#"<small class="faint">Run from a computer: it has no page at GitHub.</small>"#.to_string() }
            else { format!(r#"<a class="button secondary sm" href="{}" target="_blank" rel="noopener">{}On {} ›</a>"#, esc(&link), crate::logos::bare(if j["provider"] == "gitlab" { "gitlab" } else { "github" }, 15), if j["provider"] == "gitlab" { "GitLab" } else { "GitHub" }) };
        let why = { let w = plain_reason(&j); if (kind == "bad" || j["state"] == "waiting") && !w.is_empty() { format!(r#"<small class="jt-why {kind}">{}</small>"#, esc(&w)) } else { String::new() } };
        // The run it is of: its page here, named as the Runs tab names it.
        let jobs: Vec<serde_json::Value> = v.jobs().iter().map(|x| with_hosts(v, x)).collect();
        let run = runs_of(&jobs).into_iter().find(|r| r["repo"] == j["repo"] && r["id"] == j["run_id"] && r["job_ids"].as_array().is_some_and(|ids| ids.contains(&j["job_id"])));
        let run = run.map(|r| { let n = r["jobs"].as_u64().unwrap_or(1); format!(r#"<a href="{}">{}</a><span class="faint">{n} {}</span>"#, esc(&run_page_link(&r)), esc(&run_names(&r).0), if n == 1 { "job" } else { "jobs" }) }).unwrap_or_default();
        let facts = [
            row("Status", format!(r#"<span class="jt-state {kind}"><i></i>{word}</span>"#)),
            row("Run", run),
            // An AWS machine: what it is, how it was bought (spot, or on-demand and why), and where it ran.
            match aws_machine(&j) {
                Some((machine, market, place)) => [row("Machine", format!("{}<span>{}</span>", logo("aws", 16), esc(&machine))), row("Spot or on-demand", esc(&market)), row("Region", esc(&place))].concat(),
                None => row("Runner", if runner.is_empty() { String::new() } else { format!("{}<span>{}</span>", logo(&runner_logo, 16), esc(&runner)) }),
            },
            row("Time", duration(&j)),
            row("Cost", job_cost(&j).filter(|c| *c > 0.0).map(|c| cost_text(&j, c)).unwrap_or_default()),
            row("Queued", ago(j["at_ms"].as_u64().unwrap_or_default())),
            row("Docker images", images_said(&j).map(|s| esc(&s)).unwrap_or_default()),
        ].concat();
        let lines = job_start_lines(&j);
        let start = lines.iter().enumerate().map(|(i, l)| { let l = l.trim(); let l = if i == 0 { l.strip_prefix("Its start: ").or_else(|| l.strip_prefix("Its start, so far:")).unwrap_or(l) } else { l }; l.to_string() })
            .filter(|l| !l.is_empty()).map(|l| format!("<li>{}</li>", esc(&l))).collect::<String>();
        let start = if start.is_empty() { String::new() } else { format!(r#"<section class="card"><div class="card-head"><h2>Its start</h2></div><ul class="steps">{start}</ul></section>"#) };
        // Its log: a part of the page of its own, kept as it is when the page is read again (its scroll, the steps
        // opened), that reads its log itself while the job goes on (see `log_view` and `superciLog`).
        let gl = if j["provider"] == "gitlab" { j["gitlab"].as_str().unwrap_or_default().to_string() } else { String::new() };
        let host = match j["provider"].as_str().unwrap_or_default() { "gitlab" => "gitlab", "local" => "local", _ => "github" };
        let log = format!(r#"<section class="card log-card" data-keep="log-{id}-{gl}" data-view="/job/view?id={id}&amp;gl={gl}" data-host="{host}"><div class="card-head"><h2>Its log</h2><span class="card-sub" data-log-sub></span><div class="head-actions log-tools"><input class="log-search" type="search" placeholder="Search" aria-label="Search the log" data-log-search><button type="button" class="chip" data-log-times>Times</button><a class="chip" href="/job/raw?id={id}&amp;gl={gl}">Raw log</a></div></div><div class="log-steps" data-log-steps><p class="note"><span class="mini-spin"></span> Reading its log…</p></div></section>"#);
        format!(r#"{}<div class="bar"><h1 class="page-title">{}</h1>{back}</div><div class="cards"><section class="card"><div class="card-head"><h2>{}</h2>{at}</div>{why}<div class="limits">{facts}</div></section>{start}{log}</div>"#,
            if going { r#"<div data-refresh="4"></div>"# } else { "" }, esc(&title), esc(&sub))
    }

    /// GitLab's settings: the projects whose jobs run here (each turned on or off, or all at once), and the connection.
    fn gitlab_page(&self, v: Option<&PlaneView>) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">GitLab</h1><a class="chip" href="/?p=repos">Back to repositories</a></div>"#;
        // The connection asked for (by its name; nothing: the first).
        let which = self.gitlab_shown.clone();
        let Some(url) = v.and_then(|v| v.gitlabs().into_iter().find(|(id, ..)| *id == which)).map(|(_, url, _)| url) else { return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&logo("gitlab", 40), "GitLab is not connected", "Connect it on Repositories.", r##"<a class="button primary" href="/?p=repos#gitlab">Repositories</a>"##)) };
        let v = v.unwrap();
        let host = url.trim_start_matches("https://").to_string();
        let bar = if v.gitlabs().len() > 1 { bar.replace(">GitLab</h1>", &format!(">GitLab · {}</h1>", esc(&host))) } else { bar.to_string() };
        let hidden = format!(r#"<input type="hidden" name="gitlab" value="{}">"#, esc(&which));
        let answer = cloudflare::plane_get(v.plane.url(), &self.status_key, &if which.is_empty() { "/gitlab/projects".to_string() } else { format!("/gitlab/projects?g={which}") });
        let last_event = answer.as_ref().ok().and_then(|a| a["last_event_ms"].as_u64());
        let list = answer.map(|p| p["projects"].as_array().cloned().unwrap_or_default());
        let on = list.as_ref().map(|l| l.iter().filter(|p| p["enabled"] == true).count()).unwrap_or(0);
        let body = match list {
            Err(e) => empty_state(&logo("gitlab", 40), "GitLab did not answer", &esc(&e), r#"<a class="button" href="">Try again</a>"#),
            Ok(l) if l.is_empty() => empty_state(&logo("gitlab", 40), "No projects you maintain", "Turning a project on adds a webhook to it, which needs the Maintainer role.", ""),
            Ok(l) => format!(r#"<div class="rows">{}</div>"#, l.iter().map(|p| {
                let enabled = p["enabled"] == true;
                // GitLab pauses a webhook after failed deliveries, and disables it for good after many.
                let (dot, sub) = match (enabled, p["hook_status"].as_str()) {
                    (true, Some("temporarily_disabled")) => ("open", format!("GitLab paused its webhook after failed deliveries{}", p["hook_paused_until"].as_str().map(|u| format!(", until {}", u.get(..16).unwrap_or(u).replace('T', " "))).unwrap_or_default())),
                    (true, Some("disabled")) => ("bad", "GitLab disabled its webhook after failed deliveries: turn it off and on again".to_string()),
                    (true, _) => ("good", "Its jobs with your label run here".to_string()),
                    _ => ("", String::new()),
                };
                row(dot, &esc(p["path"].as_str().unwrap_or_default()), &esc(&sub),
                    &format!(r#"<form method="post" action="/gitlab/project">{hidden}<input type="hidden" name="id" value="{}"><input type="hidden" name="enabled" value="{}"><button class="button {} sm">{}</button></form>"#,
                        p["id"].as_u64().unwrap_or_default(), !enabled, if enabled { "secondary" } else { "primary" }, if enabled { "Turn off" } else { "Turn on" }))
            }).collect::<String>()),
        };
        let heard = match last_event { Some(t) => format!("Last event from GitLab {}", ago(t)), None => "No events from GitLab yet: they come once a turned-on project runs a pipeline".to_string() };
        let projects = format!(r#"<section class="card"><div class="card-head"><h2>Projects</h2><form method="post" action="/gitlab/project">{hidden}<input type="hidden" name="all" value="on"><button class="chip">Turn on all</button></form></div><p class="note">Turning a project on adds a webhook for its job events; nothing else in it changes. GitHub's App covers the repositories picked when it was installed; GitLab sends job events per project (group webhooks are a paid GitLab feature), so each is turned on here.</p>{body}<div class="card-foot"><span class="note">{on} on · {heard}</span></div></section>"#);
        let connection = format!(r#"<section class="card"><div class="card-head"><h2>Connection</h2></div><div class="picks"><div class="pick act">{}<span><strong>{}</strong><small>A token in your control plane's secrets (scopes api, create_runner, manage_runner). Disconnecting removes the webhooks here, then the token.</small></span><span class="pick-end"><button type="button" class="button secondary sm" onclick="document.getElementById('gl-token').showModal()">New token</button><form method="post" action="/gitlab/disconnect">{hidden}<button class="button danger sm">Disconnect</button></form></span></div></div>{}</section>"#,
            logo("gitlab", 36), esc(&host), gitlab_token_dialog(&which, &url));
        format!(r#"{bar}<div class="cards">{projects}{connection}</div>"#)
    }

    /// What a control plane's AWS role lacks of what this version asks for, read now (`superci planes allow --dry-run`).
    pub fn role_lacks(&mut self, plane_id: &str) -> Result<Vec<&'static superci_core::permissions::Need>> {
        let p = self.planes.iter().find(|p| p.plane_id() == plane_id).cloned().ok_or_else(|| format!("No control plane {plane_id} here (ids are in `superci planes list`)."))?;
        let v = view::plane_view(&p, Some(&self.status_key));
        let account = aws_account(&v).ok_or("Its machines do not run in an AWS account: it has no AWS role to give anything.")?;
        let (account, creds) = self.aws_account(&account)?;
        let regions = v.status.as_ref().map(status_regions).unwrap_or_else(|| v.aws_regions());
        aws::runner_role_missing(&creds, &account, plane_id, &regions)
    }

    /// What a control plane's AWS role was found to lack (empty unless read while signed in to its account).
    fn aws_lacks(&self, v: &PlaneView) -> Vec<&'static superci_core::permissions::Need> {
        match lock(&self.aws_missing).get(v.plane.plane_id()) { Some((Ok(m), _)) => m.clone(), _ => vec![] }
    }

    /// Signed in to AWS: each control plane's runner role in that account is read (in the background, every five
    /// minutes) and compared with what this version asks for. Nothing is changed here: the Control plane page shows
    /// what is missing, to give with Review (or with an update).
    fn check_permissions(&mut self) {
        let Some(account) = self.aws.as_ref().map(|a| a.account_id.clone()) else { return };
        let Some((views, _)) = self.views.as_ref() else { return };
        let now = now_ms();
        let known = lock(&self.aws_missing).clone();
        // Read with credentials AWS had ended: the sign-in is seen to, and the reading is not shown as what was read.
        if known.values().any(|(read, _)| read.as_ref().is_err_and(|e| aws::expired(e))) {
            lock(&self.aws_missing).retain(|_, (read, _)| !read.as_ref().is_err_and(|e| aws::expired(e)));
            return self.aws_cut_short()
        }
        let due: Vec<(String, Vec<String>)> = views.iter().filter(|v| aws_account(v).as_deref() == Some(account.as_str()))
            .map(|v| (v.plane.plane_id().to_string(), v.status.as_ref().map(status_regions).unwrap_or_else(|| v.aws_regions()))).filter(|(id, _)| known.get(id).is_none_or(|(_, at)| now > at + 300_000)).collect();
        if due.is_empty() { return }
        let Ok(creds) = self.aws_creds() else { return };
        let store = self.aws_missing.clone();
        // Marked as being read (an empty error), so it is not asked again meanwhile; what was read before stays shown.
        due.iter().for_each(|(id, _)| lock(&store).entry(id.clone()).or_insert((Err(String::new()), now)).1 = now);
        std::thread::spawn(move || due.into_iter().for_each(|(id, regions)| {
            let missing = aws::runner_role_missing(&creds, &account, &id, &regions);
            lock(&store).insert(id, (missing, now_ms()));
        }));
    }

    /// Finished Cloudflare jobs' costs as Cloudflare metered them (its usage analytics, by each container's Durable Object
    /// id), read in the background while signed in to Cloudflare, at most every two minutes. Each is kept with its job
    /// in the control plane (budgets count it) and shown at once.
    fn measure(shared: &Arc<Mutex<Dashboard>>, d: &mut Dashboard) {
        if d.measuring || now_ms() < d.measured_at + 120_000 || (d.cf_accounts.is_empty() && d.modal.is_none()) { return }
        let Some(views) = d.views.as_ref().map(|(v, _)| v.clone()) else { return };
        let now = now_ms();
        // (plane URL, job key, container id, from, to, GiB-seconds it ran at least) of finished jobs not yet measured.
        // Cloudflare's numbers come within minutes, but not always all of them: a reading is kept only once it covers
        // at least the job's own running time at the container's memory, else it is read again later (two days at most).
        let todo: Vec<(String, String, String, u64, u64, f64)> = views.iter().flat_map(|v| {
            let jobs = v.status.as_ref().and_then(|s| s["jobs"].as_array().cloned()).unwrap_or_default();
            let url = v.plane.url().to_string();
            jobs.into_iter().filter_map(move |j| {
                let id = j["machine_id"].as_str().filter(|i| i.len() == 64 && i.bytes().all(|b| b.is_ascii_hexdigit()))?.to_string();
                let end = j["ended_ms"].as_u64().filter(|e| now > e + 180_000 && now < e + 2 * 86_400_000)?;
                if j["cloud"] != "cloudflare" || j["cost_from"] == "measured" { return None }
                let key = if j["provider"] == "gitlab" { format!("job:gl{}", j["job_id"]) } else { format!("job:{}", j["job_id"]) };
                // Its container's memory ("4cpu-12gb"), over the time its job ran.
                let gb = container_gb(j["machine_type"].as_str().unwrap_or_default());
                let ran = j["started_ms"].as_u64().map(|s| end.saturating_sub(s) as f64 / 1000.0).unwrap_or(0.0);
                Some((url.clone(), key, id, j["launched_ms"].as_u64().or(j["at_ms"].as_u64()).unwrap_or(end), end, gb * ran * 0.9))
            })
        }).collect();
        let cf = if d.cf_accounts.is_empty() { None } else { d.cf_client().ok() };
        let (accounts, key, shared, modal) = (d.cf_accounts.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), d.status_key.clone(), shared.clone(), d.modal.clone());
        d.measuring = true;
        std::thread::spawn(move || {
            let mut costs = std::collections::HashMap::new();
            let mut cf_month = None;
            if let Some(cf) = &cf {
                for chunk in todo.chunks(100) {
                    let ids: Vec<String> = chunk.iter().map(|t| t.2.clone()).collect();
                    let (from, to) = (chunk.iter().map(|t| t.3).min().unwrap_or(now) - 60_000, chunk.iter().map(|t| t.4).max().unwrap_or(now) + 600_000);
                    costs.extend(accounts.iter().filter_map(|a| cf.container_costs(a, &ids, from, to).ok()).flatten());
                }
                let months: Vec<(f64, f64)> = accounts.iter().filter_map(|a| cf.containers_month(a, now).ok()).collect();
                if !months.is_empty() { cf_month = Some(months.iter().fold((0.0, 0.0), |s, m| (s.0 + m.0, s.1 + m.1))) }
            }
            let modal_month = modal.as_ref().and_then(|m| modal::month(m).ok());
            // Per control plane: its jobs' measured costs.
            let mut by_plane: std::collections::HashMap<String, serde_json::Map<String, serde_json::Value>> = Default::default();
            for (url, job, id, _, _, least) in &todo {
                if let Some((usd, gib_s)) = costs.get(id).filter(|(_, gib_s)| *gib_s >= *least && *gib_s > 0.0) { let _ = gib_s; by_plane.entry(url.clone()).or_default().insert(job.clone(), (*usd).into()); }
            }
            for (url, measured) in &by_plane { let _ = cloudflare::plane_post(url, &key, "/costs", &serde_json::json!({ "measured": measured })); }
            let mut d = lock(&shared);
            d.measuring = false;
            d.measured_at = now_ms();
            if cf_month.is_some() { d.cf_month = cf_month }
            if modal_month.is_some() { d.modal_month = modal_month }
            if let Some((views, _)) = d.views.as_mut() {
                for v in views.iter_mut() {
                    let Some(measured) = by_plane.get(v.plane.url()) else { continue };
                    v.status.as_mut().and_then(|s| s["jobs"].as_array_mut()).into_iter().flatten().for_each(|j| {
                        let key = if j["provider"] == "gitlab" { format!("job:gl{}", j["job_id"]) } else { format!("job:{}", j["job_id"]) };
                        if let Some(usd) = measured.get(&key) { j["cost_usd"] = usd.clone(); j["cost_from"] = "measured".into(); }
                    });
                }
            }
        });
    }

    /// Keeps each control plane's status; one that did not answer in time shows the last it gave.
    fn remember(&mut self, mut views: Vec<PlaneView>) -> Vec<PlaneView> {
        for v in views.iter_mut() {
            let id = v.plane.plane_id().to_string();
            // Not answering just now, but it did a moment ago: its last view stands (for a minute).
            if !v.online {
                if let Some((good, at)) = self.last_good.get(&id).filter(|(_, at)| now_ms() < at + last_good_ms()) { let _ = at; *v = good.clone(); }
            } else if v.status.is_some() {
                self.last_good.insert(id.clone(), (v.clone(), now_ms()));
            }
            self.first_asked.entry(id.clone()).or_insert_with(now_ms);
            if v.online { self.answered.insert(id); }
            match &v.status { Some(st) => { self.seen.insert(v.plane.plane_id().to_string(), st.clone()); } None if v.online => v.status = self.seen.get(v.plane.plane_id()).cloned(), None => {} }
        }
        views
    }

    /// Deletes a control plane from its cloud, and the launchers and AWS role made for it in other clouds (those
    /// this session is signed in to). Returns what could not be removed, in words.
    fn delete_plane(&mut self, plane: &Plane, status: &serde_json::Value) -> Result<Vec<String>> {
        self.own().check(plane)?;
        let mut left = vec![];
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.delete_plane(account_id, script, plane.plane_id())?,
            Plane::Aws { region, plane_id, account_id, .. } => {
                let creds = self.aws_creds()?;
                // (Its budget in AWS itself first, if one was made: its rule comes off the role before the role goes.)
                if !status["routing"]["aws_budget"].is_null() { if let Err(e) = crate::aws_budget::with(&creds, account_id, plane_id, |b| b.remove()) { left.push(format!("Its budget in AWS ({e}).")) } }
                aws_plane::delete(&creds, region, plane_id)?;
                if let Err(e) = aws::delete_networks(&creds, plane_id, &REGIONS) { left.push(format!("Its machines' network in AWS ({e}).")) }
            }
            Plane::Modal { plane_id, .. } => modal::delete_plane(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id)?,
            Plane::Seen { .. } => return Err("Sign in where this control plane runs to delete it.".into()),
        }
        let launcher = |c: &str| status["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c));
        if launcher("cloudflare") && !matches!(plane, Plane::Cloudflare { .. }) {
            match self.cloudflare() { Ok(cf) => for (a, _) in self.cf_accounts.clone() { let _ = cf.delete_runners(&a, plane.plane_id()); }, Err(_) => left.push("Its Cloudflare launcher (sign in with Cloudflare to remove it).".to_string()) }
        }
        if launcher("modal") && !matches!(plane, Plane::Modal { .. }) {
            match &self.modal { Some(m) => { let _ = modal::delete_runners(m, plane.plane_id()); } None => left.push("Its Modal launcher (sign in with Modal to remove it).".into()) }
        }
        // The launchers in further Modal workspaces, each with its own sign-in.
        for c in status["launchers"].as_array().into_iter().flatten().filter_map(|a| a["cloud"].as_str()).filter(|c| c.starts_with("modal:")) {
            match self.modal_for(c) { Some(m) => { let _ = modal::delete_runners(m, plane.plane_id()); } None => left.push(format!("Its launcher in the Modal workspace {} (sign in there to remove it).", &c["modal:".len()..])) }
        }
        if let (Some(account), false) = (status["aws"]["account_id"].as_str(), matches!(plane, Plane::Aws { .. })) {
            match (&mut self.aws, account) {
                (Some(a), acc) if a.account_id == acc => {
                    let creds = a.credentials()?;
                    if let Err(e) = aws::delete_networks(&creds, plane.plane_id(), &REGIONS) { left.push(format!("Its machines' network in AWS account {acc} ({e}).")) }
                    if !status["routing"]["aws_budget"].is_null() { if let Err(e) = crate::aws_budget::with(&creds, acc, plane.plane_id(), |b| b.remove()) { left.push(format!("Its budget in AWS account {acc} ({e}).")) } }
                    let _ = aws::disconnect_runners(&creds, acc, plane.url(), plane.plane_id());
                }
                _ => left.push(format!("Its role in AWS account {account} (sign in with AWS to remove it).")),
            }
        }
        self.planes.retain(|p| p.plane_id() != plane.plane_id());
        self.seen.remove(plane.plane_id());
        self.keyed.remove(plane.plane_id());
        self.selected = 0;
        Ok(left)
    }

    /// The runner providers of the control plane in use (`from`), and how each comes along to `to`.
    fn carry_plan(&self, from: &PlaneView, to: &PlaneView) -> Vec<Carry> {
        let st = from.status.clone().unwrap_or_default();
        let launcher = |c: &str| st["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c));
        let own = |c: &str| st["containers"] == true && st["own_cloud"] == c;
        let mut plan = vec![];
        if own("cloudflare") || launcher("cloudflare") { plan.push(if self.cf.is_some() { Carry::Comes("cloudflare") } else { Carry::NeedsSignIn("cloudflare") }) }
        if own("modal") || launcher("modal") { plan.push(if self.modal.is_some() { Carry::Comes("modal") } else { Carry::NeedsSignIn("modal") }) }
        if from.aws {
            let account = st["aws"]["account_id"].as_str().unwrap_or_default().to_string();
            plan.push(match &to.plane {
                Plane::Aws { account_id, .. } if *account_id == account => Carry::Comes("aws"),
                Plane::Cloudflare { .. } => match &self.aws { Some(a) if a.account_id == account => Carry::Comes("aws"), _ => Carry::NeedsSignIn("aws") },
                Plane::Aws { .. } => Carry::Cannot("aws", "AWS runners stay with their account: add AWS again on the new one"),
                _ => Carry::Cannot("aws", "a Modal control plane cannot start AWS machines yet"),
            });
        }
        plan
    }

    /// What the control plane in use is allowed, by each cloud and code host, against what this version asks for
    /// (`permissions.rs`): up to date, or what is missing, why, and how to give it. AWS: read from its role while signed
    /// in (Review gives it); not signed in, what AWS refused lately. GitHub: the App's and each installation's permissions.
    /// GitLab: the token's scopes.
    fn permissions_card(&self, v: &PlaneView) -> (String, String) {
        use superci_core::permissions::{github_missing, gitlab_missing, Need};
        let st = v.status.clone().unwrap_or_default();
        let Some(perms) = st.get("permissions").cloned() else { return (String::new(), String::new()) };
        let mut rows = String::new();
        let mut dialogs = String::new();
        let ok = || pill("good", "Up to date");
        // A row says in a few words how it stands; what there is to do opens from its one button (`perm_modal`).
        let row = |cloud: &str, sub: &str, end: String| format!(r#"<div class="pick act">{}<span class="pick-main"><strong>{}</strong><small>{}</small></span><span class="pick-end">{end}</span></div>"#, logo(cloud, 32), provider_name(cloud), esc(sub));
        let review = |id: &str| format!(r#"<button type="button" class="button primary sm" onclick="document.getElementById('{id}').showModal()">Review</button>"#);
        let signin = |cloud: &str| format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="button secondary sm">Sign in</button></form>"#);
        let count = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        // AWS.
        if let Some(account) = aws_account(v) {
            let role = format!("superci-plane-{}", v.plane.plane_id());
            let denied: Vec<String> = perms["denied"].as_object().into_iter().flatten().filter_map(|(k, _)| k.strip_prefix("aws:").map(str::to_string)).collect();
            let signed = self.aws.as_ref().is_some_and(|a| a.account_id == account);
            // Signed in, but to another account than the one its role is in.
            let other = self.aws.as_ref().filter(|a| a.account_id != account).map(|a| a.account_id.clone());
            let known = lock(&self.aws_missing).get(v.plane.plane_id()).map(|(m, at)| match m {
                // Still being read after 45 s: AWS did not answer (read again on the next look after five minutes).
                Err(e) if e.is_empty() && now_ms() > at + 45_000 => Err("AWS did not answer in time".to_string()),
                m => m.clone(),
            });
            let reading = || ("Checking its role…".to_string(), r#"<span class="mini-spin"></span><div data-refresh="2"></div>"#.to_string());
            let (sub, end) = match (signed, known) {
                (true, Some(Ok(missing))) if missing.is_empty() => (format!("Role in account {account}"), ok()),
                (true, Some(Ok(missing))) => {
                    let needs: Vec<Need> = missing.iter().map(|n| **n).collect();
                    dialogs.push_str(&perm_modal("perm-aws", "Allow in AWS", &[
                        ("What it needs", needs_card(&needs)),
                        ("Allow it", format!(r#"<p class="prompt-modal-instruction">Added to the role {} in account {}.</p><div class="perm-action"><form method="post" action="/permissions/aws"><input type="hidden" name="plane" value="{}"><button class="prompt-modal-copy">Allow</button></form></div>"#, esc(&role), esc(&account), esc(v.plane.plane_id()))),
                    ]));
                    (format!("{} to allow", count(missing.len(), "permission")), review("perm-aws"))
                }
                // While it is read, the page looks again every two seconds.
                (true, Some(Err(e))) if e.is_empty() => reading(),
                (true, Some(Err(e))) => (format!("Not read: {}", e.chars().take(80).collect::<String>()), pill("bad", "Not read")),
                (true, None) => reading(),
                (false, _) if !denied.is_empty() => {
                    let what: Vec<Need> = superci_core::permissions::AWS_RUNNER.iter().filter(|n| denied.iter().any(|d| d == n.id)).copied().collect();
                    dialogs.push_str(&perm_modal("perm-aws", "Allow in AWS", &[
                        ("What AWS refused", needs_card(&what)),
                        // Back from AWS, this dialog opens again, with everything to allow.
                        ("Sign in with AWS", format!(r#"<p class="prompt-modal-instruction">To account {}{}. You come back here, with everything to allow in one step.</p><div class="perm-action"><form method="post" action="/aws/signin"><input type="hidden" name="next" data-open="perm-aws"><button class="prompt-modal-copy">Sign in with AWS</button></form></div>"#,
                            esc(&account), other.as_ref().map(|o| format!(" (you are signed in to {})", esc(o))).unwrap_or_default())),
                    ]));
                    (format!("AWS refused {}", count(what.len().max(1), "permission")), review("perm-aws"))
                }
                (false, _) => (match &other { Some(o) => format!("Signed in to account {o}; its role is in {account}"), None => "Sign in to check its role".to_string() }, signin("aws")),
            };
            rows.push_str(&row("aws", &sub, end));
        }
        // Cloudflare and Modal: what each sign-in asks for (nothing kept in the account to fall behind).
        let uses = |cloud: &str| v.plane.cloud() == cloud || (st["containers"] == true && st["own_cloud"] == cloud) || st["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == cloud));
        if uses("cloudflare") {
            rows.push_str(&if self.cf.is_some() { row("cloudflare", "Your sign-in covers it", ok()) } else { row("cloudflare", "Asked for when you sign in", signin("cloudflare")) });
        }
        if uses("modal") {
            rows.push_str(&if self.modal.is_some() { row("modal", "Your sign-in covers it", ok()) }
                else if self.modal_pending.is_some() { row("modal", "Approve it in Modal's tab, then close the tab",
                    format!(r#"{}<form method="post" action="/modal/signin"><input type="hidden" name="next"><button class="button secondary sm">Open again</button></form>"#, pill("open", "Waiting"))) }
                else { row("modal", "Optional: its runners work without it", signin("modal")) });
        }
        // GitHub: each organization's App (one each), what it asks for and what its installations accepted.
        let apps = status_apps(&st);
        for app in &apps {
            let org = app["org"] != false;
            let (slug, owner, host) = (app["slug"].as_str().unwrap_or_default(), app["owner"].as_str().unwrap_or_default(), app["host"].as_str());
            let asks = github_missing(&app["permissions"], org);
            let whose = if apps.len() > 1 { format!("{owner}: ") } else { String::new() };
            let id = format!("perm-gh-{}", app["id"].as_u64().unwrap_or_default());
            let web = github::web_base(host);
            let installs: Vec<&serde_json::Value> = st["installations"].as_array().into_iter().flatten().filter(|i| i["app"].is_null() || i["app"] == app["id"]).collect();
            let install_link = |i: &serde_json::Value| { let account = i["account"].as_str().unwrap_or_default(); if org { format!("{web}/organizations/{account}/settings/installations/{}", i["id"]) } else { format!("{web}/settings/installations/{}", i["id"]) } };
            let open = |href: &str, label: &str, quiet: bool| format!(r#"<a class="prompt-modal-copy{}" href="{}" target="_blank" rel="noopener">{} ↗</a>"#, if quiet { " is-quiet" } else { "" }, esc(href), esc(label));
            let (sub, end) = if app["permissions"].is_object() && !asks.is_empty() {
                // Where each is in the App's settings.
                let place = asks.iter().map(|n| match n.id { "actions" => "Repository permissions → Actions → Read and write", "administration" => "Repository permissions → Administration → Read and write",
                    "organization_self_hosted_runners" => "Organization permissions → Self-hosted runners → Read and write", _ => n.what }).collect::<Vec<_>>().join("; ");
                let accept = installs.iter().map(|i| open(&install_link(i), &format!("Open {}", i["account"].as_str().unwrap_or("the installation")), true)).collect::<String>();
                dialogs.push_str(&perm_modal(&id, "Add in GitHub", &[
                    ("What to add", needs_card(&asks)),
                    ("Add it in the App's settings", format!(r#"<p class="prompt-modal-instruction">{}, then Save changes.</p><div class="perm-action">{}</div>"#, esc(&place), open(&format!("{}/permissions", app_settings_link(host, owner, slug, org)), "Open App settings", false))),
                    ("Accept it where it is installed", format!(r#"<p class="prompt-modal-instruction">GitHub asks the owners to accept it: open the installation and choose Review request.</p><div class="perm-action">{accept}</div>"#)),
                ]));
                (format!("{whose}{} to add in its App", count(asks.len(), "permission")), review(&id))
            } else if let Some((i, missing)) = installs.iter().filter(|i| i["permissions"].is_object()).map(|i| (*i, github_missing(&i["permissions"], org))).find(|(_, m)| !m.is_empty()) {
                let account = i["account"].as_str().unwrap_or_default();
                dialogs.push_str(&perm_modal(&id, "Accept in GitHub", &[
                    (&format!("What {account} has not accepted"), needs_card(&missing)),
                    ("Accept it on GitHub", format!(r#"<p class="prompt-modal-instruction">Open the installation, choose Review request, then accept the new permissions.</p><div class="perm-action">{}</div>"#, open(&install_link(i), &format!("Open {account}"), false))),
                ]));
                (format!("{account} has {} to accept", count(missing.len(), "permission")), review(&id))
            } else { (format!("{whose}App {slug}"), ok()) };
            rows.push_str(&row("github", &sub, end));
        }
        // GitLab: each connection's token.
        let gls = v.gitlabs();
        for (id, url, scopes) in &gls {
            let Some(scopes) = scopes else { continue };
            let missing: Vec<Need> = gitlab_missing(scopes).into_iter().copied().collect();
            let whose = if gls.len() > 1 { format!("{}: ", url.trim_start_matches("https://")) } else { String::new() };
            let (sub, end) = if missing.is_empty() { (format!("{whose}Its token has every scope"), ok()) } else {
                let all = superci_core::permissions::GITLAB.iter().map(|n| n.id).collect::<Vec<_>>().join(",");
                let dialog = format!("perm-gitlab{}", if id.is_empty() { String::new() } else { format!("-{id}") });
                dialogs.push_str(&perm_modal(&dialog, "A new GitLab token", &[
                    ("What its token lacks", needs_card(&missing)),
                    ("Make a token with every scope", format!(r#"<p class="prompt-modal-instruction">Scopes: {}.</p><div class="perm-action"><a class="prompt-modal-copy" href="{}/-/user_settings/personal_access_tokens?name=superci&amp;scopes={all}" target="_blank" rel="noopener">Tokens on GitLab ↗</a></div>"#, esc(&all.replace(',', ", ")), esc(url.trim_end_matches('/')))),
                    ("Paste it in its settings", format!(r#"<p class="prompt-modal-instruction">GitLab settings here, then New token.</p><div class="perm-action"><a class="prompt-modal-copy is-quiet" href="/?p=gitlab&g={}&open=gl-token">GitLab settings</a></div>"#, esc(id))),
                ]));
                (format!("{whose}Its token lacks {}", count(missing.len(), "scope")), review(&dialog))
            };
            rows.push_str(&row("gitlab", &sub, end));
        }
        if rows.is_empty() { return (String::new(), String::new()) }
        (format!(r#"<h2 class="list-title" id="permissions">Permissions</h2><div class="picks page">{rows}</div>"#), dialogs)
    }

    /// The control plane: one list of where it can run. The one in use first; others set up before, to move to (Move
    /// here); then each cloud without one, to set one up there (its progress in its row); more clouds folded. Each
    /// control plane's rarer actions (delete, stop using SuperCI) are in its menu.
    fn planes_page(&self, views: &[PlaneView]) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">Control plane</h1></div>"#;
        if views.is_empty() {
            let found = self.found_card();
            if !found.is_empty() { return format!(r#"{bar}{found}<p class="note" style="margin-top:14px"><a href="/?p=plane">Or set up a new one</a></p>"#) }
            return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&ICON_PLANE.replace("<svg", r#"<svg class="ic""#), "No control plane yet", "Set one up in Cloudflare, AWS or Modal; it takes a minute.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        }
        let used = view::in_use(views);
        let v = &views[used];
        // Signed in to the account it lives in (another account of the same cloud is no sign-in to this one).
        let own = self.own();
        let signed_in = |v: &PlaneView| own.check(&v.plane).is_ok() && match v.plane { Plane::Cloudflare { .. } => self.cf.is_some(), Plane::Aws { .. } => self.aws.is_some(), Plane::Modal { .. } => self.modal.is_some(), Plane::Seen { .. } => false };
        let provider = |v: &PlaneView| v.plane.cloud();
        let updating = lock(&self.updating);
        // (An update under way shows in the row's second line instead.)
        let update = |i: usize, v: &PlaneView| if updating.as_ref().is_some_and(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(_)))) { String::new() }
            // Updated, the old version still answering: said, and looked at again until the new one does.
            else if updating.as_ref().is_some_and(|u| u.restarting(v)) { r#"<span class="note upd-step"><span class="mini-spin"></span>Restarting…</span><div data-refresh="3"></div>"#.to_string() }
            // (Behind is said only of one that answers.)
            else if !v.outdated() { String::new() } else if signed_in(v) {
            format!(r#"<form method="post" action="/plane/update" onsubmit="var b=this.querySelector('button');b.disabled=true;b.textContent='Updating…'"><input type="hidden" name="plane" value="{}"><input type="hidden" name="next"><button class="button {} sm">Update to {DASHBOARD_VERSION}</button></form>"#, esc(v.plane.plane_id()), if i == used { "primary" } else { "secondary" })
        } else { String::new() };
        // Not signed in to a control plane's cloud: one button for that (the rest follows once signed in).
        let sign_in = |cloud: &str| format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="button secondary sm">Sign in with {}</button></form>"#, provider_name(cloud));
        // (Its items are its rarer actions, each one that ends something: Delete, Stop using SuperCI.)
        let menu = |items: &[(&str, String)]| if items.is_empty() { String::new() } else {
            format!(r#"<details class="menu"><summary class="icon-btn" aria-label="More">{ICON_MORE}</summary><div class="menu-pop">{}</div></details>"#,
                items.iter().map(|(label, dialog)| format!(r#"<button type="button" class="danger-item" onclick="this.closest('details').open=false;document.getElementById('{dialog}').showModal()">{label}</button>"#)).collect::<String>())
        };
        // An update under way: its progress in the row's second line (the address otherwise), a spinner at its end.
        let busy = |v: &PlaneView| updating.as_ref().filter(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(_)))).map(|u| {
            let n = u.steps.len();
            match &u.result {
                Some(Err(e)) => (format!(r#"<span class="bad-text">The update stopped: {}</span>"#, esc(&e.chars().take(140).collect::<String>())),
                    format!(r#"<form method="post" action="/plane/update"><input type="hidden" name="plane" value="{}"><input type="hidden" name="next"><button class="button secondary sm">Try again</button></form>"#, esc(v.plane.plane_id()))),
                // (Under way: one that ended well is not busy.)
                _ => (format!(r#"Updating to {DASHBOARD_VERSION} · {} <span class="faint">({} of {n})</span><span class="slim"><i style="width:{}%"></i></span>"#, esc(u.steps.get(u.at).copied().unwrap_or("Finishing")), u.at + 1, (u.at * 100 + 50) / n.max(1)), r#"<span class="mini-spin"></span>"#.to_string()),
            }
        });
        let row = |class: &str, v: &PlaneView, end: String| {
            let address = format!(r#"<a href="{}/health" target="_blank" rel="noopener">{}</a>"#, esc(v.plane.url()), esc(v.plane.url().trim_start_matches("https://")));
            let (sub, end) = match busy(v) { Some((sub, spin)) => (sub, format!("{spin}{end}")), None => (address, end) };
            format!(r#"<div class="pick act{class}">{}<span class="pick-main"><strong>{}</strong><small>{sub}</small></span><span class="pick-end">{end}</span></div>"#, logo(provider(v), 36), esc(&v.plane.place()))
        };
        let mut dialogs = String::new();

        // In use.
        let updating_used = updating.as_ref().is_some_and(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(_))));
        let state = if !v.online { pill("bad", "not answering") } else if v.outdated() || updating_used { update(used, v) } else { format!(r#"<span class="mono faint">{}</span>"#, esc(&v.version_name())) };
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="leave" onclick="if(event.target===this)this.close()"><form method="post" action="/plane/leave" class="dlg-body" onsubmit="var b=this.querySelector('.danger');b.disabled=true;b.textContent='Stopping…'"><div class="dlg-head"><span class="logo warn-mark">{ICON_ALERT}</span><span><strong>Stop using SuperCI?</strong><small>For good, from every cloud</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><ul class="checks stays"><li>–<span>GitHub's App is uninstalled and GitLab's projects stop sending jobs here (their webhooks and SuperCI's runners are removed).</span></li><li>–<span>Every control plane is deleted, with its history, and the launchers and AWS roles made for them.</span></li><li>–<span>Jobs with <code>runs-on: superci</code> then wait for a runner: change them back first.</span></li></ul><label class="field"><span>Type <code>superci</code> to confirm</span><input type="text" name="confirm" autocomplete="off" required pattern="superci"></label><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button danger sm">Stop using SuperCI</button></div></form></dialog>"#));
        let state = if matches!(v.plane, Plane::Seen { .. }) { signin_button(v.plane.cloud(), &format!("Sign in with {}", provider_name(v.plane.cloud())), false) } else { state };
        let leave = if signed_in(v) { menu(&[("Stop using SuperCI…", "leave".to_string())]) } else { String::new() };
        let mut rows = row(" current", v, format!(r#"{state}{}{leave}"#, pill("accent", "In use")));

        // Set up before: to move to (or moving to).
        for (i, o) in views.iter().enumerate().filter(|(i, _)| *i != used) {
            dialogs.push_str(&self.move_dialog(i, v, o));
            if let Some(m) = lock(&self.moving).as_ref().filter(|m| m.to == o.plane.plane_id() && !matches!(m.result, Some(Ok(_)))) {
                let n = MOVE_STEPS.len();
                let (sub, end) = match &m.result {
                    None => (format!(r#"{} <span class="faint">({} of {n})</span><span class="slim"><i style="width:{}%"></i></span>"#, MOVE_STEPS[m.at.min(n - 1)], m.at + 1, (m.at * 100 + 50) / n), r#"<span class="mini-spin"></span>"#.to_string()),
                    _ => (format!(r#"<span class="bad-text">{}</span>"#, esc(m.result.as_ref().and_then(|r| r.as_ref().err()).map(String::as_str).unwrap_or_default())), format!(r#"<button type="button" class="button secondary sm" onclick="document.getElementById('move-{i}').showModal()">Try again</button>"#)),
                };
                rows.push_str(&format!(r#"<div class="pick act">{}<span><strong>Moving to {}</strong><small>{sub}</small></span><span class="pick-end">{end}</span></div>"#, logo(provider(o), 36), esc(&o.plane.place())));
                continue;
            }
            // Its update (or the update under way), then moving to it.
            let upd = if o.online { update(i, o) } else { String::new() };
            let updating_it = busy(o).is_some();
            let end = if !o.online { pill("bad", "not answering") }
                else if updating_it { String::new() }
                // A CI of its own (another team's, reached with the same sign-ins): no place to move to, and not
                // deleted from here.
                else if o.serves_its_own() { format!(r#"<span class="note">Serves a code host of its own</span><a class="button secondary sm" href="/?c={}">Open</a>"#, esc(o.plane.plane_id())) }
                else if view::older(o.version.as_deref(), view::MOVE_VERSION) { upd.clone() }
                else if view::older(v.version.as_deref(), view::MOVE_VERSION) { r#"<span class="note">Update the one in use first</span>"#.to_string() }
                else if !signed_in(o) { sign_in(o.plane.cloud()) }
                else if !signed_in(v) { sign_in(v.plane.cloud()) }
                else { format!(r#"<button type="button" class="button secondary sm" onclick="document.getElementById('move-{i}').showModal()">Move here</button>"#) };
            let before = "";
            let items = if signed_in(o) && !o.serves_its_own() { vec![("Delete…", format!("delete-{i}"))] } else { vec![] };
            let what = match o.plane { Plane::Cloudflare { .. } => "its Worker, its storage and its containers", Plane::Aws { .. } => "its function, table, schedule, parameters and role", _ => "its app and its Dicts" };
            if !o.serves_its_own() {
                dialogs.push_str(&format!(r#"<dialog class="dlg" id="delete-{i}" onclick="if(event.target===this)this.close()"><form method="post" action="/plane/delete" class="dlg-body" onsubmit="var b=this.querySelector('.danger');b.disabled=true;b.textContent='Deleting…'"><input type="hidden" name="plane" value="{}"><div class="dlg-head">{}<span><strong>Delete {}?</strong><small>{}</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><p class="note">This deletes {what} from {}, and the launchers made for it. The control plane in use, your jobs and your history are not touched.</p><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button danger sm">Delete</button></div></form></dialog>"#,
                esc(o.plane.plane_id()), logo(provider(o), 36), esc(&o.plane.place()), esc(o.plane.url().trim_start_matches("https://")), provider_name(provider(o))));
            }
            let end = if upd.is_empty() || end == upd { end } else { format!("{upd}{end}") };
            rows.push_str(&row("", o, format!("{before}{end}{}", menu(&items))));
        }

        // Clouds without one: set one up there (its progress in its row).
        let deploying = lock(&self.deploying).as_ref().filter(|d| !matches!(d.result, Some(Ok(_)))).map(|d| {
            let n = d.steps.len();
            let (sub, end) = match &d.result {
                None => (format!(r#"{} <span class="faint">({} of {n})</span><span class="slim"><i style="width:{}%"></i></span>"#, esc(d.steps.get(d.at).copied().unwrap_or("Finishing")), d.at + 1, (d.at * 100 + 50) / n.max(1)), r#"<span class="mini-spin"></span>"#.to_string()),
                _ => (format!(r#"<span class="bad-text">{}</span>"#, esc(d.result.as_ref().and_then(|r| r.as_ref().err()).map(String::as_str).unwrap_or_default())), format!(r#"<form method="post" action="/plane/{}">{}<button class="button secondary sm">Try again</button></form>"#,
                    d.cloud, d.form.iter().map(|(k, v)| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v))).collect::<String>())),
            };
            (d.cloud, format!(r#"<div class="pick act">{}<span><strong>Setting up in {}</strong><small>{} · {sub}</small></span><span class="pick-end">{end}</span></div>"#, logo(d.cloud, 36), provider_name(d.cloud), esc(&d.place)))
        });
        let listed: Vec<Plane> = views.iter().map(|v| v.plane.clone()).collect();
        // (Not signed in, and a control plane there is listed already (with its own sign-in): no second row.)
        let signed = |cloud: &str| match cloud { "cloudflare" => self.cf.is_some(), "aws" => self.aws.is_some(), _ => self.modal.is_some() };
        for (cloud, what, action) in self.setup_actions(&listed).into_iter().filter(|(cloud, ..)| signed(cloud) || !listed.iter().any(|p| p.cloud() == *cloud)) {
            match (&deploying, action) {
                (Some((c, html)), _) if *c == cloud => rows.push_str(html),
                (_, Some(action)) => rows.push_str(&format!(r#"<div class="pick act idle">{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{action}</span></div>"#, logo(cloud, 36), provider_name(cloud), esc(&what))),
                _ => {}
            }
        }
        rows.push_str(&more_clouds(&PLANE_SOON));

        // How the last move ended, when something stayed behind; what the last delete did.
        // (Its state read once: locked again while held, it would wait for itself.)
        let (moving, ended) = {
            let m = lock(&self.moving);
            let ended = match m.as_ref().map(|m| (m, m.result.as_ref())) {
                Some((m, Some(Ok(left)))) => {
                    let to = views.iter().find(|v| v.plane.plane_id() == m.to).map(|v| provider_name(v.plane.cloud())).unwrap_or("the new control plane");
                    format!(r#"<div class="notice">{}<span>Moved to {to}: GitHub and GitLab send their jobs there now.{}</span></div>"#, icon("check"), esc(&left.iter().map(|l| format!(" {l}")).collect::<String>()))
                }
                _ => String::new(),
            };
            (m.as_ref().is_some_and(|m| m.result.is_none()), ended)
        };
        let ended = match &self.flash { Some(f) => format!(r#"{ended}<div class="notice">{}<span>{}</span></div>"#, icon("check"), esc(f)), None => ended };
        let (perms, perm_dialogs) = if v.online { self.permissions_card(v) } else { (String::new(), String::new()) };
        let dialogs = format!("{dialogs}{perm_dialogs}");
        format!(r#"{}{bar}{ended}<p class="note" style="margin:-4px 0 18px">GitHub and GitLab send their jobs to the control plane in use. To move it, set one up in another cloud and Move here: everything comes along.</p><div class="picks page">{rows}</div>{}{perms}{}{dialogs}"#,
            if moving || self.deploy_running() { r#"<div data-refresh="2"></div>"# } else { "" }, self.aws_account_section_for(Some(v)), self.found_card())
    }

    /// Move here's confirmation: what comes along, what needs a sign-in first, what stays; then Move.
    fn move_dialog(&self, i: usize, from: &PlaneView, to: &PlaneView) -> String {
        let plan = self.carry_plan(from, to);
        let jobs = from.status.as_ref().and_then(|s| s["jobs"].as_array().map(|a| a.len())).unwrap_or(0);
        let mut comes = vec!["Your GitHub App and its installations".to_string()];
        if from.gitlab { comes.push("GitLab and its projects' webhooks".into()) }
        comes.push("The order of your runner providers, their limits and the default machine".into());
        if jobs > 0 { comes.push("Job history".into()) }
        let mut needs = vec![];
        let mut stays = vec![];
        for c in &plan {
            match c {
                Carry::Comes(p) => comes.push(format!("{} runners", cloud_name(p))),
                Carry::NeedsSignIn(p) => needs.push(*p),
                Carry::Cannot(p, why) => stays.push(format!("{} runners: {why}", cloud_name(p))),
            }
        }
        let list = |items: &[String], mark: &str| items.iter().map(|t| format!(r#"<li>{mark}<span>{}</span></li>"#, esc(t))).collect::<String>();
        let needs_html = needs.iter().map(|p| format!(r#"<div class="pick act">{}<span><strong>Sign in with {}</strong><small>To bring its runners along</small></span><span class="pick-end">{}</span></div>"#, logo(p, 28), provider_name(p), connect_action(p))).collect::<String>();
        let go = if needs.is_empty() {
            format!(r#"<form method="post" action="/plane/move"><input type="hidden" name="plane" value="{}"><button class="button primary sm">Move</button></form>"#, esc(to.plane.plane_id()))
        } else { r#"<button class="button primary sm" disabled>Move</button>"#.to_string() };
        format!(r#"<dialog class="dlg" id="move-{i}" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Move to {}</strong><small>{}</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><div class="move-what"><span class="note">Comes along</span><ul class="checks">{}</ul>{}{}</div>{needs_html}<p class="note">Nothing is removed from the one in use: you can move back. Jobs running now finish where they are.</p><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button>{go}</div></div></dialog>"#,
            logo(match to.plane { Plane::Cloudflare { .. } => "cloudflare", Plane::Modal { .. } => "modal", _ => "aws" }, 36), esc(&to.plane.place()), esc(to.plane.url().trim_start_matches("https://")),
            list(&comes, &icon("check")), if stays.is_empty() { String::new() } else { format!(r#"<span class="note">Stays behind</span><ul class="checks stays">{}</ul>"#, list(&stays, "–")) },
            if needs.is_empty() { "" } else { r#"<span class="note">First</span>"# })
    }
}

/// What is new in SuperCI, every version, the ones a control plane here does not have yet marked.
fn changes_page(views: &[PlaneView], aws_lacks: &[&'static superci_core::permissions::Need]) -> String {
    // Against the control plane in use (others, not in use, update when moved to).
    let oldest = views.get(view::in_use(views)).filter(|v| v.online).map(|v| v.version.clone());
    // This dashboard's version, and the control plane's when it is older.
    let versions = match oldest.as_ref().and_then(|o| o.clone()).filter(|o| view::older(Some(o), DASHBOARD_VERSION)) {
        Some(plane) => format!("Dashboard {DASHBOARD_VERSION} · Control plane {}", esc(&plane)),
        None => format!("Dashboard {DASHBOARD_VERSION}"),
    };
    // What updating also asks of your clouds and code hosts, each with why.
    let needs = views.get(view::in_use(views)).filter(|v| v.online && v.outdated()).map(|v| new_needs(v, aws_lacks)).unwrap_or_default();
    let asks = if needs.is_empty() { String::new() } else {
        format!(r#"<div class="release asks-list"><div class="release-head"><strong>Updating also asks for</strong></div><ul>{}</ul></div>"#,
            needs.iter().map(|(cloud, n)| format!(r#"<li><strong>{}</strong>: {} <span class="faint">{}</span></li>"#, provider_name(cloud), esc(n.what), esc(n.why))).collect::<String>())
    };
    let news = changelog().iter().map(|(v, date, items)| format!(r#"<div class="release"><div class="release-head"><strong>{}</strong><span class="mono">{}</span>{}</div><ul>{}</ul></div>"#,
        esc(v), esc(date), if oldest.as_ref().is_some_and(|o| view::older(o.as_deref(), v)) { pill("open", "not on your control plane yet") } else { String::new() },
        items.iter().map(|i| format!("<li>{}</li>", change_html(i))).collect::<String>())).collect::<String>();
    format!(r#"<div class="bar"><h1 class="page-title">Changelog</h1><span class="mono">{versions}</span></div><div class="cards"><section class="card" id="whats-new">{asks}{news}</section></div>"#)
}

/// The AWS account a control plane's machines run in: its own (an AWS control plane), or the one connected for runners.
/// A button with coding agents' marks (as supercov's Improve) that opens a prompt.
fn agent_button(label: &str, accessible: &str, onclick: &str) -> String {
    let marks = crate::agents::MARKS.iter().map(|(brand, svg)| format!(r#"<span class="jmark agent-{brand}">{svg}</span>"#)).collect::<String>();
    format!(r#"<button type="button" class="subtle-button improve-button" aria-label="{}" aria-haspopup="dialog" onclick="{}"><span class="agent-marks" aria-hidden="true">{marks}</span><span>{}</span></button>"#, esc(accessible), esc(onclick), esc(label))
}

/// The prompts for switching workflows to SuperCI, each as (choice, what it does, for GitHub Actions, for GitLab CI).
fn switch_prompts(v: &PlaneView) -> Vec<(&'static str, &'static str, String, String)> {
    let l = &v.label();
    let gh = |task: &str| format!("Move this repository's GitHub Actions jobs to SuperCI runners (`runs-on: {l}`, a fresh self-hosted machine per job). {task}

- Linux jobs: `{l}`, keeping their size as key=value pairs after it (`{l}/cpu=8`, `{l}/cpu=16/ram=64`, `{l}/arch=arm64`). Windows: `{l}/os=windows`. GPU: `{l}/gpu=any`.
- Keep on GitHub, with a `# stays on GitHub: <why>` comment: macOS, npm publishing with provenance, jobs needing KVM, and fork pull requests in public repositories (`${{{{ github.event.pull_request.head.repo.fork && 'ubuntu-latest' || '{l}' }}}}`).
- Change matrix values rather than `runs-on: ${{{{ matrix.os }}}}`, and reusable workflows where they are defined. One label per job, not a list.
- Open one pull request with a before → after table. If a job fails only on the new runner, move it back and say why.");
    let gl = |task: &str| format!("Move this project's GitLab CI jobs to SuperCI runners (tag `{l}`, a fresh machine per job, in Docker). {task}

- Tag jobs on GitLab's hosted Linux runners `{l}` (replacing `saas-linux-*`, or once in `default: tags`), keeping their size: `{l}/cpu=4` for medium, `{l}/cpu=8` for large, `{l}/cpu=16` for xlarge, `{l}/arch=arm64` for arm64. Keep `image:` and `services:`.
- Keep macOS and Windows jobs on GitLab, with a `# stays on GitLab: <why>` comment. Change jobs from included files where they are defined.
- Open one merge request with a before → after table. If a job fails only on the new runner, move it back and say why.");
    vec![
        ("Every workflow", "Each job SuperCI can run, in one pull request; the rest stay, with why.", gh("Do every workflow."), gl("Do every job.")),
        ("One workflow first", "The busiest one, to try it before the rest.",
            gh("Start with one workflow only: the busiest (see `gh run list`)."), gl("Start with the busiest pipeline's jobs only.")),
    ]
}

/// Where a code host is, as two choices of a dialog's first step: the public one, or your own, which shows a field
/// for its address (`data-own`, required then; see the page's script).
fn where_choices(name: &str, public: (&str, &str), own: (&str, &str), field: &str) -> String {
    format!(r#"<fieldset class="prompt-modal-choices"><legend class="sr-only">Where</legend>
<label class="prompt-modal-choice is-selected"><input type="radio" name="{name}" value="public" checked><span><strong>{}</strong><small>{}</small></span></label>
<label class="prompt-modal-choice"><input type="radio" name="{name}" value="own"><span><strong>{}</strong><small>{}</small></span></label></fieldset>
<div class="perm-field" data-own hidden>{field}</div>"#, public.0, public.1, own.0, own.1)
}

/// The organizations (and GitLabs) whose jobs a control plane runs, in words: "GitHub: acme, GitLab: gitlab.com". (Of
/// one that serves a code host of its own: GitHub's or GitLab's.)
fn serves(v: &PlaneView) -> String {
    let st = v.status.clone().unwrap_or_default();
    let mut what: Vec<String> = status_apps(&st).iter().filter_map(|a| a["owner"].as_str().map(|o| format!("GitHub: {o}"))).collect();
    what.extend(v.gitlabs().into_iter().map(|(_, url, _)| format!("GitLab: {}", url.trim_start_matches("https://"))));
    if what.is_empty() { what.push(if v.github { "GitHub" } else { "GitLab" }.into()) }
    what.join(", ")
}

/// What to send an owner of an organization one does not own: GitHub shows the page that creates the organization's
/// App to its owners only (anyone else gets its 404), so they have to do that one step, or lend the right for it.
/// The permissions are the App's own list, so the two never differ.
pub fn owner_note(org: &str, place: &str) -> String {
    let needs = superci_core::permissions::github(true).into_iter().map(|(_, _, need)| need.what.to_lowercase()).collect::<Vec<_>>().join(", ");
    format!("We are setting up our CI to run on our own cloud account with SuperCI (superci.dev). It needs a private GitHub App owned by {org}, with these permissions: {needs}. It registers runners and reads job events, never code. Only an owner of {org} can create it. Could you either make me an owner for a few minutes, or run `npx @superci/cli dashboard`, sign in where our control plane is ({place}), and under Repositories connect {org} and install the App on our repositories?")
}

/// Connecting GitHub (the first organization, or another): where it is, whose jobs, then GitHub makes the App.
fn github_dialog(another: bool, place: &str) -> String {
    // "Not an owner?": copies the note with the name typed (or "our organization"), and says so.
    let note = esc(&owner_note("{org}", place));
    let copy = format!(r##" Not an owner? <a href="#" data-note="{note}" onclick="var o=this.closest('form').querySelector('[name=login]').value.trim()||'our organization';navigator.clipboard.writeText(this.dataset.note.split('{{org}}').join(o));this.textContent='Copied: send it to an owner';return false">Copy a note for one</a>"##);
    let wher = where_choices("gh-where", ("GitHub.com", "github.com"), ("GitHub Enterprise", "Your own server, or yours.ghe.com"),
        r#"<input type="text" name="host" placeholder="github.example.com" autocomplete="off" spellcheck="false" aria-label="Your GitHub's address"><small>Its address, without https://. It has to be reachable from your control plane.</small>"#);
    let body = format!(r#"<form method="post" action="/github/start">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Where is your GitHub?</h2></div>{wher}</section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Whose jobs?</h2></div><div class="perm-field"><input type="text" name="login" placeholder="your-organization" required pattern="[A-Za-z0-9][A-Za-z0-9-]{{0,38}}" autocomplete="off" spellcheck="false" aria-label="GitHub organization or user"><small>An organization you own, or your own username.{copy}</small></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>3</span><h2>Create its App</h2></div><p class="prompt-modal-instruction">GitHub makes a private App there, then asks where to install it. It registers runners and reads job events, never your code.</p><p class="prompt-modal-instruction">It is named after your CI and the organization. Should GitHub say that name is taken (an App of yours from before), type another on its page: any name works.</p><div class="perm-action"><button class="prompt-modal-copy">Continue on GitHub</button></div></section>
</form>"#);
    step_dialog("gh-add", if another { "Add an organization" } else { "Connect GitHub" }, &body)
}

/// Connecting GitLab: where it is, a token made there, pasted here.
fn gitlab_dialog(another: bool) -> String {
    let wher = where_choices("gl-where", ("GitLab.com", "gitlab.com"), ("Your own GitLab", "A GitLab you host"),
        r#"<input type="text" name="own_url" placeholder="https://gitlab.example.com" autocomplete="off" spellcheck="false" aria-label="Your GitLab's address"><small>Its address. It has to be reachable from your control plane.</small>"#);
    let scopes = superci_core::permissions::GITLAB.iter().map(|n| n.id).collect::<Vec<_>>();
    let body = format!(r#"<form method="post" action="/gitlab/connect" data-gitlab><input type="hidden" name="which" value="new"><input type="hidden" name="url" value="https://gitlab.com">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Where is your GitLab?</h2></div>{wher}</section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Make a token</h2></div><p class="prompt-modal-instruction">An access token with the scopes {}. The link has them ticked.</p><div class="perm-action"><a class="prompt-modal-copy is-quiet" data-make data-path="/-/user_settings/personal_access_tokens?name=superci&amp;scopes={}" href="https://gitlab.com/-/user_settings/personal_access_tokens?name=superci&amp;scopes={}" target="_blank" rel="noopener">Tokens on GitLab ↗</a></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>3</span><h2>Paste it here</h2></div><div class="perm-field"><input type="password" name="token" required placeholder="glpat-…" autocomplete="off" aria-label="GitLab token"><small>Kept in your control plane's secrets. Nothing changes in GitLab until you turn projects on.</small></div><div class="perm-action"><button class="prompt-modal-copy">Connect GitLab</button></div></section>
</form>"#, scopes.join(", "), scopes.join(","), scopes.join(","));
    step_dialog("gl-add", if another { "Add a GitLab" } else { "Connect GitLab" }, &body)
}

/// A new token for a GitLab connection that is there (its old one ran out, or lacks a scope): made there, pasted here.
fn gitlab_token_dialog(which: &str, url: &str) -> String {
    let scopes = superci_core::permissions::GITLAB.iter().map(|n| n.id).collect::<Vec<_>>();
    let body = format!(r#"<form method="post" action="/gitlab/connect"><input type="hidden" name="which" value="{}"><input type="hidden" name="url" value="{}">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Make a token</h2></div><p class="prompt-modal-instruction">An access token with the scopes {}. The link has them ticked.</p><div class="perm-action"><a class="prompt-modal-copy is-quiet" href="{}/-/user_settings/personal_access_tokens?name=superci&amp;scopes={}" target="_blank" rel="noopener">Tokens on GitLab ↗</a></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Paste it here</h2></div><div class="perm-field"><input type="password" name="token" required placeholder="glpat-…" autocomplete="off" aria-label="GitLab token"><small>It replaces the one kept in your control plane's secrets; the projects turned on stay on.</small></div><div class="perm-action"><button class="prompt-modal-copy">Save token</button></div></section>
</form>"#, esc(which), esc(url), scopes.join(", "), esc(url.trim_end_matches('/')), scopes.join(","));
    step_dialog("gl-token", "A new GitLab token", &body)
}

/// A dialog of numbered steps, in the look of the prompt dialog.
fn step_dialog(id: &str, title: &str, body: &str) -> String {
    format!(r#"<dialog class="report-modal prompt-modal perm-modal" id="{}" aria-label="{}" onclick="if(event.target===this)this.close()"><button type="button" class="prompt-modal-close" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button><div class="improve-content">{body}</div></dialog>"#, esc(id), esc(title))
}

/// What a permission row has to do, as numbered steps (in the look of the prompt dialog): each a heading and what is
/// under it (a card of permissions, a line of how, the one button that does it).
fn perm_modal(id: &str, title: &str, steps: &[(&str, String)]) -> String {
    let steps = steps.iter().enumerate().map(|(n, (head, body))| format!(r#"<section class="prompt-modal-step{}"><div class="prompt-modal-step-head"><span>{}</span><h2>{}</h2></div>{body}</section>"#,
        if n == 0 { " prompt-modal-step-first" } else { "" }, n + 1, esc(head))).collect::<String>();
    step_dialog(id, title, &steps)
}

/// Permissions as a card of such a dialog: each with what it allows, whether it changes anything, and why.
fn needs_card(needs: &[superci_core::permissions::Need]) -> String {
    format!(r#"<div class="prompt-modal-copy-card"><ul class="perm-needs">{}</ul></div>"#, needs.iter().map(|n| format!(r#"<li><strong>{}<em>{}</em></strong><small>{} <span>New in {}.</span></small></li>"#,
        esc(n.what), if n.writes { "Changes" } else { "Reads" }, esc(n.why), esc(n.since))).collect::<String>())
}

/// supercov's prompt dialog, for switching workflows: choose a prompt, copy it, open your coding agent. The prompt
/// follows the page's GitHub Actions / GitLab CI choice.
fn agent_prompt_dialog(v: &PlaneView, gh: bool, gl: bool) -> String {
    let choices = switch_prompts(v).into_iter().enumerate().map(|(i, (label, what, gh_text, gl_text))| format!(
        r#"<label class="prompt-modal-choice{}"><input type="radio" name="superci-prompt" value="{i}"{}{}{}><span><strong>{}</strong><small>{}</small></span></label>"#,
        if i == 0 { " is-selected" } else { "" }, if i == 0 { " checked" } else { "" },
        if gh { format!(r#" data-gh="{}""#, esc(&gh_text)) } else { String::new() }, if gl { format!(r#" data-gl="{}""#, esc(&gl_text)) } else { String::new() }, esc(label), esc(what))).collect::<String>();
    let agents = crate::agents::ICONS.iter().map(|(name, icon)| format!(r#"<span><img src="{icon}" alt=""><small>{name}</small></span>"#)).collect::<String>();
    let host = if gh { "gh" } else { "gl" };
    format!(r#"<dialog class="report-modal prompt-modal" id="agent-prompt" data-host="{host}" aria-label="Switch workflows" onclick="if(event.target===this)this.close()"><button type="button" class="prompt-modal-close" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button><div class="improve-content">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Choose a prompt</h2></div><fieldset class="prompt-modal-choices"><legend class="sr-only">Prompt</legend>{choices}</fieldset></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Copy prompt</h2></div><div class="prompt-modal-copy-card"><div class="prompt-modal-preview improve-prompt" data-preview></div><button type="button" class="prompt-modal-copy" data-copy><svg viewBox="0 0 20 20" aria-hidden="true"><rect x="7" y="3" width="10" height="10" rx="2"/><path d="M13 13v2a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2h2"/></svg><span>Copy prompt</span></button><p class="prompt-copy-error" role="alert" hidden></p></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>3</span><h2>Open your coding agent</h2></div><p class="prompt-modal-instruction">Start a new task in the repository, paste the prompt, and send.</p><div class="prompt-modal-agents">{agents}</div></section>
</div></dialog>"#)
}

/// Jobs with a machine on a provider now.
fn running_on(status: &serde_json::Value, cloud: &str) -> usize {
    // Counted by the control plane over all its jobs (its status lists only the newest fifty).
    if let Some(active) = status["active"].as_object() {
        let on = |place: &str| active.get(place).and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        return on(cloud) + if cloud == "aws" { on(AWS_ON_DEMAND) } else { 0 }
    }
    status["jobs"].as_array().into_iter().flatten().filter(|j| j["cloud"] == cloud && ["launching", "launched", "running"].contains(&j["state"].as_str().unwrap_or_default())).count()
}

/// Whether a control plane (by its status) starts runners on a provider.
fn provider_connected(status: &serde_json::Value, cloud: &str) -> bool {
    match cloud {
        "aws" => status["aws"]["connected"] == true,
        c => (status["containers"] == true && status["own_cloud"].as_str().unwrap_or("cloudflare") == c) || status["launchers"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c)),
    }
}

/// Removing a runner provider, confirmed: what is deleted where (the control plane and sign-ins stay), its running jobs,
/// and the sign-in that needs; or removing it from SuperCI only.
fn remove_dialog(v: &PlaneView, cloud: &str, i: usize, signed_in: &[&str]) -> String {
    let (logo_key, name) = (superci_core::plane::kind_of(cloud).to_string(), cloud_name(cloud).to_string());
    let id = v.plane.plane_id();
    let st = v.status.clone().unwrap_or_default();
    let own = st["containers"] == true && st["own_cloud"].as_str().unwrap_or("cloudflare") == cloud;
    let account = aws_account(v).unwrap_or_default();
    let deleted: Vec<String> = match cloud {
        "aws" => vec!["Its machines, and their network in each region".into(), if matches!(v.plane, Plane::Aws { .. }) { format!("The machines' policy on the role superci-plane-{id} (the control plane's own role stays)") } else { format!("The role superci-plane-{id} and its identity provider, in account {account}") }],
        _ if own => vec![],
        c if superci_core::plane::kind_of(c) == "modal" => vec![format!("The app superci-runners-{id} in your Modal workspace, and its sandboxes")],
        _ => vec![format!("The launcher superci-runners-{id} in your Cloudflare account, and its containers")],
    };
    let what = if deleted.is_empty() { format!(r#"<p class="note">Nothing is deleted: your control plane stops starting {}.</p>"#, if superci_core::plane::kind_of(cloud) == "modal" { "sandboxes" } else { "containers" }) }
        else { format!(r#"<p class="note">Deleted in {name}:</p><ul class="needs">{}</ul>"#, deleted.iter().map(|d| format!("<li><span>{}</span></li>", esc(d))).collect::<String>()) };
    let running = running_on(&st, cloud);
    let (stop, warn) = if running == 0 { (String::new(), String::new()) } else {
        (r#"<input type="hidden" name="stop" value="on">"#.to_string(), format!(r#"<p class="note bad-text">{running} job{} running there {}.</p>"#, if running == 1 { " is" } else { "s are" }, if own { "finish on their own" } else { "stop, and fail on GitHub" }))
    };
    let can = own || signed_in.contains(&cloud);
    let main = if can { format!(r#"<button class="button danger sm">{}</button>"#, if running > 0 && !own { format!("Stop {running} and remove") } else { "Remove".into() }) } else { String::new() };
    let signin = if can { String::new() } else { format!(r#"<div class="dlg-signin"><span class="note">Sign in with {name} to delete these.</span>{}</div>"#, signin_button(cloud, &format!("Sign in with {name}"), true)) };
    let only = if own { String::new() } else { r#"<button class="button secondary sm" name="only" value="on">Remove from SuperCI only</button>"#.to_string() };
    format!(r#"<dialog class="dlg" id="pool-remove-{i}" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Remove {}?</strong><small>Jobs stop going to {}. Your control plane and your sign-ins stay; adding it back is a click.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div>{what}{warn}{signin}<form method="post" action="/runners/remove" class="dlg-foot"><input type="hidden" name="plane" value="{}"><input type="hidden" name="cloud" value="{}">{stop}<button type="button" class="button secondary sm" style="margin-right:auto" onclick="this.closest('dialog').close()">Cancel</button>{only}{main}</form></div></dialog>"#,
        logo(&logo_key, 36), esc(&name), esc(&name), esc(id), esc(cloud))
}

/// The regions where a control plane's AWS machines need its own network, from its status: the regions they go to
/// (older ones: its connection's region), but for those given a network of the account's own.
fn status_regions(status: &serde_json::Value) -> Vec<String> {
    let listed: Vec<String> = status["aws_regions"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
    let all = if listed.is_empty() { status["aws"]["region"].as_str().map(|r| vec![r.to_string()]).unwrap_or_default() } else { listed };
    all.into_iter().filter(|r| status["aws_networks"].get(r).is_none()).collect()
}

/// Networks of the account's own, as typed: a line for each region, its subnets and security groups after it, and
/// `private` for subnets without public addresses ("us-east-1 subnet-0abc… subnet-0def… sg-0123… private").
fn given_networks(text: &str) -> Result<HashMap<String, superci_core::aws::GivenNetwork>> {
    let mut out = HashMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut words = line.split(|c: char| c.is_whitespace() || c == ',' || c == ':' || c == ';').filter(|w| !w.is_empty());
        let region = words.next().unwrap_or_default();
        if !REGIONS.contains(&region) { return Err(format!("Each line starts with a region ({region} is not one)")) }
        let mut n = superci_core::aws::GivenNetwork::default();
        for w in words {
            if superci_core::aws::aws_id(w, "subnet-") { n.subnets.push(w.to_string()) }
            else if superci_core::aws::aws_id(w, "sg-") { n.security_groups.push(w.to_string()) }
            else if w.eq_ignore_ascii_case("private") { n.private = true }
            else { return Err(format!("{region}: {w} is not a subnet id, a security group id, or “private”")) }
        }
        if n.subnets.is_empty() || n.security_groups.is_empty() { return Err(format!("{region}: name at least one subnet and one security group")) }
        if out.insert(region.to_string(), n).is_some() { return Err(format!("{region} is there twice")) }
    }
    Ok(out)
}

/// Those networks as the field shows them.
fn given_networks_text(status: &serde_json::Value) -> String {
    let nets: std::collections::BTreeMap<String, superci_core::aws::GivenNetwork> = serde_json::from_value(status["aws_networks"].clone()).unwrap_or_default();
    nets.iter().map(|(r, n)| format!("{r} {} {}{}", n.subnets.join(" "), n.security_groups.join(" "), if n.private { " private" } else { "" })).collect::<Vec<_>>().join("\n")
}

fn aws_account(v: &PlaneView) -> Option<String> {
    match &v.plane {
        Plane::Aws { account_id, .. } => Some(account_id.clone()),
        _ => v.status.as_ref().and_then(|s| s["aws"]["account_id"].as_str().filter(|_| v.aws).map(str::to_string)),
    }
}

/// What updating would ask for (by cloud): what this version asks for that the control plane's version did not, and
/// what its AWS role was found to lack (`aws_lacks`: read while signed in to AWS, whatever version asked for it).
fn new_needs(v: &PlaneView, aws_lacks: &[&'static superci_core::permissions::Need]) -> Vec<(&'static str, superci_core::permissions::Need)> {
    use superci_core::permissions::{github, AWS_NETWORK, AWS_RUNNER, GITLAB};
    let newer = |n: &superci_core::permissions::Need| view::older(v.version.as_deref(), n.since);
    let org = v.status.as_ref().is_none_or(|s| s["app"]["org"] != false);
    let mut out = vec![];
    if aws_account(v).is_some() { out.extend(AWS_RUNNER.iter().chain([&AWS_NETWORK]).filter(|n| newer(n) || aws_lacks.iter().any(|l| l.id == n.id)).map(|n| ("aws", *n))) }
    if v.github { out.extend(github(org).into_iter().map(|(_, _, n)| n).filter(|n| newer(n)).map(|n| ("github", n))) }
    if v.gitlab_url().is_some() { out.extend(GITLAB.iter().filter(|n| newer(n)).map(|n| ("gitlab", *n))) }
    out
}

/// "2 new read permissions in AWS" (or "permissions" that change things).
fn needs_text(needs: &[(&str, superci_core::permissions::Need)]) -> String {
    let mut by: Vec<(&str, usize, bool)> = vec![];
    needs.iter().for_each(|(cloud, n)| match by.iter_mut().find(|b| b.0 == *cloud) { Some(b) => { b.1 += 1; b.2 |= n.writes } None => by.push((cloud, 1, n.writes)) });
    by.iter().map(|(cloud, k, writes)| format!("{k} new {}permission{} in {}", if *writes { "" } else { "read " }, if *k == 1 { "" } else { "s" }, provider_name(cloud))).collect::<Vec<_>>().join(", ")
}

/// Each provider and code host the dashboard names, as people call it.
const PROVIDER_NAMES: [(&str, &str); 24] = [
    ("aws", "AWS"), ("cloudflare", "Cloudflare"), ("modal", "Modal"), ("googlecloud", "Google Cloud"), ("azure", "Azure"), ("hetzner", "Hetzner"),
    ("digitalocean", "DigitalOcean"), ("flydotio", "Fly.io"), ("kubernetes", "Kubernetes"), ("vercel", "Vercel"), ("netlify", "Netlify"), ("deno", "Deno Deploy"),
    ("github", "GitHub"), ("gitlab", "GitLab"), ("bitbucket", "Bitbucket"), ("forgejo", "Forgejo"), ("gitea", "Gitea"), ("azuredevops", "Azure DevOps"),
    ("machine", "Your own computer"), ("render", "Render"), ("railway", "Railway"), ("scaleway", "Scaleway"), ("vultr", "Vultr"), ("akamai", "Akamai (Linode)"),
];

/// A name from a table of (id, name), or the id itself when the table has none for it.
fn named<'a>(table: &[(&str, &'static str)], id: &'a str) -> &'a str { table.iter().find(|(k, _)| *k == id).map_or(id, |(_, name)| *name) }

fn provider_name(provider: &str) -> &str { named(&PROVIDER_NAMES, provider) }


/// One provider in a list (the connect screen's look): logo, name, what it is, and on the right its state or its action.
fn provider_row(provider: &str, what: &str, end: &str) -> String { provider_row_html(provider, &esc(what), end) }

/// A provider's row whose description is already HTML (a link in it).
fn provider_row_html(provider: &str, what: &str, end: &str) -> String {
    format!(r#"<div class="pick act">{}<span><strong>{}</strong><small>{what}</small></span><span class="pick-end">{end}</span></div>"#, logo(provider, 36), esc(provider_name(provider)))
}

/// A row's action that needs a sign-in first: connect that cloud, coming back to this page.
fn connect_action(cloud: &str) -> String {
    format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="button secondary sm">Connect</button></form>"#)
}

/// A row's action: a small form (a choice to make first, if any) and its button.
fn row_form(action: &str, fields: &str, label: &str) -> String {
    format!(r#"<form method="post" action="{action}">{fields}<button class="button primary sm">{label}</button></form>"#)
}

/// Where control planes can run, beyond the available ones.
const PLANE_SOON: [(&str, &str); 9] = [
    ("googlecloud", "Cloud Run with Firestore."), ("azure", "Azure Functions with Table Storage."),
    ("vercel", "A Vercel Function with KV."), ("netlify", "Netlify Functions with Blobs."), ("deno", "Deno Deploy with KV."), ("flydotio", "A Fly Machine with a volume."),
    ("render", "A web service with Key Value."), ("railway", "A service with a volume."), ("digitalocean", "App Platform with a managed database."),
];

/// Where runners can come from, beyond the available ones.
const RUNNERS_SOON: [(&str, &str); 10] = [
    ("machine", "Your own Mac or Linux machine, a clean VM per job."), ("googlecloud", "Compute Engine spot VMs, one per job."), ("azure", "Spot virtual machines, one per job."), ("hetzner", "Cloud servers by the hour, one per job."),
    ("digitalocean", "Droplets, one per job."), ("flydotio", "Fly Machines, one per job."), ("scaleway", "Instances, one per job."), ("vultr", "Cloud compute, one per job."),
    ("akamai", "Linode instances, one per job."), ("kubernetes", "Pods in your own cluster, one per job."),
];

/// One answer on the connect screen: a link to connecting that cloud, marked "Last used" if it is the one picked here last.
fn pick(provider: &str, sub: &str, last: bool) -> String {
    format!(r#"<a class="pick{}" href="/connect/{provider}">{}<span><strong>My {}{}</strong><small>{}</small></span><span class="pick-end">{}</span></a>"#,
        if last { " last-used" } else { "" }, logo(provider, 36), esc(provider_name(provider)), if last { r#" <span class="last">Last used</span>"# } else { "" }, esc(sub), icon("arrow"))
}

/// The clouds a control plane can be in today.
const CLOUDS: [&str; 3] = ["cloudflare", "aws", "modal"];

/// Remembers, in this browser only, which cloud was picked last on the connect screen (a name, nothing secret; a year).
fn last_used_cookie(cloud: &str) -> String { format!("superci_last={cloud}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax") }

/// Clouds not available yet, folded into one row of the same list; it opens downward into a row for each, tagged "Soon".
fn more_clouds(list: &[(&str, &str)]) -> String { more_rows("More clouds", list) }

/// Code hosts not available yet.
const HOSTS_SOON: [(&str, &str); 4] = [
    ("bitbucket", "Bitbucket Pipelines, on your runners."), ("forgejo", "Forgejo Actions, on your runners."), ("gitea", "Gitea Actions, on your runners."), ("azuredevops", "Azure Pipelines, on your agents."),
];

fn more_rows(title: &str, list: &[(&str, &str)]) -> String {
    let first: Vec<&str> = list.iter().take(3).map(|(p, _)| provider_name(p)).collect();
    let stack = list.iter().take(4).map(|(p, _)| logo(p, 16)).collect::<String>();
    let rows = list.iter().map(|(p, what)| format!(r#"<div class="pick soon">{}<span><strong>{}</strong><small>{}</small></span><span class="soon-tag">Soon</span></div>"#,
        logo(p, 36), esc(provider_name(p)), esc(what.trim_end_matches('.')))).collect::<String>();
    format!(r#"<details class="more"><summary class="pick"><span class="stack">{stack}</span><span><strong>{}</strong><small>{} and {} more, coming soon</small></span><span class="pick-end"><svg class="ic chev" viewBox="0 0 24 24"><path d="m6 9 6 6 6-6"/></svg></span></summary><div class="picks">{rows}</div></details>"#,
        esc(title), esc(&first.join(", ")), list.len().saturating_sub(3))
}

/// On the way to a cloud's own sign-in. The page replaces itself, so Back from the sign-in returns to the dashboard.
fn leave(url: &str, cloud: &str) -> Response {
    let js = serde_json::to_string(url).unwrap_or_default();
    document(200, "SuperCI", &format!(r#"<div class="wait"><div>{MARK}<h1>Opening {}…</h1><p>Approve there, and you come straight back.</p><div class="spinner"></div></div></div><script>location.replace({js})</script><noscript><meta http-equiv="refresh" content="0;url={}"></noscript>"#, esc(cloud), esc(url)), None)
}

/// A figure that is still being read.
/// A list row that is still being read.
const SK_ROWS: &str = r#"<div class="row sk-row"><span class="dot"></span><span><span class="sk w40"></span><span class="sk w30 thin"></span></span><span class="sk pillish"></span></div>"#;

/// A page while its live part is on its way: its own title, and the shapes of what will be there (Overview's figures and
/// jobs, other pages' lists). After a moment it says what it is waiting for.
fn skeleton(section: &str, title: &str) -> String {
    // A provider-like row: logo, two lines, an action.
    const SK_PICK: &str = r#"<div class="pick act sk-pick"><span class="sk logo-sk"></span><span><span class="sk w30"></span><span class="sk w60 thin"></span></span><span class="sk pillish"></span></div>"#;
    let card = |heading: &str, loading: bool, body: String| format!(r#"<section class="card"><div class="card-head"><h2>{heading}</h2>{}</div>{body}</section>"#, if loading { LOADING } else { "" });
    let body = match section {
        "overview" => format!(r#"<div class="cards">{}</div>"#, overview_skeleton_cards()),
        "repos" => format!(r#"<div class="cards">{}</div>"#,
            card("Where jobs come from", true, format!(r#"<span class="sk w70 thin"></span><div class="picks">{}</div>"#, SK_PICK.repeat(3)))),
        "gitlab" => format!(r#"<div class="cards">{}{}</div>"#, card("Projects", true, format!(r#"<span class="sk w70 thin"></span><div class="rows">{}</div>"#, SK_ROWS.repeat(5))), card("Connection", false, format!(r#"<div class="picks">{SK_PICK}</div>"#))),
        "runners" => format!(r#"<div class="cards">{}</div>"#,
            card("Runner providers", true, format!(r#"<span class="sk w70 thin"></span><ol class="pools">{}</ol>"#,
                r#"<li class="pool sk-pool"><span class="grip"></span><span class="sk logo-sk"></span><span class="pool-main"><span class="sk w30"></span><span class="sk w60 thin"></span></span><span class="sk pillish"></span><span></span></li>"#.repeat(2)))),
        "workflows" => format!(r#"<div class="cards"><section class="card dm-card"><div class="default-machine">{}</div></section>{}</div>"#,
            r#"<span class="sk logo-sk"></span><span class="dm-main"><span class="sk w30"></span><span class="sk w60 thin"></span></span><span class="sk pillish"></span><span></span>"#,
            card("In your workflows", true, r#"<span class="sk w60"></span><span class="sk block"></span><span class="sk w70 thin"></span>"#.to_string())),
        "settings" => format!(r#"<div class="cards"><section class="card dm-card">{}</section>{}</div>"#,
            r#"<div class="default-machine"><span class="sk logo-sk"></span><span class="dm-main"><span class="sk w30"></span><span class="sk w60 thin"></span></span><span class="sk pillish"></span><span></span></div>"#.repeat(2),
            card("Limits", true, r#"<span class="sk w60"></span><span class="sk w70 thin"></span><span class="sk w60"></span><span class="sk w70 thin"></span>"#.to_string())),
        "jobs" | "runs" => format!(r#"<div class="cards">{}</div>"#, card(if section == "runs" { "Recent runs" } else { "Recent jobs" }, true, format!(r#"<div class="rows">{}</div>"#, SK_ROWS.repeat(6)))),
        "planes" => format!(r#"<div class="cards">{}</div>"#,
            card("Your control plane", true, format!(r#"<span class="sk w70 thin"></span><div class="picks">{SK_PICK}</div><span class="sk w70 thin"></span>"#))),
        "changes" => format!(r#"<div class="cards">{}</div>"#,
            card("", true, r#"<span class="sk w30"></span><span class="sk w70 thin"></span><span class="sk w60 thin"></span><span class="sk w70 thin"></span><span class="sk w30"></span><span class="sk w60 thin"></span><span class="sk w70 thin"></span>"#.to_string())),
        "add-runners" | "plane" => format!(r#"<p class="note" style="margin-bottom:22px"><span class="sk w60 thin"></span></p><div class="picks page">{}</div>"#, SK_PICK.repeat(4)),
        _ => format!(r#"<div class="cards">{}</div>"#, card("", true, format!(r#"<div class="rows">{}</div>"#, SK_ROWS.repeat(4)))),
    };
    let bar = if section == "add-runners" { format!(r#"<div class="bar"><h1 class="page-title">{}</h1><span class="note">{LOADING}</span></div>"#, esc(title)) } else { format!(r#"<div class="bar"><h1 class="page-title">{}</h1></div>"#, esc(title)) };
    // Marked as loading this page: one loading answer after another keeps the one shown (its shimmer runs on).
    format!(r#"<div data-loading="{section}">{bar}{body}</div>"#)
}

/// A friendly empty place: an icon tile, what belongs here, and the one thing to do.
fn empty_state(icon_svg: &str, title: &str, text: &str, action: &str) -> String {
    format!(r#"<div class="empty-state"><span class="tile">{icon_svg}</span><h3>{}</h3><p>{}</p><div class="actions">{action}</div></div>"#, esc(title), esc(text))
}

fn section_name(section: &str) -> &str {
    match section {
        s if ["plane", "planes", "all", "changes", "runners", "add-runners", "workflows", "settings", "jobs", "job", "runs", "run", "repos", "gitlab"].contains(&s) => s,
        _ => "overview",
    }
}

/// How long pages wait for a control plane (to answer at all, to take this computer's key) before saying why not.
const KEY_WAIT_MS: u64 = 30_000;
fn key_wait_ms() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_KEY_WAIT_S").ok().and_then(|v| v.parse::<u64>().ok()) { return s * 1000 }
    KEY_WAIT_MS
}

/// How long a reading of the control planes serves the pages asked for meanwhile, before they are read again.
fn views_kept_ms() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_VIEWS_S").ok().and_then(|v| v.parse::<u64>().ok()) { return s * 1000 }
    120_000
}

/// How long a control plane that answered a moment ago and does not now is shown as it was (a passing miss).
fn last_good_ms() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_LAST_GOOD_S").ok().and_then(|v| v.parse::<u64>().ok()) { return s * 1000 }
    60_000
}

/// Each section's title (any other: the overview).
const SECTION_TITLES: [(&str, &str); 14] = [
    ("plane", "Set up a control plane"), ("planes", "Control plane"), ("changes", "Changelog"), ("add-runners", "Add a provider"), ("runners", "Runners"),
    ("workflows", "Workflows"), ("settings", "Settings"), ("all", "Control planes"), ("jobs", "Jobs"), ("runs", "Runs"), ("job", "Job"), ("run", "Run"), ("repos", "Repositories"), ("gitlab", "GitLab"),
];

fn section_title(section: &str) -> &'static str { SECTION_TITLES.iter().find(|(k, _)| *k == section).map_or("Overview", |(_, title)| *title) }


/// Each place a job can run, as the dashboard names it.
const CLOUD_NAMES: [(&str, &str); 5] = [("aws", "AWS"), (AWS_ON_DEMAND, "AWS on-demand"), ("cloudflare", "Cloudflare"), ("modal", "Modal"), ("machine", "Your computer")];

fn cloud_name(cloud: &str) -> &str { named(&CLOUD_NAMES, cloud) }

/// A control plane older than this dashboard, for the sidebar on every page: the versions, a link to what is new, and
/// its Update button.
fn side_update(v: &PlaneView, updating: Option<&Update>, aws_lacks: &[&'static superci_core::permissions::Need], signed_in: bool) -> String {
    let action = if let Some(u) = updating.filter(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(())))) { update_progress(u) }
        else if matches!(v.plane, Plane::Seen { .. }) { r#"<small>Sign in where it runs to update it.</small>"#.to_string() }
        // Its cloud's sign-in has ended: signing in comes first, and back here.
        else if !signed_in { signin_button(v.plane.cloud(), &format!("Sign in with {} to update", provider_name(v.plane.cloud())), true).replace(r#"class="button primary""#, r#"class="button primary sm""#) }
        else { format!(r#"<form method="post" action="/plane/update" onsubmit="var b=this.querySelector('button');b.disabled=true;b.textContent='Updating…'"><input type="hidden" name="next"><button class="button primary sm">Update to {DASHBOARD_VERSION}</button></form>"#) };
    // While it updates, and once it has: said so, on every page.
    match updating.filter(|u| u.plane == v.plane.plane_id()).map(|u| &u.result) {
        Some(None) => return format!(r#"<div class="side-update"><strong><span class="dot accent"></span>Updating to {DASHBOARD_VERSION}</strong><small>It keeps going while you look around. <a href="/?p=changes">Changelog</a></small>{action}</div>"#),
        Some(Some(Ok(()))) if updating.is_some_and(|u| u.restarting(v)) => return format!(r#"<div class="side-update"><strong><span class="dot accent"></span>Restarting with {DASHBOARD_VERSION}</strong><small>Uploaded; its cloud is switching it over, which can take a few minutes. <a href="/?p=changes">What changed</a></small><div data-refresh="3"></div></div>"#),
        Some(Some(Ok(()))) if v.outdated() => {}
        Some(Some(Ok(()))) => return format!(r#"<div class="side-update"><strong><span class="dot good"></span>Updated to {DASHBOARD_VERSION}</strong><small>{}<a href="/?p=changes">What changed</a></small></div>"#, updating.map(|u| plain(&u.note)).filter(|n| !n.is_empty()).map(|n| format!("{} ", esc(&n.replace('`', "")))).unwrap_or_default()),
        _ => {}
    }
    let needs = new_needs(v, aws_lacks);
    let asks = if needs.is_empty() { String::new() } else { format!(r#"<small class="asks">It also asks for {}.</small>"#, esc(&needs_text(&needs))) };
    format!(r#"<div class="side-update"><strong><span class="dot open"></span>Update available</strong><small>Your control plane runs {}. <a href="/?p=changes">Changelog</a></small>{asks}{action}</div>"#, esc(&v.version_name()))
}

/// An update's progress: the step it is at and a slim bar; how it ended when it stopped (with Try again).
fn update_progress(u: &Update) -> String {
    let n = u.steps.len();
    match &u.result {
        Some(Err(e)) => format!(r#"<div class="upd"><span class="bad-text">The update stopped: {}</span><form method="post" action="/plane/update"><input type="hidden" name="next"><button class="button secondary sm">Try again</button></form></div>"#, esc(&e.chars().take(160).collect::<String>())),
        // (Under way: one that ended well is not shown here, see `side_update`.)
        _ => format!(r#"<div class="upd"><span class="upd-step"><span class="mini-spin"></span>{}</span><span class="slim"><i style="width:{}%"></i></span></div>"#, esc(u.steps.get(u.at).copied().unwrap_or("Finishing")), (u.at * 100 + 50) / n.max(1)),
    }
}

/// The changelog (CHANGELOG.md, in this program): each version with its date and changes.
const CHANGELOG: &str = include_str!(env!("SUPERCI_CHANGELOG"));

fn changelog() -> Vec<(String, String, Vec<String>)> {
    let mut out: Vec<(String, String, Vec<String>)> = vec![];
    CHANGELOG.lines().for_each(|line| {
        if let Some(h) = line.strip_prefix("## ") {
            let (v, date) = h.split_once(" — ").unwrap_or((h, ""));
            out.push((v.trim().to_string(), date.trim().to_string(), vec![]));
        } else if let (Some(item), Some(last)) = (line.strip_prefix("- "), out.last_mut()) { last.2.push(item.to_string()) }
    });
    // (A version newer than this dashboard, not released yet in a build from the repository, is not one to offer.)
    out.retain(|(v, _, _)| !view::older(Some(DASHBOARD_VERSION), v));
    out
}

/// Changelog text as HTML: escaped, with `code` kept.
fn change_html(text: &str) -> String {
    esc(text).split('`').enumerate().map(|(i, part)| if i % 2 == 1 { format!("<code>{part}</code>") } else { part.to_string() }).collect()
}

/// The page's own behaviour, for every fragment it shows: the machine picker (its label and which pools can run it) and
/// dragging runner pools into a new order (saved when dropped).
const PAGE_JS: &str = r#"<script>document.addEventListener('click',function(e){document.querySelectorAll('details.menu[open]').forEach(function(d){if(!d.contains(e.target))d.open=false})});
(function(){var sm=HTMLDialogElement.prototype.showModal;HTMLDialogElement.prototype.showModal=function(){sm.call(this);if(!this.querySelector('[autofocus]')){this.setAttribute('tabindex','-1');this.focus()}}})();

function superciFit(k,c,r,d,arch,os,gpu,img){var cpu=c||k.std_cpu,ram=r||(c?Math.min(c*k.ram_per_cpu,k.max_ram_gb):k.std_ram_gb);ram=Math.max(ram,cpu*k.min_ram_per_cpu);var disk=d||k.std_disk_gb,gs=k.gpus||[];
var ok=cpu<=k.max_cpu&&ram<=k.max_ram_gb&&disk<=k.max_disk_gb&&k.arch.indexOf(arch)>=0&&k.os.indexOf(os)>=0&&!(os==='windows'&&arch!=='x64')&&!(img&&(k.images||[]).indexOf(img)<0);
var g=!gpu?'':gpu==='gpu'?gs[0]:gs.indexOf(gpu)>=0?gpu:null;if(gpu&&(!g||os==='windows'||arch!=='x64'))ok=false;
return !ok?null:k.ram_per_cpu?{cpu:cpu,ram:ram,gpu:g}:{cpu:k.max_cpu,ram:k.max_ram_gb,gpu:g}}
function superciPrompt(){var d=document.getElementById('agent-prompt');if(!d)return;superciPromptUpdate(d);d.showModal()}
function superciPromptUpdate(d){var g=document.getElementById('use-gl'),host=g?(g.checked?'gl':'gh'):d.dataset.host,c=d.querySelector('input[name=superci-prompt]:checked'),pv=d.querySelector('[data-preview]'),cp=d.querySelector('[data-copy]');
d.superciText=c.dataset[host];pv.innerHTML='';d.superciText.split(/(`[^`\n]+`)/).forEach(function(p){if(/^`.*`$/.test(p)){var k=document.createElement('code');k.textContent=p.slice(1,-1);pv.appendChild(k)}else pv.appendChild(document.createTextNode(p))});
d.querySelectorAll('.prompt-modal-choice').forEach(function(r){r.classList.toggle('is-selected',r.querySelector('input').checked)});
cp.classList.remove('is-copied');cp.querySelector('svg').outerHTML=SUPERCI_COPY;cp.querySelector('span').textContent='Copy prompt';d.querySelector('.prompt-copy-error').hidden=true}
var SUPERCI_COPY='<svg viewBox="0 0 20 20" aria-hidden="true"><rect x="7" y="3" width="10" height="10" rx="2"/><path d="M13 13v2a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2h2"/></svg>';
document.addEventListener('change',function(e){if(e.target.name==='superci-prompt')superciPromptUpdate(e.target.closest('dialog'))});
function superciWhere(f){var own=f.querySelector('input[type=radio][value=own]'),box=f.querySelector('[data-own]'),i=box.querySelector('input'),on=own.checked;f.querySelectorAll('.prompt-modal-choice').forEach(function(r){r.classList.toggle('is-selected',r.querySelector('input').checked)});box.hidden=!on;i.required=on;if(!on)i.value='';if(f.hasAttribute('data-gitlab')){var u=on?i.value.trim().replace(/\/+$/,''):'https://gitlab.com';f.querySelector('input[name=url]').value=u;var m=f.querySelector('[data-make]');m.href=(u||'https://gitlab.com')+m.dataset.path}}
document.addEventListener('change',function(e){var t=e.target;if(t.type==='radio'&&(t.name==='gh-where'||t.name==='gl-where')){superciWhere(t.form);if(t.value==='own'&&t.checked)t.form.querySelector('[data-own] input').focus()}});
document.addEventListener('input',function(e){if(e.target.name==='own_url')superciWhere(e.target.form)});
document.addEventListener('click',function(e){var cp=e.target.closest&&e.target.closest('[data-copy]');if(!cp)return;var d=cp.closest('dialog'),t=d.superciText,er=d.querySelector('.prompt-copy-error');
navigator.clipboard.writeText(t).then(function(){if(t!==d.superciText)return;cp.classList.add('is-copied');cp.querySelector('svg').outerHTML='<svg viewBox="0 0 20 20" aria-hidden="true"><path d="m4 10 4 4 8-8"/></svg>';cp.querySelector('span').textContent='Copied'},function(){er.hidden=false;er.textContent='Copy failed. Select the prompt and copy it manually.'})});
function superciPick(f){var d=JSON.parse(f.dataset.default),label=f.dataset.mode==='label',g=function(n){var e=f.elements[n];return !e?'':e.type==='checkbox'?(e.checked&&!e.disabled):e.value};
var cpu=+g('cpu')||0,ram=+g('ram')||0,disk=+g('disk')||0,arch=g('arch'),os=g('os'),gpu=g('gpu'),od=g('nospot'),img=g('image');
// (An Ubuntu is for Linux: no choice while the machine is Windows.)
var ie=f.elements.image,win=(os||(label?d.os:os))==='windows';if(ie)ie.disabled=win;if(win)img='';
if(label){var parts=[];if(cpu)parts.push('cpu='+cpu);if(ram)parts.push('ram='+ram);if(disk)parts.push('disk='+disk);if(arch)parts.push('arch='+arch);if(os)parts.push('os='+os);if(img)parts.push('image='+img);if(gpu)parts.push('gpu='+(gpu==='gpu'?'any':gpu));if(od)parts.push('spot=false');f.querySelector('[data-label]').textContent=f.dataset.base+(parts.length?'/'+parts.join('/'):'');
ram=ram||(cpu?0:d.ram);cpu=cpu||d.cpu;disk=disk||d.disk;arch=arch||d.arch;os=os||d.os;if(!img&&!win)img=d.image||''}
f.querySelectorAll('.fit').forEach(function(x){var got=null;JSON.parse(x.dataset.caps).some(function(k){got=superciFit(k,cpu,ram,disk,arch,os,gpu,img);return got});x.classList.toggle('no',!got);var s=x.querySelector('[data-size]');if(s)s.textContent=got?got.cpu+' CPU · '+got.ram+' GB'+(got.gpu?' · '+got.gpu.toUpperCase():''):''})}
(function(){var dragged=null;
window.superciRegions=function(f){f.querySelectorAll('input[data-rg]').forEach(function(i){i.remove()});f.querySelectorAll('.rg').forEach(function(li,n){var x=document.createElement('input');x.type='hidden';x.name='region_'+n;x.value=li.dataset.region;x.setAttribute('data-rg','');f.appendChild(x)})};
window.superciRegionRemove=function(b){var li=b.closest('.rg'),list=li.parentNode;if(list.querySelectorAll('.rg').length<2)return;var sel=list.parentNode.querySelector('.rg-add'),o=document.createElement('option');o.value=li.dataset.region;o.textContent=li.dataset.name+' · '+li.dataset.region;sel.appendChild(o);li.remove()};
window.superciRegionAdd=function(sel){var v=sel.value;if(!v)return;var list=sel.parentNode.querySelector('.rg-list'),tpl=list.querySelector('.rg').cloneNode(true),name=sel.options[sel.selectedIndex].textContent.split(' · ')[0];tpl.dataset.region=v;tpl.dataset.name=name;tpl.querySelector('strong').textContent=name;tpl.querySelector('small').textContent=v;tpl.querySelector('button').setAttribute('aria-label','Remove '+name);list.appendChild(tpl);sel.remove(sel.selectedIndex);sel.value=''};
document.addEventListener('dragstart',function(e){var li=e.target.closest&&e.target.closest('.pool,.rg');if(!li)return;dragged=li;li.classList.add('dragging');e.dataTransfer.effectAllowed='move';e.dataTransfer.setData('text/plain',li.dataset.cloud)});
document.addEventListener('dragover',function(e){if(!dragged)return;var li=e.target.closest&&e.target.closest(dragged.classList.contains('rg')?'.rg':'.pool');e.preventDefault();if(!li||li===dragged||li.parentNode!==dragged.parentNode)return;var r=li.getBoundingClientRect();li.parentNode.insertBefore(dragged,e.clientY-r.top>r.height/2?li.nextSibling:li)});
document.addEventListener('drop',function(e){if(dragged)e.preventDefault()});
document.addEventListener('dragend',function(){if(!dragged)return;var li=dragged,list=li.parentNode;dragged=null;li.classList.remove('dragging');if(li.classList.contains('rg'))return;var order=[].map.call(list.querySelectorAll('.pool'),function(x){return x.dataset.cloud});if(order.join()===list.dataset.order)return;
var before=list.dataset.order.split(','),note=document.getElementById('order-note'),body=new URLSearchParams();body.append('action','order');order.forEach(function(c,i){body.append('cloud_'+i,c)});
function say(c,h){if(note){note.className='order-note '+c;note.innerHTML=h}}
function keep(o){list.dataset.order=o;document.querySelectorAll('input[name=current]').forEach(function(i){i.value=o})}
keep(order.join());li.classList.add('moved');say('saving','<span class="mini-spin"></span>Saving the order…');
var qs=['w','c'].filter(function(n){return new URLSearchParams(location.search).get(n)}).map(function(n){return n+'='+encodeURIComponent(new URLSearchParams(location.search).get(n))}).join('&');
fetch('/routing'+(qs?'?'+qs:''),{method:'POST',body:body,credentials:'same-origin'}).then(function(r){if(!r.ok)throw 0;li.classList.remove('moved');say('saved','<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>Order saved');setTimeout(function(){if(note&&note.classList.contains('saved'))say('','')},2500)})
.catch(function(){before.forEach(function(c){var x=list.querySelector('.pool[data-cloud="'+c+'"]');if(x)list.appendChild(x)});keep(before.join());li.classList.remove('moved');say('bad','The order was not saved; it is back as it was.')})});})();
</script>"#;

/// A pool's name in the order: a cloud, or a further Modal workspace's (`modal:<workspace>`).
fn pool_name_ok(name: &str) -> bool {
    ["aws", AWS_ON_DEMAND, "cloudflare", "modal"].contains(&name)
        || name.strip_prefix("modal:").is_some_and(|w| !w.is_empty() && w.len() <= 64 && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

/// One numbered step of Set up: its number (a check once done), its title and what it says or offers.
fn setup_step(n: u8, state: &str, title: String, body: String) -> String {
    format!(r#"<div class="step {state}"><div class="num">{}</div><div>{title}{body}</div></div>"#, if state == "done" { icon("check") } else { n.to_string() })
}

/// What a step of Set up waits for.
fn setup_locked(what: &str) -> String { format!(r#"<div class="locked-note">{}Needs {what} first</div>"#, icon("lock")) }

/// What is said of an image name the control plane does not have (`superci runner_images update`).
pub fn no_image(name: &str) -> String { format!("There is no image {name} here (`superci runner_images list`). `superci runner_images create {name} --reference=REGISTRY/IMAGE:TAG` adds it.") }

/// The order as a page showed it (`current`: every place, taken out ones too, by name), each with its stored settings,
/// and any stored place the page did not show after them.
fn order_as_shown(routing: &Routing, current: &str) -> Vec<Pool> {
    let mut order: Vec<Pool> = vec![];
    for c in current.split(',').filter(|c| pool_name_ok(c)) { if !order.iter().any(|p| p.cloud == c) { order.push(routing.pool(c).cloned().unwrap_or(Pool::of(c))) } }
    for p in &routing.order { if !order.iter().any(|o| o.cloud == p.cloud) { order.push(p.clone()) } }
    order
}

/// Regions as a row says them: the first, then the others.
fn regions_text(regions: &[String]) -> String {
    match regions.split_first() {
        Some((first, [])) => region_short(first).to_string(),
        Some((first, rest)) => format!("{}, then {}", region_short(first), rest.iter().map(|r| region_short(r)).collect::<Vec<_>>().join(", ")),
        None => String::new(),
    }
}

/// A place in the order as shown: its logo key, its name, and what it starts and where.
fn pool_text(v: &PlaneView, pool: &Pool) -> (String, String, String) {
    let at = |p: &Pool| { let r = regions_text(&v.pool_regions(p)); if r.is_empty() { r } else { format!(" · {r}") } };
    let (name, what) = match pool.cloud.as_str() {
        "aws" => ("AWS spot".to_string(), format!("Spot machines of any size · x64, arm64{}", at(pool))),
        AWS_ON_DEMAND => ("AWS on-demand".to_string(), format!("On-demand machines of any size, never taken back · x64, arm64{}", at(pool))),
        "cloudflare" => (cloud_name("cloudflare").to_string(), "Containers sized per job · up to 4 CPU, 12 GB, x64".into()),
        "modal" => (cloud_name("modal").to_string(), "Sandboxes · up to 64 CPU, x64".into()),
        c if superci_core::plane::kind_of(c) == "modal" => (format!("Modal {}", &c["modal:".len()..]), "Sandboxes in a workspace of its own · up to 64 CPU, x64".into()),
        c => (cloud_name(c).to_string(), String::new()),
    };
    // AWS's on-demand machines carry AWS's logo.
    (superci_core::plane::kind_of(superci_core::plane::account_of(&pool.cloud)).to_string(), name, what)
}

/// What each pool can run, for the machine picker: its capacity (the largest machine, systems, standard machine and
/// memory rule).
fn pool_caps(pool: &str) -> serde_json::Value {
    match superci_core::plane::kind_of(pool) {
        "aws" => serde_json::json!([Capacity::aws()]),
        "cloudflare" => serde_json::json!([Capacity::cloudflare()]),
        "modal" => serde_json::json!([Capacity::modal()]),
        _ => serde_json::json!([]),
    }
}

const ICON_GEAR: &str = r#"<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z"/></svg>"#;
const ICON_GRIP: &str = r#"<svg viewBox="0 0 24 24"><circle cx="9" cy="6" r="1.4"/><circle cx="15" cy="6" r="1.4"/><circle cx="9" cy="12" r="1.4"/><circle cx="15" cy="12" r="1.4"/><circle cx="9" cy="18" r="1.4"/><circle cx="15" cy="18" r="1.4"/></svg>"#;

/// An AWS place's regions, in order, as its settings edit them: the regions the account may use, each with what the
/// account allows there (spot's: how many spot CPUs), and where the runner image is not yet.
fn regions_field(v: &PlaneView, p: &Pool, quotas: &HashMap<String, Result<u32>>, look: Option<&aws::Look>) -> String {
    let spot = p.cloud == "aws";
    let list = v.pool_regions(p);
    let best = closest(Some(v));
    let row = |r: &str| {
        let quota = match quotas.get(r) { Some(Ok(n)) if spot => format!(" · up to {n} spot CPUs"), _ => String::new() };
        let quota = if r == "us-east-1" { format!("{quota} · {best}") } else { quota };
        format!(r#"<li class="rg" draggable="true" data-region="{r}" data-name="{}"><span class="grip" title="Drag to reorder">{ICON_GRIP}</span><span class="rg-main"><strong>{}</strong><small>{r}{quota}</small></span><button type="button" class="icon-btn" aria-label="Remove {}" onclick="superciRegionRemove(this)"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></li>"#,
            esc(region_name(r)), esc(region_name(r)), esc(region_name(r)))
    };
    let rows = list.iter().map(|r| row(r)).collect::<String>();
    // (A region the account cannot use is shown, and cannot be chosen: why is said beside it.)
    let more = REGIONS.iter().filter(|r| !list.iter().any(|l| l == *r)).map(|r| match region_unusable(look, r) {
        Some(why) => format!(r#"<option value="{r}" disabled>{} · {r} · {why}</option>"#, region_name(r)),
        None => format!(r#"<option value="{r}">{} · {r}{}</option>"#, region_name(r), if *r == "us-east-1" { format!(" · {best}") } else { String::new() }),
    }).collect::<String>();
    // A region the account may use and the runner image is not in: where it is copied in.
    let image_first = match look.map(|l| l.regions_without_image()).filter(|w| !w.is_empty()) {
        Some(without) => format!(r#" The runner image is not in {} yet: <a href="/?p=planes#aws-account">copy it in</a> under Control plane, then add the region here."#, esc(&without.iter().map(|r| region_short(r)).collect::<Vec<_>>().join(", "))),
        None => String::new(),
    };
    let quota = if !spot { String::new() } else {
        let total: u32 = list.iter().filter_map(|r| quotas.get(r).and_then(|q| q.as_ref().ok())).sum();
        let low = list.iter().find(|r| matches!(quotas.get(*r), Some(Ok(n)) if *n < 16));
        match (total, low) {
            (0, _) => String::new(),
            (n, Some(r)) => format!(r#" Your account may run {n} spot CPUs at once in these. <a href="{}" target="_blank" rel="noopener">Request more in {} ↗</a>"#, esc(&superci_core::aws::spot_quota_link(r)), region_short(r)),
            (n, None) => format!(" Your account may run up to {n} spot CPUs at once in these ({} of 4 CPU).", jobs_word(n / 4)),
        }
    };
    // (Spot: where a job costs least, the first a little favoured. On-demand: in order.)
    let how = if spot { "Spot machines start where a job costs least, the first region counted 15% cheaper (its cache is there); when one has no spot machine left, in the next." }
        else { "Machines start in the first; when it has no room, in the next." };
    format!(r#"<div class="field"><span>Regions</span><ol class="rg-list" data-regions>{rows}</ol><select class="rg-add" aria-label="Add a region" onchange="superciRegionAdd(this)"><option value="">+ Add a region…</option>{more}</select><small>{how} Drag to reorder.{quota}{image_first}</small></div>"#)
}

/// The places jobs go, in order: each starts a fresh runner per job; a job goes to the first that can run its machine
/// and has room. Every row is alike (AWS's spot machines, its on-demand ones, Cloudflare, Modal): dragged to reorder,
/// with its settings in a dialog (jobs at once, a monthly budget, where it starts machines), and taken out of the order
/// there; one taken out is listed below the order with a way to put it back.
fn pools_card(v: &PlaneView, quotas: &HashMap<String, Result<u32>>, cf_month: Option<(f64, f64)>, modal_month: Option<&modal::Month>, look: Option<&aws::Look>) -> String {
    let order = v.order();
    let jobs = v.jobs();
    let current = order.iter().map(|p| p.cloud.as_str()).collect::<Vec<_>>().join(",");
    let shown = order.iter().filter(|p| !p.off).map(|p| p.cloud.as_str()).collect::<Vec<_>>().join(",");
    let mut rows = String::new();
    let mut dialogs = String::new();
    let mut out = String::new();
    let close = r#"<button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>"#;
    // (Shown beside a control plane that has a place in its order: never with none.)
    order.iter().enumerate().for_each(|(i, p)| {
        let (logo_key, name, what) = pool_text(v, p);
        // Taken out: a line below the order, with the way back in.
        if p.off {
            out.push_str(&format!(r#"<div class="limit"><span class="limit-text"><strong>{}</strong><small>Taken out of the order: no job goes to it. Its settings are kept.</small></span><form method="post" action="/routing" class="limit-do"><input type="hidden" name="action" value="put_back"><input type="hidden" name="cloud" value="{}"><input type="hidden" name="current" value="{}"><button class="chip">Put back</button></form></div>"#,
                esc(&name), esc(&p.cloud), esc(&current)));
            return
        }
        // AWS's spot machines in an account that may start none (its organization's rule, or its sign-in's): said
        // on the row and in its settings, in "ready"'s place. Its jobs go on down the order.
        let no_spot = match look.and_then(|l| l.spot.as_ref()).filter(|_| p.cloud == "aws") {
            Some(aws::Answer::Refused(r)) => Some(if matches!(r.by, superci_core::aws::By::Organization | superci_core::aws::By::OrganizationResources) { "your AWS organization" } else { "the role or user signed in as" }),
            _ => None,
        };
        let what = match no_spot { Some(whose) => format!("Spot machines are not allowed by {whose}: its jobs go to the next provider in the order"), None => what };
        let state = if no_spot.is_some() { pill("open", "not allowed") } else { pill("good", "ready") };
        let spent = v.spend(&p.cloud);
        let mut limits = vec![];
        if let Some(n) = p.max_jobs { limits.push(format!("{n} at once")) }
        if let Some(cap) = p.monthly_usd { limits.push(format!("{} of {}", usd(spent), usd(cap))) }
        // Modal in Function mode: so said, with its preemptions this month (all of Modal's together).
        if p.function { limits.push("Functions".into()) }
        let preempted = v.status.as_ref().and_then(|s| s["preempted"]["month"].as_u64()).unwrap_or(0);
        if p.function && preempted > 0 { limits.push(format!("{preempted} preempted · {} run again", v.status.as_ref().and_then(|s| s["preempted"]["rerun"].as_u64()).unwrap_or(0))) }
        let mut limits = limits.iter().map(|l| format!(r#"<span class="lim">{}</span>"#, esc(l))).collect::<String>();
        limits.push_str(&month_text(&p.cloud, spent, p.monthly_usd.is_none(), cf_month, modal_month));
        rows.push_str(&format!(r#"<li class="pool" draggable="true" data-cloud="{}"><span class="grip" title="Drag to reorder">{ICON_GRIP}</span>{}<span class="pool-main"><strong>{}</strong><small>{}</small></span><span class="pool-now">{limits}{state}</span><button type="button" class="icon-btn" aria-label="{} settings" onclick="document.getElementById('pool-{i}').showModal()">{ICON_GEAR}</button></li>"#,
            esc(&p.cloud), logo(&logo_key, 32), esc(&name), esc(&what), esc(&name)));
        // Its settings, alike for every row: jobs at once, a monthly budget, and where it starts machines.
        let value = |n: Option<String>| n.map(|n| format!(r#" value="{n}""#)).unwrap_or_default();
        let budget = format!(r#"<label class="field"><span>Monthly budget</span><span class="money">$<input type="number" name="usd" min="1" step="1" placeholder="No limit"{}></span><small>{} this month. At the budget, jobs go to the next provider.</small></label>"#,
            value(p.monthly_usd.map(|n| format!("{n:.0}"))), usd(spent));
        let place = if superci_core::plane::account_of(&p.cloud) == "aws" && v.status.as_ref().is_some_and(|s| s.get("aws_regions").is_some()) { regions_field(v, p, quotas, look) }
            else if p.cloud == "cloudflare" && v.status.as_ref().is_some_and(|s| s.get("cloudflare_location").is_some()) {
                // Cloudflare: where its containers start.
                let current = v.status.as_ref().and_then(|s| s["cloudflare_location"].as_str().map(str::to_string)).unwrap_or_else(|| "enam".into());
                let best = closest(Some(v));
                let opts = CF_LOCATIONS.iter().map(|(k, name)| format!(r#"<option value="{k}"{}>{name}{}</option>"#, if *k == current { " selected" } else { "" }, if *k == "enam" { format!(" · {best}") } else { String::new() })).collect::<String>();
                format!(r#"<label class="field"><span>Location</span><select name="location">{opts}</select><small>Where its containers start. Cloudflare places each one; this is the area it starts in.</small></label>"#)
            } else if superci_core::plane::kind_of(&p.cloud) == "modal" && v.status.as_ref().is_some_and(|s| s["function_mode"] == true) {
                // Modal: its jobs as sandboxes or as Functions.
                let opt = |m: &str, name: &str, on: bool| format!(r#"<option value="{m}"{}>{name}</option>"#, if on { " selected" } else { "" });
                format!(r#"<label class="field"><span>Runs jobs as</span><select name="mode">{}{}</select><small>A Function costs a third of a sandbox. Modal may preempt it: its job is then run again on a sandbox once its run has finished.</small></label>"#,
                    opt("sandbox", "Sandboxes", !p.function), opt("function", "Functions (preemptible)", p.function))
            } else { String::new() };
        // (Taking it out keeps its settings; nothing is deleted anywhere.)
        let take_out = r#"<button type="button" class="button secondary sm" style="margin-right:auto" onclick="var f=this.form;f.elements.action.value='take_out';f.requestSubmit()">Take out of the order</button>"#;
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="pool-{i}" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body" onsubmit="superciRegions(this)"><input type="hidden" name="action" value="pool"><input type="hidden" name="cloud" value="{}"><input type="hidden" name="current" value="{}"><div class="dlg-head">{}<span><strong>{}</strong><small>{}</small></span>{close}</div><label class="field"><span>Jobs at once</span><input type="number" name="max" min="1" max="999" placeholder="No limit"{}><small>When this many run, the next job goes to the next provider (or waits).</small></label>{budget}{place}<div class="dlg-foot">{take_out}<button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>"#,
            esc(&p.cloud), esc(&current), logo(&logo_key, 36), esc(&name), esc(&what), value(p.max_jobs.map(|n| n.to_string()))));
    });
    let waiting: Vec<&serde_json::Value> = jobs.iter().filter(|j| j["state"] == "waiting").collect();
    let waiting = match waiting.as_slice() {
        [] => String::new(),
        [j, ..] => format!(r#"<div class="waiting">{} {} · {}</div>"#, pill("open", "Waiting"), if waiting.len() == 1 { "1 job".to_string() } else { format!("{} jobs", waiting.len()) },
            esc(j["error"].as_str().unwrap_or_default().trim_start_matches("waiting: "))),
    };
    // Providers in the order that this control plane has not added (after a move: they belong to the one before).
    let clouds = v.clouds();
    let missing: Vec<String> = v.routing().order.iter().map(|p| p.cloud.clone()).filter(|c| !c.starts_with("machine:") && !clouds.contains(c) && !(c == AWS_ON_DEMAND && clouds.iter().any(|c| c == "aws"))).collect();
    let waiting = if missing.is_empty() { waiting } else {
        format!(r#"{waiting}<div class="waiting">{} {} {} not added to this control plane yet · <a href="/?p=add-runners">Add {}</a></div>"#, pill("open", "Not added"),
            esc(&missing.iter().map(|c| cloud_name(c)).collect::<Vec<_>>().join(", ")), if missing.len() == 1 { "is" } else { "are" }, if missing.len() == 1 { "it" } else { "them" })
    };
    // Rules by repository from before (a workflow now picks a provider in its label): shown, so nothing steers jobs unseen.
    let rules = v.routing().rules.iter().map(|r| format!(r#"<form method="post" action="/routing" class="rule"><input type="hidden" name="action" value="remove"><input type="hidden" name="repo" value="{}"><span class="note"><code>{}</code> only uses {} (a rule from before; a workflow can say <code>runs-on: {}/provider={}</code> instead)</span><button class="chip">Remove</button></form>"#,
        esc(&r.repo), esc(&r.repo), esc(cloud_name(&r.cloud)), esc(&v.label()), esc(&r.cloud))).collect::<String>();
    format!(r#"<section class="card"><div class="card-head"><h2>Runner providers</h2><span class="order-note" id="order-note" aria-live="polite"></span></div><p class="note">Each provider starts a fresh runner for every job. A job goes to the first that can run its machine and has room; when that one cannot start it (no spot machine, a failure), to the next. When all are full, it waits. Drag to reorder.</p>{waiting}<ol class="pools" data-order="{}">{rows}</ol>{}{rules}{dialogs}</section>"#, esc(&shown), if out.is_empty() { String::new() } else { format!(r#"<div class="limits out">{out}</div>"#) })
}

/// The cloud accounts jobs run in, each with what belongs to the account rather than to a place in the order: AWS's
/// own network by region, Cloudflare's runner image, and removing it (which deletes what SuperCI made there).
fn clouds_card(v: &PlaneView, signed_in: &[&str]) -> String {
    let close = r#"<button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>"#;
    let st = v.status.clone().unwrap_or_default();
    let (mut rows, mut dialogs) = (String::new(), String::new());
    for (i, cloud) in v.clouds().iter().enumerate() {
        let name = cloud_name(cloud).to_string();
        let (title, what, field) = match cloud.as_str() {
            "aws" => {
                let account = aws_account(v).map(|a| format!("AWS account {a}")).unwrap_or_else(|| name.clone());
                let own: Vec<String> = st["aws_networks"].as_object().map(|n| n.keys().map(|r| region_short(r).to_string()).collect()).unwrap_or_default();
                let what = format!("Set up in {} · {}", regions_text(&v.aws_regions()).replace(", then", ","), if own.is_empty() { "networks SuperCI made".to_string() } else { format!("your own network in {}", own.join(", ")) });
                let field = if st.get("aws_networks").is_some() {
                    let text = given_networks_text(&st);
                    format!(r#"<label class="field"><span>Your own network</span><textarea name="networks" rows="3" spellcheck="false" placeholder="us-east-1 subnet-0abc… subnet-0def… sg-0123… private">{}</textarea><small>One line a region: its subnets and security groups. Machines there start in your network, so jobs reach what it reaches. Add <code>private</code> for subnets with no public addresses (they need a NAT to reach GitHub). Regions not listed use the network SuperCI made.</small></label>"#, esc(&text))
                } else { String::new() };
                (account, what, field)
            }
            "cloudflare" => {
                let image = st["cloudflare_image"].as_str().unwrap_or_default().to_string();
                let field = if st.get("cloudflare_location").is_some() { format!(r#"<label class="field"><span>GitHub's full image (experimental)</span><input type="url" name="image" value="{}" placeholder="https://… (empty: the small image)"><small>Jobs then run inside GitHub's own runner image, with everything it has, loaded from this address as it is read and checked piece by piece (an address as <code>image-reader pack</code> names it). Empty: GitHub's small runner image, with few tools.</small></label>"#, esc(&image)) } else { String::new() };
                (name.clone(), if image.is_empty() { "GitHub's small runner image".into() } else { "GitHub's full image (experimental)".to_string() }, field)
            }
            _ => (name.clone(), "Its launcher starts the sandboxes".to_string(), String::new()),
        };
        let n = 100 + i;
        rows.push_str(&format!(r#"<li class="pool still">{}<span class="pool-main"><strong>{}</strong><small>{}</small></span><span class="pool-now"></span><button type="button" class="icon-btn" aria-label="{} settings" onclick="document.getElementById('pool-{n}').showModal()">{ICON_GEAR}</button></li>"#,
            logo(cloud, 32), esc(&title), esc(&what), esc(&if title.contains("account") { title.clone() } else { format!("{title} account") })));
        let remove = format!(r#"<button type="button" class="button danger sm" style="margin-right:auto" onclick="this.closest('dialog').close();document.getElementById('pool-remove-{n}').showModal()">Remove…</button>"#);
        let save = if field.is_empty() { String::new() } else { r#"<button class="button primary sm">Save</button>"#.to_string() };
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="pool-{n}" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body"><input type="hidden" name="action" value="account"><input type="hidden" name="cloud" value="{}"><div class="dlg-head">{}<span><strong>{}</strong><small>{}</small></span>{close}</div>{field}<div class="dlg-foot">{remove}<button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button>{save}</div></form></dialog>{}"#,
            esc(cloud), logo(cloud, 36), esc(&title), esc(&what), remove_dialog(v, cloud, n, signed_in)));
    }
    // (Shown beside the order, which has a place for each of these clouds: never without a row.)
    format!(r#"<section class="card"><div class="card-head"><h2>Clouds</h2></div><p class="note">The accounts your runners start machines in. Removing one deletes what SuperCI made there.</p><ol class="pools">{rows}</ol>{dialogs}</section>"#)
}

/// A look at an AWS account with all the pages say of it: what it allows (`aws::look_at`), where a copy of the
/// runner image is on its way, and where the account's own copy is of an older build than its maker's newest.
/// About three seconds of asking AWS.
fn look_all(creds: &superci_core::aws::Credentials) -> (aws::Look, Vec<String>, Vec<(String, String, String, bool)>) {
    let look = aws::look_at(creds);
    // Where the image is missing: whether a copy of it is on its way there.
    let copying: Vec<String> = look.regions_without_image().into_iter().filter(|r| aws::copied_images(creds, r).is_ok_and(|copies| copies.iter().any(|c| c.state == "pending"))).map(str::to_string).collect();
    // Where the account's own copy is what jobs start from: whether its maker has a newer build out (a
    // copy stays the build it was made of), and whether that one is on its way in. A copy that a newer
    // one replaced goes, once the newer one is there.
    let mut behind = vec![];
    for region in look.usable_regions() {
        let _ = aws::tidy_copies(creds, region);
        let Ok(copies) = aws::copied_images(creds, region) else { continue };
        if copies.is_empty() { continue }
        let Some(newest) = aws::newest_build() else { break };
        let have = copies.iter().filter(|c| c.state == "available").map(|c| aws::build_of(&c.name)).max().unwrap_or_default().to_string();
        if !have.is_empty() && have < newest { behind.push((region.to_string(), have, newest.clone(), aws::behind(&copies, &newest).is_none())) }
    }
    (look, copying, behind)
}

/// SuperCI's own mark on a tile, as a provider's logo is (for a row that is about all of them).
fn brand_logo(size: u32) -> String {
    format!(r#"<span class="logo" style="width:{size}px;height:{size}px;background:color-mix(in srgb,var(--ink) 10%,var(--card))">{}</span>"#, MARK.replacen(r#"class="mark""#, &format!(r#"width="{0}" height="{0}""#, size * 58 / 100), 1))
}

/// What the CI may spend on machines in a month: all its providers together, and a cloud account with several
/// providers (AWS: its spot and on-demand machines together). A provider's own budget is in its settings. Each is a
/// row with what was spent this month and its limit, changed in its own dialog.
fn spending_card(v: &PlaneView, lifted_ms: u64) -> String {
    let head = r#"<section class="card" id="limits"><div class="card-head"><h2>Limits</h2></div>"#;
    // A control plane from before these limits would drop them unsaid: said, in their place.
    if !v.account_limits() { return format!(r#"{head}<p class="note">A limit for everything, and for a cloud account, needs a newer control plane: <a href="/?p=planes">update it on the Control plane page</a>. A provider's own budget is in its settings above.</p></section>"#) }
    let routing = v.routing();
    let daily = v.daily_limits();
    let estimate = "Limits count the machines jobs ran on, as estimated here. A cloud's bill also has storage and small fees, and jobs already running finish.";
    struct Row { id: &'static str, action: &'static str, account: &'static str, mark: String, name: String, what: &'static str, month: (f64, Option<f64>), day: (f64, Option<f64>), at_limit: &'static str }
    let mut rows = vec![Row { id: "all", action: "monthly_usd", account: "", mark: brand_logo(32), name: "Everything".into(), what: "All providers together. At a limit no job starts until it is over.",
        month: (v.spend_total(), routing.monthly_usd), day: (v.spend_today_total(), routing.daily_usd), at_limit: "At a limit no job starts, until the month (or the day, UTC) is over or the limit is raised." }];
    let aws = aws_account(v).filter(|_| v.order().iter().any(|p| p.cloud == "aws"));
    if let Some(account) = &aws {
        rows.push(Row { id: "aws", action: "account_usd", account: "aws", mark: logo("aws", 32), name: format!("AWS account {account}"), what: "Its spot and on-demand machines together. At a limit, jobs go to other providers.",
            month: (v.spend_account("aws"), routing.account_usd.get("aws").copied()), day: (v.spend_today_account("aws"), routing.account_daily_usd.get("aws").copied()),
            at_limit: "At a limit, jobs go to the providers of other accounts in the order; with none, they fail saying so." });
    }
    // AWS's own budget for the account (see `aws_budget`), and whether its rule is on the control plane's role now:
    // the control plane noted a machine refused by it, later than it was last lifted here, within the day.
    let budget = routing.aws_budget.clone();
    let stopped = budget.as_ref().is_some_and(|b| b.stop) && v.budget_stopped_ms().is_some_and(|at| at > lifted_ms && now_ms().saturating_sub(at) < 86_400_000);
    let (mut list, mut dialogs) = (String::new(), String::new());
    rows.into_iter().for_each(|r| {
        let chip = |(spent, cap): (f64, Option<f64>), when: &str| match cap {
            Some(cap) => format!(r#"<span class="lim">{} of {}{when}</span>{}"#, usd(spent), usd(cap), if spent >= cap { pill("bad", "at its limit") } else { String::new() }),
            None => String::new(),
        };
        let mut now = format!("{}{}", chip(r.month, ""), chip(r.day, " today"));
        if r.month.1.is_none() { now = format!(r#"<span class="lim">{} this month</span>{now}"#, usd(r.month.0)) }
        if r.month.1.is_none() && r.day.1.is_none() { now.push_str(r#"<span class="lim">No limit</span>"#) }
        let fields = |month: Option<f64>, day: Option<f64>| format!(r#"<input type="hidden" name="action" value="{}"><input type="hidden" name="account" value="{}">{}{}"#, r.action, r.account,
            month.map(|n| format!(r#"<input type="hidden" name="usd" value="{n}">"#)).unwrap_or_default(), day.map(|n| format!(r#"<input type="hidden" name="daily" value="{n}">"#)).unwrap_or_default());
        // The AWS account's row says what AWS itself does at the limit; with its rule on the role, that, and Lift.
        let mut in_aws = String::new();
        if r.id == "aws" {
            now.push_str(&match (&budget, stopped) {
                (Some(_), true) => format!(r#"{}<form method="post" action="/routing">{}<input type="hidden" name="budget" value="lift"><button class="button secondary sm" title="Takes AWS's rule off the control plane's role: machines start again, and AWS leaves them be until next month">Lift</button></form>"#, pill("bad", "stopped by AWS's budget"), fields(r.month.1, r.day.1)),
                (Some(b), false) => pill("good", if b.stop { "AWS stops at the limit" } else { "AWS emails" }),
                (None, _) => String::new(),
            });
            let option = |value: &str, text: &str| format!(r#"<option value="{value}"{}>{text}</option>"#, if value == match &budget { None => "none", Some(b) if b.stop => "stops", Some(_) => "emails" } { " selected" } else { "" });
            in_aws = if daily {
                format!(r#"<label class="field"><span>In AWS itself</span><select name="budget">{}{}{}</select><small>A budget in the account for the monthly limit. AWS counts the account's real bill, all of it and hours late. At 100% it can also refuse new machines to the control plane, until the next month or until you lift it. It is made with your sign-in; the control plane gets no permission for it.</small></label><label class="field"><span>AWS's emails go to</span><input type="email" name="email" placeholder="{}" autocomplete="email"></label>"#,
                    option("none", "Nothing"), option("emails", "AWS emails me at 80% and 100%"), option("stops", "AWS emails me, and stops new machines at 100%"), if budget.is_some() { "As it is" } else { "you@example.com" })
            } else { String::new() };
        }
        let value = |n: Option<f64>| n.map(|n| format!(r#" value="{}""#, if n.fract() == 0.0 { format!("{n:.0}") } else { format!("{n:.2}") })).unwrap_or_default();
        let day_field = if daily { format!(r#"<label class="field"><span>Daily limit</span><span class="money">$<input type="number" name="daily" min="0.01" step="any" placeholder="No limit"{}></span><small>{} today (UTC). A day that runs away stops that day.</small></label>"#, value(r.day.1), usd(r.day.0)) } else { String::new() };
        list.push_str(&format!(r#"<li class="pool still">{}<span class="pool-main"><strong>{}</strong><small>{}</small></span><span class="pool-now">{now}</span><button type="button" class="icon-btn" aria-label="{} limits" onclick="document.getElementById('limit-{}').showModal()">{ICON_GEAR}</button></li>"#, r.mark, esc(&r.name), esc(r.what), esc(&r.name), r.id));
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="limit-{}" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body"><input type="hidden" name="action" value="{}"><input type="hidden" name="account" value="{}"><div class="dlg-head">{}<span><strong>{}</strong><small>{}</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><label class="field"><span>Monthly limit</span><span class="money">$<input type="number" name="usd" min="0.01" step="any" placeholder="No limit"{} autofocus></span><small>{} this month. {} {estimate}</small></label>{day_field}{in_aws}<div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>"#,
            r.id, r.action, r.account, r.mark, esc(&r.name), esc(r.what), value(r.month.1), usd(r.month.0), r.at_limit));
    });
    format!(r#"{head}<p class="note">The most {} may spend on machines in a month and in a day. A provider's own budget is in its settings above.</p><ol class="pools">{list}</ol>{dialogs}</section>"#, esc(&v.name()))
}

/// Machines: the default machine (what plain `runs-on: superci` gets, and anything a label leaves out), changed in
/// its own dialog; and a picker that gives the label for another machine, naming only what is picked.
/// The default machine (what the plain label gets, and anything a label leaves out), changed in its own dialog; and
/// Limits that keep jobs from costing more than meant: the largest machine a label may ask for, and the public
/// repositories allowed to run here (on a public repository anyone can open a pull request; forks' never run).
fn limits_card(v: &PlaneView, public: Option<Vec<String>>) -> String {
    let routing = v.routing();
    let max = routing.max_cpu.unwrap_or(MAX_CPU);
    let hours = routing.max_minutes.unwrap_or(MAX_JOB_MINUTES) / 60;
    // Public repositories: a line saying which are allowed, and a dialog to choose them (search, a box each). Not
    // known (a control plane from before it could say): typed in.
    let allowed = &routing.public_repos;
    let summary = match allowed.len() {
        0 => r#"<span class="note">None allowed</span>"#.to_string(),
        n => {
            let names = allowed.iter().take(3).map(|r| format!("<code>{}</code>", esc(r.split('/').nth(1).unwrap_or(r)))).collect::<Vec<_>>().join(", ");
            format!(r#"<span class="note">{names}{}</span>"#, if n > 3 { format!(" and {} more", n - 3) } else { String::new() })
        }
    };
    let (control, dialog) = match public {
        Some(list) => {
            let mut all: Vec<String> = list.clone();
            for p in allowed.iter().filter(|p| !p.ends_with("/*")) { if !all.iter().any(|r| r.eq_ignore_ascii_case(p)) { all.push(p.clone()) } }
            // Allowed ones first, then by name.
            all.sort_by_key(|r| (!routing.allows_public(r), r.to_lowercase()));
            if all.is_empty() { (r#"<span class="note">None of its repositories is public</span>"#.to_string(), String::new()) } else {
                let items = all.iter().map(|r| {
                    let (owner, name) = r.split_once('/').unwrap_or(("", r));
                    format!(r#"<label class="chk" data-name="{}"><input type="checkbox" name="repo" value="{}"{}><span><span class="faint">{}/</span>{}</span></label>"#,
                        esc(&r.to_lowercase()), esc(r), if routing.allows_public(r) { " checked" } else { "" }, esc(owner), esc(name))
                }).collect::<String>();
                let search = if all.len() > 8 { r#"<input class="dlg-search" type="search" placeholder="Search" aria-label="Search repositories" oninput="var q=this.value.toLowerCase();this.closest('form').querySelectorAll('.chk').forEach(function(l){l.hidden=q&&l.dataset.name.indexOf(q)<0})">"# } else { "" };
                (format!(r#"{summary}<button type="button" class="button secondary sm" onclick="document.getElementById('public-repos').showModal()">Choose…</button>"#),
                 format!(r#"<dialog class="dlg" id="public-repos" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body"><input type="hidden" name="action" value="public_set"><div class="dlg-head">{}<span><strong>Public repositories</strong><small>{} public · pull requests from forks never run</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div>{search}<div class="chk-list">{items}</div><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>"#, logo("github", 32), list.len()))
            }
        }
        None => (format!(r#"{summary}<form method="post" action="/routing" class="limit-do"><input type="hidden" name="action" value="public_add"><input name="repo" placeholder="owner/name" aria-label="Public repository" spellcheck="false" autocomplete="off"><button class="button secondary sm">Allow</button></form>"#), String::new()),
    };
    let public_row = if v.github { format!(r#"<div class="limit"><span class="limit-text"><strong>Public repositories</strong><small>Their jobs run only when allowed. Pull requests from forks never run.</small></span><span class="limit-do">{control}</span></div>"#) } else { String::new() };
    format!(r#"<section class="card"><div class="card-head"><h2>Limits</h2></div><div class="limits">
<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="max_cpu"><span class="limit-text"><strong>Largest machine</strong><small>A label asking for more is refused.</small></span><span class="limit-do"><input type="number" name="max_cpu" min="1" max="192" value="{max}" aria-label="Most CPUs"><span class="note">CPUs</span><button class="button secondary sm">Save</button></span></form>
<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="max_hours"><span class="limit-text"><strong>Longest job</strong><small>Its machine is ended after this. Cloudflare allows 6 hours, Modal 24.</small></span><span class="limit-do"><input type="number" name="max_hours" min="1" max="120" value="{hours}" aria-label="Most hours"><span class="note">hours</span><button class="button secondary sm">Save</button></span></form>
{public_row}
</div></section>{dialog}"#)
}

/// The cache: on or off, what a repository keeps and for how long, and what each repository keeps now (each cleared
/// with a button). Only for a control plane with AWS machines (the cache is a bucket in that account).
fn cache_card(v: &PlaneView, caches: Option<&serde_json::Value>) -> String {
    if !v.aws { return String::new() }
    let c = &v.routing().cache;
    let (on, gb, days) = (!c.off, c.size_gb(), c.days());
    let switch = if on { r#"<input type="hidden" name="on" value="off"><button class="button secondary sm">Turn off</button>"# } else { r#"<input type="hidden" name="on" value="on"><button class="button primary sm">Turn on</button>"# };
    let what = if on { "On. Jobs on AWS keep GitHub's cache in a bucket of yours, in their region." } else { "Off. Jobs use GitHub's own cache." };
    let repos = caches.and_then(|c| c["repos"].as_array()).cloned().unwrap_or_default();
    let now = caches.and_then(|c| c["now_ms"].as_u64()).unwrap_or(0);
    let list = repos.iter().map(|r| {
        let (id, entries, bytes, used) = (r["id"].as_str().unwrap_or_default(), r["entries"].as_u64().unwrap_or(0), r["bytes"].as_u64().unwrap_or(0), r["used_ms"].as_u64().unwrap_or(0));
        let name = r["name"].as_str().map(str::to_string).unwrap_or_else(|| format!("Repository {}", id.trim_start_matches("gh")));
        format!(r#"<form method="post" action="/caches/delete" class="limit"><input type="hidden" name="repo" value="{}"><span class="limit-text"><strong>{}</strong><small>{entries} {} · {} · used {}</small></span><span class="limit-do"><button class="button secondary sm">Clear</button></span></form>"#,
            esc(id), esc(&name), if entries == 1 { "entry" } else { "entries" }, size_words(bytes), ago_words(now.saturating_sub(used)))
    }).collect::<String>();
    // Docker's images: kept for the next jobs on Ubuntu 26.04 machines unless turned off.
    let images_on = !c.images_off;
    let images_switch = if images_on { r#"<input type="hidden" name="on" value="off"><button class="button secondary sm">Turn off</button>"# } else { r#"<input type="hidden" name="on" value="on"><button class="button primary sm">Turn on</button>"# };
    let images_kept = caches.map(|c| &c["images"]).filter(|i| i["layers"].as_u64().unwrap_or(0) > 0).map(|i| format!(" {} layers kept now ({}).", i["layers"], size_words(i["bytes"].as_u64().unwrap_or(0)))).unwrap_or_default();
    let images_what = if images_on { format!("On. A job's next run reads the images it pulled from your bucket instead of the internet (Ubuntu 26.04 machines).{images_kept}") } else { "Off. Docker pulls every image as it always has.".to_string() };
    let images = if on { format!(r#"<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="cache_images"><span class="limit-text"><strong>Docker images</strong><small>{}</small></span><span class="limit-do">{images_switch}</span></form>"#, esc(&images_what)) } else { String::new() };
    let kept = match caches {
        None => String::new(),
        Some(_) => format!(r#"<div class="limit"><span class="limit-text"><strong>Kept now</strong><small>{}</small></span></div>{list}"#, if repos.is_empty() { "Nothing kept yet." } else { "Clearing a repository's makes its jobs save anew." }),
    };
    format!(r#"<section class="card"><div class="card-head"><h2>Cache</h2></div><div class="limits">
<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="cache_on"><span class="limit-text"><strong>In your AWS account</strong><small>{what}</small></span><span class="limit-do">{switch}</span></form>
<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="cache_size"><span class="limit-text"><strong>Each repository</strong><small>The least recently used go first. About $0.023 a GB a month in S3.</small></span><span class="limit-do"><input type="number" name="size_gb" min="1" max="10000" value="{gb}" aria-label="Most GB"><span class="note">GB</span><button class="button secondary sm">Save</button></span></form>
<form method="post" action="/routing" class="limit"><input type="hidden" name="action" value="cache_days"><span class="limit-text"><strong>Unused for</strong><small>An entry goes after this. One saved {} days ago is saved anew.</small></span><span class="limit-do"><input type="number" name="days" min="1" max="{}" value="{days}" aria-label="Days"><span class="note">days</span><button class="button secondary sm">Save</button></span></form>
{images}
{kept}
</div></section>"#, superci_core::cache::MAX_AGE_DAYS, superci_core::cache::MAX_AGE_DAYS)
}

/// A size in words: 940 MB, 3.4 GB.
pub(crate) fn size_words(bytes: u64) -> String { if bytes >= 1_000_000_000 { format!("{:.1} GB", bytes as f64 / 1e9) } else if bytes >= 10_000_000 { format!("{} MB", (bytes + 500_000) / 1_000_000) } else if bytes >= 1_000_000 { format!("{:.1} MB", bytes as f64 / 1e6) } else { format!("{} KB", bytes.div_ceil(1000)) } }

/// How long ago, in words: 5 minutes, 3 hours, 2 days.
fn ago_words(ms: u64) -> String {
    let m = ms / 60_000;
    match m { 0 => "just now".into(), 1..=59 => format!("{m} min ago"), 60..=2879 => format!("{} h ago", m / 60), _ => format!("{} days ago", m / 1440) }
}

/// the label picker, a dialog of its own: it names only what differs from the default.
/// The Ubuntus a machine can run, as the dialogs offer them.
const UBUNTU_CHOICES: [(&str, &str); 2] = [("ubuntu26", "26.04 (Docker 29)"), ("ubuntu24", "24.04 (Docker 28)")];

fn machines_card(v: &PlaneView) -> (String, String) {
    let m = v.machine();
    let base = esc(&v.label());
    let select = |name: &str, label: &str, first: &str, choices: &[(u32, &str)], current: Option<u32>| {
        let opts = choices.iter().map(|(n, text)| format!(r#"<option value="{n}"{}>{text}</option>"#, if current == Some(*n) { " selected" } else { "" })).collect::<String>();
        format!(r#"<label class="field"><span>{label}</span><select name="{name}"><option value="">{first}</option>{opts}</select></label>"#)
    };
    const CPUS: [(u32, &str); 6] = [(2, "2"), (4, "4"), (8, "8"), (16, "16"), (32, "32"), (64, "64")];
    const RAMS: [(u32, &str); 7] = [(4, "4 GB"), (8, "8 GB"), (16, "16 GB"), (32, "32 GB"), (64, "64 GB"), (128, "128 GB"), (256, "256 GB")];
    const DISKS: [(u32, &str); 5] = [(30, "30 GB"), (60, "60 GB"), (100, "100 GB"), (200, "200 GB"), (500, "500 GB")];
    let choice = |name: &str, label: &str, first: &str, opts: &[(&str, &str)], current: Option<&str>| {
        let opts = opts.iter().map(|(val, text)| format!(r#"<option value="{val}"{}>{text}</option>"#, if current == Some(*val) { " selected" } else { "" })).collect::<String>();
        format!(r#"<label class="field"><span>{label}</span><select name="{name}">{}{opts}</select></label>"#, if first.is_empty() { String::new() } else { format!(r#"<option value="">{first}</option>"#) })
    };
    let pools = || v.order().iter().filter(|p| p.cloud != AWS_ON_DEMAND).map(|p| { let (key, name) = (p.cloud.clone(), cloud_name(&p.cloud).to_string()); format!(r#"<span class="fit" data-caps="{}">{}{}<small data-size></small></span>"#, esc(&pool_caps(&p.cloud).to_string()), logo(&key, 18), esc(&name)) }).collect::<String>();
    // The disk's speed on AWS: by the machine's size unless set (what AWS includes, at no extra price, or a number).
    let speed = m.disk_mibs.map(|s| s.to_string());
    let custom = speed.clone().filter(|s| !["125", "250", "500", "1000"].contains(&s.as_str())).map(|s| (s.clone(), format!("{s} MiB/s")));
    let mut speeds: Vec<(&str, &str)> = vec![("125", "Standard, 125 MiB/s"), ("250", "250 MiB/s"), ("500", "500 MiB/s"), ("1000", "1,000 MiB/s")];
    if let Some((val, text)) = &custom { speeds.push((val, text)) }
    let default = serde_json::json!({ "cpu": m.cpu.unwrap_or(0), "ram": m.ram_gb.unwrap_or(0), "disk": m.disk_gb.unwrap_or(0), "arch": m.arch(), "os": m.os(), "image": m.image, "ondemand": m.on_demand() });
    // The default: what it is, in words, and its dialog.
    let mut parts = vec![m.cpu.map(|c| format!("{c} CPU")).unwrap_or_else(|| "Each provider's standard size".into())];
    if let Some(r) = m.ram_gb { parts.push(format!("{r} GB")) }
    if let Some(d) = m.disk_gb { parts.push(format!("{d} GB disk")) }
    match m.disk_mibs { Some(superci_core::spec::DISK_MIBS_INCLUDED) => parts.push("standard disk speed".into()), Some(s) => parts.push(format!("{s} MiB/s disk")), None => {} }
    parts.push(m.arch().into());
    parts.push(match (m.os(), m.image.as_deref()) { ("windows", _) => "Windows".into(), ("macos", _) => "macOS".into(), (_, Some("ubuntu26")) => "Ubuntu 26.04".into(), (_, Some("ubuntu24")) => "Ubuntu 24.04".into(), _ => "Linux".into() });
    parts.push(if m.on_demand() { "no spot machines".into() } else { "spot".into() });
    // (What only a command sets for every job: where its files are kept, its machine families, its provider.)
    if let Some(scratch) = m.scratch.as_deref() { parts.push(if scratch == "ramdisk" { "RAM disk".into() } else { "local disk".into() }) }
    if !m.families.is_empty() { parts.push(m.families.join(", ")) }
    match m.docker_cache { Some(false) => parts.push("no Docker image cache".into()), Some(true) => parts.push("Docker image cache from the start".into()), None => {} }
    if let Some(provider) = &m.cloud { parts.push(format!("only {}", cloud_name(provider))) }
    let dialog = format!(r#"<dialog class="dlg" id="default-machine" onclick="if(event.target===this)this.close()"><form method="post" action="/machine" class="dlg-body picker" data-mode="default" data-default="{}" oninput="superciPick(this)" onchange="superciPick(this)"><div class="dlg-head"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span><strong>Default machine</strong><small>For <code>runs-on: {base}</code>, and any part a label leaves out.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><div class="picker-fields">{}{}{}{}{}{}</div>{}<label class="check"><input type="checkbox" name="nospot"{}> No spot machines</label><div class="fits"><span class="note">Runs on</span>{}</div><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>"#,
        esc(&default.to_string()),
        select("cpu", "CPU", "Provider's standard", &CPUS, m.cpu), select("ram", "Memory", "4 GB per CPU", &RAMS, m.ram_gb), select("disk", "Disk", "Provider's standard", &DISKS, m.disk_gb),
        choice("disk-speed", "Disk speed (AWS)", "By machine size", &speeds, speed.as_deref()),
        choice("arch", "Architecture", "", &[("x64", "x64"), ("arm64", "arm64")], Some(m.arch())),
        // (Beside the system: which Ubuntu, unset as GitHub's `ubuntu-latest` is.)
        format!("{}{}", choice("os", "System", "", &[("linux", "Linux"), ("windows", "Windows")], Some(m.os())), choice("image", "Ubuntu", "As ubuntu-latest", &UBUNTU_CHOICES, m.image.as_deref())),
        // (What the dialog does not show is kept as it is.)
        m.pairs().iter().filter(|(key, _)| ["disk-iops", "gpu", "family", "scratch", "provider", "docker-cache"].contains(key)).map(|(key, value)| format!(r#"<input type="hidden" name="{key}" value="{}">"#, esc(value))).collect::<String>(),
        if m.on_demand() { " checked" } else { "" }, pools());
    let default_row = format!(r#"<div class="default-machine"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span class="dm-main"><strong>Default machine</strong><small>What <code>{base}</code> runs on, and anything a label leaves out.</small></span><span class="dm-spec">{}</span><button type="button" class="button secondary sm" onclick="document.getElementById('default-machine').showModal()">Change</button></div>"#,
        parts.iter().map(|p| format!(r#"<span class="lim">{}</span>"#, esc(p))).collect::<String>());
    // The picker: every field starts at the default; the label names only what is picked.
    let od_default = if m.on_demand() { r#" checked disabled title="The default machine has no spot machines already""# } else { "" };
    let picker = format!(r#"<dialog class="dlg wide" id="label-picker" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span><strong>Choose a machine</strong><small>Pick what differs from the default; its label names only that.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><form class="picker" data-mode="label" data-base="{base}" data-default="{}" oninput="superciPick(this)" onchange="superciPick(this)" onsubmit="return false"><div class="picker-fields">{}{}{}{}{}{}<label class="check"><input type="checkbox" name="nospot"{od_default}> No spot machines</label></div><div class="label-out"><code><span class="k">runs-on:</span> <span class="hl" data-label>{base}</span></code><button type="button" class="chip" onclick="var b=this;navigator.clipboard.writeText('runs-on: '+b.closest('form').querySelector('[data-label]').textContent).then(function(){{b.textContent='Copied'}},function(){{}})">Copy</button></div><div class="fits"><span class="note">Runs on</span>{}</div></form></div></dialog>"#,
        esc(&default.to_string()),
        select("cpu", "CPU", "Default", &CPUS, None), select("ram", "Memory", "Default", &RAMS, None), select("disk", "Disk", "Default", &DISKS, None),
        choice("arch", "Architecture", "Default", &[("x64", "x64"), ("arm64", "arm64")], None),
        format!("{}{}", choice("os", "System", "Default", &[("linux", "Linux"), ("windows", "Windows")], None), choice("image", "Ubuntu", "Default", &UBUNTU_CHOICES, None)),
        choice("gpu", "GPU", "None", &[("gpu", "Any (least costly)"), ("t4", "T4"), ("l4", "L4"), ("a10g", "A10G"), ("l40s", "L40S"), ("a100", "A100"), ("h100", "H100"), ("h200", "H200"), ("b200", "B200")], None), pools());
    (format!(r#"<section class="card dm-card">{default_row}{dialog}</section>"#), picker)
}

/// What a control plane is called and the labels it answers to: a row each, changed in its dialog.
fn called_card(v: &PlaneView) -> String {
    // The labels: the list of them, and its dialog (add one; each stays until removed, the last one always).
    let labels = v.labels();
    let close = r#"<button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>"#;
    let logo = |icon: &str| format!(r#"<span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{icon}</span>"#);
    let kept = labels.iter().enumerate().map(|(i, l)| format!(r#"<form method="post" action="/labels" class="kept-label"><input type="hidden" name="action" value="remove"><input type="hidden" name="label" value="{l}"><span><code>{l}</code>{}</span>{}</form>"#,
        if i == 0 && labels.len() > 1 { r#" <small class="faint">shown in examples</small>"# } else { "" },
        if labels.len() > 1 { r#"<button class="button secondary sm">Remove</button>"# } else { "" }, l = esc(l))).collect::<String>();
    let label_dialog = format!(r#"<dialog class="dlg" id="labels" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Labels</strong><small>A workflow may name any of them in <code>runs-on</code>.</small></span>{close}</div><div class="kept-labels">{kept}</div><form method="post" action="/labels" class="limit-do"><input name="label" placeholder="acmeci" aria-label="A label to add" pattern="[a-z][a-z0-9]{{1,23}}" required spellcheck="false" autocomplete="off"><button class="button primary sm">Add</button></form></div></dialog>"#, logo(ICON_TAG));
    let label_row = format!(r#"<div class="default-machine">{}<span class="dm-main"><strong>Labels</strong><small>What workflows name in <code>runs-on</code>.</small></span><span class="dm-spec">{}</span><button type="button" class="button secondary sm" onclick="document.getElementById('labels').showModal()">Change</button></div>"#,
        logo(ICON_TAG), labels.iter().map(|l| format!(r#"<span class="lim">{}</span>"#, esc(l))).collect::<String>());
    // Its name: what it is called in the dashboard and on GitHub.
    let name = esc(&v.name());
    let name_dialog = format!(r#"<dialog class="dlg" id="name" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Name</strong><small>Shown in the dashboard and on GitHub.</small></span>{close}</div><form method="post" action="/name" class="limit-do"><input name="name" value="{name}" aria-label="Name" autofocus onfocus="this.select()" pattern="[A-Za-z][A-Za-z0-9 \-]{{1,23}}" required spellcheck="false" autocomplete="off"><button class="button primary sm">Save</button></form></div></dialog>"#, logo(ICON_NAME));
    let name_row = format!(r#"<div class="default-machine">{}<span class="dm-main"><strong>Name</strong><small>What your CI is called.</small></span><span class="dm-spec"><span class="lim">{name}</span></span><button type="button" class="button secondary sm" onclick="document.getElementById('name').showModal()">Change</button></div>"#, logo(ICON_NAME));
    format!(r#"<section class="card dm-card">{name_row}{label_row}{label_dialog}{name_dialog}</section>"#)
}

const ICON_SETTINGS: &str = r#"<svg viewBox="0 0 24 24"><path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/></svg>"#;
const ICON_SIGNOUT: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4h4a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-4M10 16l-4-4 4-4M6 12h9"/></svg>"#;
const ICON_NAME: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 7V5.5h14V7M12 5.5v13M9.5 18.5h5"/></svg>"#;
const ICON_TAG: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3.5 11.6V5a1.5 1.5 0 0 1 1.5-1.5h6.6a1.5 1.5 0 0 1 1.06.44l7.4 7.4a1.5 1.5 0 0 1 0 2.12l-6.1 6.1a1.5 1.5 0 0 1-2.12 0l-7.4-7.4a1.5 1.5 0 0 1-.44-1.06Z"/><circle cx="8" cy="8" r="1.4"/></svg>"#;
const ICON_CHIP: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/></svg>"#;

fn signin_button(cloud: &str, label: &str, primary: bool) -> String {
    format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="{}">{}</button></form>"#, if primary { "button primary" } else { "button secondary" }, esc(label))
}

/// A run's page here: its jobs, how long it took and what it cost (its page at its code host from there).
fn run_page_link(r: &serde_json::Value) -> String {
    let repo: String = url::form_urlencoded::byte_serialize(r["repo"].as_str().unwrap_or_default().as_bytes()).collect();
    format!("/?p=run&repo={repo}&id={}&attempt={}", r["id"].as_u64().unwrap_or_default(), r["attempt"].as_u64().unwrap_or(0))
}

/// A job's page here: what it is, how its start went, and its log (its page at its code host from there).
fn job_page_link(j: &serde_json::Value) -> String {
    let id = j["job_id"].as_u64().unwrap_or_default();
    match (j["provider"].as_str(), j["gitlab"].as_str().filter(|g| !g.is_empty())) {
        (Some("gitlab"), Some(g)) => format!("/?p=job&id={id}&gl={g}"),
        _ => format!("/?p=job&id={id}"),
    }
}

/// Files fetched one after another through links (a bucket's, GitHub's), as one, whatever their size.
fn fetch_all(urls: &[String]) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(600))).build().into();
    let mut out = vec![];
    for u in urls {
        let mut r = agent.get(&crate::aws::stand_in(u)).call().map_err(|e| e.to_string())?;
        if r.status().as_u16() >= 300 { return Err(format!("the bucket answered {} for a piece", r.status())) }
        out.extend(r.body_mut().with_config().limit(u64::MAX).read_to_vec().map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// A job's whole log, whatever its size: a run from a computer's from its pieces in the bucket, a GitHub job's from
/// GitHub's own link, else as the control plane gives it (4 MB at most).
pub fn whole_log(plane_url: &str, key: &str, j: &serde_json::Value) -> Result<Vec<u8>> {
    let id = j["job_id"].as_u64().unwrap_or_default();
    let provider = j["provider"].as_str().unwrap_or_default();
    // (A GitHub job still running: GitHub's file is as far as its runner sent it in blocks; the control plane adds
    // what its machine sent after.)
    let running = provider.is_empty() && ["launching", "launched", "running", "waiting"].contains(&j["state"].as_str().unwrap_or_default());
    if (provider == "local" || provider.is_empty()) && !running {
        if let Ok(got) = cloudflare::plane_get(plane_url, key, &format!("/job/log?id={id}&link=1")) {
            if let Some(urls) = got["urls"].as_array() { return fetch_all(&urls.iter().filter_map(|u| u.as_str().map(str::to_string)).collect::<Vec<_>>()) }
            if let Some(url) = got["url"].as_str() { return fetch_all(&[url.to_string()]) }
        }
    }
    let gl = if provider == "gitlab" { format!("&gl={}", j["gitlab"].as_str().unwrap_or_default()) } else { String::new() };
    let got = cloudflare::plane_get(plane_url, key, &format!("/job/log?id={id}&bytes=4000000{gl}"))?;
    // (Its end only is not the whole: said, not passed off as it.)
    if got["truncated"] == true {
        let last = size_words(got["log"].as_str().unwrap_or_default().len() as u64);
        return Err(if provider == "gitlab" { format!("the control plane gives only the last {last} of a GitLab job's log: GitLab's page of the job has it whole (Complete Raw)") }
            else { format!("the control plane gave only the last {last} of its log (a newer control plane gives it whole)") })
    }
    match got["log"].as_str() { Some(l) => Ok(l.as_bytes().to_vec()), None => Err(got["error"].as_str().unwrap_or("no log").to_string()) }
}

/// A GitHub job's steps' own logs, by number, from its run's logs (GitHub's link through the control plane; one that
/// gives no such link yet gives nothing).
fn step_logs(plane_url: &str, key: &str, j: &serde_json::Value) -> Option<serde_json::Value> {
    let (id, name) = (j["job_id"].as_u64()?, j["name"].as_str()?);
    let got = cloudflare::plane_get(plane_url, key, &format!("/job/log?id={id}&link=steps")).ok()?;
    let url = got["url"].as_str().filter(|u| u.starts_with("https://"))?;
    crate::log_view::step_logs(&fetch_all(&[url.to_string()]).ok()?, name)
}

/// A run from a computer's artifact, whole: its pieces from the bucket, one after another.
pub fn artifact_bytes(plane_url: &str, key: &str, run: &str, name: &str) -> Result<Vec<u8>> {
    let got = cloudflare::plane_get(plane_url, key, &format!("/runs/{run}/artifacts?name={}", url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()))?;
    fetch_all(&got["pieces"].as_array().into_iter().flatten().filter_map(|p| p["url"].as_str().map(str::to_string)).collect::<Vec<_>>())
}

/// A job's page on its code host.
fn job_link(j: &serde_json::Value) -> String {
    let (repo, job, run) = (j["repo"].as_str().unwrap_or_default(), j["job_id"].as_u64().unwrap_or_default(), j["run_id"].as_u64().unwrap_or_default());
    // (A run from a computer has no page at its code host: GitHub never knew of it.)
    if j["provider"] == "local" { return String::new() }
    if j["provider"] == "gitlab" { format!("{}/{repo}/-/jobs/{job}", j["gitlab_url"].as_str().unwrap_or("https://gitlab.com")) } else { format!("{}/{repo}/actions/runs/{run}/job/{job}", j["github_web"].as_str().unwrap_or("https://github.com")) }
}

/// Overview's cards: the last day's jobs (did they get a machine, how fast, at what cost), the ones that need a look,
/// the latest; then where they ran.
fn overview_cards(v: &PlaneView, jobs: &[serde_json::Value], period: &str) -> String {
    let jobs: Vec<serde_json::Value> = jobs.iter().map(|j| with_hosts(v, j)).collect();
    let now = now_ms();
    let day: Vec<&serde_json::Value> = jobs.iter().filter(|j| now.saturating_sub(j["at_ms"].as_u64().unwrap_or_default()) < 86_400_000).collect();
    let state = |j: &serde_json::Value| j["state"].as_str().unwrap_or_default().to_string();
    let failed: Vec<&&serde_json::Value> = day.iter().filter(|j| ["failed", "swept"].contains(&state(j).as_str())).collect();
    let waiting: Vec<&&serde_json::Value> = day.iter().filter(|j| state(j) == "waiting").collect();
    let running = day.iter().filter(|j| ["launching", "launched", "running"].contains(&state(j).as_str())).count();
    let ran = day.iter().filter(|j| j["started_ms"].as_u64().is_some()).count();
    let settled = day.iter().filter(|j| !["waiting", "launching", "launched"].contains(&state(j).as_str())).count();
    // How long jobs waited for a machine: queued to running.
    let mut waits: Vec<u64> = day.iter().filter_map(|j| Some(j["started_ms"].as_u64()?.saturating_sub(j["at_ms"].as_u64().unwrap_or_default()))).collect();
    waits.sort();
    let median = waits.get(waits.len() / 2).copied();
    let spent: f64 = day.iter().filter_map(|j| job_cost(j)).sum::<f64>() + 0.0;
    let github: f64 = day.iter().filter_map(|j| github_cost(j)).sum::<f64>() + 0.0;

    let _ = (settled, running, waiting);
    // Three figures of the same shape: the number, what it is, the day hour by hour, one line about it.
    let kpi = |big: String, caption: &str, spark: String, sub: String| format!(r#"<div class="kpi"><div class="kpi-num">{big}</div><div class="kpi-cap">{caption}</div>{spark}<div class="kpi-sub">{sub}</div></div>"#);
    // One hour's bar of 24 across, as tall as its share of the day's highest; a second part (did not run) on top,
    // a little apart. Each says its hour and value on hover.
    let spark = |values: &[(f64, f64, String)]| {
        let most = values.iter().map(|(a, b, _)| a + b).fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
        let bars = values.iter().enumerate().map(|(i, (a, b, tip))| {
            let x = i as f64 * 10.0 + 2.0;
            let (ha, hb) = (a / most * 30.0, b / most * 30.0);
            let lower = if *a > 0.0 { format!(r#"<rect class="a" x="{x}" y="{:.1}" width="6" height="{:.1}" rx="1.5"/>"#, 34.0 - ha.max(1.5), ha.max(1.5)) } else { String::new() };
            let upper = if *b > 0.0 { format!(r#"<rect class="b" x="{x}" y="{:.1}" width="6" height="{:.1}" rx="1.5"/>"#, 34.0 - ha - hb.max(1.5) - if *a > 0.0 { 1.5 } else { 0.0 }, hb.max(1.5)) } else { String::new() };
            format!(r#"<g><title>{}</title><rect class="hit" x="{}" y="0" width="10" height="36"/>{lower}{upper}</g>"#, esc(tip), x - 2.0)
        }).collect::<String>();
        format!(r#"<svg class="spark" viewBox="0 0 240 36" preserveAspectRatio="none" aria-hidden="true"><line class="base" x1="0" y1="35.5" x2="240" y2="35.5"/>{bars}</svg>"#)
    };
    // The hour (0 = 24 hours ago, 23 = this hour) each job was queued in, and that hour's name.
    let slot = |j: &serde_json::Value| 23 - (now.saturating_sub(j["at_ms"].as_u64().unwrap_or(now)) / 3_600_000).min(23) as usize;
    let hour_name = |i: usize| { let ago = 23 - i; if ago == 0 { "this hour".to_string() } else if ago == 1 { "an hour ago".into() } else { format!("{ago} hours ago") } };
    // Jobs, hour by hour over the day: those that ran and those that did not.
    let mut hours = [(0u32, 0u32); 24];
    for j in &day {
        let hour = &mut hours[slot(j)];
        if ["failed", "swept"].contains(&state(j).as_str()) { hour.1 += 1 } else { hour.0 += 1 }
    }
    let jobs_spark = spark(&hours.iter().enumerate().map(|(i, (ok, bad))| (*ok as f64, *bad as f64, format!("{}: {ok} ran{}", hour_name(i), if *bad > 0 { format!(", {bad} did not") } else { String::new() }))).collect::<Vec<_>>());
    let jobs_kpi = kpi(day.len().to_string(), if day.len() == 1 { "job" } else { "jobs" }, jobs_spark,
        if failed.is_empty() { format!("{ran} ran") } else { format!(r#"{ran} ran<span class="sep">·</span><span class="bad-text">{} did not</span>"#, failed.len()) });
    // The wait, hour by hour: each hour's median.
    let mut per_hour: Vec<Vec<u64>> = vec![vec![]; 24];
    for j in &day { if let (Some(st), Some(at)) = (j["started_ms"].as_u64(), j["at_ms"].as_u64()) { per_hour[slot(j)].push(st.saturating_sub(at)) } }
    let wait_spark = spark(&per_hour.iter_mut().enumerate().map(|(i, w)| { w.sort(); let m = w.get(w.len() / 2).copied().unwrap_or(0); (m as f64, 0.0, if w.is_empty() { format!("{}: no jobs", hour_name(i)) } else { format!("{}: {} s median", hour_name(i), (m + 500) / 1000) }) }).collect::<Vec<_>>());
    let wait_kpi = kpi(median.map(|m| if m < 60_000 { format!("{} s", (m + 500) / 1000) } else { format!("{} min", m / 60_000) }).unwrap_or_else(|| "—".into()),
        "median wait for a machine", wait_spark, if waits.is_empty() { "From queued to running".into() } else { format!("Fastest {} s · slowest {} s", waits[0] / 1000, waits[waits.len() - 1] / 1000) });
    // What it cost, and what GitHub would have charged for the same jobs.
    let top = spent.max(github).max(0.000_001);
    let compare = format!(r#"<div class="vs"><div><span>Yours</span><span class="vs-bar"><i class="you" style="width:{:.0}%"></i></span><span class="mono">{}</span></div><div><span>GitHub</span><span class="vs-bar"><i class="them" style="width:{:.0}%"></i></span><span class="mono">{}</span></div></div>"#,
        spent / top * 100.0, usd(spent), github / top * 100.0, usd(github));
    // GitHub's price for the same jobs, behind a small mark (shown on hover or focus).
    let versus = if github <= 0.0 { String::new() } else {
        let saved = if spent < github { format!("{:.0}% less", (1.0 - spent / github) * 100.0) } else { "vs GitHub".into() };
        format!(r#"<span class="gh-mark" tabindex="0">{}<span>{saved}</span><span class="gh-pop" role="tooltip"><strong>Against GitHub's hosted runners</strong>{compare}<small>The same jobs on GitHub's runners of the same size, billed by the minute.</small></span></span>"#, logo("github", 14))
    };
    // Spend, hour by hour.
    let mut spend_hours = vec![0.0_f64; 24];
    for j in &day { spend_hours[slot(j)] += job_cost(j).unwrap_or(0.0) }
    let spend_spark = spark(&spend_hours.iter().enumerate().map(|(i, c)| (*c, 0.0, format!("{}: {}", hour_name(i), usd(*c)))).collect::<Vec<_>>());
    // Estimated only while some of it is (a job still running, or a cost its cloud has not settled yet).
    let settled = day.iter().filter(|j| job_cost(j).is_some_and(|c| c > 0.0)).all(|j| j["cost_from"].is_string());
    let spend_kpi = kpi(usd(spent), if settled { "spent" } else { "spent, estimated" }, spend_spark, versus);

    // No jobs at all yet: the page as it will be, with nothing in it.
    if jobs.is_empty() { return overview_empty_cards(&v.label()) }
    // The latest runs (a run is what a push or a pull request starts; its jobs are a click away).
    let runs = runs_of(&jobs);
    let latest = if runs.is_empty() { job_table(&jobs[..jobs.len().min(5)]) } else { run_table(&runs[..runs.len().min(5)]) };
    // Where they ran.
    let mut by: Vec<(String, String, usize, u64, f64)> = vec![];
    for j in &day {
        let cloud = j["cloud"].as_str().unwrap_or_default();
        // Only jobs that ran there.
        if cloud.is_empty() || j["started_ms"].is_null() { continue }
        let (key, logo_key) = (cloud.to_string(), cloud.to_string());
        let took = match (j["started_ms"].as_u64(), j["ended_ms"].as_u64()) { (Some(a), Some(b)) => b.saturating_sub(a), _ => 0 };
        match by.iter_mut().find(|b| b.0 == key) { Some(b) => { b.2 += 1; b.3 += took; b.4 += job_cost(j).unwrap_or(0.0) } None => by.push((key, logo_key, 1, took, job_cost(j).unwrap_or(0.0))) }
    }
    by.sort_by(|a, b| b.2.cmp(&a.2));
    let total: usize = by.iter().map(|b| b.2).sum();
    let split = if by.len() < 2 { String::new() } else {
        let name = |key: &str| cloud_name(key).to_string();
        let bar = by.iter().enumerate().map(|(i, (_, _, n, _, _))| format!(r#"<i class="c{}" style="flex:{n}"></i>"#, i.min(4))).collect::<String>();
        let legend = by.iter().enumerate().map(|(i, (key, logo_key, n, _, cost))| format!(r#"<span class="split-item"><i class="c{}"></i>{}<strong>{}</strong><span class="faint">{n} job{} · {}</span></span>"#,
            i.min(4), logo(logo_key, 16), esc(&name(key)), if *n == 1 { "" } else { "s" }, usd(*cost))).collect::<String>();
        format!(r#"<div class="sub-block"><h3>Where they ran</h3><div class="split" title="{total} jobs that ran">{bar}</div><div class="split-legend">{legend}</div></div>"#)
    };
    let figures = if day.is_empty() { r#"<p class="note">No jobs in the last day.</p>"#.to_string() } else { format!(r#"<div class="kpis">{jobs_kpi}{wait_kpi}{spend_kpi}</div>"#) };
    // From a control plane that counts every job it keeps (0.18.2 and newer): the period chosen, with its minutes
    // and what the code host's own runners would have charged. Else the last day of the latest jobs, as before.
    let (head, figures, split) = match v.status.as_ref().map(|s| &s["usage"]).filter(|u| u.is_object()) {
        Some(all) => {
            let period = if ["week", "month"].contains(&period) { period } else { "day" };
            let u = &all[period];
            let head = format!(r#"<h2>Usage</h2><span class="card-sub"></span>{}"#, usage_switch(period));
            let figures = if u["jobs"].as_u64().unwrap_or(0) == 0 { format!(r#"<p class="note">No jobs in the last {}.</p>"#, match period { "week" => "7 days", "month" => "30 days", _ => "24 hours" }) } else { usage_figures(u, period, &jobs) };
            (head, figures, if period == "day" { split } else { String::new() })
        }
        None => (r#"<h2>Jobs</h2><span class="card-sub">Last 24 hours</span>"#.to_string(), figures, split),
    };
    // The period in one card; the latest jobs in their own, with all of them a click away.
    let jobs_card = format!(r#"<section class="card big jobs-card"><div class="card-head">{head}</div>{figures}{split}</section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div>{latest}<div class="card-foot end"><a class="chip" href="/?p={}">All {} ›</a></div></section>"#, if runs.is_empty() { "jobs" } else { "runs" }, if runs.is_empty() { "jobs" } else { "runs" });

    jobs_card
}

/// The last few finished jobs' log views, by control plane, job and GitLab connection (see `/job/view`).
static LOG_VIEWS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Overview's period, as its link last said: day, week or month (one dashboard, one person looking).
static USAGE_SHOWN: Mutex<String> = Mutex::new(String::new());

fn usage_shown() -> String { let p = lock(&USAGE_SHOWN).clone(); if ["week", "month"].contains(&p.as_str()) { p } else { "day".into() } }

/// Overview's period switch: 24 hours, 7 days, 30 days, the one shown marked.
fn usage_switch(period: &str) -> String {
    let links = [("day", "24 hours"), ("week", "7 days"), ("month", "30 days")].iter()
        .map(|(p, name)| format!(r#"<a href="/?p=overview&amp;usage={p}"{}>{name}</a>"#, if *p == period { r#" class="on" aria-current="true""# } else { "" })).collect::<String>();
    format!(r#"<nav class="seg seg-links" aria-label="Period">{links}</nav>"#)
}

/// Overview's four figures for a period, from the control plane's count of every job in it (see
/// `superci_core::plane::usage`): jobs, machine minutes, the wait for a machine, and what they cost beside what the
/// code host's own runners would have charged. Each with its hours (a day) or days (a week, 30 days) as bars.
fn usage_figures(u: &serde_json::Value, period: &str, jobs: &[serde_json::Value]) -> String {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let f = |k: &str| u[k].as_f64().unwrap_or(0.0);
    let steps = u["steps"].as_array().cloned().unwrap_or_default();
    let count = steps.len();
    let step_name = |i: usize| { let ago = count - 1 - i; match (period, ago) { ("day", 0) => "this hour".to_string(), ("day", 1) => "an hour ago".into(), ("day", a) => format!("{a} hours ago"), (_, 0) => "the last 24 hours".into(), (_, 1) => "the day before".into(), (_, a) => format!("{a} days ago") } };
    let kpi = |big: String, caption: &str, spark: String, sub: String| format!(r#"<div class="kpi"><div class="kpi-num">{big}</div><div class="kpi-cap">{caption}</div>{spark}<div class="kpi-sub">{sub}</div></div>"#);
    let spark = |values: Vec<(f64, f64, String)>| {
        let most = values.iter().map(|(a, b, _)| a + b).fold(0.0_f64, f64::max).max(f64::MIN_POSITIVE);
        let w = 240.0 / values.len().max(1) as f64;
        let bar = (w * 0.6).max(2.0);
        let bars = values.iter().enumerate().map(|(i, (a, b, tip))| {
            let x = i as f64 * w + (w - bar) / 2.0;
            let (ha, hb) = (a / most * 30.0, b / most * 30.0);
            let lower = if *a > 0.0 { format!(r#"<rect class="a" x="{x:.1}" y="{:.1}" width="{bar:.1}" height="{:.1}" rx="1.5"/>"#, 34.0 - ha.max(1.5), ha.max(1.5)) } else { String::new() };
            let upper = if *b > 0.0 { format!(r#"<rect class="b" x="{x:.1}" y="{:.1}" width="{bar:.1}" height="{:.1}" rx="1.5"/>"#, 34.0 - ha - hb.max(1.5) - if *a > 0.0 { 1.5 } else { 0.0 }, hb.max(1.5)) } else { String::new() };
            format!(r#"<g><title>{}</title><rect class="hit" x="{:.1}" y="0" width="{w:.1}" height="36"/>{lower}{upper}</g>"#, esc(tip), i as f64 * w)
        }).collect::<String>();
        format!(r#"<svg class="spark" viewBox="0 0 240 36" preserveAspectRatio="none" aria-hidden="true"><line class="base" x1="0" y1="35.5" x2="240" y2="35.5"/>{bars}</svg>"#)
    };
    let s = |i: usize, k: &str| steps[i][k].as_f64().unwrap_or(0.0);
    // Jobs: those that ran and those that did not.
    let jobs_spark = spark((0..count).map(|i| (s(i, "ran"), s(i, "not_ran"), format!("{}: {} ran{}", step_name(i), s(i, "ran"), if s(i, "not_ran") > 0.0 { format!(", {} did not", s(i, "not_ran")) } else { String::new() }))).collect());
    let jobs_kpi = kpi(n("jobs").to_string(), if n("jobs") == 1 { "job" } else { "jobs" }, jobs_spark,
        if n("not_ran") == 0 { format!("{} ran", n("ran")) } else { format!(r#"{} ran<span class="sep">·</span><span class="bad-text">{} did not</span>"#, n("ran"), n("not_ran")) });
    // Runs, from a control plane that counts them (0.19.6 and newer): their number, with their jobs beside it.
    let jobs_kpi = if u["runs"].is_u64() && n("runs") > 0 {
        let runs_spark = spark((0..count).map(|i| (s(i, "runs"), 0.0, format!("{}: {} {}", step_name(i), s(i, "runs"), if s(i, "runs") == 1.0 { "run" } else { "runs" }))).collect());
        kpi(n("runs").to_string(), if n("runs") == 1 { "run" } else { "runs" }, runs_spark,
            if n("not_ran") == 0 { format!("{} {}", n("jobs"), if n("jobs") == 1 { "job" } else { "jobs" }) } else { format!(r#"{} {}<span class="sep">·</span><span class="bad-text">{} did not run</span>"#, n("jobs"), if n("jobs") == 1 { "job" } else { "jobs" }, n("not_ran")) })
    } else { jobs_kpi };
    // Minutes: what the machines ran, and what the code host bills (each job rounded up to a minute).
    let minutes = |m: f64| if m >= 6000.0 { format!("{:.0} h", m / 60.0) } else if m >= 100.0 { format!("{:.0} min", m) } else { format!("{:.1} min", m) };
    let host = if jobs.iter().any(|j| j["provider"] == "gitlab") && jobs.iter().any(|j| j["provider"] != "gitlab") { "GitHub and GitLab" } else if jobs.iter().any(|j| j["provider"] == "gitlab") { "GitLab" } else { "GitHub" };
    let minutes_spark = spark((0..count).map(|i| (s(i, "minutes"), 0.0, format!("{}: {}", step_name(i), minutes(s(i, "minutes"))))).collect());
    // (Its label in the unit its figure is in: hours from 6,000 minutes on.)
    let minutes_kpi = kpi(minutes(f("machine_minutes")), if f("machine_minutes") >= 6000.0 { "machine hours" } else { "machine minutes" }, minutes_spark, format!("{} min as {host} {} them", n("billed_minutes"), if host.contains(" and ") { "bill" } else { "bills" }));
    // The wait for a machine: each step's median.
    let secs = |ms: u64| if ms < 60_000 { format!("{} s", (ms + 500) / 1000) } else { format!("{} min", ms / 60_000) };
    let wait_spark = spark((0..count).map(|i| { let m = steps[i]["wait_median_ms"].as_u64(); (m.unwrap_or(0) as f64, 0.0, match m { Some(m) => format!("{}: {} median", step_name(i), secs(m)), None => format!("{}: no jobs", step_name(i)) }) }).collect());
    let wait_kpi = kpi(u["wait_median_ms"].as_u64().map(secs).unwrap_or_else(|| "—".into()), "median wait for a machine", wait_spark,
        match (u["wait_fastest_ms"].as_u64(), u["wait_slowest_ms"].as_u64()) { (Some(a), Some(b)) => format!("Fastest {} · slowest {}", secs(a), secs(b)), _ => "From queued to running".into() });
    // What it cost, beside what the code host's runners of the same sizes would have charged: what that saved.
    let (spent, theirs) = (f("usd"), f("host_usd"));
    let spend_spark = spark((0..count).map(|i| (s(i, "usd"), 0.0, format!("{}: {} (at {host}: {})", step_name(i), usd(s(i, "usd")), usd(s(i, "host_usd"))))).collect());
    let top = spent.max(theirs).max(0.000_001);
    let compare = format!(r#"<div class="vs"><div><span>Yours</span><span class="vs-bar"><i class="you" style="width:{:.0}%"></i></span><span class="mono">{}</span></div><div><span>{host}</span><span class="vs-bar"><i class="them" style="width:{:.0}%"></i></span><span class="mono">{}</span></div></div>"#,
        spent / top * 100.0, usd(spent), theirs / top * 100.0, usd(theirs));
    let saved = if theirs > spent { format!(r#"<span class="good-text">{} saved</span><span class="sep">·</span>{:.0}% less"#, usd(theirs - spent), (1.0 - spent / theirs) * 100.0) } else if theirs > 0.0 { format!("{}: {}", host, usd(theirs)) } else { String::new() };
    let versus = format!(r#"<span class="gh-mark" tabindex="0">{saved}<span class="gh-pop" role="tooltip"><strong>Against {host}'s own runners</strong>{compare}<small>The same jobs on runners of the same size there, each billed by the minute.</small></span></span>"#);
    let spend_kpi = kpi(usd(spent), "spent", spend_spark, versus);
    format!(r#"<div class="kpis four">{jobs_kpi}{minutes_kpi}{wait_kpi}{spend_kpi}</div>"#)
}

/// Overview's cards before any job has run: the same shapes with nothing in them yet, so the page already shows what
/// it will say (the day's three figures, then the latest jobs).
fn overview_empty_cards(label: &str) -> String {
    let spark = r#"<svg class="spark" viewBox="0 0 240 36" preserveAspectRatio="none" aria-hidden="true"><line class="base" x1="0" y1="35.5" x2="240" y2="35.5"/></svg>"#;
    let kpi = |big: &str, caption: &str, sub: &str| format!(r#"<div class="kpi is-empty"><div class="kpi-num">{big}</div><div class="kpi-cap">{caption}</div>{spark}<div class="kpi-sub">{sub}</div></div>"#);
    let snippet = format!(r#"<pre class="snippet empty-snippet">jobs:
  test:
    <span class="hl">runs-on: {}</span></pre>"#, esc(label));
    format!(r#"<section class="card big jobs-card"><div class="card-head"><h2>Usage</h2><span class="card-sub"></span>{}</div><div class="kpis four">{}{}{}{}</div></section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div>{}</section>"#,
        usage_switch("day"), kpi("0", "jobs", "Hour by hour, once jobs run"), kpi("0 min", "machine minutes", "Beside what GitHub or GitLab would bill"), kpi("—", "median wait for a machine", "From queued to running"), kpi("$0.00", "spent", "Beside what GitHub or GitLab would charge"),
        empty_state(&icon("play"), "No jobs yet", "Every job with your label shows up here, with the machine it ran on, how long it took and what it cost:", &snippet))
}

/// Overview's cards while the control plane is being read: the same shapes, empty.
fn overview_skeleton_cards() -> String {
    let period = usage_shown();
    let kpi = r#"<div class="kpi"><span class="sk fig"></span><span class="sk w60 thin"></span><span class="sk spark-sk"></span><span class="sk w40 thin"></span></div>"#;
    format!(r#"<section class="card big jobs-card"><div class="card-head"><h2>Usage</h2><span class="card-sub">{LOADING}</span>{}</div><div class="kpis four">{}</div></section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div><div class="rows">{}</div></section>"#,
        usage_switch(&period), kpi.repeat(4), SK_ROWS.repeat(5))
}

/// The loader, inside the first card of a page that is still reading its control plane.
const ICON_ALERT: &str = r#"<svg viewBox="0 0 24 24"><path d="M12 3.5 2.5 20h19L12 3.5Z"/><path d="M12 10v4.5M12 17.5h.01"/></svg>"#;

const LOADING: &str = r#"<span class="loading-note"><span class="mini-spin"></span>Reading your control plane…</span>"#;

fn jobs_card(v: Option<&PlaneView>, limit: usize) -> String {
    let jobs = v.map(|v| v.jobs()).unwrap_or_default();
    // Each job links to where its GitHub or GitLab is.
    let listed: Vec<serde_json::Value> = v.map(|v| jobs.iter().take(limit).map(|j| with_hosts(v, j)).collect()).unwrap_or_default();
    let rows = match v { Some(_) if !listed.is_empty() => job_table(&listed), _ => String::new() };
    let label = esc(&v.map(|v| v.label()).unwrap_or_else(|| "superci".into()));
    let snippet = format!(r#"<pre class="snippet" style="text-align:left;padding:12px 16px;border-radius:10px;background:var(--card-2)">jobs:
  test:
    <span class="hl">runs-on: {label}</span></pre>"#);
    // (A control plane whose status is not read yet, or not at all, never gets here: its page waits, or says why.)
    let body = match v {
        Some(_) if rows.is_empty() => empty_state(&icon("play"), "No jobs yet", "They show up here as soon as a workflow uses your label:", &snippet),
        Some(_) => format!("{}{rows}", spot_limit_notice(&jobs)),
        None => empty_state(&icon("play"), "No jobs yet", "Once setup is done, every job with this label shows up here, with the machine it ran on:", &snippet),
    };
    format!(r#"<section class="card"><div class="card-head"><h2>Recent jobs</h2></div>{body}</section>"#)
}

/// The Jobs page's two ways to look: job by job, or by the run they are of.
fn jobs_tabs(runs: bool) -> String {
    format!(r#"<div class="tabs" role="tablist"><a class="tab" role="tab" href="/?p=runs" aria-selected="{runs}">Runs</a><a class="tab" role="tab" href="/?p=jobs" aria-selected="{}">Jobs</a></div>"#, !runs)
}

/// The latest runs, of the jobs the control plane keeps.
fn runs_card(v: Option<&PlaneView>) -> String {
    let jobs: Vec<serde_json::Value> = v.map(|v| v.jobs().iter().map(|j| with_hosts(v, j)).collect()).unwrap_or_default();
    let runs = runs_of(&jobs);
    // (As for jobs: one whose status is not read never gets here.)
    let body = match v {
        _ if runs.is_empty() => empty_state(&icon("play"), "No runs yet", "A workflow's jobs that ran here show up together, with how long the run took and what it cost.", ""),
        _ => format!(r#"{}<p class="note" style="margin:10px 8px 0">A run's time is from its first job queued here to its last one over. Jobs of it on other runners are not counted.</p>"#, run_table(&runs)),
    };
    format!(r#"<section class="card"><div class="card-head"><h2>Recent runs</h2></div>{body}</section>"#)
}

const ICON_OVERVIEW: &str = r#"<svg viewBox="0 0 24 24"><rect x="3.5" y="3.5" width="7" height="7" rx="2"/><rect x="13.5" y="3.5" width="7" height="7" rx="2"/><rect x="3.5" y="13.5" width="7" height="7" rx="2"/><rect x="13.5" y="13.5" width="7" height="7" rx="2"/></svg>"#;
const ICON_RUNNERS: &str = r#"<svg viewBox="0 0 24 24"><rect x="3.5" y="4" width="17" height="6.5" rx="2"/><rect x="3.5" y="13.5" width="17" height="6.5" rx="2"/><path d="M7 7.25h.01M7 16.75h.01"/></svg>"#;
const ICON_REPOS: &str = r#"<svg viewBox="0 0 24 24"><path d="M5 4.5A1.5 1.5 0 0 1 6.5 3H19v15H6.5A1.5 1.5 0 0 0 5 19.5"/><path d="M5 19.5A1.5 1.5 0 0 0 6.5 21H19v-3"/><path d="M5 4.5v15"/><path d="M9 7h6"/></svg>"#;
/// The control plane: the always-on core, its runners in orbit.
const ICON_PLANE: &str = r#"<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3.2"/><path d="M17 18.88A8.5 8.5 0 0 1 7 18.88M3.55 12.89A8.5 8.5 0 0 1 8.54 4.24M15.46 4.24A8.5 8.5 0 0 1 20.45 12.89"/><circle cx="12" cy="3.5" r="1.9" fill="currentColor" stroke="none"/><circle cx="19.36" cy="16.25" r="1.9" fill="currentColor" stroke="none"/><circle cx="4.64" cy="16.25" r="1.9" fill="currentColor" stroke="none"/></svg>"#;
const ICON_MORE: &str = r#"<svg viewBox="0 0 24 24"><circle cx="5.5" cy="12" r="1.4" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none"/><circle cx="18.5" cy="12" r="1.4" fill="currentColor" stroke="none"/></svg>"#;
const ICON_WORKFLOWS: &str = r#"<svg viewBox="0 0 24 24"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5M10 13l-2 2 2 2M14 13l2 2-2 2"/></svg>"#;
const ICON_NEWS: &str = r#"<svg viewBox="0 0 24 24"><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z"/></svg>"#;
const ICON_JOBS: &str = r#"<svg viewBox="0 0 24 24"><path d="M8 6h12M8 12h12M8 18h12"/><path d="M4 6h.01M4 12h.01M4 18h.01"/></svg>"#;

/// Checks a GitLab token: who it is, and that it has the scopes SuperCI needs.
fn gitlab_check(url: &str, token: &str) -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(20))).build().into();
    let get = |path: &str| -> Result<serde_json::Value> {
        let mut r = agent.get(&crate::aws::stand_in(&format!("{url}/api/v4{path}"))).header("private-token", token).call().map_err(|e| format!("could not reach {url}: {e}"))?;
        let status = r.status().as_u16();
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        if status == 401 { return Err("the token is not valid (or has expired)".into()) }
        if status >= 300 { return Err(format!("GitLab answered {status}")) }
        Ok(v)
    };
    let me = get("/user")?;
    let scopes: Vec<String> = get("/personal_access_tokens/self").map(|t| t["scopes"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let missing: Vec<&str> = ["api", "create_runner", "manage_runner"].into_iter().filter(|s| !scopes.iter().any(|x| x == s)).collect();
    if !scopes.is_empty() && !missing.is_empty() { return Err(format!("it needs the scopes {}", missing.join(", "))) }
    Ok(format!("@{}", me["username"].as_str().unwrap_or("?")))
}

/// Whether a GitHub name is an organization or a person (GitHub's public profile; no sign-in needed).
/// Where an App is managed on GitHub (its owner's settings).
fn app_settings_link(host: Option<&str>, owner: &str, slug: &str, org: bool) -> String {
    let web = github::web_base(host);
    if org { format!("{web}/organizations/{owner}/settings/apps/{slug}") } else { format!("{web}/settings/apps/{slug}") }
}

/// A job as the pages link it: where its GitLab is, and where its GitHub is (its App's; github.com unless said).
pub fn with_hosts(v: &PlaneView, j: &serde_json::Value) -> serde_json::Value {
    let mut j = j.clone();
    let gls = v.gitlabs();
    let its = j["gitlab"].as_str().unwrap_or_default();
    if let Some((_, u, _)) = gls.iter().find(|(id, ..)| id == its).or(gls.first()) { j["gitlab_url"] = u.clone().into() }
    let apps = v.status.as_ref().map(status_apps).unwrap_or_default();
    let app = apps.iter().find(|a| a["id"] == j["app_id"]).or(apps.first());
    if let Some(h) = app.and_then(|a| a["host"].as_str()) { j["github_web"] = github::web_base(Some(h)).into() }
    j
}

/// A control plane's GitHub Apps as its status has them (one per organization; older control planes: the one).
fn status_apps(st: &serde_json::Value) -> Vec<serde_json::Value> {
    match st["apps"].as_array() {
        Some(a) => a.clone(),
        None if st["app"].is_object() => { let mut a = st["app"].clone(); a["permissions"] = st["permissions"]["github_app"].clone(); vec![a] }
        None => vec![],
    }
}

/// Whether a name is an organization's or a user's, asked of that GitHub. One elsewhere that does not answer without
/// a sign-in (a GitHub Enterprise Server in private mode): taken as an organization's.
fn github_owner(host: Option<&str>, login: &str) -> Result<Owner> { github_owner_at(&github::api_base(host), host, login) }

fn github_owner_at(api: &str, host: Option<&str>, login: &str) -> Result<Owner> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(15))).build().into();
    let asked = agent.get(&crate::aws::stand_in(&format!("{api}/users/{login}"))).header("accept", "application/vnd.github+json").header("user-agent", "superci-cli").call();
    let mut r = match (asked, host) {
        (Ok(r), Some(_)) if [401, 403, 404].contains(&r.status().as_u16()) => return Ok(Owner { org: true, login: login.to_string() }),
        (Ok(r), _) => r,
        (Err(e), Some(h)) => return Err(format!("{h} did not answer from this computer ({e}).")),
        (Err(e), None) => return Err(e.to_string()),
    };
    if r.status().as_u16() == 404 { return Err(format!("There is no GitHub organization or user named {login}.")) }
    let v: serde_json::Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let login = v["login"].as_str().ok_or("GitHub did not answer about that name; try again in a minute")?.to_string();
    Ok(Owner { org: v["type"] == "Organization", login })
}

/// A GitLab connection's name as it may appear in an address or a setting's name: lowercase letters and digits only.
fn safe_id(id: &str) -> String { id.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).take(12).collect() }

/// A page to come back to after a sign-in: only this dashboard's own pages (`?p=…`, with the workspace and the
/// control plane their address names).
fn safe_return(next: &str) -> String {
    let ok = next.starts_with('?') && next.len() < 120 && next[1..].bytes().all(|b| b.is_ascii_alphanumeric() || b"=&-_".contains(&b));
    if ok { next.to_string() } else { String::new() }
}

/// Every workspace's dashboard behind one address: each a world of its own (its sign-ins, its control planes, what
/// is on its way), made when a page first asks for it. A request says which workspace it is for in its address or its
/// form (`w`).
struct Hub {
    /// SuperCI's folder (none: nothing is kept on this machine, and there is the one workspace).
    root: Option<std::path::PathBuf>,
    key: String,
    base: String,
    /// The workspace the program was started in: what a page whose address names none is of. None: it was started
    /// with several and none named, and such a page is the list of them.
    first: Mutex<Option<String>>,
    boards: Mutex<HashMap<String, Arc<Mutex<Dashboard>>>>,
    /// The workspace whose page was opened last (noted in SuperCI's folder when it changes, see `store::last_opened`).
    opened: Mutex<Option<String>>,
}

impl Hub {
    fn first(&self) -> Option<String> { lock(&self.first).clone() }

    /// The workspaces here, by name: those kept, and the one this was started in (which has no folder until it
    /// keeps something).
    fn names(&self) -> Vec<String> {
        let mut names = self.root.as_ref().map(|r| crate::store::workspaces_in(r)).unwrap_or_default();
        if let Some(f) = self.first().filter(|f| !names.contains(f)) { names.push(f); names.sort() }
        names
    }

    fn has(&self, name: &str) -> bool { self.names().iter().any(|n| n == name) }

    /// A workspace's dashboard: the one made for it before, or a new one from what it keeps.
    fn board(&self, name: &str) -> Arc<Mutex<Dashboard>> {
        let mut boards = lock(&self.boards);
        if let Some(b) = boards.get(name) { return b.clone() }
        let mut d = Dashboard::signed_in(self.root.as_ref().map(|r| Store::under(r, name)));
        (d.key, d.base, d.named) = (self.key.clone(), self.base.clone(), name.to_string());
        let b = Arc::new(Mutex::new(d));
        boards.insert(name.to_string(), b.clone());
        b
    }

    /// The workspace a sign-in or an App that comes back from elsewhere was started in (its address names none).
    fn awaiting(&self, req: &Req) -> Option<String> {
        let boards: Vec<(String, Arc<Mutex<Dashboard>>)> = lock(&self.boards).iter().map(|(n, b)| (n.clone(), b.clone())).collect();
        boards.into_iter().find(|(_, b)| lock(b).awaits(req)).map(|(n, _)| n)
    }

    /// The list of workspaces, each a link to its pages, and making a new one: what the dashboard opens on when it
    /// was started with several and none named.
    fn list_page(&self) -> Response {
        let rows = self.root.as_ref().map(|r| crate::store::listed(r, self.first().as_deref())).unwrap_or_default().iter().map(|l|
            format!(r#"<a class="pick" href="/?w={}&c=">{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{}</span></a>"#, esc(&l.id), workspace_logo(l, 36), esc(l.title()), esc(&workspace_sub(l)), icon("arrow"))).collect::<String>();
        document(200, "SuperCI", &format!(r#"<div class="spaces"><div>{MARK}<h1>Workspaces</h1><p>Each has its own sign-ins, control plane and runners.</p><div class="picks">{rows}</div><button type="button" class="button secondary" onclick="document.getElementById('new-workspace').showModal()">New workspace</button></div></div>{}"#, new_workspace_dialog()), None)
    }

    /// A new, empty workspace, called `name`. Its id, made from the name.
    fn create(&self, name: &str) -> Result<String> {
        let root = self.root.as_ref().ok_or("SuperCI keeps nothing on this machine (it has no home folder), so there is the one workspace.")?;
        let name = name.trim().to_string();
        superci_core::spec::valid_name(&name)?;
        // (Every valid name makes a valid id.)
        let id = crate::store::id_from(&name).unwrap_or(crate::store::FIRST.to_string());
        if self.has(&id) { return Err(format!("There is a workspace with the id {id} already. Give this one another name.")) }
        // The one this was started in has no folder until it keeps something: it gets one now, so that it stays
        // listed beside the new one.
        if let Some(first) = self.first() { Store::under(root, &first).make()? }
        Store::under(root, &id).set_about(&crate::store::About { name, ..Default::default() })?;
        Ok(id)
    }

    /// A workspace under a new id: its folder is renamed, and its dashboard goes on under it.
    fn rename(&self, from: &str, to: &str) -> Result<String> {
        let root = self.root.as_ref().ok_or("SuperCI keeps nothing on this machine (it has no home folder).")?;
        let to = to.trim().to_lowercase();
        crate::store::valid_id(&to)?;
        if to == from { return Ok(to) }
        if self.has(&to) { return Err(format!("There is a workspace with the id {to} already.")) }
        let board = lock(&self.boards).remove(from);
        if let Some(b) = &board { lock(b).keep() }
        if let Err(e) = crate::store::rename(root, from, &to) {
            if let Some(b) = board { lock(&self.boards).insert(from.to_string(), b); }
            return Err(e)
        }
        if let Some(b) = board {
            { let mut d = lock(&b); d.store = Some(Store::under(root, &to)); d.named = to.clone(); }
            lock(&self.boards).insert(to.clone(), b);
        }
        let mut first = lock(&self.first);
        if first.as_deref() == Some(from) { *first = Some(to.clone()) }
        Ok(to)
    }

    /// Forgets a workspace on this computer: signed out as Sign out does, then its folder goes. Nothing in its clouds
    /// is deleted. Where to go from there: the only one left, or the list.
    fn remove(&self, name: &str) -> Result<String> {
        let root = self.root.as_ref().ok_or("SuperCI keeps nothing on this machine (it has no home folder).")?;
        let board = self.board(name);
        lock(&self.boards).remove(name);
        {
            let mut d = lock(&board);
            d.sign_out()?;
            // (Whatever of it is still on its way writes nowhere from here on.)
            d.store = None;
        }
        Store::under(root, name).forget()?;
        let left = crate::store::workspaces_in(root);
        let mut first = lock(&self.first);
        if first.as_deref() == Some(name) || left.len() < 2 { *first = match left.as_slice() { [] => Some(crate::store::FIRST.to_string()), [only] => Some(only.clone()), _ => None } }
        Ok(match left.as_slice() { [only] => format!("/?w={only}&c="), [] => "/".to_string(), _ => "/workspaces".to_string() })
    }
}

/// The dashboard's page opened in the browser.
#[cfg(not(debug_assertions))]
fn open_link(link: &str) { let _ = open::that(link); }

/// In a debug build, as the tests run it, only with the program SUPERCI_BROWSER names, and otherwise not at all: a test
/// never opens a real browser (macOS's opener is no program a test can put first on the PATH).
#[cfg(debug_assertions)]
fn open_link(link: &str) {
    if let Some(browser) = std::env::var_os("SUPERCI_BROWSER") { let _ = std::process::Command::new(browser).arg(link).status(); }
}

/// A GitHub organization's (or person's) picture, as github.com serves it to anyone: small, and as a `data:` address,
/// so that a page shows it without asking any other site. None: GitHub did not give one now.
fn github_picture(owner: &str) -> Option<String> {
    if !github::valid_login(owner) { return None }
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(4))).build().into();
    let mut answer = agent.get(&picture_at(&format!("https://github.com/{owner}.png?size=96"))).call().ok()?;
    if answer.status().as_u16() != 200 { return None }
    let kind = answer.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().split(';').next().unwrap_or_default().trim().to_string();
    if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&kind.as_str()) { return None }
    let bytes = answer.body_mut().with_config().limit(300_000).read_to_vec().ok()?;
    Some(format!("data:{kind};base64,{}", superci_core::crypto::b64(&bytes)))
}

/// Where a picture on github.com is asked for: there; in a debug build with `SUPERCI_PICTURES_AT`, at that stand-in
/// (as `aws::stand_in` sends the sites it knows), for a test to answer it.
fn picture_at(url: &str) -> String {
    #[cfg(debug_assertions)]
    if let Some(at) = std::env::var("SUPERCI_PICTURES_AT").ok() { return format!("{}/{}", at.trim_end_matches('/'), url.trim_start_matches("https://")) }
    url.to_string()
}

/// The mark at the top of the sidebar: the workspace's picture when it has one, SuperCI's own otherwise (which stays
/// in the page, for when the picture goes).
fn brand_mark(picture: &str) -> String {
    if safe_picture(picture) { format!(r#"<img class="mark" src="{picture}" alt="">{}"#, MARK.replacen("<svg ", r#"<svg hidden "#, 1)) } else { MARK.to_string() }
}

/// Whether a kept picture is one this program fetched and encoded itself (a `data:` address of letters, digits and
/// base64's marks), and so can go into a page as it is.
fn safe_picture(picture: &str) -> bool {
    picture.strip_prefix("data:image/").is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_alphanumeric() || b";,+/=".contains(&b)))
}

/// A workspace's mark in a list: its GitHub organization's picture, else its code host's logo once one is connected,
/// and its initial until then.
fn workspace_logo(l: &crate::store::Listed, size: u32) -> String {
    if safe_picture(&l.picture) { return format!(r#"<img class="logo" src="{}" width="{size}" height="{size}" alt="">"#, l.picture) }
    match l.hosts.first() {
        Some((kind, _)) => logo(kind, size),
        None => format!(r#"<span class="logo initial" style="width:{size}px;height:{size}px;font-size:{}px">{}</span>"#, size * 45 / 100, esc(&l.title().chars().take(1).collect::<String>().to_uppercase())),
    }
}

/// A workspace in a few words: whose jobs it runs; before that, where its control plane is, or that nothing is set up.
fn workspace_sub(l: &crate::store::Listed) -> String {
    let mut parts: Vec<String> = l.hosts.iter().map(|(_, name)| name.clone()).collect();
    if parts.is_empty() && !l.place.is_empty() { parts.push(l.place.clone()) }
    if parts.is_empty() { parts.push(if l.signed_in { "No control plane yet" } else { "Nothing set up yet" }.to_string()) }
    parts.join(" · ")
}

/// The guide on what an AWS account allows, where it says how an organization's region rule is changed (by its
/// manager, in AWS's console: SuperCI changes no rule and needs none changed).
const MORE_REGIONS_GUIDE: &str = "https://superci.dev/docs/aws-accounts";

/// Shown in a Connect button's place just after coming back from GitHub, until the control plane says the App is
/// there.
const GITHUB_ON_ITS_WAY: &str = r#"<span class="waiting-for"><span class="mini-spin"></span>Connecting GitHub: a few seconds…</span>"#;

/// The dialog that makes a workspace, in the look of the other step dialogs: its name, and nothing else. It is as
/// new each time it opens.
fn new_workspace_dialog() -> String {
    let body = r#"<form method="post" action="/workspaces">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Name your workspace</h2></div><div class="perm-field"><input type="text" name="name" placeholder="AcmeCI" required autofocus pattern="[A-Za-z][A-Za-z0-9 \-]{1,23}" title="2 to 24 letters, digits, spaces and dashes" autocomplete="off" spellcheck="false" aria-label="The workspace's name"><small>What its CI is called, here and on GitHub.</small></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Create it</h2></div><p class="prompt-modal-instruction">It gets separate sign-ins, control plane and runners. Nothing is made in any cloud yet.</p><div class="perm-action"><button class="prompt-modal-copy">Create workspace</button></div></section>
</form>"#;
    step_dialog("new-workspace", "New workspace", body).replacen(r#"onclick="if(event.target===this)this.close()""#, r#"onclick="if(event.target===this)this.close()" onclose="this.querySelector('form').reset()""#, 1)
}

const ICON_LOCK: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="10.5" width="14" height="10" rx="2"/><path d="M8.5 10.5V7.5a3.5 3.5 0 0 1 7 0v3"/></svg>"#;
const ICON_REMOVE: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 7h14M10 7V5h4v2M7 7l.8 12h8.4L17 7M10.5 11v5M13.5 11v5"/></svg>"#;
const ICON_WORKSPACE: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20V6.5A1.5 1.5 0 0 1 5.5 5h8A1.5 1.5 0 0 1 15 6.5V20M15 10h3.5a1.5 1.5 0 0 1 1.5 1.5V20M2.5 20h19M8 9h3M8 12.5h3M8 16h3"/></svg>"#;

/// The key and cookie checked, and where a request comes from. What it is refused with, or (on the first visit) sent
/// on with; nothing when it may go on.
fn admit(key: &str, req: &Req) -> Option<Response> {
    let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
    // The first visit carries the key; later ones its cookie. SameSite=Lax: other sites' requests and form posts never carry it,
    // but coming back from a sign-in does (AWS's returns to 127.0.0.1, which is another site to a browser than localhost).
    // The link may name a page too (`/?k=…&p=job&id=…`): sent on to it, without the key.
    if req.path == "/" && safe_eq(q("k").as_bytes(), key.as_bytes()) {
        let rest = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(req.query.iter().filter(|(k, _)| k != "k")).finish();
        let to = if rest.is_empty() { "/".to_string() } else { format!("/?{rest}") };
        return Some(Response::redirect(&to).with_header("set-cookie", &format!("{}={key}; Path=/; HttpOnly; SameSite=Lax", key_cookie(&req.host))));
    }
    // Only as this computer's own address (no other name pointing here), and changes only from the dashboard's own pages:
    // a page on another local port shares the cookie (cookies are not kept per port), not the origin.
    let local = ["localhost:", "127.0.0.1:", "[::1]:"].iter().any(|p| req.host.strip_prefix(p).is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())));
    if !local { return Some(message(403, "Not this computer's address", "Open the dashboard at the link `superci dashboard` printed when it started.")) }
    // Browsers say where a request comes from (Sec-Fetch-Site; "same-origin" from these pages). Without it, the Origin
    // (which these pages' no-referrer policy turns into "null" on their own forms, so it only counts when it names a site).
    let own = match req.fetch_site.as_deref() {
        Some(site) => site == "same-origin",
        None => req.origin.as_deref().is_none_or(|o| o == "null" || o == format!("http://{}", req.host)),
    };
    if req.method == "POST" && !own {
        return Some(message(403, "Not from your dashboard", "Changes are made from the dashboard's own page. Nothing has changed."));
    }
    // Redirects back from GitHub, Cloudflare, AWS and Modal come from their sites: no cookie; the state they carry guards them.
    if !req.cookie.as_deref().is_some_and(|c| safe_eq(c.as_bytes(), key.as_bytes())) && !CALLBACKS.contains(&req.path.as_str()) {
        return Some(message(403, "This tab is not connected to your dashboard", &format!("Open the link `superci dashboard` printed in your terminal when it started (it begins with http://{}/?k=). Nothing has changed.", esc(&req.host))));
    }
    None
}

/// Where GitHub, Cloudflare, AWS and Modal come back to: their addresses were fixed when the sign-in began, and name
/// no workspace.
const CALLBACKS: [&str; 4] = ["/github/callback", "/github/installed", "/oauth/callback", "/oauth/modal"];

/// One request: admitted, then handed to the dashboard of the workspace it names. With one workspace none needs
/// naming. With several, a page's address says which (one that says none is sent on to the one the program was
/// started in, or to the list), and anything that is not a page stops unless it says which: nothing picks a workspace
/// for a change.
fn respond(hub: &Hub, req: &Req) -> Response {
    if let Some(answer) = admit(&hub.key, req) { return answer }
    let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
    let field = |n: &str| url::form_urlencoded::parse(&req.body).find(|(k, _)| k == n).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let said = |r: Result<Response>| r.unwrap_or_else(|e| message(400, "That did not work", &esc(&e)));
    let named = Some(q("w").to_string()).filter(|w| !w.is_empty()).or_else(|| Some(field("w")).filter(|w| !w.is_empty()));
    // The workspaces themselves: the list of them, a new one, a new name for one, and forgetting one.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/workspaces") => return hub.list_page(),
        ("POST", "/workspaces") => return said(hub.create(&field("name")).map(|name| { println!("POST /workspaces → {name}"); Response::redirect(&format!("/?w={name}&c=")) })),
        ("POST", "/workspaces/rename") => {
            let Some(from) = named.clone().or_else(|| hub.first()).filter(|w| hub.has(w)) else { return message(409, "Which workspace?", "Open one from the list of workspaces and rename it from its Settings. Nothing has changed.") };
            return said(hub.rename(&from, &field("id")).map(|to| { println!("POST /workspaces/rename → {to}"); Response::redirect(&format!("/?p=settings&w={to}")) }))
        }
        ("POST", "/workspaces/remove") => {
            // Only the one its own page names: nothing picks a workspace to forget.
            let Some(name) = named.clone().filter(|w| hub.has(w)) else { return message(409, "Which workspace?", "Open one from the list of workspaces and remove it from its Settings. Nothing has changed.") };
            return said(hub.remove(&name).map(|to| { println!("POST /workspaces/remove → {name}"); Response::redirect(&to) }))
        }
        _ => {}
    }
    let page = req.method == "GET" && req.path == "/";
    let gone = |heading: &str, text: &str| {
        if page && q("fragment") == "1" { Response::new(200, "text/html; charset=utf-8", format!(r#"<div class="empty">{heading}. <a href="/workspaces" target="_top">Workspaces</a></div>"#)) }
        else if page { Response::redirect("/workspaces") }
        else { message(409, heading, text) }
    };
    // (Several are kept only in SuperCI's folder: with none, there is the one workspace.)
    let kept_in = hub.root.as_ref().filter(|_| hub.names().len() > 1);
    let several = kept_in.is_some();
    // Started with several workspaces and none named, a page that names none opens on the one shown last (else the
    // first): the switcher at the top leads to the others. Only pages: a change still says which workspace it is for.
    if named.is_none() && page && q("fragment") != "1" && hub.first().is_none() {
        let to = hub.root.as_ref().and_then(|r| crate::store::last_opened(r)).or_else(|| hub.names().into_iter().next());
        if let Some(to) = to {
            let rest: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(req.query.iter().map(|(k, v)| (k.as_str(), v.as_str()))).finish();
            return Response::redirect(&format!("/?{}{}w={to}", rest, if rest.is_empty() { "" } else { "&" }))
        }
    }
    let name = match named {
        Some(w) if hub.has(&w) => w,
        Some(_) => return gone("That workspace is not kept on this computer", "Open one from the list of workspaces. Nothing has changed."),
        None if CALLBACKS.contains(&req.path.as_str()) => match hub.awaiting(req).or_else(|| hub.first()) { Some(w) => w, None => return message(400, "This sign-in has expired", "Start again from the dashboard.") },
        None => match hub.first() {
            // A page: its address is given the name, so that everything done on it says which workspace it is for.
            Some(first) if several && page && q("fragment") != "1" => {
                let rest: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(req.query.iter().map(|(k, v)| (k.as_str(), v.as_str()))).finish();
                return Response::redirect(&format!("/?{}{}w={first}", rest, if rest.is_empty() { "" } else { "&" }))
            }
            Some(first) if !several || page => first,
            _ => return gone("Which workspace?", "Several workspaces are kept on this computer, and this page does not say which it is for. Open it again from the dashboard. Nothing has changed."),
        },
    };
    // A workspace's page is opened: noted, for where the dashboard opens next time.
    let opening = page && (q("fragment") != "1" || q("went") == "1");
    if let Some(root) = kept_in.filter(|_| opening) {
        let mut opened = lock(&hub.opened);
        if opened.as_deref() != Some(name.as_str()) { crate::store::note_opened(root, &name); *opened = Some(name.clone()); }
    }
    let shared = hub.board(&name);
    HERE.with(|h| h.borrow_mut().clone_from(&name));
    // (A page's frame says what the workspace is called before its control plane was asked.)
    if lock(called()).is_empty() { if let Ok(d) = shared.try_lock() { lock(called()).clone_from(&d.about.name); lock(pictured()).clone_from(&d.about.picture) } }
    let answer = respond_in(&shared, req);
    // Where it sends the page on within the dashboard, the workspace goes along.
    if !several { return answer }
    carry(answer, &name)
}

/// A redirect within the dashboard (a page, a cloud's sign-in page), with its workspace named in the address. Not
/// another site's (each is an https:// address). (The list of workspaces, no workspace's, is answered before.)
fn carry(mut answer: Response, workspace: &str) -> Response {
    for (_, v) in answer.headers.iter_mut().filter(|(k, _)| k == "location") {
        if !v.starts_with('/') { continue }
        // (A part of the page it leads to, `#…`, stays at the end.)
        let (before, hash) = v.split_once('#').map(|(b, h)| (b.to_string(), format!("#{h}"))).unwrap_or((v.clone(), String::new()));
        let (path, query) = before.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((before.clone(), String::new()));
        if query.split('&').any(|kv| kv.starts_with("w=")) { continue }
        *v = format!("{path}?{query}{}w={workspace}{hash}", if query.is_empty() { "" } else { "&" });
    }
    answer
}

/// One request within a workspace: the page frame drawn at once, its live part asked for without holding the
/// dashboard's state, and everything else handled with it.
fn respond_in(shared: &Arc<Mutex<Dashboard>>, req: &Req) -> Response {
    let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
    // The control plane this request names: in its address (`c`), or in its form.
    let c = Some(q("c").to_string()).filter(|c| !c.is_empty()).or_else(|| url::form_urlencoded::parse(&req.body).find(|(k, _)| k == "c").map(|(_, v)| v.into_owned()));
    view::name(c.as_deref().filter(|c| c.len() <= 24 && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())));
    // A command's tab (a sign-in, a GitHub App): what it is for, in the dashboard's place. A page that looks again by
    // itself is told when it is done.
    if req.method == "GET" && req.path == "/" {
        let mut d = lock(shared);
        if q("fragment") != "1" { if let Some(page) = d.task_page() { return page } }
        else if d.task_done().is_some() { return Response::new(200, "text/html; charset=utf-8", r#"<div class="empty">Done. Back to your terminal: you can close this tab.</div>"#.to_string()) }
    }
    if req.method == "GET" && req.path == "/" {
        // Overview's period, from its link (the page itself, its live part, and the shapes drawn while it loads).
        if ["", "overview"].contains(&q("p")) { *lock(&USAGE_SHOWN) = match q("usage") { u @ ("week" | "month") => u.to_string(), _ => "day".to_string() } }
        let new_here = match q("new") { "1" => Some(true), "0" => Some(false), _ => None };
        if q("fragment") == "1" {
            lock(&shared).gitlab_shown = q("g").chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).take(12).collect();
            if q("p") == "run" { lock(&shared).run_shown = (q("repo").chars().filter(|c| c.is_ascii_alphanumeric() || "-_./".contains(*c)).take(200).collect(), q("id").chars().filter(|c| c.is_ascii_digit()).take(20).collect(), q("attempt").chars().filter(|c| c.is_ascii_digit()).take(4).collect()) }
            if q("p") == "job" { lock(&shared).job_shown = (q("id").chars().filter(|c| c.is_ascii_digit()).take(20).collect(), q("gl").chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').take(24).collect()) }
            return Response::new(200, "text/html; charset=utf-8", Dashboard::live(shared, q("p"), q("plane").parse().ok(), new_here, req.last.as_deref()))
        }
        return Dashboard::page(q("p"));
    }
    let mut d = lock(&shared);
    // Which control plane this is for: the one its page names; else the only one. With several live and none
    // named, only what belongs to no control plane goes on (a sign-in, setting a new one up, deleting a named one).
    match view::named() {
        Some(id) => match d.planes.iter().position(|p| p.plane_id() == id) {
            Some(i) => d.select(i),
            None => return message(404, "That control plane is not known here", "Open the dashboard again and pick one from the list. Nothing has changed."),
        },
        None => {
            let apart = ["/signout", "/aws/signin", "/cloudflare/signin", "/modal/", "/plane/aws", "/plane/cloudflare", "/plane/modal", "/plane/start-over", "/aws/image", "/plane/delete", "/plane/join", "/connect/", "/oauth/", "/github/callback", "/github/installed"];
            if !apart.iter().any(|a| req.path.starts_with(a)) && d.planes.len() > 1 {
                let views = d.views.clone().map(|(v, _)| v).unwrap_or_else(|| d.planes.iter().map(|p| view::plane_view(p, None)).collect());
                if view::several(&views).is_some() { return message(409, "Which control plane?", "Several control planes are live here. Open one from the list and do this on its pages. Nothing has changed.") }
            }
        }
    }
    d.views = None;
    // Actions and sign-ins are told in the terminal too (never their codes or tokens).
    d.take_in();
    let r = d.handle(req);
    d.keep();
    // The dashboard's terminal says each action; a command that only opened the browser for one thing says that thing.
    if d.task.is_none() {
        match &r {
            Ok(ok) => println!("{} {} → {}", req.method, req.path, ok.status),
            Err(e) => println!("{} {} → failed: {e}", req.method, req.path),
        }
    }
    r.unwrap_or_else(|e| message(500, "Something went wrong", &esc(&d.ended_said(e))))
}

/// A change to a control plane restarts it (a new secret is a new version): waits (up to 20 s) until it shows the
/// change, so the page that follows shows it too.
/// A control plane's status for a change made now: it may still be taking this session's key (just after the
/// dashboard started), so asked again for a few seconds before giving up.
fn status_soon(plane_url: &str, key: &str) -> Option<serde_json::Value> { tries(8, 1500, || cloudflare::status_light(plane_url, key)) }

/// What `f` gives, asked up to `n` times, `ms` apart, until it gives something.
fn tries<T>(n: u32, ms: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for i in 0..n {
        if let Some(t) = f() { return Some(t) }
        if i + 1 < n { aws::pause(ms) }
    }
    None
}

/// Whether it did.
/// A new key for this folder's control planes (see `cloudflare::FOLDER_KEY`).
fn folder_key() -> String { format!("{}{}", cloudflare::FOLDER_KEY, random_token(24)) }

fn wait_for(plane_url: &str, key: &str, done: impl Fn(&serde_json::Value) -> bool) -> bool {
    tries(40, 500, || cloudflare::status_light(plane_url, key).filter(|s| done(s))).is_some()
}

/// The secrets the CI keeps for runs from a computer: each one's name, where it may be used, when it was set and last
/// given to a run (and who started that run), with Remove; and Add, a dialog with its name, value and scope.
fn secrets_card(said: Option<&serde_json::Value>) -> String {
    let Some(said) = said else { return String::new() };
    let now = said["now_ms"].as_u64().unwrap_or_else(now_ms);
    let mut rows = String::new();
    for s in said["secrets"].as_array().into_iter().flatten() {
        let used = match s["used"]["at_ms"].as_u64() {
            Some(at) => format!("last given to a run {}, started by {}", ago_words(now.saturating_sub(at)), esc(&s["used"]["by"].as_str().map(|b| if s["used"]["run_key"] == true { format!("the key {}", b.to_lowercase()) } else { b.to_string() }).unwrap_or_else(|| "a computer".into()))),
            None => "not given to a run yet".into(),
        };
        let (name, scope) = (s["name"].as_str().unwrap_or_default(), &s["scope"]);
        let hidden: String = ["repo", "branch", "workflow", "environment"].iter().filter_map(|k| scope[*k].as_str().map(|v| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v)))).collect();
        rows.push_str(&format!(r#"<div class="limit"><div class="limit-text"><strong><code>{}</code></strong><small>{} · set {} · {used}</small></div><div class="limit-do"><form method="post" action="/secrets/remove" onsubmit="return confirm('Remove this secret? Runs from a computer stop getting it.')"><input type="hidden" name="name" value="{}">{hidden}<button class="button secondary sm">Remove</button></form></div></div>"#,
            esc(name), esc(&capitalized(s["said"].as_str().unwrap_or_default())), ago_words(now.saturating_sub(s["updated_ms"].as_u64().unwrap_or(0))), esc(name)));
    }
    if rows.is_empty() { rows = r#"<div class="limit"><div class="limit-text"><strong>None kept</strong><small>A run from a computer gets the secrets its workflow names. One not kept is empty, as on GitHub.</small></div></div>"#.into() }
    let close = r#"<button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>"#;
    let dialog = format!(r#"<dialog class="dlg" id="add-secret" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_LOCK}</span><span><strong>Add a secret</strong><small>Kept in your control plane's cloud, never shown again.</small></span>{close}</div><form method="post" action="/secrets/add" class="secret-form"><label>Name<input name="name" required pattern="[A-Za-z_][A-Za-z0-9_]*" title="Letters, digits and _" spellcheck="false" autocomplete="off" autocapitalize="characters" placeholder="NPM_TOKEN"></label><label>Value<textarea name="value" required rows="3" spellcheck="false" autocomplete="off" maxlength="4096"></textarea></label><details><summary>Where it may be used</summary><label>Repository<input name="repo" placeholder="acme/web" spellcheck="false" autocomplete="off"></label><label>Branch<input name="branch" placeholder="main, release/*" spellcheck="false" autocomplete="off"></label><label>Workflow<input name="workflow" placeholder="deploy.yml" spellcheck="false" autocomplete="off"></label><label>Environment<input name="environment" placeholder="production" spellcheck="false" autocomplete="off"></label></details><button class="button primary sm">Add</button></form></div></dialog>"#);
    format!(r#"<section class="card"><div class="card-head"><h2>Secrets</h2><span class="card-sub">For runs from a computer. Jobs GitHub sends get GitHub's own.</span><button type="button" class="button secondary sm" onclick="document.getElementById('add-secret').showModal()">Add</button></div><div class="limits">{rows}</div></section>{dialog}"#)
}

/// A sentence's first letter in capitals.
fn capitalized(s: &str) -> String { let mut c = s.chars(); c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default() }

/// Every control plane's view, read in parallel (with this session's key where it was accepted).
fn read_views(planes: &[(Plane, bool)], key: &str) -> Vec<PlaneView> {
    std::thread::scope(|s| {
        let handles: Vec<_> = planes.iter().map(|(p, keyed)| s.spawn(move || view::plane_view(p, keyed.then_some(key)))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// The pause between a move's looks at the new control plane (two seconds). In a debug build, `SUPERCI_MOVE_PAUSE_MS`
/// sets it: a test of what a move says after its last look does not wait a minute or two for it.
fn move_pause() {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("SUPERCI_MOVE_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) { return std::thread::sleep(Duration::from_millis(ms)) }
    std::thread::sleep(Duration::from_secs(2))
}

/// A move (see `/plane/move`): returns what stayed behind, in words.
fn move_plane(w: &Writer, key: &str, from: &Plane, to: &Plane, old: serde_json::Value, plan: &[Carry], cf_account: Option<String>, aws_account: Option<String>, step: &dyn Fn(usize)) -> Result<Vec<String>> {
    // 1. Settings and history, copied: the new one is on standby (not in use) until the switch.
    step(0);
    let (out_token, in_token) = (random_token(24), random_token(24));
    w.put(from, "MOVE_TOKEN", &out_token)?;
    w.put(to, "MOVE_TOKEN", &in_token)?;
    // Each takes its token once it has it (a few seconds after the cloud's API).
    let call = |plane: &Plane, token: &str, path: &str, body: serde_json::Value| -> Result<serde_json::Value> {
        tries(45, 2000, || match cloudflare::plane_move_call(plane.url(), key, token, path, &body) {
            Ok((200, v)) => Some(Ok(v)),
            Ok((403 | 404 | 500..=599, _)) => None,
            Ok((s, v)) => Some(Err(format!("{} answered {s}: {v}", plane.url()))),
            Err(e) => Some(Err(e)),
        }).unwrap_or_else(|| Err(format!("{} did not take its move token", plane.url())))
    };
    let out = call(from, &out_token, "/move/export", serde_json::json!({}))?;
    [("GITHUB_APP", "app"), ("GITLAB", "gitlab"), ("ROUTING", "routing"), ("MACHINE", "machine")].into_iter().filter(|(_, k)| !out[k].is_null()).try_for_each(|(name, k)| {
        // (A budget in AWS itself is for the old control plane's role: not carried. The new one says there is none,
        // until it is made again; the old one's goes when the old control plane is deleted.)
        let mut carried = out[k].clone();
        if k == "routing" { if let Some(routing) = carried.as_object_mut() { routing.remove("aws_budget"); } }
        w.put(to, name, &carried.to_string())
    })?;
    // Further organizations' Apps, each a secret of its own. One the new control plane has from an earlier move and
    // the old one no longer has (its organization was removed since) goes.
    let coming: Vec<u64> = out["more_apps"].as_array().into_iter().flatten().filter_map(|a| a["id"].as_u64()).collect();
    for app in out["more_apps"].as_array().into_iter().flatten() {
        if let Some(id) = app["id"].as_u64() { w.put(to, &format!("GITHUB_APP_{id}"), &app.to_string())? }
    }
    for stale in status_apps(&cloudflare::status(to.url(), key).unwrap_or_default()).iter().skip(1).filter_map(|a| a["id"].as_u64()).filter(|id| !coming.contains(id) && out["app"]["id"].as_u64() != Some(*id)) {
        w.drop(to, &format!("GITHUB_APP_{stale}"))?;
    }
    // Further GitLab connections, each a secret of its own; one the new control plane has from before and the old one
    // no longer has goes.
    let gitlabs: Vec<String> = out["more_gitlabs"].as_array().into_iter().flatten().filter_map(|g| g["id"].as_str().map(str::to_string)).collect();
    for g in out["more_gitlabs"].as_array().into_iter().flatten() {
        if let Some(id) = g["id"].as_str().filter(|id| superci_core::gitlab::valid_id(id)) { w.put(to, &format!("GITLAB_{id}"), &g.to_string())? }
    }
    for stale in cloudflare::status(to.url(), key).unwrap_or_default()["gitlabs"].as_array().into_iter().flatten().filter_map(|g| g["id"].as_str()).filter(|id| !id.is_empty() && !gitlabs.iter().any(|g| g == id)) {
        w.drop(to, &format!("GITLAB_{}", safe_id(stale)))?;
    }
    // The secrets it keeps for runs from a computer, each a setting of its own; one the new control plane has from
    // before and the old one does not goes.
    let secrets: Vec<(String, String)> = serde_json::from_value(out["secrets"].clone()).unwrap_or_default();
    for (name, value) in &secrets { if name.starts_with("SECRET_") { w.put(to, name, value)? } }
    let there = cloudflare::plane_get(to.url(), key, "/secrets").unwrap_or_default();
    for stale in there["secrets"].as_array().into_iter().flatten().filter_map(|s| s["setting"].as_str()).filter(|n| !secrets.iter().any(|(k, _)| k == n)) {
        w.drop(to, stale)?;
    }
    if old["aws_regions"].as_array().is_some_and(|r| !r.is_empty()) { w.put(to, "AWS_REGIONS", &old["aws_regions"].to_string())? }
    // Networks of the account's own for AWS's machines, and GitHub's full image for Cloudflare's containers.
    if old["aws_networks"].as_object().is_some_and(|n| !n.is_empty()) { w.put(to, "AWS_NETWORKS", &old["aws_networks"].to_string())? }
    if let Some(i) = old["cloudflare_image"].as_str() { w.put(to, "CF_IMAGE", i)? }
    // The labels workflows name, when set to other than the one every control plane starts with.
    if old["labels"].as_array().is_some_and(|l| l.len() > 1 || l.first().is_some_and(|f| f != "superci")) { w.put(to, "LABELS", &old["labels"].to_string())? }
    if let Some(n) = old["name"].as_str().filter(|n| *n != "SuperCI") { w.put(to, "NAME", n)? }
    // Where Cloudflare's containers start, when set to other than the default.
    if let Some(l) = old["cloudflare_location"].as_str().filter(|l| *l != "enam") { w.put(to, "CF_LOCATION", l)? }
    call(to, &in_token, "/move/import", serde_json::json!({ "state": out["state"] }))?;
    let (has_app, has_gitlab) = (!out["app"].is_null(), !out["gitlab"].is_null());
    // It has taken them all: the first App, each further one, GitLab.
    let all_apps = |s: &serde_json::Value| { let have: Vec<u64> = status_apps(s).iter().filter_map(|a| a["id"].as_u64()).collect(); coming.iter().all(|id| have.contains(id)) };
    tries(30, 0, || {
        let taken = cloudflare::status(to.url(), key).is_some_and(|s| (!has_app || !s["app"].is_null()) && all_apps(&s) && (!has_gitlab || !s["gitlab"].is_null()));
        if !taken { move_pause() }
        taken.then_some(())
    });

    // 2. The runner providers: the same clouds, started by the new one (the old one keeps its own).
    step(1);
    let mut launchers: Vec<superci_core::plane::Launcher> = cloudflare::status(to.url(), key).and_then(|s| serde_json::from_value(s["launchers"].clone()).ok()).unwrap_or_default();
    let launchers_before = launchers.len();
    let mut left = vec![];
    // (Each provider that comes is one signed in to: see `carry_plan`, and the writer made with those sign-ins.)
    let comes = |p: &'static str| plan.contains(&Carry::Comes(p));
    if let (true, Some(cf)) = (comes("cloudflare"), &w.cf) {
        match to {
            Plane::Cloudflare { account_id, script, label, .. } => {
                cf.deploy(account_id, script, label, true)?;
                w.put(to, "CONTAINERS", "on")?;
            }
            _ => {
                let url = cf.deploy_runners(cf_account.as_deref().ok_or("no Cloudflare account")?, to.url(), to.plane_id())?;
                launchers.retain(|a| a.cloud != "cloudflare");
                launchers.push(superci_core::plane::Launcher { cloud: "cloudflare".into(), url });
            }
        }
    }
    if let (true, Some(session)) = (comes("modal"), &w.modal) {
        match to {
            Plane::Modal { .. } => w.put(to, "CONTAINERS", "on")?,
            _ => {
                let url = modal::deploy_runners(session, to.url(), to.plane_id(), &plane_keys(to.url())?)?;
                launcher_up(&url);
                launchers.retain(|a| a.cloud != "modal");
                launchers.push(superci_core::plane::Launcher { cloud: "modal".into(), url });
            }
        }
    }
    // AWS: a Cloudflare control plane assumes its own role in the account (one in AWS uses its own).
    if let (true, Plane::Cloudflare { .. }, Some(creds), Some(account)) = (comes("aws"), to, &w.aws, aws_account.clone()) {
        let region = old["aws"]["region"].as_str().unwrap_or("us-east-1").to_string();
        let role_arn = aws::connect_runners(creds, &account, to.url(), to.plane_id(), AUDIENCE)?;
        aws::make_networks(creds, to.plane_id(), &status_regions(&old))?;
        let token = random_token(24);
        w.put(to, "AWS_CONNECT", &serde_json::json!({ "region": region, "token": token }).to_string())?;
        let report = serde_json::json!({ "token": token, "accountId": account, "region": region, "roleArn": role_arn });
        let mut last = String::new();
        if tries(12, 3000, || report_to_plane(to.url(), &report).map_err(|e| last = e).ok()).is_none() { return Err(format!("AWS runners: {last}")) }
    }
    for c in plan { if let Carry::Cannot(p, why) = c { left.push(format!("{} runners stayed behind: {why}.", cloud_name(p))) } }
    if launchers.len() != launchers_before || plan.iter().any(|c| matches!(c, Carry::Comes("cloudflare") | Carry::Comes("modal"))) {
        w.put(to, "LAUNCHERS", &json_text(&launchers))?;
    }
    // The new one says each provider it was given is there, before any job is sent to it: a control plane given its
    // containers a moment ago starts again with them, and until it has, a job for them would find none.
    let brought: Vec<&str> = plan.iter().filter_map(|c| match c { Carry::Comes(cloud) => Some(*cloud), _ => None }).collect();
    let ready = brought.is_empty() || tries(60, 0, || { move_pause(); cloudflare::status_light(to.url(), key).filter(|s| brought.iter().all(|c| provider_connected(s, c))) }).is_some();
    if !ready { left.push("The new control plane had not started with every runner provider yet: a job in the next minutes may find one missing.".into()) }

    // 3. The switch: the old one passes on what still reaches it, then the new one takes GitHub and GitLab over.
    step(2);
    cloudflare::plane_post(from.url(), key, "/move/away", &serde_json::json!({ "to": to.url() }))?;
    let claimed = tries(30, 0, || {
        let c = cloudflare::plane_post(to.url(), key, "/move/claim", &serde_json::json!({ "from": from.url() })).ok().filter(|c| !has_app || c["github"] == true);
        if c.is_none() { move_pause() }
        c
    });
    let Some(c) = claimed else { return Err("The new control plane has not taken GitHub over yet. Jobs still reach it through the old one, which passes them on; Try again finishes the switch.".into()) };
    // An organization whose App would not move (deleted on GitHub, say): its jobs still arrive through the old
    // control plane, which passes them on.
    for f in c["github_failed"].as_array().into_iter().flatten().filter_map(|f| f.as_str()) { left.push(format!("GitHub would not point an App here ({f}); its jobs still come through the old control plane.")) }

    // 4. It answers, in use.
    step(3);
    wait_online(to.url());
    Ok(left)
}

/// Back to the dashboard after a sign-in elsewhere: a same-site navigation, so the page's cookie goes along
/// (a plain redirect at the end of another site's redirect chain would count as cross-site and lose it).
fn back_to(heading: &str, text: &str, url: &str) -> Response {
    document(200, "SuperCI", &format!(r#"<meta http-equiv="refresh" content="0;url={}"><div class="wait"><div>{MARK}<h1>{}</h1><p>{}</p><div class="spinner"></div></div></div>"#, esc(url), esc(heading), esc(text)), None)
}

/// Tells the control plane which role to use; a new role can take a few seconds
/// before AWS lets the control plane assume it.
fn report_to_plane(plane_url: &str, report: &serde_json::Value) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut last = String::new();
    let answered = tries(12, 5000, || {
        let mut r = match agent.post(&crate::aws::stand_in(&format!("{plane_url}/aws/callback"))).header("content-type", "application/json").send(report.to_string()) { Ok(r) => r, Err(e) => return Some(Err(e.to_string())) };
        let text = r.body_mut().read_to_string().unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if r.status().as_u16() == 200 && v["connected"] == true { return Some(Ok(())) }
        if r.status().as_u16() != 200 { return Some(Err(format!("the control plane refused the connection: {text}"))) }
        last = v["error"].as_str().unwrap_or("not connected").to_string();
        None
    });
    answered.unwrap_or_else(|| Err(format!("the role exists but the control plane could not use it yet: {last}")))
}

/// A new or updated control plane takes a few seconds to answer.
/// Until the control plane answers as `version` (up to a minute).
fn wait_for_version(url: &str, version: &str) { tries(60, 2000, || cloudflare::health(url).filter(|h| h["version"] == version)); }

fn wait_online(url: &str) { tries(30, 2000, || cloudflare::health(url)); }

/// The keys a control plane signs its launchers' requests with (its JWKS), for a launcher to trust.
fn plane_keys(url: &str) -> Result<serde_json::Value> {
    ureq::get(&crate::aws::stand_in(&format!("{url}/.well-known/jwks.json"))).call().map_err(|e| e.to_string())?.body_mut().read_json().map_err(|e| e.to_string())
}

/// Until a launcher just deployed answers (its first answer can take a few seconds: a cold start).
fn launcher_up(url: &str) { tries(20, 3000, || ureq::get(&crate::aws::stand_in(&format!("{url}/health"))).call().ok().filter(|r| r.status() == 200)); }

/// The manifest flow's last step, from this machine: the code becomes the App's id, key and webhook secret.
/// The manifest flow's last step, from this machine: GitHub's code becomes the App's id, key and webhook secret.
/// `api`: that GitHub's API (see `github::api_base`).
fn convert_manifest_code(api: &str, host: Option<&str>, code: &str) -> Result<github::App> {
    if code.len() < 8 || code.len() > 100 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("invalid manifest code".into()) }
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent.post(&crate::aws::stand_in(&format!("{api}/app-manifests/{code}/conversions")))
        .header("accept", "application/vnd.github+json").header("user-agent", "superci-cli").send_empty().map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if r.status().as_u16() >= 300 { return Err(format!("GitHub: {} {}", r.status(), text.chars().take(200).collect::<String>())) }
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let s = |k: &str| v[k].as_str().map(str::to_string).ok_or_else(|| format!("App conversion without {k}"));
    Ok(github::App { id: v["id"].as_u64().ok_or("App conversion without id")?, slug: s("slug")?, pem: s("pem")?, webhook_secret: s("webhook_secret")?,
        owner: v["owner"]["login"].as_str().unwrap_or_default().to_string(), owner_is_org: v["owner"]["type"] == "Organization", host: host.map(str::to_string) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_cloudflare(accounts: &[(&str, &str)]) -> Dashboard {
        let mut d = Dashboard::new();
        d.cf = Some(cloudflare::Session::from_token("test"));
        d.cf_accounts = accounts.iter().map(|(id, n)| (id.to_string(), n.to_string())).collect();
        d
    }

    /// One dashboard as the only workspace's, with no folder (as on a machine that keeps nothing).
    fn hub_of(d: Dashboard, key: &str) -> Hub {
        let mut d = d;
        d.key = key.to_string();
        Hub { root: None, key: key.to_string(), base: d.base.clone(), first: Mutex::new(Some("default".into())), boards: Mutex::new(HashMap::from([("default".to_string(), Arc::new(Mutex::new(d)))])), opened: Mutex::new(None) }
    }

    /// Two companies on one computer: each workspace has its own sign-ins and its own pages, a page's address says
    /// which it is of, and nothing that changes something picks one by itself.
    #[test]
    fn a_redirect_within_the_dashboard_carries_its_workspace() {
        let to = |at: &str| carry(Response::redirect(at), "acme").headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).unwrap_or_default();
        assert_eq!(to("/?p=runners"), "/?p=runners&w=acme");
        assert_eq!(to("/"), "/?w=acme");
        assert_eq!(to("/connect/cloudflare"), "/connect/cloudflare?w=acme");
        assert_eq!(to("/?p=planes#aws-account"), "/?p=planes&w=acme#aws-account");
        assert_eq!(to("/?p=runners&w=globex"), "/?p=runners&w=globex");
        assert_eq!(to("https://github.com/settings"), "https://github.com/settings");
    }

    #[test]
    fn workspaces_are_worlds_of_their_own() {
        let root = std::env::temp_dir().join(format!("superci-hub-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        Store::under(&root, "acme").write(&Kept { modal: Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme-team".into() }), ..Default::default() }).unwrap();
        // (Its organization's picture as GitHub gave it once: shown as it is, with nothing asked of any site.)
        Store::under(&root, "acme").set_about(&crate::store::About { name: "AcmeCI".into(), hosts: vec![("github".into(), "acme-inc".into())], picture: "data:image/png;base64,iVBORw0KGgo=".into(), picture_of: "acme-inc".into() }).unwrap();
        Store::under(&root, "globex").make().unwrap();
        let key = "k".repeat(32);
        let hub = Hub { root: Some(root.clone()), key: key.clone(), base: "http://localhost:8976".into(), first: Mutex::new(Some("acme".into())), boards: Mutex::new(HashMap::new()), opened: Mutex::new(None) };
        let ask = |method: &str, path: &str, query: &[(&str, &str)], body: &str| {
            let r = respond(&hub, &Req { method: method.into(), path: path.into(), query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), cookie: Some(key.clone()), last: None, body: body.as_bytes().to_vec(),
                host: "localhost:8976".into(), origin: None, fetch_site: Some("same-origin".into()) });
            (r.status, r.headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).unwrap_or_default(), String::from_utf8_lossy(&r.body).to_string())
        };
        // A page that names none is sent on to the one the program was started in, by name.
        assert_eq!(ask("GET", "/", &[], "").1, "/?w=acme");
        assert_eq!(ask("GET", "/", &[("p", "settings")], "").1, "/?p=settings&w=acme");
        // `superci login aws --workspace=acme`: its one button says which workspace it is for, and goes on to AWS.
        lock(&hub.board("acme")).task = Some(Task::Login(Some("aws")));
        let (_, _, page) = ask("GET", "/", &[("w", "acme")], "");
        assert!(page.contains(r#"<form method="post" action="/aws/signin"><input type="hidden" name="w" value="acme">"#), "{page}");
        let (status, to, said) = ask("POST", "/aws/signin", &[], "w=acme&next=");
        assert!(status == 303 && to.contains("signin.aws.amazon.com"), "{status} {to} {said}");
        // `superci login cloudflare --workspace=acme` (and modal): its tab goes on to that cloud's page, in acme.
        lock(&hub.board("acme")).task = Some(Task::Login(Some("cloudflare")));
        let (status, to, _) = ask("GET", "/", &[("w", "acme")], "");
        assert!((300..400).contains(&status) && to.starts_with("/connect/cloudflare") && to.contains("w=acme"), "{status} {to}");
        let (status, _, said) = ask("GET", "/connect/cloudflare", &[("w", "acme")], "");
        assert!(!said.contains("Which workspace?"), "{status} {said}");
        lock(&hub.board("acme")).task = None;
        // Each has its own dashboard, from its own folder: nothing of acme is in globex.
        assert_eq!(lock(&hub.board("acme")).signed_in_as().as_deref(), Some("Modal workspace acme-team"));
        assert!(lock(&hub.board("globex")).signed_in_as().is_none() && lock(&hub.board("globex")).named == "globex");
        // Globex's page is the first screen of a new SuperCI, with the switcher's rows: both workspaces, this one
        // marked, acme by its name with its organization's picture, and making a new one.
        let (status, _, page) = ask("GET", "/", &[("w", "globex"), ("fragment", "1")], "");
        assert!(status == 200 && page.contains("data-gated") && page.contains("Where is your control plane?"), "{page}");
        assert!(page.contains(r#"<a class="space" href="/?w=acme&c=" aria-current="false"><img class="logo" src="data:image/png;base64,iVBORw0KGgo=" width="28" height="28" alt=""><span>AcmeCI</span><small>acme-inc</small></a>"#), "{page}");
        assert!(page.contains(r#"<a class="space" href="/?w=globex&c=" aria-current="true"><span class="logo initial""#) && page.contains(">G</span><span>globex</span><small>Nothing set up yet</small></a>"), "{page}");
        assert!(page.contains("<span>New workspace…</span>") && !page.contains("acme-team") && !page.contains("Add an organization"));
        // Its frame has the dialog that makes one.
        assert!(ask("GET", "/", &[("w", "globex")], "").2.contains(r#"id="new-workspace" aria-label="New workspace" onclick="if(event.target===this)this.close()" onclose="this.querySelector('form').reset()">"#));
        // What changes something says which workspace it is for, or stops: nothing picks one.
        let (status, _, said) = ask("POST", "/routing", &[], "action=max_cpu&max_cpu=8");
        assert!(status == 409 && said.contains("Which workspace?") && said.contains("Nothing has changed."), "{status} {said}");
        let (status, _, said) = ask("GET", "/connect/aws", &[], "");
        assert!(status == 409 && said.contains("Which workspace?"), "a sign-in is started in a workspace: {status}");
        // One that is not kept here is none: a page goes to the list, a change stops.
        assert_eq!(ask("GET", "/", &[("w", "nope")], "").1, "/workspaces");
        assert!(ask("GET", "/", &[("w", "nope"), ("fragment", "1")], "").2.contains("That workspace is not kept on this computer"));
        assert_eq!(ask("POST", "/routing", &[("w", "nope")], "action=max_cpu&max_cpu=8").0, 409);
        // A sign-in that comes back from elsewhere names none: it goes to the workspace that started it, and from
        // there back to that workspace's pages.
        lock(&hub.board("globex")).github_expected = Some(now_ms() + 60_000);
        let (_, _, back) = ask("GET", "/github/installed", &[], "");
        assert!(back.contains("url=http://localhost:8976/?w=globex"), "{back}");
        lock(&hub.board("globex")).github_expected = None;
        // The list of them.
        let (_, _, list) = ask("GET", "/workspaces", &[], "");
        assert!(list.contains("<h1>Workspaces</h1>") && list.contains(r#"<a class="pick" href="/?w=acme&c=">"#) && list.contains(r#"<a class="pick" href="/?w=globex&c=">"#) && list.contains(">New workspace</button>"), "{list}");
        // A new one: a name, and nothing else; its id is made from the name. It opens on its own pages, with no
        // control plane named, and is called its name there before anything is set up.
        assert_eq!(ask("POST", "/workspaces", &[("w", "acme")], "name=Initech+CI").1, "/?w=initech-ci&c=");
        assert_eq!(Store::under(&root, "initech-ci").about().name, "Initech CI");
        let (_, _, page) = ask("GET", "/", &[("w", "initech-ci"), ("fragment", "1")], "");
        assert!(page.contains(r#"<div data-called="Initech CI" hidden></div>"#) && page.contains(r#"aria-current="true"><span class="logo initial" style="width:28px;height:28px;font-size:12px">I</span><span>Initech CI</span><small>Nothing set up yet</small></a>"#), "{page}");
        assert!(String::from_utf8_lossy(&ask("GET", "/", &[("w", "initech-ci")], "").2.as_bytes()[..400]).contains("<title>Initech CI</title>"), "its frame is called it at once");
        assert_eq!(ask("POST", "/workspaces/rename", &[("w", "initech-ci")], "id=initech").1, "/?p=settings&w=initech");
        assert!(lock(&hub.board("initech-ci")).render("overview", &[], None).contains(r#"href="/?w=acme&c=" aria-current="false""#), "the others are in its switcher");
        assert_eq!(crate::store::workspaces_in(&root), ["acme", "globex", "initech"]);
        let (status, _, said) = ask("POST", "/workspaces", &[], "name=Initech");
        assert!(status == 400 && said.contains("There is a workspace with the id initech already. Give this one another name."));
        assert!(ask("POST", "/workspaces", &[], "name=9").2.contains("A name is 2 to 24 letters"));
        // A new id for one: its folder is renamed and its pages go on under it, called what they were.
        assert_eq!(ask("POST", "/workspaces/rename", &[("w", "initech")], "id=umbrella").1, "/?p=settings&w=umbrella");
        assert_eq!(crate::store::workspaces_in(&root), ["acme", "globex", "umbrella"]);
        assert_eq!(Store::under(&root, "umbrella").about().name, "Initech CI");
        assert!(ask("POST", "/workspaces/rename", &[("w", "umbrella")], "id=acme").2.contains("There is a workspace with the id acme already."));
        // Settings of one among several: its name with Rename, signing out of it, and removing it from this computer.
        let settings = lock(&hub.board("umbrella")).settings_page(None);
        assert!(settings.contains(r#"<span class="lim">umbrella</span>"#) && settings.contains(r#"action="/workspaces/rename""#) && !settings.contains("Sign out") && settings.contains(r#"action="/workspaces/remove" class="limit-do"><input type="hidden" name="w" value="umbrella">"#) && settings.contains("<strong>Remove Initech CI?</strong>"), "{settings}");
        // (Settings is there in a workspace with nothing set up: that is where it is renamed or removed.)
        let (_, _, page) = ask("GET", "/", &[("w", "umbrella"), ("p", "settings"), ("fragment", "1")], "");
        assert!(page.contains(r#"<h1 class="page-title">Settings</h1>"#) && page.contains("Remove this workspace") && !page.contains("data-gated"), "{page}");
        // Forgetting one takes its name from its own page: without it nothing is forgotten.
        assert_eq!(ask("POST", "/workspaces/remove", &[], "").0, 409);
        assert_eq!(crate::store::workspaces_in(&root).len(), 3);
        assert_eq!(ask("POST", "/workspaces/remove", &[("w", "umbrella")], "").1, "/workspaces");
        assert_eq!(ask("POST", "/workspaces/remove", &[], "w=globex").1, "/?w=acme&c=");
        // The one left is as it was, and with it alone nothing needs naming again.
        assert_eq!(crate::store::workspaces_in(&root), ["acme"]);
        assert!(Store::under(&root, "acme").read().signed_in());
        let alone = lock(&hub.board("acme")).settings_page(None);
        assert!(alone.contains(r#"<span class="lim">acme</span>"#) && !alone.contains("Remove this workspace") && alone.contains("Signed in: Modal workspace acme-team. Signing out removes this workspace's sign-ins from it"));
        assert_eq!(ask("GET", "/", &[], "").0, 200);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Started with several workspaces and none named, the dashboard opens on the one it showed last (the first until
    /// then), and a change that names no workspace is still refused.
    #[test]
    fn the_dashboard_opens_on_the_workspace_shown_last() {
        let root = std::env::temp_dir().join(format!("superci-hub-last-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        for name in ["acme", "globex"] { Store::under(&root, name).make().unwrap() }
        let key = "k".repeat(32);
        let hub = Hub { root: Some(root.clone()), key: key.clone(), base: "http://localhost:8976".into(), first: Mutex::new(None), boards: Mutex::new(HashMap::new()), opened: Mutex::new(None) };
        let ask = |method: &str, path: &str, query: &[(&str, &str)]| {
            let r = respond(&hub, &Req { method: method.into(), path: path.into(), query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), cookie: Some(key.clone()), last: None, body: vec![], host: "localhost:8976".into(), origin: None, fetch_site: Some("same-origin".into()) });
            (r.status, r.headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).unwrap_or_default())
        };
        // Nothing shown yet: the first. The page asked for goes along.
        assert_eq!(ask("GET", "/", &[]).1, "/?w=acme");
        assert_eq!(ask("GET", "/", &[("p", "jobs")]).1, "/?p=jobs&w=acme");
        // Globex's page is opened: from then on a page that names none opens there, also after a restart (the note is in the folder).
        assert_eq!(ask("GET", "/", &[("w", "globex")]).0, 200);
        assert_eq!(ask("GET", "/", &[]).1, "/?w=globex");
        // So does going to one through the switcher (the page's part alone is fetched); a page refreshing itself notes nothing.
        ask("GET", "/", &[("w", "acme"), ("fragment", "1")]);
        assert_eq!(ask("GET", "/", &[]).1, "/?w=globex");
        ask("GET", "/", &[("w", "acme"), ("fragment", "1"), ("went", "1")]);
        assert_eq!(ask("GET", "/", &[]).1, "/?w=acme");
        ask("GET", "/", &[("w", "globex"), ("fragment", "1"), ("went", "1")]);
        assert_eq!(crate::store::last_opened(&root).as_deref(), Some("globex"));
        // A change that names no workspace is refused as before: the note opens pages, it picks nothing to act on.
        assert_eq!(ask("POST", "/routing", &[]).0, 409);
        // The one shown last is gone: the first again.
        Store::under(&root, "globex").forget().unwrap();
        Store::under(&root, "initech").make().unwrap();
        assert_eq!(crate::store::last_opened(&root), None);
        assert_eq!(ask("GET", "/", &[]).1, "/?w=acme");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The first workspace has no folder until it keeps something: making a second gives it one, so both are listed.
    #[test]
    fn the_first_workspace_stays_listed_when_a_second_is_made() {
        let root = std::env::temp_dir().join(format!("superci-hub-first-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let hub = Hub { root: Some(root.clone()), key: "k".repeat(32), base: "http://localhost:8976".into(), first: Mutex::new(Some("default".into())), boards: Mutex::new(HashMap::new()), opened: Mutex::new(None) };
        assert_eq!(hub.names(), ["default"]);
        assert_eq!(hub.create("Acme").unwrap(), "acme");
        assert_eq!(crate::store::workspaces_in(&root), ["acme", "default"]);
        let menu = lock(&hub.board("acme")).render("overview", &[], None);
        assert!(menu.contains(r#"href="/?w=default&c=" aria-current="false""#) && menu.contains(r#"href="/?w=acme&c=" aria-current="true""#), "{menu}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What a real account answered (a recording, see `aws::replay`), as the look the dashboard keeps of it.
    fn looked(recording: &'static str) -> aws::Look {
        let answers = aws::replay::Replay::of(recording);
        aws::look_with(&|| answers.clone(), &superci_core::aws::Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, expires_at_ms: 0 })
    }

    /// An AWS account that does not allow everything (here a project of AWS's new sign-up, as one really answered):
    /// the set-up page says what it allows and whose rule that is, offers no region it cannot use, and a deploy to
    /// one is refused in plain words before anything is made.
    #[test]
    fn a_job_sent_on_demand_by_the_spot_limit_says_when_machines_still_ending_counted() {
        let j = |ending: bool| serde_json::json!({ "cloud": "aws", "on_demand": true, "spot_refused": "us-east-1: MaxSpotInstanceCountExceeded: Max spot instance count exceeded", "spot_ending": ending });
        assert_eq!(off_spot(&j(true)).unwrap().0, "On-demand: your AWS spot limit in N. Virginia was reached (machines AWS was still ending count too)");
        assert_eq!(off_spot(&j(false)).unwrap().0, "On-demand: your AWS spot limit in N. Virginia was reached");
    }

    #[test]
    fn a_restricted_aws_account_is_said_and_only_what_it_allows_is_offered() {
        let mut d = Dashboard::new();
        d.aws = Some(aws::Session::for_test("111111111111"));
        // Not looked at yet: nothing is said, every region is offered.
        assert!(d.aws_account_section().is_empty() && d.plane_page().contains(r#"<option value="eu-north-1">"#));
        // The project: Stockholm only, no spot, and the image jobs run on is not there.
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-paid/look.jsonl"))));
        let page = d.plane_page();
        preview("plane-aws-project", &page);
        // Its section says what it is and what it allows, each with whose rule it is, and where AWS raises its limit.
        assert!(!page.contains(r#"class="notice"#) && page.contains(r#"<h2 class="list-title" id="aws-account">AWS account</h2>"#), "{page}");
        for said in [r#"<strong>111111111111</strong><small>A project of AWS&#39;s new sign-up: AWS keeps it to Stockholm and allows it no spot machines, so jobs run on on-demand machines, at about twice the price.</small></span><span class="acct-do"><span class="pill open"><i></i>AWS project</span><form method="post" action="/aws/look">"#,
            "<dt>Regions</dt><dd>Stockholm<small>The other 11 are refused. By its AWS organization (policy p-example1).</small></dd>",
            "<dt>Spot machines</dt><dd>Not allowed<small>By its AWS organization (policy p-example2). Jobs run on on-demand machines, at about twice the price.</small></dd>",
            "<dt>On-demand machines</dt><dd>Allowed</dd>",
            r#"<dt>CPUs at once</dt><dd>5 on-demand · 5 spot<small>One job of the default size at a time, in Stockholm.</small></dd><a class="button secondary sm" href="https://eu-north-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-1216C47A" target="_blank" rel="noopener">Ask AWS for more ↗</a>"#,
            "<dt>Runner image</dt><dd>Not in Stockholm<small>No job can start there until it is copied in: about ten minutes, and about $1.50 a month for the copy&#39;s storage in your account.</small></dd>",
            "<dt>Organization</dt><dd>o-example001<small>Managed from account 222222222222: its policies are not this account&#39;s to read or change.</small></dd>"] {
            assert!(page.contains(said), "{said}\n\n{page}");
        }
        // (The row says the account alone: there is no region to name until the image is there.)
        assert!(page.contains("A Lambda function with a table · account 111111111111</small>") && !page.contains("N. Virginia"), "{page}");
        // The one thing to press is copying the image in; setting up comes after.
        assert!(page.contains(r#"<form method="post" action="/aws/image" class="aws-add"><input type="hidden" name="account" value="111111111111"><input type="hidden" name="region" value="eu-north-1"><span class="note">Needed before Set up: about ten minutes</span><button class="button primary sm">Copy the image into Stockholm</button></form>"#) && !page.contains(r#"action="/plane/aws""#), "{page}");
        // While AWS copies, that is said, and the page looks again by itself.
        d.aws_copying.insert("111111111111".into(), vec!["eu-north-1".into()]);
        let copying = d.plane_page();
        assert!(copying.starts_with(r#"<div data-refresh="10"></div>"#) && copying.contains(r#"<span class="waiting-for"><span class="mini-spin"></span>Copying the image into Stockholm: about ten minutes</span>"#) && !copying.contains(r#"action="/aws/image""#));
        d.aws_copying.clear();
        // A deploy asked for anyway (a command, an old page) is refused, with what the account can use.
        // (What a page that says one thing said: its heading and its text.)
        let post = |d: &mut Dashboard, region: &str| { let r = d.handle(&Req { method: "POST".into(), path: "/plane/aws".into(), query: vec![], cookie: None, last: None, body: format!("region={region}").into_bytes(), host: String::new(), origin: None, fetch_site: None }).unwrap();
            (r.status, SAID.with(|s| s.borrow().clone()).map(|(_, heading, text)| format!("{heading} | {text}")).unwrap_or_default()) };
        let (status, said) = post(&mut d, "us-east-1");
        assert!(status == 409 && said.contains("AWS account 111111111111 cannot use N. Virginia") && said.contains("The AWS organization the account is in does not allow it there. It can use: Stockholm (eu-north-1)."), "{status} {said}");
        let (status, said) = post(&mut d, "eu-north-1");
        assert!(status == 409 && said.contains("No job could start in Stockholm") && said.contains("The image SuperCI's jobs run on is not in eu-north-1 yet"), "{status} {said}");
        assert!(lock(&d.deploying).is_none(), "nothing was started");
        // The same project once its owner activated AWS's advanced features: spot is allowed. It is said as it is,
        // with nothing to change: SuperCI works where the account may.
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-activated/look.jsonl"))));
        let page = d.plane_page();
        preview("plane-aws-project-activated", &page);
        for said in [r#"<small>A project of AWS&#39;s new sign-up: it works in Stockholm only. Everything SuperCI does works there.</small></span><span class="acct-do"><span class="pill good"><i></i>AWS project</span><form method="post" action="/aws/look">"#,
            r#"<dt>Regions</dt><dd>Stockholm<small>The other 11 are refused. By its AWS organization (policy p-example1).</small></dd><a class="button secondary sm" href="https://superci.dev/docs/aws-accounts" target="_blank" rel="noopener">How to allow more ↗</a></div>"#,
            "<dt>Spot machines</dt><dd>Allowed</dd>", "<dt>Runner image</dt><dd>In Stockholm</dd>"] {
            assert!(page.contains(said), "{said}\n\n{page}");
        }
        assert!(!page.contains("management account"), "{page}");
        // Both kinds allowed: more of either is asked from one button's menu.
        let ask = page.split("<dt>CPUs at once</dt>").nth(1).and_then(|r| r.split("</details>").next()).unwrap_or_default();
        assert!(ask.contains(r#"<summary class="button secondary sm">Ask AWS for more"#)
            && ask.contains(r#"href="https://eu-north-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-34B43A08" target="_blank" rel="noopener">Spot CPUs ↗<small>"#)
            && ask.contains(r#"quotas/L-1216C47A" target="_blank" rel="noopener">On-demand CPUs ↗<small>"#) && ask.contains(" now, in Stockholm</small>"), "{ask}");
        // The same project before its image is there: the headline does not say everything works beside a step
        // that has to come first.
        let mut bare = looked(include_str!("../fixtures/aws/project-activated/look.jsonl"));
        for image in bare.images.iter_mut() { image.1 = Some(false); }
        d.aws_looks.insert("111111111111".into(), (now_ms(), bare));
        let first = d.plane_page();
        preview("plane-aws-project-no-image", &first);
        assert!(first.contains("it works in Stockholm only. The image jobs run on is not there yet: copying it in comes first.</small>") && first.contains(r#"<span class="pill open"><i></i>AWS project</span>"#) && !first.contains("Everything SuperCI does works there"), "{first}");
        // Signed in to the organization's management account itself: said, with signing in to another.
        let mut managing = looked(include_str!("../fixtures/aws/project-activated/look.jsonl"));
        managing.management_account = Some("111111111111".into());
        d.aws_looks.insert("111111111111".into(), (now_ms(), managing));
        let own = d.plane_page();
        preview("plane-aws-management", &own);
        assert!(own.contains("<dt>Organization</dt><dd>o-example001<small>The organization&#39;s management account. A CI is better kept in an account of its own.</small></dd>") && own.contains("Another AWS account</button>"), "{own}");
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-activated/look.jsonl"))));
        // Its copy of the image is of an older build than its maker's newest: said, with copying the new one in;
        // and while that one is on its way.
        d.aws_behind.insert("111111111111".into(), vec![("eu-north-1".into(), "20261006080949".into(), "20261020070000".into(), false)]);
        let behind = d.plane_page();
        assert!(behind.contains(r#"<dt>Runner image</dt><dd>In Stockholm<small>Your copy in Stockholm is the build of 6 October 2026. Its maker has one of 20 October 2026 out.</small></dd><span class="acct-do"><form method="post" action="/aws/image"><input type="hidden" name="account" value="111111111111"><input type="hidden" name="region" value="eu-north-1"><input type="hidden" name="newer" value="1"><button class="button secondary sm">Copy the new one in</button></form>"#), "{behind}");
        d.aws_behind.get_mut("111111111111").unwrap()[0].3 = true;
        let coming = d.plane_page();
        assert!(coming.contains("The build of 20 October 2026 is being copied into Stockholm: about ten minutes. Jobs start from your older copy until it is there.") && !coming.contains("Copy the new one in"), "{coming}");
        d.aws_behind.clear();
        // A look AWS did not answer (it was out of reach) does not take the place of the one before it: what was
        // known stands, and AWS is asked again in a moment. With none before, the section says that AWS did not
        // answer, and never "nothing refused".
        let silent = || aws::Look { regions: REGIONS.iter().map(|r| (r.to_string(), aws::Answer::Unknown("AWS did not answer in time".into()))).collect(), ..looked("") };
        d.take_look("111111111111".into(), silent(), vec![], vec![]);
        assert!(d.aws_account_section().contains("it works in Stockholm only"), "the look before stands");
        d.aws_looks.clear();
        d.take_look("111111111111".into(), silent(), vec![], vec![]);
        let unknown = d.aws_account_section();
        assert!(unknown.contains(r#"<small>AWS did not answer when asked what this account allows (AWS did not answer in time). Refresh asks again.</small></span><span class="acct-do"><span class="pill open"><i></i>Not known</span>"#) && !unknown.contains("Nothing refused"), "{unknown}");
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-activated/look.jsonl"))));
        // Kept to two regions by its organization, set up in the one that has the runner image (as that project was
        // once its owner let N. Virginia in): said as where SuperCI works, with nothing wrong. The image row says
        // where it is; the region that lacks it is no trouble, and has its copy a press away.
        let mut two = looked(include_str!("../fixtures/aws/project-activated/look.jsonl"));
        for (region, answer) in two.regions.iter_mut() { if region == "us-east-1" { *answer = aws::Answer::Allowed } }
        two.images = vec![("us-east-1".into(), Some(true)), ("eu-north-1".into(), Some(false))];
        d.aws_looks.insert("111111111111".into(), (now_ms(), two));
        let section = d.aws_account_section();
        for said in [r#"<small>SuperCI works there in N. Virginia. Its AWS organization keeps it out of the other regions.</small></span><span class="acct-do"><span class="pill good"><i></i>Some regions</span>"#,
            "<dt>Regions</dt><dd>N. Virginia, Stockholm<small>The other 10 are refused.",
            r#"<dt>Runner image</dt><dd>In N. Virginia<small>Not in Stockholm, which this account may also use. To run jobs there too, copy it in: about ten minutes, and about $1.50 a month for the copy&#39;s storage.</small></dd><span class="acct-do"><form method="post" action="/aws/image"><input type="hidden" name="account" value="111111111111"><input type="hidden" name="region" value="eu-north-1"><input type="hidden" name="next"><button class="button secondary sm">Copy into Stockholm</button></form></span>"#] {
            assert!(section.contains(said), "{said}\n\n{section}");
        }
        assert!(!section.contains("No job can start") && !section.contains("Restricted"), "{section}");
        // Asked for there, it is on its way: said, with what comes after.
        d.aws_copying.insert("111111111111".into(), vec!["eu-north-1".into()]);
        let coming_in = d.aws_account_section();
        assert!(coming_in.contains("<dt>Runner image</dt><dd>In N. Virginia<small>It is being copied into Stockholm: about ten minutes. Then it can be added as a region on Runners.</small></dd></div>") && !coming_in.contains("Copy into"), "{coming_in}");
        d.aws_copying.clear();
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-activated/look.jsonl"))));
        // Refresh asks AWS again (the look is marked old; the next page starts one beside it). While it is on its
        // way the section says so in the button's place, and the page looks again by itself until it is in.
        assert!(coming.contains(r#"<button class="button secondary sm" title="Asks AWS again what this account allows">Refresh</button>"#) && !coming.contains("Asking AWS…"), "{coming}");
        let r = d.handle(&Req { method: "POST".into(), path: "/aws/look".into(), query: vec![], cookie: None, last: None, body: b"next=%3Fp%3Dplanes".to_vec(), host: String::new(), origin: None, fetch_site: None }).unwrap();
        assert!(r.status == 303 && d.aws_looks["111111111111"].0 == 1, "the look is taken as old");
        d.aws_looking.insert("111111111111".into());
        let asking = d.aws_account_section();
        assert!(asking.starts_with(r#"<div data-refresh="2"></div>"#) && asking.contains("Asking AWS…") && !asking.contains(">Refresh<"), "{asking}");
        d.aws_looking.clear();
        // An account of its own: its section says nothing is refused, with its limits, and the regions offered are
        // the ones the image is in.
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/classic/look.jsonl"))));
        let page = d.plane_page();
        assert!(page.contains(r#"<span class="pill good"><i></i>Nothing refused</span>"#) && page.contains("<dt>Regions</dt><dd>All 12 SuperCI works in</dd>") && page.contains("<dt>Spot machines</dt><dd>Allowed</dd>")
            && page.contains("<dt>CPUs at once</dt><dd>64 on-demand · 96 spot<small>24 jobs of the default size at a time, in N. Virginia.</small></dd>") && page.contains("<dt>Runner image</dt><dd>In 10 of the 12 regions it may use<small>Not in Canada, Stockholm, which this account may also use. To run jobs there too, copy it in:") && page.contains(r#"<button class="button secondary sm">Copy into Canada</button>"#) && page.contains(r#"<button class="button secondary sm">Copy into Stockholm</button>"#), "{page}");
        assert!(page.contains(r#"<option value="us-east-1" selected>"#) && page.contains(r#"<option value="eu-west-1">"#) && !page.contains(r#"<option value="eu-north-1">"#) && !page.contains(r#"<option value="ca-central-1">"#), "{page}");
        // Where regions are chosen (a provider's regions on Runners), one the account cannot use is shown and cannot
        // be chosen, with why; the one chosen to begin with is one it can use.
        let (project, classic) = (looked(include_str!("../fixtures/aws/project-paid/look.jsonl")), looked(include_str!("../fixtures/aws/classic/look.jsonl")));
        assert_eq!((region_unusable(Some(&project), "us-east-1"), region_unusable(Some(&project), "eu-north-1"), region_unusable(None, "us-east-1")), (Some("not allowed by your AWS organization"), Some("copy the runner image in first"), None));
        let options = region_select_options("us-east-1", &HashMap::new(), "closest to GitHub", Some(&classic));
        assert!(options.contains(r#"<option value="us-east-1" selected>US East (N. Virginia) · us-east-1 · closest to GitHub</option>"#) && options.contains(r#"<option value="eu-north-1" disabled>Europe (Stockholm) · eu-north-1 · copy the runner image in first</option>"#) && options.contains(r#"<option value="eu-west-1">"#), "{options}");
        let options = region_select_options("us-east-1", &HashMap::new(), "closest to GitHub", Some(&project));
        assert!(options.contains(r#"<option value="us-east-1" disabled>US East (N. Virginia) · us-east-1 · not allowed by your AWS organization</option>"#) && !options.contains("selected"), "{options}");
        // A refusal in plain words, by whose rule it is (the first as the project answered a deploy to N. Virginia).
        let refused = "us-east-1: UnauthorizedOperation: You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/AccountFullAccessRole/someone is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy: arn:aws:organizations::222222222222:policy/o-example001/service_control_policy/p-example1";
        assert_eq!(plain_refusal(refused).unwrap(), "AWS refuses this (ec2:DescribeVpcs) in N. Virginia (us-east-1) for the whole account: the AWS organization the account is in does not allow it, and nothing inside the account lifts that. Its policy is p-example1, managed from account 222222222222. `superci accounts retrieve aws` shows what the account allows.");
        assert_eq!(plain_refusal("AccessDenied: User: arn:aws:iam::111111111111:user/ci is not authorized to perform: iam:CreateRole on resource: x because no identity-based policy allows the iam:CreateRole action").unwrap(),
            "The AWS role or user you signed in as may not do this (iam:CreateRole). Sign in with one that administers the account: `superci login aws`.");
        assert!(plain_refusal("ResourceConflictException: the function exists").is_none() && plain_refusal("UnauthorizedOperation: You are not authorized to perform this operation.").is_none());
        // A deploy that stopped on one says it so, and offers choosing another place.
        *lock(&d.deploying) = Some(Deploy { cloud: "aws", place: "account 111111111111 · us-east-1".into(), steps: &aws_plane::DEPLOY_STEPS, at: 0, form: vec![("region", "us-east-1".into())], result: Some(Err(refused.into())) });
        let stopped = d.plane_page();
        assert!(stopped.contains("AWS refuses this (ec2:DescribeVpcs) in N. Virginia (us-east-1) for the whole account") && !stopped.contains("explicit deny") && stopped.contains(r#"<form method="post" action="/plane/start-over"><button class="button secondary">Choose another place</button></form>"#), "{stopped}");
        let r = d.handle(&Req { method: "POST".into(), path: "/plane/start-over".into(), query: vec![], cookie: None, last: None, body: vec![], host: String::new(), origin: None, fetch_site: None }).unwrap();
        assert!(r.status == 303 && lock(&d.deploying).is_none() && d.plane_page().contains("Set up a control plane"));
    }

    /// Cloudflare's costs are read once at a time and at most every two minutes: asked while a reading is under way,
    /// or within two minutes of the last, nothing more is started (a page refreshing meanwhile asks each time).
    #[test]
    fn cloudflare_costs_are_not_read_again_while_a_reading_is_under_way_or_was_just_made() {
        let shared = Arc::new(Mutex::new(Dashboard::new()));
        for (measuring, measured_at) in [(true, 0), (false, now_ms())] {
            let mut d = Dashboard::new();
            (d.measuring, d.measured_at) = (measuring, measured_at);
            d.views = Some((vec![], now_ms()));
            Dashboard::measure(&shared, &mut d);
            assert_eq!((d.measuring, d.measured_at), (measuring, measured_at));
        }
    }

    /// An account whose refusals are not its organization's: spot refused by the sign-in's own permissions and a
    /// region by a rule AWS did not name are said as "Restricted", each with whose rule it is; with several accounts
    /// signed in and none named, which one is asked.
    #[test]
    fn refusals_by_the_sign_in_or_unsaid_are_said_as_restricted_and_several_accounts_need_one_named() {
        let mut d = Dashboard::new();
        d.aws = Some(aws::Session::for_test("111111111111"));
        let mut look = looked(include_str!("../fixtures/aws/project-activated/look.jsonl"));
        let refused = |by| aws::Answer::Refused(superci_core::aws::Refusal { action: "ec2:RunInstances".into(), by, policy: String::new() });
        look.spot = Some(refused(superci_core::aws::By::Identity));
        for (_, answer) in look.regions.iter_mut() { if matches!(answer, aws::Answer::Refused(_)) { *answer = refused(superci_core::aws::By::Unsaid) } }
        d.aws_looks.insert("111111111111".into(), (now_ms(), look));
        let section = d.aws_account_section();
        for said in ["<small>Some of what SuperCI does there is refused: see below.</small>", r#"<span class="pill open"><i></i>Restricted</span>"#,
            "<small>The other 11 are refused. AWS did not say by whose rule.</small></dd></div>",
            "<dt>Spot machines</dt><dd>Not allowed<small>By the permissions of the role or user signed in as. Jobs run on on-demand machines, at about twice the price.</small>"] {
            assert!(section.contains(said), "{said}\n\n{section}");
        }
        assert!(!section.contains("How to allow more"), "{section}");
        d.aws_others.push(aws::Session::for_test("222222222222"));
        assert_eq!(d.aws_account("").unwrap_err(), "Say which account: 111111111111, 222222222222.");
        assert!(d.aws_account("333333333333").unwrap_err().contains("not signed in to account 333333333333"));
    }

    /// An account its organization keeps to three regions, beside a control plane whose spot machines start in two of
    /// them in turn: each region on a line of its own, saying how jobs use it and what AWS lets run there at once; the
    /// spot CPUs together; the one without the image offered a copy on its line. A region AWS gave no clear answer
    /// for is not offered as if allowed.
    #[test]
    fn several_regions_are_each_said_with_how_jobs_use_them() {
        let mut d = Dashboard::new();
        d.aws = Some(aws::Session::for_test("111111111111"));
        let policy = |by| aws::Answer::Refused(superci_core::aws::Refusal { action: "ec2:DescribeVpcs".into(), by, policy: String::new() });
        let look = aws::Look {
            regions: REGIONS.iter().map(|r| (r.to_string(), match *r {
                "us-east-1" | "us-east-2" | "eu-north-1" => aws::Answer::Allowed,
                "eu-central-1" => aws::Answer::Unknown("RequestExpired".into()),
                _ => policy(superci_core::aws::By::Organization),
            })).collect(),
            asked_in: Some("us-east-1".into()), spot: Some(aws::Answer::Allowed), on_demand: Some(aws::Answer::Allowed), spot_cpus: Some(300), on_demand_cpus: Some(192),
            images: vec![("us-east-1".into(), Some(true)), ("us-east-2".into(), Some(true)), ("eu-north-1".into(), Some(false))], organization_id: None, management_account: None,
        };
        d.aws_looks.insert("111111111111".into(), (now_ms(), look.clone()));
        d.quotas.insert("us-east-2".into(), Ok(32));
        let plane = Plane::Aws { account_id: "111111111111".into(), region: "us-east-1".into(), url: "https://abc.lambda-url.us-east-1.on.aws".into(), plane_id: "abcdefabcdef".into(), label: "superci".into() };
        let status = serde_json::json!({ "aws": { "account_id": "111111111111", "region": "us-east-1", "connected": true }, "aws_regions": ["us-east-1", "us-east-2"],
            "routing": { "order": [{ "cloud": "aws", "regions": ["us-east-1", "us-east-2"] }, { "cloud": "aws-on-demand", "regions": ["us-east-1"] }] } });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let section = d.aws_account_section_for(Some(&view));
        preview("plane-aws-regions", &section);
        for said in [
            r#"<li><span class="ar-name">N. Virginia<small>us-east-1</small></span><span class="ar-use">Spot, favoured · on-demand</span><span class="ar-cpu">300 spot CPUs</span></li>"#,
            r#"<li><span class="ar-name">Ohio<small>us-east-2</small></span><span class="ar-use">Spot when cheaper</span><span class="ar-cpu">32 spot CPUs</span></li>"#,
            r#"<span class="ar-use">Not used: the runner image is not here</span><span class="ar-cpu"></span><form method="post" action="/aws/image">"#,
            "<small>The other 8 are refused. By its AWS organization. AWS did not say for Frankfurt: Refresh asks again. <a href=\"https://superci.dev/docs/aws-accounts\"",
            "<dt>CPUs at once</dt><dd>332 spot · 192 on-demand<small>83 jobs of the default size at a time on spot, across N. Virginia, Ohio. On-demand: in N. Virginia.</small>",
            "Spot CPUs in Ohio ↗<small>32 now</small>",
        ] { assert!(section.contains(said), "{said}\n\n{section}"); }
        assert!(!section.contains("<dt>Runner image</dt>"), "each region's line says where the image is: {section}");
        // Without a control plane beside it (setting one up), as before.
        assert!(d.aws_account_section().contains("<dt>Regions</dt><dd>N. Virginia, Ohio, Stockholm<small>"));
        // Frankfurt, which AWS answered with an error, is not offered; unless AWS answered for no region at all.
        assert_eq!(region_unusable(Some(&look), "eu-central-1"), Some("AWS did not say: Refresh under Control plane"));
        assert_eq!(region_unusable(Some(&look), "us-east-2"), None);
        let silent = aws::Look { regions: REGIONS.iter().map(|r| (r.to_string(), aws::Answer::Unknown("timed out".into()))).collect(), ..look };
        assert_eq!(region_unusable(Some(&silent), "eu-central-1"), None);
    }

    /// A job's page says its AWS machine in three plain parts: the machine, spot or on-demand (and why on-demand),
    /// and where.
    #[test]
    fn an_aws_jobs_machine_is_said_as_what_how_bought_and_where() {
        let spot = serde_json::json!({ "cloud": "aws", "machine_type": "r8a.16xlarge", "cpu": 64, "ramdisk": true, "region": "us-east-1", "zone": "us-east-1a" });
        assert_eq!(aws_machine(&spot), Some(("r8a.16xlarge · 64 CPU · RAM disk".into(), "Spot".into(), "N. Virginia · us-east-1a".into())));
        let again = serde_json::json!({ "cloud": "aws", "machine_type": "c8a.xlarge", "cpu": 4, "on_demand": true, "after_reclaim": true, "region": "us-east-2" });
        assert_eq!(aws_machine(&again), Some(("c8a.xlarge · 4 CPU".into(), "On-demand · run again after AWS took back its spot machine".into(), "Ohio · us-east-2".into())));
        // Why this one: its price, the cheaper first choice that had no machine, and what was not asked for its price.
        let why = serde_json::json!({ "cloud": "aws", "machine_type": "c8a.16xlarge", "cpu": 64, "region": "us-east-1", "zone": "us-east-1b", "priced_out": ["r7a.16xlarge"], "asks": [
            { "kind": "m7a.16xlarge", "zone": "us-east-1a", "spot": true, "said": "InsufficientInstanceCapacity", "usd": 0.7339 },
            { "kind": "c8a.16xlarge", "zone": "us-east-1b", "spot": true, "said": "started", "pools": 3, "usd": 0.9663 }] });
        assert_eq!(aws_machine(&why).unwrap().1, "Spot · $0.9663 an hour · its first choice, m7a.16xlarge in us-east-1a at $0.7339, had no machine · not asked, priced over an on-demand machine: r7a.16xlarge");
        assert_eq!(aws_machine(&serde_json::json!({ "cloud": "aws" })), None);
        assert_eq!(aws_machine(&serde_json::json!({ "cloud": "cloudflare", "machine_type": "4cpu-12gb" })), None);
    }

    #[test]
    fn an_organizations_picture_is_asked_for_at_the_start_and_once_a_day() {
        let mut d = Dashboard::new();
        assert!(d.picture_due().is_none(), "nothing connected on github.com: nothing to ask for");
        d.picture_owner = "acme".into();
        assert_eq!(d.picture_due().as_deref(), Some("acme"));
        assert!(d.picture_due().is_none(), "asked a moment ago");
        // None came: again ten minutes later. One is kept: again a day later, and it stands meanwhile.
        d.picture_asked_ms = now_ms() - 11 * 60_000;
        assert_eq!(d.picture_due().as_deref(), Some("acme"));
        (d.about.picture, d.about.picture_of, d.picture_asked_ms) = ("data:image/png;base64,iVBORw0KGgo=".into(), "acme".into(), now_ms() - 11 * 60_000);
        assert!(d.picture_due().is_none());
        d.picture_asked_ms = now_ms() - 25 * 3_600_000;
        assert_eq!(d.picture_due().as_deref(), Some("acme"));
        // The mark at the top of the sidebar is that picture, and SuperCI's own without one or with one not made here.
        assert!(brand_mark(&d.about.picture).starts_with(r#"<img class="mark" src="data:image/png;base64,iVBORw0KGgo=" alt=""><svg hidden class="mark""#));
        assert!(brand_mark("").starts_with(r#"<svg class="mark""#) && brand_mark(r#"data:image/png;base64,x" onerror="alert(1)"#).starts_with(r#"<svg class="mark""#) && brand_mark("https://example.com/x.png").starts_with("<svg"));
    }

    #[test]
    fn a_workspace_goes_along_in_its_addresses() {
        // Where the dashboard sends a page on, the workspace is named; an address that names one is left as it is.
        let to = |location: &str| carry(Response::redirect(location), "acme").headers.into_iter().find(|(k, _)| k == "location").unwrap().1;
        assert_eq!((to("/"), to("/?p=repos"), to("/?p=repos&w=globex"), to("/?w=acme&c=")), ("/?w=acme".to_string(), "/?p=repos&w=acme".to_string(), "/?p=repos&w=globex".to_string(), "/?w=acme&c=".to_string()));
        assert_eq!(to("https://github.com/login"), "https://github.com/login");
        // Back from a sign-in elsewhere: to this workspace's page, as its address was.
        let mut d = Dashboard::new();
        assert_eq!((d.back_url(""), d.back_url("?p=repos")), ("http://localhost:8976/".to_string(), "http://localhost:8976/?p=repos".to_string()));
        d.named = "acme".into();
        assert_eq!((d.back_url(""), d.back_url("?p=repos"), d.back_url("?p=repos&w=acme&c=abc")), ("http://localhost:8976/?w=acme".to_string(), "http://localhost:8976/?p=repos&w=acme".to_string(), "http://localhost:8976/?p=repos&w=acme&c=abc".to_string()));
        assert_eq!((safe_return("?w=acme&p=add-runners&c=abc123"), safe_return("?p=repos"), safe_return("//evil.example"), safe_return("?p=x&next=https://evil.example")), ("?w=acme&p=add-runners&c=abc123".to_string(), "?p=repos".to_string(), String::new(), String::new()));
    }

    /// With SUPERCI_PREVIEW_DIR set, each screen is also written there as a page, to look at.
    fn preview(name: &str, fragment: &str) {
        let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") else { return };
        let page = String::from_utf8(Dashboard::page("overview").body).unwrap()
            .replace(r#"<main class="main" id="live">"#, r#"<main class="main" id="live" data-static>"#)
            .replacen("<script>(function(){var main", "<script>(function(){return;var main", 1);
        let (a, b) = page.split_once(r#"data-static>"#).unwrap();
        let rest = &b[b.find("</main>").unwrap()..];
        let gated = if fragment.contains("data-gated") { r#"<script>document.body.classList.add('gated')</script>"# } else { "" };
        let gated = format!(r#"{gated}<script>var c=document.querySelector('[data-called]');if(c){{document.querySelector('.brand span').textContent=document.title=c.getAttribute('data-called')}}var u=document.querySelector('[data-side]');if(u){{document.getElementById('side-update').innerHTML=u.innerHTML;u.remove()}}document.querySelectorAll('form.picker').forEach(superciPick)</script>"#);
        std::fs::write(format!("{dir}/{name}.html"), format!(r#"{a}data-static>{fragment}{rest}{gated}"#)).unwrap();
    }

    #[test]
    fn before_connecting_it_asks_where_superci_runs() {
        let d = Dashboard::new();
        let html = d.render("overview", &[], None);
        preview("connect", &html);
        assert!(html.contains("data-gated") && html.contains("Where is your control plane?") && html.contains("First time here?"));
        // Links to each cloud's own sign-in, never a sign-up; nothing marked before a cloud was picked here.
        assert!(html.contains(r#"href="/connect/cloudflare""#) && html.contains(r#"href="/connect/aws""#) && html.contains(r#"href="/connect/modal""#) && !html.contains("Last used"));
        assert!(!html.to_lowercase().contains("sign in") && !html.to_lowercase().contains("sign up"));
        assert!(html.contains("More clouds") && html.contains(r#"<span class="soon-tag">Soon</span>"#) && html.contains("Google Cloud"));
        // The cloud picked last (its cookie), and only that one, is marked.
        let marked = d.render("overview", &[], Some("aws"));
        preview("connect-last", &marked);
        assert_eq!(marked.matches("Last used").count(), 1);
        assert!(marked.contains(r#"<a class="pick last-used" href="/connect/aws">"#));
        // Every page waits behind the same question.
        assert!(d.render("runners", &[], None).contains("data-gated"));
    }

    #[test]
    fn nowhere_yet_asks_where_it_should_live() {
        let mut d = Dashboard::new();
        d.show_setup = true;
        let html = d.render("overview", &[], None);
        preview("where", &html);
        assert!(html.contains("Where should your control plane live?") && html.contains("Nothing is created until you press Deploy.") && html.contains("new=0"));
    }

    #[test]
    fn connecting_to_an_empty_account_offers_a_deploy_and_deploys_nothing() {
        let d = with_cloudflare(&[("a1", "Acme Inc."), ("a2", "Domas's Account")]);
        let html = d.render("overview", &[], None);
        preview("deploy-cloudflare", &html);
        // Deploying is the first step of Set up; the steps after it wait, and the page below is as it will be, empty.
        assert!(html.contains("No SuperCI in your Cloudflare account yet") && html.contains(r#"<a class="button primary" href="/?p=plane">Set up control plane</a>"#) && !html.contains(r#"action="/plane/cloudflare""#));
        assert!(html.contains(r#"<section class="card" id="setup">"#) && html.matches(r#"<div class="then-step">"#).count() == 3 && html.contains("<h3>Connect GitHub or GitLab</h3>"));
        assert!(html.contains(r#"<div class="kpi is-empty"><div class="kpi-num">0</div><div class="kpi-cap">jobs</div>"#) && html.contains("<h2>Latest</h2>") && html.contains("No jobs yet"));
        assert!(html.find(r#"id="setup""#).unwrap() < html.find("<h2>Latest</h2>").unwrap());
        // Where it goes is chosen on "Set up a control plane": a row per cloud, the account picked in the row.
        let plane = d.render("plane", &[], None);
        assert!(plane.contains(r#"action="/plane/cloudflare""#) && plane.contains(r#"<select name="account" aria-label="Account"><option value="a1">Acme Inc.</option>"#) && plane.contains("Domas&#39;s Account") && plane.contains("More clouds"));
        assert!(plane.contains(r#"action="/aws/signin""#) && plane.contains(r#"action="/modal/signin""#), "the other clouds are one click away");
        assert!(lock(&d.deploying).is_none());

        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        d.aws = Some(aws::Session::for_test("123456789012"));
        d.show_setup = true;
        let html = d.render("overview", &[], None);
        preview("deploy-both", &html);
        assert!(html.contains(r#"href="/?p=plane">Set up control plane</a>"#) && html.contains("Runners can be on other clouds too."));
        let plane = d.render("plane", &[], None);
        assert!(plane.contains(r#"action="/plane/cloudflare""#) && plane.contains(r#"action="/plane/aws""#) && plane.contains(r#"name="region""#) && plane.matches(">Set up</button>").count() == 2);
        assert!(plane.contains(r#"action="/modal/signin""#), "the cloud not signed in to is one click away");
        assert!(!plane.contains(r#"<select name="account""#), "one account needs no choice");
    }

    #[test]
    fn runners_and_control_planes_are_added_from_the_same_list() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "containers": true, "launchers": [] })) };
        let html = d.render("add-runners", &[view.clone()], None);
        preview("add-runners", &html);
        assert!(html.contains(r#"<div class="pick act">"#) && html.contains("Added") && html.contains(r#"action="/aws/signin""#) && html.contains(r#"action="/modal/signin""#));
        assert!(html.contains("More clouds") && html.contains("Hetzner") && !html.contains("CloudFormation"));
        // Your own computers: coming soon, nothing to add yet.
        assert!(html.contains("Your own Mac or Linux machine, a clean VM per job") && !html.contains("/computers/") && !html.contains("Update available"));

        // A control plane from an earlier SuperCI: updated first.
        let mut old = view.clone();
        old.version = Some("0.1.0".into());
        let html = d.render("add-runners", &[old.clone()], None);
        preview("add-runners-old", &html);
        assert!(html.contains("<div data-side hidden>") && html.contains("Update available"));
        assert!(d.render("jobs", &[old.clone()], None).contains("Update available"), "on every page");
        // Updating: its steps where the Update button was, and the page looks again meanwhile.
        *lock(&d.updating) = Some(Update { plane: old.plane.plane_id().to_string(), steps: &UPDATE_CLOUDFLARE, at: 1, result: None, ended_ms: 0, note: String::new() });
        let html = d.render("jobs", &[old.clone()], None);
        assert!(html.contains(r#"<span class="upd-step"><span class="mini-spin"></span>Giving it its address</span>"#) && html.contains("data-refresh") && !html.contains(">Update to "));
        let planes = d.render("planes", &[old.clone()], None);
        preview("planes-updating", &planes);
        assert!(planes.contains(r#"<small>Updating to "#) && planes.contains("Giving it its address <span class=\"faint\">(2 of 3)</span>") && !planes.contains(r#"<span class="pick-end"><div class="upd""#), "its progress in the row's second line");
        *lock(&d.updating) = Some(Update { plane: old.plane.plane_id().to_string(), steps: &UPDATE_CLOUDFLARE, at: 0, result: Some(Err("Cloudflare said no".into())), ended_ms: 0, note: String::new() });
        assert!(d.render("jobs", &[old.clone()], None).contains("The update stopped: Cloudflare said no"));
        *lock(&d.updating) = None;
        d.render("jobs", &[old.clone()], None);
        assert!(lock(side()).contains(&format!("Update to {DASHBOARD_VERSION}")) && String::from_utf8(Dashboard::page("jobs").body).unwrap().contains("Update available"), "drawn with the next page at once");
        // Settings is a page of its own, as what is new is.
        assert_eq!(section_name("settings"), "settings");
        let html = d.render("planes", &[old.clone()], None);
        preview("planes-old", &html);
        assert!(html.contains(&format!("Update to {DASHBOARD_VERSION}")) && html.contains(r#"<span class="pill accent"><i></i>In use</span>"#) && !html.contains("Changelog</h2>"));
        let html = d.render("changes", &[old], None);
        preview("changes", &html);
        assert!(html.matches("Not on your control plane yet").count() == changelog().len() - 1 && html.contains("<code>superci join</code>") && html.matches(r#"class="release""#).count() == changelog().len());
        let html = d.render("planes", &[view.clone()], None);
        preview("planes", &html);
        assert!(html.contains(&format!(r#"<span class="mono faint">{DASHBOARD_VERSION}</span>"#)) && !html.contains("Update to") && !html.contains("Not in use") && !html.contains("shown") && !html.contains("+ Add control plane"));
        // One list: the one in use (stopping SuperCI in its menu), then clouds to set one up in.
        assert!(html.contains(r#"<div class="pick act current">"#) && !html.contains("This dashboard is") && !html.contains("Set up another control plane"));
        assert!(html.contains(r#"<details class="menu">"#) && html.contains(r#"document.getElementById('leave').showModal()">Stop using SuperCI…</button>"#) && !html.contains(r#"action="/plane/delete""#), "the one in use is not deleted from its row");
        assert!(lock(side()).is_empty());

        // One in use; another, set up to move to: Move here. The in-use one is the one GitHub or GitLab sends jobs to,
        // wherever it is in the list; one that moved away is never in use.
        d.modal = Some(modal::Session { token_id: "ak-test".into(), token_secret: "as-test".into(), workspace: "acme".into() });
        let other = PlaneView { plane: Plane::Modal { workspace: "acme".into(), url: "https://acme--superci-plane-xyz.modal.run".into(), plane_id: "xyz000000001".into(), label: "superci".into() },
            online: true, github: false, installed: false, aws: false, runners: false, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: None };
        assert_eq!(view::in_use(&[other.clone(), view.clone()]), 1);
        let html = d.render("planes", &[other.clone(), view.clone()], None);
        preview("planes-move", &html);
        assert!(html.contains(r#"onclick="document.getElementById('move-0').showModal()">Move here</button>"#) && html.find("pick act current").unwrap() < html.find("Move here</button>").unwrap());
        // Another one that serves a code host of its own (another team's): shown, with no Move here and no Delete.
        let mut theirs = other.clone();
        (theirs.github, theirs.installed) = (true, true);
        // Two that are live and a page that names neither: the list of them, each with one button to its pages, and
        // setting up a new one. Nothing picks one of them.
        let list = d.render("planes", &[view.clone(), theirs.clone()], None);
        preview("all", &list);
        assert!(list.contains(r#"<h1 class="page-title">Control planes</h1>"#) && list.contains(&format!(r#"href="/?c={}">Open</a>"#, view.plane.plane_id())) && list.contains(r#"href="/?c=xyz000000001">Open</a>"#), "{}", &list[..400.min(list.len())]);
        assert!(list.contains(r#"<a class="button primary sm" href="/?p=plane&c=">New control plane</a>"#) && list.contains("A control plane is one CI") && list.contains("data-list") && !list.contains("Add an organization"));
        // (The switcher at the top is between workspaces: it lists no control plane and adds no organization.)
        assert!(!list.contains("data-switch") && !list.contains("All control planes"));
        assert!(!list.contains("Move here") && !list.contains(r#"action="/plane/"#) && d.render("jobs", &[view.clone(), theirs.clone()], None).contains("Control planes</h1>"), "every page is the list until one is named");
        // A page that names one shows that one, and offers nothing of the other.
        view::name(Some(view.plane.plane_id()));
        let beside = d.render("planes", &[view.clone(), theirs.clone()], None);
        preview("planes-two-teams", &beside);
        assert!(beside.contains("pick act current") && beside.contains("Serves a code host of its own") && !beside.contains("Move here</button>") && !beside.contains(r#"action="/plane/delete""#), "nothing of another team's is offered");
        assert!(!beside.contains("Add an organization to"));
        view::name(Some("xyz000000001"));
        assert_eq!(view::in_use(&[view.clone(), theirs.clone()]), 1, "the one named, not the first that answers");
        view::name(None);
        assert!(view::several(&[view.clone(), theirs]).is_some() && view::several(&[view.clone(), other.clone()]).is_none(), "one live beside an empty one is as before");
        // Move here's confirmation says what comes along; the other one can be deleted.
        assert!(html.contains(r#"<dialog class="dlg" id="move-0""#) && html.contains("Your GitHub App and its installations") && html.contains(r#"<input type="hidden" name="plane" value="xyz000000001"><button class="button primary sm">Move</button>"#));
        assert!(html.contains(r#"<form method="post" action="/plane/delete""#) && html.contains("Delete Modal · acme?") && html.contains(r#"document.getElementById('delete-0').showModal()">Delete…</button>"#));
        assert!(html.find("superci.acme.workers.dev").unwrap() < html.find("Move here</button>").unwrap(), "the one in use first");
        let mut moved = view.clone();
        moved.moved_to = Some(other.plane.url().into());
        let mut took_over = other.clone();
        took_over.github = true;
        took_over.status = Some(serde_json::json!({ "containers": false, "launchers": [], "jobs": [] }));
        assert_eq!(view::in_use(&[moved.clone(), took_over.clone()]), 1);
        let html = d.render("planes", &[moved, took_over], None);
        preview("planes-moved", &html);
        assert!(html.contains(r#"<span class="pick-end"><button type="button" class="button secondary sm" onclick="document.getElementById('move-0').showModal()">Move here</button>"#) && !html.contains("Used before"), "and back again");
        d.modal = None;

        d.aws = Some(aws::Session::for_test("123456789012"));
        let html = d.render("plane", &[view.clone()], None);
        preview("plane", &html);
        // Another control plane is set up on the control plane's page, to move to: not in an account that has one.
        assert!(!html.contains("Has your control plane") && !html.contains(r#"action="/plane/cloudflare""#), "the account has one: its row is the control plane's");
        assert!(html.contains(r#"action="/plane/aws""#) && html.contains(r#"name="region""#) && html.contains("Google Cloud"));
        assert!(html.contains("US East (N. Virginia) · us-east-1"), "regions by their names");

        // AWS runners: the region set, with why; what the account may run there; one click; another region if needed.
        d.quotas = [("us-east-1".to_string(), Ok(32)), ("eu-west-1".to_string(), Ok(5))].into_iter().collect();
        let html = d.render("add-runners", &[view.clone()], None);
        preview("add-runners-aws", &html);
        assert!(html.contains("in US East (N. Virginia), closest to GitHub, with the most spot capacity; Ohio and Oregon when it runs out")
            && html.contains("up to 32 spot CPUs at once there (8 jobs of 4 CPU)") && !html.contains("Ask AWS for more"));
        assert!(html.contains(r#"<summary class="button secondary sm">Change region</summary>"#) && html.contains("Europe (Ireland) · eu-west-1 · up to 5 CPUs") && html.contains(r#"<option value="us-east-1" selected>"#));
        assert!(quota_text("eu-west-1", 5).contains("5 spot CPUs at once there (1 job of 4 CPU)") && quota_text("eu-west-1", 5).contains("eu-west-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-34B43A08"));
        // A job the order sent on to an on-demand machine says why, in its row and in a line above the jobs.
        let ran = |refused: Option<&str>| serde_json::json!({ "job_id": 1, "state": "done", "cloud": "aws", "on_demand": true, "machine_type": "c8a.xlarge", "cpu": 4, "spot_refused": refused, "at_ms": now_ms() });
        let limited = ran(Some("us-east-1: MaxSpotInstanceCountExceeded"));
        assert_eq!(job_words(&limited)["why"], "On-demand: your AWS spot limit in N. Virginia was reached");
        assert_eq!(job_words(&ran(Some("us-east-1: InsufficientInstanceCapacity; us-east-2: InsufficientInstanceCapacity")))["why"], "On-demand: AWS had no spot machine for it");
        // One run again after AWS took its spot machine back says that (AWS may well have had spot machines), and one
        // that went back on a spot machine because its on-demand machine did not fit the account's limit says so.
        let mut again = ran(Some("us-east-1 on-demand: VcpuLimitExceeded"));
        again["after_reclaim"] = true.into();
        assert_eq!(job_words(&again)["why"], "On-demand: run again after AWS took back its spot machine");
        (again["on_demand"], again["spot_again"]) = (false.into(), true.into());
        assert_eq!(job_words(&again)["why"], "On a spot machine again: an on-demand one does not fit your AWS account's on-demand limit");
        // A disk that is not the standard one is in the job's row, beside its machine.
        (again["disk_gb"], again["machine_type"], again["cpu"]) = (120.into(), "r8a.8xlarge".into(), 32.into());
        assert_eq!(job_words(&again)["runner"], "32 CPU · r8a.8xlarge · 120 GB disk");
        again["disk_gb"] = 60.into();
        assert_eq!(job_words(&again)["runner"], "32 CPU · r8a.8xlarge");
        // Its disk's speed too, when above what AWS includes; and where its files went, when its label asked.
        again["disk_mibs"] = 1000.into();
        assert_eq!(job_words(&again)["runner"], "32 CPU · r8a.8xlarge · 1000 MiB/s disk");
        (again["disk_gb"], again["ramdisk"]) = (120.into(), true.into());
        assert_eq!(job_words(&again)["runner"], "32 CPU · r8a.8xlarge · 120 GB disk at 1000 MiB/s · RAM disk");
        let words = job_words(&again);
        assert_eq!((words["disk"]["gb"].as_u64(), words["disk"]["mibs"].as_u64(), words["disk"]["iops"].as_u64(), words["disk"]["ramdisk"].as_bool()), (Some(120), Some(1000), Some(4000), Some(true)));
        assert_eq!(job_words(&ran(None))["why"], "", "a job whose label asked for on-demand");
        assert!(job_table(&[limited.clone()]).contains(r#"<small class="jt-why open">On-demand: your AWS spot limit in N. Virginia was reached</small>"#));
        let notice = spot_limit_notice(&[limited.clone(), limited, ran(None)]);
        assert!(notice.contains("2 jobs in the last day ran on on-demand machines") && notice.contains("us-east-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-34B43A08"), "{notice}");
        assert_eq!(spot_limit_notice(&[ran(None), ran(Some("us-east-1: InsufficientInstanceCapacity"))]), "");
    }

    #[test]
    fn several_organizations_are_rows_of_their_own_with_one_more_to_add() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let perms = serde_json::json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" });
        let status = serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "launchers": [], "jobs": [],
            "app": { "id": 1, "slug": "superci-acme-p1", "owner": "acme", "org": true },
            "apps": [{ "id": 1, "slug": "superci-acme-p1", "owner": "acme", "org": true, "permissions": perms },
                     { "id": 2, "slug": "superci-beta-p1", "owner": "beta", "org": true, "permissions": { "organization_self_hosted_runners": "write", "actions": "read", "metadata": "read" } }],
            "installations": [{ "id": 42, "account": "acme", "repositories": "all", "permissions": perms, "app": 1 }],
            "permissions": { "github_app": perms, "denied": {} } });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let html = d.render("repos", &[view.clone()], None);
        preview("repos-orgs", &html);
        // Each organization a row: the first connected; the second's App made but not installed yet, and removable.
        assert!(html.contains("GitHub · acme") && html.contains("Jobs from acme (all repositories)") && html.contains("GitHub · beta"));
        assert!(html.contains("The App superci-beta-p1 exists; install it") && html.contains("https://github.com/apps/superci-beta-p1/installations/new"));
        assert!(html.matches(r#"action="/github/remove""#).count() == 1 && html.contains(r#"name="app" value="2""#), "only an added one can be removed");
        // And one more can be added, the way the first was.
        assert!(html.contains("Another organization") && html.matches(r#"action="/github/start""#).count() == 1);
        // An organization on a GitHub Enterprise Server: its links go to that server (its Apps' pages are under /github-apps).
        let mut corp = view.clone();
        if let Some(st) = corp.status.as_mut() { st["apps"][1]["host"] = "ghe.corp.example".into(); st["jobs"] = serde_json::json!([{ "job_id": 5, "run_id": 50, "repo": "beta/api", "state": "done", "cloud": "cloudflare", "at_ms": 1, "app_id": 2 }, { "job_id": 6, "run_id": 60, "repo": "acme/app", "state": "done", "cloud": "cloudflare", "at_ms": 1, "app_id": 1 }]) }
        let html_corp = d.render("repos", &[corp.clone()], None);
        assert!(html_corp.contains("https://ghe.corp.example/github-apps/superci-beta-p1/installations/new") && !html_corp.contains("https://github.com/apps/superci-beta-p1"));
        let links: Vec<String> = corp.jobs().iter().map(|j| job_link(&with_hosts(&corp, j))).collect();
        assert!(links.contains(&"https://ghe.corp.example/beta/api/actions/runs/50/job/5".to_string()) && links.contains(&"https://github.com/acme/app/actions/runs/60/job/6".to_string()), "each job's page links to its own GitHub");
        // Where GitHub is can be said when an App is made (github.com unless your own is chosen).
        assert!(html.contains(r#"name="gh-where" value="own""#) && html.contains(r#"name="host" placeholder="github.example.com""#));
        // Not on a control plane from before it knew several: updated first.
        let old = PlaneView { version: Some("0.9.30".into()), ..view.clone() };
        let html_old = d.render("repos", &[old], None);
        assert!(html_old.contains("Another organization") && html_old.contains("Update the control plane first") && !html_old.contains(r#"action="/github/start""#));
        // Permissions: a row each, naming whose.
        let html = d.render("planes", &[view], None);
        assert!(html.contains("<small>acme: App superci-acme-p1</small>"));
        // The second's App asks for less than this version needs: its steps say where to add it, each with its link.
        assert!(html.contains("<small>beta: 1 permission to add in its App</small>") && html.contains(r#"id="perm-gh-2""#) && html.contains("Repository permissions → Actions → Read and write, then Save changes.")
            && html.contains(r#"href="https://github.com/organizations/beta/settings/apps/superci-beta-p1/permissions""#));
    }

    #[test]
    fn a_network_of_your_own_is_a_line_a_region_and_keeps_supercis_out_of_that_region() {
        let nets = given_networks("us-east-1 subnet-0aaaaaaaa, subnet-0bbbbbbbb sg-0cccccccc private\n\neu-west-1: subnet-0dddddddd sg-0eeeeeeee\n").unwrap();
        assert_eq!(nets["us-east-1"], superci_core::aws::GivenNetwork { subnets: vec!["subnet-0aaaaaaaa".into(), "subnet-0bbbbbbbb".into()], security_groups: vec!["sg-0cccccccc".into()], private: true });
        assert!(!nets["eu-west-1"].private && nets.len() == 2);
        assert!(given_networks("").unwrap().is_empty());
        for (bad, why) in [("mars-1 subnet-0aaaaaaaa sg-0cccccccc", "not one"), ("us-east-1 subnet-0aaaaaaaa", "at least one subnet and one security group"), ("us-east-1 subnet-0aaaaaaaa sg-0cccccccc $(reboot)", "is not a subnet id"),
            ("us-east-1 subnet-0aaaaaaaa sg-0cccccccc\nus-east-1 subnet-0bbbbbbbb sg-0cccccccc", "twice")] {
            assert!(given_networks(bad).unwrap_err().contains(why), "{bad}");
        }
        // Its own network is made (and asked for under Permissions) only where none was given.
        let status = serde_json::json!({ "aws": { "region": "us-east-1" }, "aws_regions": ["us-east-1", "us-east-2"], "aws_networks": serde_json::to_value(&nets).unwrap() });
        assert_eq!(status_regions(&status), ["us-east-2"]);
        assert_eq!(given_networks(&given_networks_text(&status)).unwrap(), nets, "shown as it is typed");
        // Machines up are counted by the control plane over all its jobs, not over the fifty its status lists.
        assert_eq!(running_on(&serde_json::json!({ "active": { "aws": 3 }, "jobs": [] }), "aws"), 3);
        assert_eq!(running_on(&serde_json::json!({ "jobs": [{ "cloud": "aws", "state": "running" }] }), "aws"), 1, "an older control plane: from its list");
    }

    #[test]
    fn an_app_is_made_at_a_github_enterprise_server_through_its_own_api() {
        use std::io::{Read, Write};
        // A stand-in for a GitHub Enterprise Server, on this machine: it answers what the dashboard asks while an App
        // is made (whose a name is, and the manifest code's exchange), and notes what it was asked.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://127.0.0.1:{}/api/v3", listener.local_addr().unwrap().port());
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let noted = asked.clone();
        std::thread::spawn(move || for stream in listener.incoming().take(3) {
            let mut stream = stream.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") { if stream.read(&mut byte).unwrap_or(0) == 0 { break } head.push(byte[0]) }
            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().trim_end_matches(" HTTP/1.1").to_string();
            let (status, body) = match line.as_str() {
                "GET /api/v3/users/corp" => ("200 OK", serde_json::json!({ "login": "Corp", "type": "Organization" }).to_string()),
                "POST /api/v3/app-manifests/CODE12345/conversions" => ("201 Created", serde_json::json!({ "id": 7, "slug": "superci-corp-p1", "pem": "-----BEGIN RSA PRIVATE KEY-----", "webhook_secret": "whsec", "owner": { "login": "Corp", "type": "Organization" } }).to_string()),
                // In private mode it tells nothing to someone not signed in.
                _ => ("404 Not Found", r#"{"message":"Not Found"}"#.to_string()),
            };
            noted.lock().unwrap().push(line);
            let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        });
        let host = Some("ghe.corp.example");
        assert_eq!(github_owner_at(&api, host, "corp").unwrap(), Owner { org: true, login: "Corp".into() });
        assert_eq!(github_owner_at(&api, host, "hidden").unwrap(), Owner { org: true, login: "hidden".into() }, "not told: taken as an organization");
        let app = convert_manifest_code(&api, host, "CODE12345").unwrap();
        assert_eq!((app.id, app.owner.as_str(), app.owner_is_org, app.host.as_deref()), (7, "Corp", true, host));
        // From then on everything about it goes to that server: its API, where it is installed, its settings.
        assert_eq!(app.api(), "https://ghe.corp.example/api/v3");
        assert_eq!(github::install_link(app.host.as_deref(), &app.slug), "https://ghe.corp.example/github-apps/superci-corp-p1/installations/new");
        assert_eq!(app_settings_link(host, "Corp", "superci-corp-p1", true), "https://ghe.corp.example/organizations/Corp/settings/apps/superci-corp-p1");
        assert_eq!(manifest_target(host, &Owner { org: true, login: "Corp".into() }, "s1"), "https://ghe.corp.example/organizations/Corp/settings/apps/new?state=s1");
        assert_eq!(*asked.lock().unwrap(), ["GET /api/v3/users/corp", "GET /api/v3/users/hidden", "POST /api/v3/app-manifests/CODE12345/conversions"]);
        // Its key is kept with where it is from, and github.com's Apps say nothing of a host.
        assert!(serde_json::to_string(&app).unwrap().contains(r#""host":"ghe.corp.example""#));
        assert!(!serde_json::to_string(&github::App { host: None, ..app }).unwrap().contains("host"));
    }

    #[test]
    fn an_aws_sign_in_that_did_not_come_back_offers_signing_out_of_aws_first() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        assert_eq!(d.aws_stuck(), "", "nothing started");
        // Started, not back yet (AWS's page said "400 Bad Request" and the person came back): the way on.
        let (link, pending) = aws::authorize("us-east-1", "http://127.0.0.1:8976/oauth/callback");
        (d.aws_pending, d.aws_asked_ms) = (Some(pending), now_ms());
        let note = d.aws_stuck();
        assert!(note.contains("400 Bad Request") && note.contains(r#"action="/aws/signin""#) && note.contains(r#"name="fresh" value="on""#));
        // AWS's own way too: signed in to the console as usual in another tab, its sign-in uses that session.
        assert!(note.contains(r#"href="https://console.aws.amazon.com/" target="_blank""#));
        // That goes by AWS's sign-out (which clears the old session) on to the same sign-in.
        let fresh = aws::signed_out_first(&link);
        assert!(fresh.starts_with("https://signin.aws.amazon.com/oauth?Action=logout&redirect_uri=https%3A%2F%2Fus-east-1.signin.aws.amazon.com%2Fv1%2Fauthorize%3F") && !fresh[60..].contains('&'), "{fresh}");
        // Ten minutes on, or once signed in: nothing more is said.
        d.aws_asked_ms = now_ms() - 11 * 60_000;
        assert_eq!(d.aws_stuck(), "");
        (d.aws_asked_ms, d.aws) = (now_ms(), Some(aws::Session::for_test("123456789012")));
        assert_eq!(d.aws_stuck(), "");
    }

    #[test]
    fn while_modals_sign_in_waits_every_page_looks_again() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "launchers": [{ "cloud": "modal", "url": "https://acme--superci-runners-p1.modal.run" }], "permissions": {} });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        assert!(!d.render("planes", &[view.clone()], None).contains("data-refresh"));
        d.modal_pending = Some(modal::Pending::for_test());
        for page in ["planes", "runners", "jobs", "overview"] { assert!(d.render(page, &[view.clone()], None).contains("data-refresh"), "{page}") }
        assert!(d.render("planes", &[view], None).contains("Approve it in Modal&#39;s tab, then close the tab"));
    }

    #[test]
    fn removing_a_runner_provider_says_what_goes_where_and_needs_that_clouds_sign_in() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = serde_json::json!({
            "containers": true, "own_cloud": "cloudflare", "launchers": [{ "cloud": "modal", "url": "https://acme--superci-runners-p1.modal.run" }],
            "aws": { "account_id": "123456789012", "region": "us-east-1", "connected": true }, "aws_regions": ["us-east-1"],
            "routing": { "rules": [], "order": [{ "cloud": "aws" }, { "cloud": "cloudflare" }, { "cloud": "modal" }] },
            "jobs": [{ "job_id": 7, "run_id": 70, "repo": "acme/app", "state": "running", "cloud": "modal", "at_ms": 1 }],
        });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status.clone()) };
        let html = d.render("runners", &[view.clone()], None);
        preview("runners-remove", &html);
        // Removing is the cloud's (under Clouds, a row each: AWS, Cloudflare, Modal), not a place's in the order:
        // AWS's spot and on-demand rows are only taken out of the order.
        assert!(html.contains("<h2>Clouds</h2>") && html.matches(r#"action="/runners/remove""#).count() == 3 && html.matches("Take out of the order").count() == 4);
        let dialog = |cloud: &str| { let i = 100 + ["aws", "cloudflare", "modal"].iter().position(|c| *c == cloud).unwrap(); let start = html.find(&format!(r#"id="pool-remove-{i}""#)).unwrap(); html[start..start + html[start..].find("</dialog>").unwrap()].to_string() };
        // Each names the control plane it is for (another may be in use by the time it is sent).
        assert_eq!(html.matches(r#"action="/runners/remove""#).count(), html.matches(r#"<input type="hidden" name="plane" value="p1"><input type="hidden" name="cloud""#).count());
        // Each provider's settings open its removal, which says what goes where; the control plane and sign-ins stay.
        assert!(html.contains("document.getElementById('pool-remove-100').showModal()") && html.contains("Your control plane and your sign-ins stay"));
        let aws = dialog("aws");
        assert!(aws.contains("Remove AWS?") && aws.contains("The role superci-plane-p1 and its identity provider, in account 123456789012"));
        // Not signed in to AWS: a sign-in to delete them, or removing it from SuperCI only.
        assert!(aws.contains("Sign in with AWS to delete these") && aws.contains(r#"name="only" value="on""#) && !aws.contains(r#"<button class="button danger sm">"#));
        // Cloudflare's own containers: nothing to delete, nothing to sign in to.
        let cf = dialog("cloudflare");
        assert!(cf.contains("Nothing is deleted: your control plane stops starting containers.") && cf.contains(r#"<button class="button danger sm">Remove</button>"#) && !cf.contains("only"));
        // Modal, a job running there: said, and the removal stops it (once signed in).
        let modal = dialog("modal");
        assert!(modal.contains("superci-runners-p1") && modal.contains("1 job is running there stop, and fail on GitHub"));
        assert!(running_on(&status, "modal") == 1 && provider_connected(&status, "aws") && provider_connected(&status, "modal") && provider_connected(&status, "cloudflare") && !provider_connected(&status, "x"));
        d.modal = Some(modal::Session { token_id: "ak-test".into(), token_secret: "as-test".into(), workspace: "acme".into() });
        let html = d.render("runners", &[view], None);
        assert!(html.contains(r#"<button class="button danger sm">Stop 1 and remove</button>"#));
    }

    #[test]
    fn a_finished_job_is_said_by_how_the_job_itself_ended() {
        let job = |state: &str, conclusion: Option<&str>| serde_json::json!({ "job_id": 7, "state": state, "conclusion": conclusion, "repo": "acme/app", "cloud": "aws" });
        // A machine ran each to its end; what the row says is whether the job's own steps passed.
        assert_eq!((job_ended(&job("done", Some("success"))), job_ended(&job("done", Some("failure"))), job_ended(&job("done", Some("timed_out")))), (("good", "Passed"), ("bad", "Failed"), ("bad", "Failed")));
        assert_eq!((job_ended(&job("done", Some("cancelled"))), job_ended(&job("done", Some("skipped")))), (("", "Cancelled"), ("", "Skipped")));
        // One from before its ending was kept: done, as it was said then. One SuperCI could not run: failed.
        assert_eq!((job_ended(&job("done", None)), job_ended(&job("failed", None)), job_ended(&job("running", None))), (("good", "Done"), ("bad", "Failed"), ("accent", "Running")));
        let table = job_table(&[job("done", Some("failure"))]);
        assert!(table.contains("Failed") && !table.contains("Done"), "{table}");
        // Two jobs queued together that ran on each other's machines: each machine's row is the job it ran. The
        // machine AWS took back was started for "E2E (2/2)" and ran "E2E (1/2)": that one is what failed, as on GitHub.
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let swapped = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: None, status: Some(serde_json::json!({ "jobs": [
            { "job_id": 12, "run_id": 5, "repo": "acme/app", "name": "E2E (2/2)", "workflow": "Tests", "state": "failed", "error": "AWS took back its spot machine; the job is run again, off spot machines once its run has finished", "cloud": "aws", "ran": 11 },
            { "job_id": 11, "run_id": 5, "repo": "acme/app", "name": "E2E (1/2)", "workflow": "Tests", "state": "done", "cloud": "aws", "ran": 12, "conclusion": "failure" },
            { "job_id": 12, "run_id": 5, "repo": "acme/app", "name": "E2E (2/2)", "workflow": "Tests", "state": "ignored" } ] })) };
        let listed = swapped.jobs();
        assert_eq!((listed[0]["name"].as_str(), listed[0]["job_id"].as_u64(), listed[0]["state"].as_str(), listed[0]["started_for"].as_u64()), (Some("E2E (1/2)"), Some(11), Some("failed"), Some(12)));
        assert_eq!((listed[1]["name"].as_str(), listed[1]["job_id"].as_u64(), listed[1]["conclusion"].as_str()), (Some("E2E (2/2)"), Some(12), None));
        assert!(job_link(&listed[0]).ends_with("/acme/app/actions/runs/5/job/11"));
        // Commands say the same in a job's line, and keep `state` as it was beside it.
        let words = job_words(&job("done", Some("failure")));
        assert_eq!((words["state"].as_str(), words["ended"].as_str()), (Some("done"), Some("failed")));
        assert!(job_words(&job("done", None))["ended"].is_null() && job_words(&job("failed", None))["ended"].is_null());
    }

    #[test]
    fn runners_are_pools_in_order_each_with_its_settings_and_a_machine_picker() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = serde_json::json!({
            "containers": true, "launchers": [{ "cloud": "modal", "url": "https://acme--superci-runners.modal.run" }],
            "aws": { "account_id": "123456789012", "region": "us-east-1" }, "aws_regions": ["us-east-1", "us-east-2"], "cloudflare_location": "auto",
            "aws_networks": { "us-east-2": { "subnets": ["subnet-0aaaaaaaa"], "security_groups": ["sg-0cccccccc"], "private": true } },
            // An order from before computers were set aside: their pools are left out.
            "routing": { "rules": [], "order": [{ "cloud": "machine:m1", "max_jobs": 2 }, { "cloud": "cloudflare", "max_jobs": 10 }, { "cloud": "aws", "monthly_usd": 50.0 }] },
            "machine": { "cpu": 4, "disk_gb": 100 },
            "spend": { "aws": 12.4, "cloudflare": 0.82 },
            "jobs": [
                { "job_id": 7, "run_id": 70, "repo": "acme/app", "state": "waiting", "error": "waiting: Cloudflare runs 10 at once", "cloud": "", "at_ms": 1 },
            ],
        });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        // This month as the clouds bill it: Cloudflare's account within its included usage, Modal's after credits.
        d.cf_month = Some((0.9, 0.0));
        d.modal_month = Some(modal::Month { metered: 3.0, billed: 1.0, superci: 1.5 });
        // In an AWS account whose organization allows no spot machines (as a project of AWS's new sign-up answered),
        // the spot provider says so in "ready"'s place.
        d.aws = Some(aws::Session::for_test("111111111111"));
        d.aws_looks.insert("111111111111".into(), (now_ms(), looked(include_str!("../fixtures/aws/project-paid/look.jsonl"))));
        let no_spot = d.render("runners", &[view.clone()], None);
        assert!(no_spot.contains("<small>Spot machines are not allowed by your AWS organization: its jobs go to the next provider in the order</small>") && no_spot.contains(r#"<span class="pill open"><i></i>Not allowed</span>"#) && !no_spot.contains("Spot machines of any size"), "{no_spot}");
        (d.aws, d.aws_looks) = (None, HashMap::new());
        let html = d.render("runners", &[view.clone()], None);
        preview("runners", &html);
        assert!(html.contains("Spot machines of any size") && !html.contains("not allowed"), "an account that allows them says nothing of it");
        assert!(html.contains(">$0.82 this month · included<"), "cf");
        assert!(html.contains(">$1.50 this month · $0.50 billed<"), "modal");
        // Monthly limits, below the providers. A control plane from before them says so in their place.
        assert!(html.contains(r#"<section class="card" id="limits">"#) && html.contains(r#"<a href="/?p=planes">update it on the Control plane page</a>"#) && !html.contains("limit-all"), "{html}");
        // One that keeps them: the whole CI's, and the AWS account's (its spot and on-demand machines together),
        // each with what was spent this month, its limit, and a dialog of its own to change it.
        let mut newer = view.clone();
        { let st = newer.status.as_mut().unwrap(); st["account_limits"] = true.into(); st["routing"]["monthly_usd"] = 100.0.into(); st["routing"]["account_usd"] = serde_json::json!({ "aws": 12.0 }); st["spend"]["aws-on-demand"] = 1.0.into(); }
        let limits = d.render("runners", &[newer.clone()], None);
        for said in [r#"<strong>Everything</strong><small>All providers together. At a limit no job starts until it is over.</small></span><span class="pool-now"><span class="lim">$14.22 of $100.00</span></span>"#,
            r#"<strong>AWS account 123456789012</strong><small>Its spot and on-demand machines together. At a limit, jobs go to other providers.</small></span><span class="pool-now"><span class="lim">$13.40 of $12.00</span><span class="pill bad"><i></i>At its limit</span></span>"#,
            r#"<dialog class="dlg" id="limit-all" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body"><input type="hidden" name="action" value="monthly_usd">"#,
            r#"<input type="hidden" name="action" value="account_usd"><input type="hidden" name="account" value="aws">"#,
            r#"<input type="number" name="usd" min="0.01" step="any" placeholder="No limit" value="12" autofocus>"#,
            "$13.40 this month. At a limit, jobs go to the providers of other accounts in the order; with none, they fail saying so."] {
            assert!(limits.contains(said), "{said}\n\n{limits}");
        }
        // (It keeps no limit for a day and no budget in AWS yet: neither is offered.)
        assert!(!limits.contains(r#"name="daily""#) && !limits.contains(r#"name="budget""#), "{limits}");
        // One that does: a limit for a day beside the month's, each with what was spent; and for the AWS account,
        // what AWS itself does at the monthly limit, with where its emails go.
        { let st = newer.status.as_mut().unwrap(); st["daily_limits"] = true.into(); st["routing"]["daily_usd"] = 5.0.into(); st["routing"]["account_daily_usd"] = serde_json::json!({ "aws": 2.5 }); st["spend_today"] = serde_json::json!({ "aws": 0.4, "cloudflare": 0.1 }); }
        let limits = d.render("runners", &[newer.clone()], None);
        preview("runners-limits", &limits);
        for said in [r#"<span class="lim">$14.22 of $100.00</span><span class="lim">$0.50 of $5.00 today</span></span>"#,
            r#"<span class="lim">$0.40 of $2.50 today</span></span>"#,
            r#"<label class="field"><span>Daily limit</span><span class="money">$<input type="number" name="daily" min="0.01" step="any" placeholder="No limit" value="2.50"></span><small>$0.40 today (UTC). A day that runs away stops that day.</small></label>"#,
            r#"<select name="budget"><option value="none" selected>Nothing</option><option value="emails">AWS emails me at 80% and 100%</option><option value="stops">AWS emails me, and stops new machines at 100%</option></select>"#,
            r#"<input type="email" name="email" placeholder="you@example.com" autocomplete="email">"#] {
            assert!(limits.contains(said), "{said}\n\n{limits}");
        }
        assert_eq!(limits.matches(r#"name="budget""#).count(), 1, "only the AWS account has a budget in AWS");
        // With a budget that stops machines: the row says so. Once the control plane was refused a machine by it,
        // the row says that, with Lift (which keeps the limits as they are); lifted here since, it is as before.
        newer.status.as_mut().unwrap()["routing"]["aws_budget"] = serde_json::json!({ "stop": true });
        let armed = d.render("runners", &[newer.clone()], None);
        assert!(armed.contains(r#"<span class="pill good"><i></i>AWS stops at the limit</span>"#) && armed.contains(r#"<option value="stops" selected>"#) && armed.contains(r#"placeholder="As it is""#) && !armed.contains("Lift"), "{armed}");
        newer.status.as_mut().unwrap()["aws_budget_stopped_ms"] = (now_ms() - 60_000).into();
        let stopped = d.render("runners", &[newer.clone()], None);
        assert!(stopped.contains(r#"<span class="pill bad"><i></i>Stopped by AWS&#39;s budget</span><form method="post" action="/routing"><input type="hidden" name="action" value="account_usd"><input type="hidden" name="account" value="aws"><input type="hidden" name="usd" value="12"><input type="hidden" name="daily" value="2.5"><input type="hidden" name="budget" value="lift">"#), "{stopped}");
        d.budget_lifted.insert("p1".into(), now_ms());
        assert!(d.render("runners", &[newer.clone()], None).contains("AWS stops at the limit"));
        d.budget_lifted.clear();
        // With emails only, that is what the row says.
        newer.status.as_mut().unwrap()["routing"]["aws_budget"] = serde_json::json!({});
        assert!(d.render("runners", &[newer], None).contains(r#"<span class="pill good"><i></i>AWS emails</span>"#));
        assert!(!html.contains(">$12.40 this month<"), "a budget already says the spend");
        // The pools as ordered; ones added since (Modal) after them.
        let at = |s: &str| html.find(s).unwrap_or_else(|| panic!("{s}"));
        assert!(at(r#"data-cloud="cloudflare""#) < at(r#"data-cloud="aws""#) && at(r#"data-cloud="aws""#) < at(r#"data-cloud="modal""#) && !html.contains("machine:m1"));
        // AWS's on-demand machines are a place of their own, right under its spot machines until moved.
        assert!(html.contains(r#"<ol class="pools" data-order="cloudflare,aws,aws-on-demand,modal">"#) && html.contains(r#"draggable="true""#));
        // Alike rows: each says what it starts and where (AWS's two, their regions), each dialog with the same settings.
        assert!(html.contains("<strong>AWS spot</strong><small>Spot machines of any size · x64, arm64 · N. Virginia, then Ohio</small>") && html.contains("<strong>AWS on-demand</strong><small>On-demand machines of any size, never taken back · x64, arm64 · N. Virginia, then Ohio</small>"), "{html}");
        assert!(!html.contains("Use on-demand machines") && html.matches("data-regions").count() == 2 && html.matches("Take out of the order").count() == 4);
        assert!(html.contains("when that one cannot start it (no spot machine, a failure), to the next"));
        // Limits show only where set; settings are in each pool's dialog.
        assert!(html.contains(">10 at once<") && html.contains(">$12.40 of $50.00<") && !html.contains("No cap"));
        assert!(html.contains(r#"<dialog class="dlg" id="pool-3""#) && html.matches("Monthly budget").count() == 4);
        assert!(html.contains(r#"name="current" value="cloudflare,aws,aws-on-demand,modal""#));
        // One entry for AWS in the machine picker.
        assert_eq!(html.matches(r#"<span class="fit" data-caps="[]""#).count(), 0);
        // Taken out: not in the order, said below it with a way to put it back.
        let mut off = view.clone();
        off.status.as_mut().unwrap()["routing"]["order"] = serde_json::json!([{ "cloud": "aws" }, { "cloud": "cloudflare" }, { "cloud": "aws-on-demand", "off": true }]);
        let html_off = d.render("runners", &[off], None);
        preview("runners-on-demand-off", &html_off);
        assert!(html_off.contains(r#"<ol class="pools" data-order="aws,cloudflare,modal">"#) && !html_off.contains(r#"data-cloud="aws-on-demand""#), "{html_off}");
        assert!(html_off.contains(r#"<strong>AWS on-demand</strong><small>Taken out of the order: no job goes to it. Its settings are kept.</small></span><form method="post" action="/routing" class="limit-do"><input type="hidden" name="action" value="put_back"><input type="hidden" name="cloud" value="aws-on-demand"><input type="hidden" name="current" value="aws,cloudflare,aws-on-demand,modal"><button class="chip">Put back</button>"#), "{html_off}");
        assert!(!html.contains("1 running") && html.contains("Cloudflare runs 10 at once") && !html.contains("Exceptions"));
        // AWS's regions, in order, in its dialog (the next when one has no spot capacity).
        assert!(html.contains("x64, arm64 · N. Virginia, then Ohio") && html.contains(r#"<li class="rg" draggable="true" data-region="us-east-1""#)
            && html.find(r#"data-region="us-east-1""#).unwrap() < html.find(r#"data-region="us-east-2""#).unwrap() && html.contains(r#"<option value="us-west-2">US West (Oregon) · us-west-2</option>"#) && !html.contains(r#"<option value="us-east-1">"#));
        assert!(!html.contains("Default machine"), "machines are on Workflows");
        // AWS: a network of your own, by region, as it is set.
        // (The account's, under Clouds.)
        assert!(html.contains(r#"<span>Your own network</span>"#) && html.contains(">us-east-2 subnet-0aaaaaaaa sg-0cccccccc private</textarea>") && html.contains("your own network in Ohio"));
        // Cloudflare's location, as set (here: Cloudflare chooses); the area near the code hosts marked, and AWS's region.
        assert!(html.contains(r#"<option value="auto" selected>Automatic: Cloudflare chooses</option>"#) && html.contains("Eastern North America · closest to GitHub</option>"));
        assert!(html.contains("us-east-1 · closest to GitHub</small>"));
        // Cloudflare: where GitHub's full image is published, if anywhere (empty: the small image).
        assert!(html.contains(r#"name="image" value="""#) && html.contains("GitHub&#39;s full image") || html.contains("GitHub's full image"));
        // Settings: its name and its labels, a row each, changed in its dialog. The only label has no Remove.
        let html = d.render("settings", &[view.clone()], None);
        preview("settings", &html);
        assert!(html.contains("<strong>Name</strong>") && html.contains(r#"<span class="lim">SuperCI</span>"#) && html.contains(r#"data-called="SuperCI""#));
        assert!(html.contains("<strong>Labels</strong>") && html.contains(r#"<form method="post" action="/labels" class="kept-label">"#) && !html.contains(r#"<button class="button secondary sm">Remove</button>"#));
        // Named, with a label added: the page's frame says the name, and each label can go.
        let mut named = view.clone();
        named.status.as_mut().unwrap()["name"] = "AcmeCI".into();
        named.status.as_mut().unwrap()["labels"] = serde_json::json!(["acmeci", "superci"]);
        let renamed = d.render("settings", &[named], None);
        preview("settings-named", &renamed);
        assert!(renamed.contains(r#"data-called="AcmeCI""#) && renamed.contains(r#"name="name" value="AcmeCI""#) && renamed.matches(r#"<button class="button secondary sm">Remove</button>"#).count() == 2);
        assert!(renamed.contains(r#"<code>acmeci</code> <small class="faint">shown in examples</small>"#) && Dashboard::sidebar("workflows").contains("<span>AcmeCI</span>"));
        *lock(called()) = String::new();
        // Workflows: the default machine, in words, changed in its own dialog (which starts at it). Not what Settings has.
        let html = d.render("workflows", &[view], None);
        preview("workflows", &html);
        assert!(!html.contains("<strong>Name</strong>") && !html.contains("<strong>Labels</strong>") && !html.contains("<h2>Limits</h2>"));
        assert!(html.contains(r#"<span class="lim">4 CPU</span><span class="lim">100 GB disk</span><span class="lim">x64</span><span class="lim">Linux</span><span class="lim">spot</span>"#));
        assert!(html.contains(r#"<dialog class="dlg" id="default-machine""#) && html.contains(r#"<option value="4" selected>4</option>"#) && html.contains(r#"<option value="100" selected>100 GB</option>"#));
        // The disk's speed on AWS is by the machine's size unless the default machine says: what AWS includes, or a number.
        assert!(html.contains(r#"<label class="field"><span>Disk speed (AWS)</span><select name="disk-speed"><option value="">By machine size</option><option value="125">Standard, 125 MiB/s</option><option value="250">250 MiB/s</option>"#), "{html}");
        // Spot machines unless said: one box, in the default's dialog and in the picker.
        assert!(html.matches(r#"<label class="check"><input type="checkbox" name="nospot"> No spot machines</label>"#).count() == 2 && !html.contains("ondemand\"") && !html.contains("On-demand only"), "{html}");
        // The picker writes a label as key=value pairs after its name.
        let shell = String::from_utf8(Dashboard::page("workflows").body).unwrap();
        assert!(shell.contains("if(cpu)parts.push('cpu='+cpu);if(ram)parts.push('ram='+ram);if(disk)parts.push('disk='+disk);if(arch)parts.push('arch='+arch);if(os)parts.push('os='+os);if(img)parts.push('image='+img);if(gpu)parts.push('gpu='+(gpu==='gpu'?'any':gpu));if(od)parts.push('spot=false');") && shell.contains("f.dataset.base+(parts.length?'/'+parts.join('/'):'')"), "the script");
        assert!(html.contains("<code>superci/cpu=8</code>, <code>superci/arch=arm64</code> or <code>superci/cpu=16/ram=64</code>"));
        // The picker starts at Default for everything (the label is plain), each field naming only what is picked.
        assert!(html.contains(r#"data-mode="label""#) && html.contains(r#"<option value="">Default</option>"#) && html.contains(r#"<span class="hl" data-label>superci</span>"#));
        assert!(html.matches(r#"class="fit""#).count() == 6 && !html.contains("Use as default"));
        // Switching workflows with a coding agent: supercov's prompt dialog, two prompts, GitHub's rules (no GitLab here).
        assert!(html.contains(r#"aria-label="Switch workflows with a coding agent""#) && html.contains(r#"id="agent-prompt""#) && html.matches(r#"name="superci-prompt""#).count() == 2);
        assert!(html.contains("Keep on GitHub, with a") && html.contains("One label per job") && !html.contains("data-gl="));
    }

    #[test]
    fn spend_counts_what_machines_were_up_for() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let t = now_ms() - 3_600_000;
        let job = |id: u64, cloud: &str, state: &str, started: Option<u64>, ended: Option<u64>, per_hour: f64, cpu: u32| serde_json::json!({ "job_id": id, "run_id": 1, "repo": "acme/app", "state": state, "at_ms": t,
            "runner": null, "runner_id": null, "installation_id": 1, "cloud": cloud, "machine_id": "m", "machine_type": null, "error": null, "seen_in_progress": started.is_some(),
            "usd_per_hour": per_hour, "started_ms": started, "ended_ms": ended, "launched_ms": t, "cpu": cpu });
        let status = serde_json::json!({ "containers": true, "launchers": [], "jobs": [
            // A container whose job was cancelled before it began: its memory and disk while its runner waited (7 minutes at
            // $0.112/h: $0.40/h less 4 CPUs, billed only as used).
            job(1, "cloudflare", "cancelled", None, Some(t + 420_000), 0.4, 4),
            // A 4-CPU container: 10 s waiting, then 6 minutes at $0.40/h; GitHub's 4-core runner for 6 minutes at $0.012.
            job(2, "cloudflare", "done", Some(t + 10_000), Some(t + 370_000), 0.4, 4),
            // An AWS spot machine is paid from its launch: 10 minutes at $0.06/h.
            job(3, "aws", "done", Some(t + 60_000), Some(t + 600_000), 0.06, 4),
        ] });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let html = d.render("overview", &[view], None);
        preview("overview-spend", &html);
        // $0.013 + $0.040 + $0.01 = $0.06; GitHub: 6 min × $0.012 + 9 min × $0.012 = $0.18 (65% less).
        assert!(html.contains(r#"<div class="kpi-num">$0.06</div><div class="kpi-cap">spent, estimated</div>"#) && html.contains("<span>65% less</span><span class=\"gh-pop\"")
            && html.contains(r#"<span class="mono">$0.18</span>"#), "{}", &html[html.find("kpis").unwrap()..][..2500]);
    }

    #[test]
    fn the_cache_card_says_what_is_kept_and_clears_a_repository() {
        let plane = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://abc.lambda-url.us-east-1.on.aws".into(), plane_id: "p1".into(), label: "superci".into() };
        let view = |st: serde_json::Value| PlaneView { plane: plane.clone(), online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(st) };
        let status = |st: serde_json::Value| st;
        let v = view(status(serde_json::json!({})));
        let caches = serde_json::json!({ "repos": [{ "id": "gh77", "name": "acme/app", "entries": 3, "bytes": 3_400_000_000u64, "used_ms": 0 }, { "id": "gh78", "entries": 1, "bytes": 5_000_000, "used_ms": 7_100_000 }], "now_ms": 7_200_000 });
        let html = cache_card(&v, Some(&caches));
        preview("cache", &html);
        assert!(html.contains("<small>On. Jobs on AWS") && html.contains("Turn off") && html.contains(r#"name="size_gb" min="1" max="10000" value="25""#) && html.contains(r#"name="days" min="1" max="30" value="7""#), "{html}");
        assert!(html.contains("<strong>acme/app</strong><small>3 entries · 3.4 GB · used 2 h ago</small>") && html.contains("<strong>Repository 78</strong><small>1 entry · 5.0 MB · used 1 min ago</small>") && html.matches(r#"action="/caches/delete""#).count() == 2);
        assert!(cache_card(&v, Some(&serde_json::json!({ "repos": [], "now_ms": 1 }))).contains("Nothing kept yet."));
        let mut off = view(status(serde_json::json!({ "routing": { "cache": { "off": true, "size_gb": 50 } } })));
        let html = cache_card(&off, None);
        assert!(html.contains("<small>Off. Jobs use GitHub") && html.contains("Turn on") && html.contains(r#"value="50""#) && !html.contains("Nothing kept"));
        off.aws = false;
        assert!(cache_card(&off, None).is_empty(), "no AWS machines, no cache");
    }

    #[test]
    fn permissions_say_what_is_missing_why_and_how_to_give_it() {
        use superci_core::permissions::AWS_RUNNER;
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = |denied: serde_json::Value| serde_json::json!({ "containers": true, "launchers": [], "jobs": [],
            "app": { "id": 1, "slug": "superci-acme", "owner": "acme", "org": true },
            "installations": [{ "id": 42, "account": "acme", "selection": "all", "permissions": { "organization_self_hosted_runners": "write", "metadata": "read" } }],
            "aws": { "account_id": "123456789012", "region": "us-east-1", "connected": true }, "gitlab": { "url": "https://gitlab.com" },
            "permissions": { "github_app": { "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" }, "gitlab_scopes": ["api", "create_runner"], "denied": denied } });
        let view = |st: serde_json::Value| PlaneView { plane: plane.clone(), online: true, github: true, installed: true, aws: true, runners: true, gitlab: true, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(st) };
        // Not signed in to AWS: what AWS refused lately, with a sign-in.
        let html = d.render("planes", &[view(status(serde_json::json!({ "aws:MachineTraffic": { "at_ms": 1 } })))], None);
        // Each row: a few words and, when there is something to do, one button that opens its steps.
        let dialog = |html: &str, id: &str| { let start = html.find(&format!(r#"id="{id}""#)).unwrap_or_else(|| panic!("{id}")); html[start..start + html[start..].find("</dialog>").unwrap()].to_string() };
        assert!(html.contains(r#"id="permissions">Permissions</h2>"#) && html.contains("<small>AWS refused 1 permission</small>") && html.contains(r#"onclick="document.getElementById('perm-aws').showModal()">Review</button>"#));
        let aws = dialog(&html, "perm-aws");
        assert!(aws.contains("<h2>What AWS refused</h2>") && aws.contains("Read machines&#39; network counts (CloudWatch)<em>Reads</em>") && aws.contains("<h2>Sign in with AWS</h2>") && aws.contains(r#"action="/aws/signin""#));
        // Cloudflare (here signed in): nothing to do, said shortly.
        assert!(html.contains("<small>Your sign-in covers it</small>") && !html.contains("Deploy Workers and set their secrets"));
        // Nothing refused: still a sign-in, to check.
        let html2 = d.render("planes", &[view(status(serde_json::json!({})))], None);
        assert!(html2.contains("<small>Sign in to check its role</small>") && html2.contains(r#"action="/aws/signin""#) && !html2.contains(r#"id="perm-aws""#));
        // GitHub: the installation has not accepted what the App asks for. GitLab: the token lacks a scope.
        let gh = dialog(&html, "perm-gh-1");
        assert!(html.contains("<small>acme has 1 permission to accept</small>") && gh.contains("<h2>What acme has not accepted</h2>") && gh.contains("Actions (write)<em>Changes</em>") && gh.contains(r#"href="https://github.com/organizations/acme/settings/installations/42""#));
        let gl = dialog(&html, "perm-gitlab");
        assert!(html.contains("<small>Its token lacks 1 scope</small>") && gl.contains("manage_runner<em>Changes</em>") && gl.contains("https://gitlab.com/-/user_settings/personal_access_tokens?name=superci&amp;scopes=api,create_runner,manage_runner") && gl.contains(r#"href="/?p=gitlab&g=&open=gl-token""#));
        // The sign-in in the dialog comes back to the dialog (the page opens it again once it is there).
        assert!(aws.contains(r#"<input type="hidden" name="next" data-open="perm-aws">"#));
        // Signed in to another account than the one its role is in: said, in the row and in the dialog.
        d.aws = Some(aws::Session::for_test("999999999999"));
        let wrong = d.render("planes", &[view(status(serde_json::json!({})))], None);
        assert!(wrong.contains("<small>Signed in to account 999999999999; its role is in 123456789012</small>"));
        let wrong = d.render("planes", &[view(status(serde_json::json!({ "aws:MachineTraffic": { "at_ms": 1 } })))], None);
        assert!(dialog(&wrong, "perm-aws").contains("To account 123456789012 (you are signed in to 999999999999)."));
        // Signed in to that account, its role read: what is missing, each with why, to allow in one step.
        d.aws = Some(aws::Session::for_test("123456789012"));
        lock(&d.aws_missing).insert("p1".into(), (Ok(AWS_RUNNER.iter().filter(|n| n.since == "0.9.12" || n.since == "0.9.13").collect()), now_ms()));
        let html = d.render("planes", &[view(status(serde_json::json!({})))], None);
        preview("permissions", &html);
        let aws = dialog(&html, "perm-aws");
        assert!(html.contains("<small>2 permissions to allow</small>") && aws.contains("<h2>What it needs</h2>") && aws.contains("Read AWS&#39;s price list<em>Reads</em>") && aws.contains(r#"action="/permissions/aws""#) && aws.contains(">Allow</button>"));
        assert!(aws.contains("To price on-demand machines and disks at what AWS bills.") && aws.contains("New in 0.9.13.") && aws.contains("the role superci-plane-p1 in account 123456789012"));
        // Being read: says so, and the page looks again until it is; too long, it says AWS did not answer.
        lock(&d.aws_missing).insert("p1".into(), (Err(String::new()), now_ms()));
        let html = d.render("planes", &[view(status(serde_json::json!({})))], None);
        assert!(html.contains("Checking its role…") && html.contains(r#"<div data-refresh="2"></div>"#));
        lock(&d.aws_missing).insert("p1".into(), (Err(String::new()), now_ms() - 60_000));
        assert!(d.render("planes", &[view(status(serde_json::json!({})))], None).contains("Not read: AWS did not answer in time"));
        lock(&d.aws_missing).insert("p1".into(), (Ok(vec![]), now_ms()));
        let html = d.render("planes", &[view(status(serde_json::json!({})))], None);
        assert!(html.contains("<small>Role in account 123456789012</small>") && !html.contains(r#"id="perm-aws""#));
        // An older control plane: its update says what it also asks for.
        let mut old = view(status(serde_json::json!({})));
        old.version = Some("0.9.11".into());
        assert!(side_update(&old, None, &[], true).contains("It also asks for 9 new permissions in AWS, 1 new permission in GitHub."));
        // Its cloud's sign-in ended: the card signs in first (and comes back), not an Update that cannot go on.
        let card = side_update(&old, None, &[], false);
        assert!(card.contains(&format!(r#"<form method="post" action="/{}/signin"><input type="hidden" name="next"><button class="button primary sm">Sign in with {} to update</button></form>"#, old.plane.cloud(), provider_name(old.plane.cloud()))) && !card.contains(r#"action="/plane/update""#), "{card}");
        old.version = Some("0.9.20".into());
        assert!(side_update(&old, None, &[], true).contains("It also asks for 7 new permissions in AWS, 1 new permission in GitHub."), "reading its network, and the network; one request for many spot pools (four); its cache buckets; running a job again");
        // While it updates, on every page: says so (also when the control plane does not answer a moment, or already
        // answers with the new version); once done, that it is.
        let mut u = Update { plane: "p1".into(), steps: &UPDATE_CLOUDFLARE, at: 1, result: None, ended_ms: 0, note: String::new() };
        let mut new = view(status(serde_json::json!({})));
        new.online = false;
        let card = side_update(&new, Some(&u), &[], true);
        assert!(card.contains(&format!("Updating to {DASHBOARD_VERSION}")) && card.contains("It keeps going while you look around.") && card.contains("mini-spin"));
        u.result = Some(Ok(()));
        assert!(side_update(&new, Some(&u), &[], true).contains(&format!("Updated to {DASHBOARD_VERSION}")));
        // Uploaded, and the version before still answers (its cloud is switching it over): said, looked at again, and
        // no Update button meanwhile. Still the old one ten minutes on: the button is back.
        let mut before = view(status(serde_json::json!({})));
        before.version = Some("0.9.20".into());
        u.ended_ms = now_ms();
        let card = side_update(&before, Some(&u), &[], true);
        assert!(card.contains(&format!("Restarting with {DASHBOARD_VERSION}")) && card.contains("data-refresh") && !card.contains(">Update to ") && !card.contains("Updated to"));
        *lock(&d.updating) = Some(Update { plane: "p1".into(), steps: &UPDATE_CLOUDFLARE, at: 2, result: Some(Ok(())), ended_ms: now_ms(), note: String::new() });
        let planes = d.render("planes", &[before.clone()], None);
        assert!(planes.contains("Restarting…") && !planes.contains(">Update to "), "no second update while it restarts");
        u.ended_ms = now_ms() - 11 * 60_000;
        assert!(side_update(&before, Some(&u), &[], true).contains(">Update to ") && side_update(&before, Some(&u), &[], true).contains("Update available"));
        *lock(&d.updating) = None;
        // Its version had them, but its role was found to lack them (made before): the update says so too.
        old.version = Some("0.9.29".into());
        let lacks: Vec<_> = AWS_RUNNER.iter().filter(|n| n.since == "0.9.12").collect();
        // (Beside what came since its version: one request for many spot pools, four permissions, and its cache buckets.)
        assert!(side_update(&old, None, &lacks, true).contains("It also asks for 6 new permissions in AWS.") && side_update(&old, None, &[], true).contains("It also asks for 5 new permissions in AWS."));
    }

    #[test]
    fn public_repositories_are_chosen_in_a_dialog() {
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let v = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "routing": { "rules": [], "public_repos": ["acme/site", "acme/gone"] } })) };
        let list: Vec<String> = ["docs", "site", "a", "b", "c", "d", "e", "f", "g"].iter().map(|n| format!("acme/{n}")).collect();
        let html = limits_card(&v, Some(list.clone()));
        preview("limits", &format!(r#"<div class="cards">{html}</div>"#));
        // One line: which are allowed, and Choose… for the dialog: a box each (allowed ones checked; one allowed and no
        // longer there listed too), a search when there are many, saved all at once.
        assert!(html.contains(r#"<span class="note"><code>site</code>, <code>gone</code></span><button type="button" class="button secondary sm" onclick="document.getElementById('public-repos').showModal()">Choose…</button>"#));
        assert!(html.contains(r#"value="public_set""#) && html.contains(r#"name="repo" value="acme/site" checked>"#) && html.contains(r#"name="repo" value="acme/docs">"#) && html.contains(r#"value="acme/gone" checked"#));
        assert!(html.contains(r#"class="dlg-search""#) && html.contains("9 public · pull requests from forks never run"));
        assert!(html.find(r#"value="acme/gone""#).unwrap() < html.find(r#"value="acme/a""#).unwrap(), "allowed ones first");
        let mut none = v.clone();
        none.status = Some(serde_json::json!({ "routing": { "rules": [] } }));
        assert!(limits_card(&none, Some(vec![])).contains("None of its repositories is public") && limits_card(&none, Some(list)).contains("None allowed"));
        // Not known (an older control plane): typed in.
        assert!(limits_card(&v, None).contains(r#"placeholder="owner/name""#));
    }

    #[test]
    fn a_cost_says_where_it_comes_from() {
        let j = |from: Option<&str>, cloud: &str| serde_json::json!({ "cost_from": from, "cloud": cloud });
        assert_eq!(cost_text(&j(None, "aws"), 0.0123), r#"<span class="est" title="Estimated until it ends; then settled at the prices AWS billed">≈$0.0123</span>"#);
        assert!(cost_text(&j(None, "cloudflare"), 0.0123).contains("Estimated until Cloudflare&#39;s metering has it") || cost_text(&j(None, "cloudflare"), 0.0123).contains("Estimated until Cloudflare's metering has it"));
        assert!(cost_text(&j(Some("measured"), "cloudflare"), 0.0123).contains(r#"title="As Cloudflare metered it">$0.0123<"#));
        assert!(cost_text(&j(Some("prices"), "aws"), 0.5).contains(">$0.50<") && !cost_text(&j(Some("prices"), "aws"), 0.5).contains('≈'));
    }

    #[test]
    fn several_aws_accounts_are_kept_and_each_control_plane_is_acted_on_with_its_own() {
        let dir = std::env::temp_dir().join(format!("superci-accounts-{}", random_token(6)));
        let store = Store::at(&dir);
        let aws = |id: &str, account: &str| Plane::Aws { account_id: account.into(), region: "us-east-1".into(), url: format!("https://{id}.lambda-url.us-east-1.on.aws"), plane_id: id.into(), label: "superci".into() };
        let mut d = Dashboard::new();
        d.store = Some(store.clone());
        // A sign-in to AWS adds an account beside the one there; the same account again is renewed, not doubled.
        d.add_account(aws::Session::for_test("111111111111"));
        d.add_account(aws::Session::for_test("222222222222"));
        d.add_account(aws::Session::for_test("222222222222"));
        assert_eq!(d.aws_accounts().iter().map(|a| a.account_id.as_str()).collect::<Vec<_>>(), ["111111111111", "222222222222"]);
        assert_eq!(d.signed_in_as().as_deref(), Some("AWS account 111111111111, AWS account 222222222222"));
        assert!(d.aws_came && d.aws.as_ref().unwrap().account_id == "222222222222", "the one just signed in to is in hand");
        // Selecting a control plane takes its own account's sign-in in hand: what is written is written with that one.
        d.planes = vec![aws("aaaaaaaaaaaa", "111111111111"), aws("bbbbbbbbbbbb", "222222222222")];
        d.select(0);
        assert!(d.aws.as_ref().unwrap().account_id == "111111111111" && d.own().check(&d.planes[0]).is_ok() && d.own().check(&d.planes[1]).is_err());
        d.select(1);
        assert!(d.aws.as_ref().unwrap().account_id == "222222222222" && d.own().check(&d.planes[1]).is_ok());
        assert!(d.hold_account("111111111111") && !d.hold_account("333333333333") && d.aws_accounts().len() == 2);
        // Kept in one order whichever is in hand, with every control plane known; and loaded again with the kept
        // control plane's own account in hand.
        d.select(1);
        d.keep();
        let kept = store.read();
        assert_eq!((kept.aws.as_ref().map(|a| a.account_id.as_str()), kept.aws_more.len(), kept.plane.as_ref().map(|p| p.plane_id()), kept.planes_more.len()), (Some("111111111111"), 1, Some("bbbbbbbbbbbb"), 1));
        let again = Dashboard::signed_in(Some(store.clone()));
        assert!(again.aws.as_ref().unwrap().account_id == "222222222222" && again.aws_others.len() == 1 && again.planes.len() == 2);
        // The set-up page: each account has a control plane, so another account can be signed in to beside them.
        let actions = again.setup_actions(&again.planes);
        assert!(actions.iter().any(|(cloud, _, action)| *cloud == "aws" && action.as_ref().is_some_and(|a| a.contains("Another AWS account"))));
        // An account with none is offered for setting one up, named in the form.
        let mut third = Dashboard::signed_in(Some(store.clone()));
        third.add_account(aws::Session::for_test("333333333333"));
        assert!(third.setup_actions(&third.planes).iter().any(|(cloud, what, action)| *cloud == "aws" && what.contains("account 333333333333") && action.as_ref().is_some_and(|a| a.contains(r#"name="account" value="333333333333""#))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sign_in_made_by_another_process_is_taken_in_not_undone() {
        let dir = std::env::temp_dir().join(format!("superci-two-{}", random_token(6)));
        let store = Store::at(&dir);
        let session = |workspace: &str| modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: workspace.into() };
        // A dashboard runs, signed in to Cloudflare only (so something is kept).
        store.write(&crate::store::Kept { cloudflare: with_cloudflare(&[("a1", "Acme Inc.")]).cf.clone(), ..Default::default() }).unwrap();
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.keep();
        assert!(d.modal.is_none() && d.cf.is_some());
        // `superci login modal` in a terminal: another process reads the file, adds its sign-in and writes it.
        let mut other = store.read();
        other.modal = Some(session("acme"));
        store.write(&other).unwrap();
        // The dashboard's next page has it, and its next write keeps it (before: the write put the old state back).
        d.take_in();
        assert_eq!(d.modal.as_ref().map(|m| m.workspace.as_str()), Some("acme"));
        d.keep();
        assert_eq!(store.read().modal.map(|m| m.workspace), Some("acme".to_string()));
        // What this process changed itself is not replaced by what another wrote meanwhile: its own is the newer.
        d.modal = Some(session("mine"));
        let mut other = store.read();
        other.modal = Some(session("theirs"));
        store.write(&other).unwrap();
        d.keep();
        assert_eq!(store.read().modal.map(|m| m.workspace), Some("mine".to_string()));
        // `superci login aws` elsewhere: its account is taken in; a second account added there comes after the one in use.
        let mut other = store.read();
        other.aws = Some(aws::Session::for_test("111111111111"));
        store.write(&other).unwrap();
        d.take_in();
        assert_eq!(d.aws.as_ref().map(|a| a.account_id.as_str()), Some("111111111111"));
        let mut other = store.read();
        (other.aws, other.aws_more) = (Some(aws::Session::for_test("222222222222")), vec![aws::Session::for_test("111111111111")]);
        store.write(&other).unwrap();
        d.take_in();
        assert_eq!((d.aws.as_ref().map(|a| a.account_id.as_str()), d.aws_others.iter().map(|a| a.account_id.as_str()).collect::<Vec<_>>()), (Some("111111111111"), vec!["222222222222"]), "the one in use stays in use");
        d.keep();
        // AWS ended the sign-ins, said so by another process (which kept the control plane and the key).
        let mut other = store.read();
        (other.aws, other.aws_more, other.aws_ended) = (None, vec![], true);
        store.write(&other).unwrap();
        d.take_in();
        assert!(d.aws.is_none() && d.aws_others.is_empty() && d.aws_ended);
        d.keep();
        // Cloudflare signed in again elsewhere, and a new key: both taken in.
        let mut other = store.read();
        other.cloudflare = with_cloudflare(&[("a2", "Initech")]).cf.clone();
        other.key = Some(crate::store::Key { name: "DEVICE_KEY_9999999999_ABCDEF".into(), value: "a-new-folder-key-0001".into(), planes: vec!["https://a.example".into()] });
        store.write(&other).unwrap();
        d.take_in();
        assert!(d.cf.is_some() && d.status_key == "a-new-folder-key-0001" && d.keyed.contains("https://a.example"));
        d.keep();
        // `superci logout` elsewhere removes the file: this dashboard is signed out too, and does not write it back.
        store.remove().unwrap();
        d.take_in();
        d.keep();
        assert!(d.modal.is_none() && d.cf.is_none() && d.aws.is_none() && !store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_aws_sign_in_another_superci_renewed_a_moment_ago_is_taken_not_renewed_again() {
        // Two SuperCIs with the same AWS sign-in, its credentials due. One renewed it (AWS takes its refresh token
        // once) and wrote it; the other, about to renew, reads the file again under the lock and uses that one, asking
        // AWS nothing (a second renewal would be refused, and was taken for the sign-in's end).
        let dir = std::env::temp_dir().join(format!("superci-renew-{}", random_token(6)));
        let store = Store::at(&dir);
        store.write(&crate::store::Kept { aws: Some(aws::Session::for_test("111111111111")), ..Default::default() }).unwrap();
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.keep();
        assert!(d.aws.as_ref().is_some_and(|a| a.due()));
        let mut other = store.read();
        let until = now_ms() + 3_600_000;
        other.aws = Some(aws::Session::for_test_until("111111111111", until));
        store.write(&other).unwrap();
        d.aws_renewed();
        assert!(d.aws.as_ref().is_some_and(|a| !a.due()) && !d.aws_ended, "the renewed one, taken in");
        let (_, creds) = d.aws_account("").unwrap();
        assert_eq!(creds.expires_at_ms, until);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_control_plane_is_asked_with_a_key_of_its_own() {
        // The folder's key is never sent as itself: each address gets its own key from it, always the same one.
        let d = Dashboard::new();
        assert!(d.status_key.starts_with(cloudflare::FOLDER_KEY) && d.status_key.len() > 40);
        let (a, b) = (cloudflare::plane_key(&d.status_key, "https://a.example"), cloudflare::plane_key(&d.status_key, "https://b.example/"));
        assert!(a.len() == 64 && a != b && !a.contains(&d.status_key) && a == cloudflare::plane_key(&d.status_key, "https://a.example/"));
        assert_ne!(a, cloudflare::plane_key(&Dashboard::new().status_key, "https://a.example"), "another folder's key opens nothing here");
        // A key that only reads is one control plane's already: sent as it is.
        assert_eq!(cloudflare::plane_key("a-key-that-only-reads-0001", "https://a.example"), "a-key-that-only-reads-0001");
        let dir = std::env::temp_dir().join(format!("superci-keys-{}", random_token(6)));
        let store = Store::at(&dir);
        let name = format!("DEVICE_KEY_{}_ABCDEF", now_ms() / 1000 + 20 * 86_400);
        store.write(&crate::store::Kept { modal: Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() }),
            key: Some(crate::store::Key { name: name.clone(), value: "an-old-shared-key-value-0001".into(), planes: vec!["abcdef123456".into()] }), ..Default::default() }).unwrap();
        // A key kept from before (one value in every control plane) goes on working until every control plane
        // listed has been handed its own: with none listed and one known to have it, nothing changes yet.
        let mut again = Dashboard::signed_in(Some(store.clone()));
        again.renew_folder_key();
        assert!(again.status_key == "an-old-shared-key-value-0001" && again.status_secret == name && again.keyed.contains("abcdef123456"));
        // With no control plane having it, there is nothing to carry: a new one at once.
        again.keyed.clear();
        again.renew_folder_key();
        assert!(again.status_key.starts_with(cloudflare::FOLDER_KEY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_teams_control_plane_is_never_moved_onto_deleted_or_written_with_the_wrong_sign_in() {
        let aws = |id: &str, account: &str| Plane::Aws { account_id: account.into(), region: "us-east-1".into(), url: format!("https://{id}.lambda-url.us-east-1.on.aws"), plane_id: id.into(), label: "superci".into() };
        let seen = |plane: Plane, github: bool, moved_to: Option<&str>, standby: bool, online: bool| PlaneView { plane, online, github, installed: github, aws: true, runners: true, gitlab: false,
            moved_to: moved_to.map(str::to_string), standby, version: Some(superci_core::plane::VERSION.into()), status: None };
        let (ours, theirs, spare) = (aws("aaaaaaaaaaaa", "111111111111"), aws("bbbbbbbbbbbb", "222222222222"), aws("cccccccccccc", "111111111111"));
        // Another team's: GitHub sends it jobs. Not a place to move to, not deleted from beside ours, and not deleted
        // when SuperCI is stopped at ours.
        let live = seen(theirs.clone(), true, None, false, true);
        assert!(live.serves_its_own() && !live.takes_a_move_from(ours.url()));
        // Stopping ours leaves theirs, with the command that stops theirs later; a spare is theirs to move to then.
        assert_eq!(live.goes_with(&ours, true), Err("AWS · 222222222222 · us-east-1 serves a code host of its own and goes on as before (`superci leave --plane=bbbbbbbbbbbb --confirm` stops that one)".to_string()));
        // One that serves nothing yet: a place to move to, deletable, and gone with ours.
        let empty = seen(spare.clone(), false, None, false, true);
        assert!(!empty.serves_its_own() && empty.takes_a_move_from(ours.url()));
        assert_eq!((empty.goes_with(&ours, false), empty.goes_with(&ours, true)), (Ok(true), Ok(false)), "gone when SuperCI is stopped altogether, kept while another CI runs here");
        // The one ours moved from still holds its App (it passes late events on): a move back is allowed, it can be
        // deleted, and it goes with ours. One that stands by for a move under way is no CI of its own either.
        let old = seen(spare.clone(), true, Some(ours.url()), false, true);
        assert!(!old.serves_its_own() && old.goes_with(&ours, false) == Ok(true) && old.goes_with(&ours, true) == Ok(true));
        assert!(seen(ours.clone(), true, Some(spare.url()), false, true).takes_a_move_from(spare.url()), "back to where it came from");
        let standing_by = seen(spare.clone(), true, None, true, true);
        assert!(!standing_by.serves_its_own() && standing_by.takes_a_move_from(ours.url()));
        // One that does not answer is left alone when SuperCI is stopped (it may be anyone's), and said.
        assert!(seen(theirs.clone(), false, None, false, false).goes_with(&ours, false).unwrap_err().contains("did not answer and was left as it is (`superci planes delete bbbbbbbbbbbb --confirm` deletes it)"));

        // A change is written only with the sign-in of the control plane's own account or workspace.
        let own = Own { aws: Some("111111111111".into()), cf: vec!["cf-a".into()], modal: Some("acme".into()) };
        assert!(own.check(&ours).is_ok());
        assert_eq!(own.check(&theirs).unwrap_err(), "This control plane is in AWS account 222222222222, and SuperCI is signed in to 111111111111. Sign in to 222222222222 to change it. Nothing has changed.");
        let cf = |account: &str| Plane::Cloudflare { account_id: account.into(), account_name: "Other Inc.".into(), script: "superci".into(), url: "https://superci.other.workers.dev".into(), plane_id: "dddddddddddd".into(), label: "superci".into() };
        assert!(own.check(&cf("cf-a")).is_ok() && own.check(&cf("cf-b")).unwrap_err().contains("the Cloudflare account Other Inc., which the Cloudflare sign-in kept does not reach"));
        let modal = |workspace: &str| Plane::Modal { workspace: workspace.into(), url: "https://x.modal.run".into(), plane_id: "eeeeeeeeeeee".into(), label: "superci".into() };
        assert!(own.check(&modal("acme")).is_ok() && own.check(&modal("other")).unwrap_err().contains("the Modal workspace other, and SuperCI is signed in to acme"));
        // Not signed in to that cloud at all: the existing "Sign in with … first" says it, not this check.
        assert!(Own::default().check(&theirs).is_ok());
        // The dashboard's own writers carry it: a setting of another account's control plane is refused before any call.
        let writer = Writer { cf: None, aws: None, modal: None, own: own.clone() };
        assert!(writer.put(&theirs, "ROUTING", "{}").unwrap_err().starts_with("This control plane is in AWS account 222222222222") && writer.drop(&theirs, "ROUTING").is_err());
    }

    #[test]
    fn a_second_dashboard_on_another_port_keeps_its_own_tab_and_signs_in_to_aws_there() {
        // A browser keeps cookies per computer name, not per port: each dashboard's key has a name of its own.
        assert_eq!((key_cookie("localhost:8976"), key_cookie("127.0.0.1:8977"), key_cookie("")), ("superci_local".to_string(), "superci_local_8977".to_string(), "superci_local".to_string()));
        let mut d = Dashboard::new();
        assert_eq!(d.port(), 8976);
        d.base = "http://localhost:8977".into();
        let get = |path: &str| Req { method: "GET".into(), path: path.into(), query: vec![], cookie: None, last: None, body: vec![], host: "localhost:8977".into(), origin: None, fetch_site: None };
        // AWS's sign-in comes back to the port this dashboard is on (AWS allows any port of this computer).
        let to_aws = d.handle(&get("/connect/aws")).unwrap();
        assert!(String::from_utf8_lossy(&to_aws.body).contains("127.0.0.1%3A8977%2Foauth%2Fcallback") || to_aws.headers.iter().any(|(_, v)| v.contains("127.0.0.1%3A8977%2Foauth%2Fcallback")), "{}", String::from_utf8_lossy(&to_aws.body));
        // Cloudflare's comes back to 8976 only: said, not tried.
        let to_cf = d.handle(&get("/connect/cloudflare")).unwrap();
        assert!(to_cf.status == 409 && String::from_utf8_lossy(&to_cf.body).contains("needs port 8976") && d.cf_pending.is_none());
        // The first visit sets this port's cookie, and the other port's cookie is not this one's.
        let shared = hub_of(Dashboard::new(), &"k".repeat(32));
        let mut first = get("/");
        first.query = vec![("k".into(), "k".repeat(32))];
        let set = respond(&shared, &first);
        assert!(set.headers.iter().any(|(k, v)| k == "set-cookie" && v.starts_with(&format!("superci_local_8977={}", "k".repeat(32)))));
        assert!(set.headers.iter().any(|(k, v)| k == "location" && v == "/"));
        // A link that names a page goes on to it, without the key.
        first.query.extend([("p".into(), "job".into()), ("id".into(), "42".into())]);
        assert!(respond(&shared, &first).headers.iter().any(|(k, v)| k == "location" && v == "/?p=job&id=42"));
    }

    #[test]
    fn updating_a_control_plane_from_before_the_new_names_moves_its_launchers_and_takes_the_old_names_out() {
        let status = serde_json::json!({ "agents": [{ "cloud": "modal", "url": "https://acme--superci-runners.modal.run" }] });
        let names: Vec<String> = ["AGENTS", "DASHBOARD_KEY_4102444800_AB12CD", "DEVICE_KEY_4102444800_AB12CD", "READ_KEY_4102444800_AGENT", "ROUTING"].iter().map(|s| s.to_string()).collect();
        let (puts, drops) = renames(&status, &names);
        assert_eq!(puts, [("LAUNCHERS".to_string(), r#"[{"cloud":"modal","url":"https://acme--superci-runners.modal.run"}]"#.to_string())]);
        assert_eq!(drops, ["AGENTS", "DASHBOARD_KEY_4102444800_AB12CD"]);
        // None to move: nothing written.
        assert!(renames(&serde_json::json!({ "agents": [] }), &[]).0.is_empty());
        assert_eq!(old_key_name("DEVICE_KEY_4102444800_AB12CD"), "DASHBOARD_KEY_4102444800_AB12CD");
        assert_eq!(key_until("DEVICE_KEY_4102444800_AB12CD"), 4_102_444_800);
        assert_eq!(key_until("DASHBOARD_KEY_4102444800_AB12CD"), 0, "a key kept under the old name is made anew");
    }

    #[test]
    fn it_answers_only_on_this_computers_address_and_changes_only_from_its_own_pages() {
        let shared = hub_of(with_cloudflare(&[("a1", "Acme Inc.")]), &"k".repeat(32));
        let req = |host: &str, origin: Option<&str>| Req { method: "POST".into(), path: "/routing".into(), query: vec![], cookie: Some("k".repeat(32)), last: None, body: b"action=max_cpu&max_cpu=8".to_vec(), host: host.into(), origin: origin.map(str::to_string),
            fetch_site: origin.map(|o| if o == "http://localhost:8976" || o == "null" { "same-origin".to_string() } else { "same-site".to_string() }) };
        let text = |r: Response| (r.status, String::from_utf8_lossy(&r.body).to_string());
        let (status, body) = text(respond(&shared, &req("localhost:8976", Some("http://localhost:3000"))));
        assert!(status == 403 && body.contains("Not from your dashboard"), "{status}");
        let (status, body) = text(respond(&shared, &req("attacker.example:8976", Some("http://attacker.example:8976"))));
        assert!(status == 403 && body.contains("Not this computer"), "{status}");
        // From its own page it goes through (and here finds no control plane to change), its Origin given or, as these
        // pages' no-referrer policy makes browsers send it, "null".
        for origin in ["http://localhost:8976", "null"] {
            let (status, body) = text(respond(&shared, &req("localhost:8976", Some(origin))));
            assert!(!body.contains("Not from your dashboard") && !body.contains("Not this computer"), "{origin}: {status}");
        }
        // Another local page, its browser saying so.
        let mut other = req("localhost:8976", Some("null"));
        other.fetch_site = Some("same-site".into());
        assert_eq!(respond(&shared, &other).status, 403);
    }

    #[test]
    fn github_and_gitlab_are_rows_of_one_list_where_jobs_come_from() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let mut view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "containers": true, "launchers": [], "jobs": [], "app": { "slug": "superci-acme", "owner": "acme", "org": true }, "installations": [{ "account": "acme", "repositories": "all" }] })) };
        let html = d.render("repos", &[view.clone()], None);
        preview("repos", &html);
        // GitHub connected, GitLab one click away (its row's button opens its three steps), more hosts folded as coming.
        assert!(html.contains("Where jobs come from") && html.contains("Jobs from acme (all repositories)") && html.contains("https://github.com/organizations/acme/settings/apps/superci-acme"));
        let dialog = |html: &str, id: &str| { let start = html.find(&format!(r#"<dialog class="report-modal prompt-modal perm-modal" id="{id}""#)).unwrap_or_else(|| panic!("{id}")); html[start..start + html[start..].find("</dialog>").unwrap()].to_string() };
        assert!(html.contains(r#"onclick="document.getElementById('gl-add').showModal()">Connect</button>"#));
        let gl = dialog(&html, "gl-add");
        // Where it is (gitlab.com, or your own: its address then), a token made there with the scopes ticked, pasted here.
        assert!(gl.contains("<h2>Where is your GitLab?</h2>") && gl.contains(r#"name="gl-where" value="public" checked"#) && gl.contains(r#"<div class="perm-field" data-own hidden><input type="text" name="own_url""#));
        assert!(gl.contains(r#"action="/gitlab/connect""#) && gl.contains(r#"<input type="hidden" name="url" value="https://gitlab.com">"#) && gl.contains("scopes=api,create_runner,manage_runner") && gl.contains(r#"type="password" name="token" required"#));
        assert!(gl.contains(r#"aria-label="Connect GitLab""#) && gl.contains(r#"name="which" value="new""#));
        // Several GitLabs: a row each (named by where it is), each with its own settings, and one more to add.
        let mut two = view.clone();
        if let Some(st) = two.status.as_mut() {
            st["gitlab"] = serde_json::json!({ "url": "https://gitlab.com" });
            st["gitlabs"] = serde_json::json!([{ "id": "", "url": "https://gitlab.com", "scopes": ["api", "create_runner", "manage_runner"] }, { "id": "gcorp", "url": "https://gitlab.corp.example", "scopes": ["api", "create_runner"] }]);
            st["permissions"] = serde_json::json!({ "denied": {} });
            st["jobs"] = serde_json::json!([{ "job_id": 5, "run_id": 50, "repo": "corp/api", "state": "done", "cloud": "cloudflare", "at_ms": 1, "provider": "gitlab", "gitlab": "gcorp" }, { "job_id": 6, "run_id": 60, "repo": "acme/app", "state": "done", "cloud": "cloudflare", "at_ms": 1, "provider": "gitlab" }]);
        }
        two.gitlab = true;
        let both = d.render("repos", &[two.clone()], None);
        preview("repos-gitlabs", &both);
        assert!(both.contains("<strong>GitLab · gitlab.com</strong>") && both.contains("<strong>GitLab · gitlab.corp.example</strong>") && both.contains(r#"href="/?p=gitlab&g=gcorp">Settings</a>"#) && both.contains(r#"href="/?p=gitlab&g=">Settings</a>"#));
        assert!(both.contains("<strong>Another GitLab</strong>") && dialog(&both, "gl-add").contains(r#"aria-label="Add a GitLab""#) && both.matches(r#"id="gl-add""#).count() == 1);
        // Each job opens its page here, which links to its own GitLab; each token has its own row under Permissions.
        let jobs = d.render("jobs", &[two.clone()], None);
        assert!(jobs.contains(r#"href="/?p=job&amp;id=5&amp;gl=gcorp""#) && jobs.contains(r#"href="/?p=job&amp;id=6""#), "{jobs}");
        let links: Vec<String> = two.jobs().iter().map(|j| job_link(&with_hosts(&two, j))).collect();
        assert!(links.contains(&"https://gitlab.corp.example/corp/api/-/jobs/5".to_string()) && links.contains(&"https://gitlab.com/acme/app/-/jobs/6".to_string()), "{links:?}");
        let perms = d.render("planes", &[two.clone()], None);
        assert!(perms.contains("<small>gitlab.com: Its token has every scope</small>") && perms.contains("<small>gitlab.corp.example: Its token lacks 1 scope</small>") && perms.contains(r#"href="/?p=gitlab&g=gcorp&open=gl-token""#));
        // A control plane from before it knew several: updated first.
        let before = PlaneView { version: Some("0.9.36".into()), ..two };
        let old = d.render("repos", &[before], None);
        assert!(old.contains("Update the control plane first: several GitLabs need 0.9.37 or newer") && !old.contains(r#"id="gl-add""#));
        // A new token for a connection that is there: made at its GitLab, pasted, saved for that one.
        let token = gitlab_token_dialog("gcorp", "https://gitlab.corp.example");
        assert!(token.contains(r#"name="which" value="gcorp""#) && token.contains(r#"name="url" value="https://gitlab.corp.example""#) && token.contains("https://gitlab.corp.example/-/user_settings/personal_access_tokens?name=superci&amp;scopes=api,create_runner,manage_runner"));
        assert_eq!((safe_id("gcorp"), safe_id("g/../x; rm"), safe_id("")), ("gcorp".to_string(), "gxrm".to_string(), String::new()));
        // Another GitHub organization: the same three steps (where GitHub is, whose jobs, then GitHub makes the App).
        let gh = dialog(&html, "gh-add");
        // Not an owner of the organization: a note to send one, with the App's own permissions and where the
        // control plane is.
        let note = owner_note("acme", "AWS · 123456789012 · us-east-1");
        assert!(note.contains("a private GitHub App owned by acme, with these permissions: self-hosted runners (write), actions (write)") && note.contains("Only an owner of acme can create it") && note.contains("(AWS · 123456789012 · us-east-1)"), "{note}");
        assert!(gh.contains("Not an owner? <a href=") && gh.contains("Copy a note for one</a>") && gh.contains("owned by {org}"));
        assert!(gh.contains(r#"aria-label="Add an organization""#) && gh.contains("<h2>Where is your GitHub?</h2>") && gh.contains("GitHub Enterprise") && gh.contains(r#"<div class="perm-field" data-own hidden><input type="text" name="host""#)
            && gh.contains(r#"name="login" placeholder="your-organization" required"#) && gh.contains(">Continue on GitHub</button>"));
        // Not connected yet: the same dialog, as connecting.
        let fresh = PlaneView { github: false, installed: false, status: Some(serde_json::json!({ "containers": true, "launchers": [], "jobs": [] })), ..view.clone() };
        let first = d.render("repos", &[fresh.clone()], None);
        assert!(first.contains(r#"onclick="document.getElementById('gh-add').showModal()">Connect</button>"#) && dialog(&first, "gh-add").contains(r#"aria-label="Connect GitHub""#) && first.matches(r#"id="gh-add""#).count() == 1);
        assert!(html.contains("More code hosts") && html.contains("Bitbucket") && html.contains(r#"href="/?p=workflows""#) && !html.contains("runs-on: superci"));
        // How to use it is Workflows: only GitHub's steps while only GitHub is connected.
        assert_eq!(section_name("gitlab"), "gitlab");
        let workflows = d.render("workflows", &[view.clone()], None);
        assert!(workflows.contains("runs-on: superci") && !workflows.contains("tags: [superci]") && !workflows.contains(r#"id="use-gh""#) && workflows.contains("Using GitLab too?"));
        view.installed = false;
        let overview = d.render("overview", &[view.clone()], None);
        preview("setup-steps", &overview);
        // An App just made on GitHub: until its installation is seen, the page looks again by itself.
        let mut waiting = with_cloudflare(&[("a1", "Acme Inc.")]);
        waiting.planes = d.planes.clone();
        let mut not_yet = view.clone();
        not_yet.github = false;
        assert!(!waiting.render("overview", &[not_yet.clone()], None).contains("data-refresh"));
        waiting.github_expected = Some(now_ms() + 60_000);
        assert!(waiting.render("overview", &[not_yet.clone()], None).contains(r#"data-refresh="5""#));
        waiting.github_expected = Some(now_ms() - 1);
        assert!(!waiting.render("overview", &[not_yet], None).contains("data-refresh"), "not for ever");
        // Set up says where it stands and has one button to Repositories: connecting is not done on Overview.
        assert!(overview.contains("Connect GitHub or GitLab") && overview.contains(r#"<a class="button primary" href="/?p=repos">Finish on Repositories</a>"#) && !overview.contains("open=gl-add") && !overview.contains(r#"class="pick"#));
        // GitLab connected (jobs link to it): ready.
        view.gitlab = true;
        view.status = Some(serde_json::json!({ "containers": true, "launchers": [], "gitlab": { "url": "https://gitlab.com" },
            "jobs": [{ "job_id": 1977, "run_id": 2366, "repo": "acme/app", "state": "done", "at_ms": now_ms() - 60_000, "provider": "gitlab", "cloud": "cloudflare", "machine_id": "m",
                "installation_id": 0, "runner": null, "runner_id": null, "machine_type": "2cpu-8gb", "error": null, "seen_in_progress": true }] }));
        assert!(view.ready());
        assert_eq!(closest(Some(&view)), "closest to GitHub and GitLab");
        assert!(d.render("jobs", &[view.clone()], None).contains(r#"href="/?p=job&amp;id=1977""#));
        assert_eq!(job_link(&with_hosts(&view, &view.jobs()[0])), "https://gitlab.com/acme/app/-/jobs/1977");
        // Both connected: a switch between them.
        view.installed = true;
        let workflows = d.render("workflows", &[view.clone()], None);
        preview("workflows-both", &workflows);
        // Limits, on Settings: the largest machine, the longest job, and public repositories allowed (none yet).
        let settings = d.render("settings", &[view], None);
        preview("settings-limits", &settings);
        assert!(settings.contains("<strong>Longest job</strong>") && settings.contains(r#"name="max_hours" min="1" max="120" value="6""#), "a job's machine lives six hours unless set");
        assert!(settings.contains("<h2>Limits</h2>") && settings.contains(r#"name="max_cpu" min="1" max="192" value="32""#) && settings.contains(r#"value="public_add""#) && !workflows.contains("tag-chips"));
        assert!(workflows.contains(r#"<input type="radio" name="host" id="use-gh" checked>"#) && workflows.contains("Turn the project on in") && workflows.contains("tags: [superci]"));
    }

    #[test]
    fn jobs_are_also_shown_by_the_run_they_are_of_with_its_time_cost_and_the_job_it_waited_for() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let t = now_ms() - 3_600_000;
        // (A job queued `at` after the hour began, that waited and then ran, in seconds; a price of $3.60 an hour is a
        // tenth of a cent for each second it ran.)
        let job = |id: u64, run: u64, attempt: Option<u64>, name: &str, at: u64, waited: u64, ran: u64, state: &str, conclusion: Option<&str>| serde_json::json!({ "job_id": id, "run_id": run, "attempt": attempt, "repo": "acme/app", "state": state, "at_ms": t + at * 1000,
            "runner": null, "runner_id": null, "installation_id": 1, "cloud": "aws", "machine_id": "i-1", "machine_type": "m7a.large", "error": null, "seen_in_progress": true, "usd_per_hour": 3.6,
            "started_ms": t + (at + waited) * 1000, "ended_ms": if state == "running" { None } else { Some(t + (at + waited + ran) * 1000) }, "cpu": 2, "name": name, "workflow": "Tests", "conclusion": conclusion });
        let jobs = vec![
            // A run of three: lint and a test at once, then an end-to-end job that waited a minute for its machine.
            job(1, 70, Some(1), "Lint", 0, 10, 50, "done", Some("success")), job(2, 70, Some(1), "Unit", 0, 12, 108, "done", Some("success")), job(3, 70, Some(1), "E2E (1/2)", 120, 65, 372, "done", Some("failure")),
            // The failed job run again: the same run's second attempt, apart.
            job(4, 70, Some(2), "E2E (1/2)", 900, 5, 300, "done", Some("success")),
            // One still going, and a job alone in its run (from before attempts were kept).
            job(5, 71, Some(1), "Unit", 1000, 8, 0, "running", None), job(6, 72, None, "Deploy", 2000, 5, 40, "done", Some("success")) ];
        let runs = runs_of(&jobs);
        let said: Vec<(u64, Option<u64>, &str, u64, Option<u64>)> = runs.iter().map(|r| (r["id"].as_u64().unwrap(), r["attempt"].as_u64(), r["state"].as_str().unwrap(), r["jobs"].as_u64().unwrap(), r["took_ms"].as_u64().filter(|_| r["state"] != "running"))).collect();
        assert_eq!(said, vec![(72, None, "passed", 1, Some(45_000)), (71, Some(1), "running", 1, None), (70, Some(2), "passed", 1, Some(305_000)), (70, Some(1), "failed", 3, Some(557_000))]);
        // The run's cost is its machines' together; the job it waited for is the one over last.
        let first = &runs[3];
        assert!((first["usd"].as_f64().unwrap() - 0.53).abs() < 0.001, "{first}");
        assert_eq!(run_last(first), "Last to end: E2E (1/2), 6m 12s after 1m 5s waiting for a machine");
        assert_eq!((run_last(&runs[0]).as_str(), run_last(&runs[1]).as_str()), ("", ""), "one job: nothing to compare");
        assert_eq!((first["link"].as_str(), runs[2]["link"].as_str()), (Some("https://github.com/acme/app/actions/runs/70"), Some("https://github.com/acme/app/actions/runs/70/attempts/2")));
        assert_eq!(run_names(&runs[2]), ("Tests".to_string(), "app · attempt 2".to_string()));
        let words = run_words(first);
        assert_eq!((words["state"].as_str(), words["took"].as_str(), words["jobs"].as_u64(), words["last"]["job"].as_str(), words["job_ids"].to_string()), (Some("failed"), Some("9m 17s"), Some(3), Some("E2E (1/2)"), "[1,2,3]".to_string()));
        // The Jobs page has them under Runs, beside the jobs one by one.
        let status = serde_json::json!({ "containers": true, "launchers": [], "aws": { "account_id": "1", "region": "us-east-1" }, "jobs": jobs });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let page = d.render("runs", &[view.clone()], None);
        preview("runs", &page);
        assert!(page.contains(r#"<a class="tab" role="tab" href="/?p=runs" aria-selected="true">Runs</a><a class="tab" role="tab" href="/?p=jobs" aria-selected="false">Jobs</a>"#) && page.contains("<h2>Recent runs</h2>"), "{page}");
        assert!(page.contains(r#"<small class="jt-why held">Last to end: E2E (1/2), 6m 12s after 1m 5s waiting for a machine</small></span><span class="jt-runner count">3 jobs</span><span class="jt-num">9m 17s</span><span class="jt-num">$0.53</span><span class="jt-state bad"><i></i>Failed</span>"#), "{page}");
        assert!(page.contains(r#"<span class="jt-runner count">1 job</span>"#) && page.contains(r#"<span class="jt-state accent"><i></i>Running</span>"#) && page.contains("app · attempt 2"));
        let jobs_page = d.render("jobs", &[view], None);
        assert!(jobs_page.contains(r#"href="/?p=jobs" aria-selected="true">Jobs</a>"#) && jobs_page.contains("<h2>Recent jobs</h2>"));
        assert!(Dashboard::sidebar("runs").contains(r#"<a href="/?p=runs" aria-current="true">"#) && Dashboard::sidebar("jobs").contains(r#"<a href="/?p=runs" aria-current="true">"#));
    }

    #[test]
    fn a_jobs_start_is_said_step_by_step_and_a_machine_that_was_not_the_first_choice_is_noted() {
        let t = 1_790_000_000_000u64;
        // GitHub made it 2 s before SuperCI heard; AWS had no spot machine of the first type in three zones (0.8 s
        // each) nor of the second, and started the third; the machine's kernel started 6 s after it was asked for and
        // took 6.6 s to reach the script; the runner's files took 37 s to read; the runner listened 2 s after.
        let ask = |at: u64, kind: &str, zone: &str, said: &str| serde_json::json!({ "at": at, "kind": kind, "zone": zone, "spot": true, "said": said, "ms": 800 });
        let j = serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "running", "cloud": "aws", "machine_id": "i-1", "machine_type": "m7a.8xlarge", "cpu": 32, "at_ms": t, "queued_ms": t - 2_000,
            "launched_ms": t + 4_000, "started_ms": t + 4_000 + 6_000 + 6_600 + 1_000 + 37_000 + 2_000 + 1_500,
            "asks": [ask(0, "m8a.8xlarge", "us-east-1a", "InsufficientInstanceCapacity"), ask(800, "m8a.8xlarge", "us-east-1b", "InsufficientInstanceCapacity"), ask(1600, "m8a.8xlarge", "us-east-1c", "InsufficientInstanceCapacity"),
                ask(2400, "r8a.8xlarge", "us-east-1a", "InsufficientInstanceCapacity"), ask(3200, "m7a.8xlarge", "us-east-1d", "started")],
            "machine": { "boot_s": 6.6, "script_ms": t + 16_600, "read_ms": t + 17_600, "ready_ms": t + 54_600, "listening_ms": t + 56_600 } });
        assert_eq!(job_start_lines(&j), vec![
            "Its start: 1m from queued to running",
            "    2.0 s  GitHub told SuperCI of the job",
            "    4.0 s  AWS gave a machine (no spot m8a.8xlarge in 3 zones; no spot r8a.8xlarge; spot m7a.8xlarge in us-east-1d)",
            "    6.0 s  the machine was made and switched on",
            "    6.6 s  it booted",
            "    1.0 s  its disk and Docker were set up",
            "     37 s  the runner's files were read from the image",
            "    2.0 s  the runner started and reached GitHub",
            "    1.5 s  GitHub gave it the job"]);
        let words = job_words(&j);
        assert_eq!((words["start"]["ms"].as_u64(), words["start"]["steps"].as_array().map(|s| s.len()), words["start"]["steps"][5]["ms"].as_u64()), (Some(60_100), Some(8), Some(37_000)));
        // Its row says, quietly, that its machine was not the first choice (no fault of anyone's; it changes the job's time and price).
        assert_eq!(words["choice"], "Not the first choice: AWS had no spot m8a.8xlarge in us-east-1a or r8a.8xlarge in us-east-1a");
        assert!(job_table(&[j.clone()]).contains(r#"<small class="jt-why held">Not the first choice: AWS had no spot m8a.8xlarge in us-east-1a or r8a.8xlarge in us-east-1a</small>"#));
        // With the price AWS's spot pool had when it was asked: why a pricier machine was taken.
        let mut priced = j.clone();
        priced["asks"][0]["usd"] = serde_json::json!(0.135);
        assert!(job_words(&priced)["choice"].as_str().unwrap().starts_with("Not the first choice: AWS had no spot m8a.8xlarge in us-east-1a ($0.135 an hour)"), "{}", job_words(&priced)["choice"]);
        // A job that got its first choice, and one from before any of this was kept, say what there is.
        let mut first = j.clone();
        first["asks"] = serde_json::json!([ask(0, "m8a.8xlarge", "us-east-1a", "started")]);
        assert!(job_words(&first)["choice"].is_null() && !job_table(&[first]).contains("Not the first choice"));
        let before = serde_json::json!({ "job_id": 8, "state": "done", "cloud": "aws", "machine_id": "i-2", "machine_type": "m8a.xlarge", "at_ms": t, "launched_ms": t + 3_400, "started_ms": t + 16_500 });
        assert_eq!(job_start_lines(&before), vec!["Its start: 17 s from queued to running", "    3.4 s  AWS gave a machine", "     13 s  its machine booted and its runner took the job"]);
        assert!(job_start_lines(&serde_json::json!({ "job_id": 9, "state": "waiting", "at_ms": t })).is_empty());
        // One that waited for room under the account's spot limit first: that wait said apart from AWS's part.
        let waited = serde_json::json!({ "job_id": 10, "state": "done", "cloud": "aws", "machine_id": "i-3", "machine_type": "c8a.4xlarge", "at_ms": t, "spot_waited_ms": 67_500, "launched_ms": t + 70_000, "started_ms": t + 80_000 });
        assert_eq!(job_start_lines(&waited), vec!["Its start: 1m 20s from queued to running", "    1m 7s  waited for room under your AWS spot limit", "    2.5 s  AWS gave a machine", "     10 s  its machine booted and its runner took the job"]);
        // The first choice had no spot machine, and the others were asked in one request: what AWS chose among them.
        let mut many = j.clone();
        many["asks"] = serde_json::json!([ask(0, "m8a.8xlarge", "us-east-1a", "InsufficientInstanceCapacity"), { "at": 800, "kind": "r8a.8xlarge", "zone": "us-east-1d", "spot": true, "said": "started", "ms": 2_900, "pools": 14 }]);
        many["machine_type"] = "r8a.8xlarge".into();
        assert_eq!(job_start_lines(&many)[2], "    4.0 s  AWS gave a machine (no spot m8a.8xlarge; spot r8a.8xlarge in us-east-1d, AWS's choice of 14 pools)");
        assert_eq!(job_words(&many)["choice"], "Not the first choice: AWS had no spot m8a.8xlarge in us-east-1a");
        // None of them had one either: said once, and the job went on to an on-demand machine.
        many["asks"] = serde_json::json!([ask(0, "m8a.8xlarge", "us-east-1a", "InsufficientInstanceCapacity"), { "at": 800, "kind": "", "spot": true, "said": "InsufficientInstanceCapacity", "ms": 2_900, "pools": 14 },
            { "at": 3_700, "kind": "m8a.8xlarge", "zone": "us-east-1a", "spot": false, "said": "started", "ms": 900 }]);
        (many["machine_type"], many["on_demand"]) = ("m8a.8xlarge".into(), true.into());
        assert_eq!(job_start_lines(&many)[2], "    4.0 s  AWS gave a machine (no spot m8a.8xlarge; no spot machine in 14 other pools; on-demand m8a.8xlarge in us-east-1a)");
        // A run from a computer's: the runner reaches the control plane, which gives it the job; one that has only
        // listened so far; a GitHub job's runner that took its job before it said it listened.
        let mut local = j.clone();
        local["provider"] = "local".into();
        let lines = job_start_lines(&local);
        assert_eq!(&lines[lines.len() - 2..], ["    2.0 s  the runner started and reached the control plane", "    1.5 s  the control plane gave it the job"], "{lines:?}");
        local["started_ms"] = serde_json::Value::Null;
        assert!(job_start_lines(&local).last().unwrap().ends_with("the runner started and reached the control plane"));
        let mut listening = j.clone();
        listening["started_ms"] = serde_json::Value::Null;
        assert!(job_start_lines(&listening).last().unwrap().ends_with("the runner started and reached GitHub"));
        let mut quiet = j.clone();
        quiet["machine"]["listening_ms"] = serde_json::Value::Null;
        assert!(job_start_lines(&quiet).last().unwrap().ends_with("the runner started and took the job"));
        quiet["started_ms"] = serde_json::Value::Null;
        assert!(job_start_lines(&quiet).last().unwrap().ends_with("the runner's files were read from the image"));
        // A runner that was slow to reach GitHub: what it logged meanwhile comes after the steps.
        let mut slow = j.clone();
        slow["machine"]["log"] = "[2026-10-13 22:56:11Z INFO ConfigurationStore] HasCredentials()\n... 30 s ...\n[2026-10-13 22:56:41Z WARN GitHubActionsService] Attempt 1 of POST request failed".into();
        let lines = job_start_lines(&slow);
        assert_eq!(lines[lines.len() - 4..], ["What its runner logged while it was slow to reach GitHub (its waits, warnings and errors):", "  [2026-10-13 22:56:11Z INFO ConfigurationStore] HasCredentials()", "  ... 30 s ...", "  [2026-10-13 22:56:41Z WARN GitHubActionsService] Attempt 1 of POST request failed"]);
        assert!(job_words(&slow)["start"]["runner_log"].as_str().is_some_and(|l| l.contains("... 30 s ...")) && job_words(&j)["start"].get("runner_log").is_none());
    }

    #[test]
    fn overview_says_how_jobs_went_what_needs_a_look_and_where_they_ran() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let t = now_ms() - 3_600_000;
        let job = |id: u64, state: &str, cloud: &str, wait: Option<u64>, took: u64, error: Option<&str>| { let t = t - (id % 4) * 3 * 3_600_000; serde_json::json!({ "job_id": id, "run_id": 0, "repo": "acme/app", "state": state, "at_ms": t,
            "runner": null, "runner_id": null, "installation_id": 1, "cloud": cloud, "machine_id": if cloud.is_empty() { serde_json::Value::Null } else { "m".into() }, "machine_type": "4cpu-12gb", "error": error, "seen_in_progress": wait.is_some(),
            "usd_per_hour": 0.4, "started_ms": wait.map(|w| t + w), "ended_ms": wait.map(|w| t + w + took), "launched_ms": t, "cpu": 4,
            "name": (["", "test", "lint", "build", "e2e", "deploy", "e2e"][id as usize % 7]), "workflow": "ci" }) };
        let status = serde_json::json!({ "containers": true, "launchers": [], "aws": { "account_id": "1", "region": "us-east-1" }, "jobs": [
            job(1, "done", "cloudflare", Some(8_000), 21_000, None), job(2, "done", "cloudflare", Some(9_000), 30_000, None), job(3, "done", "aws", Some(45_000), 240_000, None),
            job(4, "failed", "cloudflare", None, 0, Some("runner container: 502 the container cannot be kept running: JsValue(Error: Container connection temporarily unavailable)")),
            job(5, "waiting", "", None, 0, Some("waiting: Cloudflare runs 20 at once")), job(6, "failed", "cloudflare", None, 0, Some("runner container: 502 the container cannot be kept running")) ] });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let html = d.render("overview", &[view], None);
        preview("overview-new", &html);
        // The control plane is one quiet chip: no address, version or label.
        assert!(!html.contains("plane-chip") && !html.contains("superci.acme.workers.dev") && !html.contains("runs-on: superci") && !html.contains("Copy"));
        // (Jobs of no run, as here: listed as jobs; see the next test for runs.)
        // Three figures (no "did not run" headline, no count in the footer); All jobs as a button.
        assert!(!html.contains("jobs did not run") && html.contains(r#"<div class="kpi-num">6</div><div class="kpi-cap">jobs</div>"#) && html.contains(r#"<div class="kpi-sub">3 ran<span class="sep">·</span><span class="bad-text">2 did not</span>"#)
            && html.matches(r#"<svg class="spark""#).count() == 3 && html.contains("<h2>Latest</h2>") && html.contains(">9 s</div>"));
        assert!(html.contains(r#"<a class="chip" href="/?p=jobs">All jobs ›</a>"#) && !html.contains("jobs in the last day</span>"));
        assert!(html.contains("this hour: ") && html.contains(r#"class="vs""#) && !html.contains(r#"class="ring"#));
        // What went wrong is said in each job's row, in plain words (no separate list).
        assert!(!html.contains("Needs a look") && html.contains(r#"jt-why bad">Cloudflare could not keep its container running"#) && !html.contains("JsValue") && !html.contains("502"));
        assert!(html.contains(r#"jt-why open">Cloudflare runs 20 at once"#));
        // Latest: a table of names, runners in words, how each ended.
        assert!(html.contains(r#"<span class="jt-job"><strong>test</strong><small>"#) && html.contains("app · ci</small>") && html.contains("<span>4 CPU · 12 GB</span>") && html.contains(r#"<span class="jt-state good"><i></i>Done</span>"#));
        assert!(!html.contains("job 1 ·") && !html.contains("cloudflare 4cpu-12gb"));
        // Where they ran: a split only when there is more than one place.
        assert!(html.contains("Where they ran") && html.contains("2 jobs · ") && html.contains("1 job · ") && !html.contains("Where jobs ran"));
        // Every page waits in its own shape, the loader inside its first card.
        for section in ["overview", "repos", "runners", "workflows", "settings", "jobs", "planes", "changes", "add-runners"] {
            let page = skeleton(section, section_title(section));
            preview(&format!("skeleton-{section}"), &page);
            assert!(page.contains("Reading your control plane"), "{section}");
        }
    }

    #[test]
    fn overview_lists_the_latest_runs_and_counts_runs_when_the_control_plane_does() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let t = now_ms() - 3_600_000;
        let job = |id: u64, run: u64| serde_json::json!({ "job_id": id, "run_id": run, "repo": "acme/app", "state": "done", "conclusion": "success", "at_ms": t, "started_ms": t + 5_000, "ended_ms": t + 65_000,
            "launched_ms": t, "cloud": "cloudflare", "machine_id": "m", "machine_type": "4cpu-12gb", "cpu": 4, "usd_per_hour": 0.4, "name": format!("job{id}"), "workflow": "ci", "installation_id": 1, "runner": null, "runner_id": null, "error": null });
        let usage = |runs: bool| { let mut u = serde_json::json!({ "jobs": 3, "ran": 3, "not_ran": 0, "machine_minutes": 3.0, "billed_minutes": 3, "usd": 0.02, "host_usd": 0.05, "steps": (0..24).map(|i| serde_json::json!({ "ran": if i == 23 { 3 } else { (i == 22) as u64 }, "not_ran": (i == 22) as u64, "runs": if i == 23 { 2 } else { 0 }, "minutes": 0.0, "usd": 0.0, "host_usd": 0.0 })).collect::<Vec<_>>() }); if runs { u["runs"] = 2.into() } u };
        let view = |runs: bool| PlaneView { plane: plane.clone(), online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "containers": true, "launchers": [], "jobs": [job(1, 7), job(2, 7), job(3, 8)], "usage": { "day": usage(runs), "week": usage(runs), "month": usage(runs) } })) };
        let html = d.render("overview", &[view(true)], None);
        preview("overview-runs", &html);
        // Runs counted, their jobs beside them; the latest runs listed, all of them a click away.
        assert!(html.contains(r#"<div class="kpi-num">2</div><div class="kpi-cap">runs</div>"#) && html.contains(r#"<div class="kpi-sub">3 jobs</div>"#), "{html}");
        assert!(html.contains(r#"<a class="chip" href="/?p=runs">All runs ›</a>"#) && html.matches(r#"class="jt-row" href="/?p=run&amp;"#).count() == 2, "{html}");
        // A control plane that does not count runs yet: jobs, as before; an hour of jobs that ran and one that did not, one
        // bar on the other.
        let older = d.render("overview", &[view(false)], None);
        assert!(older.contains(r#"<div class="kpi-num">3</div><div class="kpi-cap">jobs</div>"#));
        assert!(older.contains(r#"<title>an hour ago: 1 ran, 1 did not</title>"#) && older.contains(r#"<rect class="b" x="222.0" y="12.5" "#), "{older}");
        // Runs is the page in the sidebar, its Runs tab first.
        assert!(Dashboard::sidebar("overview").contains(r#"<a href="/?p=runs" aria-current="false">"#) && Dashboard::sidebar("overview").contains("<span>Runs</span>"));
    }

    #[test]
    fn a_move_brings_the_runner_providers_and_an_empty_overview_shows_its_shape() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let cf = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let aws_plane = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "p2".into(), label: "superci".into() };
        let from = PlaneView { plane: cf, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "launchers": [{ "cloud": "modal", "url": "https://m" }], "aws": { "account_id": "123456789012", "region": "us-east-1" }, "jobs": [] })) };
        let to = PlaneView { plane: aws_plane, online: true, github: false, installed: false, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "jobs": [] })) };
        // Cloudflare's containers come (a launcher there for the new one); Modal needs a sign-in first; AWS is the new one's own account.
        assert_eq!(d.carry_plan(&from, &to), vec![Carry::Comes("cloudflare"), Carry::NeedsSignIn("modal"), Carry::Comes("aws")]);
        assert!(d.move_dialog(0, &from, &to).contains(r#"<button class="button primary sm" disabled>Move</button>"#), "not before the sign-in");
        d.modal = Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() });
        assert!(d.move_dialog(0, &from, &to).contains("Cloudflare runners") && d.move_dialog(0, &from, &to).contains(r#"<button class="button primary sm">Move</button>"#));
        // No jobs yet: the page as it will be, with nothing in it (the day's figures at zero, then the latest jobs).
        let html = d.render("overview", &[from], None);
        assert!(html.contains("No jobs yet") && html.contains(r#"<div class="kpi-num">$0.00</div>"#) && html.contains("<h2>Latest</h2>") && !html.contains("$-0"));
        assert_eq!(usd(-0.0), "$0.00");
    }

    #[test]
    fn a_control_plane_in_a_cloud_not_signed_in_to_stays_the_one_in_use() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        d.modal = Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() });
        let aws_url = "https://abc.lambda-url.us-east-1.on.aws";
        let cf = PlaneView { plane: Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() },
            online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: Some(aws_url.into()), standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "jobs": [] })) };
        let modal_plane = PlaneView { plane: Plane::Modal { workspace: "acme".into(), url: "https://acme--superci-plane-xyz.modal.run".into(), plane_id: "p2".into(), label: "superci".into() },
            online: true, github: false, installed: false, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "jobs": [] })) };
        let seen = PlaneView { plane: Plane::Seen { url: aws_url.into(), plane_id: "p3".into() }, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: None };
        let views = [cf, modal_plane, seen];
        assert_eq!(view::in_use(&views), 2, "the one it moved to, though this session cannot read it");
        // Changelog compares with the one in use only (one not in use may be older).
        let mut older = views.clone();
        older[0].version = Some("0.1.0".into());
        assert!(!d.render("changes", &older, None).contains("Not on your control plane yet"));
        let html = d.render("overview", &views, None);
        preview("gate-seen", &html);
        assert!(html.contains("<h2>Your control plane is in AWS</h2>") && html.contains("It moved there from Cloudflare.") && html.contains(r#"href="/connect/aws""#) && html.contains("data-gated"));
        // One not heard from yet is loading (not "set up"); one silent for long says so.
        let mut quiet = views[1].clone();
        quiet.online = false;
        d.first_asked.insert("p2".into(), now_ms());
        let html = d.render("overview", &[quiet.clone()], None);
        assert!(html.contains(r#"<div data-refresh="2"></div>"#) && !html.contains("Set up") && !html.contains("starting"));
        d.first_asked.insert("p2".into(), now_ms() - 60_000);
        let html = d.render("overview", &[quiet], None);
        assert!(html.contains("Your control plane is not answering") && !html.contains("4 steps"));
        // A control plane that did not answer just now (its Worker restarting after a setting changed) keeps its last
        // full view for a minute: its runners do not seem to vanish.
        let mut good = views[1].clone();
        good.aws = true;
        d.remember(vec![good]);
        let mut blip = views[1].clone();
        (blip.online, blip.aws, blip.status) = (false, false, None);
        let after = d.remember(vec![blip]);
        assert!(after[0].online && after[0].aws && after[0].status.is_some());
        let html = d.render("planes", &views, None);
        // Not signed in to AWS (where the one in use runs): one button for that, and no move until then.
        assert!(!html.contains("sign in there") && !html.contains("Move here</button>") && html.contains(r#"action="/aws/signin"><input type="hidden" name="next"><button class="button secondary sm">Sign in with AWS</button>"#));
    }

    #[test]
    fn pages_wait_in_the_shape_of_what_is_coming() {
        preview("skeleton", &String::from_utf8(Dashboard::page("overview").body).unwrap().split(r#"id="live">"#).nth(1).unwrap().split("</main>").next().unwrap().to_string());
        let page = String::from_utf8(Dashboard::page("overview").body).unwrap();
        assert!(page.contains("<h2>Usage</h2>") && page.contains("Reading your control plane") && page.contains(r#"aria-label="Period""#) && page.contains(r#"<div class="kpis four">"#));
    }

    #[test]
    fn a_finished_deploy_joins_the_dashboard() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        d.show_setup = true;
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let made = plane.clone();
        d.start_deploy("cloudflare", "Acme Inc.".into(), &cloudflare::DEPLOY_STEPS, vec![], move |step| { step(1); step(2); Ok(made) });
        for _ in 0..200 { if lock(&d.deploying).as_ref().is_some_and(|x| x.result.is_some()) { break } std::thread::sleep(Duration::from_millis(5)) }
        assert!(!d.deploy_running());
        d.adopt_deploy();
        assert_eq!(d.planes, vec![plane]);
        assert!(lock(&d.deploying).is_none() && !d.show_setup);
        // A stopped deploy stays, to show why and to try again.
        d.start_deploy("cloudflare", "Acme Inc.".into(), &cloudflare::DEPLOY_STEPS, vec![], |step| { step(1); Err("no".into()) });
        for _ in 0..200 { if lock(&d.deploying).as_ref().is_some_and(|x| x.result.is_some()) { break } std::thread::sleep(Duration::from_millis(5)) }
        d.adopt_deploy();
        assert!(lock(&d.deploying).as_ref().is_some_and(|x| x.at == 1) && d.planes.len() == 1);
    }

    #[test]
    fn a_deploy_shows_its_steps_and_how_it_stopped() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        *lock(&d.deploying) = Some(Deploy { cloud: "cloudflare", place: "Acme Inc.".into(), steps: &cloudflare::DEPLOY_STEPS, at: 1, form: vec![("account", "a1".into())], result: None });
        // A deploy has its own page; Overview says it in a line, with a link there.
        let html = d.render("plane", &[], None);
        preview("deploying", &html);
        assert!(html.contains("Setting up your control plane") && html.contains("Deploying to Cloudflare") && html.contains(r#"data-refresh="1""#));
        assert!(html.contains(r#"<li class="done">"#) && html.contains(r#"<li class="now">"#) && !html.contains("More clouds"));
        let overview = d.render("overview", &[], None);
        preview("deploying-overview", &overview);
        assert!(overview.contains("Deploying to Cloudflare · Acme Inc.") && overview.contains(r#"href="/?p=plane">See its progress</a>"#) && !overview.contains(r#"<ol class="progress">"#));

        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        *lock(&d.deploying) = Some(Deploy { cloud: "aws", place: "account 123456789012 · us-east-1".into(), steps: &aws_plane::DEPLOY_STEPS, at: 2, form: vec![("region", "us-east-1".into()), ("plane", "abcdef123456".into())],
            result: Some(Err("CreateFunction: AccessDenied".into())) });
        let html = d.render("plane", &[], None);
        preview("stopped", &html);
        assert!(html.contains("The deploy to AWS stopped") && html.contains(r#"<li class="failed">"#) && html.contains(r#"name="region" value="us-east-1""#) && html.contains(r#"name="plane" value="abcdef123456""#) && !html.contains("data-refresh"));
        assert!(d.render("overview", &[], None).contains(r#"href="/?p=plane">See why</a>"#));
        // Once a deploy has finished, the page that showed it goes on to Overview, once.
        *lock(&d.deploying) = None;
        *lock(&d.deployed) = true;
        assert!(d.render("plane", &[], None).contains(r#"<div data-go="/"></div>"#));
        assert!(d.render("plane", &[], None).contains("Set up a control plane"));
    }

    #[test]
    fn a_cloudflare_sign_in_another_superci_just_renewed_is_taken_not_renewed_again() {
        let dir = std::env::temp_dir().join(format!("superci-renew-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let store = Store::at(&dir);
        // Two SuperCIs on one folder, both with the sign-in as it was (due to be renewed, here with nothing to renew it).
        let mut first = Dashboard::signed_in(Some(store.clone()));
        first.cf = Some(cloudflare::Session::expired_for_test());
        first.keep();
        let mut second = Dashboard::signed_in(Some(store.clone()));
        assert!(second.cf.as_ref().is_some_and(|s| s.due()));
        // The first renews it and writes it (as a renewal does).
        first.cf = Some(cloudflare::Session::from_token("renewed"));
        first.keep();
        // The second, its own due, takes the renewed one from the file instead of renewing (which would end it).
        assert!(second.cf_client().is_ok());
        assert!(second.cf.as_ref().is_some_and(|s| !s.due()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_ins_are_kept_in_supercis_own_folder() {
        let dir = std::env::temp_dir().join(format!("superci-kept-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let store = Store::at(&dir);
        // Signed in nowhere: nothing is kept, and a command says what a person must do first.
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.keep();
        assert!(d.signed_in_as().is_none() && !store.path().exists());
        assert_eq!(d.current().err().unwrap(), NOT_SIGNED_IN);
        // A sign-in made in the dashboard is kept; the next start has it, with the same key for its control planes.
        d.aws = Some(aws::Session::for_test("123456789012"));
        d.modal = Some(modal::Session { token_id: "ak-test".into(), token_secret: "as-test".into(), workspace: "acme".into() });
        d.keyed.insert("abcdef123456".into());
        d.planes.push(Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "abcdef123456".into(), label: "superci".into() });
        d.keep();
        assert!(key_until(&d.status_secret) > now_ms() / 1000 + 29 * 86_400, "a kept key lasts thirty days");
        let again = Dashboard::signed_in(Some(store.clone()));
        assert_eq!(again.signed_in_as().unwrap(), "AWS account 123456789012, Modal workspace acme");
        assert!(again.status_key == d.status_key && again.status_secret == d.status_secret && again.keyed.contains("abcdef123456"));
        assert_eq!(again.kept_plane.as_ref().map(|p| p.plane_id()), Some("abcdef123456"));
        // A sign-in given by name for one run is used, and not kept.
        let mut given = Dashboard::signed_in(Some(store.clone()));
        given.cf = Some(cloudflare::Session::from_token("t"));
        given.cf_given = true;
        given.keep();
        assert!(given.signed_in_as().unwrap().contains("Cloudflare (a token given for this run)") && store.read().cloudflare.is_none());
        // A key about to end is not taken up again.
        let mut kept = store.read();
        kept.key.as_mut().unwrap().name = format!("DEVICE_KEY_{}_ABCDEF", now_ms() / 1000 + 3600);
        store.write(&kept).unwrap();
        assert!(Dashboard::signed_in(Some(store.clone())).status_key != d.status_key);
        // Signed in nowhere any more (a sign-in that ended): nothing is left on this machine.
        d.aws = None;
        d.modal = None;
        d.keep();
        assert!(!store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn days_are_said_as_dates() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_791_331_200), "2026-10-07");
        assert_eq!(day(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn a_command_does_what_the_page_does() {
        // What a page would say is what the command says: a refusal as an error, with its text and no markup.
        let mut d = Dashboard::new();
        assert_eq!(d.act("/plane/leave", &[("confirm", "nope".into())]).unwrap_err(), "Type superci to confirm");
        assert_eq!(d.act("/plane/delete", &[("plane", "abcdef123456".into())]).unwrap_err(), "That control plane is not known here");
        assert_eq!(d.act("/routing", &[("action", "order".into())]).unwrap_err(), "no control plane yet");
        assert_eq!(d.act("/nowhere", &[]).unwrap_err(), "Not found");
        assert_eq!(d.ready().unwrap_err(), NOT_SIGNED_IN);
        assert_eq!(plain(r#"Its control planes are deleted.</p><ul class="checks"><li>Delete the App <a href="https://github.com/x">superci-acme</a> on GitHub.</li><li>Change <code>runs-on: superci</code> back &amp; push.</li></ul><p>"#),
            "Its control planes are deleted. Delete the App superci-acme on GitHub. Change runs-on: superci back & push.");
        // What runs in the background is waited for, each step said as it begins.
        let plane = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "abcdef123456".into(), label: "superci".into() };
        *lock(&d.deploying) = Some(Deploy { cloud: "aws", place: "account 123456789012 · us-east-1".into(), steps: &aws_plane::DEPLOY_STEPS, at: 6, form: vec![], result: Some(Ok(plane.clone())) });
        let mut steps = vec![];
        let said = d.wait(Background::Deploy, &mut |s: &str| steps.push(s.to_string())).unwrap();
        assert!(said.starts_with("The control plane is running: https://x.lambda-url.us-east-1.on.aws") && steps.len() == aws_plane::DEPLOY_STEPS.len());
        assert_eq!(d.plane_in_use().unwrap().plane_id(), "abcdef123456");
        // A second one joins the list; the one in use stays the one in use (and so the one kept).
        let second = Plane::Modal { workspace: "acme".into(), url: "http://127.0.0.1:9".into(), plane_id: "second123456".into(), label: "superci".into() };
        *lock(&d.deploying) = Some(Deploy { cloud: "modal", place: "workspace acme".into(), steps: &modal::DEPLOY_STEPS, at: 3, form: vec![], result: Some(Ok(second)) });
        d.wait(Background::Deploy, &mut |_| {}).unwrap();
        assert_eq!((d.planes.len(), d.plane_in_use().unwrap().plane_id().to_string()), (2, "abcdef123456".to_string()));
        *lock(&d.updating) = Some(Update { plane: "abcdef123456".into(), steps: &UPDATE_AWS, at: 2, result: Some(Err("AccessDenied: lambda:UpdateFunctionCode".into())), ended_ms: now_ms(), note: String::new() });
        let mut steps = vec![];
        assert_eq!(d.wait(Background::Update, &mut |s: &str| steps.push(s.to_string())).unwrap_err(), "AccessDenied: lambda:UpdateFunctionCode");
        assert_eq!(steps, UPDATE_AWS[..3], "a stop says the steps up to where it stopped");
        // An update that left a part out (its launcher in Cloudflare, whose sign-in is over) says so with its
        // result, to a command and on the pages: the rest was updated.
        let left = "Its Cloudflare launcher was not updated: the Cloudflare sign-in has expired. Sign in again (`superci login cloudflare`) and update once more.";
        *lock(&d.updating) = Some(Update { plane: "abcdef123456".into(), steps: &UPDATE_AWS, at: 3, result: Some(Ok(())), ended_ms: now_ms() - 11 * 60_000, note: left.into() });
        assert_eq!(d.wait(Background::Update, &mut |_| {}).unwrap(), format!("Updated to {DASHBOARD_VERSION}. {left}"));
        let updated = PlaneView { plane: d.plane_in_use().unwrap(), online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(DASHBOARD_VERSION.into()), status: None };
        let card = side_update(&updated, lock(&d.updating).as_ref(), &[], true);
        assert!(card.contains(&format!("Updated to {DASHBOARD_VERSION}")) && card.contains("Its Cloudflare launcher was not updated: the Cloudflare sign-in has expired."), "{card}");
        assert_eq!(d.wait(Background::Move, &mut |_| {}).unwrap_err(), "no move was started");
    }

    #[test]
    fn what_a_person_does_in_the_browser() {
        // A sign-in with one cloud goes straight to it, and is done once that cloud is signed in to.
        let mut cf = Dashboard::new().for_task(Task::Login(Some("cloudflare")));
        assert_eq!(cf.task_page().unwrap().headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.as_str()), Some("/connect/cloudflare"));
        // AWS's starts from a page with one button, so that Back from AWS's "400 Bad Request" finds a page here.
        let mut d = Dashboard::new().for_task(Task::Login(Some("aws")));
        let first = String::from_utf8(d.task_page().unwrap().body).unwrap();
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/login-aws.html"), &first).unwrap() }
        assert!(first.contains("<h1>Sign in with AWS</h1>") && first.contains(r#"action="/aws/signin""#) && first.contains("Continue to AWS") && !first.contains(r#"name="fresh""#), "one button, to AWS");
        // Started and not back (AWS said "400 Bad Request", and the person pressed Back): signed out of AWS first, or again.
        let (_, pending) = aws::authorize("us-east-1", "http://127.0.0.1:8976/oauth/callback");
        (d.aws_pending, d.aws_asked_ms) = (Some(pending), now_ms());
        let back = String::from_utf8(d.task_page().unwrap().body).unwrap();
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/login-aws-back.html"), &back).unwrap() }
        assert!(back.contains("<h1>Your AWS sign-in is not finished</h1>") && back.contains("400 Bad Request") && back.contains(r#"name="fresh" value="on"><button class="button">Sign out of AWS and try again"#) && back.contains(">Try again<"));
        (d.aws_pending, d.aws_asked_ms) = (None, 0);
        d.modal = Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() });
        assert!(d.task_done().is_none(), "another cloud's sign-in is not this one");
        d.aws = Some(aws::Session::for_test("123456789012"));
        assert!(d.task_done().unwrap().starts_with("Signed in: AWS account 123456789012, Modal workspace acme."));
        let done = String::from_utf8(d.task_page().unwrap().body).unwrap();
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/done-login.html"), &done).unwrap() }
        assert!(done.contains("<h1>Signed in with AWS</h1>") && done.contains("SuperCI stays signed in on this computer until you sign out.") && done.contains("You can close this tab.") && !done.contains(">Back<"), "what was done, and nothing to press");
        // With any cloud: the dashboard's own first screen asks which.
        let mut any = Dashboard::new().for_task(Task::Login(None));
        assert!(any.task_page().is_none() && any.task_done().is_none());
        // A GitHub App: its tab's first load starts it (here with no control plane to make it for: said).
        let mut fresh = Dashboard::new().for_task(Task::GitHub { login: "acme".into(), host: String::new(), started: false, done: false });
        let first = fresh.task_page().unwrap();
        assert!(matches!(fresh.task, Some(Task::GitHub { started: true, .. })) && first.status == 500 && String::from_utf8_lossy(&first.body).contains("no control plane yet"), "{}", first.status);
        // Done when GitHub comes back from choosing its repositories.
        let mut g = Dashboard::new().for_task(Task::GitHub { login: "acme".into(), host: String::new(), started: true, done: false });
        assert!(g.task_done().is_none() && String::from_utf8(g.task_page().unwrap().body).unwrap().contains("Finish on GitHub"));
        let _ = g.handle(&Req { method: "GET".into(), path: "/github/installed".into(), query: vec![], cookie: None, last: None, body: vec![], host: String::new(), origin: None, fetch_site: None });
        assert!(g.task_done().unwrap().starts_with("GitHub is connected for acme"));
        let done = String::from_utf8(g.task_page().unwrap().body).unwrap();
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/done-github.html"), &done).unwrap() }
        assert!(done.contains("<h1>GitHub is connected</h1>") && done.contains("acme&#39;s repositories") && !done.contains(">Back<"));
    }

    #[test]
    fn a_cloudflare_sign_in_that_expired_stops_nothing_the_other_sign_ins_reach() {
        let dir = std::env::temp_dir().join(format!("superci-cf-ended-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let store = Store::at(&dir);
        let in_aws = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "http://127.0.0.1:9".into(), plane_id: "abcdef123456".into(), label: "superci".into() };
        let in_cf = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "http://127.0.0.1:9".into(), plane_id: "p1".into(), label: "superci".into() };
        // Signed in to AWS and to Cloudflare, a control plane in each; Cloudflare no longer renews its sign-in.
        // (AWS was looked in already and both control planes have this session's key: nothing is asked of a cloud here.)
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.cf = Some(cloudflare::Session::expired_for_test());
        d.cf_accounts = vec![("a1".into(), "Acme Inc.".into())];
        d.aws = Some(aws::Session::for_test("123456789012"));
        d.looked_aws = true;
        d.planes = vec![in_aws.clone(), in_cf.clone()];
        d.keyed.extend(["abcdef123456".to_string(), "p1".to_string()]);
        // Looking for control planes goes on: the sign-in is forgotten, the control plane known in Cloudflare stays
        // listed, and AWS's is still the session's.
        d.discover().unwrap();
        assert!(d.cf.is_none() && d.cf_ended && d.cf_accounts.is_empty());
        assert_eq!((d.planes.len(), d.signed_in_as().as_deref()), (2, Some("AWS account 123456789012")));
        // Said once beside what a command lists, and on the pages, with the way to sign in again.
        assert_eq!(d.ended_notes(), ["The Cloudflare sign-in has expired: what is in Cloudflare was not looked at. `superci login cloudflare` signs in again."]);
        let notice = d.others_ended_notice();
        assert!(notice.contains("Your Cloudflare sign-in has expired.") && notice.contains(r#"action="/cloudflare/signin""#) && !notice.contains("Modal"), "{notice}");
        // What needs Cloudflare says so; what does not goes on.
        assert_eq!(d.cloudflare().err().as_deref(), Some(CF_ENDED));
        assert_eq!((crate::commands::needs(CF_ENDED), crate::commands::needs(MODAL_ENDED)), (Some("superci login cloudflare"), Some("superci login modal")));
        assert!(d.look(false).is_ok(), "a command that lists control planes goes on with AWS's");
        // Kept as that: no Cloudflare sign-in, that it ended, and AWS's as it was. The next start knows.
        d.keep();
        let kept = store.read();
        assert!(kept.cloudflare.is_none() && kept.cloudflare_ended && !kept.modal_ended && kept.aws.is_some());
        let mut next = Dashboard::signed_in(Some(store.clone()));
        assert!(next.cf_ended && next.cf.is_none() && !next.ended_notes().is_empty());
        // Signed in again: nothing more is said, and nothing of it is kept.
        next.cf = Some(cloudflare::Session::from_token("test"));
        assert!(next.ended_notes().is_empty() && next.others_ended_notice().is_empty());
        next.keep();
        assert!(!store.read().cloudflare_ended);
        let _ = std::fs::remove_dir_all(&dir);

        // Cloudflare alone was signed in to: a command says that its sign-in expired, not that there was none.
        let mut only = Dashboard::new();
        only.cf = Some(cloudflare::Session::expired_for_test());
        assert_eq!(only.ready().unwrap_err(), CF_ENDED);
        // A token given by name for this run is not SuperCI's to forget: its error is said as it is.
        let mut given = Dashboard::new();
        (given.cf, given.cf_given) = (Some(cloudflare::Session::expired_for_test()), true);
        assert!(given.ready().unwrap_err().contains("the Cloudflare sign-in expired") && given.cf.is_some() && !given.cf_ended);
        // Modal no longer taking its token is the same.
        let mut m = Dashboard::new();
        m.modal = Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() });
        m.aws = Some(aws::Session::for_test("123456789012"));
        m.modal_over();
        assert!(m.modal.is_none() && m.others_ended_notice().contains("Modal no longer accepts your sign-in.") && m.others_ended_notice().contains(r#"action="/modal/signin""#));
        assert_eq!(m.ended_notes(), ["Modal no longer accepts the sign-in: what is in Modal was not looked at. `superci login modal` signs in again."]);
    }

    #[test]
    fn signing_out_and_a_sign_in_that_ended() {
        let dir = std::env::temp_dir().join(format!("superci-out-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let store = Store::at(&dir);
        let plane = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "http://127.0.0.1:9".into(), plane_id: "abcdef123456".into(), label: "superci".into() };
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.modal = Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "acme".into() });
        d.planes.push(plane.clone());
        d.keep();
        // Settings, below the control plane in the sidebar, signs out: nothing is left on this machine, and the page
        // says so once. (No More menu.)
        let side = Dashboard::sidebar("settings");
        assert!(side.contains(r#"<a href="/?p=settings" aria-current="true">"#) && !side.contains("More") && side.find("Control plane") < side.find("Settings") && side.find("Settings") < side.find("Changelog"));
        let settings = d.render("settings", &[], None);
        assert!(settings.contains("Signed in: Modal workspace acme.") && settings.contains(r#"<form method="post" action="/signout"><button class="button secondary sm">Sign out</button></form>"#), "{settings}");
        // The control plane kept from the last run is listed at once, before any cloud was asked.
        let mut next = Dashboard::signed_in(Some(store.clone()));
        assert_eq!(next.planes, vec![plane.clone()]);
        // Its AWS sign-in ended meanwhile: its pages still show, with a way to sign in again.
        next.aws_ended = true;
        let notice = next.aws_ended_notice();
        preview("aws-ended", &format!("{notice}{}", next.render("overview", &[view::plane_view(&plane, None)], None)));
        assert!(notice.contains("Your AWS sign-in has ended (AWS ends one after 12 hours)") && notice.contains(r#"action="/aws/signin""#));
        next.aws = Some(aws::Session::for_test("123456789012"));
        assert!(next.aws_ended_notice().is_empty());
        assert_eq!(next.act("/signout", &[]).unwrap(), "");
        assert!(!store.path().exists() && next.signed_in_as().is_none() && next.planes.is_empty());
        assert!(next.notice.as_deref().is_some_and(|n| n.starts_with("Signed out.")));
        assert!(!Dashboard::signed_in(Some(store.clone())).read_only_of_test());
        // AWS alone was signed in to and AWS ended it: the control plane and its key stay kept, so the next start
        // still reads; a change says the sign-in ended.
        let mut only = Dashboard::signed_in(Some(store.clone()));
        only.planes.push(plane.clone());
        only.keyed.insert("abcdef123456".into());
        only.aws_ended = true;
        only.keep();
        assert!(store.path().exists() && store.read().aws_ended && store.read().plane.is_some() && !store.read().signed_in());
        let mut next = Dashboard::signed_in(Some(store.clone()));
        assert!(next.aws_ended && next.signed_in_as().is_none() && next.planes == vec![plane.clone()]);
        assert_eq!(next.current().unwrap().unwrap().plane, plane, "read with the kept key, without a sign-in");
        assert_eq!(next.ready().unwrap_err(), AWS_ENDED);
        assert_eq!(next.act("/runners/aws-own", &[]).unwrap_err(), AWS_ENDED);
        // Signed in to AWS again: nothing says it ended any more.
        next.aws = Some(aws::Session::for_test("123456789012"));
        next.aws_ended = false;
        next.keep();
        assert!(!store.read().aws_ended && store.read().signed_in());
        next.sign_out().unwrap();
        assert!(!store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn every_provider_section_and_cloud_has_its_name_and_any_other_is_said_as_it_is() {
        for (table, name) in [(&PROVIDER_NAMES[..], provider_name as fn(&str) -> &str), (&CLOUD_NAMES[..], cloud_name)] {
            for (id, want) in table { assert_eq!(name(id), *want, "{id}") }
            assert_eq!(name("elsewhere"), "elsewhere");
        }
        for (id, title) in SECTION_TITLES { assert_eq!(section_title(id), title, "{id}") }
        assert_eq!((section_title(""), section_title("nowhere")), ("Overview", "Overview"));
        assert_eq!((cloud_name(AWS_ON_DEMAND), provider_name("machine"), cloud_name("machine")), ("AWS on-demand", "Your own computer", "Your computer"));
    }
}

/// What the other tests leave out: the ways things go wrong that need no cloud (a sign-in or a control plane missing,
/// a request that is no request), and the words a page has for what is rare.
#[cfg(test)]
mod rest_tests {
    use super::*;

    /// Where nothing answers (the discard port: refused at once).
    const NOWHERE: &str = "http://127.0.0.1:9";

    fn aws_plane(id: &str, url: &str) -> Plane { Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: url.into(), plane_id: id.into(), label: "superci".into() } }
    fn cf_plane(id: &str, url: &str) -> Plane { Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme".into(), script: "superci".into(), url: url.into(), plane_id: id.into(), label: "superci".into() } }
    fn modal_plane(id: &str, url: &str) -> Plane { Plane::Modal { workspace: "acme".into(), url: url.into(), plane_id: id.into(), label: "superci".into() } }
    fn seen_plane(id: &str, url: &str) -> Plane { Plane::Seen { url: url.into(), plane_id: id.into() } }
    fn modal_session(workspace: &str) -> modal::Session { modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: workspace.into() } }

    /// A form sent to the dashboard as its pages send it.
    fn send(d: &mut Dashboard, path: &str, fields: &[(&str, &str)]) -> Result<Response> {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish().into_bytes();
        d.handle(&Req { method: "POST".into(), path: path.into(), query: vec![], cookie: None, last: None, body, host: String::new(), origin: None, fetch_site: None })
    }

    /// A page asked for.
    fn open(d: &mut Dashboard, path: &str, query: &[(&str, &str)]) -> Result<Response> {
        d.handle(&Req { method: "GET".into(), path: path.into(), query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), cookie: None, last: None, body: vec![], host: String::new(), origin: None, fetch_site: None })
    }

    fn words_of(r: &Response) -> String { plain(&String::from_utf8_lossy(&r.body)) }

    /// A server on this computer answering each request as `answer` says (its method, path and body: a status and a
    /// JSON body): a control plane or another service, as far as a test needs one.
    fn serving<B: Into<Vec<u8>>>(answer: impl Fn(&str, &str, &str) -> (u16, B) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let answer = Arc::new(answer);
        std::thread::spawn(move || listener.incoming().flatten().for_each(|mut s| {
            let answer = answer.clone();
            std::thread::spawn(move || {
                let Some(req) = read_request(&mut s) else { return };
                let (status, body) = answer(&req.method, &req.path, &String::from_utf8_lossy(&req.body));
                write_response(&mut s, &Response::new(status, "application/json", body));
            });
        }));
        url
    }

    /// A control plane of this version that answers its health and its status (as `status` has it), and refuses
    /// anything else.
    fn plane_answering(status: serde_json::Value) -> String {
        serving(move |_, path, _| match path {
            "/health" => (200, serde_json::json!({ "ok": true, "version": DASHBOARD_VERSION }).to_string()),
            "/status" => (200, status.to_string()),
            _ => (500, r#"{"error":"not here"}"#.into()),
        })
    }

    #[test]
    fn tries_asks_at_most_as_often_as_it_is_told() {
        let mut asked = 0;
        assert_eq!(tries(0, 0, || { asked += 1; Some(1) }), None);
        assert_eq!(asked, 0, "told none, it asks none");
        assert_eq!(tries(3, 0, || { asked += 1; (asked == 2).then_some(asked) }), Some(2));
        assert_eq!(tries(2, 0, || None::<u8>), None);
    }

    #[test]
    fn a_lock_whose_holder_panicked_is_taken_as_it_was_left() {
        let m = Arc::new(Mutex::new(1));
        let held = m.clone();
        let _ = std::thread::spawn(move || { let mut g = held.lock().unwrap(); *g = 2; panic!("while holding it") }).join();
        assert!(m.is_poisoned());
        assert_eq!(*lock(&m), 2);
        // (Each test's own frame notes.)
        assert!(lock(pictured()).is_empty());
    }

    #[test]
    fn what_is_no_http_request_is_dropped_without_an_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let read = |bytes: &[u8]| {
            let mut client = TcpStream::connect(at).unwrap();
            client.write_all(bytes).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s).map(|r| r.path)
        };
        assert_eq!(read(b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n").as_deref(), Some("/x"));
        for (what, bytes) in [("nothing", &b""[..]), ("no target", b"GET\r\n"), ("not text", b"\xff\xfe /x\r\n\r\n"), ("a header not text", b"GET /x HTTP/1.1\r\n\xff\xfe\r\n\r\n"),
            ("a header without a colon", b"GET /x HTTP/1.1\r\nnonsense\r\n\r\n"), ("a length that is no number", b"POST /x HTTP/1.1\r\ncontent-length: many\r\n\r\n"),
            ("a body shorter than said", b"POST /x HTTP/1.1\r\ncontent-length: 10\r\n\r\nabc"), ("no address", b"GET :99999 HTTP/1.1\r\n\r\n")] {
            assert_eq!(read(bytes), None, "{what}");
        }
    }

    #[test]
    fn an_answer_without_headers_is_written_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut s, _) = listener.accept().unwrap();
        write_response(&mut s, &Response { status: 204, headers: vec![], body: vec![] });
        drop(s);
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        assert_eq!(got, "HTTP/1.1 204 X\r\ncontent-length: 0\r\ncache-control: no-store\r\nconnection: close\r\n\r\n");
    }

    #[test]
    fn rare_things_have_words_of_their_own() {
        // A key listed without its name or its end is none.
        assert_eq!(read_keys(&serde_json::json!({ "read_keys": [{ "until_ms": 1 }, { "name": "AGENT" }, { "name": "CI", "until_ms": 2 }] })), [("CI".to_string(), 2)]);
        // A job cancelled while its machine came up is cancelled.
        for state in ["cancelled", "orphan"] { assert_eq!(job_words(&serde_json::json!({ "state": state }))["state"], "cancelled") }
        assert_eq!(plain_reason(&serde_json::json!({ "state": "waiting" })), "Waiting for a free runner");
        // GitLab's own, alone, is what runners are best near.
        let gitlab_only = PlaneView { plane: aws_plane("p1", NOWHERE), online: true, github: false, installed: false, aws: false, runners: false, gitlab: true, moved_to: None, standby: false, version: None,
            status: Some(serde_json::json!({ "gitlab": { "url": "https://gitlab.com" } })) };
        assert_eq!(closest(Some(&gitlab_only)), "closest to GitLab");
        // Refusals by the organization's resource policies, a permissions boundary and the session's policy.
        for (by, said) in [("resource control policy", "a resource policy of the AWS organization"), ("permissions boundary", "has a permissions boundary that does not allow"), ("session policy", "This AWS sign-in's session does not allow")] {
            let e = format!("AccessDenied: User: arn:aws:iam::111111111111:user/ci is not authorized to perform: iam:CreateRole with an explicit deny in a {by}");
            assert!(plain_refusal(&e).unwrap().contains(said), "{by}: {:?}", plain_refusal(&e));
        }
        assert!(quota_text("us-east-1", 2).starts_with("Your account may run only 2 spot CPUs at once there, too few for a 4-CPU job."));
        // A job's cost: Cloudflare's and Modal's at their list prices when it says none; AWS's not without one.
        let job = |cloud: &str| serde_json::json!({ "job_id": 1, "run_id": 1, "repo": "acme/web", "installation_id": 1, "seen_in_progress": true, "machine_id": "m", "cloud": cloud, "state": "done", "at_ms": 1, "started_ms": 1, "ended_ms": 3_600_001 });
        assert!(job_cost(&job("cloudflare")).is_some_and(|c| c > 0.0) && job_cost(&job("modal")).is_some_and(|c| c > 0.0));
        assert_eq!(job_cost(&job("aws")), None);
        // A container's size that is not CPU and GB is said by its provider's name; its memory is none.
        assert_eq!(runner_text(&serde_json::json!({ "cloud": "cloudflare", "machine_type": "4cpu-12tb" })).1, "Cloudflare");
        assert_eq!((container_gb("4cpu-12gb"), container_gb("4cpu-12tb"), container_gb("basic")), (12.0, 0.0, 0.0));
        // A key's end, from a name that has none.
        assert_eq!((key_until("DEVICE_KEY_"), key_until("DEVICE_KEY_4102444800_AB")), (0, 4102444800));
    }

    #[test]
    fn a_task_page_with_no_task_says_done() {
        let page = String::from_utf8(Dashboard::new().done_page().body).unwrap();
        assert!(page.contains("<h1>Done</h1>"), "{page}");
        // Waiting for what was never started says so.
        let mut d = Dashboard::new();
        assert_eq!(d.wait(Background::Deploy, &mut |_| {}).unwrap_err(), "no deploy was started");
        assert_eq!(d.wait(Background::Update, &mut |_| {}).unwrap_err(), "no update was started");
    }

    #[test]
    fn what_comes_back_from_elsewhere_is_awaited_only_where_it_was_started() {
        let d = Dashboard::new();
        let req = |path: &str| Req { method: "GET".into(), path: path.into(), query: vec![("state".into(), "s".into())], cookie: None, last: None, body: vec![], host: String::new(), origin: None, fetch_site: None };
        assert!(!d.awaits(&req("/oauth/modal")) && !d.awaits(&req("/elsewhere")) && !d.awaits(&req("/github/installed")));
    }

    #[test]
    fn everything_that_changes_a_control_plane_needs_one_first() {
        let mut d = Dashboard::new();
        for path in ["/runners/aws-own", "/runners/remove", "/runners/modal-own", "/gitlab/connect", "/gitlab/project", "/gitlab/disconnect", "/github/start", "/github/remove", "/aws/connect",
            "/runners/modal", "/runners/cloudflare", "/labels", "/name", "/machine", "/routing", "/run/cancel", "/caches/delete", "/plane/update"] {
            assert_eq!(send(&mut d, path, &[]).err().as_deref(), Some("no control plane yet"), "{path}");
        }
        assert_eq!(send(&mut d, "/plane/leave", &[("confirm", "superci")]).err().as_deref(), Some("no control plane yet"));
        assert_eq!(send(&mut d, "/plane/leave", &[("confirm", "superci"), ("plane", "nope")]).err().as_deref(), Some("That control plane is not known here"));
        assert_eq!(d.create_key("agent", 30, false).unwrap_err(), "no control plane yet");
        assert_eq!(d.revoke_key("agent").unwrap_err(), "no control plane yet");
        assert_eq!(d.drop_device_key("DEVICE_KEY_1_OTHER").unwrap_err(), "no control plane yet");
        assert_eq!(d.kept_secrets_said().unwrap_err(), "no control plane yet");
        let scope = superci_core::secrets::valid_scope(None, None, None, None).unwrap();
        assert_eq!(d.keep_secret("NPM_TOKEN", &scope, "x", true).unwrap_err(), "no control plane yet");
        assert_eq!(d.forget_secret("NPM_TOKEN", &scope).unwrap_err(), "no control plane yet");
        assert!(d.forget_secret("not a name", &scope).is_err());
        // A job's log and a run's artifact are read only signed in somewhere.
        assert_eq!(open(&mut d, "/job/view", &[("id", "1")]).err().as_deref(), Some(NOT_SIGNED_IN));
        assert_eq!(open(&mut d, "/run/artifact", &[("run", "1"), ("name", "a")]).err().as_deref(), Some(NOT_SIGNED_IN));
        // One not found among the signed-in clouds is none to join.
        assert_eq!(d.join("nope"), Err((404, "nope is not a control plane found in your signed-in clouds".to_string())));
        let refused = send(&mut d, "/plane/join", &[("plane", "nope")]).unwrap();
        assert!(refused.status == 404 && words_of(&refused).contains("Nothing has changed."));
        // Nothing kept on a machine with no folder: signing out says so.
        assert_eq!(d.sign_out().unwrap(), ["Nothing is kept on this machine (it has no home folder)."]);
    }

    #[test]
    fn cloudflares_sign_in_starts_from_its_own_port() {
        let mut d = Dashboard::new();
        let r = send(&mut d, "/cloudflare/signin", &[("next", "?p=runners")]).unwrap();
        let to = r.headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone()).unwrap_or_default();
        assert!(to.starts_with("https://dash.cloudflare.com/oauth2/auth") && d.cf_pending.is_some() && d.return_to == "?p=runners", "{to}");
    }

    #[test]
    fn a_control_plane_known_only_from_afar_is_changed_nowhere() {
        // Its settings are written nowhere: by the dashboard, or in the background.
        let seen = seen_plane("p3", NOWHERE);
        let writer = Writer { cf: None, aws: None, modal: None, own: Own::default() };
        assert_eq!(writer.put(&seen, "NAME", "x").unwrap_err(), "Sign in where this control plane runs first.");
        assert_eq!(writer.drop(&seen, "NAME").unwrap_err(), "Sign in where this control plane runs first.");
        assert!(writer.names(&seen).is_empty() && writer.names(&modal_plane("p2", NOWHERE)).is_empty() && writer.names(&cf_plane("p1", NOWHERE)).is_empty());
        // Without the sign-in of its cloud, nothing is written.
        assert_eq!(writer.drop(&cf_plane("p1", NOWHERE), "NAME").unwrap_err(), "Sign in with Cloudflare first.");
        assert_eq!(writer.drop(&modal_plane("p2", NOWHERE), "NAME").unwrap_err(), "Sign in with Modal first.");
        let mut d = Dashboard::new();
        assert!(d.put_secret(&seen, "NAME", "x").unwrap_err().contains("known only from AWS"));
        assert!(d.drop_secret(&seen, "NAME").unwrap_err().contains("known only from AWS"));
        assert_eq!(d.put_secret(&cf_plane("p1", NOWHERE), "NAME", "x").unwrap_err(), "Sign in with Cloudflare first.");
        assert_eq!(d.put_secret(&modal_plane("p2", NOWHERE), "NAME", "x").unwrap_err(), "Sign in with Modal first.");
        assert_eq!(d.drop_secret(&cf_plane("p1", NOWHERE), "NAME").unwrap_err(), "Sign in with Cloudflare first.");
        assert_eq!(d.drop_secret(&modal_plane("p2", NOWHERE), "NAME").unwrap_err(), "Sign in with Modal first.");
        assert_eq!(d.drop_secret(&aws_plane("p0", NOWHERE), "NAME").unwrap_err(), "Sign in with AWS first.");
        // Another account's sign-in changes nothing there.
        d.aws = Some(aws::Session::for_test_until("999999999999", u64::MAX));
        assert!(d.drop_secret(&aws_plane("p0", NOWHERE), "NAME").unwrap_err().contains("SuperCI is signed in to 999999999999"));
    }

    #[test]
    fn a_control_plane_deleted_from_beside_another_needs_its_own_clouds_sign_in() {
        let mut d = Dashboard::new();
        d.planes = vec![aws_plane("p0", NOWHERE), cf_plane("p1", NOWHERE), modal_plane("p2", NOWHERE), seen_plane("p3", NOWHERE), aws_plane("p4", NOWHERE)];
        assert_eq!(send(&mut d, "/plane/delete", &[("plane", "p1")]).err().as_deref(), Some("Sign in with Cloudflare first."));
        assert_eq!(send(&mut d, "/plane/delete", &[("plane", "p2")]).err().as_deref(), Some("Sign in with Modal first."));
        assert_eq!(send(&mut d, "/plane/delete", &[("plane", "p3")]).err().as_deref(), Some("Sign in where this control plane runs to delete it."));
        assert_eq!(send(&mut d, "/plane/delete", &[("plane", "p4")]).err().as_deref(), Some("Sign in with AWS first."));
        d.modal = Some(modal_session("globex"));
        assert!(send(&mut d, "/plane/delete", &[("plane", "p2")]).unwrap_err().contains("the Modal workspace acme"));
    }

    #[test]
    fn an_update_needs_its_clouds_sign_in_and_a_control_plane_that_runs_somewhere() {
        let mut d = Dashboard::new();
        // Signed in to another Modal workspace than the control plane's.
        d.planes = vec![modal_plane("p2", NOWHERE)];
        d.modal = Some(modal_session("globex"));
        assert!(send(&mut d, "/plane/update", &[("plane", "0")]).unwrap_err().contains("the Modal workspace acme"));
        // One known only from afar answers, and is still updated nowhere.
        let url = plane_answering(serde_json::json!({ "jobs": [] }));
        d.planes = vec![seen_plane("p3", &url)];
        let r = send(&mut d, "/plane/update", &[("plane", "p3")]).unwrap();
        assert!(r.status == 400 && words_of(&r).contains("Sign in where this control plane runs to update it"), "{}", words_of(&r));
        // Leaving it is not done for another Modal workspace either.
        d.planes = vec![modal_plane("p2", NOWHERE)];
        assert!(send(&mut d, "/plane/leave", &[("confirm", "superci")]).unwrap_err().contains("the Modal workspace acme"));
        // With its own: a control plane that cannot let GitHub go yet is not deleted.
        d.modal = Some(modal_session("acme"));
        assert!(send(&mut d, "/plane/leave", &[("confirm", "superci")]).unwrap_err().starts_with("GitHub and GitLab could not be let go of yet"));
    }

    #[test]
    fn a_move_needs_the_one_in_use_and_a_new_one_that_takes_github() {
        // The switch of a move that did not finish, to a new one that cannot take GitHub yet: said, nothing changed.
        let to = serving(|_, path, _| match path { "/health" => (200, serde_json::json!({ "version": DASHBOARD_VERSION, "standby": true }).to_string()), _ => (500, "{}".into()) });
        let to_for_old = to.clone();
        let old = serving(move |_, path, _| match path { "/health" => (200, serde_json::json!({ "version": DASHBOARD_VERSION, "moved_to": to_for_old }).to_string()), _ => (500, "{}".into()) });
        let mut d = Dashboard::new();
        d.planes = vec![aws_plane("p0", &old), cf_plane("p1", &to)];
        assert!(send(&mut d, "/plane/move", &[("plane", "p1")]).unwrap_err().starts_with("The new control plane could not take GitHub over yet"));
        // None in use (its place in the list is gone).
        d.planes = vec![cf_plane("p1", NOWHERE)];
        d.selected = 9;
        assert_eq!(send(&mut d, "/plane/move", &[("plane", "p1")]).err().as_deref(), Some("no control plane yet"));
        // A Cloudflare sign-in that has ended is said before anything moves.
        d.planes = vec![aws_plane("p0", NOWHERE), cf_plane("p1", NOWHERE)];
        d.selected = 0;
        d.cf = Some(cloudflare::Session::expired_for_test());
        assert!(send(&mut d, "/plane/move", &[("plane", "p1")]).is_err());
    }

    #[test]
    fn pages_without_a_control_plane_say_to_set_one_up() {
        let d = Dashboard::new();
        assert!(d.runners_page(None).contains("No runners yet"));
        for page in [d.add_runners_page(None), d.repos_page(None), d.workflows_page(None)] { assert!(page.contains("Set up a control plane first"), "{page}") }
        assert!(d.run_page(None).contains(r#"<h1 class="page-title">Run</h1>"#) && d.job_page(None).contains(r#"<h1 class="page-title">Job</h1>"#));
    }

    #[test]
    fn a_deploy_that_finished_is_no_longer_on_its_way() {
        let d = Dashboard::new();
        let deploy = |cloud: &'static str, result| Deploy { cloud, place: "workspace acme".into(), steps: &modal::DEPLOY_STEPS, at: 1, form: vec![], result };
        *lock(&d.deploying) = Some(deploy("modal", None));
        assert!(d.deploy_progress().unwrap().contains("about 2 minutes"));
        *lock(&d.deploying) = Some(deploy("modal", Some(Ok(modal_plane("p2", NOWHERE)))));
        assert!(d.deploy_progress().is_none() && d.deploy_line().is_none());
    }

    /// A control plane's view as a page has it: online at this version, answering with `status`.
    fn seen_as(plane: Plane, status: serde_json::Value) -> PlaneView {
        PlaneView { plane, online: true, github: false, installed: false, aws: false, runners: false, gitlab: false, moved_to: None, standby: false, version: Some(DASHBOARD_VERSION.into()), status: Some(status) }
    }

    /// A folder for one test, gone when it ends.
    struct Folder(std::path::PathBuf);
    impl Folder { fn new(what: &str) -> Folder { let dir = std::env::temp_dir().join(format!("superci-{what}-{}-{}", std::process::id(), superci_core::crypto::random_id(6))); std::fs::create_dir_all(&dir).unwrap(); Folder(dir) } }
    impl Drop for Folder { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    fn hub_at(root: Option<std::path::PathBuf>, first: Option<&str>) -> Hub {
        Hub { root, key: "k".repeat(32), base: "http://localhost:8976".into(), first: Mutex::new(first.map(str::to_string)), boards: Mutex::new(HashMap::new()), opened: Mutex::new(None) }
    }

    fn ask(hub: &Hub, path: &str, body: &str) -> (u16, String) {
        let r = respond(hub, &Req { method: "POST".into(), path: path.into(), query: vec![], cookie: Some("k".repeat(32)), last: None, body: body.as_bytes().to_vec(), host: "localhost:8976".into(), origin: None, fetch_site: Some("same-origin".into()) });
        (r.status, words_of(&r))
    }

    /// A server that says more is coming than it sends: what it answers is cut short.
    fn cut_short() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || listener.incoming().flatten().for_each(|mut s| {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nabc");
        }));
        url
    }

    #[test]
    fn workspaces_are_not_made_renamed_or_forgotten_where_nothing_can_be_kept() {
        // A machine with no folder: there is the one workspace.
        let hub = hub_at(None, Some("default"));
        lock(&hub.boards).insert("default".into(), Arc::new(Mutex::new(Dashboard::new())));
        assert!(ask(&hub, "/workspaces", "name=Acme").1.contains("SuperCI keeps nothing on this machine"));
        assert!(ask(&hub, "/workspaces/rename", "w=default&id=acme").1.contains("SuperCI keeps nothing on this machine"));
        assert!(ask(&hub, "/workspaces/remove", "w=default").1.contains("SuperCI keeps nothing on this machine"));
        // A folder that cannot be made (where a file is): said, for the one started in and for the new one.
        let f = Folder::new("hub-file");
        let file = f.0.join("not-a-folder");
        std::fs::write(&file, "x").unwrap();
        assert!(hub_at(Some(file.clone()), Some("default")).create("Acme").unwrap_err().contains("could not make"));
        assert!(hub_at(Some(file), None).create("Acme").unwrap_err().contains("could not make"));
        // An id that cannot be one.
        let hub = hub_at(Some(f.0.clone()), Some("default"));
        assert!(hub.rename("default", "Not An Id!").unwrap_err().contains("A workspace's id is"));
        // Its sign-ins that cannot be taken out (a folder where the file is), and a folder that cannot go: said,
        // and nothing more is forgotten.
        std::fs::create_dir_all(f.0.join("workspaces/acme/sign-ins.json")).unwrap();
        assert!(hub.remove("acme").unwrap_err().contains("could not remove"));
        let mut d = Dashboard::signed_in(Some(Store::under(&f.0, "acme")));
        assert!(send(&mut d, "/signout", &[]).unwrap_err().contains("could not remove"));
        std::fs::remove_dir_all(f.0.join("workspaces/acme/sign-ins.json")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f.0.join("workspaces"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let gone = hub.remove("acme");
        std::fs::set_permissions(f.0.join("workspaces"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(gone.unwrap_err().contains("could not remove"));
    }

    #[test]
    fn a_deploy_of_a_control_plane_listed_already_shows_that_one() {
        let mut d = Dashboard::new();
        d.planes = vec![aws_plane("p0", NOWHERE), modal_plane("p2", NOWHERE)];
        *lock(&d.deploying) = Some(Deploy { cloud: "modal", place: "workspace acme".into(), steps: &modal::DEPLOY_STEPS, at: 3, form: vec![], result: Some(Ok(modal_plane("p2", NOWHERE))) });
        d.adopt_deploy();
        assert_eq!((d.planes.len(), d.selected), (2, 1));
        // Back from installing GitHub's App for one of several: to that one's pages.
        d.github_for = Some("p2".into());
        let back = words_of(&open(&mut d, "/github/installed", &[]).unwrap());
        assert!(back.contains("Installed on GitHub"), "{back}");
    }

    #[test]
    fn forms_with_what_cannot_be_taken_say_so_before_anything_is_written() {
        let mut d = Dashboard::new();
        d.planes = vec![aws_plane("p0", NOWHERE)];
        assert_eq!(send(&mut d, "/gitlab/project", &[("id", "many")]).err().as_deref(), Some("which project?"));
        // A further GitLab's token is taken out with its cloud's sign-in: none here.
        assert_eq!(send(&mut d, "/gitlab/disconnect", &[("gitlab", "g1")]).err().as_deref(), Some("Sign in with AWS first."));
    }

    /// A Modal control plane, not signed in to Modal, answering with `status`: each change below stops where it would
    /// be written.
    fn modal_answering(status: serde_json::Value) -> Dashboard {
        let mut d = Dashboard::new();
        d.planes = vec![modal_plane("p2", &plane_answering(status))];
        d
    }

    #[test]
    fn changes_to_a_control_plane_elsewhere_go_by_what_it_says() {
        // Its AWS account as its status says it: not signed in to it, AWS's permissions and budget are not changed.
        let mut d = modal_answering(serde_json::json!({ "aws": { "account_id": "222222222222" }, "routing": { "public_repos": ["acme/*"] }, "account_limits": true, "daily_limits": true }));
        let r = send(&mut d, "/permissions/aws", &[("plane", "p2")]).unwrap();
        assert!(r.status == 403 && words_of(&r).contains("Sign in to account 222222222222"), "{}", words_of(&r));
        let r = send(&mut d, "/routing", &[("action", "account_usd"), ("account", "aws"), ("usd", "100"), ("budget", "emails")]).unwrap();
        assert!(r.status == 403 && words_of(&r).contains("Sign in to account 222222222222"), "{}", words_of(&r));
        // Public repositories: none chosen, or some; providers in an order (one taken out, or one unknown); its own
        // network and its regions in AWS: each stops at writing, without a sign-in to Modal.
        for fields in [&[("action", "public_set")][..], &[("action", "public_set"), ("repo", "acme/web")], &[("action", "order"), ("out_0", "aws"), ("cloud_x", "")],
            &[("action", "account"), ("cloud", "aws"), ("networks", "us-east-1 subnet-0123456789abcdef0 sg-0123456789abcdef0")], &[("action", "pool"), ("cloud", "aws"), ("region_0", "us-west-2"), ("region_1", "us-west-2")]] {
            let said = send(&mut d, "/routing", fields);
            assert!(said.as_ref().err().is_some_and(|e| e == "Sign in with Modal first.") || said.as_ref().is_ok_and(|r| r.status == 400), "{fields:?}: {:?}", said.map(|r| words_of(&r)));
        }
        let r = send(&mut d, "/routing", &[("action", "order"), ("cloud_0", "aws"), ("out_0", "nowhere!")]).unwrap();
        assert!(r.status == 400 && words_of(&r).contains("Unknown provider"));
        let r = send(&mut d, "/routing", &[("action", "pool"), ("cloud", "aws"), ("region_0", "mars-1")]).unwrap();
        assert!(r.status == 400 && words_of(&r).contains("Choose a region from the list"));
        assert_eq!(send(&mut d, "/machine", &[]).err().as_deref(), Some("Sign in with Modal first."));
        // Cloudflare's runners go into an account named when the sign-in reaches several.
        let mut d = modal_answering(serde_json::json!({ "launchers": [] }));
        d.cf = Some(cloudflare::Session::from_token("test"));
        d.cf_accounts = vec![("a1".into(), "Acme".into()), ("a2".into(), "Globex".into())];
        let r = send(&mut d, "/runners/cloudflare", &[]).unwrap();
        assert!(r.status == 409 && words_of(&r).contains("Your Cloudflare sign-in reaches 2: Acme (a1), Globex (a2)."), "{}", words_of(&r));
    }

    #[test]
    fn an_image_of_ones_own_is_checked_before_it_is_written_and_needs_a_control_plane_that_keeps_it() {
        let add = |d: &mut Dashboard, name: &str, reference: &str, secret: &str| send(d, "/routing", &[("action", "image_add"), ("name", name), ("reference", reference), ("modal_secret", secret)]);
        // A control plane from before images of one's own would drop it unsaid.
        let mut d = modal_answering(serde_json::json!({ "routing": {} }));
        let r = add(&mut d, "rust-ci", "ghcr.io/acme/ci:1", "").unwrap();
        assert!(r.status == 409 && words_of(&r).contains("Images of your own need a newer control plane"), "{}", words_of(&r));
        let mut d = modal_answering(serde_json::json!({ "routing": {}, "runner_images": true }));
        for (name, reference, secret, says) in [("Rust", "ghcr.io/acme/ci:1", "", "Check the image's name"), ("ubuntu-ci", "ghcr.io/acme/ci:1", "", "begins with ubuntu"),
            ("rust-ci", "", "", "Check the image's address"), ("rust-ci", "ghcr.io/acme/ci:1 --privileged", "", "Check the image's address"), ("rust-ci", "https://ghcr.io/acme/ci", "", "Check the image's address"),
            ("rust-ci", "ghcr.io/acme/ci:1", "a secret", "Check the Modal Secret's name")] {
            let r = add(&mut d, name, reference, secret).unwrap();
            assert!(r.status == 400 && words_of(&r).contains(says), "{name} {reference}: {}", words_of(&r));
        }
        // What can be taken stops at writing it, without a sign-in to Modal.
        for (reference, secret) in [("ghcr.io/acme/ci:1", ""), ("ghcr.io/acme/ci@sha256:0f1e", "ghcr-read"), ("acme/node-ci:22", "")] {
            assert_eq!(add(&mut d, "rust-ci", reference, secret).err().as_deref(), Some("Sign in with Modal first."), "{reference}");
        }
        assert_eq!(send(&mut d, "/routing", &[("action", "image_remove"), ("name", "rust-ci")]).err().as_deref(), Some("Sign in with Modal first."));
    }

    /// Modal's jobs as Functions: only Modal's, only on a control plane that keeps it, and its Functions deployed in
    /// that workspace before it is set (and again before an image is added); sandboxes need no deploy.
    #[test]
    fn modals_function_mode_is_checked_and_its_functions_deployed_before_it_is_set() {
        let mode = |d: &mut Dashboard, cloud: &str, mode: &str| send(d, "/routing", &[("action", "pool"), ("cloud", cloud), ("mode", mode)]);
        let mut d = modal_answering(serde_json::json!({ "routing": {} }));
        let r = mode(&mut d, "modal", "function").unwrap();
        assert!(r.status == 409 && words_of(&r).contains("Function mode needs a newer control plane"), "{}", words_of(&r));
        let mut d = modal_answering(serde_json::json!({ "routing": {}, "function_mode": true }));
        for (cloud, m, says) in [("aws", "function", "Only Modal has a mode"), ("modal", "fast", "A mode is function or sandbox")] {
            let r = mode(&mut d, cloud, m).unwrap();
            assert!(r.status == 400 && words_of(&r).contains(says), "{cloud} {m}: {}", words_of(&r));
        }
        // Its Functions first, with that workspace's sign-in; sandboxes: straight to the one write.
        assert_eq!(mode(&mut d, "modal:initech", "function").err().as_deref(), Some("Sign in with Modal to the workspace initech first: `superci login modal --add`."));
        assert_eq!(mode(&mut d, "modal", "function").err().as_deref(), Some("Sign in with Modal first."));
        assert_eq!(mode(&mut d, "modal:initech", "sandbox").err().as_deref(), Some("Sign in with Modal first."));
        // An image added while a workspace runs Functions: its Function there first.
        let mut d = modal_answering(serde_json::json!({ "routing": { "order": [{ "cloud": "modal:initech", "function": true }] }, "runner_images": true, "function_mode": true }));
        let r = send(&mut d, "/routing", &[("action", "image_add"), ("name", "rust-ci"), ("reference", "ghcr.io/acme/ci:1"), ("modal_secret", "")]);
        assert_eq!(r.err().as_deref(), Some("Sign in with Modal to the workspace initech first: `superci login modal --add`."));
    }

    #[test]
    fn an_image_changes_in_place_only_when_the_control_plane_has_it_by_that_name() {
        let update = |d: &mut Dashboard, fields: &[(&str, &str)]| send(d, "/routing", &[&[("action", "image_update")][..], fields].concat());
        let mut d = modal_answering(serde_json::json!({ "routing": { "images": { "rust-ci": { "reference": "ghcr.io/acme/ci:1", "modal_secret": "ghcr-read" } } }, "runner_images": true }));
        // A name it does not have: refused, saying how to add it; nothing written.
        let r = update(&mut d, &[("name", "gone"), ("reference", "ghcr.io/acme/ci:2")]).unwrap();
        assert!(r.status == 404 && words_of(&r).contains("There is no image gone here (`superci runner_images list`)"), "{}", words_of(&r));
        // What it says is checked as a new one's is; what it leaves out is kept, so it is checked too.
        for (fields, says) in [(&[("name", "rust-ci"), ("reference", "https://ghcr.io/acme/ci")][..], "Check the image's address"), (&[("name", "rust-ci"), ("modal_secret", "a secret")], "Check the Modal Secret's name")] {
            let r = update(&mut d, fields).unwrap();
            assert!(r.status == 400 && words_of(&r).contains(says), "{fields:?}: {}", words_of(&r));
        }
        // A new tag, a Secret taken off (empty), or both: each goes as far as the one write, without a sign-in to Modal.
        for fields in [&[("name", "rust-ci"), ("reference", "ghcr.io/acme/ci:2")][..], &[("name", "rust-ci"), ("modal_secret", "")], &[("name", "rust-ci"), ("reference", "ghcr.io/acme/ci@sha256:0f1e"), ("modal_secret", "ghcr-other")]] {
            assert_eq!(update(&mut d, fields).err().as_deref(), Some("Sign in with Modal first."), "{fields:?}");
        }
        // A control plane from before images of one's own has none to change.
        let mut d = modal_answering(serde_json::json!({ "routing": {} }));
        assert_eq!(update(&mut d, &[("name", "rust-ci"), ("reference", "ghcr.io/acme/ci:2")]).unwrap().status, 409);
    }

    #[test]
    fn the_kept_control_plane_is_read_with_its_key_also_when_it_is_listed_no_more() {
        let url = plane_answering(serde_json::json!({ "jobs": [] }));
        let mut d = Dashboard::new();
        d.kept_plane = Some(aws_plane("p0", &url));
        d.keyed.insert("p0".into());
        d.aws_ended = true;
        let v = d.current().unwrap().unwrap();
        assert!(v.status.is_some() && d.planes.is_empty());
    }

    #[test]
    fn gitlabs_projects_say_how_their_webhooks_stand() {
        let url = serving(|_, path, _| match path {
            "/gitlab/projects" => (200, serde_json::json!({ "last_event_ms": now_ms() - 120_000, "projects": [
                { "id": 1, "path": "acme/paused", "enabled": true, "hook_status": "temporarily_disabled", "hook_paused_until": "2026-10-25T12:00:00Z" },
                { "id": 2, "path": "acme/off", "enabled": true, "hook_status": "disabled" }] }).to_string()),
            _ => (500, "{}".into()),
        });
        let mut d = Dashboard::new();
        let v = seen_as(aws_plane("p0", &url), serde_json::json!({ "gitlab": { "url": "https://gitlab.com" } }));
        let page = plain(&d.gitlab_page(Some(&v)));
        assert!(page.contains("GitLab paused its webhook after failed deliveries, until 2026-10-25 12:00") && page.contains("GitLab disabled its webhook") && page.contains("Last event from GitLab 2m ago"), "{page}");
        let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "gitlab": { "url": "https://gitlab.com" } }));
        assert!(plain(&d.gitlab_page(Some(&v))).contains("GitLab did not answer"));
        d.gitlab_shown = "nope".into();
        assert!(plain(&d.gitlab_page(Some(&v))).contains("GitLab is not connected"));
    }

    #[test]
    fn pages_say_what_is_rare() {
        let mut d = Dashboard::new();
        // Setting a control plane up in one AWS account of two signed in to, none looked at yet.
        d.aws = Some(aws::Session::for_test_until("111111111111", u64::MAX));
        d.aws_others = vec![aws::Session::for_test_until("222222222222", u64::MAX)];
        let actions = d.setup_actions(&[]);
        assert!(actions[1].2.as_deref().is_some_and(|a| a.contains(r#"<option value="111111111111">account 111111111111</option><option value="222222222222">"#)), "{actions:?}");
        let in_first = Plane::Aws { account_id: "111111111111".into(), region: "us-east-1".into(), url: NOWHERE.into(), plane_id: "p0".into(), label: "superci".into() };
        assert!(d.setup_actions(&[in_first])[1].2.as_deref().is_some_and(|a| a.contains(r#"<input type="hidden" name="account" value="222222222222">"#)));
        // Keys: one that ends tomorrow, and no device listed.
        let keys = plain(&d.keys_card(Some(&serde_json::json!({ "read": [{ "name": "AGENT", "until_ms": now_ms() + 86_400_000 + 60_000 }] }))));
        assert!(keys.contains("agentRead key: reads only · 1 more day"), "{keys}");
        // Every control plane: none but one only seen from afar.
        assert!(!d.all_page(&[seen_as(seen_plane("p3", NOWHERE), serde_json::json!({}))]).contains("pick act"));
        // A run still running.
        d.run_shown = ("acme/web".into(), "7".into(), String::new());
        let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "jobs": [{ "job_id": 1, "run_id": 7, "repo": "acme/web", "state": "running", "at_ms": now_ms(), "name": "test" }] }));
        assert!(d.run_page(Some(&v)).contains("Running"));
        // What a GitLab connection's newer version asks for.
        let old = PlaneView { version: Some("0.1.0".into()), ..seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "gitlab": { "url": "https://gitlab.com" } })) };
        assert!(!new_needs(&old, &[]).iter().any(|(cloud, _)| *cloud == "gitlab"), "GitLab's scopes are all from the first version");
        // Places of other kinds, and what each can run.
        let v = seen_as(modal_plane("p2", NOWHERE), serde_json::json!({}));
        assert_eq!(pool_text(&v, &Pool::of("modal")).1, "Modal");
        assert_eq!(pool_text(&v, &Pool::of("machine:mac")).2, "");
        assert!(pool_caps("modal").as_array().is_some_and(|a| a.len() == 1) && pool_caps("machine:mac").as_array().is_some_and(|a| a.is_empty()));
        // Clouds: AWS's account unsaid (a control plane elsewhere that does not say it), and none at all.
        let v = PlaneView { aws: true, ..seen_as(modal_plane("p2", NOWHERE), serde_json::json!({})) };
        assert!(clouds_card(&v, &[]).contains("<strong>AWS</strong>"));
        assert!(!clouds_card(&seen_as(modal_plane("p2", NOWHERE), serde_json::json!({})), &[]).contains("pool still"));
        // The default machine, in words.
        for (machine, said) in [(serde_json::json!({ "disk_mibs": superci_core::spec::DISK_MIBS_INCLUDED, "image": "ubuntu24", "docker_cache": false }), ["standard disk speed", "Ubuntu 24.04", "no Docker image cache"]),
            (serde_json::json!({ "image": "ubuntu26", "docker_cache": true }), ["Ubuntu 26.04", "Docker image cache from the start", "spot"]),
            (serde_json::json!({ "os": "windows" }), ["Windows", "x64", "spot"]), (serde_json::json!({ "os": "macos" }), ["macOS", "x64", "spot"])] {
            let card = plain(&machines_card(&seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "machine": machine }))).0);
            for s in said { assert!(card.contains(s), "{machine}: {s} in {card}") }
        }
        // Networks: a region with nothing after it.
        assert!(given_networks("us-east-1").unwrap_err().contains("name at least one subnet and one security group"));
        // Jobs before there is a control plane, and a week or a month with none.
        assert!(plain(&jobs_card(None, 5)).contains("Once setup is done"));
        let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "jobs": [], "usage": { "day": { "jobs": 0 }, "week": { "jobs": 0 }, "month": { "jobs": 0 } } }));
        let long_ago = [serde_json::json!({ "job_id": 1, "run_id": 1, "repo": "acme/web", "state": "done", "at_ms": 1 })];
        assert!(plain(&overview_cards(&v, &long_ago, "week")).contains("No jobs in the last 7 days") && plain(&overview_cards(&v, &long_ago, "month")).contains("No jobs in the last 30 days"));
    }

    #[test]
    fn logs_and_pieces_that_do_not_come_say_why() {
        let url = serving(|_, path, _| match path {
            "/missing" => (404, Vec::new()),
            "/job/log" => (200, br#"{"url":"https://example.invalid/steps"}"#.to_vec()),
            _ => (500, Vec::new()),
        });
        assert_eq!(fetch_all(&[format!("{url}/missing")]).unwrap_err(), "the bucket answered 404 Not Found for a piece");
        assert!(fetch_all(&[format!("{NOWHERE}/piece")]).is_err());
        assert!(fetch_all(&[cut_short()]).is_err(), "a piece cut short is no piece");
        // A job without its number or its name has no steps' logs to read; nor one whose control plane does not
        // answer, gives no address, or one its logs are not at.
        let test = serde_json::json!({ "job_id": 1, "name": "test" });
        assert!(step_logs(&url, "k", &serde_json::json!({ "name": "test" })).is_none() && step_logs(&url, "k", &serde_json::json!({ "job_id": 1 })).is_none());
        assert!(step_logs(NOWHERE, "k", &test).is_none() && step_logs(&url, "k", &test).is_none());
        let plain_http = serving(|_, _, _| (200, br#"{"url":"http://example.invalid/steps"}"#.to_vec()));
        assert!(step_logs(&plain_http, "k", &test).is_none());
        // GitHub's answer about a name: none from this computer, one that is no text, no JSON, or without the name.
        assert!(github_owner_at(NOWHERE, None, "acme").is_err());
        let api = serving(|_, path, _| match path {
            "/users/broken" => (200, vec![0xff, 0xfe]),
            "/users/nojson" => (200, b"not json".to_vec()),
            _ => (200, b"{}".to_vec()),
        });
        assert!(github_owner_at(&api, None, "broken").is_err() && github_owner_at(&api, None, "nojson").is_err());
        assert_eq!(github_owner_at(&api, None, "nameless").unwrap_err(), "GitHub did not answer about that name; try again in a minute");
    }

    #[test]
    fn an_app_that_github_converts_without_its_parts_is_refused() {
        assert!(convert_manifest_code(NOWHERE, None, "abcdefgh").is_err());
        let api = serving(|_, path, _| match path.split('/').nth(2).unwrap_or_default() {
            "broken000" => (201, vec![0xff, 0xfe]),
            "nojson000" => (201, b"not json".to_vec()),
            "noid00000" => (201, br#"{"slug":"s","pem":"p","webhook_secret":"w"}"#.to_vec()),
            "noslug000" => (201, br#"{"id":1,"pem":"p","webhook_secret":"w"}"#.to_vec()),
            "nopem0000" => (201, br#"{"id":1,"slug":"s","webhook_secret":"w"}"#.to_vec()),
            "nosecret0" => (201, br#"{"id":1,"slug":"s","pem":"p"}"#.to_vec()),
            _ => (201, br#"{"id":1,"slug":"s","pem":"p","webhook_secret":"w","owner":{"login":"acme","type":"Organization"}}"#.to_vec()),
        });
        assert!(convert_manifest_code(&api, None, "broken000").is_err() && convert_manifest_code(&api, None, "nojson000").is_err());
        for (code, part) in [("noid00000", "id"), ("noslug000", "slug"), ("nopem0000", "pem"), ("nosecret0", "webhook_secret")] {
            assert_eq!(convert_manifest_code(&api, None, code).err(), Some(format!("App conversion without {part}")), "{code}");
        }
        assert!(convert_manifest_code(&api, None, "complete0").is_ok_and(|a| a.owner_is_org && a.slug == "s"));
    }

    #[test]
    fn a_name_is_checked_only_in_an_account_signed_in_to() {
        // Not signed in to the control plane's AWS account: no other name there is known, and the name is written
        // with that account's sign-in (none).
        let mut d = Dashboard::new();
        d.planes = vec![aws_plane("p0", &plane_answering(serde_json::json!({ "name": "SuperCI" })))];
        assert_eq!(send(&mut d, "/name", &[("name", "Acme CI")]).err().as_deref(), Some("Sign in with AWS first."));
    }

    #[test]
    fn permissions_say_where_in_githubs_settings_and_skip_a_token_not_read_yet() {
        let d = Dashboard::new();
        let v = seen_as(modal_plane("p2", NOWHERE), serde_json::json!({ "permissions": {},
            "apps": [{ "id": 1, "slug": "superci-acme", "owner": "acme", "org": true, "permissions": { "actions": "write", "administration": "write", "metadata": "read" } }],
            "gitlabs": [{ "id": "", "url": "https://gitlab.com" }] }));
        let (card, dialogs) = d.permissions_card(&v);
        assert!(dialogs.contains("Organization permissions → Self-hosted runners → Read and write") && !card.contains("Its token"), "{card}");
    }

    #[test]
    fn control_planes_in_every_cloud_not_signed_in_to_offer_no_cloud_to_set_up_in() {
        let d = Dashboard::new();
        let views: Vec<PlaneView> = [cf_plane("p1", NOWHERE), aws_plane("p0", NOWHERE), modal_plane("p2", NOWHERE)].into_iter().map(|p| seen_as(p, serde_json::json!({}))).collect();
        assert!(!d.planes_page(&views).contains("pick act idle"));
    }

    #[test]
    fn a_places_budget_and_a_repository_without_its_name_are_shown() {
        let v = PlaneView { aws: true, ..seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "routing": { "order": [{ "cloud": "aws", "monthly_usd": 50.0, "max_jobs": 4 }] } })) };
        let card = pools_card(&v, &HashMap::new(), None, None, None);
        assert!(card.contains(r#"name="usd" min="1" step="1" placeholder="No limit" value="50""#) && card.contains("4 at once"), "{card}");
        let cache = plain(&cache_card(&v, Some(&serde_json::json!({ "now_ms": 1, "repos": [{ "id": "gh123", "entries": 1, "bytes": 10, "used_ms": 1 }] }))));
        assert!(cache.contains("Repository 123"), "{cache}");
    }

    #[test]
    fn a_clouds_search_that_stopped_is_said_as_that() {
        let found: Result<u8> = std::thread::scope(|s| searched(s.spawn(|| -> Result<u8> { panic!("a search that broke") }), "Modal"));
        assert_eq!(found.unwrap_err(), "Modal's search stopped");
        // A control plane's keys, when it does not answer, or answers with what is no JSON.
        assert!(plane_keys(NOWHERE).is_err());
        assert!(plane_keys(&serving(|_, _, _| (200, "not json"))).is_err());
    }

    /// `superci login modal --add`: another Modal workspace goes beside the first (the first stays the one the
    /// control plane is written with), is kept with it, and is a provider of its own (`modal:<workspace>`) with its
    /// launcher, limits and removal.
    #[test]
    fn further_modal_workspaces_are_kept_beside_the_first_and_each_is_a_provider_of_its_own() {
        let f = Folder::new("modal-more");
        let store = Store::at(f.0.join("workspaces/default"));
        let mut d = Dashboard::signed_in(Some(store.clone()));
        d.modal = Some(modal_session("acme"));
        // Signed in again with --add: not done until one comes back; to another workspace, it goes beside the first.
        (d.adding, d.task) = (true, Some(Task::Login(Some("modal"))));
        assert!(d.task_done().is_none());
        d.came_modal(modal_session("acme-two"));
        d.came_modal(modal_session("acme-three"));
        // The same workspace again: its token is replaced, not listed twice.
        d.came_modal(modal::Session { token_id: "ak-new".into(), ..modal_session("acme-two") });
        assert_eq!((d.modal.as_ref().map(|m| m.workspace.as_str()), d.modal_others.iter().map(|m| m.workspace.as_str()).collect::<Vec<_>>()), (Some("acme"), vec!["acme-three", "acme-two"]));
        assert!(d.task_done().is_some_and(|t| t.contains("Modal workspace acme, Modal workspace acme-three, Modal workspace acme-two")), "{:?}", d.task_done());
        assert_eq!(d.other_accounts_signed_in().iter().filter(|(c, _)| *c == "modal").count(), 3);
        // Each place in the order has its workspace's sign-in.
        assert_eq!((d.modal_for("modal").map(|m| m.workspace.as_str()), d.modal_for("modal:acme-two").map(|m| m.token_id.as_str()), d.modal_for("modal:globex").is_none()), (Some("acme"), Some("ak-new"), true));
        // Kept, and read back as they were.
        d.keep();
        let again = Dashboard::signed_in(Some(store.clone()));
        assert_eq!((again.modal.as_ref().map(|m| m.workspace.clone()), again.modal_others.iter().map(|m| m.workspace.clone()).collect::<Vec<_>>()), (Some("acme".into()), vec!["acme-three".to_string(), "acme-two".into()]));
        // Without --add, a sign-in is the first, as before.
        d.adding = false;
        d.came_modal(modal_session("globex"));
        assert_eq!((d.modal.as_ref().map(|m| m.workspace.as_str()), d.modal_others.len()), (Some("globex"), 2));
        // A workspace not signed in to: said, nothing deployed.
        d.planes = vec![cf_plane("p1", NOWHERE)];
        assert_eq!(send(&mut d, "/runners/modal", &[("account", "initech")]).err().as_deref(), Some("Sign in with Modal to the workspace initech first: `superci login modal --add`."));
        // Its runners: a row in the order and under Clouds, with Modal's logo, its own limit, and its own removal.
        let (mut d, plane) = (Dashboard::new(), modal_plane("p1", NOWHERE));
        (d.modal, d.modal_others) = (Some(modal_session("acme")), vec![modal_session("acme-two"), modal_session("acme-three")]);
        d.planes = vec![plane.clone()];
        let status = serde_json::json!({ "containers": true, "own_cloud": "modal", "launchers": [{ "cloud": "modal:acme-two", "url": "https://acme-two--superci-runners-p1.modal.run" }],
            "routing": { "rules": [], "order": [{ "cloud": "modal" }, { "cloud": "modal:acme-two", "monthly_usd": 25.0 }] }, "jobs": [], "permissions": {} });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status.clone()) };
        let html = d.render("runners", &[view.clone()], None);
        assert!(html.contains(r#"data-cloud="modal:acme-two""#) && html.contains("<strong>Modal acme-two</strong>") && html.matches(r#"action="/runners/remove""#).count() == 2, "{html}");
        assert!(provider_connected(&status, "modal:acme-two") && !provider_connected(&status, "modal:acme-three"));
        let start = html.find(r#"id="pool-remove-101""#).unwrap();
        let dialog = &html[start..start + html[start..].find("</dialog>").unwrap()];
        assert!(dialog.contains("superci-runners-p1 in your Modal workspace") && dialog.contains(r#"<button class="button danger sm">Remove</button>"#) && dialog.contains(r#"value="modal:acme-two""#), "{dialog}");
        // Add runners: each further workspace a row; the one added says so, the other adds its own launcher.
        let add = d.render("add-runners", &[view], None);
        assert!(add.contains("in the Modal workspace acme-two: a provider of its own") && add.contains(r#"<input type="hidden" name="account" value="acme-three"><button class="button primary sm">Add</button>"#), "{add}");
        // A place's name: a cloud, or a further Modal workspace's.
        assert!(pool_name_ok("modal:acme-two") && pool_name_ok("modal") && !pool_name_ok("modal:") && !pool_name_ok("modal:a b") && !pool_name_ok("gcp:x"));
        assert_eq!((pool_caps("modal:acme-two"), logo("modal:acme-two", 32)), (pool_caps("modal"), logo("modal", 32)));
    }

    #[test]
    fn changes_for_another_kind_of_control_plane_or_without_its_sign_in_are_refused() {
        let mut d = Dashboard::new();
        d.planes = vec![modal_plane("p2", NOWHERE)];
        let r = send(&mut d, "/runners/aws-own", &[]).unwrap();
        assert!(r.status == 400 && words_of(&r).contains("Not a control plane in AWS"));
        assert_eq!(send(&mut d, "/runners/modal-own", &[]).err().as_deref(), Some("Sign in with Modal first."));
        assert_eq!(send(&mut d, "/aws/connect", &[("region", "us-east-1")]).err().as_deref(), Some("Sign in with AWS first"));
    }

    #[test]
    fn every_control_plane_says_whether_its_cloud_is_signed_in_to() {
        let serving_its_own = |p: Plane| PlaneView { github: true, ..seen_as(p, serde_json::json!({})) };
        let views = [serving_its_own(cf_plane("p1", NOWHERE)), serving_its_own(modal_plane("p2", NOWHERE)), serving_its_own(aws_plane("p0", NOWHERE))];
        let mut d = Dashboard::new();
        let signed = |d: &Dashboard| plain(&d.all_page(&views)).matches("Signed in").count();
        assert_eq!(signed(&d), 0);
        d.cf = Some(cloudflare::Session::from_token("test"));
        d.modal = Some(modal_session("acme"));
        d.aws = Some(aws::Session::for_test_until("123456789012", u64::MAX));
        assert_eq!(signed(&d), 3);
        // Another account's or workspace's sign-in is none to these.
        d.cf_accounts = vec![("a2".into(), "Globex".into())];
        d.modal = Some(modal_session("globex"));
        assert_eq!(signed(&d), 1);
    }

    #[test]
    fn a_secret_given_to_a_run_by_no_one_named_was_given_to_a_computer() {
        let card = plain(&secrets_card(Some(&serde_json::json!({ "now_ms": 120_000, "secrets": [{ "name": "NPM_TOKEN", "said": "every run", "updated_ms": 1, "used": { "at_ms": 60_000 } }] }))));
        assert!(card.contains("started by a computer"), "{card}");
    }

    #[test]
    fn a_change_names_its_control_plane_in_its_form_and_forgetting_the_last_but_one_workspace_leaves_the_other_first() {
        let f = Folder::new("hub-rest");
        for w in ["acme", "globex"] { Store::under(&f.0, w).make().unwrap() }
        let hub = hub_at(Some(f.0.clone()), Some("globex"));
        // (Named in its form, `c`, as the pages send it beside a change: one not kept here is none to change.)
        let (status, said) = ask(&hub, "/routing", "w=acme&c=p0&action=order");
        assert_eq!(status, 404, "a control plane not kept here: {said}");
        assert_eq!(hub.remove("globex").unwrap(), "/?w=acme&c=");
        assert_eq!(hub.first().as_deref(), Some("acme"));
    }

    #[test]
    fn on_demand_regions_are_said_in_their_order_beside_spots() {
        let mut d = Dashboard::new();
        d.aws = Some(aws::Session::for_test_until("111111111111", u64::MAX));
        let look = aws::Look { regions: REGIONS.iter().map(|r| (r.to_string(), aws::Answer::Allowed)).collect(), asked_in: Some("us-east-1".into()), spot: Some(aws::Answer::Allowed), on_demand: Some(aws::Answer::Allowed),
            spot_cpus: Some(96), on_demand_cpus: Some(64), images: vec![], organization_id: None, management_account: None };
        d.aws_looks.insert("111111111111".into(), (now_ms(), look));
        let status = serde_json::json!({ "aws": { "account_id": "111111111111", "region": "us-east-1", "connected": true }, "aws_regions": ["us-east-1", "us-east-2", "us-west-2"],
            "routing": { "order": [{ "cloud": "aws", "regions": ["us-east-1", "us-east-2", "us-west-2"] }, { "cloud": "aws-on-demand", "regions": ["us-east-2", "us-west-2"] }] } });
        let v = PlaneView { aws: true, ..seen_as(Plane::Aws { account_id: "111111111111".into(), region: "us-east-1".into(), url: NOWHERE.into(), plane_id: "p0".into(), label: "superci".into() }, status) };
        let section = plain(&d.aws_account_section_for(Some(&v)));
        assert!(section.contains("Spot when cheaper · on-demand first") && section.contains("Spot when cheaper · on-demand when Ohio has none"), "{section}");
    }

    #[test]
    fn rarer_states_of_keys_runs_machines_caches_and_places_are_said() {
        let mut d = Dashboard::new();
        // Keys ending today and in some days; one used by a run key.
        let keys = plain(&d.keys_card(Some(&serde_json::json!({ "read": [{ "name": "TODAY", "until_ms": now_ms() + 3_600_000 }, { "name": "LATER", "until_ms": now_ms() + 5 * 86_400_000 + 60_000, "runs": true }] }))));
        assert!(keys.contains("ends today") && keys.contains("5 more days") && keys.contains("Run key"), "{keys}");
        let card = plain(&secrets_card(Some(&serde_json::json!({ "now_ms": 120_000, "secrets": [{ "name": "NPM_TOKEN", "said": "every run", "updated_ms": 1, "used": { "at_ms": 60_000, "by": "AGENT", "run_key": true } }] }))));
        assert!(card.contains("started by the key agent"), "{card}");
        // Runs that passed, failed and were cancelled.
        for (state, word) in [("done", "Passed"), ("failed", "Failed"), ("cancelled", "Cancelled")] {
            d.run_shown = ("acme/web".into(), "7".into(), String::new());
            let mut job = serde_json::json!({ "job_id": 1, "run_id": 7, "repo": "acme/web", "state": state, "at_ms": now_ms(), "name": "test" });
            if state == "done" { job["conclusion"] = "success".into() }
            let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "jobs": [job] }));
            assert!(d.run_page(Some(&v)).contains(word), "{state}");
        }
        // A disk at a speed of its own; a week's usage with none.
        let card = plain(&machines_card(&seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "machine": { "disk_mibs": 500 } }))).0);
        assert!(card.contains("500 MiB/s disk"), "{card}");
        let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "jobs": [], "usage": { "day": { "jobs": 0 } } }));
        assert!(plain(&overview_cards(&v, &[serde_json::json!({ "job_id": 1, "run_id": 1, "repo": "acme/web", "state": "done", "at_ms": 1 })], "day")).contains("No jobs in the last 24 hours"));
        // Docker's images kept, and Cloudflare's place where its location is not said.
        let v = PlaneView { aws: true, ..seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "routing": {} })) };
        assert!(plain(&cache_card(&v, Some(&serde_json::json!({ "now_ms": 1, "repos": [], "images": { "layers": 3, "bytes": 2_000_000 } })))).contains("3 layers kept now (2.0 MB)."));
        let v = seen_as(cf_plane("p1", NOWHERE), serde_json::json!({ "cloudflare_location": null, "containers": true, "routing": { "order": [{ "cloud": "cloudflare" }] } }));
        assert!(pools_card(&v, &HashMap::new(), None, None, None).contains(r#"<option value="enam" selected>"#));
        // What GitHub's App asks for anew of a control plane from before versions were said.
        let first = PlaneView { version: Some("0.0.1".into()), ..seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "gitlab": { "url": "https://gitlab.com" } })) };
        assert!(new_needs(&first, &[]).iter().any(|(cloud, _)| *cloud == "gitlab"));
    }

    #[test]
    fn a_workspace_renamed_from_where_none_is_named_is_the_first_and_forgetting_them_leaves_the_rest_listed() {
        let f = Folder::new("hub-forget");
        for w in ["acme", "globex", "initech"] { Store::under(&f.0, w).make().unwrap() }
        let hub = hub_at(Some(f.0.clone()), Some("acme"));
        let (status, said) = ask(&hub, "/workspaces/rename", "id=umbrella");
        assert!(status < 400, "{status} {said}");
        assert_eq!(hub.first().as_deref(), Some("umbrella"));
        // The first forgotten with two left: none is first; then the rest, to none.
        assert_eq!(hub.remove("umbrella").unwrap(), "/workspaces");
        assert_eq!(hub.first(), None);
        assert_eq!(hub.remove("globex").unwrap(), "/?w=initech&c=");
        assert_eq!(hub.remove("initech").unwrap(), "/");
        assert_eq!(hub.first().as_deref(), Some(crate::store::FIRST));
    }

    #[test]
    fn signed_in_nowhere_the_kept_control_plane_is_read_only_with_its_key() {
        let mut d = Dashboard::new();
        d.kept_plane = Some(aws_plane("p0", NOWHERE));
        // No key handed it, and no sign-in that ended: nothing to read it with.
        assert_eq!(d.current().err().as_deref(), Some(NOT_SIGNED_IN));
        // With its key: read as it answers (here not at all).
        d.keyed.insert("p0".into());
        assert!(d.current().unwrap().is_some_and(|v| !v.online));
    }

    #[test]
    fn a_rule_replaces_the_one_for_its_repository_and_a_place_not_in_the_order_is_none_to_take_out() {
        let mut d = modal_answering(serde_json::json!({ "routing": { "rules": [{ "repo": "acme/web", "cloud": "aws" }], "order": [{ "cloud": "aws" }] } }));
        assert_eq!(send(&mut d, "/routing", &[("action", "add"), ("repo", "acme/web"), ("cloud", "modal")]).err().as_deref(), Some("Sign in with Modal first."));
        let r = send(&mut d, "/routing", &[("action", "take_out"), ("cloud", "modal"), ("current", "aws")]).unwrap();
        assert!(r.status == 400 && words_of(&r).contains("Unknown provider"));
        // A run whose jobs are over, said by no conclusion: done.
        d.run_shown = ("acme/web".into(), "7".into(), String::new());
        let v = seen_as(aws_plane("p0", NOWHERE), serde_json::json!({ "jobs": [{ "job_id": 1, "run_id": 7, "repo": "acme/web", "state": "done", "at_ms": now_ms(), "name": "test" }] }));
        assert!(d.run_page(Some(&v)).contains("Done"));
        // Renaming with no workspace named, none opened first: which one is asked.
        let hub = hub_at(Some(Folder::new("hub-none").0.clone()), None);
        let (status, said) = ask(&hub, "/workspaces/rename", "id=acme");
        assert!(status == 409 && said.contains("Which workspace?"), "{status} {said}");
    }
}
