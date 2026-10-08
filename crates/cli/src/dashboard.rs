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
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use superci_core::crypto::{random_token, safe_eq};
use superci_core::github::{self, app_manifest, manifest_target, Owner};
use superci_core::plane::AUDIENCE;
use superci_core::io::Response;
use superci_core::plane::{Pool, Routing, Rule, AWS_ON_DEMAND, MAX_CPU, MAX_JOB_MINUTES};
use superci_core::spec::{Capacity, Spec};
use superci_core::page::{document, esc, manifest_form, nearby_regions, region_name, REGIONS};

use crate::aws;
use crate::aws_plane;
use crate::cloudflare::{self, Cloudflare, OAUTH_PORT};
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
    cf: Option<cloudflare::Session>,
    /// A sign-in given by name for this run (SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_MODAL_TOKEN_ID): used, never kept.
    cf_given: bool,
    modal_given: bool,
    /// SuperCI's folder on this machine, where its sign-ins are kept (none: nothing is kept), what was last written
    /// there, the control plane in use as kept (for commands, before any cloud was asked), and whether writing failed.
    store: Option<Store>,
    kept_as: String,
    kept_plane: Option<Plane>,
    keep_failed: bool,
    /// A kept AWS sign-in turned out to have ended (AWS ends one after twelve hours at most).
    aws_ended: bool,
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
    aws_pending: Option<aws::Pending>,
    /// When the sign-in with AWS under way was started (see `aws_stuck`).
    aws_asked_ms: u64,
    modal: Option<modal::Session>,
    modal_pending: Option<modal::Pending>,
    /// The page a sign-in started from, to come back to (`?p=…`).
    return_to: String,
    /// "Nowhere yet": the connect screen asks where SuperCI should live, and the deploy card offers to put it there.
    show_setup: bool,
    /// An App being made on GitHub: the state it comes back with, whose it is, and where that GitHub is (none: github.com).
    manifest_state: Option<(String, Owner, Option<String>)>,
    /// A control plane being deployed in the background (the page shows its steps), or one that stopped.
    deploying: Arc<Mutex<Option<Deploy>>>,
    /// A deploy just finished: the page that showed it goes to Overview, once.
    deployed: Arc<Mutex<bool>>,
    /// A move to another control plane, under way in the background (its steps), or how it ended.
    moving: Arc<Mutex<Option<Move>>>,
    /// An update of a control plane, under way in the background (its steps), or how it ended.
    updating: Arc<Mutex<Option<Update>>>,
    /// Spot CPU quotas per region for the AWS account signed in (read from AWS, kept a few minutes while this runs).
    quotas: HashMap<String, Result<u32>>,
    quotas_for: Option<(String, u64)>,
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
struct Update { plane: String, steps: &'static [&'static str], at: usize, result: Option<Result<()>>, ended_ms: u64 }

impl Update {
    /// Uploaded, and its control plane still answers with the version before: its cloud is switching it over (with
    /// containers that can take a few minutes). Not asked to update again meanwhile; after ten minutes, it is.
    fn restarting(&self, v: &PlaneView) -> bool {
        self.plane == v.plane.plane_id() && matches!(self.result, Some(Ok(()))) && v.outdated() && now_ms() < self.ended_ms + 10 * 60_000
    }
}

const UPDATE_CLOUDFLARE: [&str; 3] = ["Uploading the new version", "Giving it its address", "Waiting for the new version"];
const UPDATE_AWS: [&str; 6] = ["Updating its role", "Checking its table", "Uploading the new function", "Checking its address", "Checking its schedule", "Waiting for the new version"];
const UPDATE_MODAL: [&str; 3] = ["Building the new version", "Deploying it", "Waiting for the new version"];

/// A move: to which control plane, the step it is at, and how it ended (with what did not come along).
struct Move { to: String, at: usize, result: Option<Result<Vec<String>>>, ended_ms: u64 }

const MOVE_STEPS: [&str; 4] = ["Copying your settings and history", "Bringing your runner providers", "Switching GitHub and GitLab over", "Checking it answers"];

/// Writes control planes' secrets from a background thread, with the sign-ins it was given.
struct Writer { cf: Option<cloudflare::Cloudflare>, aws: Option<superci_core::aws::Credentials>, modal: Option<modal::Session> }

impl Writer {
    fn put(&self, plane: &Plane, name: &str, value: &str) -> Result<()> {
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cf.as_ref().ok_or("Sign in with Cloudflare first.")?.put_secret(account_id, script, name, value),
            Plane::Aws { region, plane_id, .. } => aws_plane::put_secret(self.aws.as_ref().ok_or("Sign in with AWS first.")?, region, plane_id, name, value),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, value),
            Plane::Seen { .. } => Err("Sign in where this control plane runs first.".into()),
        }
    }

    /// Removes a setting (Modal's is emptied: its settings are entries of one Dict).
    fn drop(&self, plane: &Plane, name: &str) -> Result<()> {
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

/// The sidebar's update card as last drawn, so each page's frame has it at once (asking the control plane comes after).
#[cfg(not(test))]
fn side() -> &'static Mutex<String> { static SIDE: Mutex<String> = Mutex::new(String::new()); &SIDE }
/// Tests run side by side: each its own.
#[cfg(test)]
fn side() -> &'static Mutex<String> { thread_local!(static SIDE: &'static Mutex<String> = Box::leak(Box::new(Mutex::new(String::new())))); SIDE.with(|s| *s) }

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
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let (mut length, mut cookie, mut last, mut host, mut origin, mut fetch_site) = (0usize, None, None, String::new(), None, None);
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
                cookie = find("superci_local=");
                last = find("superci_last=").filter(|c| CLOUDS.contains(&c.as_str()));
            }
            _ => {}
        }
    }
    let mut body = vec![0u8; length.min(64 * 1024)];
    reader.read_exact(&mut body).ok()?;
    let url = url::Url::parse(&format!("http://local{target}")).ok()?;
    Some(Req { method, path: url.path().to_string(), query: url.query_pairs().into_owned().collect(), cookie, last, body, host, origin, fetch_site })
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

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// The keys that only read, as a control plane's status lists them: each one's name and when it ends (ms).
pub fn read_keys(status: &serde_json::Value) -> Vec<(String, u64)> {
    status["read_keys"].as_array().into_iter().flatten().filter_map(|k| Some((k["name"].as_str()?.to_string(), k["until_ms"].as_u64()?))).collect()
}

/// A day, as 2026-10-07 (UTC), from unix seconds: when a key ends, as commands say it.
pub fn day_of(unix: u64) -> String { day(unix) }

/// What a command waits for after starting it (see `Dashboard::wait`).
#[derive(Clone, Copy)]
pub enum Background { Deploy, Update, Move }

/// A job as commands say it: how it stands, what it is (its name, repository and workflow), the runner it got, how
/// long it ran, what it cost, and why (when it failed or waits): the words the Jobs page has.
pub fn job_words(j: &serde_json::Value) -> serde_json::Value {
    let state = match j["state"].as_str().unwrap_or_default() { "done" => "done", "failed" | "orphan" | "swept" => "failed", "cancelled" => "cancelled", "running" => "running", "waiting" => "waiting", _ => "starting" };
    let (title, sub) = job_names(j);
    let why = if ["failed", "waiting"].contains(&state) { plain_reason(j) } else { String::new() };
    serde_json::json!({ "id": j["job_id"], "state": state, "job": title, "in": sub, "runner": runner_text(j).1, "cloud": j["cloud"], "took": duration(j), "usd": job_cost(j).filter(|c| *c > 0.0), "why": why, "link": job_link(j), "at_ms": j["at_ms"] })
}

