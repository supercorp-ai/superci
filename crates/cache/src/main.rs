//! `superci-cache`: GitHub's Actions cache on a SuperCI machine. It serves the cache protocol GitHub's own cache
//! client speaks (cache service v2: a few JSON calls, then the archive up and down) to the job's actions, and keeps
//! what they save in the control plane's own bucket, in the machine's region. It holds no keys: for every lookup and
//! save it asks the control plane, with the job's own token (GitHub's, which says what the job may read and write),
//! and gets links to the bucket that each do one thing (see `superci_core::cache`).
//!
//! `superci-cache wrap <externals>` puts itself in front of the runner's own Node (`externals/node*/bin/node`, which
//! runs every JavaScript action): run as `node`, it points the action's `ACTIONS_RESULTS_URL` here when this program
//! is up, and becomes the real Node (`node.real`) with everything else as it was. (Not a shell script: a shell drops
//! the variables whose names it cannot take, such as an action's `INPUT_IF-NO-FILES-FOUND`. And it is one static
//! program, so it runs in a job's containers too, which see the externals at /__e.)
//! What is not the cache (artifacts) goes on to GitHub's results service unchanged (GitHub's client keeps only the
//! address's host, so that is where it goes: GitHub's one address for it).
//! `superci-cache serve <externals>` listens on the machine's private address (a job's containers reach it too;
//! nothing off the machine does) and writes that address beside the runner's externals for the script. Nothing is
//! done without the job's own token: the control plane checks it at every lookup and save.
//!
//! When the control plane cannot be reached, a job of GitHub's uses GitHub's own cache as it would without SuperCI.
//! A lookup that finds nothing here is also asked of GitHub, so caches saved before SuperCI are used until they age.
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod http;
use http::{Request, Response};


const SERVICE: &str = "twirp/github.actions.results.api.v1.CacheService/";
const ARTIFACTS: &str = "twirp/github.actions.results.api.v1.ArtifactService/";
/// GitHub's results service (its cache, artifacts), where what is not this cache goes.
const GITHUB: &str = "https://results-receiver.actions.githubusercontent.com";
/// Where it listens, on the machine's private address: Turbo, Nx and Bazel are told of it before it starts.
const PORT: u16 = 8257;
/// What the tools are given as their token here: a request with another (a workflow's own, for Vercel's remote cache)
/// goes on to where that token is for.
const TOOL_TOKEN: &str = "superci";
/// Vercel's API, where Turbo's requests with a token of Vercel's go.
const VERCEL: &str = "https://vercel.com/api";

/// Where the address is written, in the runner's externals folder (a job's containers see it at /__e).
const ADDRESS: &str = "superci-cache.address";
/// Parts of a download fetched at a time, and their size.
const CHUNK: u64 = 16 << 20;
/// A large upload (a tool's, GitLab's) goes to the bucket in pieces of this.
const PIECE: u64 = 64 << 20;

fn main() {
    let given: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if given.first().and_then(|a| Path::new(a).file_stem()).is_some_and(|n| n == "node") { node(given) }
    let args: Vec<String> = std::env::args().collect();
    let done = match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("wrap"), Some(dir)) => wrap(Path::new(dir)),
        (Some("serve"), Some(dir)) => serve(Path::new(dir)),
        _ => Err("usage: superci-cache wrap|serve <the runner's externals folder>".into()),
    };
    if let Err(e) = done { eprintln!("superci-cache: {e}"); std::process::exit(1) }
}

/// The real Node's name beside this program, once `wrap` put it there.
const REAL: &str = if cfg!(windows) { "node.real.exe" } else { "node.real" };
const NODE: &str = if cfg!(windows) { "node.exe" } else { "node" };

/// Run as the runner's `node`: the real one, with the action's results service here when this program is up (and
/// not here already: a Node an action started itself). On Linux it becomes the real Node; on Windows, which has no
/// such thing, it runs it and ends as it ends.
fn node(given: Vec<std::ffi::OsString>) -> ! {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("."));
    let mut real = std::process::Command::new(dir.join(REAL));
    #[cfg(unix)] { use std::os::unix::process::CommandExt; real.arg0(&given[0]); }
    real.args(&given[1..]);
    let here = std::fs::read_to_string(dir.join("../..").join(ADDRESS)).unwrap_or_default();
    let here = here.trim();
    if let Ok(results) = std::env::var("ACTIONS_RESULTS_URL") {
        if !here.is_empty() && results.starts_with("http") && !results.starts_with(here) { real.env("ACTIONS_RESULTS_URL", format!("{here}/")); }
    }
    #[cfg(unix)] let e = { use std::os::unix::process::CommandExt; real.exec() };
    #[cfg(not(unix))] let e = match real.status() { Ok(s) => std::process::exit(s.code().unwrap_or(1)), Err(e) => e };
    eprintln!("superci-cache: {}: {e}", dir.join(REAL).display());
    std::process::exit(127)
}

/// Puts this program in front of each of the runner's Nodes (again: only this program is put there anew).
fn wrap(externals: &Path) -> Result<(), String> {
    // (This program, as it was started: its own path is there to read.)
    let me = std::env::current_exe().expect("this program's path");
    let mut wrapped = 0;
    for dir in std::fs::read_dir(externals).map_err(|e| format!("{}: {e}", externals.display()))?.flatten() {
        if !dir.file_name().to_string_lossy().starts_with("node") { continue }
        let (node, real) = (dir.path().join("bin").join(NODE), dir.path().join("bin").join(REAL));
        if !real.exists() {
            if !node.is_file() { continue }
            std::fs::rename(&node, &real).map_err(|e| format!("{}: {e}", node.display()))?;
        }
        let next = dir.path().join("bin").join(".node.superci");
        std::fs::copy(&me, &next).and_then(|_| executable(&next)).and_then(|_| std::fs::rename(&next, &node)).map_err(|e| format!("{}: {e}", node.display()))?;
        wrapped += 1;
    }
    if wrapped == 0 { return Err(format!("no Node in {}", externals.display())) }
    Ok(())
}

#[cfg(unix)]
fn executable(path: &Path) -> std::io::Result<()> { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)) }
#[cfg(not(unix))]
fn executable(_: &Path) -> std::io::Result<()> { Ok(()) }

/// The machine's private address (what its network gives it; containers on it reach it there).
fn private_ip() -> Result<IpAddr, String> {
    if let Some(given) = std::env::var("SUPERCI_CACHE_LISTEN").ok().filter(|g| !g.is_empty()) { return given.parse().map_err(|e| format!("SUPERCI_CACHE_LISTEN: {e}")) }
    // (The address the system would send from to the instance metadata service: no packet is sent.)
    std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect("169.254.169.254:80").and_then(|_| s.local_addr())).map(|a| a.ip()).map_err(|e| ToString::to_string(&e))
}

/// (The system's randomness, there on every system this runs on.)
fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("the system's randomness");
    hex(&b)
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

/// A query's percent-encoding undone (a `+` stays: Azure's client encodes its block ids' own).
fn unpercent(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) { out.push(v); i += 3; continue }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn serve(externals: &Path) -> Result<(), String> {
    let plane = std::env::var("SUPERCI_CACHE_PLANE").map_err(|_| "SUPERCI_CACHE_PLANE is not set")?;
    let ip = private_ip()?;
    // The port the tools are told of (see the machine's start-up script), else any.
    let port: u16 = std::env::var("SUPERCI_CACHE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(PORT);
    let listener = std::net::TcpListener::bind((ip, port)).or_else(|_| std::net::TcpListener::bind((ip, 0))).map_err(|e| e.to_string())?;
    // (A socket just bound has its address.)
    let port = listener.local_addr().expect("a bound socket's address").port();
    let host = if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() };
    let base = format!("http://{host}:{port}");
    let github = std::env::var("SUPERCI_CACHE_GITHUB").unwrap_or_else(|_| GITHUB.to_string());
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_connect(Some(Duration::from_secs(10))).timeout_global(Some(Duration::from_secs(900)))
        .max_idle_connections(256).max_idle_connections_per_host(64).build().into();
    let workers = std::thread::available_parallelism().map(|n| n.get() * 2).unwrap_or(8).clamp(8, 32);
    let state = Arc::new(State { plane, base: base.clone(), github, agent, workers, uploads: Default::default(), stored: Condvar::new(), downloads: Default::default(), elsewhere: Default::default(), ids: AtomicU64::new(1) });
    let (at, next) = (externals.join(ADDRESS), externals.join(format!(".{ADDRESS}")));
    std::fs::write(&next, &base).and_then(|_| std::fs::rename(&next, &at)).map_err(|e| format!("{}: {e}", at.display()))?;
    eprintln!("serving GitHub's cache at {host}:{port}");
    http::serve(listener, move |req, body| state.handle(req, body))
}

/// An entry being saved (a cache entry, or a run's artifact: its name is its key): the job's token, its key and
/// version, links to upload pieces not used yet (asked for many at a time), and the pieces uploaded so far (by GitHub's
/// client's block id), then their order.
/// Blocks are taken at once and put in the bucket beside, so many at a time (`pending`; what failed, `failed`).
/// Kept once finished (`done`: nothing more is taken for it), so that a request on its way still finds it.
struct Upload { token: String, key: String, version: String, artifact: bool, links: std::collections::VecDeque<(String, String)>, next: u32, blocks: HashMap<String, (String, u64)>, order: Option<Vec<(String, u64)>>, began: Instant, pending: usize, failed: Option<String>, done: bool }

impl Upload {
    fn new(token: String, key: String, version: String, artifact: bool) -> Upload { Upload { token, key, version, artifact, links: Default::default(), next: 0, blocks: HashMap::new(), order: None, began: Instant::now(), pending: 0, failed: None, done: false } }
}

/// What every upload's lookup says: an upload, once begun, is never taken out (see `Upload`).
const KEPT: &str = "an upload is kept once begun";

/// Blocks of an upload being put in the bucket at most at once (a client that sends one small block after another,
/// as BuildKit's does, is answered at once and waits only here).
const BLOCKS_AT_ONCE: usize = 32;

/// Links to upload pieces asked for at a time (GitHub's artifact client uploads 8 MB at a time, hundreds at once).
const LINKS_AT_A_TIME: u32 = 32;

struct State {
    plane: String,
    base: String,
    github: String,
    agent: ureq::Agent,
    workers: usize,
    uploads: Mutex<HashMap<String, Upload>>,
    /// Told when a block of an upload is in the bucket.
    stored: Condvar,
    /// What is given at a download address (a cache hit, an artifact: what it is, in words, and its pieces' links and
    /// sizes), by the id in the address.
    downloads: Mutex<HashMap<String, (String, Vec<(String, u64)>)>>,
    /// Saves that went to GitHub's own cache (the control plane could not be reached): key and version.
    elsewhere: Mutex<HashSet<(String, String)>>,
    ids: AtomicU64,
}

fn json_reply(v: Value) -> Response { Response::bytes(200, "application/json", v.to_string().into_bytes()) }
fn empty(status: u16) -> Response { Response::bytes(status, "text/plain", vec![]) }
fn error(status: u16, body: Value) -> Response { Response::bytes(status, "application/json", body.to_string().into_bytes()) }