/// A control plane as commands say it.
pub fn plane_json(v: &PlaneView) -> serde_json::Value {
    serde_json::json!({ "id": v.plane.plane_id(), "cloud": v.plane.cloud(), "where": v.plane.place(), "url": v.plane.url(), "label": v.plane.label(),
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

/// When a key ends, from the name it is stored under in a control plane (`DASHBOARD_KEY_<unix seconds>_<random>`).
fn key_until(name: &str) -> u64 { name.strip_prefix("DASHBOARD_KEY_").and_then(|r| r.split('_').next()?.parse().ok()).unwrap_or(0) }

fn ago(at_ms: u64) -> String {
    let s = now_ms().saturating_sub(at_ms) / 1000;
    match s { 0..=59 => "just now".into(), 60..=3599 => format!("{}m ago", s / 60), 3600..=86399 => format!("{}h ago", s / 3600), _ => format!("{}d ago", s / 86400) }
}

fn region_options() -> String { region_select_options("us-east-1", &HashMap::new(), &closest(None)) }

/// Why one place is best for runners: where the code hosts connected are (GitHub's Actions and gitlab.com both run
/// in the eastern US). A GitLab of your own is wherever it is: not counted.
fn closest(v: Option<&PlaneView>) -> String {
    let gh = v.is_none_or(|v| v.github);
    let gl = v.and_then(|v| v.gitlab_url()).is_some_and(|u| u.contains("://gitlab.com"));
    match (gh, gl) { (true, true) => "closest to GitHub and GitLab", (false, true) => "closest to GitLab", _ => "closest to GitHub" }.into()
}

/// Where Cloudflare's containers can start (Durable Object location hints), or where Cloudflare chooses.
const CF_LOCATIONS: [(&str, &str); 10] = [("auto", "Automatic: Cloudflare chooses"), ("enam", "Eastern North America"), ("wnam", "Western North America"), ("sam", "South America"),
    ("weur", "Western Europe"), ("eeur", "Eastern Europe"), ("apac", "Asia-Pacific"), ("oc", "Oceania"), ("afr", "Africa"), ("me", "Middle East")];

/// Regions by their full names, with what this account may run there when known.
fn region_select_options(selected: &str, quotas: &HashMap<String, Result<u32>>, best: &str) -> String {
    REGIONS.iter().map(|r| {
        let quota = match quotas.get(*r) { Some(Ok(n)) => format!(" · up to {n} CPUs"), _ => String::new() };
        let best = if *r == "us-east-1" { format!(" · {best}") } else { String::new() };
        format!(r#"<option value="{r}"{}>{} · {r}{best}{quota}</option>"#, if *r == selected { " selected" } else { "" }, region_name(r))
    }).collect()
}

/// A region's short name: "N. Virginia" for us-east-1.
fn region_short(region: &str) -> String {
    match region {
        "us-east-1" => "N. Virginia", "us-east-2" => "Ohio", "us-west-2" => "Oregon", "ca-central-1" => "Canada", "eu-west-1" => "Ireland", "eu-west-2" => "London",
        "eu-central-1" => "Frankfurt", "eu-north-1" => "Stockholm", "ap-south-1" => "Mumbai", "ap-northeast-1" => "Tokyo", "ap-southeast-1" => "Singapore", "ap-southeast-2" => "Sydney",
        other => other,
    }.to_string()
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

fn jobs_word(n: u32) -> String { if n == 1 { "1 job".into() } else { format!("{n} jobs") } }

/// The mark: four broad petals, one solid shape (the same outline as on superci.dev).
const MARK: &str = r#"<svg class="mark" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M4.31 12.77L4.58 12.25L4.56 11.66L4.40 11.34L3.58 10.29L3.10 9.39L2.72 8.33L2.51 7.12L2.51 5.99L2.69 4.85L3.16 3.56L3.52 3.18L4.53 2.79L5.54 2.56L6.65 2.48L7.69 2.58L8.65 2.81L9.52 3.15L10.48 3.71L11.42 4.45L11.97 4.61L12.56 4.46L13.62 3.64L14.61 3.10L15.56 2.75L16.65 2.53L17.89 2.50L19.04 2.67L20.42 3.15L20.82 3.52L21.21 4.53L21.44 5.54L21.52 6.65L21.42 7.69L21.19 8.65L20.85 9.52L20.29 10.48L19.55 11.42L19.39 11.97L19.54 12.56L20.36 13.62L20.90 14.61L21.28 15.65L21.47 16.65L21.51 17.81L21.33 19.04L20.85 20.42L20.48 20.82L19.47 21.21L18.46 21.44L17.35 21.52L16.31 21.42L15.35 21.19L14.50 20.86L13.60 20.34L12.58 19.55L11.99 19.39L11.42 19.55L10.40 20.34L9.50 20.86L8.57 21.21L7.69 21.42L6.65 21.52L5.54 21.44L4.53 21.21L3.56 20.84L3.16 20.44L2.69 19.15L2.51 18.01L2.52 16.79L2.72 15.67L3.10 14.61L3.64 13.62L4.31 12.77Z"/></svg>"#;
const THEME_TOGGLE: &str = r#"<button class="toggle" onclick="superciTheme()" aria-label="Switch light or dark"><svg viewBox="0 0 24 24"><path d="M20.2 15.1A8.4 8.4 0 0 1 8.9 3.8 8.5 8.5 0 1 0 20.2 15.1Z"/></svg></button>"#;

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
        job.usd_per_hour = match job.cloud.as_str() { "cloudflare" => Some(superci_core::plane::CLOUDFLARE_CONTAINER_USD_PER_HOUR), "modal" => Some(superci_core::plane::MODAL_SANDBOX_USD_PER_HOUR), _ => None };
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
        let (kind, word) = match state.as_str() {
            "done" => ("good", "Done"), "failed" | "orphan" | "swept" => ("bad", "Failed"), "cancelled" => ("", "Cancelled"), "running" => ("accent", "Running"),
            "waiting" => ("open", "Waiting"), _ => ("open", "Starting"),
        };
        let (title, sub) = job_names(j);
        let host = if j["provider"] == "gitlab" { logo("gitlab", 14) } else { logo("github", 14) };
        let (runner_logo, runner) = runner_text(j);
        let runner = if runner.is_empty() { r#"<span class="faint">—</span>"#.to_string() } else { format!("{}<span>{}</span>", logo(&runner_logo, 16), esc(&runner)) };
        let took = duration(j);
        let cost = job_cost(j).filter(|c| *c > 0.0).map(|c| cost_text(j, c)).unwrap_or_default();
        let why = if kind == "bad" || state == "waiting" { format!(r#"<small class="jt-why {kind}">{}</small>"#, esc(&plain_reason(j))) } else { String::new() };
        format!(r#"<a class="jt-row" href="{}" target="_blank" rel="noopener"><span class="jt-job"><strong>{}</strong><small>{host}{}</small>{why}</span><span class="jt-runner">{runner}</span><span class="jt-num">{took}</span><span class="jt-num">{cost}</span><span class="jt-state {kind}"><i></i>{word}</span><span class="jt-when">{}</span></a>"#,
            esc(&job_link(j)), esc(&title), esc(&sub), ago(j["at_ms"].as_u64().unwrap_or_default()))
    }).collect::<String>();
    format!(r#"<div class="jt"><div class="jt-head"><span>Job</span><span>Runner</span><span class="jt-num">Time</span><span class="jt-num">Cost</span><span>Status</span><span class="jt-when"></span></div>{rows}</div>"#)
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

/// The runner a job got, in words: its provider's logo and its size ("4 CPU · 12 GB"), or the AWS machine's type.
fn runner_text(j: &serde_json::Value) -> (String, String) {
    let cloud = j["cloud"].as_str().unwrap_or_default();
    let kind = j["machine_type"].as_str().unwrap_or_default();
    match cloud {
        "" => (String::new(), String::new()),
        // From before computers were set aside (coming soon).
        "machine" => ("machine".into(), "Your computer".into()),
        // An on-demand machine says so (its label asked, or the order sent it there).
        "aws" => ("aws".into(), format!("{}{}", match j["cpu"].as_u64() { Some(c) if !kind.is_empty() => format!("{c} CPU · {kind}"), _ => kind.to_string() }, if j["on_demand"] == true && !kind.is_empty() { " · on-demand" } else { "" })),
        _ => {
            // "4cpu-12gb" as "4 CPU · 12 GB".
            let size = kind.split_once("cpu-").and_then(|(c, r)| Some(format!("{c} CPU · {} GB", r.strip_suffix("gb")?)));
            (cloud.to_string(), size.unwrap_or_else(|| cloud_name(cloud).to_string()))
        }
    }
}

/// Why a job did not run (or waits), in plain words: what the control plane recorded, without its machinery.
fn plain_reason(j: &serde_json::Value) -> String {
    let state = j["state"].as_str().unwrap_or_default();
    let place = cloud_name(j["cloud"].as_str().unwrap_or_default()).to_string();
    let Some(e) = j["error"].as_str() else {
        return match state {
            "swept" => "Its machine never started the job".into(),
            "orphan" => "Its machine was lost".into(),
            "waiting" => "Waiting for a free runner".into(),
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
        Dashboard { key: random_token(24), base: format!("http://localhost:{OAUTH_PORT}"), planes: vec![], selected: 0, looked_cf: false, looked_aws: false, looked_modal: false,
            status_key: random_token(24), status_secret: format!("DASHBOARD_KEY_{}_{}", now_ms() / 1000 + 12 * 3600, superci_core::crypto::random_id(6).to_uppercase()), keyed: HashSet::new(), cf_given: cf.is_some(), modal_given: modal.is_some(), store: None, kept_as: String::new(), kept_plane: None, keep_failed: false, aws_ended: false, task: None, commanding: false, preloaded: None, notice: None, cf, cf_pending: None, cf_accounts: vec![], aws: None, aws_pending: None, aws_asked_ms: 0, gitlab_shown: String::new(), modal, modal_pending: None, return_to: String::new(), show_setup: false,
            manifest_state: None, github_expected: None, deploying: Arc::new(Mutex::new(None)), deployed: Arc::new(Mutex::new(false)), moving: Arc::new(Mutex::new(None)), updating: Arc::new(Mutex::new(None)), quotas: HashMap::new(), quotas_for: None, seen: HashMap::new(), flash: None, views: None, refreshing: false, measured_at: 0, measuring: false, cf_month: None, modal_month: None, aws_missing: Arc::new(Mutex::new(HashMap::new())), answered: HashSet::new(), first_asked: HashMap::new(), last_good: HashMap::new() }
    }

    /// A dashboard that remembers: with SuperCI's own sign-ins from its folder, kept there again as they change. A
    /// sign-in given by name for this run comes first.
    pub fn signed_in(store: Option<Store>) -> Self {
        let mut d = Dashboard::new();
        let Some(store) = store else { return d };
        let kept = store.read();
        if d.cf.is_none() { d.cf = kept.cloudflare }
        d.aws = kept.aws;
        if d.modal.is_none() { d.modal = kept.modal }
        d.aws_ended = kept.aws_ended && d.aws.is_none();
        // The key the control planes were handed, while it lasts another day; else a new one, good for thirty days.
        match kept.key.filter(|k| key_until(&k.name) > now_ms() / 1000 + 86_400) {
            Some(k) => { d.status_key = k.value; d.status_secret = k.name; d.keyed = k.planes.into_iter().collect() }
            None => d.status_secret = format!("DASHBOARD_KEY_{}_{}", now_ms() / 1000 + 30 * 86_400, superci_core::crypto::random_id(6).to_uppercase()),
        }
        // The control plane in use is listed at once: its pages are read with the kept key, whether or not its
        // cloud's sign-in still stands (AWS ends one after twelve hours).
        if let Some(p) = &kept.plane { d.planes.push(p.clone()); d.preloaded = Some(p.plane_id().to_string()) }
        d.kept_plane = kept.plane;
        d.store = Some(store);
        d.kept_as = serde_json::to_string(&d.kept()).unwrap_or_default();
        d
    }

    /// A dashboard opened for one thing a person does in the browser.
    pub fn for_task(mut self, task: Task) -> Self { self.task = Some(task); self }

    fn signed_in_with(&self, cloud: &str) -> bool { match cloud { "aws" => self.aws.is_some(), "cloudflare" => self.cf.is_some(), _ => self.modal.is_some() } }

    /// The task is done: what to say in the terminal.
    fn task_done(&self) -> Option<String> {
        let kept = || self.store.as_ref().map(|s| s.path().display().to_string()).unwrap_or_else(|| "nothing (no home folder)".into());
        match self.task.as_ref()? {
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

    /// What the task's tab shows at the dashboard's address, in the dashboard's place.
    fn task_page(&mut self) -> Option<Response> {
        if self.task_done().is_some() { return Some(self.done_page()) }
        // Modal's page does not come back with the sign-in: it is asked for here until it is approved there.
        if matches!(self.task, Some(Task::Login(Some("modal")))) {
            if let Some(pending) = self.modal_pending.clone() {
                if let Ok(Some(session)) = modal::wait(&pending, 1.0) {
                    (self.modal, self.modal_pending, self.looked_modal) = (Some(session), None, false);
                    return Some(self.done_page())
                }
                return Some(back_to("Approve it in Modal's tab", "This tab goes on by itself once you have.", &format!("{}/", self.base)))
            }
        }
        match self.task.as_mut()? {
            // A sign-in with one cloud goes straight to it; with any, the dashboard's own first screen asks which.
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
    pub fn ready(&mut self) -> Result<()> {
        self.signed_in_as().ok_or(if self.aws_ended { AWS_ENDED } else { NOT_SIGNED_IN })?;
        self.discover()?;
        if self.signed_in_as().is_none() { return Err(if self.aws_ended { AWS_ENDED.into() } else { NOT_SIGNED_IN.into() }) }
        let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
        if !views.is_empty() {
            let id = views[view::in_use(&views)].plane.plane_id().to_string();
            if let Some(i) = self.planes.iter().position(|p| p.plane_id() == id) { self.selected = i }
        }
        self.keep();
        Ok(())
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
        // A change that needs AWS, after AWS ended the sign-in kept here: said as that.
        let ended = |e: String| if self.aws_ended && e.contains("Sign in with AWS") { AWS_ENDED.to_string() } else { e };
        let answer = answer.map_err(ended)?;
        let said = SAID.with(|s| s.borrow_mut().take()).map(|(_, heading, text)| { let text = plain(&text); if text.is_empty() { heading } else { format!("{}: {text}", heading.trim_end_matches('.')) } });
        match (answer.status, said) {
            (300..=399, _) => Ok(self.flash.take().unwrap_or_default()),
            (status, Some(said)) if status >= 400 => Err(ended(said)),
            (status, None) if status >= 400 => Err(format!("refused ({status})")),
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
                    (d.steps.iter().map(|s| s.to_string()).collect(), d.at, d.result.as_ref().map(|r| r.as_ref().map(|p| format!("The control plane is running: {} ({})", p.url(), p.place())).map_err(|e| e.clone()))) }
                Background::Update => { let g = lock(&self.updating); let u = g.as_ref().ok_or("no update was started")?;
                    (u.steps.iter().map(|s| s.to_string()).collect(), u.at, u.result.as_ref().map(|r| r.as_ref().map(|_| format!("Updated to {DASHBOARD_VERSION}.")).map_err(|e| e.clone()))) }
                Background::Move => { let g = lock(&self.moving); let m = g.as_ref().ok_or("no move was started")?;
                    (MOVE_STEPS.iter().map(|s| s.to_string()).collect(), m.at, m.result.as_ref().map(|r| r.as_ref().map(|left| if left.is_empty() { "Moved.".to_string() } else { format!("Moved. Did not come along: {}.", left.join("; ")) }).map_err(|e| e.clone()))) }
            };
            // Each step as it begins; all of them once it has ended well.
            let upto = if ended.as_ref().is_some_and(|e| e.is_ok()) { steps.len() } else { (at + 1).min(steps.len()) };
            while said < upto { say(&steps[said]); said += 1 }
            if let Some(ended) = ended {
                if matches!(what, Background::Deploy) && ended.is_ok() {
                    self.adopt_deploy();
                    self.key_planes();
                    // The new one joins the list; the one in use stays the one in use (and the one kept), unless
                    // this is the first.
                    let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
                    if let Some(i) = views.get(view::in_use(&views)).and_then(|v| self.planes.iter().position(|p| p.plane_id() == v.plane.plane_id())) { self.selected = i }
                }
                self.views = None;
                self.keep();
                return ended
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    /// The control plane in use, read for a command: with the kept key when its control plane has it (no cloud is
    /// asked), else after looking in the clouds signed in to. None: signed in, and no control plane found.
    pub fn current(&mut self) -> Result<Option<PlaneView>> {
        // Signed in nowhere: only what is left of a sign-in AWS ended can still be read (with the kept key).
        let only_reading = self.signed_in_as().is_none();
        if only_reading && !(self.aws_ended && self.kept_plane.is_some()) { return Err(NOT_SIGNED_IN.into()) }
        let kept = self.kept_plane.clone().filter(|p| self.keyed.contains(p.plane_id()));
        if let Some(v) = kept.as_ref().map(|p| view::plane_view(p, Some(&self.status_key))).filter(|v| v.status.is_some()) { return Ok(Some(v)) }
        // Nothing to find it again with: as it answers now (not at all, or without taking the key any more).
        if only_reading { return match self.kept_plane.clone() { Some(p) => Ok(Some(view::plane_view(&p, Some(&self.status_key)))), None => Err(AWS_ENDED.into()) } }
        // Not known yet, or its key is gone there: found again (and handed the key) with the sign-ins.
        self.keyed.clear();
        self.discover()?;
        if self.planes.is_empty() { return if self.signed_in_as().is_none() { Err(if self.aws_ended { AWS_ENDED.into() } else { NOT_SIGNED_IN.into() }) } else { Ok(None) } }
        let views: Vec<PlaneView> = self.planes.iter().map(|p| view::plane_view(p, None)).collect();
        let p = views[view::in_use(&views)].plane.clone();
        if let Some(i) = self.planes.iter().position(|x| x.plane_id() == p.plane_id()) { self.selected = i }
        let mut v = view::plane_view(&p, Some(&self.status_key));
        // Just handed the key: its whole status, once it takes it (a few tries).
        for _ in 0..8 { if !v.online || v.status.is_some() || !self.keyed.contains(p.plane_id()) { break } std::thread::sleep(Duration::from_millis(1500)); v.status = cloudflare::status(p.url(), &self.status_key) }
        self.keep();
        Ok(Some(v))
    }

    /// Every control plane found in the clouds signed in to, the one in use marked.
    pub fn all(&mut self) -> Result<(Vec<PlaneView>, usize)> {
        self.ready()?;
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
        if let Some(a) = &self.aws { at.push(format!("AWS account {}", a.account_id)) }
        if self.cf.is_some() { at.push(if self.cf_given { "Cloudflare (a token given for this run)".to_string() } else { "Cloudflare".to_string() }) }
        if let Some(m) = &self.modal { at.push(format!("Modal workspace {}", m.workspace)) }
        if at.is_empty() { None } else { Some(at.join(", ")) }
    }

    /// A key that only reads, for a coding agent or a script that should look and not change: made here, its SHA-256
    /// written into the control plane's secrets (through the cloud's API, like every setting), and shown once. The
    /// key, and when it ends (unix seconds).
    pub fn create_key(&mut self, name: &str, days: u64) -> Result<(String, u64)> {
        let plane = self.plane()?;
        let name = name.to_ascii_uppercase();
        if name.is_empty() || name.len() > 24 || !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) { return Err("A key's name is letters and digits, 24 at most (agent, ci2).".into()) }
        if !(1..=366).contains(&days) { return Err("A key lasts from 1 to 366 days.".into()) }
        let seen = view::plane_view(&plane, Some(&self.status_key));
        if view::older(seen.version.as_deref(), "0.11.0") { return Err("Update the control plane first: keys that only read need control plane 0.11.0 or newer (`superci planes update`).".into()) }
        let status = seen.status.or_else(|| status_soon(plane.url(), &self.status_key)).ok_or("The control plane did not answer: try again in a few seconds")?;
        if read_keys(&status).iter().any(|(n, _)| *n == name) { return Err(format!("There is a key named {} already: `superci keys delete {}` first.", name.to_lowercase(), name.to_lowercase())) }
        let (key, until) = (format!("superci_read_{}", random_token(32)), now_ms() / 1000 + days * 86_400);
        self.put_secret(&plane, &format!("READ_KEY_{until}_{name}"), &superci_core::crypto::sha256_hex(key.as_bytes()))?;
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

    /// `superci logout`: what is kept on this machine is removed, after the clouds that can end a sign-in were asked
    /// to (Cloudflare), and the control plane in use was asked to forget this machine's key. What was done, in words.
    pub fn logout(mut self) -> Result<Vec<String>> { self.sign_out() }

    /// Signs SuperCI out on this machine (the dashboard's Sign out, and `superci logout`): see `logout`. Afterwards
    /// this dashboard knows no cloud and no control plane, as on a first start.
    fn sign_out(&mut self) -> Result<Vec<String>> {
        let Some(store) = self.store.clone() else { return Ok(vec!["Nothing is kept on this machine (it has no home folder).".into()]) };
        let mut said = vec![];
        if store.read().key.is_some() {
            if let Some(p) = self.kept_plane.clone().filter(|p| self.keyed.contains(p.plane_id())) {
                let writer = Writer { cf: self.cf.as_mut().and_then(|s| s.client().ok()), aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone() };
                match writer.drop(&p, &self.status_secret) {
                    Ok(()) => said.push(format!("The control plane ({}) no longer takes this machine's key.", p.place())),
                    Err(_) => said.push(format!("The control plane ({}) still has this machine's key, which only reads; it ends by itself by {}.", p.place(), day(key_until(&self.status_secret)))),
                }
            }
        }
        if let Some(refresh) = self.cf.as_ref().filter(|_| !self.cf_given).and_then(|s| s.refresh_token()) {
            said.push(if cloudflare::end_sign_in(refresh) { "Cloudflare ended SuperCI's sign-in.".into() } else { "Cloudflare did not confirm ending SuperCI's sign-in; it is removed here.".to_string() });
        }
        if self.modal.is_some() && !self.modal_given { said.push("SuperCI's Modal token is removed here; it stays listed in Modal (Settings → API tokens) until deleted there.".into()) }
        if self.aws.is_some() { said.push("SuperCI's AWS sign-in is removed here; AWS ends it by itself within twelve hours of when it was made.".into()) }
        said.push(if store.remove()? { format!("Removed {}.", store.path().display()) } else { "Nothing was kept on this machine.".into() });
        // As on a first start: no sign-in (but one given by name for this run), no control plane, a new key.
        if !self.cf_given { self.cf = None }
        if !self.modal_given { self.modal = None }
        self.aws = None;
        (self.aws_ended, self.looked_cf, self.looked_aws, self.looked_modal) = (false, false, false, false);
        (self.planes, self.selected, self.kept_plane, self.preloaded, self.views) = (vec![], 0, None, None, None);
        self.cf_accounts.clear();
        self.keyed.clear();
        self.status_key = random_token(24);
        self.status_secret = format!("DASHBOARD_KEY_{}_{}", now_ms() / 1000 + 30 * 86_400, superci_core::crypto::random_id(6).to_uppercase());
        self.kept_as = serde_json::to_string(&self.kept()).unwrap_or_default();
        Ok(said)
    }

    /// What is to be kept, as things are now.
    fn kept(&self) -> Kept {
        let mut planes: Vec<String> = self.keyed.iter().cloned().collect();
        planes.sort();
        Kept {
            cloudflare: if self.cf_given { None } else { self.cf.clone() },
            aws: self.aws.clone(),
            modal: if self.modal_given { None } else { self.modal.clone() },
            key: Some(Key { name: self.status_secret.clone(), value: self.status_key.clone(), planes }),
            plane: self.planes.get(self.selected).filter(|p| !matches!(p, Plane::Seen { .. })).cloned().or_else(|| self.kept_plane.clone()),
            aws_ended: self.aws_ended && self.aws.is_none(),
        }
    }

    /// Keeps the sign-ins as they are now (a new one, a renewed one, one that ended), when they changed. Signed in
    /// nowhere: nothing is kept.
    fn keep(&mut self) {
        let Some(store) = self.store.clone() else { return };
        let kept = self.kept();
        let text = serde_json::to_string(&kept).unwrap_or_default();
        if text == self.kept_as { return }
        let done = if kept.worth_keeping() { store.write(&kept) } else { store.remove().map(|_| ()) };
        match done {
            Ok(()) => self.kept_as = text,
            Err(e) => if !self.keep_failed { eprintln!("{e}"); self.keep_failed = true },
        }
    }

    /// Serves the dashboard until you quit it (or Ctrl-C), opening it in the browser.
    pub fn serve(mut self, open_browser: bool) -> Result<()> {
        // "localhost" can mean either loopback address to a browser: listen on both.
        let (tx, rx) = mpsc::channel::<TcpStream>();
        let mut listening = 0;
        // Another port (SUPERCI_PORT) is for trying a second dashboard: Cloudflare's browser sign-in needs 8976.
        let port: u16 = std::env::var("SUPERCI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(OAUTH_PORT);
        if port != OAUTH_PORT { self.base = format!("http://localhost:{port}") }
        for addr in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            let Ok(listener) = TcpListener::bind(&addr) else { continue };
            listening += 1;
            let tx = tx.clone();
            std::thread::spawn(move || for s in listener.incoming().flatten() { if tx.send(s).is_err() { break } });
        }
        if listening == 0 { return Err(format!("port {port} is in use (is another SuperCI or `wrangler login` running?)")) }
        let link = format!("{}/?k={}", self.base, self.key);
        println!("{}: {link}", match &self.task { Some(Task::Login(_)) => "Sign in with your cloud here (on this machine only)", Some(Task::GitHub { .. }) => "Create the GitHub App and choose its repositories here (on this machine only)", None => "SuperCI dashboard (on this machine only)" });
        if self.task.is_some() {
            if open_browser { let _ = open::that(&link); }
        } else if open_browser {
            println!("It is opening in your browser. Ctrl-C stops it; your runners keep working without it.");
            let _ = open::that(&link);
        } else {
            println!("Ctrl-C stops it; your runners keep working without it.");
        }
        // One thread per connection, each with a read timeout: a browser's idle spare connection cannot hold up a page.
        // The page frame needs no shared state, so it is drawn at once; the live part holds it only briefly.
        let key = self.key.clone();
        let for_task = self.task.is_some();
        let shared = Arc::new(Mutex::new(self));
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
                        let mut d = lock(&shared);
                        (d.modal, d.modal_pending, d.looked_modal) = (Some(session), None, false);
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
        for stream in rx {
            let (shared, key) = (shared.clone(), key.clone());
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let Some(req) = read_request(&mut stream) else { return };
                write_response(&mut stream, &respond(&shared, &key, &req));
            });
        }
        Ok(())
    }

    fn cloudflare(&mut self) -> Result<Cloudflare> {
        self.cf.as_mut().ok_or("Sign in with Cloudflare first.")?.client()
    }

    fn plane(&self) -> Result<Plane> { self.planes.get(self.selected).cloned().ok_or_else(|| "no control plane yet".to_string()) }

    /// Writes one of a control plane's secrets through its cloud's API (a Worker secret; an SSM parameter).
    fn put_secret(&mut self, plane: &Plane, name: &str, value: &str) -> Result<()> {
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.put_secret(account_id, script, name, value),
            Plane::Aws { region, plane_id, .. } => aws_plane::put_secret(&self.aws_creds()?, region, plane_id, name, value),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, value),
            Plane::Seen { .. } => Err("This control plane is known only from AWS: sign in where it runs to change it.".into()),
        }
    }

    /// Removes a setting from the control plane (Modal's are emptied: its store keeps names).
    fn drop_secret(&mut self, plane: &Plane, name: &str) -> Result<()> {
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.delete_secret(account_id, script, name),
            Plane::Aws { region, plane_id, .. } => aws_plane::delete_secret(&self.aws_creds()?, region, plane_id, name),
            Plane::Modal { plane_id, .. } => modal::put_setting(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id, name, ""),
            Plane::Seen { .. } => Err("This control plane is known only from AWS: sign in where it runs to change it.".into()),
        }
    }

    fn aws_creds(&mut self) -> Result<superci_core::aws::Credentials> {
        match self.aws.as_mut().ok_or("Sign in with AWS first.")?.credentials() {
            // Over on AWS's side (they last twelve hours at most): forgotten here, so the pages ask for it again.
            Err(e) if e.starts_with(aws::ENDED) => { self.aws = None; self.views = None; Err(format!("{}. Sign in with AWS again.", aws::ENDED)) }
            r => r,
        }
    }

    /// Looks for control planes in the clouds you signed in to (once per sign-in), and lets the ones in Cloudflare accept this
    /// session's status key.
    fn discover(&mut self) -> Result<()> {
        // Every cloud signed in to and not looked in yet, at once.
        let cf = if self.cf.is_some() && !self.looked_cf { Some(self.cloudflare()?) } else { None };
        let asked = if self.looked_aws { None } else { self.aws.as_mut().map(|s| (s.credentials(), s.account_id.clone())) };
        let aws = match asked {
            Some((Ok(creds), account)) => Some((creds, account)),
            // Over on AWS's side (a sign-in lasts twelve hours at most, and one kept from yesterday has ended):
            // forgotten, so the pages ask for it again.
            Some((Err(e), _)) if e.starts_with(aws::ENDED) => { self.aws = None; self.aws_ended = true; None }
            Some((Err(e), _)) => return Err(e),
            None => None,
        };
        let modal = if self.looked_modal { None } else { self.modal.clone() };
        let accounts = self.cf_accounts.clone();
        let (cf_found, aws_found, modal_found) = std::thread::scope(|s| {
            let c = cf.as_ref().map(|cf| { let accounts = accounts.clone(); s.spawn(move || -> Result<(Vec<(String, String)>, Vec<Plane>)> {
                let accounts = if accounts.is_empty() { timed("Cloudflare: accounts", || cf.accounts())? } else { accounts };
                let planes = timed("Cloudflare: find control planes", || cf.find_planes(&accounts))?;
                Ok((accounts, planes))
            }) });
            let a = aws.as_ref().map(|(creds, _)| s.spawn(move || timed("AWS: find control planes", || aws::connected_planes(creds))));
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
            let join = |h: std::thread::ScopedJoinHandle<'_, _>| h.join().unwrap_or_else(|_| Err::<_, String>("a cloud's search stopped".into()));
            (c.map(join), a.map(|h| h.join().unwrap_or_else(|_| Err("AWS's search stopped".into()))), m.map(|h| h.join().unwrap_or_else(|_| Err("Modal's search stopped".into()))))
        });
        // Where the kept control plane was looked for, and what was found there.
        let (mut looked, mut there): (Vec<String>, Vec<String>) = (vec![], vec![]);
        if let Some(found) = cf_found {
            let (accounts, planes) = found?;
            looked.extend(accounts.iter().map(|(id, _)| format!("cloudflare:{id}")));
            there.extend(planes.iter().map(|p| p.plane_id().to_string()));
            self.cf_accounts = accounts;
            for h in planes {
                match self.planes.iter().position(|x| x.plane_id() == h.plane_id()) { Some(i) => self.planes[i] = h, None => self.planes.push(h) }
            }
            self.looked_cf = true;
        }
        if let (Some(found), Some((_, account_id))) = (aws_found, aws) {
            looked.push(format!("aws:{account_id}"));
            for f in found? {
                if !f.url.starts_with("https://") { continue }
                if f.lambda.is_some() { there.push(f.plane_id.clone()) }
                let found = match f.lambda {
                    Some((region, label)) => Plane::Aws { account_id: account_id.clone(), region, url: f.url, plane_id: f.plane_id, label },
                    None => Plane::Seen { url: f.url, plane_id: f.plane_id },
                };
                // What runs here replaces what was only seen from here; otherwise the first sighting stays.
                match self.planes.iter().position(|p| p.plane_id() == found.plane_id()) {
                    Some(i) if matches!(self.planes[i], Plane::Seen { .. }) => self.planes[i] = found,
                    Some(_) => {}
                    None => self.planes.push(found),
                }
            }
            self.looked_aws = true;
        }
        if let Some(found) = modal_found {
            if let Some(m) = &self.modal { looked.push(format!("modal:{}", m.workspace)) }
            for found in found? {
                there.push(found.plane_id().to_string());
                match self.planes.iter().position(|x| x.plane_id() == found.plane_id()) { Some(i) => self.planes[i] = found, None => self.planes.push(found) }
            }
            self.looked_modal = true;
        }
        // The control plane kept from the last run: looked for where it lives and not there any more, it is gone.
        if let Some(i) = self.preloaded.as_ref().and_then(|id| self.planes.iter().position(|p| p.plane_id() == id)) {
            let home = match &self.planes[i] {
                Plane::Cloudflare { account_id, .. } => format!("cloudflare:{account_id}"),
                Plane::Aws { account_id, .. } => format!("aws:{account_id}"),
                Plane::Modal { workspace, .. } => format!("modal:{workspace}"),
                Plane::Seen { .. } => String::new(),
            };
            if looked.contains(&home) {
                if !there.iter().any(|id| id == self.planes[i].plane_id()) {
                    self.planes.remove(i);
                    self.kept_plane = None;
                    self.selected = 0;
                    self.views = None;
                }
                self.preloaded = None;
            }
        }
        self.key_planes();
        Ok(())
    }

    /// Lets every control plane this dashboard can change accept this session's status key. Earlier sessions' keys that
    /// have expired are removed afterwards, in the background: the page does not wait for that.
    fn key_planes(&mut self) {
        let unkeyed: Vec<Plane> = self.planes.iter().filter(|p| !matches!(p, Plane::Seen { .. }) && !self.keyed.contains(p.plane_id())).cloned().collect();
        if unkeyed.is_empty() { return }
        // Handed to each at once.
        let writer = Writer { cf: self.cf.as_mut().and_then(|s| s.client().ok()), aws: self.aws.as_mut().and_then(|s| s.credentials().ok()), modal: self.modal.clone() };
        let (name, key) = (self.status_secret.clone(), self.status_key.clone());
        let done: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = unkeyed.iter().map(|p| { let (writer, name, key) = (&writer, &name, &key); s.spawn(move || timed(&format!("{}: hand it this session's key", p.place()), || writer.put(p, name, key)).ok().map(|_| p.plane_id().to_string())) }).collect();
            hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
        });
        self.keyed.extend(done);
        let mut tidy: Vec<Box<dyn FnOnce() + Send>> = vec![];
        for p in &unkeyed {
            let expired = |names: Vec<String>| { let now = now_ms() / 1000; names.into_iter().filter(move |n| n.strip_prefix("DASHBOARD_KEY_").and_then(|r| r.split('_').next()?.parse::<u64>().ok()).is_some_and(|until| until < now)) };
            match p.clone() {
                Plane::Cloudflare { account_id, script, .. } => if let Ok(cf) = self.cloudflare() {
                    tidy.push(Box::new(move || for old in expired(cf.secret_names(&account_id, &script).unwrap_or_default()) { let _ = cf.delete_secret(&account_id, &script, &old); }));
                },
                Plane::Aws { region, plane_id, .. } => if let Ok(creds) = self.aws_creds() {
                    tidy.push(Box::new(move || for old in expired(aws_plane::secret_names(&creds, &region, &plane_id).unwrap_or_default()) { let _ = aws_plane::delete_secret(&creds, &region, &plane_id, &old); }));
                },
                // Modal keeps expired keys; the control plane ignores them.
                Plane::Modal { .. } => {}
                Plane::Seen { .. } => {}
            }
        }
        if !tidy.is_empty() { std::thread::spawn(move || for job in tidy { job() }); }
    }

    /// A deploy that finished: its control plane joins the list and is shown (discovery then gives it the key).
    fn adopt_deploy(&mut self) {
        let finished = { let mut g = lock(&self.deploying); if g.as_ref().is_some_and(|x| matches!(x.result, Some(Ok(_)))) { g.take() } else { None } };
        if let Some(Deploy { result: Some(Ok(p)), .. }) = finished {
            self.selected = match self.planes.iter().position(|x| x.plane_id() == p.plane_id()) { Some(i) => i, None => { self.planes.push(p); self.planes.len() - 1 } };
            self.show_setup = false;
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
            let step = |i: usize| if let Some(d) = lock(&state).as_mut() { d.at = i };
            let result = work(&step);
            // Said in the dashboard's terminal; a command says how it ended itself.
            if !quiet { match &result { Ok(p) => eprintln!("The control plane is running: {}", p.url()), Err(e) => eprintln!("The deploy stopped: {e}") } }
            // A stop keeps the step it stopped at; a finish marks them all done.
            if let Some(d) = lock(&state).as_mut() { if result.is_ok() { d.at = d.steps.len() } d.result = Some(result) }
        });
    }

    fn handle(&mut self, req: &Req) -> Result<Response> {
        let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
        // Back to the dashboard after a sign-in elsewhere: a same-site navigation, so the page's cookie goes along
        // (a plain redirect at the end of another site's redirect chain would count as cross-site and lose it).
        let base = self.base.clone();
        let back = |heading: &str, text: &str| back_to(heading, text, &format!("{base}/"));
        let form: Vec<(String, String)> = url::form_urlencoded::parse(&req.body).into_owned().collect();
        let field = |n: &str| form.iter().find(|(k, _)| k == n).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
        let form_has = |n: &str| form.iter().any(|(k, _)| k == n);
        match (req.method.as_str(), req.path.as_str()) {

            // The connect screen's links: each remembers itself as the cloud picked last ("Last used"), in this browser only.
            ("GET", "/connect/cloudflare") => {
                let (link, pending) = cloudflare::authorize();
                self.cf_pending = Some(pending);
                self.return_to.clear();
                Ok(leave(&link, "Cloudflare").with_header("set-cookie", &last_used_cookie("cloudflare")))
            }
            ("GET", "/connect/aws") => {
                let (link, pending) = aws::authorize("us-east-1", &format!("http://127.0.0.1:{OAUTH_PORT}/oauth/callback"));
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
                let plane_id = if again.len() == 12 && again.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) { again } else { superci_core::crypto::random_id(12) };
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
                let Plane::Aws { account_id, .. } = &plane else { return Ok(message(400, "Not a control plane in AWS", "")) };
                if self.aws.as_ref().is_none_or(|a| &a.account_id != account_id) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {}.", esc(account_id)))) }
                let creds = self.aws_creds()?;
                // With its runners off its status lists no regions: its own, then (its network is made in each).
                let Plane::Aws { region, .. } = &plane else { unreachable!() };
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
                if !["aws", "cloudflare", "modal"].contains(&cloud.as_str()) { return Ok(message(400, "Unknown provider", "")) }
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
                // Signed in where its part is, before anything changes.
                if !only {
                    let signed_in = match cloud.as_str() {
                        "aws" => self.aws.as_ref().is_some_and(|a| Some(&a.account_id) == account.as_ref()),
                        _ if own_containers => true,
                        "modal" => self.modal.is_some(),
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
                // 1. Forgotten: out of the order (and any rule naming it), then its connection.
                let mut routing: Routing = serde_json::from_value(status["routing"].clone()).unwrap_or_default();
                routing.order.retain(|p| p.cloud != cloud && !(cloud == "aws" && p.cloud == AWS_ON_DEMAND));
                routing.rules.retain(|r| r.cloud != cloud);
                if routing.default.as_deref() == Some(cloud.as_str()) { routing.default = None }
                self.put_secret(&plane, "ROUTING", &serde_json::to_string(&routing).map_err(|e| e.to_string())?)?;
                if cloud == "aws" {
                    if matches!(plane, Plane::Aws { .. }) { self.put_secret(&plane, "AWS_RUNNERS", "off")? } else { cloudflare::plane_post(plane.url(), &self.status_key, "/aws/forget", &serde_json::json!({}))?; }
                } else if own_containers {
                    self.put_secret(&plane, "CONTAINERS", "off")?;
                } else {
                    let mut agents: Vec<superci_core::plane::Agent> = serde_json::from_value(status["agents"].clone()).unwrap_or_default();
                    agents.retain(|a| a.cloud != cloud);
                    self.put_secret(&plane, "AGENTS", &serde_json::to_string(&agents).map_err(|e| e.to_string())?)?;
                }
                let c = cloud.clone();
                let forgotten = wait_for(plane.url(), &self.status_key, move |s| !provider_connected(s, &c));
                // 2. Its part in that cloud (its machines go with it).
                let id = plane.plane_id().to_string();
                let mut left = vec![];
                match (cloud.as_str(), only) {
                    ("aws", false) => {
                        let creds = aws_creds.as_ref().ok_or("Sign in with AWS first")?;
                        // What lets the control plane start machines goes first: none can start after, whether or
                        // not it has taken the change yet. Then its machines and their network, once more if a
                        // machine started meanwhile.
                        let gone = match &plane {
                            Plane::Aws { region, .. } => aws_plane::delete_runner_policy(creds, region, &id),
                            _ => aws::disconnect_runners(creds, account.as_deref().unwrap_or_default(), plane.url(), &id),
                        };
                        if let Err(e) = gone { left.push(format!("its role ({e})")) }
                        if let Err(e) = aws::delete_networks(creds, &id, &REGIONS).or_else(|_| aws::delete_networks(creds, &id, &REGIONS)) { left.push(format!("its machines' network ({e}); remove AWS again to delete it")) }
                    }
                    // A control plane that has not taken the change yet may still start a runner there: its part is
                    // left until it has (nothing is half-deleted under a running job).
                    (_, false) if !forgotten && !own_containers => left.push(format!("its part in {name}: the control plane had not taken the change yet. Add {name} again and remove it in a moment")),
                    ("aws", true) => left.push(format!("in AWS account {}: the role superci-plane-{id}{}, and its machines' network (none cost anything; add AWS again and remove it while signed in to delete them)",
                        account.as_deref().unwrap_or(""), if matches!(plane, Plane::Aws { .. }) { "'s machines policy" } else { " and its identity provider" })),
                    (_, _) if own_containers => {}
                    ("modal", false) => if let Some(m) = &self.modal { if let Err(e) = modal::delete_runners(m, &id) { left.push(format!("its Modal app superci-runners-{id} ({e})")) } },
                    ("modal", true) => left.push(format!("the Modal app superci-runners-{id} in your workspace")),
                    (_, false) => { let cf = cf.as_ref().ok_or("Sign in with Cloudflare first")?; for (a, _) in self.cf_accounts.clone() { if let Err(e) = cf.delete_runners(&a, &id) { left.push(format!("its Cloudflare runner agent ({e})")) } } }
                    (_, true) => left.push(format!("the Cloudflare runner agent superci-runners-{id}")),
                }
                self.views = None;
                self.flash = Some(format!("Removed {name}.{}", if left.is_empty() { String::new() } else { format!(" Left: {}.", left.join("; ")) }));
                Ok(Response::redirect("/?p=runners"))
            }
            // A Modal control plane starts sandboxes itself: switching them on is one setting.
            ("POST", "/runners/modal-own") => {
                let plane = self.plane()?;
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
                self.aws = Some(aws::exchange(pending, q("code"))?);
                self.aws_ended = false;
                println!("Signed in to AWS.");
                self.looked_aws = false;
                Ok(back_to("Signed in with AWS", "Looking for SuperCI in your account…", &format!("{}/{}", self.base, self.return_to)))
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
                Ok(back_to("Signed in with Cloudflare", "Looking for SuperCI in your accounts…", &format!("{}/{}", self.base, self.return_to)))
            }
            ("POST", "/aws/signin") => {
                self.return_to = safe_return(&field("next"));
                // AWS's developer-tools sign-in allows only this return path (Cloudflare's shares it; `state` tells them apart).
                let (link, pending) = aws::authorize("us-east-1", &format!("http://127.0.0.1:{OAUTH_PORT}/oauth/callback"));
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
                let account_id = self.aws.as_ref().ok_or("Sign in with AWS first.")?.account_id.clone();
                let label = "superci".to_string();
                // Trying again after a stop reuses what the first try created (same id, same names).
                let again = field("plane");
                let plane_id = if again.len() == 12 && again.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) { again } else { superci_core::crypto::random_id(12) };
                let creds = self.aws_creds()?;
                self.start_deploy("aws", format!("account {account_id} · {region}"), &aws_plane::DEPLOY_STEPS, vec![("region", region.clone()), ("plane", plane_id.clone())], move |step| {
                    let deployed = aws_plane::deploy_with(&creds, &account_id, &region, &plane_id, &label, true, step)?;
                    step(5);
                    wait_online(&deployed.url);
                    Ok(Plane::Aws { account_id, region, url: deployed.url, plane_id, label })
                });
                Ok(Response::redirect(if self.planes.is_empty() { "/?p=plane" } else { "/?p=planes" }))
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
                let plan = self.carry_plan(&from_view, &to_view);
                if let Some(Carry::NeedsSignIn(p)) = plan.iter().find(|c| matches!(c, Carry::NeedsSignIn(_))) { return Ok(message(409, &format!("Sign in with {} first", provider_name(p)), "Its runners come along with the move.")) }
                let writer = Writer { cf: self.cf.as_mut().map(|s| s.client()).transpose()?, aws: self.aws.as_mut().map(|s| s.credentials()).transpose()?, modal: self.modal.clone() };
                let aws_account = self.aws.as_ref().map(|a| a.account_id.clone());
                let cf_account = match &from { Plane::Cloudflare { account_id, .. } => Some(account_id.clone()), _ => self.cf_accounts.first().map(|a| a.0.clone()) };
                let state = self.moving.clone();
                *lock(&state) = Some(Move { to: to.plane_id().to_string(), at: 0, result: None, ended_ms: 0 });
                std::thread::spawn(move || {
                    let step = |n: usize| if let Some(m) = lock(&state).as_mut() { m.at = n };
                    let result = move_plane(&writer, &key, &from, &to, from_view.status.unwrap_or_default(), &plan, cf_account, aws_account, &step);
                    if let Some(m) = lock(&state).as_mut() { m.result = Some(result); m.ended_ms = now_ms() }
                });
                Ok(Response::redirect("/?p=planes"))
            }
            // Deleting a control plane not in use (never the one in use; never one a move is under way to).
            ("POST", "/plane/delete") => {
                let plane = self.planes.iter().find(|p| p.plane_id() == field("plane")).cloned().ok_or("That control plane is not known here")?;
                let key = self.status_key.clone();
                if self.plane().is_ok_and(|p| p.plane_id() == plane.plane_id()) { return Ok(message(409, "This control plane is in use", "Move to another one first, or stop using SuperCI.")) }
                if lock(&self.moving).as_ref().is_some_and(|m| m.result.is_none() && m.to == plane.plane_id()) { return Ok(message(409, "A move to it is under way", "")) }
                let status = cloudflare::status(plane.url(), &key).unwrap_or_default();
                let running = status["jobs"].as_array().into_iter().flatten().filter(|j| ["launching", "launched", "running"].contains(&j["state"].as_str().unwrap_or_default())).count();
                if running > 0 { return Ok(message(409, &format!("{running} job{} still running there", if running == 1 { " is" } else { "s are" }), "Delete it when they finish.")) }
                let left = self.delete_plane(&plane, &status)?;
                self.flash = Some(format!("Deleted {}.{}", plane.place(), left.iter().map(|l| format!(" Left: {l}")).collect::<String>()));
                Ok(Response::redirect("/?p=planes"))
            }
            // Stopping SuperCI: GitHub and GitLab stop sending jobs, then every control plane is deleted with what
            // was made for it. The GitHub App itself is deleted on GitHub (it has no way to delete itself).
            ("POST", "/plane/leave") => {
                if field("confirm").trim() != "superci" { return Ok(message(400, "Type superci to confirm", "")) }
                let used = self.plane()?;
                let key = self.status_key.clone();
                let status = cloudflare::status(used.url(), &key).unwrap_or_default();
                let app = status["app"].clone();
                cloudflare::plane_post(used.url(), &key, "/leave", &serde_json::json!({})).map_err(|e| format!("GitHub and GitLab could not be let go of yet ({e}); nothing was deleted."))?;
                let mut left = vec![];
                for p in self.planes.clone() {
                    let st = cloudflare::status(p.url(), &key).unwrap_or_default();
                    match self.delete_plane(&p, &st) { Ok(l) => left.extend(l), Err(e) => left.push(format!("{}: {e}", p.place())) }
                }
                let app_link = match (app["slug"].as_str(), app["owner"].as_str()) {
                    (Some(slug), Some(owner)) => format!(r#"<li>Delete the GitHub App <a href="{}" target="_blank" rel="noopener">{}</a> on GitHub (Advanced → Delete). It is uninstalled already.</li>"#,
                        esc(&format!("{}/advanced", app_settings_link(app["host"].as_str(), owner, slug, app["org"] == true))), esc(slug)),
                    _ => String::new(),
                };
                let more = left.iter().map(|l| format!("<li>{}</li>", esc(l))).collect::<String>();
                Ok(message(200, "SuperCI is stopped", &format!(r#"Its control planes and what was made for them are deleted from your clouds.</p><ul class="checks" style="margin-top:12px">{app_link}<li>Change <code>runs-on: superci</code> back in your workflows (and <code>tags:</code> in GitLab).</li>{more}</ul><p>"#)))
            }
            ("POST", "/plane/update") => {
                // Which: its id (or, from before, its place in the list); none: the one in use.
                let which = field("plane");
                if let Some(i) = self.planes.iter().position(|p| p.plane_id() == which).or_else(|| which.parse::<usize>().ok().filter(|i| *i < self.planes.len())) { self.selected = i }
                let plane = self.plane()?;
                // What it runs now: whether its jobs' containers come along, and whether any would be cut short.
                let Some(status) = status_soon(plane.url(), &self.status_key) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let running = status["jobs"].as_array().into_iter().flatten().filter(|j| j["cloud"] == "cloudflare" && ["launching", "launched", "running"].contains(&j["state"].as_str().unwrap_or_default())).count();
                if running > 0 {
                    return Ok(message(409, &format!("{running} job{} running on Cloudflare", if running == 1 { " is" } else { "s are" }), "Update when they finish: the update gives Cloudflare's containers a new setup, which would stop them."))
                }
                if lock(&self.updating).as_ref().is_some_and(|u| u.result.is_none()) { return Ok(Response::redirect(&format!("/{}", safe_return(&field("next"))))) }
                // In the background, its steps shown where its Update button was (the sidebar, the Control plane page).
                let cf = if matches!(plane, Plane::Cloudflare { .. }) || status["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")) { self.cloudflare().ok() } else { None };
                let creds = if matches!(plane, Plane::Aws { .. }) { Some(self.aws_creds()?) } else { None };
                let session = if matches!(plane, Plane::Modal { .. }) { Some(self.modal.clone().ok_or("Sign in with Modal first.")?) } else { None };
                let cf_account = self.cf_accounts.first().map(|a| a.0.clone());
                let steps: &'static [&'static str] = match plane { Plane::Cloudflare { .. } => &UPDATE_CLOUDFLARE, Plane::Aws { .. } => &UPDATE_AWS, Plane::Modal { .. } => &UPDATE_MODAL,
                    Plane::Seen { .. } => return Ok(message(400, "Sign in where this control plane runs to update it", "")) };
                let state = self.updating.clone();
                *lock(&state) = Some(Update { plane: plane.plane_id().to_string(), steps, at: 0, result: None, ended_ms: 0 });
                let containers = status["containers"] == true;
                let cf_agent = status["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare"));
                // Signed in to the AWS account its machines run in: the update also gives its runner role what this
                // version asks for (an AWS control plane's own update does that already).
                let aws_give = match (&self.aws, status["aws"]["account_id"].as_str()) {
                    (Some(a), Some(account)) if a.account_id == account && !matches!(plane, Plane::Aws { .. }) && status["aws"]["connected"] == true => Some((self.aws_creds()?, account.to_string())),
                    _ => None,
                };
                let regions = status_regions(&status);
                // An AWS control plane whose AWS runners were removed keeps them off.
                let aws_runners = !status["aws"].is_null();
                let (aws_missing, key) = (self.aws_missing.clone(), self.status_key.clone());
                std::thread::spawn(move || {
                    let step = |n: usize| if let Some(u) = lock(&state).as_mut() { u.at = n };
                    let result = (|| -> Result<()> {
                        let url = match &plane {
                            Plane::Cloudflare { account_id, script, label, .. } => cf.as_ref().ok_or("Sign in with Cloudflare first.")?.deploy_with(account_id, script, label, containers, &step)?.url,
                            Plane::Aws { account_id, region, plane_id, label, .. } => aws_plane::deploy_with(creds.as_ref().ok_or("Sign in with AWS first.")?, account_id, region, plane_id, label, aws_runners, &step)?.url,
                            Plane::Modal { plane_id, label, .. } => modal::deploy_plane(session.as_ref().ok_or("Sign in with Modal first.")?, plane_id, label, &step)?,
                            Plane::Seen { .. } => return Err("Sign in where this control plane runs to update it".into()),
                        };
                        // A separate Cloudflare runner agent (for a control plane elsewhere) is updated with it.
                        if let (true, Some(cf), Some(account)) = (cf_agent && !matches!(plane, Plane::Cloudflare { .. }), &cf, &cf_account) { cf.deploy_runners(account, plane.url(), plane.plane_id())?; }
                        // An AWS control plane's machines get their own network in each of its regions too.
                        if let (Plane::Aws { .. }, Some(creds), true) = (&plane, &creds, aws_runners) { aws::make_networks(creds, plane.plane_id(), &regions)?; }
                        if let Some((creds, account)) = &aws_give {
                            aws::give_runner_role(creds, account, plane.plane_id(), &regions)?;
                            let _ = cloudflare::plane_post(&url, &key, "/permissions/given", &serde_json::json!({ "cloud": "aws" }));
                            lock(&aws_missing).insert(plane.plane_id().to_string(), (Ok(vec![]), now_ms()));
                        }
                        // The old version keeps answering for a few seconds after a deploy: wait for this one.
                        step(steps.len() - 1);
                        wait_for_version(&url, DASHBOARD_VERSION);
                        Ok(())
                    })();
                    if result.is_ok() { lock(side()).clear() }
                    if let Some(u) = lock(&state).as_mut() { u.result = Some(result); u.ended_ms = now_ms() }
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
                let mut manifest = app_manifest(plane.url(), &owner, plane.label(), plane.plane_id());
                // GitHub comes back to this machine, not to the control plane: the code is exchanged here.
                manifest["redirect_url"] = format!("{}/github/callback", self.base).into();
                manifest["setup_url"] = format!("{}/github/installed", self.base).into();
                let target = manifest_target(host.as_deref(), &owner, &state);
                self.manifest_state = Some((state, owner, host));
                Ok(manifest_form(&target, &manifest))
            }
            ("GET", "/github/callback") => {
                let fresh = self.manifest_state.as_ref().is_some_and(|(s, ..)| safe_eq(q("state").as_bytes(), s.as_bytes()));
                if !fresh { return Ok(message(400, "This App creation has expired", "Start again from the dashboard.")) }
                let host = self.manifest_state.as_ref().and_then(|(_, _, h)| h.clone());
                let app = convert_manifest_code(&github::api_base(host.as_deref()), host.as_deref(), q("code"))?;
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
                self.put_secret(&plane, &name, &serde_json::to_string(&app).map_err(|e| e.to_string())?)?;
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
                Ok(back("Installed on GitHub", "Back to your dashboard…"))
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
                self.put_secret(&plane, "AWS_REGIONS", &serde_json::to_string(&regions).map_err(|e| e.to_string())?)?;
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
            ("GET", "/oauth/modal") => Ok(back_to("Signed in with Modal", "Back to where you were…", &format!("{}/{}", self.base, self.return_to))),
            ("POST", "/runners/modal") => {
                let plane = self.plane()?;
                let session = self.modal.clone().ok_or("Sign in with Modal first.")?;
                let Some(mut agents) = status_soon(plane.url(), &self.status_key).and_then(|s| serde_json::from_value::<Vec<superci_core::plane::Agent>>(s["agents"].clone()).ok()) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                let keys: serde_json::Value = ureq::get(&format!("{}/.well-known/jwks.json", plane.url())).call().map_err(|e| e.to_string())?.body_mut().read_json().map_err(|e| e.to_string())?;
                eprintln!("Deploying the Modal runner agent (the first time builds a small image in your workspace)…");
                let url = modal::deploy_runners(&session, plane.url(), plane.plane_id(), &keys)?;
                // Its first answer can take a few seconds (a cold start).
                for _ in 0..20 { if ureq::get(&format!("{url}/health")).call().is_ok_and(|r| r.status() == 200) { break } std::thread::sleep(std::time::Duration::from_secs(3)) }
                agents.retain(|a| a.cloud != "modal");
                agents.push(superci_core::plane::Agent { cloud: "modal".into(), url });
                self.put_secret(&plane, "AGENTS", &serde_json::to_string(&agents).map_err(|e| e.to_string())?)?;
                wait_for(plane.url(), &self.status_key, |s| s["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "modal")));
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
                        // A separate Cloudflare agent from before is no longer needed.
                        let mut agents: Vec<superci_core::plane::Agent> = serde_json::from_value(status["agents"].clone()).unwrap_or_default();
                        if agents.iter().any(|a| a.cloud == "cloudflare") {
                            agents.retain(|a| a.cloud != "cloudflare");
                            self.put_secret(&plane, "AGENTS", &serde_json::to_string(&agents).map_err(|e| e.to_string())?)?;
                            let _ = cf.delete_runners(account_id, plane.plane_id());
                        }
                        wait_for(plane.url(), &self.status_key, |s| s["containers"] == true && !s["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")));
                    }
                    // A control plane elsewhere cannot start Cloudflare containers: a small agent in Cloudflare does.
                    _ => {
                        let mut agents: Vec<superci_core::plane::Agent> = serde_json::from_value(status["agents"].clone()).unwrap_or_default();
                        let account = self.cf_accounts.first().map(|a| a.0.clone()).ok_or("no Cloudflare account")?;
                        let url = cf.deploy_runners(&account, plane.url(), plane.plane_id())?;
                        agents.retain(|a| a.cloud != "cloudflare");
                        agents.push(superci_core::plane::Agent { cloud: "cloudflare".into(), url });
                        self.put_secret(&plane, "AGENTS", &serde_json::to_string(&agents).map_err(|e| e.to_string())?)?;
                        wait_for(plane.url(), &self.status_key, |s| s["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == "cloudflare")));
                    }
                }
                Ok(Response::redirect("/?p=runners"))
            }
            // The default machine: what `runs-on: superci` alone gets.
            ("POST", "/machine") => {
                let plane = self.plane()?;
                let num = |n: &str, max: u32| field(n).parse::<u32>().ok().filter(|v| *v > 0 && *v <= max);
                let spec = Spec { cpu: num("cpu", 192), ram_gb: num("ram", 1536), disk_gb: num("disk", 16_000), arch: Some(field("arch")).filter(|a| a == "arm64"),
                    os: Some(field("os")).filter(|o| o == "windows"), gpu: None, cloud: None, on_demand: field("ondemand") == "on" };
                if spec.arch.is_some() && spec.os.is_some() { return Ok(message(400, "Windows machines are x64", "Choose x64 for Windows, or Linux for arm64.")) }
                let want = serde_json::to_value(&spec).map_err(|e| e.to_string())?;
                self.put_secret(&plane, "MACHINE", &want.to_string())?;
                wait_for(plane.url(), &self.status_key, |s| s["machine"] == want);
                Ok(Response::redirect("/?p=runners"))
            }
            // Review → Allow: the control plane's AWS runner role gets what this version asks for.
            ("POST", "/permissions/aws") => {
                let id = field("plane");
                let plane = self.planes.iter().find(|p| p.plane_id() == id).cloned().ok_or("Which control plane?")?;
                let status = status_soon(plane.url(), &self.status_key).ok_or("The control plane did not answer: try again in a few seconds")?;
                let account = match &plane { Plane::Aws { account_id, .. } => account_id.clone(), _ => status["aws"]["account_id"].as_str().unwrap_or_default().to_string() };
                if self.aws.as_ref().is_none_or(|a| a.account_id != account) { return Ok(message(403, "Sign in with AWS first", &format!("Sign in to account {} to give its role these permissions.", esc(&account)))) }
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
                let Some(mut routing) = status_soon(plane.url(), &self.status_key).and_then(|s| serde_json::from_value::<Routing>(s["routing"].clone()).ok()) else {
                    return Ok(message(503, "The control plane did not answer yet", "It is starting with a new setting. Try again in a few seconds."))
                };
                match field("action").as_str() {
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
                    // The whole set, as chosen in the dialog (each checked repository; patterns like owner/* kept).
                    "public_set" => {
                        let chosen: Vec<String> = form.iter().filter(|(k, _)| k == "repo").map(|(_, v)| v.trim().to_string()).collect();
                        let ok = |repo: &str| { let mut p = repo.split('/'); matches!((p.next(), p.next(), p.next()), (Some(o), Some(r), None) if github::valid_login(o) && !r.is_empty() && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))) };
                        if chosen.iter().any(|r| !ok(r)) { return Ok(message(400, "A repository is owner/name", "")) }
                        routing.public_repos.retain(|p| p.ends_with("/*"));
                        for r in chosen { if !routing.public_repos.iter().any(|p| p.eq_ignore_ascii_case(&r)) { routing.public_repos.push(r) } }
                    }
                    // A new order (dragged), each pool keeping its limits.
                    "order" => {
                        let mut order = vec![];
                        for i in 0..64 {
                            let cloud = field(&format!("cloud_{i}"));
                            if cloud.is_empty() { break }
                            if !pool_name_ok(&cloud) || order.iter().any(|p: &Pool| p.cloud == cloud) { return Ok(message(400, "Unknown provider", "")) }
                            order.push(routing.pool(&cloud).cloned().unwrap_or(Pool { off: false, cloud, max_jobs: None, monthly_usd: None }));
                        }
                        routing.order = order;
                        // The order replaces the one default cloud from before.
                        routing.default = None;
                    }
                    // One pool's settings: jobs at once, dollars a month. The order shown (`current`) is kept as it was.
                    "pool" => {
                        let cloud = field("cloud");
                        if !pool_name_ok(&cloud) { return Ok(message(400, "Unknown provider", "")) }
                        let mut order: Vec<Pool> = field("current").split(',').filter(|c| pool_name_ok(c))
                            .map(|c| routing.pool(c).cloned().unwrap_or(Pool { off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None })).collect();
                        if !order.iter().any(|p| p.cloud == cloud) { order.push(Pool { off: false, cloud: cloud.clone(), max_jobs: None, monthly_usd: None }) }
                        // Checked whole before anything changes (a form refused halfway would leave half of it set).
                        let want_image = if cloud == "cloudflare" && form_has("image") {
                            let image = field("image").trim().to_string();
                            Some(if image.is_empty() { None } else { Some(superci_core::plane::image_address(&image).ok_or("The image's address must start with https:// and be a plain address")?) })
                        } else { None };
                        let want_networks = if cloud == "aws" && form_has("networks") { Some(match given_networks(&field("networks")) { Ok(n) => n, Err(e) => return Ok(message(400, "Check your networks", &esc(&e))) }) } else { None };
                        // AWS: networks of the account's own, by region. Looked up with the account's sign-in when
                        // there is one, so a wrong id is said here, not on the first job.
                        if let Some(networks) = want_networks {
                            let st = cloudflare::status(plane.url(), &self.status_key).unwrap_or_default();
                            let want = serde_json::to_value(&networks).map_err(|e| e.to_string())?;
                            if st["aws_networks"] != want && !(networks.is_empty() && st["aws_networks"].as_object().is_none_or(|n| n.is_empty())) {
                                let account = match &plane { Plane::Aws { account_id, .. } => Some(account_id.clone()), _ => st["aws"]["account_id"].as_str().map(str::to_string) };
                                if self.aws.as_ref().is_some_and(|a| Some(&a.account_id) == account.as_ref()) {
                                    let creds = self.aws_creds()?;
                                    let http = aws::Blocking::new();
                                    for (region, n) in &networks {
                                        if let Err(e) = futures::executor::block_on(superci_core::aws::given_network(&http, region, &creds, n, now_ms())) { return Ok(message(400, "Check your networks", &esc(&e))) }
                                    }
                                }
                                self.put_secret(&plane, "AWS_NETWORKS", &want.to_string())?;
                                wait_for(plane.url(), &self.status_key, |s| s["aws_networks"] == want);
                            }
                        }
                        // AWS's regions, in order (the next when one has no spot capacity left).
                        if cloud == "aws" && !field("region_0").is_empty() {
                            let mut regions: Vec<String> = vec![];
                            for i in 0..REGIONS.len() {
                                let r = field(&format!("region_{i}"));
                                if r.is_empty() || regions.contains(&r) { continue }
                                if !REGIONS.contains(&r.as_str()) { return Ok(message(400, "Choose a region from the list", "")) }
                                regions.push(r);
                            }
                            let want = serde_json::to_value(&regions).map_err(|e| e.to_string())?;
                            let have = cloudflare::status(plane.url(), &self.status_key).map(|s| s["aws_regions"].clone()).unwrap_or_default();
                            if have != want {
                                // Signed in to its account: its own network in each, first (else Permissions asks for it).
                                let account = cloudflare::status(plane.url(), &self.status_key).and_then(|s| s["aws"]["account_id"].as_str().map(str::to_string));
                                let given = cloudflare::status(plane.url(), &self.status_key).map(|s| s["aws_networks"].clone()).unwrap_or_default();
                                let own: Vec<String> = regions.iter().filter(|r| given.get(r.as_str()).is_none()).cloned().collect();
                                if self.aws.as_ref().is_some_and(|a| Some(&a.account_id) == account.as_ref()) { aws::make_networks(&self.aws_creds()?, plane.plane_id(), &own)?; }
                                self.put_secret(&plane, "AWS_REGIONS", &want.to_string())?;
                                wait_for(plane.url(), &self.status_key, |s| s["aws_regions"] == want);
                            }
                        }
                        // Cloudflare's location (where its containers start).
                        let location = field("location");
                        if cloud == "cloudflare" && CF_LOCATIONS.iter().any(|(k, _)| *k == location) {
                            let have = cloudflare::status(plane.url(), &self.status_key).and_then(|s| s["cloudflare_location"].as_str().map(str::to_string));
                            if have.as_deref() != Some(location.as_str()) {
                                self.put_secret(&plane, "CF_LOCATION", &location)?;
                                wait_for(plane.url(), &self.status_key, |s| s["cloudflare_location"] == location.as_str());
                            }
                        }
                        // Cloudflare: GitHub's full image, by the address it is published at (empty: the small image).
                        if let Some(want) = want_image {
                            let have = cloudflare::status(plane.url(), &self.status_key).and_then(|s| s["cloudflare_image"].as_str().map(str::to_string));
                            if have != want {
                                self.put_secret(&plane, "CF_IMAGE", want.as_deref().unwrap_or("none"))?;
                                let w = want.clone();
                                wait_for(plane.url(), &self.status_key, move |s| s["cloudflare_image"].as_str().map(str::to_string) == w);
                            }
                        }
                        for p in order.iter_mut().filter(|p| p.cloud == cloud) {
                            // AWS on-demand can be turned off (the others are removed instead).
                            p.off = cloud == AWS_ON_DEMAND && field("on") != "on";
                            p.max_jobs = field("max").parse().ok().filter(|n| *n > 0 && *n <= 999);
                            p.monthly_usd = field("usd").parse::<f64>().ok().filter(|n| *n > 0.0 && n.is_finite());
                        }
                        routing.order = order;
                        routing.default = None;
                    }
                    _ => return Ok(message(400, "Unknown change", "")),
                }
                let want = serde_json::to_value(&routing).map_err(|e| e.to_string())?;
                self.put_secret(&plane, "ROUTING", &want.to_string())?;
                wait_for(plane.url(), &self.status_key, |s| s["routing"] == want);
                Ok(Response::redirect(if field("action").starts_with("public") || ["max_cpu", "max_hours"].contains(&field("action").as_str()) { "/?p=workflows" } else { "/?p=runners" }))
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
var sections={plane:'planes',settings:'planes','add-runners':'runners',gitlab:'repos'};
function frag(){var q=location.search;return '/'+(q?q+'&fragment=1':'?fragment=1')}
function current(){var p=new URLSearchParams(location.search).get('p')||'overview';p=sections[p]||p;document.querySelectorAll('.side .nav a').forEach(function(a){a.setAttribute('aria-current',String(new URL(a.href).searchParams.get('p')===p))})}
var shown='';
function show(h){var to=h.match(/data-go="([^"]+)"/);if(to){go(to[1]);return}document.body.classList.remove('busy');clearTimeout(timer);var ld=h.match(/data-loading="([^"]+)"/),cur=main.querySelector('[data-loading]'),again=h.match(/data-refresh="(\d+)"/);if(h===shown||(ld&&cur&&cur.getAttribute('data-loading')===ld[1])){if(again)timer=setTimeout(load,again[1]*1000);return}shown=h;main.innerHTML=h;main.querySelectorAll('form.picker').forEach(superciPick);if(location.hash){var hd=document.getElementById(location.hash.slice(1));if(hd){if(hd.tagName==='DETAILS')hd.open=true;hd.scrollIntoView({block:'center'})}}document.body.classList.toggle('gated',!!main.querySelector('[data-gated]'));var u=main.querySelector('[data-side]'),su=document.getElementById('side-update'),nx=u?u.innerHTML:'';if(u)u.remove();if(su.innerHTML!==nx)su.innerHTML=nx;document.querySelectorAll('input[name=next]').forEach(function(i){i.value=location.search+(i.dataset.open&&location.search?'&open='+i.dataset.open:'')});var op=new URLSearchParams(location.search).get('open');if(op){var od=document.getElementById(op);if(od&&od.showModal)od.showModal();if(od||!main.querySelector('[data-refresh]')){var ou=new URL(location.href);ou.searchParams.delete('open');history.replaceState(null,'',ou.pathname+ou.search+ou.hash)}}clearTimeout(timer);var t=main.querySelector('[data-refresh]');if(t)timer=setTimeout(load,t.getAttribute('data-refresh')*1000)}
function load(){fetch(frag(),{credentials:'same-origin'}).then(function(r){return r.text()}).then(show).catch(function(){document.body.classList.remove('busy');main.innerHTML='<div class="empty">The dashboard stopped. Run superci dashboard again.</div>'})}
function go(url){var u=new URL(url,location.href);history.pushState(null,'',u.pathname+u.search+u.hash);current();clearTimeout(timer);document.body.classList.add('busy');window.scrollTo(0,0);load()}
document.addEventListener('click',function(e){var a=e.target.closest&&e.target.closest('a[href]');if(!a||a.target||e.defaultPrevented||e.metaKey||e.ctrlKey||e.shiftKey||e.altKey)return;var u=new URL(a.href,location.href);if(u.origin!==location.origin||u.pathname!=='/'||u.searchParams.has('k')||(u.search===location.search&&u.hash))return;e.preventDefault();go(u.href)});
document.addEventListener('submit',function(e){var f=e.target,act=f.getAttribute('action')||'';if(e.defaultPrevented||(f.getAttribute('method')||'').toLowerCase()!=='post'||act.charAt(0)!=='/'||/signin$|^\/github\/start/.test(act))return;e.preventDefault();var d=f.closest('dialog');document.body.classList.add('busy');var b=e.submitter||f.querySelector('button:not([type=button])');if(b&&!b.disabled){b.disabled=true;b.innerHTML='<span class="mini-spin"></span>'+(b.classList.contains('danger')?'Working…':'Saving…');b.classList.add('working')}fetch(act,{method:'POST',body:new URLSearchParams(new FormData(f,e.submitter)),credentials:'same-origin'}).then(function(r){if(d&&d.open)d.close();var u=new URL(r.url);if(r.redirected&&u.origin===location.origin){go(u.href);return}if(r.redirected){location.href=r.url;return}return r.text().then(function(t){document.open();document.write(t);document.close()})}).catch(function(){f.submit()})});
window.addEventListener('popstate',function(){current();document.body.classList.add('busy');load()});
window.addEventListener('pageshow',function(e){if(e.persisted)load()});
load()})();</script>"#;
        document(200, "SuperCI", &format!(r#"<div class="layout">{}<main class="main" id="live">{skeleton}</main></div>{PAGE_JS}{script}"#, Self::sidebar(section)), None)
    }

    /// The live part of a page: control planes (and a pending Modal sign-in) asked now, in parallel, without holding
    /// the dashboard's state meanwhile.
    fn live(shared: &Arc<Mutex<Dashboard>>, section: &str, plane: Option<usize>, new_here: Option<bool>, last: Option<&str>) -> String {
        let lock = || shared.lock().unwrap_or_else(|e| e.into_inner());
        let (planes, key, pending, quota_creds) = {
            let mut d = lock();
            if let Some(i) = plane { if i < d.planes.len() { d.selected = i } }
            if let Some(n) = new_here { d.show_setup = n }
            d.adopt_deploy();
            // Finding control planes after a sign-in asks the cloud once; afterwards this is instant.
            if let Err(e) = d.discover() {
                // A sign-in that ran out (Cloudflare's lasts an hour): forget it and ask again, where the user is.
                if e.contains("(9109)") || e.contains("Invalid access token") || e.contains("sign-in expired") {
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
            let quota_creds = if stale && ["add-runners", "runners"].contains(&section) { d.aws_creds().ok().zip(account) } else { None };
            (d.planes.iter().map(|p| (p.clone(), d.keyed.contains(p.plane_id()))).collect::<Vec<_>>(), d.status_key.clone(), d.modal_pending.clone(), quota_creds)
        };
        // Read a moment ago, and nothing changed since: shown at once, read again in the background. A page shown from
        // what was kept looks again in a moment (`look_again`), so it does not stay on what was true before.
        let mut look_again = false;
        let cached = {
            let mut d = lock();
            let expecting = d.github_expected.is_some_and(|until| now_ms() < until);
            let same = d.views.as_ref().is_some_and(|(v, _)| v.len() == planes.len() && v.iter().zip(&planes).all(|(v, (p, _))| v.plane.plane_id() == p.plane_id()));
            // A move or an update that ended after the reading changed what it shows: read again.
            let ended = d.moving.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.ended_ms).unwrap_or(0)
                .max(d.updating.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|u| u.ended_ms).unwrap_or(0));
            let same = same && d.views.as_ref().is_some_and(|(_, at)| *at > ended);
            // An update that is uploaded while the old version still answers: read anew at every look.
            let restarting = d.updating.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|u| d.views.as_ref().is_some_and(|(views, _)| views.iter().any(|v| u.restarting(v))));
            let same = same && !restarting;
            match d.views.clone() {
                Some((views, at)) if same && pending.is_none() && quota_creds.is_none() && !expecting && now_ms() < at + 120_000 => {
                    look_again = now_ms() > at + 3_000;
                    if now_ms() > at + 3_000 && !d.refreshing {
                        d.refreshing = true;
                        let (shared, planes, key) = (shared.clone(), planes.clone(), key.clone());
                        std::thread::spawn(move || {
                            let views = read_views(&planes, &key);
                            let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
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
        let (views, modal, quotas) = match cached { Some(v) => (v, None, HashMap::new()), None => std::thread::scope(|s| {
            let handles: Vec<_> = planes.iter().map(|(p, keyed)| { let key = key.clone(); s.spawn(move || view::plane_view(p, keyed.then_some(key.as_str()))) }).collect();
            let quotas: Vec<_> = quota_creds.iter().flat_map(|(creds, _)| REGIONS.iter().map(move |r| s.spawn(move || {
                let http = aws::Blocking::new();
                (r.to_string(), futures::executor::block_on(superci_core::aws::spot_cpu_quota(&http, r, creds, now_ms())))
            }))).collect();
            // A Modal sign-in in the browser: asked about briefly on each look until it is approved.
            let modal = pending.as_ref().map(|p| modal::wait(p, 1.0));
            (handles.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<PlaneView>>(), modal, quotas.into_iter().filter_map(|h| h.join().ok()).collect::<HashMap<_, _>>())
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
        let mut d = lock();
        for v in &views { if !d.planes.iter().any(|p| p.plane_id() == v.plane.plane_id()) { d.planes.push(v.plane.clone()) } }
        // Kept to show again only when complete: a control plane still taking this session's key is read again next time.
        let complete = views.iter().all(|v| !v.online || v.status.is_some() || !d.keyed.contains(v.plane.plane_id()));
        let views = d.remember(views);
        d.views = if complete { Some((views.clone(), now_ms())) } else { None };
        Dashboard::measure(shared, &mut d);
        d.check_permissions();
        // The page shows, and changes, the control plane in use.
        if let Some(i) = views.get(view::in_use(&views)).and_then(|v| d.planes.iter().position(|p| p.plane_id() == v.plane.plane_id())) { d.selected = i }
        if let Some((_, account)) = quota_creds { d.quotas = quotas; d.quotas_for = Some((account, now_ms())); }
        if let Some(Ok(Some(session))) = modal { d.modal = Some(session); d.modal_pending = None; d.looked_modal = false; }
        // The App is installed (or the wait is over): nothing more to wait for.
        if d.github_expected.is_some_and(|until| now_ms() >= until) || views.get(view::in_use(&views)).is_some_and(|v| v.github && v.installed) { d.github_expected = None }
        let said_once = d.notice.take().map(|n| format!(r#"<div class="notice"><span>{}</span></div>"#, esc(&n))).unwrap_or_default();
        let html = format!("{said_once}{}{}{}", d.aws_stuck(), d.aws_ended_notice(), d.render(section, &views, last));
        let html = if look_again && !html.contains("data-refresh=") { format!(r#"<div data-refresh="2"></div>{html}"#) } else { html };
        // A change's note is said once.
        if ["planes", "runners"].contains(&section_name(section)) { d.flash = None }
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

    fn render(&self, section: &str, views: &[PlaneView], last: Option<&str>) -> String {
        let section = section_name(section);
        // No cloud signed in to: whether (and where) SuperCI runs is not known yet.
        if self.cf.is_none() && self.aws.is_none() && self.modal.is_none() && views.is_empty() {
            lock(side()).clear();
            // Back from Modal's sign-in: the page looks again until it is approved.
            let gate = self.gate(section, last);
            return if self.modal_pending.is_some() { format!(r#"<div data-refresh="3"></div>{gate}"#) } else { gate }
        }
        let current = views.get(view::in_use(views));
        // A control plane that has not picked up this session's key yet: look again in a moment.
        let settling = current.is_some_and(|v| v.online && v.status.is_none() && !matches!(v.plane, Plane::Seen { .. }));
        // Until then, pages that list what the control plane has would guess (and offer to add what exists): they wait.
        // Overview does not: its public state is enough, and its numbers and jobs fill in when the key is read.
        if settling && !["plane", "overview"].contains(&section) {
            let title = section_title(section);
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
        if let Some(v) = current.filter(|v| !v.online && !self.answered.contains(v.plane.plane_id()) && now_ms() < self.first_asked.get(v.plane.plane_id()).copied().unwrap_or(0) + 30_000) {
            let _ = v;
            return format!(r#"<div data-refresh="2"></div>{}"#, skeleton(section, section_title(section)));
        }
        let (html, waiting) = match section {
            "plane" if !views.is_empty() => (self.planes_page(views), self.deploy_running()),
            "plane" => (self.plane_page(), false),
            "runners" => (self.runners_page(current), false),
            "add-runners" => (self.add_runners_page(current), self.modal_pending.is_some()),
            _ if self.modal_pending.is_some() && current.is_none() => (self.overview_page(current).0, true),
            "jobs" => (format!(r#"<div class="bar"><h1 class="page-title">Jobs</h1></div><div class="cards">{}</div>"#, jobs_card(current, 200)), false),
            "planes" => (self.planes_page(views), self.deploy_running()),
            "changes" => (changes_page(views, &views.get(view::in_use(views)).map(|v| self.aws_lacks(v)).unwrap_or_default()), false),
            "workflows" => (self.workflows_page(current), false),
            "repos" => (self.repos_page(current), false),
            "gitlab" => (self.gitlab_page(current), false),
            _ => self.overview_page(current),
        };
        let updating = lock(&self.updating);
        // Shown while it is older, and also while its update runs (it does not answer for a moment while it restarts,
        // and answers with the new version before the update's last step) and for a little while after it ended.
        let its_update = |v: &PlaneView| updating.as_ref().is_some_and(|u| u.plane == v.plane.plane_id() && (u.result.is_none() || now_ms() < u.ended_ms + 20_000));
        let card = current.filter(|v| v.outdated() || its_update(v)).map(|v| side_update(v, updating.as_ref(), &self.aws_lacks(v))).unwrap_or_default();
        // While an update runs, the page looks again every two seconds (its steps).
        // While Modal's sign-in waits, every page looks again: Modal's page does not come back here by itself, so the
        // dashboard notices the approval wherever you are.
        let waiting = waiting || updating.as_ref().is_some_and(|u| u.result.is_none()) || self.modal_pending.is_some() || self.github_expected.is_some_and(|until| now_ms() < until);
        drop(updating);
        *lock(side()) = card.clone();
        let html = if card.is_empty() { html } else { format!(r#"{html}<div data-side hidden>{card}</div>"#) };
        if waiting || settling { format!(r#"<div data-refresh="{}"></div>{html}"#, if settling { 2 } else { 5 }) } else { html }
    }

    fn sidebar(section: &str) -> String {
        let current = match section { "plane" => "planes", "add-runners" => "runners", "gitlab" => "repos", s => s };
        let link = |id: &str, label: &str, icon: &str| format!(r#"<a href="/?p={id}" aria-current="{}">{icon}<span>{label}</span></a>"#, current == id);
        let nav = format!(r#"<nav class="nav">{}{}{}{}{}</nav>"#, link("overview", "Overview", ICON_OVERVIEW), link("repos", "Repositories", ICON_REPOS), link("runners", "Runners", ICON_RUNNERS),
            link("workflows", "Workflows", ICON_WORKFLOWS), link("jobs", "Jobs", ICON_JOBS));
        // At the bottom: an update for the control plane, when it needs one (filled in by the page's live part), the
        // control plane itself, and what is new.
        // Last, More: what is about this computer and not the control plane (signing SuperCI out here).
        let more = format!(r#"<details class="menu side-more"><summary>{ICON_MORE}<span>More</span></summary><div class="menu-pop"><form method="post" action="/signout"><button>Sign out</button></form></div></details>"#);
        let foot = format!(r#"<div class="side-foot"><div id="side-update">{}</div><nav class="nav">{}{}{more}</nav></div>"#, lock(side()), link("planes", "Control plane", ICON_PLANE), link("changes", "Changelog", ICON_NEWS));
        format!(r#"<aside class="side"><div class="brand">{MARK}<span>SuperCI</span>{THEME_TOGGLE}</div>{nav}{foot}</aside>"#)
    }

    /// Where a control plane runs: reachable from Settings to add another. The connect screen's list: a row per cloud with
    /// its one action (connect, then deploy), the ones not available yet folded under "More clouds".
    fn plane_page(&self) -> String {
        // A deploy has this page to itself while it runs (or once it stopped); when it finished, on to Overview.
        if std::mem::take(&mut *lock(&self.deployed)) { return r#"<div data-go="/"></div>"#.to_string() }
        if let Some(progress) = self.deploy_progress() {
            return format!(r#"<div class="bar"><h1 class="page-title">Setting up your control plane</h1></div><div class="cards">{progress}</div>"#)
        }
        let rows = self.plane_rows();
        format!(r#"<div class="bar"><h1 class="page-title">Set up a control plane</h1></div><p class="note" style="margin-bottom:22px">The always-on part GitHub and GitLab send jobs to, in your own cloud account. Runners can be on other clouds too.</p><div class="picks page">{rows}</div>"#)
    }

    /// Where a control plane can be set up: a row per cloud with its one action (connect, then set up); an account
    /// that has one already is not offered again.
    fn plane_rows(&self) -> String {
        let rows = self.setup_actions(&self.planes).into_iter().map(|(cloud, what, action)| provider_row(cloud, &what, &action.unwrap_or_else(|| pill("good", "Has your control plane")))).collect::<String>();
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
        let aws = match &self.aws {
            Some(a) if planes.iter().any(|p| matches!(p, Plane::Aws { account_id, .. } if *account_id == a.account_id)) => None,
            Some(_) => Some(format!(r#"<form method="post" action="/plane/aws" class="aws-add"><details class="change"><summary class="button secondary sm">Change region</summary><select name="region" aria-label="Region">{}</select></details><button class="button primary sm">Set up</button></form>"#, region_options())),
            None => Some(connect_action("aws")),
        };
        let modal = match &self.modal {
            Some(m) if planes.iter().any(|p| matches!(p, Plane::Modal { workspace, .. } if *workspace == m.workspace)) => None,
            Some(_) => Some(row_form("/plane/modal", "", "Set up")),
            None => Some(connect_action("modal")),
        };
        vec![
            ("cloudflare", format!("A Worker with its own storage{}", signed(self.cf.as_ref().map(|_| cf_free.iter().map(|a| a.1.as_str()).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()))), cloudflare),
            ("aws", format!("A Lambda function with a table{}", signed(self.aws.as_ref().map(|a| format!("account {} · US East (N. Virginia)", a.account_id)))), aws),
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
            Some(Err(e)) => (format!("The deploy to {name} stopped"), esc(e), false),
            Some(Ok(_)) => return None,
        };
        let steps = d.steps.iter().enumerate().map(|(i, s)| {
            let state = if i < d.at { "done" } else if i == d.at { if running { "now" } else { "failed" } } else { "" };
            format!(r#"<li class="{state}"><span class="tick">{}</span>{}</li>"#, match state { "done" => icon("check"), "failed" => "!".to_string(), _ => String::new() }, esc(s))
        }).collect::<String>();
        let retry = if running { String::new() } else {
            format!(r#"<form method="post" action="/plane/{}" class="deploy-do">{}<button class="button primary">Try again</button><span class="note">Anything already created is reused.</span></form>"#,
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
                let about = if self.show_setup || connected.is_empty() {
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
        if v.status.is_none() && v.online && setup.is_empty() { return (skeleton("overview", "Overview"), true) }
        let jobs_card = if v.status.is_none() && v.online { overview_skeleton_cards() } else { overview_cards(v, &jobs) };
        (format!(r#"<div class="bar"><h1 class="page-title">Overview</h1></div><div class="cards">{setup}{jobs_card}</div>"#), waiting)
    }

    /// Setting up after the control plane: numbered steps; each says what it does, shows its one action, and stays locked
    /// until the step before is done. Gone when everything is ready. Also whether the page should refresh by itself.
    fn setup_card(&self, v: &PlaneView) -> (String, bool) {
        if v.ready() { return (String::new(), false) }
        let (step, locked) = (setup_step, setup_locked);

        // 1. The control plane.
        let plane = step(1, if v.online { "done" } else { "" }, format!("<h3>Control plane {}</h3>", if v.online { pill("good", "running") } else { pill("open", "starting") }),
            format!("<p>{} · <code>{}</code></p>", esc(&v.plane.place()), esc(v.plane.url().trim_start_matches("https://"))));

        // Each step says where it stands in a line and has one button, to the page where it is done: nothing is done
        // here.
        // 2. Where jobs come from: GitHub or GitLab (or both), on Repositories.
        let gh_done = v.github && v.installed;
        let connected: Vec<String> = [v.app_owner().filter(|_| gh_done).map(|o| format!("GitHub · {o}")), v.gitlab_url().map(|u| format!("GitLab · {}", u.trim_start_matches("https://")))].into_iter().flatten().collect();
        let github = if !v.online {
            step(2, "locked", "<h3>Connect GitHub or GitLab</h3>".into(), format!("<p>Where your jobs come from.</p>{}", locked("the control plane")))
        } else if gh_done || v.gitlab {
            step(2, "done", format!("<h3>GitHub or GitLab {}</h3>", pill("good", "connected")), format!("<p>{}</p>", esc(&connected.join(", "))))
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
        } else if v.online {
            step(3, "", format!("<h3>Add runners {logos}</h3>"), r#"<p>Where jobs run: containers in Cloudflare, spot machines in AWS, sandboxes in Modal. Add one or more; the control plane decides per job.</p><div class="do"><a class="button primary" href="/?p=add-runners">Add runners</a></div>"#.to_string())
        } else {
            step(3, "locked", format!("<h3>Add runners {logos}</h3>"), format!("<p>Where jobs run: containers in Cloudflare, spot machines in AWS, sandboxes in Modal.</p>{}", locked("the control plane")))
        };
        // 4. The one line a workflow needs, on Workflows.
        let label = esc(v.plane.label());
        let ready_before = v.online && ((v.github && v.installed) || v.gitlab) && v.runners;
        let workflow = if ready_before {
            step(4, "", "<h3>Use it in a workflow</h3>".into(), format!(r#"<p>Set <code>runs-on: {label}</code> in a workflow; nothing else changes.</p><div class="do"><a class="button primary" href="/?p=workflows">Open Workflows</a></div>"#))
        } else {
            step(4, "locked", "<h3>Use it in a workflow</h3>".into(), format!("<p>Set <code>runs-on: {label}</code> in a workflow; nothing else changes.</p>{}", locked("GitHub or GitLab, and runners")))
        };
        let waiting = !v.online || (v.github && !v.installed);
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
            let signed_in: Vec<&str> = [("aws", self.aws.as_ref().is_some_and(|a| Some(&a.account_id) == account.as_ref())), ("modal", self.modal.is_some()), ("cloudflare", self.cf.is_some())]
                .into_iter().filter(|(_, yes)| *yes).map(|(c, _)| c).collect();
            pools_card(v, &self.quotas, self.cf_month, self.modal_month.as_ref(), &signed_in)
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
        let agent = |cloud: &str| v.agents().into_iter().any(|a| a.cloud == cloud);
        let added = pill("good", "Added");
        let cloudflare = if agent("cloudflare") || (matches!(v.plane, Plane::Cloudflare { .. }) && v.own_containers()) { added.clone() }
            else if self.cf.is_some() { row_form("/runners/cloudflare", "", "Add") } else { connect_action("cloudflare") };
        // AWS: a region picked for you (with why), what the account may run there, and one click; another region if needed.
        let aws = if v.aws { added.clone() }
            // A control plane in AWS whose AWS runners were removed: back in its own account.
            else if matches!(v.plane, Plane::Aws { .. }) { if self.aws.is_some() { row_form("/runners/aws-own", "", "Add") } else { connect_action("aws") } }
            else if self.aws.is_some() {
            format!(r#"<form method="post" action="/aws/connect" class="aws-add"><details class="change"><summary class="button secondary sm">Change region</summary><select name="region" aria-label="Region">{}</select></details><button class="button primary sm">Add</button></form>"#,
                region_select_options("us-east-1", &self.quotas, &closest(Some(v))))
        } else { connect_action("aws") };
        // A Modal control plane starts sandboxes itself; one elsewhere gets a small agent in your Modal workspace.
        let own_modal = matches!(v.plane, Plane::Modal { .. });
        let modal = if agent("modal") || (own_modal && v.own_containers()) { added } else if own_modal { row_form("/runners/modal-own", "", "Add") } else if self.modal.is_some() { row_form("/runners/modal", "", "Add") }
            else if self.modal_pending.is_some() { format!(r#"{}<form method="post" action="/modal/signin"><input type="hidden" name="next"><button class="button secondary sm">Open again</button></form>"#, pill("open", "Waiting for Modal")) }
            else { connect_action("modal") };
        let containers = if matches!(v.plane, Plane::Cloudflare { .. }) { "Containers, one per job, started by your control plane." } else { "Containers, one per job, through a small agent in your Cloudflare." };
        let aws_what = match &self.aws {
            Some(a) if !v.aws => {
                let quota = match self.quotas.get("us-east-1") { Some(Ok(n)) => format!("<br>{}", quota_text("us-east-1", *n)), _ => String::new() };
                format!("Spot machines, one per job, in account {}: in {}, {}, with the most spot capacity; {} and {} when it runs out.{quota}",
                    esc(&a.account_id), region_name("us-east-1"), closest(Some(v)), region_short("us-east-2"), region_short("us-west-2"))
            }
            _ => "Spot machines, one per job, in your AWS account.".to_string(),
        };
        let rows = format!("{}{}{}{}", provider_row("cloudflare", containers, &cloudflare), provider_row_html("aws", &aws_what, &aws),
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
        let github = if !v.github {
            format!("{}{}", row("GitHub", "A private App in your organization: it registers runners and reads job events, never your code", &opens("gh-add", "Connect", true)), github_dialog(false))
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
            format!("{rows}{add}{}", if old { String::new() } else { github_dialog(true) })
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
        format!(r#"{bar}<div class="cards">{sources}</div>"#)
    }

    /// How workflows use your runners, for the code hosts connected (a switch when both are): the label, the steps
    /// for each host; then the machines: the default one, and a picker for the label of another.
    fn workflows_page(&self, v: Option<&PlaneView>) -> String {
        let bar = r#"<div class="bar"><h1 class="page-title">Workflows</h1></div>"#;
        let Some(v) = v else {
            return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&icon("lock"), "Set up a control plane first", "Workflows use your runners through its label.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        };
        let label = esc(v.plane.label());
        let gh_on = v.github;
        let gl_on = v.gitlab_url().is_some();
        let (default_card, picker) = machines_card(v);
        let step = |n: u8, body: String| format!(r#"<li><span class="tick">{n}</span><div>{body}</div></li>"#);
        let sizes = |note: &str| format!(r#"Another machine: add it to the label, like <code>{label}-8cpu</code>, <code>{label}-arm64</code> or <code>{label}-16cpu-64gb</code>.{note}<div class="step-do"><button type="button" class="button secondary sm" onclick="document.getElementById('label-picker').showModal()">Choose a machine…</button></div>"#);
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
        // The public repositories GitHub's App is installed on (none when GitHub is not connected).
        let public = if v.github { cloudflare::plane_get(v.plane.url(), &self.status_key, "/github/public-repos").ok().and_then(|r| r["repos"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())) } else { Some(vec![]) };
        format!(r#"{bar}<div class="cards">{default_card}{usage}{}</div>{picker}{prompt}"#, limits_card(v, public))
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
        let due: Vec<(String, Vec<String>)> = views.iter().filter(|v| aws_account(v).as_deref() == Some(account.as_str()))
            .map(|v| (v.plane.plane_id().to_string(), v.status.as_ref().map(status_regions).unwrap_or_else(|| v.aws_regions()))).filter(|(id, _)| known.get(id).is_none_or(|(_, at)| now > at + 300_000)).collect();
        if due.is_empty() { return }
        let Ok(creds) = self.aws_creds() else { return };
        let store = self.aws_missing.clone();
        // Marked as being read (an empty error), so it is not asked again meanwhile; what was read before stays shown.
        for (id, _) in &due { lock(&store).entry(id.clone()).or_insert((Err(String::new()), now)).1 = now; }
        std::thread::spawn(move || for (id, regions) in due {
            let missing = aws::runner_role_missing(&creds, &account, &id, &regions);
            lock(&store).insert(id, (missing, now_ms()));
        });
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
                let gb = j["machine_type"].as_str().and_then(|t| t.split('-').nth(1)?.strip_suffix("gb")?.parse::<f64>().ok()).unwrap_or(0.0);
                let ran = j["started_ms"].as_u64().map(|s| end.saturating_sub(s) as f64 / 1000.0).unwrap_or(0.0);
                Some((url.clone(), key, id, j["launched_ms"].as_u64().or(j["at_ms"].as_u64()).unwrap_or(end), end, gb * ran * 0.9))
            })
        }).collect();
        let cf = if d.cf_accounts.is_empty() { None } else { d.cf.as_mut().and_then(|s| s.client().ok()) };
        let (accounts, key, shared, modal) = (d.cf_accounts.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(), d.status_key.clone(), shared.clone(), d.modal.clone());
        d.measuring = true;
        std::thread::spawn(move || {
            let mut costs = std::collections::HashMap::new();
            let mut cf_month = None;
            if let Some(cf) = &cf {
                for chunk in todo.chunks(100) {
                    let ids: Vec<String> = chunk.iter().map(|t| t.2.clone()).collect();
                    let (from, to) = (chunk.iter().map(|t| t.3).min().unwrap_or(now) - 60_000, chunk.iter().map(|t| t.4).max().unwrap_or(now) + 600_000);
                    for a in &accounts { if let Ok(c) = cf.container_costs(a, &ids, from, to) { costs.extend(c) } }
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
            let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
            d.measuring = false;
            d.measured_at = now_ms();
            if cf_month.is_some() { d.cf_month = cf_month }
            if modal_month.is_some() { d.modal_month = modal_month }
            if let Some((views, _)) = d.views.as_mut() {
                for v in views.iter_mut() {
                    let Some(measured) = by_plane.get(v.plane.url()) else { continue };
                    for j in v.status.as_mut().and_then(|s| s["jobs"].as_array_mut()).into_iter().flatten() {
                        let key = if j["provider"] == "gitlab" { format!("job:gl{}", j["job_id"]) } else { format!("job:{}", j["job_id"]) };
                        if let Some(usd) = measured.get(&key) { j["cost_usd"] = usd.clone(); j["cost_from"] = "measured".into(); }
                    }
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
                if let Some((good, at)) = self.last_good.get(&id).filter(|(_, at)| now_ms() < at + 60_000) { let _ = at; *v = good.clone(); }
            } else if v.status.is_some() {
                self.last_good.insert(id.clone(), (v.clone(), now_ms()));
            }
            self.first_asked.entry(id.clone()).or_insert_with(now_ms);
            if v.online { self.answered.insert(id); }
            match &v.status { Some(st) => { self.seen.insert(v.plane.plane_id().to_string(), st.clone()); } None if v.online => v.status = self.seen.get(v.plane.plane_id()).cloned(), None => {} }
        }
        views
    }

    /// Deletes a control plane from its cloud, and the runner agents and AWS role made for it in other clouds (those
    /// this session is signed in to). Returns what could not be removed, in words.
    fn delete_plane(&mut self, plane: &Plane, status: &serde_json::Value) -> Result<Vec<String>> {
        let mut left = vec![];
        match plane {
            Plane::Cloudflare { account_id, script, .. } => self.cloudflare()?.delete_plane(account_id, script, plane.plane_id())?,
            Plane::Aws { region, plane_id, .. } => {
                let creds = self.aws_creds()?;
                aws_plane::delete(&creds, region, plane_id)?;
                if let Err(e) = aws::delete_networks(&creds, plane_id, &REGIONS) { left.push(format!("Its machines' network in AWS ({e}).")) }
            }
            Plane::Modal { plane_id, .. } => modal::delete_plane(self.modal.as_ref().ok_or("Sign in with Modal first.")?, plane_id)?,
            Plane::Seen { .. } => return Err("Sign in where this control plane runs to delete it.".into()),
        }
        let agent = |c: &str| status["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c));
        if agent("cloudflare") && !matches!(plane, Plane::Cloudflare { .. }) {
            match self.cloudflare() { Ok(cf) => for (a, _) in self.cf_accounts.clone() { let _ = cf.delete_runners(&a, plane.plane_id()); }, Err(_) => left.push("Its Cloudflare runner agent (sign in with Cloudflare to remove it).".to_string()) }
        }
        if agent("modal") && !matches!(plane, Plane::Modal { .. }) {
            match &self.modal { Some(m) => { let _ = modal::delete_runners(m, plane.plane_id()); } None => left.push("Its Modal runner agent (sign in with Modal to remove it).".into()) }
        }
        if let (Some(account), false) = (status["aws"]["account_id"].as_str(), matches!(plane, Plane::Aws { .. })) {
            match (&mut self.aws, account) {
                (Some(a), acc) if a.account_id == acc => {
                    let creds = a.credentials()?;
                    if let Err(e) = aws::delete_networks(&creds, plane.plane_id(), &REGIONS) { left.push(format!("Its machines' network in AWS account {acc} ({e}).")) }
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
        let agent = |c: &str| st["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c));
        let own = |c: &str| st["containers"] == true && st["own_cloud"] == c;
        let mut plan = vec![];
        if own("cloudflare") || agent("cloudflare") { plan.push(if self.cf.is_some() { Carry::Comes("cloudflare") } else { Carry::NeedsSignIn("cloudflare") }) }
        if own("modal") || agent("modal") { plan.push(if self.modal.is_some() { Carry::Comes("modal") } else { Carry::NeedsSignIn("modal") }) }
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
        let uses = |cloud: &str| v.plane.cloud() == cloud || (st["containers"] == true && st["own_cloud"] == cloud) || st["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == cloud));
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
            return format!(r#"{bar}<div class="cards"><section class="card">{}</section></div>"#, empty_state(&ICON_PLANE.replace("<svg", r#"<svg class="ic""#), "No control plane yet", "Set one up in Cloudflare, AWS or Modal; it takes a minute.", r#"<a class="button primary" href="/?p=plane">Set up a control plane</a>"#))
        }
        let used = view::in_use(views);
        let v = &views[used];
        let signed_in = |v: &PlaneView| match v.plane { Plane::Cloudflare { .. } => self.cf.is_some(), Plane::Aws { .. } => self.aws.is_some(), Plane::Modal { .. } => self.modal.is_some(), Plane::Seen { .. } => false };
        let provider = |v: &PlaneView| v.plane.cloud();
        let updating = lock(&self.updating);
        // (An update under way shows in the row's second line instead.)
        let update = |i: usize, v: &PlaneView| if updating.as_ref().is_some_and(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(_)))) { String::new() }
            // Updated, the old version still answering: said, and looked at again until the new one does.
            else if updating.as_ref().is_some_and(|u| u.restarting(v)) { r#"<span class="note upd-step"><span class="mini-spin"></span>Restarting…</span><div data-refresh="3"></div>"#.to_string() }
            else if !v.outdated() || !v.online { String::new() } else if signed_in(v) {
            format!(r#"<form method="post" action="/plane/update" onsubmit="var b=this.querySelector('button');b.disabled=true;b.textContent='Updating…'"><input type="hidden" name="plane" value="{}"><input type="hidden" name="next"><button class="button {} sm">Update to {DASHBOARD_VERSION}</button></form>"#, esc(v.plane.plane_id()), if i == used { "primary" } else { "secondary" })
        } else { String::new() };
        // Not signed in to a control plane's cloud: one button for that (the rest follows once signed in).
        let sign_in = |cloud: &str| format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="button secondary sm">Sign in with {}</button></form>"#, provider_name(cloud));
        let menu = |items: &[(&str, String)]| if items.is_empty() { String::new() } else {
            format!(r#"<details class="menu"><summary class="icon-btn" aria-label="More">{ICON_MORE}</summary><div class="menu-pop">{}</div></details>"#,
                items.iter().map(|(label, dialog)| format!(r#"<button type="button" class="{}" onclick="this.closest('details').open=false;document.getElementById('{dialog}').showModal()">{label}</button>"#, if label.starts_with("Delete") || label.starts_with("Stop") { "danger-item" } else { "" })).collect::<String>())
        };
        // An update under way: its progress in the row's second line (the address otherwise), a spinner at its end.
        let busy = |v: &PlaneView| updating.as_ref().filter(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(_)))).map(|u| {
            let n = u.steps.len();
            match &u.result {
                None => (format!(r#"Updating to {DASHBOARD_VERSION} · {} <span class="faint">({} of {n})</span><span class="slim"><i style="width:{}%"></i></span>"#, esc(u.steps.get(u.at).copied().unwrap_or("Finishing")), u.at + 1, (u.at * 100 + 50) / n.max(1)), r#"<span class="mini-spin"></span>"#.to_string()),
                Some(Err(e)) => (format!(r#"<span class="bad-text">The update stopped: {}</span>"#, esc(&e.chars().take(140).collect::<String>())),
                    format!(r#"<form method="post" action="/plane/update"><input type="hidden" name="plane" value="{}"><input type="hidden" name="next"><button class="button secondary sm">Try again</button></form>"#, esc(v.plane.plane_id()))),
                Some(Ok(())) => (String::new(), String::new()),
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
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="leave" onclick="if(event.target===this)this.close()"><form method="post" action="/plane/leave" class="dlg-body" onsubmit="var b=this.querySelector('.danger');b.disabled=true;b.textContent='Stopping…'"><div class="dlg-head"><span class="logo warn-mark">{ICON_ALERT}</span><span><strong>Stop using SuperCI?</strong><small>For good, from every cloud</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><ul class="checks stays"><li>–<span>GitHub's App is uninstalled and GitLab's projects stop sending jobs here (their webhooks and SuperCI's runners are removed).</span></li><li>–<span>Every control plane is deleted, with its history, and the runner agents and AWS roles made for them.</span></li><li>–<span>Jobs with <code>runs-on: superci</code> then wait for a runner: change them back first.</span></li></ul><label class="field"><span>Type <code>superci</code> to confirm</span><input type="text" name="confirm" autocomplete="off" required pattern="superci"></label><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button danger sm">Stop using SuperCI</button></div></form></dialog>"#));
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
                else if view::older(o.version.as_deref(), view::MOVE_VERSION) { upd.clone() }
                else if view::older(v.version.as_deref(), view::MOVE_VERSION) { r#"<span class="note">Update the one in use first</span>"#.to_string() }
                else if !signed_in(o) { sign_in(o.plane.cloud()) }
                else if !signed_in(v) { sign_in(v.plane.cloud()) }
                else { format!(r#"<button type="button" class="button secondary sm" onclick="document.getElementById('move-{i}').showModal()">Move here</button>"#) };
            let before = "";
            let items = if signed_in(o) { vec![("Delete…", format!("delete-{i}"))] } else { vec![] };
            let what = match o.plane { Plane::Cloudflare { .. } => "its Worker, its storage and its containers", Plane::Aws { .. } => "its function, table, schedule, parameters and role", _ => "its app and its Dicts" };
            dialogs.push_str(&format!(r#"<dialog class="dlg" id="delete-{i}" onclick="if(event.target===this)this.close()"><form method="post" action="/plane/delete" class="dlg-body" onsubmit="var b=this.querySelector('.danger');b.disabled=true;b.textContent='Deleting…'"><input type="hidden" name="plane" value="{}"><div class="dlg-head">{}<span><strong>Delete {}?</strong><small>{}</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><p class="note">This deletes {what} from {}, and the runner agents made for it. The control plane in use, your jobs and your history are not touched.</p><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button danger sm">Delete</button></div></form></dialog>"#,
                esc(o.plane.plane_id()), logo(provider(o), 36), esc(&o.plane.place()), esc(o.plane.url().trim_start_matches("https://")), provider_name(provider(o))));
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
        for (cloud, what, action) in self.setup_actions(&listed) {
            // Not signed in, and a control plane there is listed already (with its own sign-in): no second row.
            let signed = match cloud { "cloudflare" => self.cf.is_some(), "aws" => self.aws.is_some(), _ => self.modal.is_some() };
            if !signed && listed.iter().any(|p| p.cloud() == cloud) { continue }
            match (&deploying, action) {
                (Some((c, html)), _) if *c == cloud => rows.push_str(html),
                (_, Some(action)) => rows.push_str(&format!(r#"<div class="pick act idle">{}<span><strong>{}</strong><small>{}</small></span><span class="pick-end">{action}</span></div>"#, logo(cloud, 36), provider_name(cloud), esc(&what))),
                _ => {}
            }
        }
        rows.push_str(&more_clouds(&PLANE_SOON));

        // How the last move ended, when something stayed behind; what the last delete did.
        let moving = lock(&self.moving).as_ref().is_some_and(|m| m.result.is_none());
        let ended = match lock(&self.moving).as_ref().and_then(|m| m.result.as_ref()) {
            Some(Ok(left)) => {
                let to = lock(&self.moving).as_ref().and_then(|m| views.iter().find(|v| v.plane.plane_id() == m.to)).map(|v| provider_name(v.plane.cloud())).unwrap_or("the new control plane");
                format!(r#"<div class="notice">{}<span>Moved to {to}: GitHub and GitLab send their jobs there now.{}</span></div>"#, icon("check"), esc(&left.iter().map(|l| format!(" {l}")).collect::<String>()))
            }
            _ => String::new(),
        };
        let ended = match &self.flash { Some(f) => format!(r#"{ended}<div class="notice">{}<span>{}</span></div>"#, icon("check"), esc(f)), None => ended };
        let (perms, perm_dialogs) = if v.online { self.permissions_card(v) } else { (String::new(), String::new()) };
        let dialogs = format!("{dialogs}{perm_dialogs}");
        format!(r#"{}{bar}{ended}<p class="note" style="margin:-4px 0 18px">GitHub and GitLab send their jobs to the control plane in use. To move it, set one up in another cloud and Move here: everything comes along.</p><div class="picks page">{rows}</div>{perms}{dialogs}"#,
            if moving || self.deploy_running() { r#"<div data-refresh="2"></div>"# } else { "" })
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
    let l = v.plane.label();
    let gh = |task: &str| format!("Move this repository's GitHub Actions jobs to SuperCI runners (`runs-on: {l}`, a fresh self-hosted machine per job). {task}

- Linux jobs: `{l}`, keeping their size (`{l}-8cpu`, `{l}-16cpu-64gb`, `{l}-arm64`). Windows: `{l}-windows`. GPU: `{l}-gpu`.
- Keep on GitHub, with a `# stays on GitHub: <why>` comment: macOS, npm publishing with provenance, jobs needing KVM, and fork pull requests in public repositories (`${{{{ github.event.pull_request.head.repo.fork && 'ubuntu-latest' || '{l}' }}}}`).
- Change matrix values rather than `runs-on: ${{{{ matrix.os }}}}`, and reusable workflows where they are defined. One label per job, not a list.
- Open one pull request with a before → after table. If a job fails only on the new runner, move it back and say why.");
    let gl = |task: &str| format!("Move this project's GitLab CI jobs to SuperCI runners (tag `{l}`, a fresh machine per job, in Docker). {task}

- Tag jobs on GitLab's hosted Linux runners `{l}` (replacing `saas-linux-*`, or once in `default: tags`), keeping their size: `{l}-4cpu` for medium, `{l}-8cpu` for large, `{l}-16cpu` for xlarge, `{l}-arm64` for arm64. Keep `image:` and `services:`.
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

/// Connecting GitHub (the first organization, or another): where it is, whose jobs, then GitHub makes the App.
fn github_dialog(another: bool) -> String {
    let wher = where_choices("gh-where", ("GitHub.com", "github.com"), ("GitHub Enterprise", "Your own server, or yours.ghe.com"),
        r#"<input type="text" name="host" placeholder="github.example.com" autocomplete="off" spellcheck="false" aria-label="Your GitHub's address"><small>Its address, without https://. It has to be reachable from your control plane.</small>"#);
    let body = format!(r#"<form method="post" action="/github/start">
<section class="prompt-modal-step prompt-modal-step-first"><div class="prompt-modal-step-head"><span>1</span><h2>Where is your GitHub?</h2></div>{wher}</section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>2</span><h2>Whose jobs?</h2></div><div class="perm-field"><input type="text" name="login" placeholder="your-organization" required pattern="[A-Za-z0-9][A-Za-z0-9-]{{0,38}}" autocomplete="off" spellcheck="false" aria-label="GitHub organization or user"><small>An organization you own, or your own username.</small></div></section>
<section class="prompt-modal-step"><div class="prompt-modal-step-head"><span>3</span><h2>Create its App</h2></div><p class="prompt-modal-instruction">GitHub makes a private App there, then asks where to install it. It registers runners and reads job events, never your code.</p><div class="perm-action"><button class="prompt-modal-copy">Continue on GitHub</button></div></section>
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
        c => (status["containers"] == true && status["own_cloud"].as_str().unwrap_or("cloudflare") == c) || status["agents"].as_array().is_some_and(|a| a.iter().any(|a| a["cloud"] == c)),
    }
}

/// Removing a runner provider, confirmed: what is deleted where (the control plane and sign-ins stay), its running jobs,
/// and the sign-in that needs; or removing it from SuperCI only.
fn remove_dialog(v: &PlaneView, cloud: &str, i: usize, signed_in: &[&str]) -> String {
    let (logo_key, name, _) = pool_text(v, cloud);
    let id = v.plane.plane_id();
    let st = v.status.clone().unwrap_or_default();
    let own = st["containers"] == true && st["own_cloud"].as_str().unwrap_or("cloudflare") == cloud;
    let account = aws_account(v).unwrap_or_default();
    let deleted: Vec<String> = match cloud {
        "aws" => vec!["Its machines, and their network in each region".into(), if matches!(v.plane, Plane::Aws { .. }) { format!("The machines' policy on the role superci-plane-{id} (the control plane's own role stays)") } else { format!("The role superci-plane-{id} and its identity provider, in account {account}") }],
        _ if own => vec![],
        "modal" => vec![format!("The app superci-runners-{id} in your Modal workspace, and its sandboxes")],
        _ => vec![format!("The runner agent superci-runners-{id} in your Cloudflare account, and its containers")],
    };
    let what = if deleted.is_empty() { format!(r#"<p class="note">Nothing is deleted: your control plane stops starting {}.</p>"#, if cloud == "modal" { "sandboxes" } else { "containers" }) }
        else { format!(r#"<p class="note">Deleted in {name}:</p><ul class="needs">{}</ul>"#, deleted.iter().map(|d| format!("<li><span>{}</span></li>", esc(d))).collect::<String>()) };
    let running = running_on(&st, cloud);
    let (stop, warn) = if running == 0 { (String::new(), String::new()) } else {
        (r#"<input type="hidden" name="stop" value="on">"#.to_string(), format!(r#"<p class="note bad-text">{running} job{} running there {}.</p>"#, if running == 1 { " is" } else { "s are" }, if own { "finish on their own" } else { "stop, and fail on GitHub" }))
    };
    let can = own || signed_in.contains(&cloud);
    let main = if can { format!(r#"<button class="button danger sm">{}</button>"#, if running > 0 && !own { format!("Stop {running} and remove") } else { "Remove".into() }) } else { String::new() };
    let signin = if can { String::new() } else { format!(r#"<div class="dlg-signin"><span class="note">Sign in with {name} to delete these.</span>{}</div>"#, signin_button(cloud, &format!("Sign in with {name}"), true)) };
    let only = if own { String::new() } else { r#"<button class="button secondary sm" name="only" value="on">Remove from SuperCI only</button>"#.to_string() };
    format!(r#"<dialog class="dlg" id="pool-remove-{i}" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head">{}<span><strong>Remove {}?</strong><small>Jobs stop going to {}. Your control plane and your sign-ins stay; adding it back is a click.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div>{what}{warn}{signin}<form method="post" action="/runners/remove" class="dlg-foot" onsubmit="this.querySelectorAll('button').forEach(function(b){{b.disabled=true}})"><input type="hidden" name="plane" value="{}"><input type="hidden" name="cloud" value="{}">{stop}<button type="button" class="button secondary sm" style="margin-right:auto" onclick="this.closest('dialog').close()">Cancel</button>{only}{main}</form></div></dialog>"#,
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
    for (cloud, n) in needs { match by.iter_mut().find(|b| b.0 == *cloud) { Some(b) => { b.1 += 1; b.2 |= n.writes } None => by.push((cloud, 1, n.writes)) } }
    by.iter().map(|(cloud, k, writes)| format!("{k} new {}permission{} in {}", if *writes { "" } else { "read " }, if *k == 1 { "" } else { "s" }, provider_name(cloud))).collect::<Vec<_>>().join(", ")
}

fn provider_name(provider: &str) -> &str {
    match provider {
        "aws" => "AWS", "cloudflare" => "Cloudflare", "modal" => "Modal", "googlecloud" => "Google Cloud", "azure" => "Azure", "hetzner" => "Hetzner",
        "digitalocean" => "DigitalOcean", "flydotio" => "Fly.io", "kubernetes" => "Kubernetes", "vercel" => "Vercel", "netlify" => "Netlify", "deno" => "Deno Deploy",
        "github" => "GitHub", "gitlab" => "GitLab", "bitbucket" => "Bitbucket", "forgejo" => "Forgejo", "gitea" => "Gitea", "azuredevops" => "Azure DevOps",
        "machine" => "Your own computer", "render" => "Render", "railway" => "Railway", "scaleway" => "Scaleway", "vultr" => "Vultr", "akamai" => "Akamai (Linode)", other => other,
    }
}


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
        "jobs" => format!(r#"<div class="cards">{}</div>"#, card("Recent jobs", true, format!(r#"<div class="rows">{}</div>"#, SK_ROWS.repeat(6)))),
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
        // Settings from before: its control planes are their own page now (and Changelog another).
        "settings" => "planes",
        s if ["plane", "planes", "changes", "runners", "add-runners", "workflows", "jobs", "repos", "gitlab"].contains(&s) => s,
        _ => "overview",
    }
}

fn section_title(section: &str) -> &'static str {
    match section {
        "plane" => "Set up a control plane", "planes" => "Control plane", "changes" => "Changelog", "add-runners" => "Add a provider", "runners" => "Runners",
        "workflows" => "Workflows", "jobs" => "Jobs", "repos" => "Repositories", "gitlab" => "GitLab", _ => "Overview",
    }
}


fn cloud_name(cloud: &str) -> &str {
    match cloud { "aws" => "AWS", AWS_ON_DEMAND => "AWS on-demand", "cloudflare" => "Cloudflare", "modal" => "Modal", "machine" => "Your computer", other => other }
}

/// A control plane older than this dashboard, for the sidebar on every page: the versions, a link to what is new, and
/// its Update button.
fn side_update(v: &PlaneView, updating: Option<&Update>, aws_lacks: &[&'static superci_core::permissions::Need]) -> String {
    let action = if let Some(u) = updating.filter(|u| u.plane == v.plane.plane_id() && !matches!(u.result, Some(Ok(())))) { update_progress(u) }
        else if matches!(v.plane, Plane::Seen { .. }) { r#"<small>Sign in where it runs to update it.</small>"#.to_string() }
        else { format!(r#"<form method="post" action="/plane/update" onsubmit="var b=this.querySelector('button');b.disabled=true;b.textContent='Updating…'"><input type="hidden" name="next"><button class="button primary sm">Update to {DASHBOARD_VERSION}</button></form>"#) };
    // While it updates, and once it has: said so, on every page.
    match updating.filter(|u| u.plane == v.plane.plane_id()).map(|u| &u.result) {
        Some(None) => return format!(r#"<div class="side-update"><strong><span class="dot accent"></span>Updating to {DASHBOARD_VERSION}</strong><small>It keeps going while you look around. <a href="/?p=changes">Changelog</a></small>{action}</div>"#),
        Some(Some(Ok(()))) if updating.is_some_and(|u| u.restarting(v)) => return format!(r#"<div class="side-update"><strong><span class="dot accent"></span>Restarting with {DASHBOARD_VERSION}</strong><small>Uploaded; its cloud is switching it over, which can take a few minutes. <a href="/?p=changes">What changed</a></small><div data-refresh="3"></div></div>"#),
        Some(Some(Ok(()))) if v.outdated() => {}
        Some(Some(Ok(()))) => return format!(r#"<div class="side-update"><strong><span class="dot good"></span>Updated to {DASHBOARD_VERSION}</strong><small><a href="/?p=changes">What changed</a></small></div>"#),
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
        None => format!(r#"<div class="upd"><span class="upd-step"><span class="mini-spin"></span>{}</span><span class="slim"><i style="width:{}%"></i></span></div>"#, esc(u.steps.get(u.at).copied().unwrap_or("Finishing")), (u.at * 100 + 50) / n.max(1)),
        Some(Err(e)) => format!(r#"<div class="upd"><span class="bad-text">The update stopped: {}</span><form method="post" action="/plane/update"><input type="hidden" name="next"><button class="button secondary sm">Try again</button></form></div>"#, esc(&e.chars().take(160).collect::<String>())),
        Some(Ok(())) => String::new(),
    }
}

/// The changelog (CHANGELOG.md, in this program): each version with its date and changes.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

fn changelog() -> Vec<(String, String, Vec<String>)> {
    let mut out: Vec<(String, String, Vec<String>)> = vec![];
    for line in CHANGELOG.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            let (v, date) = h.split_once(" — ").unwrap_or((h, ""));
            out.push((v.trim().to_string(), date.trim().to_string(), vec![]));
        } else if let (Some(item), Some(last)) = (line.strip_prefix("- "), out.last_mut()) { last.2.push(item.to_string()) }
    }
    out
}

/// Changelog text as HTML: escaped, with `code` kept.
fn change_html(text: &str) -> String {
    esc(text).split('`').enumerate().map(|(i, part)| if i % 2 == 1 { format!("<code>{part}</code>") } else { part.to_string() }).collect()
}

/// The page's own behaviour, for every fragment it shows: the machine picker (its label and which pools can run it) and
/// dragging runner pools into a new order (saved when dropped).
const PAGE_JS: &str = r#"<script>document.addEventListener('click',function(e){document.querySelectorAll('details.menu[open]').forEach(function(d){if(!d.contains(e.target))d.open=false})});

function superciFit(k,c,r,d,arch,os,gpu){var cpu=c||k.std_cpu,ram=r||(c?Math.min(c*k.ram_per_cpu,k.max_ram_gb):k.std_ram_gb);ram=Math.max(ram,cpu*k.min_ram_per_cpu);var disk=d||k.std_disk_gb,gs=k.gpus||[];
var ok=cpu<=k.max_cpu&&ram<=k.max_ram_gb&&disk<=k.max_disk_gb&&k.arch.indexOf(arch)>=0&&k.os.indexOf(os)>=0&&!(os==='windows'&&arch!=='x64');
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
var cpu=+g('cpu')||0,ram=+g('ram')||0,disk=+g('disk')||0,arch=g('arch'),os=g('os'),gpu=g('gpu'),od=g('ondemand');
if(label){var parts=[];if(cpu)parts.push(cpu+'cpu');if(ram)parts.push(ram+'gb');if(disk)parts.push(disk+'disk');if(arch)parts.push(arch);if(os)parts.push(os);if(gpu)parts.push(gpu);if(od)parts.push('ondemand');f.querySelector('[data-label]').textContent=f.dataset.base+(parts.length?'-'+parts.join('-'):'');
ram=ram||(cpu?0:d.ram);cpu=cpu||d.cpu;disk=disk||d.disk;arch=arch||d.arch;os=os||d.os}
f.querySelectorAll('.fit').forEach(function(x){var got=null;JSON.parse(x.dataset.caps).some(function(k){got=superciFit(k,cpu,ram,disk,arch,os,gpu);return got});x.classList.toggle('no',!got);var s=x.querySelector('[data-size]');if(s)s.textContent=got?got.cpu+' CPU · '+got.ram+' GB'+(got.gpu?' · '+got.gpu.toUpperCase():''):''})}
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
fetch('/routing',{method:'POST',body:body,credentials:'same-origin'}).then(function(r){if(!r.ok)throw 0;li.classList.remove('moved');say('saved','<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>Order saved');setTimeout(function(){if(note&&note.classList.contains('saved'))say('','')},2500)})
.catch(function(){before.forEach(function(c){var x=list.querySelector('.pool[data-cloud="'+c+'"]');if(x)list.appendChild(x)});keep(before.join());li.classList.remove('moved');say('bad','The order was not saved; it is back as it was.')})});})();
</script>"#;

/// A pool's name in the order: a cloud.
fn pool_name_ok(name: &str) -> bool {
    ["aws", AWS_ON_DEMAND, "cloudflare", "modal"].contains(&name)
}

/// One numbered step of Set up: its number (a check once done), its title and what it says or offers.
fn setup_step(n: u8, state: &str, title: String, body: String) -> String {
    format!(r#"<div class="step {state}"><div class="num">{}</div><div>{title}{body}</div></div>"#, if state == "done" { icon("check") } else { n.to_string() })
}

/// What a step of Set up waits for.
fn setup_locked(what: &str) -> String { format!(r#"<div class="locked-note">{}Needs {what} first</div>"#, icon("lock")) }

/// A pool as shown: its logo key, its name, and what it starts.
fn pool_text(v: &PlaneView, pool: &str) -> (String, String, String) {
    let what = match pool {
        "aws" => {
            let regions = v.aws_regions();
            let at = match regions.split_first() {
                Some((first, [])) => format!(" · {}", region_short(first)),
                Some((first, rest)) => format!(" · {}, then {}", region_short(first), rest.iter().map(|r| region_short(r)).collect::<Vec<_>>().join(", ")),
                None => String::new(),
            };
            format!("Spot machines of any size · x64, arm64{at}")
        }
        AWS_ON_DEMAND => "On-demand machines · one per job, never interrupted".into(),
        "cloudflare" => "Containers sized per job · up to 4 CPU, 12 GB, x64".into(),
        "modal" => "Sandboxes · up to 64 CPU, x64".into(),
        _ => String::new(),
    };
    // AWS's on-demand machines carry AWS's logo.
    (if pool == AWS_ON_DEMAND { "aws" } else { pool }.into(), cloud_name(pool).to_string(), what)
}

/// What each pool can run, for the machine picker: its capacity (the largest machine, systems, standard machine and
/// memory rule).
fn pool_caps(pool: &str) -> serde_json::Value {
    match pool {
        "aws" => serde_json::json!([Capacity::aws()]),
        "cloudflare" => serde_json::json!([Capacity::cloudflare()]),
        "modal" => serde_json::json!([Capacity::modal()]),
        _ => serde_json::json!([]),
    }
}

const ICON_GEAR: &str = r#"<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z"/></svg>"#;
const ICON_GRIP: &str = r#"<svg viewBox="0 0 24 24"><circle cx="9" cy="6" r="1.4"/><circle cx="15" cy="6" r="1.4"/><circle cx="9" cy="12" r="1.4"/><circle cx="15" cy="12" r="1.4"/><circle cx="9" cy="18" r="1.4"/><circle cx="15" cy="18" r="1.4"/></svg>"#;

/// The runner pools, in order: each starts a runner per job; a job goes to the first that can run its machine and has
/// room. Rows are dragged to reorder; each has its settings (limits) in a dialog.
fn pools_card(v: &PlaneView, quotas: &HashMap<String, Result<u32>>, cf_month: Option<(f64, f64)>, modal_month: Option<&modal::Month>, signed_in: &[&str]) -> String {
    let order = v.order();
    let jobs = v.jobs();
    let current = order.iter().map(|p| p.cloud.as_str()).collect::<Vec<_>>().join(",");
    let mut rows = String::new();
    let mut dialogs = String::new();
    for (i, p) in order.iter().enumerate() {
        let (logo_key, name, what) = pool_text(v, &p.cloud);
        let on_demand = p.cloud == AWS_ON_DEMAND;
        let state = if p.off { pill("open", "off") } else { pill("good", "ready") };
        let spent = v.spend(&p.cloud);
        let mut limits = vec![];
        if let Some(n) = p.max_jobs { limits.push(format!("{n} at once")) }
        if let Some(cap) = p.monthly_usd { limits.push(format!("{} of {}", usd(spent), usd(cap))) }
        let mut limits = limits.iter().map(|l| format!(r#"<span class="lim">{}</span>"#, esc(l))).collect::<String>();
        limits.push_str(&month_text(&p.cloud, spent, p.monthly_usd.is_none(), cf_month, modal_month));
        let tag = "";
        rows.push_str(&format!(r#"<li class="pool" draggable="true" data-cloud="{}"><span class="grip" title="Drag to reorder">{ICON_GRIP}</span>{}<span class="pool-main"><strong>{}{tag}</strong><small>{}</small></span><span class="pool-now">{limits}{state}</span><button type="button" class="icon-btn" aria-label="{} settings" onclick="document.getElementById('pool-{i}').showModal()">{ICON_GEAR}</button></li>"#,
            esc(&p.cloud), logo(&logo_key, 32), esc(&name), esc(&what), esc(&name)));
        // Its settings.
        let value = |n: Option<String>| n.map(|n| format!(r#" value="{n}""#)).unwrap_or_default();
        let budget = {
            format!(r#"<label class="field"><span>Monthly budget</span><span class="money">$<input type="number" name="usd" min="1" step="1" placeholder="No limit"{}></span><small>{} this month. At the budget, jobs go to the next provider.</small></label>"#,
                value(p.monthly_usd.map(|n| format!("{n:.0}"))), usd(spent))
        };
        // AWS: its regions, in order.
        let regions = if p.cloud == "aws" && v.status.as_ref().is_some_and(|s| s.get("aws_regions").is_some()) {
            let list = v.aws_regions();
            let best = closest(Some(v));
            let row = |r: &str| {
                let quota = match quotas.get(r) { Some(Ok(n)) => format!(" · up to {n} spot CPUs"), _ => String::new() };
                let quota = if r == "us-east-1" { format!("{quota} · {best}") } else { quota };
                format!(r#"<li class="rg" draggable="true" data-region="{r}" data-name="{}"><span class="grip" title="Drag to reorder">{ICON_GRIP}</span><span class="rg-main"><strong>{}</strong><small>{r}{quota}</small></span><button type="button" class="icon-btn" aria-label="Remove {}" onclick="superciRegionRemove(this)"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></li>"#,
                    esc(region_name(r)), esc(region_name(r)), esc(region_name(r)))
            };
            let rows = list.iter().map(|r| row(r)).collect::<String>();
            let more = REGIONS.iter().filter(|r| !list.iter().any(|l| l == *r)).map(|r| format!(r#"<option value="{r}">{} · {r}{}</option>"#, region_name(r), if *r == "us-east-1" { format!(" · {best}") } else { String::new() })).collect::<String>();
            let total: u32 = list.iter().filter_map(|r| quotas.get(r).and_then(|q| q.as_ref().ok())).sum();
            let low = list.iter().find(|r| matches!(quotas.get(*r), Some(Ok(n)) if *n < 16));
            let quota = match (total, low) {
                (0, _) => String::new(),
                (n, Some(r)) => format!(r#" Your account may run {n} spot CPUs at once in these. <a href="{}" target="_blank" rel="noopener">Request more in {} ↗</a>"#, esc(&superci_core::aws::spot_quota_link(r)), region_short(r)),
                (n, None) => format!(" Your account may run up to {n} spot CPUs at once in these ({} of 4 CPU).", jobs_word(n / 4)),
            };
            // A network of the account's own, by region (a control plane that knows of them).
            let networks = match v.status.as_ref().filter(|s| s.get("aws_networks").is_some()) {
                Some(st) => { let text = given_networks_text(st); format!(r#"<details class="field-more"{}><summary>Your own network</summary><label class="field"><textarea name="networks" rows="3" spellcheck="false" placeholder="us-east-1 subnet-0abc… subnet-0def… sg-0123… private">{}</textarea><small>One line a region: its subnets and security groups. Machines there start in your network, so jobs reach what it reaches. Add <code>private</code> for subnets with no public addresses (they need a NAT to reach GitHub). Regions not listed use the network SuperCI made.</small></label></details>"#, if text.is_empty() { "" } else { " open" }, esc(&text)) }
                None => String::new(),
            };
            format!(r#"<div class="field"><span>Regions</span><ol class="rg-list" data-regions>{rows}</ol><select class="rg-add" aria-label="Add a region" onchange="superciRegionAdd(this)"><option value="">+ Add a region…</option>{more}</select><small>Machines start in the first; when it has no spot capacity left, in the next. Drag to reorder.{quota}</small></div>{networks}"#)
        } else if p.cloud == "cloudflare" && v.status.as_ref().is_some_and(|s| s.get("cloudflare_location").is_some()) {
            // Cloudflare: where its containers start.
            let current = v.status.as_ref().and_then(|s| s["cloudflare_location"].as_str().map(str::to_string)).unwrap_or_else(|| "enam".into());
            let best = closest(Some(v));
            let opts = CF_LOCATIONS.iter().map(|(k, name)| format!(r#"<option value="{k}"{}>{name}{}</option>"#, if *k == current { " selected" } else { "" }, if *k == "enam" { format!(" · {best}") } else { String::new() })).collect::<String>();
            let image = v.status.as_ref().and_then(|s| s["cloudflare_image"].as_str().map(str::to_string)).unwrap_or_default();
            format!(r#"<label class="field"><span>Location</span><select name="location">{opts}</select><small>Where its containers start. Cloudflare places each one; this is the area it starts in.</small></label><label class="field"><span>GitHub's full image (experimental)</span><input type="url" name="image" value="{}" placeholder="https://… (empty: the small image)"><small>Jobs then run inside GitHub's own runner image, with everything it has, loaded from this address as it is read and checked piece by piece (an address as <code>image-reader pack</code> names it). Empty: GitHub's small runner image, with few tools.</small></label>"#, esc(&image))
        } else { String::new() };
        let at_once_note = "When this many run, the next job goes to the next provider (or waits).";
        // AWS on-demand is part of AWS: turned off here, removed with AWS.
        let (switch, remove_button, remove) = if on_demand {
            (format!(r#"<label class="check"><input type="checkbox" name="on"{}> Use on-demand machines</label>"#, if p.off { "" } else { " checked" }), String::new(), String::new())
        } else {
            (String::new(), format!(r#"<button type="button" class="button danger sm" style="margin-right:auto" onclick="this.closest('dialog').close();document.getElementById('pool-remove-{i}').showModal()">Remove…</button>"#), remove_dialog(v, &p.cloud, i, signed_in))
        };
        dialogs.push_str(&format!(r#"<dialog class="dlg" id="pool-{i}" onclick="if(event.target===this)this.close()"><form method="post" action="/routing" class="dlg-body" onsubmit="superciRegions(this)"><input type="hidden" name="action" value="pool"><input type="hidden" name="cloud" value="{}"><input type="hidden" name="current" value="{}"><div class="dlg-head">{}<span><strong>{}</strong><small>{}</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><label class="field"><span>Jobs at once</span><input type="number" name="max" min="1" max="999" placeholder="{}"{}><small>{at_once_note}</small></label>{budget}{regions}{switch}<div class="dlg-foot">{remove_button}<button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>{remove}"#,
            esc(&p.cloud), esc(&current), logo(&logo_key, 36), esc(&name), esc(&what), "No limit", value(p.max_jobs.map(|n| n.to_string()))));
    }
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
    let rules = v.routing().rules.iter().map(|r| format!(r#"<form method="post" action="/routing" class="rule"><input type="hidden" name="action" value="remove"><input type="hidden" name="repo" value="{}"><span class="note"><code>{}</code> only uses {} (a rule from before; a workflow can say <code>runs-on: {}-{}</code> instead)</span><button class="chip">Remove</button></form>"#,
        esc(&r.repo), esc(&r.repo), esc(cloud_name(&r.cloud)), esc(v.plane.label()), esc(&r.cloud))).collect::<String>();
    format!(r#"<section class="card"><div class="card-head"><h2>Runner providers</h2><span class="order-note" id="order-note" aria-live="polite"></span></div><p class="note">Each provider starts a fresh runner for every job. A job goes to the first that can run its machine and has room; when that one cannot start it (no spot machine, a failure), to the next. When all are full, it waits. Drag to reorder.</p>{waiting}<ol class="pools" data-order="{}">{rows}</ol>{rules}{dialogs}</section>"#, esc(&current))
}

/// Machines: the default machine (what plain `runs-on: superci` gets, and any part a label leaves out), changed in
/// its own dialog; and a picker that gives the label for another machine, naming only what is picked.
/// The default machine (what the plain label gets, and any part a label leaves out), changed in its own dialog; and
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

/// the label picker, a dialog of its own: it names only what differs from the default.
fn machines_card(v: &PlaneView) -> (String, String) {
    let m = v.machine();
    let base = esc(v.plane.label());
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
    let pools = || v.order().iter().filter(|p| p.cloud != AWS_ON_DEMAND).map(|p| { let (key, name, _) = pool_text(v, &p.cloud); format!(r#"<span class="fit" data-caps="{}">{}{}<small data-size></small></span>"#, esc(&pool_caps(&p.cloud).to_string()), logo(&key, 18), esc(&name)) }).collect::<String>();
    let default = serde_json::json!({ "cpu": m.cpu.unwrap_or(0), "ram": m.ram_gb.unwrap_or(0), "disk": m.disk_gb.unwrap_or(0), "arch": m.arch(), "os": m.os(), "ondemand": m.on_demand });
    // The default: what it is, in words, and its dialog.
    let mut parts = vec![m.cpu.map(|c| format!("{c} CPU")).unwrap_or_else(|| "Each provider's standard size".into())];
    if let Some(r) = m.ram_gb { parts.push(format!("{r} GB")) }
    if let Some(d) = m.disk_gb { parts.push(format!("{d} GB disk")) }
    parts.push(m.arch().into());
    parts.push(match m.os() { "windows" => "Windows".into(), "macos" => "macOS".into(), _ => "Linux".into() });
    parts.push(if m.on_demand { "on-demand".into() } else { "spot".into() });
    let dialog = format!(r#"<dialog class="dlg" id="default-machine" onclick="if(event.target===this)this.close()"><form method="post" action="/machine" class="dlg-body picker" data-mode="default" data-default="{}" oninput="superciPick(this)" onchange="superciPick(this)"><div class="dlg-head"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span><strong>Default machine</strong><small>For <code>runs-on: {base}</code>, and any part a label leaves out.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><div class="picker-fields">{}{}{}{}{}</div><label class="check"><input type="checkbox" name="ondemand"{}> On-demand only (no spot)</label><div class="fits"><span class="note">Runs on</span>{}</div><div class="dlg-foot"><button type="button" class="button secondary sm" onclick="this.closest('dialog').close()">Cancel</button><button class="button primary sm">Save</button></div></form></dialog>"#,
        esc(&default.to_string()),
        select("cpu", "CPU", "Provider's standard", &CPUS, m.cpu), select("ram", "Memory", "4 GB per CPU", &RAMS, m.ram_gb), select("disk", "Disk", "Provider's standard", &DISKS, m.disk_gb),
        choice("arch", "Architecture", "", &[("x64", "x64"), ("arm64", "arm64")], Some(m.arch())), choice("os", "System", "", &[("linux", "Linux"), ("windows", "Windows")], Some(m.os())),
        if m.on_demand { " checked" } else { "" }, pools());
    let default_row = format!(r#"<div class="default-machine"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span class="dm-main"><strong>Default machine</strong><small>What <code>{base}</code> runs on, and any part a label leaves out.</small></span><span class="dm-spec">{}</span><button type="button" class="button secondary sm" onclick="document.getElementById('default-machine').showModal()">Change</button></div>"#,
        parts.iter().map(|p| format!(r#"<span class="lim">{}</span>"#, esc(p))).collect::<String>());
    // The picker: every field starts at the default; the label names only what is picked.
    let od_default = if m.on_demand { r#" checked disabled title="The default machine is on-demand already""# } else { "" };
    let picker = format!(r#"<dialog class="dlg wide" id="label-picker" onclick="if(event.target===this)this.close()"><div class="dlg-body"><div class="dlg-head"><span class="logo" style="width:36px;height:36px;background:var(--accent-soft);color:var(--accent)">{ICON_CHIP}</span><span><strong>Choose a machine</strong><small>Pick what differs from the default; its label names only that.</small></span><button type="button" class="icon-btn" aria-label="Close" onclick="this.closest('dialog').close()"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div><form class="picker" data-mode="label" data-base="{base}" data-default="{}" oninput="superciPick(this)" onchange="superciPick(this)" onsubmit="return false"><div class="picker-fields">{}{}{}{}{}{}<label class="check"><input type="checkbox" name="ondemand"{od_default}> On-demand only</label></div><div class="label-out"><code><span class="k">runs-on:</span> <span class="hl" data-label>{base}</span></code><button type="button" class="chip" onclick="var b=this;navigator.clipboard.writeText('runs-on: '+b.closest('form').querySelector('[data-label]').textContent).then(function(){{b.textContent='Copied'}},function(){{}})">Copy</button></div><div class="fits"><span class="note">Runs on</span>{}</div></form></div></dialog>"#,
        esc(&default.to_string()),
        select("cpu", "CPU", "Default", &CPUS, None), select("ram", "Memory", "Default", &RAMS, None), select("disk", "Disk", "Default", &DISKS, None),
        choice("arch", "Architecture", "Default", &[("x64", "x64"), ("arm64", "arm64")], None), choice("os", "System", "Default", &[("linux", "Linux"), ("windows", "Windows")], None),
        choice("gpu", "GPU", "None", &[("gpu", "Any (least costly)"), ("t4", "T4"), ("l4", "L4"), ("a10g", "A10G"), ("l40s", "L40S"), ("a100", "A100"), ("h100", "H100"), ("h200", "H200"), ("b200", "B200")], None), pools());
    (format!(r#"<section class="card dm-card">{default_row}{dialog}</section>"#), picker)
}

const ICON_CHIP: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/></svg>"#;

fn signin_button(cloud: &str, label: &str, primary: bool) -> String {
    format!(r#"<form method="post" action="/{cloud}/signin"><input type="hidden" name="next"><button class="{}">{}</button></form>"#, if primary { "button primary" } else { "button secondary" }, esc(label))
}

/// A job's page on its code host.
fn job_link(j: &serde_json::Value) -> String {
    let (repo, job, run) = (j["repo"].as_str().unwrap_or_default(), j["job_id"].as_u64().unwrap_or_default(), j["run_id"].as_u64().unwrap_or_default());
    if j["provider"] == "gitlab" { format!("{}/{repo}/-/jobs/{job}", j["gitlab_url"].as_str().unwrap_or("https://gitlab.com")) } else { format!("{}/{repo}/actions/runs/{run}/job/{job}", j["github_web"].as_str().unwrap_or("https://github.com")) }
}

/// Overview's cards: the last day's jobs (did they get a machine, how fast, at what cost), the ones that need a look,
/// the latest; then where they ran.
fn overview_cards(v: &PlaneView, jobs: &[serde_json::Value]) -> String {
    let jobs: Vec<serde_json::Value> = jobs.iter().map(|j| with_hosts(v, j)).collect();
    let now = now_ms();
    let day: Vec<&serde_json::Value> = jobs.iter().filter(|j| now.saturating_sub(j["at_ms"].as_u64().unwrap_or_default()) < 86_400_000).collect();
    let state = |j: &serde_json::Value| j["state"].as_str().unwrap_or_default().to_string();
    let failed: Vec<&&serde_json::Value> = day.iter().filter(|j| ["failed", "orphan", "swept"].contains(&state(j).as_str())).collect();
    let waiting: Vec<&&serde_json::Value> = day.iter().filter(|j| state(j) == "waiting").collect();
    let running = day.iter().filter(|j| ["launching", "launched", "running"].contains(&state(j).as_str())).count();
    let ran = day.iter().filter(|j| j["started_ms"].as_u64().is_some()).count();
    let settled = day.iter().filter(|j| !["waiting", "launching", "launched"].contains(&state(j).as_str())).count();
    // How long jobs waited for a machine: queued to running.
    let mut waits: Vec<u64> = day.iter().filter_map(|j| Some(j["started_ms"].as_u64()?.saturating_sub(j["at_ms"].as_u64()?))).collect();
    waits.sort();
    let median = waits.get(waits.len() / 2).copied();
    let spent: f64 = day.iter().filter_map(|j| job_cost(j)).sum();
    let github: f64 = day.iter().filter_map(|j| github_cost(j)).sum();

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
        if ["failed", "orphan", "swept"].contains(&state(j).as_str()) { hour.1 += 1 } else { hour.0 += 1 }
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
    if jobs.is_empty() { return overview_empty_cards(v.plane.label()) }
    let latest = job_table(&jobs[..jobs.len().min(5)]);
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
    // The day in one card; the latest jobs in their own, with all of them a click away.
    let jobs_card = format!(r#"<section class="card big jobs-card"><div class="card-head"><h2>Jobs</h2><span class="card-sub">Last 24 hours</span></div>{figures}{split}</section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div>{latest}<div class="card-foot end"><a class="chip" href="/?p=jobs">All jobs ›</a></div></section>"#);

    jobs_card
}

/// Overview's cards before any job has run: the same shapes with nothing in them yet, so the page already shows what
/// it will say (the day's three figures, then the latest jobs).
fn overview_empty_cards(label: &str) -> String {
    let spark = r#"<svg class="spark" viewBox="0 0 240 36" preserveAspectRatio="none" aria-hidden="true"><line class="base" x1="0" y1="35.5" x2="240" y2="35.5"/></svg>"#;
    let kpi = |big: &str, caption: &str, sub: &str| format!(r#"<div class="kpi is-empty"><div class="kpi-num">{big}</div><div class="kpi-cap">{caption}</div>{spark}<div class="kpi-sub">{sub}</div></div>"#);
    let snippet = format!(r#"<pre class="snippet empty-snippet">jobs:
  test:
    <span class="hl">runs-on: {}</span></pre>"#, esc(label));
    format!(r#"<section class="card big jobs-card"><div class="card-head"><h2>Jobs</h2><span class="card-sub">Last 24 hours</span></div><div class="kpis">{}{}{}</div></section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div>{}</section>"#,
        kpi("0", "jobs", "Hour by hour, once jobs run"), kpi("—", "median wait for a machine", "From queued to running"), kpi("$0.00", "spent", "At your cloud's prices"),
        empty_state(&icon("play"), "No jobs yet", "Every job with your label shows up here, with the machine it ran on, how long it took and what it cost:", &snippet))
}

/// Overview's cards while the control plane is being read: the same shapes, empty.
fn overview_skeleton_cards() -> String {
    let kpi = r#"<div class="kpi"><span class="sk fig"></span><span class="sk w60 thin"></span><span class="sk spark-sk"></span><span class="sk w40 thin"></span></div>"#;
    format!(r#"<section class="card big jobs-card"><div class="card-head"><h2>Jobs</h2>{LOADING}</div><div class="kpis">{}</div></section><section class="card big jobs-card"><div class="card-head"><h2>Latest</h2></div><div class="rows">{}</div></section>"#,
        kpi.repeat(3), SK_ROWS.repeat(5))
}

/// The loader, inside the first card of a page that is still reading its control plane.
const ICON_ALERT: &str = r#"<svg viewBox="0 0 24 24"><path d="M12 3.5 2.5 20h19L12 3.5Z"/><path d="M12 10v4.5M12 17.5h.01"/></svg>"#;

const LOADING: &str = r#"<span class="loading-note"><span class="mini-spin"></span>Reading your control plane…</span>"#;

fn jobs_card(v: Option<&PlaneView>, limit: usize) -> String {
    let jobs = v.map(|v| v.jobs()).unwrap_or_default();
    // Each job links to where its GitHub or GitLab is.
    let listed: Vec<serde_json::Value> = jobs.iter().take(limit).map(|j| match v { Some(v) => with_hosts(v, j), None => j.clone() }).collect();
    let rows = match v { Some(_) if !listed.is_empty() => job_table(&listed), _ => String::new() };
    let more = if jobs.len() > limit { r#"<div style="text-align:right"><a class="chip" href="/?p=jobs">All jobs ›</a></div>"#.to_string() } else { String::new() };
    let label = esc(v.map(|v| v.plane.label()).unwrap_or("superci"));
    let snippet = format!(r#"<pre class="snippet" style="text-align:left;padding:12px 16px;border-radius:10px;background:var(--card-2)">jobs:
  test:
    <span class="hl">runs-on: {label}</span></pre>"#);
    let body = match v {
        Some(v) if v.status.is_none() && v.online => SK_ROWS.repeat(3),
        Some(v) if v.status.is_none() => empty_state(&icon("jobs"), "Jobs are out of reach", "Sign in where the control plane runs to see its jobs.", ""),
        Some(_) if rows.is_empty() => empty_state(&icon("play"), "No jobs yet", "They show up here as soon as a workflow uses your label:", &snippet),
        Some(_) => format!("{rows}{more}"),
        None => empty_state(&icon("play"), "No jobs yet", "Once setup is done, every job with this label shows up here, with the machine it ran on:", &snippet),
    };
    format!(r#"<section class="card"><div class="card-head"><h2>Recent jobs</h2></div>{body}</section>"#)
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
        let mut r = agent.get(&format!("{url}/api/v4{path}")).header("private-token", token).call().map_err(|e| format!("could not reach {url}: {e}"))?;
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
fn with_hosts(v: &PlaneView, j: &serde_json::Value) -> serde_json::Value {
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
    let asked = agent.get(&format!("{api}/users/{login}")).header("accept", "application/vnd.github+json").header("user-agent", "superci-cli").call();
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

/// A page to come back to after a sign-in: only this dashboard's own `?p=…` pages.
fn safe_return(next: &str) -> String {
    let ok = next.starts_with("?p=") && next.len() < 80 && next[1..].bytes().all(|b| b.is_ascii_alphanumeric() || b"=&-_".contains(&b));
    if ok { next.to_string() } else { String::new() }
}

/// One request: the key and cookie checked, the page frame drawn at once, its live part asked for without holding the
/// dashboard's state, and everything else handled with it.
fn respond(shared: &Arc<Mutex<Dashboard>>, key: &str, req: &Req) -> Response {
    let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or("");
    // The first visit carries the key; later ones its cookie. SameSite=Lax: other sites' requests and form posts never carry it,
    // but coming back from a sign-in does (AWS's returns to 127.0.0.1, which is another site to a browser than localhost).
    if req.path == "/" && safe_eq(q("k").as_bytes(), key.as_bytes()) {
        return Response::redirect("/").with_header("set-cookie", &format!("superci_local={key}; Path=/; HttpOnly; SameSite=Lax"));
    }
    // Only as this computer's own address (no other name pointing here), and changes only from the dashboard's own pages:
    // a page on another local port shares the cookie (cookies are not kept per port), not the origin.
    let local = ["localhost:", "127.0.0.1:", "[::1]:"].iter().any(|p| req.host.strip_prefix(p).is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())));
    if !local { return message(403, "Not this computer's address", "Open the dashboard at http://localhost:8976.") }
    // Browsers say where a request comes from (Sec-Fetch-Site; "same-origin" from these pages). Without it, the Origin
    // (which these pages' no-referrer policy turns into "null" on their own forms, so it only counts when it names a site).
    let own = match req.fetch_site.as_deref() {
        Some(site) => site == "same-origin",
        None => req.origin.as_deref().is_none_or(|o| o == "null" || o == format!("http://{}", req.host)),
    };
    if req.method == "POST" && !own {
        return message(403, "Not from your dashboard", "Changes are made from the dashboard's own page. Nothing has changed.");
    }
    // Redirects back from GitHub, Cloudflare, AWS and Modal come from their sites: no cookie; the state they carry guards them.
    if !req.cookie.as_deref().is_some_and(|c| safe_eq(c.as_bytes(), key.as_bytes())) && !["/github/callback", "/github/installed", "/oauth/callback", "/oauth/modal"].contains(&req.path.as_str()) {
        return message(403, "This tab is not connected to your dashboard", "Open the link `superci dashboard` printed in your terminal when it started (it begins with http://localhost:8976/?k=). Nothing has changed.");
    }
    // A command's tab (a sign-in, a GitHub App): what it is for, in the dashboard's place. A page that looks again by
    // itself is told when it is done.
    if req.method == "GET" && req.path == "/" {
        let mut d = lock(shared);
        if q("fragment") != "1" { if let Some(page) = d.task_page() { return page } }
        else if d.task_done().is_some() { return Response::new(200, "text/html; charset=utf-8", r#"<div class="empty">Done. Back to your terminal: you can close this tab.</div>"#.to_string()) }
    }
    if req.method == "GET" && req.path == "/" {
        let new_here = match q("new") { "1" => Some(true), "0" => Some(false), _ => None };
        if q("fragment") == "1" {
            shared.lock().unwrap_or_else(|e| e.into_inner()).gitlab_shown = q("g").chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).take(12).collect();
            return Response::new(200, "text/html; charset=utf-8", Dashboard::live(shared, q("p"), q("plane").parse().ok(), new_here, req.last.as_deref()))
        }
        return Dashboard::page(q("p"));
    }
    let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
    d.views = None;
    // Actions and sign-ins are told in the terminal too (never their codes or tokens).
    let r = d.handle(req);
    d.keep();
    match &r {
        Ok(ok) => println!("{} {} → {}", req.method, req.path, ok.status),
        Err(e) => println!("{} {} → failed: {e}", req.method, req.path),
    }
    r.unwrap_or_else(|e| message(500, "Something went wrong", &esc(&e)))
}

/// A change to a control plane restarts it (a new secret is a new version): waits (up to 20 s) until it shows the
/// change, so the page that follows shows it too.
/// A control plane's status for a change made now: it may still be taking this session's key (just after the
/// dashboard started), so asked again for a few seconds before giving up.
fn status_soon(plane_url: &str, key: &str) -> Option<serde_json::Value> {
    for i in 0..8 {
        if let Some(s) = cloudflare::status_light(plane_url, key) { return Some(s) }
        if i < 7 { std::thread::sleep(Duration::from_millis(1500)) }
    }
    None
}

/// Whether it did.
fn wait_for(plane_url: &str, key: &str, done: impl Fn(&serde_json::Value) -> bool) -> bool {
    for _ in 0..40 {
        if cloudflare::status_light(plane_url, key).is_some_and(|s| done(&s)) { return true }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Every control plane's view, read in parallel (with this session's key where it was accepted).
fn read_views(planes: &[(Plane, bool)], key: &str) -> Vec<PlaneView> {
    std::thread::scope(|s| {
        let handles: Vec<_> = planes.iter().map(|(p, keyed)| s.spawn(move || view::plane_view(p, keyed.then_some(key)))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
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
        for _ in 0..45 {
            match cloudflare::plane_move_call(plane.url(), key, token, path, &body)? {
                (200, v) => return Ok(v),
                (403, _) | (404, _) | (500..=599, _) => std::thread::sleep(Duration::from_secs(2)),
                (s, v) => return Err(format!("{} answered {s}: {v}", plane.url())),
            }
        }
        Err(format!("{} did not take its move token", plane.url()))
    };
    let out = call(from, &out_token, "/move/export", serde_json::json!({}))?;
    for (name, k) in [("GITHUB_APP", "app"), ("GITLAB", "gitlab"), ("ROUTING", "routing"), ("MACHINE", "machine")] {
        if !out[k].is_null() { w.put(to, name, &out[k].to_string())? }
    }
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
    if old["aws_regions"].as_array().is_some_and(|r| !r.is_empty()) { w.put(to, "AWS_REGIONS", &old["aws_regions"].to_string())? }
    // Networks of the account's own for AWS's machines, and GitHub's full image for Cloudflare's containers.
    if old["aws_networks"].as_object().is_some_and(|n| !n.is_empty()) { w.put(to, "AWS_NETWORKS", &old["aws_networks"].to_string())? }
    if let Some(i) = old["cloudflare_image"].as_str() { w.put(to, "CF_IMAGE", i)? }
    // Where Cloudflare's containers start, when set to other than the default.
    if let Some(l) = old["cloudflare_location"].as_str().filter(|l| *l != "enam") { w.put(to, "CF_LOCATION", l)? }
    call(to, &in_token, "/move/import", serde_json::json!({ "state": out["state"] }))?;
    let (has_app, has_gitlab) = (!out["app"].is_null(), !out["gitlab"].is_null());
    // It has taken them all: the first App, each further one, GitLab.
    let all_apps = |s: &serde_json::Value| { let have: Vec<u64> = status_apps(s).iter().filter_map(|a| a["id"].as_u64()).collect(); coming.iter().all(|id| have.contains(id)) };
    for _ in 0..30 {
        if cloudflare::status(to.url(), key).is_some_and(|s| (!has_app || !s["app"].is_null()) && all_apps(&s) && (!has_gitlab || !s["gitlab"].is_null())) { break }
        std::thread::sleep(Duration::from_secs(2));
    }

    // 2. The runner providers: the same clouds, started by the new one (the old one keeps its own).
    step(1);
    let mut agents: Vec<superci_core::plane::Agent> = cloudflare::status(to.url(), key).and_then(|s| serde_json::from_value(s["agents"].clone()).ok()).unwrap_or_default();
    let agents_before = agents.len();
    let mut left = vec![];
    for c in plan {
        match c {
            Carry::Comes("cloudflare") => match to {
                Plane::Cloudflare { account_id, script, label, .. } => {
                    w.cf.as_ref().ok_or("Sign in with Cloudflare first.")?.deploy(account_id, script, label, true)?;
                    w.put(to, "CONTAINERS", "on")?;
                }
                _ => {
                    let url = w.cf.as_ref().ok_or("Sign in with Cloudflare first.")?.deploy_runners(cf_account.as_deref().ok_or("no Cloudflare account")?, to.url(), to.plane_id())?;
                    agents.retain(|a| a.cloud != "cloudflare");
                    agents.push(superci_core::plane::Agent { cloud: "cloudflare".into(), url });
                }
            },
            Carry::Comes("modal") => match to {
                Plane::Modal { .. } => w.put(to, "CONTAINERS", "on")?,
                _ => {
                    let keys: serde_json::Value = ureq::get(&format!("{}/.well-known/jwks.json", to.url())).call().map_err(|e| e.to_string())?.body_mut().read_json().map_err(|e| e.to_string())?;
                    let url = modal::deploy_runners(w.modal.as_ref().ok_or("Sign in with Modal first.")?, to.url(), to.plane_id(), &keys)?;
                    for _ in 0..20 { if ureq::get(&format!("{url}/health")).call().is_ok_and(|r| r.status() == 200) { break } std::thread::sleep(Duration::from_secs(3)) }
                    agents.retain(|a| a.cloud != "modal");
                    agents.push(superci_core::plane::Agent { cloud: "modal".into(), url });
                }
            },
            // AWS: a Cloudflare control plane assumes its own role in the account (one in AWS uses its own).
            Carry::Comes("aws") => if let Plane::Cloudflare { .. } = to {
                let creds = w.aws.as_ref().ok_or("Sign in with AWS first.")?;
                let account = aws_account.clone().ok_or("Sign in with AWS first.")?;
                let region = old["aws"]["region"].as_str().unwrap_or("us-east-1").to_string();
                let role_arn = aws::connect_runners(creds, &account, to.url(), to.plane_id(), AUDIENCE)?;
                aws::make_networks(creds, to.plane_id(), &status_regions(&old))?;
                let token = random_token(24);
                w.put(to, "AWS_CONNECT", &serde_json::json!({ "region": region, "token": token }).to_string())?;
                let report = serde_json::json!({ "token": token, "accountId": account, "region": region, "roleArn": role_arn });
                let mut done = Err(String::new());
                for _ in 0..12 { done = report_to_plane(to.url(), &report); if done.is_ok() { break } std::thread::sleep(Duration::from_secs(3)) }
                done.map_err(|e| format!("AWS runners: {e}"))?;
            },
            Carry::Cannot(p, why) => left.push(format!("{} runners stayed behind: {why}.", cloud_name(p))),
            _ => {}
        }
    }
    if agents.len() != agents_before || plan.iter().any(|c| matches!(c, Carry::Comes("cloudflare") | Carry::Comes("modal"))) {
        w.put(to, "AGENTS", &serde_json::to_string(&agents).map_err(|e| e.to_string())?)?;
    }

    // 3. The switch: the old one passes on what still reaches it, then the new one takes GitHub and GitLab over.
    step(2);
    cloudflare::plane_post(from.url(), key, "/move/away", &serde_json::json!({ "to": to.url() }))?;
    let mut claimed = Err("not tried".to_string());
    for _ in 0..30 {
        claimed = cloudflare::plane_post(to.url(), key, "/move/claim", &serde_json::json!({ "from": from.url() }));
        if claimed.as_ref().is_ok_and(|c| !has_app || c["github"] == true) { break }
        std::thread::sleep(Duration::from_secs(2));
    }
    match claimed {
        Ok(c) if !has_app || c["github"] == true => {
            // An organization whose App would not move (deleted on GitHub, say): its jobs still arrive through the old
            // control plane, which passes them on.
            for f in c["github_failed"].as_array().into_iter().flatten().filter_map(|f| f.as_str()) { left.push(format!("GitHub would not point an App here ({f}); its jobs still come through the old control plane.")) }
        }
        _ => return Err("The new control plane has not taken GitHub over yet. Jobs still reach it through the old one, which passes them on; Try again finishes the switch.".into()),
    }

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
    for _ in 0..12 {
        let mut r = agent.post(&format!("{plane_url}/aws/callback")).header("content-type", "application/json").send(report.to_string()).map_err(|e| e.to_string())?;
        let text = r.body_mut().read_to_string().unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if r.status().as_u16() == 200 && v["connected"] == true { return Ok(()) }
        if r.status().as_u16() != 200 { return Err(format!("the control plane refused the connection: {text}")) }
        last = v["error"].as_str().unwrap_or("not connected").to_string();
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err(format!("the role exists but the control plane could not use it yet: {last}"))
}

/// A new or updated control plane takes a few seconds to answer.
/// Until the control plane answers as `version` (up to a minute).
fn wait_for_version(url: &str, version: &str) {
    for _ in 0..60 {
        if cloudflare::health(url).is_some_and(|h| h["version"] == version) { return }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn wait_online(url: &str) {
    for _ in 0..30 {
        if cloudflare::health(url).is_some() { return }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// The manifest flow's last step, from this machine: the code becomes the App's id, key and webhook secret.
/// The manifest flow's last step, from this machine: GitHub's code becomes the App's id, key and webhook secret.
/// `api`: that GitHub's API (see `github::api_base`).
fn convert_manifest_code(api: &str, host: Option<&str>, code: &str) -> Result<github::App> {
    if code.len() < 8 || code.len() > 100 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("invalid manifest code".into()) }
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent.post(&format!("{api}/app-manifests/{code}/conversions"))
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

    /// With SUPERCI_PREVIEW_DIR set, each screen is also written there as a page, to look at.
    fn preview(name: &str, fragment: &str) {
        let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") else { return };
        let page = String::from_utf8(Dashboard::page("overview").body).unwrap()
            .replace(r#"<main class="main" id="live">"#, r#"<main class="main" id="live" data-static>"#)
            .replacen("<script>(function(){var main", "<script>(function(){return;var main", 1);
        let (a, b) = page.split_once(r#"data-static>"#).unwrap();
        let rest = &b[b.find("</main>").unwrap()..];
        let gated = if fragment.contains("data-gated") { r#"<script>document.body.classList.add('gated')</script>"# } else { "" };
        let gated = format!(r#"{gated}<script>var u=document.querySelector('[data-side]');if(u){{document.getElementById('side-update').innerHTML=u.innerHTML;u.remove()}}document.querySelectorAll('form.picker').forEach(superciPick)</script>"#);
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
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "containers": true, "agents": [] })) };
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
        *lock(&d.updating) = Some(Update { plane: old.plane.plane_id().to_string(), steps: &UPDATE_CLOUDFLARE, at: 1, result: None, ended_ms: 0 });
        let html = d.render("jobs", &[old.clone()], None);
        assert!(html.contains(r#"<span class="upd-step"><span class="mini-spin"></span>Giving it its address</span>"#) && html.contains("data-refresh") && !html.contains(">Update to "));
        let planes = d.render("planes", &[old.clone()], None);
        preview("planes-updating", &planes);
        assert!(planes.contains(r#"<small>Updating to "#) && planes.contains("Giving it its address <span class=\"faint\">(2 of 3)</span>") && !planes.contains(r#"<span class="pick-end"><div class="upd""#), "its progress in the row's second line");
        *lock(&d.updating) = Some(Update { plane: old.plane.plane_id().to_string(), steps: &UPDATE_CLOUDFLARE, at: 0, result: Some(Err("Cloudflare said no".into())), ended_ms: 0 });
        assert!(d.render("jobs", &[old.clone()], None).contains("The update stopped: Cloudflare said no"));
        *lock(&d.updating) = None;
        d.render("jobs", &[old.clone()], None);
        assert!(lock(side()).contains(&format!("Update to {DASHBOARD_VERSION}")) && String::from_utf8(Dashboard::page("jobs").body).unwrap().contains("Update available"), "drawn with the next page at once");
        // Settings from before is the control plane's page; what is new has its own.
        assert_eq!(section_name("settings"), "planes");
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
        // Move here's confirmation says what comes along; the other one can be deleted.
        assert!(html.contains(r#"<dialog class="dlg" id="move-0""#) && html.contains("Your GitHub App and its installations") && html.contains(r#"<input type="hidden" name="plane" value="xyz000000001"><button class="button primary sm">Move</button>"#));
        assert!(html.contains(r#"<form method="post" action="/plane/delete""#) && html.contains("Delete Modal · acme?") && html.contains(r#"document.getElementById('delete-0').showModal()">Delete…</button>"#));
        assert!(html.find("superci.acme.workers.dev").unwrap() < html.find("Move here</button>").unwrap(), "the one in use first");
        let mut moved = view.clone();
        moved.moved_to = Some(other.plane.url().into());
        let mut took_over = other.clone();
        took_over.github = true;
        took_over.status = Some(serde_json::json!({ "containers": false, "agents": [], "jobs": [] }));
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
    }

    #[test]
    fn several_organizations_are_rows_of_their_own_with_one_more_to_add() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let perms = serde_json::json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" });
        let status = serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "agents": [], "jobs": [],
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
        let jobs = d.render("jobs", &[corp], None);
        assert!(jobs.contains("https://ghe.corp.example/beta/api/actions/runs/50/job/5") && jobs.contains("https://github.com/acme/app/actions/runs/60/job/6"), "each job links to its own GitHub");
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
        let status = serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "agents": [{ "cloud": "modal", "url": "https://acme--superci-runners-p1.modal.run" }], "permissions": {} });
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
            "containers": true, "own_cloud": "cloudflare", "agents": [{ "cloud": "modal", "url": "https://acme--superci-runners-p1.modal.run" }],
            "aws": { "account_id": "123456789012", "region": "us-east-1", "connected": true }, "aws_regions": ["us-east-1"],
            "routing": { "rules": [], "order": [{ "cloud": "aws" }, { "cloud": "cloudflare" }, { "cloud": "modal" }] },
            "jobs": [{ "job_id": 7, "run_id": 70, "repo": "acme/app", "state": "running", "cloud": "modal", "at_ms": 1 }],
        });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status.clone()) };
        let html = d.render("runners", &[view.clone()], None);
        preview("runners-remove", &html);
        // In the order: AWS, its on-demand machines (no removal of their own: they go with AWS), Cloudflare, Modal.
        assert!(!html.contains(r#"id="pool-remove-1""#));
        let dialog = |cloud: &str| { let i = ["aws", "aws-on-demand", "cloudflare", "modal"].iter().position(|c| *c == cloud).unwrap(); let start = html.find(&format!(r#"id="pool-remove-{i}""#)).unwrap(); html[start..start + html[start..].find("</dialog>").unwrap()].to_string() };
        // Each names the control plane it is for (another may be in use by the time it is sent).
        assert_eq!(html.matches(r#"action="/runners/remove""#).count(), html.matches(r#"<input type="hidden" name="plane" value="p1"><input type="hidden" name="cloud""#).count());
        // Each provider's settings open its removal, which says what goes where; the control plane and sign-ins stay.
        assert!(html.contains("document.getElementById('pool-remove-0').showModal()") && html.contains("Your control plane and your sign-ins stay"));
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
    fn runners_are_pools_in_order_each_with_its_settings_and_a_machine_picker() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = serde_json::json!({
            "containers": true, "agents": [{ "cloud": "modal", "url": "https://acme--superci-runners.modal.run" }],
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
        let html = d.render("runners", &[view.clone()], None);
        preview("runners", &html);
        assert!(html.contains(">$0.82 this month · included<"), "cf");
        assert!(html.contains(">$1.50 this month · $0.50 billed<"), "modal");
        assert!(!html.contains(">$12.40 this month<"), "a budget already says the spend");
        // The pools as ordered; ones added since (Modal) after them.
        let at = |s: &str| html.find(s).unwrap_or_else(|| panic!("{s}"));
        assert!(at(r#"data-cloud="cloudflare""#) < at(r#"data-cloud="aws""#) && at(r#"data-cloud="aws""#) < at(r#"data-cloud="modal""#) && !html.contains("machine:m1"));
        // AWS's on-demand machines are a place of their own, right under its spot machines until moved.
        assert!(html.contains(r#"<ol class="pools" data-order="cloudflare,aws,aws-on-demand,modal">"#) && html.contains(r#"draggable="true""#));
        assert!(html.contains("<strong>AWS on-demand</strong><small>On-demand machines · one per job, never interrupted</small>") && html.contains(r#"<input type="checkbox" name="on" checked> Use on-demand machines"#));
        assert!(html.contains("when that one cannot start it (no spot machine, a failure), to the next"));
        // Limits show only where set; settings are in each pool's dialog.
        assert!(html.contains(">10 at once<") && html.contains(">$12.40 of $50.00<") && !html.contains("No cap"));
        assert!(html.contains(r#"<dialog class="dlg" id="pool-3""#) && html.matches("Monthly budget").count() == 4);
        assert!(html.contains(r#"name="current" value="cloudflare,aws,aws-on-demand,modal""#));
        // One entry for AWS in the machine picker.
        assert_eq!(html.matches(r#"<span class="fit" data-caps="[]""#).count(), 0);
        // Turned off, it says so, where the order has it (here: last), with its switch off.
        let mut off = view.clone();
        off.status.as_mut().unwrap()["routing"]["order"] = serde_json::json!([{ "cloud": "aws" }, { "cloud": "cloudflare" }, { "cloud": "aws-on-demand", "off": true }]);
        let html_off = d.render("runners", &[off], None);
        preview("runners-on-demand-off", &html_off);
        assert!(html_off.contains(r#"data-order="aws,cloudflare,aws-on-demand,modal""#) && html_off.contains(r#"<span class="pill open"><i></i>Off</span>"#) && html_off.contains(r#"<input type="checkbox" name="on"> Use on-demand machines"#));
        assert!(!html.contains("1 running") && html.contains("Cloudflare runs 10 at once") && !html.contains("Exceptions"));
        // AWS's regions, in order, in its dialog (the next when one has no spot capacity).
        assert!(html.contains("x64, arm64 · N. Virginia, then Ohio") && html.contains(r#"<li class="rg" draggable="true" data-region="us-east-1""#)
            && html.find(r#"data-region="us-east-1""#).unwrap() < html.find(r#"data-region="us-east-2""#).unwrap() && html.contains(r#"<option value="us-west-2">US West (Oregon) · us-west-2</option>"#) && !html.contains(r#"<option value="us-east-1">"#));
        assert!(!html.contains("Default machine"), "machines are on Workflows");
        // AWS: a network of your own, by region, as it is set.
        assert!(html.contains(r#"<summary>Your own network</summary>"#) && html.contains(">us-east-2 subnet-0aaaaaaaa sg-0cccccccc private</textarea>"));
        // Cloudflare's location, as set (here: Cloudflare chooses); the area near the code hosts marked, and AWS's region.
        assert!(html.contains(r#"<option value="auto" selected>Automatic: Cloudflare chooses</option>"#) && html.contains("Eastern North America · closest to GitHub</option>"));
        assert!(html.contains("us-east-1 · closest to GitHub</small>"));
        // Cloudflare: where GitHub's full image is published, if anywhere (empty: the small image).
        assert!(html.contains(r#"name="image" value="""#) && html.contains("GitHub&#39;s full image") || html.contains("GitHub's full image"));
        let html = d.render("workflows", &[view], None);
        preview("workflows", &html);
        // The default machine, in words, changed in its own dialog (which starts at it).
        assert!(html.contains(r#"<span class="lim">4 CPU</span><span class="lim">100 GB disk</span><span class="lim">x64</span><span class="lim">Linux</span><span class="lim">spot</span>"#));
        assert!(html.contains(r#"<dialog class="dlg" id="default-machine""#) && html.contains(r#"<option value="4" selected>4</option>"#) && html.contains(r#"<option value="100" selected>100 GB</option>"#));
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
        let status = serde_json::json!({ "containers": true, "agents": [], "jobs": [
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
    fn permissions_say_what_is_missing_why_and_how_to_give_it() {
        use superci_core::permissions::AWS_RUNNER;
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        d.planes = vec![plane.clone()];
        let status = |denied: serde_json::Value| serde_json::json!({ "containers": true, "agents": [], "jobs": [],
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
        assert!(side_update(&old, None, &[]).contains("It also asks for 4 new permissions in AWS, 1 new permission in GitHub."));
        old.version = Some("0.9.20".into());
        assert!(side_update(&old, None, &[]).contains("It also asks for 2 new permissions in AWS, 1 new permission in GitHub."), "reading its network, and the network; running a job again");
        // While it updates, on every page: says so (also when the control plane does not answer a moment, or already
        // answers with the new version); once done, that it is.
        let mut u = Update { plane: "p1".into(), steps: &UPDATE_CLOUDFLARE, at: 1, result: None, ended_ms: 0 };
        let mut new = view(status(serde_json::json!({})));
        new.online = false;
        let card = side_update(&new, Some(&u), &[]);
        assert!(card.contains(&format!("Updating to {DASHBOARD_VERSION}")) && card.contains("It keeps going while you look around.") && card.contains("mini-spin"));
        u.result = Some(Ok(()));
        assert!(side_update(&new, Some(&u), &[]).contains(&format!("Updated to {DASHBOARD_VERSION}")));
        // Uploaded, and the version before still answers (its cloud is switching it over): said, looked at again, and
        // no Update button meanwhile. Still the old one ten minutes on: the button is back.
        let mut before = view(status(serde_json::json!({})));
        before.version = Some("0.9.20".into());
        u.ended_ms = now_ms();
        let card = side_update(&before, Some(&u), &[]);
        assert!(card.contains(&format!("Restarting with {DASHBOARD_VERSION}")) && card.contains("data-refresh") && !card.contains(">Update to ") && !card.contains("Updated to"));
        *lock(&d.updating) = Some(Update { plane: "p1".into(), steps: &UPDATE_CLOUDFLARE, at: 2, result: Some(Ok(())), ended_ms: now_ms() });
        let planes = d.render("planes", &[before.clone()], None);
        assert!(planes.contains("Restarting…") && !planes.contains(">Update to "), "no second update while it restarts");
        u.ended_ms = now_ms() - 11 * 60_000;
        assert!(side_update(&before, Some(&u), &[]).contains(">Update to ") && side_update(&before, Some(&u), &[]).contains("Update available"));
        *lock(&d.updating) = None;
        // Its version had them, but its role was found to lack them (made before): the update says so too.
        old.version = Some("0.9.29".into());
        let lacks: Vec<_> = AWS_RUNNER.iter().filter(|n| n.since == "0.9.12").collect();
        assert!(side_update(&old, None, &lacks).contains("It also asks for 1 new read permission in AWS.") && !side_update(&old, None, &[]).contains("asks for"));
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
    fn it_answers_only_on_this_computers_address_and_changes_only_from_its_own_pages() {
        let shared = Arc::new(Mutex::new(with_cloudflare(&[("a1", "Acme Inc.")])));
        let req = |host: &str, origin: Option<&str>| Req { method: "POST".into(), path: "/routing".into(), query: vec![], cookie: Some("k".repeat(32)), last: None, body: b"action=max_cpu&max_cpu=8".to_vec(), host: host.into(), origin: origin.map(str::to_string),
            fetch_site: origin.map(|o| if o == "http://localhost:8976" || o == "null" { "same-origin".to_string() } else { "same-site".to_string() }) };
        let text = |r: Response| (r.status, String::from_utf8_lossy(&r.body).to_string());
        let (status, body) = text(respond(&shared, &"k".repeat(32), &req("localhost:8976", Some("http://localhost:3000"))));
        assert!(status == 403 && body.contains("Not from your dashboard"), "{status}");
        let (status, body) = text(respond(&shared, &"k".repeat(32), &req("attacker.example:8976", Some("http://attacker.example:8976"))));
        assert!(status == 403 && body.contains("Not this computer"), "{status}");
        // From its own page it goes through (and here finds no control plane to change), its Origin given or, as these
        // pages' no-referrer policy makes browsers send it, "null".
        for origin in ["http://localhost:8976", "null"] {
            let (status, body) = text(respond(&shared, &"k".repeat(32), &req("localhost:8976", Some(origin))));
            assert!(!body.contains("Not from your dashboard") && !body.contains("Not this computer"), "{origin}: {status}");
        }
        // Another local page, its browser saying so.
        let mut other = req("localhost:8976", Some("null"));
        other.fetch_site = Some("same-site".into());
        assert_eq!(respond(&shared, &"k".repeat(32), &other).status, 403);
    }

    #[test]
    fn github_and_gitlab_are_rows_of_one_list_where_jobs_come_from() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let mut view = PlaneView { plane, online: true, github: true, installed: true, aws: false, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "containers": true, "agents": [], "jobs": [], "app": { "slug": "superci-acme", "owner": "acme", "org": true }, "installations": [{ "account": "acme", "repositories": "all" }] })) };
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
        // Each job links to its own GitLab; each token has its own row under Permissions.
        let jobs = d.render("jobs", &[two.clone()], None);
        assert!(jobs.contains("https://gitlab.corp.example/corp/api/-/jobs/5") && jobs.contains("https://gitlab.com/acme/app/-/jobs/6"));
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
        assert!(gh.contains(r#"aria-label="Add an organization""#) && gh.contains("<h2>Where is your GitHub?</h2>") && gh.contains("GitHub Enterprise") && gh.contains(r#"<div class="perm-field" data-own hidden><input type="text" name="host""#)
            && gh.contains(r#"name="login" placeholder="your-organization" required"#) && gh.contains(">Continue on GitHub</button>"));
        // Not connected yet: the same dialog, as connecting.
        let fresh = PlaneView { github: false, installed: false, status: Some(serde_json::json!({ "containers": true, "agents": [], "jobs": [] })), ..view.clone() };
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
        view.status = Some(serde_json::json!({ "containers": true, "agents": [], "gitlab": { "url": "https://gitlab.com" },
            "jobs": [{ "job_id": 1977, "run_id": 2366, "repo": "acme/app", "state": "done", "at_ms": now_ms() - 60_000, "provider": "gitlab", "cloud": "cloudflare", "machine_id": "m",
                "installation_id": 0, "runner": null, "runner_id": null, "machine_type": "2cpu-8gb", "error": null, "seen_in_progress": true }] }));
        assert!(view.ready());
        assert_eq!(closest(Some(&view)), "closest to GitHub and GitLab");
        assert!(d.render("jobs", &[view.clone()], None).contains(r#"href="https://gitlab.com/acme/app/-/jobs/1977""#));
        // Both connected: a switch between them.
        view.installed = true;
        let workflows = d.render("workflows", &[view], None);
        preview("workflows-both", &workflows);
        // Limits: the largest machine, and public repositories allowed (none yet).
        assert!(workflows.contains("<strong>Longest job</strong>") && workflows.contains(r#"name="max_hours" min="1" max="120" value="6""#), "a job's machine lives six hours unless set");
        assert!(workflows.contains("<h2>Limits</h2>") && workflows.contains(r#"name="max_cpu" min="1" max="192" value="32""#) && workflows.contains(r#"value="public_add""#) && !workflows.contains("tag-chips"));
        assert!(workflows.contains(r#"<input type="radio" name="host" id="use-gh" checked>"#) && workflows.contains("Turn the project on in") && workflows.contains("tags: [superci]"));
    }

    #[test]
    fn overview_says_how_jobs_went_what_needs_a_look_and_where_they_ran() {
        let d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let plane = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let t = now_ms() - 3_600_000;
        let job = |id: u64, state: &str, cloud: &str, wait: Option<u64>, took: u64, error: Option<&str>| { let t = t - (id % 4) * 3 * 3_600_000; serde_json::json!({ "job_id": id, "run_id": 9, "repo": "acme/app", "state": state, "at_ms": t,
            "runner": null, "runner_id": null, "installation_id": 1, "cloud": cloud, "machine_id": if cloud.is_empty() { serde_json::Value::Null } else { "m".into() }, "machine_type": "4cpu-12gb", "error": error, "seen_in_progress": wait.is_some(),
            "usd_per_hour": 0.4, "started_ms": wait.map(|w| t + w), "ended_ms": wait.map(|w| t + w + took), "launched_ms": t, "cpu": 4,
            "name": (["", "test", "lint", "build", "e2e", "deploy", "e2e"][id as usize % 7]), "workflow": "ci" }) };
        let status = serde_json::json!({ "containers": true, "agents": [], "aws": { "account_id": "1", "region": "us-east-1" }, "jobs": [
            job(1, "done", "cloudflare", Some(8_000), 21_000, None), job(2, "done", "cloudflare", Some(9_000), 30_000, None), job(3, "done", "aws", Some(45_000), 240_000, None),
            job(4, "failed", "cloudflare", None, 0, Some("runner container: 502 the container cannot be kept running: JsValue(Error: Container connection temporarily unavailable)")),
            job(5, "waiting", "", None, 0, Some("waiting: Cloudflare runs 20 at once")), job(6, "failed", "cloudflare", None, 0, Some("runner container: 502 the container cannot be kept running")) ] });
        let view = PlaneView { plane, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(status) };
        let html = d.render("overview", &[view], None);
        preview("overview-new", &html);
        // The control plane is one quiet chip: no address, version or label.
        assert!(!html.contains("plane-chip") && !html.contains("superci.acme.workers.dev") && !html.contains("runs-on: superci") && !html.contains("Copy"));
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
        for section in ["overview", "repos", "runners", "workflows", "jobs", "planes", "changes", "add-runners"] {
            let page = skeleton(section, section_title(section));
            preview(&format!("skeleton-{section}"), &page);
            assert!(page.contains("Reading your control plane"), "{section}");
        }
    }

    #[test]
    fn a_move_brings_the_runner_providers_and_an_empty_overview_shows_its_shape() {
        let mut d = with_cloudflare(&[("a1", "Acme Inc.")]);
        let cf = Plane::Cloudflare { account_id: "a1".into(), account_name: "Acme Inc.".into(), script: "superci".into(), url: "https://superci.acme.workers.dev".into(), plane_id: "p1".into(), label: "superci".into() };
        let aws_plane = Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "p2".into(), label: "superci".into() };
        let from = PlaneView { plane: cf, online: true, github: true, installed: true, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()),
            status: Some(serde_json::json!({ "containers": true, "own_cloud": "cloudflare", "agents": [{ "cloud": "modal", "url": "https://m" }], "aws": { "account_id": "123456789012", "region": "us-east-1" }, "jobs": [] })) };
        let to = PlaneView { plane: aws_plane, online: true, github: false, installed: false, aws: true, runners: true, gitlab: false, moved_to: None, standby: false, version: Some(superci_core::plane::VERSION.into()), status: Some(serde_json::json!({ "jobs": [] })) };
        // Cloudflare's containers come (an agent there for the new one); Modal needs a sign-in first; AWS is the new one's own account.
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
        assert!(page.contains("<h2>Jobs</h2>") && page.contains("Reading your control plane"));
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
        kept.key.as_mut().unwrap().name = format!("DASHBOARD_KEY_{}_ABCDEF", now_ms() / 1000 + 3600);
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
        *lock(&d.updating) = Some(Update { plane: "abcdef123456".into(), steps: &UPDATE_AWS, at: 2, result: Some(Err("AccessDenied: lambda:UpdateFunctionCode".into())), ended_ms: now_ms() });
        let mut steps = vec![];
        assert_eq!(d.wait(Background::Update, &mut |s: &str| steps.push(s.to_string())).unwrap_err(), "AccessDenied: lambda:UpdateFunctionCode");
        assert_eq!(steps, UPDATE_AWS[..3], "a stop says the steps up to where it stopped");
        assert_eq!(d.wait(Background::Move, &mut |_| {}).unwrap_err(), "no move was started");
    }

    #[test]
    fn what_a_person_does_in_the_browser() {
        // A sign-in with one cloud goes straight to it, and is done once that cloud is signed in to.
        let mut d = Dashboard::new().for_task(Task::Login(Some("aws")));
        assert_eq!(d.task_page().unwrap().headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.as_str()), Some("/connect/aws"));
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
        // A GitHub App: done when GitHub comes back from choosing its repositories.
        let mut g = Dashboard::new().for_task(Task::GitHub { login: "acme".into(), host: String::new(), started: true, done: false });
        assert!(g.task_done().is_none() && String::from_utf8(g.task_page().unwrap().body).unwrap().contains("Finish on GitHub"));
        let _ = g.handle(&Req { method: "GET".into(), path: "/github/installed".into(), query: vec![], cookie: None, last: None, body: vec![], host: String::new(), origin: None, fetch_site: None });
        assert!(g.task_done().unwrap().starts_with("GitHub is connected for acme"));
        let done = String::from_utf8(g.task_page().unwrap().body).unwrap();
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/done-github.html"), &done).unwrap() }
        assert!(done.contains("<h1>GitHub is connected</h1>") && done.contains("acme&#39;s repositories") && !done.contains(">Back<"));
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
        // The sidebar's More menu signs out: nothing is left on this machine, and the page says so once.
        assert!(Dashboard::sidebar("overview").contains(r#"<details class="menu side-more"><summary>"#) && Dashboard::sidebar("overview").contains(r#"<form method="post" action="/signout"><button>Sign out</button></form>"#));
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