/// A time (milliseconds since 1970) as RFC 3339, in UTC.
fn rfc3339(ms: u64) -> String {
    let (days, rem) = ((ms / 86_400_000) as i64, (ms / 1000) % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    format!("{:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", yoe + era * 400 + i64::from(m <= 2), rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A line for finding a fault, when `SUPERCI_CACHE_DEBUG` is set.
fn debug(line: &str) { if std::env::var_os("SUPERCI_CACHE_DEBUG").is_some() { eprintln!("{line}") } }

fn mb(n: u64) -> String { format!("{:.0} MB", n as f64 / 1e6) }

impl State {
    fn id(&self) -> String { format!("{}{}", self.ids.fetch_add(1, Ordering::Relaxed), random_hex(6)) }

    fn handle(self: &Arc<Self>, req: Request, body: &mut dyn Read) -> Response {
        let url = req.target.trim_start_matches('/').to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        if let Some(id) = path.strip_prefix("superci/d/") { return self.download(&req, id) }
        // The tools' own remote caches.
        if path == "v8/artifacts" || path.starts_with("v8/artifacts/") { return self.turbo(&req, path, query).unwrap_or_else(|e| { eprintln!("turbo: {e}"); empty(500) }) }
        if let Some(hash) = path.strip_prefix("v1/cache/") { return self.nx(&req, hash) }
        if let Some(rest) = path.strip_prefix("bazel/") { return self.bazel(&req, rest, body) }
        // GitLab's runner's cache: "S3" here, path-style, in the bucket `superci` (see the machine's start-up script).
        // (Its keys are under the cache path it is given, `gl`: the program's own addresses are `superci/u/`, `superci/d/`.)
        if path.starts_with("superci/gl/") { return self.gitlab(&req, &path["superci/".len()..], body) }
        let blob = path.strip_prefix("superci/u/");
        let reply = match blob { Some(id) => self.upload(&req, id, query), None => self.results(&req, &url) };
        let reply = reply.unwrap_or_else(|e| { eprintln!("{e}"); Response::bytes(500, "text/plain", e.into_bytes()) });
        if blob.is_some() { reply.with_headers(azure_headers()) } else { reply }
    }

    /// A call to the control plane's cache (see `superci_core::plane`'s `cache_route`): its status and answer.
    fn plane(&self, what: &str, body: Value) -> Result<(u16, Value), String> {
        // (Asked three times, waiting a little longer each time.)
        let mut attempt = 0;
        loop {
            let last = match self.agent.post(&format!("{}/{what}", self.plane)).header("content-type", "application/json").send(body.to_string()) {
                Ok(r) if r.status().as_u16() >= 500 => format!("the control plane said {}", r.status()),
                Ok(r) => {
                    let status = r.status().as_u16();
                    let text = r.into_body().read_to_string().unwrap_or_default();
                    return Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
                }
                Err(e) => e.to_string(),
            };
            attempt += 1;
            if attempt == 3 { return Err(last) }
            std::thread::sleep(Duration::from_millis(300 * attempt));
        }
    }

    /// GitHub's results service, as the job's actions call it: the cache's calls here, the rest on to GitHub's (for a
    /// job of GitHub's; a run from a computer's job has a token of its control plane's, and only the cache here).
    fn results(&self, req: &Request, rest: &str) -> Result<Response, String> {
        let body = &req.body;
        let token = req.header("authorization").map(|v| v.trim_start_matches("Bearer ").to_string()).unwrap_or_default();
        let github = githubs(&token);
        let original = self.github.clone();
        let asked: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let (key, version) = (asked["key"].as_str().unwrap_or_default().to_string(), asked["version"].as_str().unwrap_or_default().to_string());
        let on = || if github { self.forward(req, &original, rest) } else { Ok(error(501, json!({ "code": "unimplemented", "msg": "not on SuperCI's machines yet" }))) };
        match rest.strip_prefix(SERVICE) {
            Some("GetCacheEntryDownloadURL") => {
                let restore = asked["restore_keys"].clone();
                match self.plane("lookup", json!({ "token": token, "key": key, "restore_keys": restore, "version": version })) {
                    Ok((200, hit)) if hit["hit"] == true => {
                        let pieces: Vec<(String, u64)> = hit["pieces"].as_array().into_iter().flatten().filter_map(|p| Some((p["url"].as_str()?.to_string(), p["size"].as_u64()?))).collect();
                        let matched = hit["key"].as_str().unwrap_or(&key).to_string();
                        let id = self.id();
                        self.downloads.lock().unwrap().insert(id.clone(), (format!("restore {matched}"), pieces));
                        eprintln!("restore {key}: found {matched} ({})", mb(hit["size"].as_u64().unwrap_or(0)));
                        Ok(json_reply(json!({ "ok": true, "signed_download_url": format!("{}/superci/d/{id}", self.base), "matched_key": matched })))
                    }
                    Ok((200, _)) if github => { eprintln!("restore {key}: not here, asked of GitHub"); on() }
                    Ok((200, _)) => { eprintln!("restore {key}: not found"); Ok(json_reply(json!({ "ok": false }))) }
                    Ok((status, said)) => { eprintln!("restore {key}: refused ({status} {})", said["why"].as_str().unwrap_or_default()); if github { on() } else { Ok(json_reply(json!({ "ok": false }))) } }
                    Err(e) => { eprintln!("restore {key}: the control plane could not be reached ({e})"); if github { on() } else { Ok(json_reply(json!({ "ok": false }))) } }
                }
            }
            Some("CreateCacheEntry") => match self.plane("begin", json!({ "token": token, "key": key, "version": version })) {
                Ok((200, said)) if said["ok"] == true => {
                    let id = self.id();
                    self.uploads.lock().unwrap().insert(id.clone(), Upload::new(token, key.clone(), version, false));
                    eprintln!("save {key}: begun");
                    Ok(json_reply(json!({ "ok": true, "signed_upload_url": format!("{}/superci/u/{id}", self.base) })))
                }
                // Refused for what the control plane cannot take (a key, a moment's trouble): a job of GitHub's saves
                // it in GitHub's own cache, where a lookup not found here goes too.
                // (A moment's trouble that lasts, 5xx, comes as an error: see `plane`.)
                Ok((400, said)) if github => {
                    eprintln!("save {key}: not taken here (400 {}); saved in GitHub's cache", said["why"].as_str().unwrap_or_default());
                    self.elsewhere.lock().unwrap().insert((key, version));
                    on()
                }
                // There already (or being saved): as GitHub's own service says it (BuildKit takes that as done).
                Ok((409, _)) => {
                    eprintln!("save {key}: there already");
                    Ok(error(409, json!({ "code": "already_exists", "msg": "cache entry with the same key, version, and scope already exists" })))
                }
                Ok((status, said)) => {
                    eprintln!("save {key}: not saved ({status} {})", said["why"].as_str().unwrap_or_default());
                    Ok(json_reply(json!({ "ok": false, "message": format!("SuperCI's cache: {}", said["why"].as_str().unwrap_or("refused")) })))
                }
                Err(e) if github => { eprintln!("save {key}: the control plane could not be reached ({e}); saved in GitHub's cache"); self.elsewhere.lock().unwrap().insert((key, version)); on() }
                Err(e) => { eprintln!("save {key}: the control plane could not be reached ({e})"); Ok(json_reply(json!({ "ok": false }))) }
            },
            Some("FinalizeCacheEntryUpload") => {
                if self.elsewhere.lock().unwrap().contains(&(key.clone(), version.clone())) { return on() }
                let found = { let up = self.uploads.lock().unwrap(); up.iter().find(|(_, u)| !u.artifact && !u.done && u.key == key && u.version == version && u.order.is_some()).map(|(id, u)| (id.clone(), u.token.clone(), u.order.clone().unwrap_or_default(), u.began)) };
                let Some((id, token, pieces, began)) = found else { eprintln!("save {key}: finished before its upload"); return Ok(json_reply(json!({ "ok": false }))) };
                let size: u64 = pieces.iter().map(|(_, s)| s).sum();
                if asked["size_bytes"].as_str().and_then(|s| s.parse::<u64>().ok()).or(asked["size_bytes"].as_u64()).is_some_and(|said| said != size) {
                    eprintln!("save {key}: {} uploaded, {} said", mb(size), asked["size_bytes"]);
                    return Ok(json_reply(json!({ "ok": false })))
                }
                let pieces: Vec<Value> = pieces.iter().map(|(n, s)| json!([n, s])).collect();
                match self.plane("finish", json!({ "token": token, "key": key, "version": version, "pieces": pieces })) {
                    Ok((200, said)) if said["ok"] == true => {
                        self.uploads.lock().unwrap().get_mut(&id).expect(KEPT).done = true;
                        let secs = began.elapsed().as_secs_f64();
                        eprintln!("save {key}: saved, {} in {secs:.1} s ({:.0} MB/s)", mb(size), size as f64 / 1e6 / secs.max(0.001));
                        Ok(json_reply(json!({ "ok": true, "entry_id": "1" })))
                    }
                    other => { eprintln!("save {key}: not finished ({other:?})"); Ok(json_reply(json!({ "ok": false }))) }
                }
            }
            _ => match rest.strip_prefix(ARTIFACTS) {
                Some(what) if !github => self.artifacts(what, &token, &asked),
                _ => on(),
            },
        }
    }

    /// A run from a computer's artifacts (GitHub's artifact service, as `upload-artifact` and `download-artifact` call
    /// it), kept by its control plane in the bucket; a job of GitHub's keeps its own at GitHub.
    fn artifacts(&self, what: &str, token: &str, asked: &Value) -> Result<Response, String> {
        // (A wrapped value, `{"value": …}`, or the value itself: protobuf's JSON allows both.)
        let field = |k: &str| asked[k]["value"].as_str().or(asked[k].as_str()).map(str::to_string);
        let name = field("name").unwrap_or_default();
        let twirp = |status: u16, code: &str, msg: &str| Ok(error(status, json!({ "code": code, "msg": msg })));
        let said = |r: Result<(u16, Value), String>| -> Result<Value, (u16, String)> {
            match r { Ok((200, v)) => Ok(v), Ok((s, v)) => Err((s, v["why"].as_str().unwrap_or("refused").to_string())), Err(e) => Err((503, format!("the control plane could not be reached ({e})"))) }
        };
        match what {
            "CreateArtifact" => match said(self.plane("artifact-begin", json!({ "token": token, "name": name }))) {
                Ok(_) => {
                    let id = self.id();
                    self.uploads.lock().unwrap().insert(id.clone(), Upload::new(token.to_string(), name.clone(), String::new(), true));
                    eprintln!("artifact {name}: begun");
                    Ok(json_reply(json!({ "ok": true, "signed_upload_url": format!("{}/superci/u/{id}", self.base) })))
                }
                Err((409, why)) => twirp(409, "already_exists", &why),
                Err((s, why)) => twirp(if s == 503 { 503 } else { 400 }, "failed_precondition", &why),
            },
            "FinalizeArtifact" => {
                let found = { let up = self.uploads.lock().unwrap(); up.iter().find(|(_, u)| u.artifact && !u.done && u.key == name && u.order.is_some()).map(|(id, u)| (id.clone(), u.order.clone().unwrap_or_default(), u.began)) };
                let Some((id, pieces, began)) = found else { return twirp(400, "failed_precondition", "finished before its upload") };
                let size: u64 = pieces.iter().map(|(_, s)| s).sum();
                let pieces: Vec<Value> = pieces.iter().map(|(n, s)| json!([n, s])).collect();
                match said(self.plane("artifact-finish", json!({ "token": token, "name": name, "pieces": pieces }))) {
                    Ok(v) => {
                        self.uploads.lock().unwrap().get_mut(&id).expect(KEPT).done = true;
                        eprintln!("artifact {name}: kept, {} in {:.1} s", mb(size), began.elapsed().as_secs_f64());
                        Ok(json_reply(json!({ "ok": true, "artifact_id": v["id"].as_u64().unwrap_or(1).to_string() })))
                    }
                    Err((s, why)) => twirp(s.max(400), "internal", &why),
                }
            }
            "ListArtifacts" => match said(self.plane("artifacts", json!({ "token": token }))) {
                Ok(v) => {
                    let (want_name, want_id) = (field("name_filter"), field("id_filter"));
                    let list: Vec<Value> = v["artifacts"].as_array().into_iter().flatten()
                        .filter(|a| want_name.as_deref().is_none_or(|n| a["name"] == n) && want_id.as_deref().is_none_or(|i| a["id"].as_u64().map(|x| x.to_string()).as_deref() == Some(i)))
                        .map(|a| json!({ "workflow_run_backend_id": asked["workflow_run_backend_id"], "workflow_job_run_backend_id": asked["workflow_job_run_backend_id"],
                            "database_id": a["id"].as_u64().unwrap_or(0).to_string(), "name": a["name"], "size": a["size"].as_u64().unwrap_or(0).to_string(), "created_at": rfc3339(a["made"].as_u64().unwrap_or(0)) }))
                        .collect();
                    Ok(json_reply(json!({ "artifacts": list })))
                }
                Err((s, why)) => twirp(s.max(400), "internal", &why),
            },
            "GetSignedArtifactURL" => match said(self.plane("artifact-get", json!({ "token": token, "name": name }))) {
                Ok(v) => {
                    let pieces: Vec<(String, u64)> = v["pieces"].as_array().into_iter().flatten().filter_map(|p| Some((p["url"].as_str()?.to_string(), p["size"].as_u64()?))).collect();
                    let id = self.id();
                    self.downloads.lock().unwrap().insert(id.clone(), (format!("artifact {name}"), pieces));
                    Ok(json_reply(json!({ "signed_url": format!("{}/superci/d/{id}", self.base) })))
                }
                Err((404, why)) => twirp(404, "not_found", &why),
                Err((s, why)) => twirp(s.max(400), "internal", &why),
            },
            "DeleteArtifact" => match said(self.plane("artifact-delete", json!({ "token": token, "name": name }))) {
                Ok(v) => Ok(json_reply(json!({ "ok": true, "artifact_id": v["id"].as_u64().unwrap_or(0).to_string() }))),
                Err((404, why)) => twirp(404, "not_found", &why),
                Err((s, why)) => twirp(s.max(400), "internal", &why),
            },
            _ => twirp(501, "unimplemented", "not on SuperCI's machines yet"),
        }
    }

    /// A tool's artifact by its hash: its pieces' links, its size and what was kept beside it; none when not kept
    /// (or the control plane cannot be reached: a tool then does without).
    fn tool_get(&self, tool: &str, hash: &str) -> Option<(Vec<(String, u64)>, u64, Value)> {
        let (status, hit) = self.plane("tool-get", json!({ "tool": tool, "hash": hash })).ok()?;
        if status != 200 || hit["hit"] != true { return None }
        let pieces = hit["pieces"].as_array()?.iter().filter_map(|p| Some((p["url"].as_str()?.to_string(), p["size"].as_u64()?))).collect();
        let meta = hit["meta"].as_str().and_then(|m| serde_json::from_str(m).ok()).unwrap_or(Value::Null);
        Some((pieces, hit["size"].as_u64().unwrap_or(0), meta))
    }

    /// Keeps a tool's artifact under its hash: whether it is kept now (true), or was already (false).
    fn tool_put(&self, tool: &str, hash: &str, body: &[u8], meta: Option<Value>) -> Result<bool, String> {
        self.tool_put_from(tool, hash, &mut &body[..], body.len() as u64, meta)
    }

    /// The same from a reader of `length` bytes (a large one in pieces of `PIECE`, each to the bucket as it comes).
    fn tool_put_from(&self, tool: &str, hash: &str, body: &mut dyn Read, length: u64, meta: Option<Value>) -> Result<bool, String> {
        let (status, said) = self.plane("tool-begin", json!({ "tool": tool, "hash": hash }))?;
        if status == 409 { return Ok(false) }
        let mut links: std::collections::VecDeque<(String, String)> = Default::default();
        links.push_back(("p00000".into(), said["url"].as_str().filter(|_| status == 200).ok_or_else(|| format!("{tool} {hash}: not taken ({status} {})", said["why"].as_str().unwrap_or_default()))?.to_string()));
        let (mut pieces, mut left, mut next) = (vec![], length, 1u32);
        loop {
            let n = left.min(PIECE);
            let mut chunk = vec![0u8; n as usize];
            body.read_exact(&mut chunk).map_err(|e| format!("{tool} {hash}: {e}"))?;
            if links.is_empty() {
                let names: Vec<String> = (next..next + 32).map(|i| format!("p{i:05}")).collect();
                next += 32;
                let (status, said) = self.plane("tool-sign", json!({ "tool": tool, "hash": hash, "names": names }))?;
                let urls: Vec<String> = said["urls"].as_array().into_iter().flatten().filter_map(|u| u.as_str().map(str::to_string)).collect();
                if status != 200 || urls.len() != names.len() { return Err(format!("{tool} {hash}: no links ({status})")) }
                links.extend(names.into_iter().zip(urls));
            }
            let (name, link) = links.pop_front().unwrap_or_default();
            self.put(&link, &chunk)?;
            pieces.push(json!([name, n]));
            left -= n;
            if left == 0 { break }
        }
        let (status, said) = self.plane("tool-finish", json!({ "tool": tool, "hash": hash, "pieces": pieces, "meta": meta.map(|m| m.to_string()) }))?;
        if status != 200 { return Err(format!("{tool} {hash}: not finished ({status} {})", said["why"].as_str().unwrap_or_default())) }
        Ok(true)
    }

    /// A tool's artifact given back (or only its size, to a `HEAD`), with what was kept beside it as headers.
    fn tool_answer(&self, req: &Request, pieces: Vec<(String, u64)>, size: u64, headers: Vec<(String, String)>) -> Response {
        let mut headers = headers;
        headers.push(("content-type".into(), "application/octet-stream".into()));
        if req.method == "HEAD" { return Response::stream(200, Box::new(std::io::empty()), size).with_headers(headers) }
        Response::stream(200, Box::new(Parallel::start(self.agent.clone(), parts(&pieces, 0, size), self.workers)), size).with_headers(headers)
    }

    /// Turbo's remote cache (Vercel's `/v8/artifacts` API), with the token the machine gives it; with any other token
    /// (a workflow's own, for Vercel), on to Vercel as it came.
    fn turbo(&self, req: &Request, path: &str, query: &str) -> Result<Response, String> {
        if req.header("authorization").map(|a| a.trim_start_matches("Bearer ").trim()) != Some(TOOL_TOKEN) {
            let vercel = std::env::var("SUPERCI_CACHE_VERCEL").unwrap_or(VERCEL.to_string());
            return self.forward(req, &vercel, &if query.is_empty() { path.to_string() } else { format!("{path}?{query}") })
        }
        let hash = path.strip_prefix("v8/artifacts/").unwrap_or_default();
        Ok(match (req.method.as_str(), hash) {
            ("GET", "status") => json_reply(json!({ "status": "enabled" })),
            ("POST", "events") => json_reply(json!({})),
            // Which of these hashes are kept, and how large each is.
            ("POST", "") => {
                let asked: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let mut out = serde_json::Map::new();
                for h in asked["hashes"].as_array().into_iter().flatten().filter_map(|h| h.as_str()).take(100) {
                    out.insert(h.to_string(), match self.tool_get("turbo", h) { Some((_, size, meta)) => json!({ "size": size, "taskDurationMs": meta["duration"].as_str().and_then(|d| d.parse::<u64>().ok()).unwrap_or(0), "tag": meta["tag"] }), None => Value::Null });
                }
                json_reply(Value::Object(out))
            }
            ("GET" | "HEAD", h) if !h.is_empty() => match self.tool_get("turbo", h) {
                Some((pieces, size, meta)) => {
                    let mut headers = vec![];
                    if let Some(d) = meta["duration"].as_str() { headers.push(("x-artifact-duration".to_string(), d.to_string())) }
                    if let Some(t) = meta["tag"].as_str() { headers.push(("x-artifact-tag".to_string(), t.to_string())) }
                    self.tool_answer(req, pieces, size, headers)
                }
                None => empty(404),
            },
            ("PUT", h) if !h.is_empty() => {
                let mut meta = serde_json::Map::new();
                if let Some(d) = req.header("x-artifact-duration") { meta.insert("duration".into(), d.into()); }
                if let Some(t) = req.header("x-artifact-tag") { meta.insert("tag".into(), t.into()); }
                self.tool_put("turbo", h, &req.body, Some(Value::Object(meta)))?;
                Response::bytes(202, "application/json", json!({ "urls": [] }).to_string().into_bytes())
            }
            _ => empty(404),
        })
    }

    /// Nx's self-hosted remote cache (`/v1/cache/{hash}`: a task's outputs as a tar).
    fn nx(&self, req: &Request, hash: &str) -> Response {
        match req.method.as_str() {
            "GET" | "HEAD" => match self.tool_get("nx", hash) { Some((pieces, size, _)) => self.tool_answer(req, pieces, size, vec![]), None => empty(404) },
            "PUT" => match self.tool_put("nx", hash, &req.body, None) {
                Ok(true) => empty(200),
                Ok(false) => Response::bytes(409, "text/plain", b"already kept".to_vec()),
                Err(e) => { eprintln!("nx: {e}"); Response::bytes(500, "text/plain", e.into_bytes()) }
            },
            _ => empty(405),
        }
    }

    /// GitLab's runner's cache, as S3 (it signs links to it with keys that are no secret; what a job may read and
    /// write is the control plane's to say): `GET`, `HEAD` and `PUT` of an object, its key all after the bucket's name.
    fn gitlab(&self, req: &Request, key: &str, body: &mut dyn Read) -> Response {
        let key = unpercent(key);
        match req.method.as_str() {
            "GET" | "HEAD" => match self.tool_get("gitlab", &key) {
                Some((pieces, size, _)) => self.tool_answer(req, pieces, size, vec![("etag".into(), format!("\"{}\"", size)), ("last-modified".into(), http_date(0))]),
                None => Response::bytes(404, "application/xml", b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>".to_vec()),
            },
            "PUT" => {
                let done = if req.streamed { self.tool_put_from("gitlab", &key, body, req.length, None) } else { self.tool_put("gitlab", &key, &req.body, None) };
                match done {
                    Ok(_) => { eprintln!("gitlab {key}: kept, {}", mb(req.length)); empty(200).with_headers(vec![("etag".into(), "\"superci\"".into())]) }
                    Err(e) => { eprintln!("gitlab: {e}"); empty(500) }
                }
            }
            _ => empty(405),
        }
    }

    /// Bazel's HTTP remote cache: `ac/{hash}` (what an action made) and `cas/{hash}` (content by its hash), after any
    /// instance name.
    fn bazel(&self, req: &Request, rest: &str, body: &mut dyn Read) -> Response {
        let mut parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        let (Some(hash), Some(kind)) = (parts.pop(), parts.pop()) else { return empty(404) };
        let tool = match kind { "ac" => "bazelac", "cas" => "bazelcas", _ => return empty(404) };
        match req.method.as_str() {
            "GET" | "HEAD" => match self.tool_get(tool, hash) { Some((pieces, size, _)) => self.tool_answer(req, pieces, size, vec![]), None => empty(404) },
            "PUT" => match if req.streamed { self.tool_put_from(tool, hash, body, req.length, None) } else { self.tool_put(tool, hash, &req.body, None) } { Ok(_) => empty(200), Err(e) => { eprintln!("bazel: {e}"); empty(500) } },
            _ => empty(405),
        }
    }

    /// A call that is not the cache's, sent on to where the job's runner meant it to go, as it came.
    fn forward(&self, req: &Request, original: &str, rest: &str) -> Result<Response, String> {
        let url = format!("{}/{rest}", original.trim_end_matches('/'));
        let mut out = ureq::http::Request::builder().method(req.method.as_str()).uri(&url);
        for (name, value) in &req.headers {
            if ["host", "content-length", "connection", "transfer-encoding", "accept-encoding", "expect"].contains(&name.as_str()) { continue }
            out = out.header(name.as_str(), value.as_str());
        }
        let r = self.agent.run(out.body(req.body.clone()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let kind = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        let body = r.into_body().with_config().limit(64 << 20).read_to_vec().map_err(|e| e.to_string())?;
        Ok(Response::bytes(status, &kind, body))
    }

    /// GitHub's client uploading an entry as to Azure's blob storage: one piece (`PUT`), or blocks
    /// (`?comp=block&blockid=…`) and then their order (`?comp=blocklist`). Each piece goes to the bucket as it comes.
    fn upload(self: &Arc<Self>, req: &Request, id: &str, query: &str) -> Result<Response, String> {
        if req.method != "PUT" { return Ok(empty(405)) }
        let q: HashMap<String, String> = query.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (unpercent(k), unpercent(v))).collect();
        if !self.uploads.lock().unwrap().get(id).is_some_and(|u| !u.done) { return Ok(empty(404)) }
        let body = req.body.clone();
        match q.get("comp").map(String::as_str) {
            Some("blocklist") => {
                let text = String::from_utf8_lossy(&body);
                let ids: Vec<String> = text.split('<').filter_map(|t| { let (tag, v) = t.split_once('>')?; ["Latest", "Uncommitted", "Committed"].contains(&tag).then(|| v.trim().to_string()) }).collect();
                // Once every block is in the bucket.
                let mut ups = self.uploads.lock().unwrap();
                while ups.get(id).is_some_and(|u| u.pending > 0) { ups = self.stored.wait(ups).unwrap() }
                let up = ups.get_mut(id).expect(KEPT);
                if let Some(e) = up.failed.clone() { return Err(format!("a block was not stored: {e}")) }
                let order: Option<Vec<(String, u64)>> = ids.iter().map(|b| up.blocks.get(b).cloned()).collect();
                let Some(order) = order.filter(|o| !o.is_empty()) else { return Ok(empty(400)) };
                up.order = Some(order);
                Ok(empty(201))
            }
            comp => {
                let block = q.get("blockid").cloned().unwrap_or_default();
                if comp == Some("block") && block.is_empty() { return Ok(empty(400)) }
                debug(&format!("block {block} of {id}: {} bytes taken", body.len()));
                let (name, link) = self.link(id)?;
                debug(&format!("block {block} of {id}: link for {name}"));
                let size = body.len() as u64;
                // One piece, the whole entry: stored before the answer.
                if comp != Some("block") {
                    self.put(&link, &body)?;
                    self.uploads.lock().unwrap().get_mut(id).expect(KEPT).order = Some(vec![(name, size)]);
                    return Ok(empty(201))
                }
                // A block: taken now, stored beside (at most `BLOCKS_AT_ONCE` of the upload's at a time).
                {
                    let mut ups = self.uploads.lock().unwrap();
                    while ups.get(id).is_some_and(|u| u.pending >= BLOCKS_AT_ONCE) { ups = self.stored.wait(ups).unwrap() }
                    let up = ups.get_mut(id).expect(KEPT);
                    up.blocks.insert(block, (name, size));
                    up.pending += 1;
                }
                let (me, id) = (self.clone(), id.to_string());
                std::thread::spawn(move || {
                    let stored = me.put(&link, &body);
                    debug(&format!("{id}: a block stored ({stored:?})"));
                    let mut ups = me.uploads.lock().unwrap();
                    let up = ups.get_mut(&id).expect(KEPT);
                    up.pending -= 1;
                    if let Err(e) = stored { up.failed.get_or_insert(e); }
                    drop(ups);
                    me.stored.notify_all();
                });
                Ok(empty(201))
            }
        }
    }

    /// A link to upload the next piece of an upload, and the piece's name: one asked for before and not used yet, else
    /// `LINKS_AT_A_TIME` more.
    fn link(&self, id: &str) -> Result<(String, String), String> {
        let (op, body, names) = {
            let mut ups = self.uploads.lock().unwrap();
            let up = ups.get_mut(id).expect(KEPT);
            if let Some(l) = up.links.pop_front() { return Ok(l) }
            let names: Vec<String> = (up.next..up.next + LINKS_AT_A_TIME).map(|n| format!("p{n:05}")).collect();
            up.next += LINKS_AT_A_TIME;
            if up.artifact { ("artifact-sign", json!({ "token": up.token, "name": up.key, "names": names }), names) } else { ("sign", json!({ "token": up.token, "key": up.key, "version": up.version, "names": names }), names) }
        };
        let (status, said) = self.plane(op, body)?;
        let urls: Vec<String> = said["urls"].as_array().into_iter().flatten().filter_map(|u| u.as_str().map(str::to_string)).collect();
        if status != 200 || urls.len() != names.len() { return Err(format!("no links to upload ({status} {})", said["why"].as_str().unwrap_or_default())) }
        let mut links: Vec<(String, String)> = names.into_iter().zip(urls).collect();
        let first = links.remove(0);
        self.uploads.lock().unwrap().get_mut(id).expect(KEPT).links.extend(links);
        Ok(first)
    }

    /// (Tried four times, waiting a little longer each time.)
    fn put(&self, link: &str, body: &[u8]) -> Result<(), String> {
        let mut attempt = 0;
        loop {
            debug(&format!("put {} bytes to {}", body.len(), link.split('?').next().unwrap_or_default()));
            let last = match self.agent.put(&stand_in(link)).send(body) {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => format!("the bucket said {}: {}", r.status(), r.into_body().read_to_string().unwrap_or_default().chars().take(200).collect::<String>()),
                Err(e) => e.to_string(),
            };
            attempt += 1;
            if attempt == 4 { return Err(last) }
            std::thread::sleep(Duration::from_millis(500 * attempt));
        }
    }

    /// A hit, as one stream (GitHub's client reads one from an address that is not Azure's), or the part of it a
    /// range asks for (BuildKit's reads from where it needs), fetched from the bucket many parts at a time and given on
    /// in order.
    fn download(&self, req: &Request, id: &str) -> Response {
        let Some((what, pieces)) = self.downloads.lock().unwrap().get(id).cloned() else { return empty(404) };
        let size: u64 = pieces.iter().map(|(_, s)| s).sum();
        let mut headers = azure_headers();
        // (An artifact is the zip `upload-artifact` made, said as GitHub's storage says it: `download-artifact` unpacks
        // only what is said to be a zip, and keeps anything else as the file it is.)
        let kind = if what.starts_with("artifact ") { "application/zip" } else { "application/octet-stream" };
        headers.extend([("content-type", kind), ("accept-ranges", "bytes"), ("x-ms-blob-type", "BlockBlob")].iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let (status, first, len) = match req.header("x-ms-range").or(req.header("range")).map(|r| range(r, size)) {
            None => (200, 0, size),
            Some(Some((a, b))) => { headers.push(("content-range".into(), format!("bytes {a}-{b}/{size}"))); (206, a, b - a + 1) }
            Some(None) => { headers.push(("content-range".into(), format!("bytes */{size}"))); return empty(416).with_headers(headers) }
        };
        if req.method == "HEAD" { return Response::stream(status, Box::new(std::io::empty()), len).with_headers(headers) }
        let reader = Parallel::start(self.agent.clone(), parts(&pieces, first, len), self.workers);
        Response::stream(status, Box::new(Said { inner: reader, what, len, given: 0, began: Instant::now(), first }), len).with_headers(headers)
    }
}

/// The parts to fetch for `len` bytes from `first` on, of pieces (links and sizes) one after another: each a range
/// of one piece, `CHUNK` at most.
fn parts(pieces: &[(String, u64)], first: u64, len: u64) -> Vec<(String, u64, u64)> {
    let (mut out, mut start, end) = (vec![], 0u64, first + len);
    for (link, size) in pieces {
        let (from, to) = (first.max(start), end.min(start + size));
        let mut at = from;
        while at < to { let n = CHUNK.min(to - at); out.push((link.clone(), at - start, n)); at += n }
        start += size;
    }
    out
}

/// Whether a job's token is GitHub's (RS256, from GitHub's token service; a run from a computer's is its control
/// plane's, ES256). Only which: the control plane checks the signature.
fn githubs(token: &str) -> bool {
    let header = token.split('.').next().unwrap_or_default();
    let decoded = base64url(header);
    serde_json::from_slice::<Value>(&decoded).is_ok_and(|h| h["alg"] == "RS256")
}

fn base64url(s: &str) -> Vec<u8> {
    let value = |c: u8| match c { b'A'..=b'Z' => Some(c - b'A'), b'a'..=b'z' => Some(c - b'a' + 26), b'0'..=b'9' => Some(c - b'0' + 52), b'-' | b'+' => Some(62), b'_' | b'/' => Some(63), _ => None };
    let (mut out, mut bits, mut n) = (vec![], 0u32, 0);
    for v in s.bytes().filter_map(value) { bits = bits << 6 | v as u32; n += 6; if n >= 8 { n -= 8; out.push((bits >> n) as u8) } }
    out
}

/// A download that says, once it is over, how it went (a whole one, or a large part).
struct Said<R: Read> { inner: R, what: String, len: u64, given: u64, began: Instant, first: u64 }

impl<R: Read> Read for Said<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { let n = self.inner.read(buf)?; self.given += n as u64; Ok(n) }
}

impl<R: Read> Drop for Said<R> {
    fn drop(&mut self) {
        if self.first != 0 && self.len <= 64 << 20 { return }
        let secs = self.began.elapsed().as_secs_f64();
        eprintln!("{}: {} {} in {secs:.1} s ({:.0} MB/s)", self.what, if self.given == self.len { "given" } else { "broken off after" }, mb(self.given), self.given as f64 / 1e6 / secs.max(0.001));
    }
}

/// What Azure's blob storage says with every answer, which Azure's clients read (BuildKit's takes the request's id
/// as given, and stops without one).
fn azure_headers() -> Vec<(String, String)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let id = random_hex(16);
    let id = format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]);
    [("x-ms-request-id", id.as_str()), ("x-ms-version", "2023-11-03"), ("date", &http_date(now)), ("last-modified", &http_date(now)), ("etag", &format!("\"0x{:X}\"", now)), ("x-ms-request-server-encrypted", "true")]
        .iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// A time (seconds since 1970) as HTTP writes it: `Tue, 14 Oct 2026 11:31:18 GMT`.
fn http_date(secs: u64) -> String {
    let iso = rfc3339(secs * 1000);
    let (y, m, d) = (&iso[0..4], iso[5..7].parse::<usize>().unwrap_or(1), &iso[8..10]);
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{}, {d} {} {y} {} GMT", DAYS[(secs / 86_400 % 7) as usize], MONTHS[m - 1], &iso[11..19])
}

/// A `Range` (or Azure's `x-ms-range`) of a download of `size` bytes: its first and last byte.
fn range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
    let a: u64 = a.trim().parse().ok()?;
    let b: u64 = if b.trim().is_empty() { size.checked_sub(1)? } else { b.trim().parse::<u64>().ok()?.min(size.checked_sub(1)?) };
    (a <= b).then_some((a, b))
}

/// Parts of a download fetched by `workers` threads at once, read in order; at most twice as many parts held as
/// there are workers (a slow reader holds the fetching back).
struct Parallel { shared: Arc<(Mutex<Fetched>, Condvar)>, current: Cursor<Vec<u8>>, total: usize }

struct Fetched { next_read: usize, next_fetch: usize, done: HashMap<usize, Result<Vec<u8>, String>>, stop: bool }

impl Parallel {
    fn start(agent: ureq::Agent, parts: Vec<(String, u64, u64)>, workers: usize) -> Parallel {
        let total = parts.len();
        let shared = Arc::new((Mutex::new(Fetched { next_read: 0, next_fetch: 0, done: HashMap::new(), stop: false }), Condvar::new()));
        let parts = Arc::new(parts);
        (0..workers.min(total.max(1))).for_each(|_| {
            let (shared, parts, agent) = (shared.clone(), parts.clone(), agent.clone());
            std::thread::spawn(move || loop {
                let i = {
                    let (lock, wake) = &*shared;
                    let mut f = lock.lock().unwrap();
                    while !f.stop && f.next_fetch < parts.len() && f.next_fetch >= f.next_read + workers * 2 { f = wake.wait(f).unwrap() }
                    if f.stop || f.next_fetch >= parts.len() { return }
                    f.next_fetch += 1;
                    f.next_fetch - 1
                };
                let got = fetch(&agent, &parts[i]);
                let (lock, wake) = &*shared;
                lock.lock().unwrap().done.insert(i, got);
                wake.notify_all();
            });
        });
        Parallel { shared, current: Cursor::new(vec![]), total }
    }
}

impl Drop for Parallel {
    fn drop(&mut self) { let (lock, wake) = &*self.shared; lock.lock().unwrap().stop = true; wake.notify_all(); }
}

impl Read for Parallel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // (What was fetched, in memory: reading it does not fail.)
            let n = self.current.read(buf).expect("a part in memory");
            if n > 0 || buf.is_empty() { return Ok(n) }
            let (lock, wake) = &*self.shared;
            let mut f = lock.lock().unwrap();
            if f.next_read >= self.total { return Ok(0) }
            let i = f.next_read;
            while !f.done.contains_key(&i) { f = wake.wait(f).unwrap() }
            let got = f.done.remove(&i).unwrap();
            f.next_read += 1;
            wake.notify_all();
            drop(f);
            self.current = Cursor::new(got.map_err(std::io::Error::other)?);
        }
    }
}

/// A link to the bucket, or (in a debug build with `SUPERCI_STAND_IN`, as the CLI's) the test kit's stand-in for it.
fn stand_in(link: &str) -> String {
    #[cfg(debug_assertions)]
    if let Some(at) = std::env::var("SUPERCI_STAND_IN").ok().filter(|a| !a.is_empty()) {
        if let Some(rest) = link.strip_prefix("https://").filter(|u| u.split('/').next().is_some_and(|h| h.ends_with(".amazonaws.com"))) { return format!("{}/{rest}", at.trim_end_matches('/')) }
    }
    link.to_string()
}

/// One part of a piece, by its range; tried four times.
fn fetch(agent: &ureq::Agent, (link, at, len): &(String, u64, u64)) -> Result<Vec<u8>, String> {
    let mut attempt = 0;
    loop {
        let last = match agent.get(&stand_in(link)).header("range", &format!("bytes={at}-{}", at + len - 1)).call() {
            Ok(r) if r.status().as_u16() == 206 || (r.status().as_u16() == 200 && *at == 0) => {
                let mut body = Vec::with_capacity(*len as usize);
                match r.into_body().into_reader().take(*len).read_to_end(&mut body) {
                    Ok(_) if body.len() as u64 == *len => return Ok(body),
                    Ok(_) => format!("{} bytes of {len}", body.len()),
                    Err(e) => e.to_string(),
                }
            }
            Ok(r) => format!("the bucket said {}", r.status()),
            Err(e) => e.to_string(),
        };
        attempt += 1;
        if attempt == 4 { return Err(last) }
        std::thread::sleep(Duration::from_millis(300 * attempt));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A time as RFC 3339 on each side of every leap rule, as the control plane's calendar says the day.
    #[test]
    fn a_time_is_said_on_its_day_across_every_leap_rule() {
        for days in [0i64, 11_016, 11_017, 47_540, 47_541, 84_005, 84_065, 157_112, 157_113, 157_114, 157_419] {
            let (y, m, d) = superci_core::plane::civil(days);
            assert_eq!(rfc3339(days as u64 * 86_400_000 + 3_661_000), format!("{y:04}-{m:02}-{d:02}T01:01:01Z"), "{days}");
        }
    }

    #[test]
    fn names_and_tokens() {
        assert_eq!(unpercent("AAAA%2FBB%3D%3D"), "AAAA/BB==");
        assert_eq!(rfc3339(1_790_000_000_123), "2026-09-21T14:13:20Z");
        assert_eq!(http_date(1_791_977_478), "Wed, 14 Oct 2026 11:31:18 GMT");
        assert_eq!((range("bytes=0-", 10), range("bytes=4-6", 10), range("bytes=4-99", 10), range("bytes=10-", 10), range("x", 10)), (Some((0, 9)), Some((4, 6)), Some((4, 9)), None, None));
        let pieces = vec![("a".to_string(), CHUNK + 5), ("b".to_string(), 10)];
        assert_eq!(parts(&pieces, 0, CHUNK + 15), vec![("a".to_string(), 0, CHUNK), ("a".to_string(), CHUNK, 5), ("b".to_string(), 0, 10)]);
        assert_eq!(parts(&pieces, CHUNK + 2, 6), vec![("a".to_string(), CHUNK + 2, 3), ("b".to_string(), 0, 3)]);
        // GitHub's (the header of a token GitHub gave a job), and a control plane's.
        assert!(githubs("eyJhbGciOiJSUzI1NiIsImtpZCI6IjM4ODI2YjE3LTZhMzAtNWY5Yi1iMTY5LThiZWI4MjAyZjcyMyIsInR5cCI6IkpXVCIsIng1dCI6InlrTmFZNHFNX3RhNGsyVGdaT0NFWUxrY1lsQSJ9.e30.x"));
        assert!(!githubs("eyJhbGciOiJFUzI1NiJ9.e30.x"));
        assert!(!githubs("nonsense"));
        // base64url's own `-` and `_`, base64's `+` and `/`, and what is neither.
        assert_eq!(base64url("-_+/!"), base64url("-_-_"));
    }

    /// Parts fetched ahead of a slow reader, at most twice as many as there are workers; a read into no room reads
    /// nothing; read to the end, nothing more; dropped midway, the workers stop.
    #[test]
    fn a_download_is_fetched_ahead_of_its_reader_and_stops_when_dropped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || for mut c in listener.incoming().flatten() {
            use std::io::Write;
            let mut head = [0u8; 2048];
            let _ = c.read(&mut head);
            let _ = c.write_all(b"HTTP/1.1 206 X\r\ncontent-length: 1\r\nconnection: close\r\n\r\na");
        });
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let parts: Vec<(String, u64, u64)> = (0..12).map(|i| (format!("{at}/{i}"), 0, 1)).collect();
        let mut p = Parallel::start(agent.clone(), parts.clone(), 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(p.read(&mut []).unwrap(), 0);
        let mut all = vec![];
        p.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"a".repeat(12));
        assert_eq!(p.read(&mut [0u8; 4]).unwrap(), 0);
        let mut first = [0u8; 1];
        let mut dropped = Parallel::start(agent, parts, 1);
        assert_eq!(dropped.read(&mut first).unwrap(), 1);
        drop(dropped);
        std::thread::sleep(Duration::from_millis(200));
    }
}
