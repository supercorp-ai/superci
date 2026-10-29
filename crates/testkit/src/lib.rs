//! A SuperCI control plane served on this computer for tests (`serve`), its own code end to end: a store in memory
//! the test can look into, stand-ins for the clouds (a machine is a container that starts at once and does nothing
//! more; AWS's SSM, where the dashboard keeps the control plane's settings, `Aws`), and a runner (`Runner`) that does
//! what GitHub's runner does on a machine, by the same calls. So the CLI and the dashboard are tested as they run,
//! against the control plane as it runs, offline.
pub mod github;
pub use github::GitHub;
pub mod gitlab;
pub use gitlab::GitLab;
#[cfg(unix)]
pub mod cloudflare;
#[cfg(unix)]
pub use cloudflare::{serve_cloudflare, CloudflarePlane};
pub mod sweep;
/// A SuperCI Worker under `wrangler dev`, as Cloudflare runs it.
#[cfg(unix)]
pub mod worker;
pub use sweep::{sweep, sweep_with_outages, Fault, Faults, Outcome};
#[cfg(unix)]
mod browser;
#[cfg(unix)]
pub use browser::Browser;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::executor::block_on;
use serde_json::{json, Value};
use superci_core::io::{self, Clock, Containers, Request, Store, Timer, Work};
use superci_core::plane::{Cache, Config, ControlPlane};

/// A store in memory, shared with the test; with what other requests write meanwhile (`Served::written_meanwhile`),
/// the records it refuses to write (`Served::store_refuses`), and its operations counted, one of them failing when a
/// sweep says so (`Faults`).
#[derive(Clone, Default)]
pub struct Shared(pub Arc<Mutex<BTreeMap<String, String>>>, pub Arc<Mutex<Vec<(String, u32, Change)>>>, pub Arc<Mutex<Vec<String>>>, pub Faults,
    /// How long each round trip to it takes, in milliseconds (none unless set: see `Served::store_takes`).
    pub Arc<std::sync::atomic::AtomicU64>);

/// What another request writes over a record (`None`: takes it out), from what is there.
/// The newest version of a served control plane's settings, as its store keeps it (see `superci_core::plane::SETTINGS`).
pub fn newest_settings(store: &BTreeMap<String, String>) -> Option<String> {
    store.range(superci_core::plane::SETTINGS.to_string()..).take_while(|(k, _)| k.starts_with(superci_core::plane::SETTINGS)).last().map(|(_, v)| v.clone())
}

thread_local! {
    /// On a served control plane's own thread: the settings views (`Aws::settings`, …) are its cloud's copy there,
    /// what a control plane builds its configuration from; elsewhere (a test's) they are what it reads, its record's.
    static AS_PLANE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Each control plane served, by its address: its store (where what was written through it is kept).
static SERVED: Mutex<BTreeMap<String, Arc<Mutex<BTreeMap<String, String>>>>> = Mutex::new(BTreeMap::new());

pub type Change = Arc<dyn Fn(Option<&str>) -> Option<String> + Send + Sync>;

#[async_trait(?Send)]
impl Store for Shared {
    async fn get(&self, k: &str) -> io::Result<Option<String>> { self.trip(); self.3.op(format!("get {k}"))?; Ok(self.0.lock().unwrap().get(k).cloned()) }
    async fn put(&self, k: &str, v: String) -> io::Result<()> { self.trip(); self.3.op(format!("put {k}"))?; self.refused(k)?; self.0.lock().unwrap().insert(k.into(), v); Ok(()) }
    async fn put_if_absent(&self, k: &str, v: String) -> io::Result<bool> { self.trip(); self.3.op(format!("put_if_absent {k}"))?; self.refused(k)?; let mut m = self.0.lock().unwrap(); if m.contains_key(k) { return Ok(false) } m.insert(k.into(), v); Ok(true) }
    // (Read, then written, as a Durable Object's store does: a control plane served here answers one request at a time.)
    async fn swap(&self, k: &str, was: Option<&str>, v: String) -> io::Result<bool> {
        self.trip();
        self.3.op(format!("swap {k}"))?;
        self.refused(k)?;
        let meanwhile = { let mut w = self.1.lock().unwrap(); w.iter_mut().find(|(p, n, _)| *n > 0 && k.starts_with(p.as_str())).map(|e| { e.1 -= 1; e.2.clone() }) };
        if let Some(change) = meanwhile { let mut m = self.0.lock().unwrap(); match change(m.get(k).map(String::as_str)) { Some(v) => { m.insert(k.into(), v); } None => { m.remove(k); } } }
        // (Its two steps, read then written, counted as the one operation.)
        let mut m = self.0.lock().unwrap();
        if m.get(k).map(String::as_str) != was { return Ok(false) }
        m.insert(k.into(), v);
        Ok(true)
    }
    async fn delete(&self, k: &str) -> io::Result<()> { self.trip(); self.3.op(format!("delete {k}"))?; self.0.lock().unwrap().remove(k); Ok(()) }
    async fn list(&self, p: &str) -> io::Result<Vec<(String, String)>> { self.trip(); self.3.op(format!("list {p}"))?; Ok(self.0.lock().unwrap().iter().filter(|(k, _)| k.starts_with(p)).map(|(k, v)| (k.clone(), v.clone())).collect()) }
    /// Several at once, in one round trip (as AWS's DynamoDB and a Modal control plane read them).
    async fn get_many(&self, keys: &[String]) -> io::Result<Vec<Option<String>>> {
        self.trip();
        self.3.op(format!("get_many {}", keys.first().map(String::as_str).unwrap_or_default()))?;
        let m = self.0.lock().unwrap();
        Ok(keys.iter().map(|k| m.get(k).cloned()).collect())
    }
}

impl Shared {
    /// One round trip to it: as long as it is set to take (see `Served::store_takes`).
    fn trip(&self) {
        let ms = self.4.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 { std::thread::sleep(std::time::Duration::from_millis(ms)) }
    }

    /// Its refusal of a write of `k`, when it refuses records under a prefix of `k` (see `Served::store_refuses`).
    fn refused(&self, k: &str) -> io::Result<()> {
        match self.2.lock().unwrap().iter().any(|p| k.starts_with(p.as_str())) { true => Err(format!("the store refused to write {k}")), false => Ok(()) }
    }

    /// The records under `prefix`, as the test reads them.
    pub fn under(&self, prefix: &str) -> Vec<(String, Value)> {
        self.0.lock().unwrap().iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| (k.clone(), serde_json::from_str(v).unwrap_or(Value::String(v.clone())))).collect()
    }
}


/// Machines: each starts at once, its runner given to the test to play (see `Runner`); or, while the cloud refuses
/// them (`Served::machines_refuse`), none, with what it said; the machines stopped, by id. Each start and stop is a
/// call a sweep counts.
#[derive(Clone, Default)]
pub struct Machines(pub Arc<Mutex<Vec<String>>>, pub Arc<Mutex<Option<String>>>, pub Faults, pub Arc<Mutex<Vec<String>>>);
#[async_trait(?Send)]
impl Containers for Machines {
    async fn start(&self, name: &str, _work: &Work, _max_minutes: u32, _size: superci_core::spec::Size) -> io::Result<String> {
        if self.2.call(format!("start machine {name}")).is_some() { return Err("the cloud did not answer: an internal error".into()) }
        if let Some(said) = self.1.lock().unwrap().clone() { return Err(said) }
        self.0.lock().unwrap().push(name.to_string()); Ok(format!("machine-{name}"))
    }
    async fn stop(&self, id: &str) -> io::Result<()> { if self.2.call(format!("stop machine {id}")).is_some() { return Err("the cloud did not answer: an internal error".into()) } self.3.lock().unwrap().push(id.to_string()); Ok(()) }
}

fn system_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// The control plane's time: this computer's, moved on by the test (`Served::later`).
#[derive(Clone, Default)]
struct Now(Arc<std::sync::atomic::AtomicU64>);
impl Clock for Now { fn now_ms(&self) -> u64 { system_ms() + self.0.load(std::sync::atomic::Ordering::SeqCst) } }
struct NoTimer;
#[async_trait(?Send)]
impl Timer for NoTimer { async fn wake_in(&self, _ms: u64) -> io::Result<()> { Ok(()) } }

/// A control plane served on this computer: its address, its store, its machines, AWS as it finds it (its settings
/// are SSM parameters there, `/superci/testplane0001/…`, read on every request), and a key with which the dashboard
/// and the CLI reach it (and one for coding agents, which may also start runs).
pub struct Served { pub plane: String, pub url: String, pub store: Shared, pub machines: Machines, pub aws: Aws, pub github: GitHub, pub gitlab: GitLab, pub key: String, pub run_key: String, clock: Now,
    /// The address the control plane is told it answers at, when not this computer's (`https://…`, as behind a
    /// deployed one's URL): its machines' links are made with it.
    pub public: Arc<Mutex<Option<String>>>,
    /// It stopped answering (its connections closed at once), until this is set back.
    pub down: Arc<std::sync::atomic::AtomicBool>,
    /// Its servers (its own, and its stand-ins'), ended when it is dropped.
    pub ends: Ends }

/// Servers that end when this is dropped: each told to stop, then reached once, so that one waiting for a connection
/// sees it. (A sweep serves thousands of control planes in one test program: left on, their threads would pass what
/// a program may have.)
pub struct Ends { stops: Vec<Arc<std::sync::atomic::AtomicBool>>, at: Vec<String>, github: Arc<Mutex<Option<GitHub>>> }

impl Ends {
    fn of(plane: &Arc<std::sync::atomic::AtomicBool>, url: &str, aws: &Aws) -> Ends {
        Ends { stops: vec![plane.clone(), aws.stop.clone()], at: [url, aws.url.as_str()].iter().map(|u| u.trim_start_matches("http://").to_string()).collect(), github: aws.github.clone() }
    }
}

impl Drop for Ends {
    fn drop(&mut self) {
        for s in &self.stops { s.store(true, std::sync::atomic::Ordering::SeqCst) }
        for a in &self.at { let _ = TcpStream::connect(a); }
        // (AWS's stand-in and GitHub's hold each other: let go, or neither is ever freed, with all the answers each
        // kept.)
        if let Some(g) = self.github.lock().unwrap().take() { g.aws.lock().unwrap().take(); }
    }
}

impl Served {
    /// Moves the control plane's time on by `ms`.
    /// Time passing, for a hook (`Aws::during`): each call moves this control plane's clock on by `ms`.
    pub fn ticker(&self) -> impl Fn(u64) + Send + Sync + 'static { let c = self.clock.0.clone(); move |ms| { c.fetch_add(ms, std::sync::atomic::Ordering::SeqCst); } }

    pub fn later(&self, ms: u64) { self.clock.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst); }

    /// The control plane's time now.
    pub fn now_ms(&self) -> u64 { self.clock.now_ms() }

    /// SuperCI's App for the organization `acme`, as the dashboard leaves it once made and installed (`GitHub`).
    /// Each round trip to its store takes `ms` from now on, as a store in the cloud does (a Modal control plane's some
    /// 120 ms; DynamoDB's a few).
    pub fn store_takes(&self, ms: u64) { self.store.4.store(ms, std::sync::atomic::Ordering::Relaxed) }

    pub fn connect_github(&self) { self.aws.put(&format!("/superci/{}/GITHUB_APP", self.plane), &GitHub::app().to_string()) }

    /// A webhook GitHub sends (`GitHub::queue`, `start`, `finish`), as it sends it: the control plane's answer.
    pub fn webhook(&self, (body, signature): (Vec<u8>, String)) -> u16 {
        let host = self.url.trim_start_matches("http://");
        let mut s = TcpStream::connect(host).unwrap();
        s.write_all(format!("POST /webhook HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\nx-github-event: workflow_job\r\nx-hub-signature-256: {signature}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
        s.write_all(&body).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        // (Kept among the App's deliveries, as GitHub keeps each.)
        self.github.delivered(&body, status, out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or_default(), &format!("{}/webhook", self.url), self.clock.now_ms());
        status
    }

    /// A GitLab connection, as the dashboard leaves it (`GitLab`).
    pub fn connect_gitlab(&self) { self.aws.put(&format!("/superci/{}/GITLAB", self.plane), &GitLab::connection().to_string()) }

    /// A job event of GitLab's (`GitLab::pending`, `running`, `finish`), with the webhook's secret: the control
    /// plane's answer.
    pub fn gitlab_webhook(&self, body: Vec<u8>) -> u16 {
        let host = self.url.trim_start_matches("http://");
        let mut s = TcpStream::connect(host).unwrap();
        s.write_all(format!("POST /gitlab/webhook HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\nx-gitlab-event: Job Hook\r\nx-gitlab-token: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", gitlab::HOOK_SECRET, body.len()).as_bytes()).unwrap();
        s.write_all(&body).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0)
    }

    /// Its upkeep, as its cloud runs it every minute (`ControlPlane::alarm`).
    pub fn upkeep(&self) {
        let host = self.url.trim_start_matches("http://");
        let mut s = TcpStream::connect(host).unwrap();
        s.write_all(format!("POST /__testkit/upkeep HTTP/1.1\r\nhost: {host}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    }

    /// From now on it answers as an older version did, its key still taken: its health says `version` (empty: its own)
    /// and its status is without each of `without`, a path of fields (`active`, `usage.*.runs`, `installations.*.app`:
    /// `*` is each item of a list or each field of an object). `None` of it again with `answer_as("", &[])`.
    pub fn answer_as(&self, version: &str, without: &[&str]) {
        *self.aws.answers_as.lock().unwrap() = (!version.is_empty() || !without.is_empty()).then(|| (version.to_string(), without.iter().map(|w| w.to_string()).collect()));
    }

    /// Once it has answered a request to `after` (empty: from now on), each of the `next` answers to a path (`/status`,
    /// `/move/claim`), in turn, leaves out those fields (paths as `answer_as` takes them): one that has not taken a
    /// change yet, as a control plane just given it answers for a moment.
    pub fn then_answer(&self, after: &str, next: &[(&str, &[&str])]) {
        *self.aws.answers_next.lock().unwrap() = ((!after.is_empty()).then(|| after.to_string()), next.iter().map(|(p, w)| (p.to_string(), w.iter().map(|w| w.to_string()).collect())).collect());
    }

    /// What its stand-ins, store and machines counted, and the fault armed (see `sweep`).
    pub fn faults(&self) -> &Faults { &self.aws.faults }

    /// Counting starts now, with `fault` armed (a scenario of a `sweep` calls this once set up).
    pub fn arm(&self, fault: Fault) { self.aws.faults.arm(fault) }

    /// The calls answered since `arm` (AWS's, GitHub's, GitLab's, Cloudflare's, Modal's, its machines').
    pub fn calls(&self) -> Vec<String> { self.aws.faults.calls() }

    /// Its store's operations since `arm`.
    pub fn store_ops(&self) -> Vec<String> { self.aws.faults.store_ops() }

    /// The k-th call from now answered with its service's error.
    pub fn fail_call(&self, k: usize) { self.aws.faults.fail_call(k) }

    /// The k-th store operation from now not answered.
    pub fn fail_store_op(&self, k: usize) { self.aws.faults.fail_store_op(k) }

    /// Its upkeep, as `upkeep`, whatever it answers: its status and what it said.
    pub fn try_upkeep(&self) -> (u16, String) {
        let host = self.url.trim_start_matches("http://");
        let Ok(mut s) = TcpStream::connect(host) else { return (0, String::new()) };
        let _ = s.write_all(format!("POST /__testkit/upkeep HTTP/1.1\r\nhost: {host}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes());
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        (out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0), out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
    }

    /// How many of those answers are still to be given.
    pub fn answers_to_come(&self) -> usize { self.aws.answers_next.lock().unwrap().1.len() }
}

/// The control plane's id, and the AWS account and region it is in.
pub const PLANE: &str = "testplane0001";
pub const ACCOUNT: &str = "111111111111";
pub const REGION: &str = "us-east-1";

/// Serves a control plane with machines of its own (`containers`), `configure`d further by the test.
pub fn serve(configure: impl Fn(&mut Config) + Send + 'static) -> Served { serve_as(PLANE, configure) }

/// The same, with this id (a second control plane beside the first).
pub fn serve_as(plane: &str, configure: impl Fn(&mut Config) + Send + 'static) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let aws = Aws::serve();
    let (store, machines) = (Shared(Default::default(), Default::default(), Default::default(), aws.faults.clone(), Default::default()), Machines(Default::default(), Default::default(), aws.faults.clone(), Default::default()));
    *aws.plane_url.lock().unwrap() = url.clone();
    *aws.plane_store.lock().unwrap() = Some(store.0.clone());
    SERVED.lock().unwrap().insert(url.clone(), store.0.clone());
    let github = GitHub::default();
    *aws.github.lock().unwrap() = Some(github.clone());
    *github.aws.lock().unwrap() = Some(aws.clone());
    let (key, run_key) = ("dashboard-key-for-tests-0001".to_string(), "superci_run_0123456789abcdef0123456789abcdef".to_string());
    // Its keys, as a deploy and `superci keys create agent --runs` leave them.
    aws.put(&format!("/superci/{plane}/DEVICE_KEY_4102444800_TEST"), &key);
    aws.put(&format!("/superci/{plane}/READ_KEY_4102444800_AGENT"), &format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes())));
    aws.put(&format!("/superci/{plane}/CONTAINERS"), "on");
    let clock = Now::default();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let public: Arc<Mutex<Option<String>>> = Arc::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s, m, a, u, c, g, id, dn, pb, st) = (store.clone(), machines.clone(), aws.clone(), url.clone(), clock.clone(), github.clone(), plane.to_string(), down.clone(), public.clone(), stop.clone());
    // (Written before it answers anything: before a sweep's counting starts.)
    block_on(store.put("plane_url", format!("\"{url}\""))).unwrap();
    std::thread::spawn(move || {
        AS_PLANE.with(|p| p.set(true));
        let cache = Cache::default();
        for stream in listener.incoming().flatten() {
            if st.load(std::sync::atomic::Ordering::SeqCst) { break }
            if dn.load(std::sync::atomic::Ordering::SeqCst) { drop(stream); continue }
            let mut config = Config::new(id.clone());
            let mut settings = a.settings(&id);
            // (One from before reads its launchers and its devices' keys under their old names, and not the new ones.)
            if a.old_version.lock().unwrap().is_some() {
                settings = settings.into_iter().filter(|(k, _)| k != "LAUNCHERS" && !k.starts_with("DEVICE_KEY_"))
                    .map(|(k, v)| (if k == "AGENTS" { "LAUNCHERS".into() } else { k.replacen("DASHBOARD_KEY_", "DEVICE_KEY_", 1) }, v)).collect();
            }
            // (Its machines are its own containers, as a Cloudflare or Modal control plane's are: on until removed.)
            config.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
            config.read_settings(&settings);
            configure(&mut config);
            let plane = ControlPlane { store: &s, http: &g, clock: &c, timer: &NoTimer, config: &config, cache: &cache, containers: Some(&m) };
            let old = a.old_version.lock().unwrap().clone();
            let base = pb.lock().unwrap().clone().unwrap_or_else(|| u.clone());
            // (A panic is kept, the connection closed unanswered, and the next request answered.)
            if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| answer(&plane, &base, stream, old, &a))) { a.faults.panicked(p) }
        }
    });
    let gitlab = github.gitlab.clone();
    let ends = Ends::of(&stop, &url, &aws);
    Served { plane: plane.to_string(), url, store, machines, aws, github, gitlab, key, run_key, clock, down, public, ends }
}

/// A request as it arrived: method, target, headers, body.
fn read(stream: &TcpStream) -> std::io::Result<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut r = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut p = line.split_whitespace();
    let (method, target) = (p.next().unwrap_or_default().to_string(), p.next().unwrap_or_default().to_string());
    let (mut headers, mut len) = (vec![], 0usize);
    loop {
        let mut h = String::new();
        r.read_line(&mut h)?;
        if h.trim().is_empty() { break }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap_or(0) }
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok((method, target, headers, body))
}

fn write(mut out: TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
    for (k, v) in headers { if !k.eq_ignore_ascii_case("content-length") { head.push_str(&format!("{k}: {v}\r\n")) } }
    out.write_all(format!("{head}\r\n").as_bytes())?;
    out.write_all(body)
}

/// One request on its connection, answered by the control plane, then the connection closed.
/// (One said to be of an older version answers as that version did.)
fn answer(plane: &ControlPlane, base: &str, stream: TcpStream, old: Option<String>, aws: &Aws) -> std::io::Result<()> {
    let (method, target, headers, body) = read(&stream)?;
    aws.faults.asking(format!("{method} {target}"));
    if target == "/__testkit/upkeep" {
        let (status, said) = match block_on(plane.alarm()) { Ok(()) => (200, "ok".to_string()), Err(e) => (500, e) };
        aws.faults.answered(status, said.as_bytes());
        return write(stream, status, &[], said.as_bytes());
    }
    let mut req = Request::new(&method, &format!("{base}{target}")).with_body(body);
    for (k, v) in headers { req = req.with_header(&k, &v) }
    let res = block_on(plane.handle(req));
    let body = match (old, target.split('?').next().unwrap_or_default(), serde_json::from_slice::<Value>(&res.body)) {
        (Some(version), "/health", Ok(mut v)) => { v["version"] = json!(version); v.to_string().into_bytes() }
        (Some(version), "/status", Ok(mut v)) => {
            if let Some(o) = v.as_object_mut() {
                let l = o.remove("launchers").unwrap_or_default();
                o.insert("agents".into(), l);
                // (What came after it, it does not say: limits for the whole CI and an account came in 0.15.0, a day's in 0.16.0.)
                let older = |than: &str| { let n = |s: &str| s.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>(); n(&version) < n(than) };
                if older("0.15.0") { o.remove("account_limits"); }
                if older("0.16.0") { o.remove("daily_limits"); }
            }
            v.to_string().into_bytes()
        }
        _ => res.body,
    };
    // (Answering as an older one, or once as one that has not taken a change yet: see `Served::answer_as`, `then_answer`.)
    let path = target.split('?').next().unwrap_or_default().to_string();
    let like = aws.answers_as.lock().unwrap().clone();
    let once = {
        let mut next = aws.answers_next.lock().unwrap();
        if next.0.as_deref() == Some(path.as_str()) { next.0 = None; None }
        else if next.0.is_none() { next.1.iter().position(|(p, _)| *p == path).map(|i| next.1.remove(i).1) } else { None }
    };
    let body = match (like, path.as_str(), serde_json::from_slice::<Value>(&body)) {
        (Some((version, _)), "/health", Ok(mut v)) if !version.is_empty() => { v["version"] = json!(version); v.to_string().into_bytes() }
        (Some((_, without)), "/status", Ok(mut v)) => { for path in &without { leave_out(&mut v, &path.split('.').collect::<Vec<_>>()) } v.to_string().into_bytes() }
        _ => body,
    };
    let body = match (once, serde_json::from_slice::<Value>(&body)) {
        (Some(without), Ok(mut v)) => { for path in &without { leave_out(&mut v, &path.split('.').collect::<Vec<_>>()) } v.to_string().into_bytes() }
        _ => body,
    };
    aws.faults.answered(res.status, &body);
    write(stream, res.status, &res.headers, &body)
}

/// A DictUpdateRequest's entries (`updates`, field 2: each a key, field 1, and a value, field 2), each pickled as
/// Modal's client pickles a string (protocol 4), as text.
fn modal_entries(b: &[u8]) -> Vec<(String, String)> {
    fn varint(b: &[u8], i: &mut usize) -> Option<u64> { let (mut n, mut shift) = (0u64, 0); loop { let c = *b.get(*i)?; *i += 1; n |= ((c & 0x7f) as u64) << shift; shift += 7; if c < 0x80 { return Some(n) } } }
    fn fields(b: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let (mut i, mut out) = (0, vec![]);
        while i < b.len() {
            let Some(key) = varint(b, &mut i) else { break };
            match key & 7 {
                0 => { if varint(b, &mut i).is_none() { break } }
                2 => { let Some(n) = varint(b, &mut i) else { break }; let end = (i + n as usize).min(b.len()); out.push((key >> 3, b[i..end].to_vec())); i = end }
                _ => break,
            }
        }
        out
    }
    let unpickle = |p: &[u8]| -> String {
        // 0x80 0x04 0x95, 8 bytes of length; then 0x8c and one byte, or `X` and four, of the text's length; the text.
        let body = p.get(11..).unwrap_or_default();
        let (at, len) = match body.first() { Some(0x8c) => (2, *body.get(1).unwrap_or(&0) as usize), Some(b'X') => (5, u32::from_le_bytes(body.get(1..5).and_then(|s| s.try_into().ok()).unwrap_or([0; 4])) as usize), _ => (0, 0) };
        String::from_utf8_lossy(body.get(at..at + len).unwrap_or_default()).into_owned()
    };
    fields(b).into_iter().filter(|(f, _)| *f == 2).map(|(_, entry)| {
        let e = fields(&entry);
        let get = |n: u64| e.iter().find(|(f, _)| *f == n).map(|(_, v)| unpickle(v)).unwrap_or_default();
        (get(1), get(2))
    }).collect()
}

/// AWS as SuperCI's calls find it (the CLI and the dashboard reach it with `SUPERCI_STAND_IN` set to `url`; what they ask of other hosts there, GitHub's stand-in answers): SSM's
/// parameters, kept and answered for exactly as SSM does (its answers recorded from a real account are
/// `crates/cli/fixtures/aws/settings/ssm.jsonl`, and a test holds this to them). Each exchange is kept, in the shape
/// of a recording (`asked`); what it has no answer for is answered with an error that names it, and kept apart
/// (`unanswered`), so a test sees what it reached that is not stood in for yet.
#[derive(Clone, Default)]
pub struct Aws {
    pub url: String,
    /// Each parameter: its value, version, and when it was written last (milliseconds).
    pub params: Arc<Mutex<BTreeMap<String, (String, u64, u64)>>>,
    pub asked: Arc<Mutex<Vec<Value>>>,
    pub unanswered: Arc<Mutex<Vec<String>>>,
    /// What a real account answered for the rest (`fixtures/aws/settings/dashboard.jsonl`: what the dashboard asks of
    /// AWS when signed in), and the address its control plane's stands in for (`http://plane.test`).
    pub recorded: Arc<Mutex<Vec<Value>>>,
    /// How many of each recorded call's answers were given (several of the same call are answered in their order).
    replayed: Arc<Mutex<BTreeMap<String, usize>>>,
    pub plane_url: Arc<Mutex<String>>,
    /// Where what is not AWS's goes (GitHub's logs).
    pub github: Arc<Mutex<Option<GitHub>>>,
    /// The secrets written to each Worker (by its script's name), as Cloudflare keeps them for it: its settings.
    pub worker_secrets: Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>,
    /// What Modal's settings Dicts were given (`DictUpdate`), by key: a Modal control plane's settings.
    pub modal_settings: Arc<Mutex<BTreeMap<String, String>>>,
    /// A Modal sign-in as Modal has it (see `modal_approve`): the calls waiting for its approval now, and whether it
    /// was approved (then each call waiting at that moment is given the token, and none after).
    pub modal_flow: Arc<(Mutex<ModalFlow>, std::sync::Condvar)>,
    /// What Modal says beside its answers from now on (`x-modal-warning`, as it warns of a client it will stop taking),
    /// as it sends it: each warning percent-encoded, joined with commas.
    pub modal_warning: Arc<Mutex<Option<String>>>,
    /// Workers on workers.dev, by host (`superci.acme.workers.dev`), and the control plane on this computer each stands
    /// for (its address).
    pub workers: Arc<Mutex<BTreeMap<String, String>>>,
    /// DynamoDB's items, by table, partition and sort key (`p`, `k`: an AWS control plane's table), as its item calls
    /// keep them.
    pub items: Arc<Mutex<BTreeMap<(String, String, String), Value>>>,
    /// Lambda functions called without waiting (`Event`): each function's name and what it was given.
    pub invoked: Arc<Mutex<Vec<(String, Value)>>>,
    /// A control plane not updated yet: while set, it says this version and answers as one from before 0.19.13 did
    /// (its launchers as `agents`). Its function's code uploaded again (an update) ends it.
    pub old_version: Arc<Mutex<Option<String>>>,
    /// Calls answered with an error, by action (`RunInstances`, `AmazonSSM.PutParameter`, …, or `Action|address part`):
    /// its status and answer, and how many more times (none: every time). Written as AWS documents its errors, for what
    /// a test cannot have a real account answer (throttling, a fault of AWS's, an organization's policy).
    pub failing: Arc<Mutex<Vec<(String, u16, String, Option<u32>)>>>,
    /// Headers some of those answers carry (see `fail_with_headers`), by the same action.
    pub failing_headers: Arc<Mutex<BTreeMap<String, Vec<(String, String)>>>>,
    /// What happens as an action is asked (`during`): time passing while AWS answers, a job's end arriving meanwhile.
    pub during: Arc<Mutex<Vec<(String, Arc<dyn Fn() + Send + Sync>)>>>,
    /// The control plane answering as an older version does, its key taken as now (see `Served::answer_as`): the
    /// version its health says (none: as it is), and what its status leaves out.
    pub answers_as: Arc<Mutex<Option<(String, Vec<String>)>>>,
    /// The store of the control plane served beside it: where the settings written through it are kept (`settings`).
    pub plane_store: Arc<Mutex<Option<Arc<Mutex<BTreeMap<String, String>>>>>>,
    /// Answers given once each without some of their fields, once a request to a path was answered (see
    /// `Served::then_answer`): that path (none: from now on), and each answer's path and the fields it leaves out.
    pub answers_next: Arc<Mutex<(Option<String>, Vec<(String, Vec<String>)>)>>,
    /// Every call answered here (and by GitHub's stand-in, the control plane's store and machines) counted, one of
    /// them failing when a sweep says so; what the control plane answered.
    pub faults: Faults,
    /// Its server ends (see `Ends`).
    pub stop: Arc<std::sync::atomic::AtomicBool>,
}

/// A Modal sign-in's approval as Modal hands it out (see `Aws::modal_approve`).
#[derive(Default)]
pub struct ModalFlow {
    /// Calls to `TokenFlowWait` waiting now, and how many came in all (each call's place among them).
    waiting: usize,
    entered: u64,
    /// Approved: the calls that had come by then (those still waiting are given `token`; the ones after, never).
    approved_upto: Option<u64>,
    token: Vec<u8>,
}

/// A protobuf message's fields, as far as the test kit reads them: each number with its varint, or its bytes (a
/// fixed32 as its four).
fn proto_fields(mut b: &[u8]) -> Vec<(u64, u64, Vec<u8>)> {
    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0;
        for shift in (0..64).step_by(7) { let (&x, rest) = b.split_first()?; *b = rest; v |= u64::from(x & 0x7F) << shift; if x & 0x80 == 0 { return Some(v) } }
        None
    }
    let mut out = vec![];
    while let Some(key) = varint(&mut b) {
        let (no, wire) = (key >> 3, key & 7);
        let field = match wire {
            0 => (no, varint(&mut b).unwrap_or_default(), vec![]),
            5 if b.len() >= 4 => { let (f, rest) = b.split_at(4); b = rest; (no, 0, f.to_vec()) }
            2 => { let n = varint(&mut b).unwrap_or_default() as usize; let (f, rest) = b.split_at(n.min(b.len())); b = rest; (no, 0, f.to_vec()) }
            _ => break,
        };
        out.push(field);
    }
    out
}

/// Modal's refusal of a call, by what the client said it is (its headers), as Modal answered on 2026-10-28: a Go
/// client (`x-modal-client-type: 9`) creates an app (`AppGetOrCreate` asked to create one when missing) only when
/// its `x-modal-libmodal-version` reads as the Go SDK says it (`modal-go/0.11.1`, `modal-go/v0.7.3`), or when it says
/// none. The test kit's workspace has none of SuperCI's apps yet: each create-if-missing creates one.
fn modal_refuses(method: &str, headers: &http::HeaderMap, message: &[u8]) -> Option<(u32, String)> {
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let creates = method == "AppGetOrCreate" && proto_fields(message).iter().any(|(no, v, _)| *no == 3 && *v != 0);
    let read = header("x-modal-libmodal-version").is_none_or(|v| v.strip_prefix("modal-go/").map(|v| v.trim_start_matches('v')).is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit())));
    (creates && header("x-modal-client-type") == Some("9") && !read)
        .then(|| (9, "Could not determine the SDK version for client type CLIENT_TYPE_LIBMODAL_GO, so it cannot be used to create new apps.".to_string()))
}

/// What Modal's Python client says when it is to build with an image builder version newer than it knows (1.6.0 in a
/// workspace on 2026.10, as it said on 2026-10-29): the test kit's workspace stands for one on a version no client knows
/// yet, as every new workspace comes to be. A client told which to build with (`MODAL_IMAGE_BUILDER_VERSION`) builds.
pub const MODAL_BUILDER_REFUSED: &str = "This version of the modal client supports the following image builder versions: {'2025.06', '2026.10'}. You are using '2099.01'. Please update your client library.";

/// What Modal's client in a sandbox, or in an app it deploys, refuses: running `modal` (a deploy) without being told
/// which image builder version to build with, or a script whose `import modal` comes before it is told (the app
/// builds its runner's image as it runs). `asked` is `SandboxCreate`'s request, or `SandboxStdinWrite`'s (the script
/// comes first on a sandbox's input, in base64).
fn modal_builder_refuses(method: &str, asked: &[u8]) -> bool {
    use base64::Engine;
    let field = |m: &[u8], no: u64| -> Vec<Vec<u8>> { proto_fields(m).into_iter().filter(|(n, _, _)| *n == no).map(|(_, _, b)| b).collect() };
    match method {
        "SandboxCreate" => field(asked, 2).first().is_some_and(|definition| {
            let runs_modal = field(definition, 1).iter().any(|a| String::from_utf8_lossy(a).contains("modal "));
            let told = field(definition, 40).iter().flat_map(|map| field(map, 1)).any(|entry| field(&entry, 1).first().is_some_and(|k| k == b"MODAL_IMAGE_BUILDER_VERSION"));
            runs_modal && !told
        }),
        "SandboxStdinWrite" => field(asked, 2).first().and_then(|input| input.split(|b| *b == b'\n').next().map(<[u8]>::to_vec))
            .and_then(|line| base64::engine::general_purpose::STANDARD.decode(line).ok()).is_some_and(|script| {
                let script = String::from_utf8_lossy(&script);
                let (told, imported) = (script.find("os.environ[\"MODAL_IMAGE_BUILDER_VERSION\"] = \"20"), script.find("\nimport modal"));
                imported.is_some_and(|i| told.is_none_or(|t| t > i))
            }),
        _ => false,
    }
}

/// EC2's error answer (and IAM's, STS's: the query APIs'), as AWS documents it.
pub fn ec2_error(code: &str, message: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Response><Errors><Error><Code>{code}</Code><Message>{message}</Message></Error></Errors><RequestID>00000000-0000-4000-8000-00000000e001</RequestID></Response>")
}

/// A JSON API's error answer (SSM, DynamoDB, Lambda, …), as AWS documents it.
pub fn json_error(kind: &str, message: &str) -> String { json!({ "__type": kind, "message": message }).to_string() }

/// What a request asks AWS to do: its target (JSON APIs) or its Action (query APIs).
pub(crate) fn action(target: &str, asked: &str) -> String {
    if !target.is_empty() { return target.to_string() }
    asked.split('&').find_map(|kv| kv.strip_prefix("Action=")).unwrap_or_default().to_string()
}

impl Aws {
    /// Modal's answer to one call (`method`, its request's message), as a recording has it: each answer's message, or
    /// Modal's refusal as a gRPC status (its code and message).
    fn modal_answer(&self, method: &str, headers: &http::HeaderMap, message: &[u8]) -> (Vec<Vec<u8>>, Option<(u32, String)>) {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let url = format!("https://api.modal.com/modal.client.ModalClient/{method}");
        let asked = b64.encode(message);
        // (A sweep's fault: Modal's own status for an internal error.)
        if self.faults.call(format!("POST {url}")).is_some() { return (vec![], Some((13, "internal error".to_string()))) }
        // (What Modal refuses whatever a recording has: a client it cannot tell the version of; and a client that says
        // it is another than the one recorded.)
        if let Some((code, message)) = modal_refuses(method, headers, message).or_else(|| self.modal_sent_differs(&url, headers).map(|m| (3, m)))
            .or_else(|| modal_builder_refuses(method, message).then(|| (9, MODAL_BUILDER_REFUSED.to_string()))) {
            self.asked.lock().unwrap().push(json!({ "method": "POST", "url": url, "target": "", "asked": asked, "status": 500, "answer": json!({ "error": format!("Modal {method}: {message}") }).to_string() }));
            return (vec![], Some((code, message)))
        }
        // (What a Modal settings Dict is given, kept: a Modal control plane served here reads it.)
        if method == "DictUpdate" { for (k, v) in modal_entries(message) { self.modal_settings.lock().unwrap().insert(k, v); } }
        // (What a sandbox is given on its input, taken as Modal takes it: nothing answered but that it was, a recording
        // having it or not.)
        let written = (method == "SandboxStdinWrite").then(|| (200, vec![], json!({ "answers": [""] }).to_string()));
        let (status, _, answer) = self.replay("POST", &url, "", &asked).or(written).unwrap_or_else(|| {
            self.unanswered.lock().unwrap().push(format!("POST {url}"));
            (404, vec![], json!({ "error": format!("Modal {method}: the test kit's Modal has no answer for it") }).to_string())
        });
        self.asked.lock().unwrap().push(json!({ "method": "POST", "url": url, "target": "", "asked": asked, "status": status, "answer": answer }));
        let v: Value = serde_json::from_str(&answer).unwrap_or_default();
        let answers = v["answers"].as_array().into_iter().flatten().filter_map(|a| b64.decode(a.as_str()?).ok()).collect();
        // (A refusal is recorded as the CLI said it: "Modal METHOD: …", or a sign-in Modal no longer takes, code 16.
        // One with answers too is a stream that broke off after them.)
        if let Some(e) = v["error"].as_str() {
            let said = format!("Modal {method}: ");
            return match e.find(&format!("(Modal {method}: ")) {
                Some(at) if e.contains("no longer accepted") => (answers, Some((16, e[at + 1 + said.len()..].trim_end_matches(')').to_string()))),
                _ => (answers, Some((2, e.strip_prefix(&said).unwrap_or(e).to_string()))),
            }
        }
        // A sign-in's approval is answered as Modal hands it out, whatever a recording's order was (see `modal_wait`).
        if method == "TokenFlowWait" {
            let timeout = proto_fields(message).into_iter().find(|(no, _, f)| *no == 1 && f.len() == 4).map(|(_, _, f)| f32::from_le_bytes([f[0], f[1], f[2], f[3]])).unwrap_or_default();
            return (vec![self.modal_wait(timeout)], None)
        }
        (answers, None)
    }

    /// How what a client says it is (`x-modal-client-type` and the like) differs from what it said when the newest
    /// recording of this call was made: a header left out, another client type, another SDK. A release's number may
    /// differ (Modal's rule is `modal_refuses`'s; the newest release, scripts/modal-drift.py's).
    fn modal_sent_differs(&self, url: &str, headers: &http::HeaderMap) -> Option<String> {
        let recorded = self.recorded.lock().unwrap();
        let sent = recorded.iter().filter(|r| r["url"] == url && r["sent"].is_object()).max_by_key(|r| r["recording"].as_u64().unwrap_or(0))?["sent"].clone();
        sent.as_object()?.iter().find_map(|(k, was)| {
            let now = headers.get(k).and_then(|h| h.to_str().ok());
            let kind = |v: Option<&str>| v.map(|v| if k == "x-modal-libmodal-version" { v.split('/').next().unwrap_or_default().to_string() } else { v.to_string() });
            (kind(now) != kind(was.as_str())).then(|| format!("the test kit's Modal: {k} was {was} when this was recorded, and is {} now", now.unwrap_or("not said")))
        })
    }

    /// One `TokenFlowWait` as Modal answers it: it waits up to `timeout` seconds, and is given the token only when the
    /// sign-in is approved while it waits (see `modal_approve`); otherwise, and after, it is told to wait on.
    fn modal_wait(&self, timeout: f32) -> Vec<u8> {
        let (flow, changed) = &*self.modal_flow;
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f32(timeout.max(0.0));
        let mut f = flow.lock().unwrap();
        (f.entered, f.waiting) = (f.entered + 1, f.waiting + 1);
        let ticket = f.entered;
        changed.notify_all();
        loop {
            if f.approved_upto.is_some_and(|upto| ticket <= upto) { f.waiting -= 1; return f.token.clone() }
            let left = end.saturating_duration_since(std::time::Instant::now());
            // (`timeout: true`, as Modal says it.)
            if left.is_zero() { f.waiting -= 1; return vec![0x18, 0x01] }
            f = changed.wait_timeout(f, left).unwrap().0;
        }
    }

    /// The person approves the Modal sign-in on Modal's page: once a call waits for it (five seconds at most, as they
    /// would not wait for one), its token (the one a recording has) goes to each call waiting at that moment. Modal
    /// keeps it for no call after: approved with none waiting, a sign-in never ends.
    pub fn modal_approve(&self) {
        let recorded: Vec<Value> = self.recorded.lock().unwrap().iter().filter(|r| r["url"].as_str().is_some_and(|u| u.ends_with("/TokenFlowWait"))).cloned().collect();
        use base64::Engine;
        let token = recorded.iter().filter_map(|r| serde_json::from_str::<Value>(r["answer"].as_str()?).ok()?["answers"][0].as_str().map(str::to_string))
            .filter_map(|a| base64::engine::general_purpose::STANDARD.decode(a).ok()).find(|m| !proto_fields(m).iter().any(|(no, v, _)| *no == 3 && *v != 0))
            .expect("a recording with the sign-in's approved answer");
        let (flow, changed) = &*self.modal_flow;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut f = flow.lock().unwrap();
        while f.waiting == 0 && std::time::Instant::now() < end { f = changed.wait_timeout(f, end.saturating_duration_since(std::time::Instant::now())).unwrap().0 }
        (f.token, f.approved_upto) = (token, Some(f.entered));
        changed.notify_all();
    }

    /// One HTTP/2 connection, each request a gRPC call to Modal (`modal_answer`): its message out of gRPC's frame, each
    /// answer framed, then its status in the trailers.
    fn serve_grpc(&self, stream: TcpStream) {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let a = self.clone();
        rt.block_on(async move {
            if stream.set_nonblocking(true).is_err() { return }
            let Ok(s) = tokio::net::TcpStream::from_std(stream) else { return };
            let Ok(mut conn) = h2::server::handshake(s).await else { return };
            while let Some(Ok((req, mut respond))) = conn.accept().await {
                let a = a.clone();
                tokio::spawn(async move {
                    let method = req.uri().path().rsplit('/').next().unwrap_or_default().to_string();
                    let headers = req.headers().clone();
                    let mut body = req.into_body();
                    let mut data = vec![];
                    while let Some(Ok(chunk)) = body.data().await { let _ = body.flow_control().release_capacity(chunk.len()); data.extend_from_slice(&chunk); }
                    // (gRPC's frame: a flag, four bytes of length, the message. Answered apart: a sign-in's wait waits.)
                    let asked = a.clone();
                    let Ok((answers, refused)) = tokio::task::spawn_blocking(move || asked.modal_answer(&method, &headers, data.get(5..).unwrap_or_default())).await else { return };
                    // (Its message percent-encoded where gRPC asks it to be.)
                    let encode = |message: &str| -> String { message.bytes().map(|b| if (0x20..0x7f).contains(&b) && b != b'%' { (b as char).to_string() } else { format!("%{b:02X}") }).collect() };
                    // (What Modal says beside it, in the trailers, as it does.)
                    let warning = a.modal_warning.lock().unwrap().clone();
                    // A refusal with nothing before it comes as gRPC servers send one, in the answer's headers alone.
                    if let (Some((code, message)), true) = (&refused, answers.is_empty()) {
                        let mut r = http::Response::builder().status(200).header("content-type", "application/grpc").header("grpc-status", code.to_string()).header("grpc-message", encode(message));
                        if let Some(w) = &warning { r = r.header("x-modal-warning", w) }
                        let _ = respond.send_response(r.body(()).unwrap(), true);
                        return;
                    }
                    let Ok(mut send) = respond.send_response(http::Response::builder().status(200).header("content-type", "application/grpc").body(()).unwrap(), false) else { return };
                    for m in &answers {
                        let mut f = vec![0u8];
                        f.extend((m.len() as u32).to_be_bytes());
                        f.extend(m);
                        let _ = send.send_data(bytes::Bytes::from(f), false);
                    }
                    let mut trailers = http::HeaderMap::new();
                    let (code, message) = refused.unwrap_or((0, String::new()));
                    trailers.insert("grpc-status", code.to_string().parse().unwrap());
                    if code != 0 { if let Ok(v) = encode(&message).parse() { trailers.insert("grpc-message", v); } }
                    if let Some(w) = warning.and_then(|w| w.parse().ok()) { trailers.insert("x-modal-warning", w); }
                    let _ = send.send_trailers(trailers);
                });
            }
        });
    }

    pub fn serve() -> Aws {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let recorded = include_str!("../../cli/fixtures/aws/settings/dashboard.jsonl").lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let aws = Aws { url: format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()), recorded: Arc::new(Mutex::new(recorded)), ..Default::default() };
        let a = aws.clone();
        std::thread::spawn(move || for stream in listener.incoming().flatten() {
            if a.stop.load(std::sync::atomic::Ordering::SeqCst) { break }
            // HTTP/2 (its preface first): Modal's API over gRPC, as the CLI calls it.
            let mut first = [0u8; 3];
            if stream.peek(&mut first).is_ok_and(|n| n == 3) && &first == b"PRI" { let a = a.clone(); std::thread::spawn(move || a.serve_grpc(stream)); continue }
            let Ok((method, target, headers, body)) = read(&stream) else { continue };
            let header = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone()).unwrap_or_default();
            let host = target.split('/').nth(1).unwrap_or_default().to_string();
            // (Counted here, but what goes on to GitHub's stand-in, which counts it.)
            let s3_recorded = host.contains(".s3.") && a.recorded.lock().unwrap().iter().any(|r| r["url"] == format!("https:/{target}").as_str() && r["method"] == method.as_str());
            let to_github = ((!host.ends_with(".amazonaws.com") && !host.ends_with(".signin.aws.amazon.com")) || host.contains(".s3."))
                && !["api.cloudflare.com", "dash.cloudflare.com", "registry.cloudflare.com", "ghcr.io"].contains(&host.as_str()) && !a.workers.lock().unwrap().contains_key(&host) && !s3_recorded;
            if !to_github {
                let url = format!("https:/{target}");
                let doing = action(&header("x-amz-target"), &String::from_utf8_lossy(&body));
                match a.faults.call(format!("{method} {url} {doing}").trim_end().to_string()) {
                    Some(true) => { drop(stream); continue }
                    Some(false) => { let (status, said, answer) = sweep::failed_answer(&url, &header("x-amz-target")); let _ = write(stream, status, &said, answer.as_bytes()); continue }
                    None => {}
                }
            }
            // Cloudflare's API and its container registry, GitHub's (where its runner image is), and Modal's API (its
            // calls as the CLI sends them to a stand-in), as a recording has them (what is not there is unanswered).
            if ["api.cloudflare.com", "dash.cloudflare.com", "registry.cloudflare.com", "ghcr.io"].contains(&host.as_str()) {
                let url = format!("https:/{target}");
                let asked = String::from_utf8_lossy(&body).to_string();
                // (A Worker's secrets kept as written: what a control plane served here reads as its settings.)
                let path = url.split('?').next().unwrap_or_default().to_string();
                if let Some((script, rest)) = path.split_once("/workers/scripts/").map(|(_, r)| r.split_once('/').unwrap_or((r, ""))) {
                    let mut kept = a.worker_secrets.lock().unwrap();
                    match (method.as_str(), rest) {
                        ("PUT", "secrets") => if let Ok(v) = serde_json::from_str::<Value>(&asked) { if let (Some(n), Some(t)) = (v["name"].as_str(), v["text"].as_str()) { kept.entry(script.to_string()).or_default().insert(n.to_string(), t.to_string()); } },
                        ("DELETE", r) if r.starts_with("secrets/") => { if let Some(m) = kept.get_mut(script) { m.remove(&r["secrets/".len()..]); } }
                        ("DELETE", "") => { kept.remove(script); }
                        _ => {}
                    }
                }
                // (A Worker's secret written or taken out, with no recording of it: answered as Cloudflare answers.)
                let secret_op = path.contains("/workers/scripts/") && path.contains("/secrets") && ["PUT", "DELETE"].contains(&method.as_str());
                let (status, said, answer) = a.failed(&url, "", &asked).or_else(|| a.replay(&method, &url, "", &asked))
                    .or_else(|| secret_op.then(|| (200, vec![], json!({ "success": true, "errors": [], "messages": [], "result": null }).to_string()))).unwrap_or_else(|| {
                    a.unanswered.lock().unwrap().push(format!("{method} {url}"));
                    (404, vec![], json!({ "success": false, "errors": [{ "code": 0, "message": format!("the test kit's Cloudflare has no answer for {method} {url}") }] }).to_string())
                });
                a.asked.lock().unwrap().push(json!({ "method": method, "url": url, "target": "", "asked": asked, "status": status, "answer": answer }));
                let mut said = said;
                if !said.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) { said.push(("content-type".into(), "application/json".into())) }
                let _ = write(stream, status, &said, answer.as_bytes());
                continue;
            }
            // A Worker on workers.dev: the control plane on this computer it stands for.
            let worker = a.workers.lock().unwrap().get(&host).cloned();
            if let Some(to) = worker {
                let rest = target.splitn(3, '/').nth(2).unwrap_or_default();
                let u = to.trim_start_matches("http://");
                let (h, _) = u.split_once('/').unwrap_or((u, ""));
                let mut out = vec![];
                if let Ok(mut s) = TcpStream::connect(h) {
                    let extra: String = headers.iter().filter(|(k, _)| !["host", "content-length", "connection"].contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                    let _ = s.write_all(format!("{method} /{rest} HTTP/1.1\r\nhost: {h}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                    let _ = s.write_all(&body);
                    let _ = s.read_to_end(&mut out);
                }
                let mut stream = stream;
                let _ = stream.write_all(&out);
                continue;
            }
            // (S3's objects are kept by GitHub's stand-in, which the control plane reaches AWS through; a call to S3 a
            // recording has, at that very address, is answered as recorded.)
            let s3 = host.contains(".s3.");
            if s3 {
                let url = format!("https:/{target}");
                if let Some((status, said, answer)) = a.replay(&method, &url, "", &String::from_utf8_lossy(&body)).filter(|_| a.recorded.lock().unwrap().iter().any(|r| r["url"] == url.as_str() && r["method"] == method.as_str())) {
                    a.asked.lock().unwrap().push(json!({ "method": method, "url": url, "target": "", "asked": String::from_utf8_lossy(&body), "status": status, "answer": answer }));
                    let _ = write(stream, status, &said, answer.as_bytes());
                    continue;
                }
            }
            // (So is a control plane assuming the role it was given: GitHub's stand-in answers it as AWS documents it.)
            let sts = host.starts_with("sts.") && (target.contains("Action=AssumeRoleWithWebIdentity") || String::from_utf8_lossy(&body).contains("Action=AssumeRoleWithWebIdentity"));
            if (!host.ends_with(".amazonaws.com") && !host.ends_with(".signin.aws.amazon.com")) || s3 || sts {
                let gh = a.github.lock().unwrap().clone().unwrap_or_default();
                let mut req = Request::new(&method, &format!("https:/{target}")).with_body(body);
                for (k, v) in &headers { if !k.eq_ignore_ascii_case("host") { req = req.with_header(k, v) } }
                // (A call GitHub's stand-in drops is dropped here too.)
                let Ok(res) = block_on(superci_core::io::Http::send(&gh, req)) else { drop(stream); continue };
                let _ = write(stream, res.status, &res.headers, &res.body);
                continue;
            }
            let (status, mut said, answer) = a.handle_full(&method, &format!("https:/{target}"), &header("x-amz-target"), &String::from_utf8_lossy(&body));
            if !said.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) { said.push(("content-type".into(), if answer.starts_with('<') { "text/xml" } else { "application/x-amz-json-1.1" }.into())) }
            let _ = write(stream, status, &said, answer.as_bytes());
        });
        aws
    }

    /// What a real account answered in a situation (`fixtures/aws/planes/update.jsonl`, …), answered first from now
    /// on, with what stood for something there replaced by what it is here (a control plane's id, its address).
    pub fn replaying(&self, recording: &str, replace: &[(&str, &str)]) {
        let mut all = self.recorded.lock().unwrap();
        // (Each answer marked with its recording's place: the newest recording's answers come first.)
        let newest = all.iter().filter_map(|r| r["recording"].as_u64()).max().unwrap_or(0) + 1;
        let mut r: Vec<Value> = recording.lines().filter_map(|l| serde_json::from_str::<Value>(&replace.iter().fold(l.to_string(), |l, (from, to)| l.replace(from, to))).ok())
            .map(|mut v| { v["recording"] = newest.into(); v }).collect();
        r.extend(all.drain(..));
        *all = r;
        self.replayed.lock().unwrap().clear();
    }

    /// What Modal's settings Dicts were given, as a Modal control plane reads them (its settings; an empty value is
    /// one taken out).
    pub fn modal_plane_settings(&self) -> Vec<(String, String)> { self.overlaid(self.served_at(|h| h.ends_with(".modal.run")), self.modal_settings.lock().unwrap().iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.clone(), v.clone())).collect()) }

    /// A Worker's secrets, as a control plane there reads them (its settings).
    pub fn worker_settings(&self, script: &str) -> Vec<(String, String)> { self.overlaid(self.served_at(|h| h.starts_with(&format!("{script}."))), self.worker_secrets.lock().unwrap().get(script).map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()) }

    /// The answer `fail` set for this call, if one was (counted once more).
    fn failed(&self, url: &str, target: &str, asked: &str) -> Option<(u16, Vec<(String, String)>, String)> {
        let mut f = self.failing.lock().unwrap();
        let doing = action(target, asked);
        // (`Action|part of the address`: that action there only, `RunInstances|ec2.eu-west-1`; `DryRun=true` and the
        // like match what was asked.)
        let i = f.iter().position(|(a, ..)| a.split('|').all(|p| p == doing || url.contains(p) || asked.contains(p)))?;
        let got = (f[i].1, self.failing_headers.lock().unwrap().get(&f[i].0).cloned().unwrap_or_default(), f[i].2.clone());
        match &mut f[i].3 { Some(1) => { f.remove(i); } Some(n) => *n -= 1, None => {} }
        Some(got)
    }

    /// `action` answered with this error from now on: `times` times, or every time.
    pub fn fail(&self, action: &str, status: u16, answer: &str, times: Option<u32>) { self.failing.lock().unwrap().push((action.to_string(), status, answer.to_string(), times)) }

    /// `f` run each time `action` is asked (as `fail` matches it), before it is answered.
    pub fn during(&self, action: &str, f: impl Fn() + Send + Sync + 'static) { self.during.lock().unwrap().push((action.to_string(), Arc::new(f))) }

    /// No call answered with an error any more (each answered as before `fail`).
    pub fn fail_none(&self) { self.failing.lock().unwrap().clear(); self.failing_headers.lock().unwrap().clear() }

    /// As `fail`, the answer with these headers (a registry's `location`, say).
    pub fn fail_with_headers(&self, action: &str, status: u16, headers: &[(&str, &str)], answer: &str, times: Option<u32>) {
        self.failing_headers.lock().unwrap().insert(action.to_string(), headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        self.fail(action, status, answer, times)
    }

    /// A parameter written as the dashboard writes one: the routing and launchers of a control plane that keeps its
    /// settings (once written through it) into that record, as a write through it would (its version one more).
    pub fn put(&self, name: &str, value: &str) {
        let field = [("ROUTING", "routing"), ("LAUNCHERS", "launchers")].into_iter().find(|(k, _)| name.starts_with("/superci/") && name.ends_with(&format!("/{k}"))).map(|(_, f)| f);
        if let (Some(field), Some(store)) = (field, self.plane_store.lock().unwrap().clone()) {
            let mut kept = store.lock().unwrap();
            if let Some(mut stored) = newest_settings(&kept).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                stored[field] = serde_json::from_str(value).unwrap();
                let version = stored["version"].as_u64().unwrap() + 1;
                stored["version"] = version.into();
                kept.insert(superci_core::plane::settings_key(version), stored.to_string());
                return
            }
        }
        self.put_param(name, value)
    }

    /// A parameter written as SSM's `PutParameter` writes one.
    pub fn put_param(&self, name: &str, value: &str) {
        let mut p = self.params.lock().unwrap();
        let version = p.get(name).map(|(_, v, _)| v + 1).unwrap_or(1);
        p.insert(name.to_string(), (value.to_string(), version, system_ms()));
    }

    /// A control plane's settings, by name, as it reads them (`/superci/<id>/<NAME>`).
    pub fn settings(&self, plane: &str) -> Vec<(String, String)> {
        let prefix = format!("/superci/{plane}/");
        self.overlaid(self.plane_store.lock().unwrap().clone(), self.params.lock().unwrap().iter().filter_map(|(k, (v, _, _))| Some((k.strip_prefix(&prefix)?.to_string(), v.clone()))).filter(|(k, _)| !k.contains('/')).collect())
    }

    /// Settings as a deploy left them, with what was written through the control plane over them: what it reads.
    fn overlaid(&self, store: Option<Arc<Mutex<BTreeMap<String, String>>>>, mut kept: Vec<(String, String)>) -> Vec<(String, String)> {
        // (A control plane served here reads its cloud's copy, as a real one does: see `AS_PLANE`.)
        if AS_PLANE.with(|p| p.get()) { return kept }
        let stored = store.and_then(|s| newest_settings(&s.lock().unwrap()));
        if let Some(stored) = stored.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            for (key, name) in [("ROUTING", "routing"), ("LAUNCHERS", "launchers")] {
                kept.retain(|(k, _)| k != key);
                kept.push((key.into(), stored[name].to_string()));
            }
        }
        kept
    }

    /// The store of the control plane served for a host (a Worker, a Modal app) this stands in for, as `workers` maps
    /// it; none mapped: the one served beside it.
    fn served_at(&self, host: impl Fn(&str) -> bool) -> Option<Arc<Mutex<BTreeMap<String, String>>>> {
        let url = self.workers.lock().unwrap().iter().find(|(h, _)| host(h)).map(|(_, u)| u.clone());
        match url { Some(url) => SERVED.lock().unwrap().get(&url).cloned(), None => self.plane_store.lock().unwrap().clone() }
    }

    /// One exchange: `url` is AWS's own (`https://ssm.us-east-1.amazonaws.com/`).
    pub fn handle(&self, method: &str, url: &str, target: &str, asked: &str) -> (u16, String) { let (s, _, a) = self.handle_full(method, url, target, asked); (s, a) }

    /// The same, with the answer's headers.
    pub fn handle_full(&self, method: &str, url: &str, target: &str, asked: &str) -> (u16, Vec<(String, String)>, String) {
        let doing = action(target, asked);
        let hooks: Vec<Arc<dyn Fn() + Send + Sync>> = self.during.lock().unwrap().iter().filter(|(a, _)| a.split('|').all(|p| p == doing || url.contains(p) || asked.contains(p))).map(|(_, f)| f.clone()).collect();
        for f in hooks { f() }
        // (AWS's own public parameters, `/aws/service/…`, are not the account's: as a recording has them.)
        if let Some((status, headers, answer)) = self.failed(url, target, asked) {
            self.asked.lock().unwrap().push(json!({ "method": method, "url": url, "target": target, "asked": asked, "status": status, "answer": answer }));
            return (status, headers, answer)
        }
        let public = target == "AmazonSSM.GetParameter" && asked.contains("\"/aws/service/");
        let recorded = if public { self.replay(method, url, target, asked) } else { None };
        let (status, headers, answer) = match recorded {
            Some(r) => r,
            None => match self.answer(url, target, asked) {
                (400, a) if a["__type"] == "SuperCITestkitUnknown" => self.replay(method, url, target, asked).unwrap_or((400, vec![], a.to_string())),
                (status, a) => (status, vec![], a.to_string()),
            },
        };
        if method == "PUT" && url.contains("/functions/superci-plane-") && url.ends_with("/code") && status == 200 { *self.old_version.lock().unwrap() = None }
        if status == 400 && answer.contains("SuperCITestkitUnknown") { self.unanswered.lock().unwrap().push(format!("{target} {url} {}", asked.chars().take(200).collect::<String>())) }
        self.asked.lock().unwrap().push(json!({ "method": method, "url": url, "target": target, "asked": asked, "status": status, "answer": answer }));
        (status, headers, answer)
    }

    /// The recorded answer to the same request, else to the same action at the same address.
    fn replay(&self, method: &str, url: &str, target: &str, asked: &str) -> Option<(u16, Vec<(String, String)>, String)> {
        let recorded = self.recorded.lock().unwrap();
        let at = |r: &&Value| r["url"] == url && r["target"] == target && r["method"] == method;
        // The same call (exactly, else the same action at the same address), the next of its answers in their order.
        let same: Vec<&Value> = recorded.iter().filter(|r| at(r) && r["asked"] == asked).collect();
        let same: Vec<&Value> = if same.is_empty() { recorded.iter().filter(|r| at(r) && action(r["target"].as_str().unwrap_or_default(), r["asked"].as_str().unwrap_or_default()) == action(target, asked)).collect() } else { same };
        // Of the newest recording that has it.
        let newest = same.iter().filter_map(|r| r["recording"].as_u64()).max().unwrap_or(0);
        let same: Vec<&Value> = same.into_iter().filter(|r| r["recording"].as_u64().unwrap_or(0) == newest).collect();
        // (Of those, the ones asked most alike, by the names of what was asked: a spot request's answer for a spot
        // request, an on-demand one's for an on-demand one.)
        let names = |a: &str| -> std::collections::BTreeSet<String> { a.split('&').filter_map(|kv| kv.split('=').next()).map(|k| k.chars().filter(|c| !c.is_ascii_digit()).collect()).collect() };
        // (Only a query's: a JSON body is one name, and a control plane's code uploaded is megabytes of it.)
        let same: Vec<&Value> = if asked.contains('&') {
            let want = names(asked);
            let alike = |r: &&Value| { let n = names(r["asked"].as_str().unwrap_or_default()); n.intersection(&want).count() as i64 - n.symmetric_difference(&want).count() as i64 };
            let best = same.iter().map(alike).max().unwrap_or(0);
            same.into_iter().filter(|r| alike(r) == best).collect()
        } else { same };
        // (Counted apart for each kind of the same call: a renewal's answers in their order, a first sign-in's in theirs.)
        let kind = same.first().map(|r| names(r["asked"].as_str().unwrap_or_default()).into_iter().collect::<Vec<_>>().join(",")).unwrap_or_default();
        let key = format!("{method} {url} {target} {} {kind}", action(target, asked));
        let mut given = self.replayed.lock().unwrap();
        let n = given.entry(key).or_insert(0);
        let found = same.get(*n).or(same.last())?;
        *n += 1;
        let answer = found["answer"].as_str()?.replace("http://plane.test", &self.plane_url.lock().unwrap());
        let headers = found["headers"].as_array().into_iter().flatten().filter_map(|h| Some((h[0].as_str()?.to_string(), h[1].as_str()?.to_string()))).collect();
        Some((found["status"].as_u64()? as u16, headers, answer))
    }

    fn answer(&self, url: &str, target: &str, asked: &str) -> (u16, Value) {
        let a: Value = serde_json::from_str(asked).unwrap_or_default();
        let region = url.split('.').nth(1).unwrap_or(REGION).to_string();
        let not_found = || (400, json!({ "__type": "ParameterNotFound" }));
        // (SSM's time: seconds, with milliseconds.)
        let at = |ms: u64| Value::from(ms as f64 / 1000.0);
        let one = |name: &str, (value, version, ms): &(String, u64, u64), decrypt: bool| json!({ "ARN": format!("arn:aws:ssm:{region}:{ACCOUNT}:parameter{name}"), "DataType": "text", "LastModifiedDate": at(*ms),
            "Name": name, "Type": "SecureString", "Value": if decrypt { value.clone() } else { "EXAMPLECIPHERTEXT".to_string() }, "Version": version });
        let name = a["Name"].as_str().unwrap_or_default().to_string();
        // AWS's sign-in refusing its last step, as it answered a real one whose account was changed on its page
        // midway (15 October 2026).
        // (A recording of a sign-in that finished, and of its renewals, answers instead when one is played.)
        if url.contains(".signin.aws.amazon.com/v1/token") && !self.recorded.lock().unwrap().iter().any(|r| r["url"] == url) {
            return (400, json!({ "error": "INVALID_REQUEST", "message": "The request is missing a required parameter, includes an invalid parameter value, or is otherwise malformed." }))
        }
        // Lambda, a function called without waiting for it (the control plane's own sweep, its converter).
        if let Some(function) = url.split("/2015-03-31/functions/").nth(1).and_then(|f| f.strip_suffix("/invocations")) {
            self.invoked.lock().unwrap().push((function.to_string(), a));
            return (202, json!({}))
        }
        if let Some(op) = target.strip_prefix("DynamoDB_20120810.") { if let Some(answered) = self.dynamo(op, &a) { return answered } }
        match target {
            "AmazonSSM.PutParameter" => {
                if self.params.lock().unwrap().contains_key(&name) && a["Overwrite"] != true { return (400, json!({ "__type": "ParameterAlreadyExists" })) }
                self.put_param(&name, a["Value"].as_str().unwrap_or_default());
                (200, json!({ "Tier": "Standard", "Version": self.params.lock().unwrap()[&name].1 }))
            }
            "AmazonSSM.GetParameter" => match self.params.lock().unwrap().get(&name) {
                Some(p) => (200, json!({ "Parameter": one(&name, p, a["WithDecryption"] == true) })),
                None => not_found(),
            },
            "AmazonSSM.GetParameters" => {
                let p = self.params.lock().unwrap();
                let (mut found, mut not) = (vec![], vec![]);
                for n in a["Names"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) { match p.get(n) { Some(v) => found.push(one(n, v, a["WithDecryption"] == true)), None => not.push(n) } }
                (200, json!({ "Parameters": found, "InvalidParameters": not }))
            }
            "AmazonSSM.DeleteParameter" => match self.params.lock().unwrap().remove(&name) { Some(_) => (200, json!({})), None => not_found() },
            "AmazonSSM.DeleteParameters" => {
                let mut p = self.params.lock().unwrap();
                let (mut gone, mut not) = (vec![], vec![]);
                for n in a["Names"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) { if p.remove(n).is_some() { gone.push(n) } else { not.push(n) } }
                (200, json!({ "DeletedParameters": gone, "InvalidParameters": not }))
            }
            "AmazonSSM.GetParametersByPath" => {
                let path = a["Path"].as_str().unwrap_or_default();
                let p = self.params.lock().unwrap();
                let under: Vec<Value> = p.iter().filter(|(k, _)| k.strip_prefix(path).is_some_and(|rest| a["Recursive"] == true || !rest.contains('/'))).map(|(k, v)| one(k, v, a["WithDecryption"] == true)).collect();
                let from: usize = a["NextToken"].as_str().and_then(|t| t.strip_prefix("next-")).and_then(|n| n.parse().ok()).unwrap_or(0);
                let max = a["MaxResults"].as_u64().unwrap_or(10) as usize;
                let page: Vec<Value> = under.iter().skip(from).take(max).cloned().collect();
                let mut out = json!({ "Parameters": page });
                if from + max < under.len() { out["NextToken"] = json!(format!("next-{}", from + max)) }
                (200, out)
            }
            _ => (400, json!({ "__type": "SuperCITestkitUnknown", "message": format!("the test kit's AWS has no answer for {target} at {url}") })),
        }
    }
}

impl Aws {
    /// DynamoDB's item calls, as DynamoDB answers them (a page of a query is 25 items at most, so callers page); none
    /// for its other calls (a table made or deleted: as recorded).
    fn dynamo(&self, op: &str, a: &Value) -> Option<(u16, Value)> {
        let table = a["TableName"].as_str().unwrap_or_default().to_string();
        let key = |table: &str, k: &Value| (table.to_string(), k["p"]["S"].as_str().unwrap_or_default().to_string(), k["k"]["S"].as_str().unwrap_or_default().to_string());
        let failed = |kind: &str| (400, json!({ "__type": format!("com.amazonaws.dynamodb.v20120810#{kind}"), "message": "The conditional request failed" }));
        let mut items = self.items.lock().unwrap();
        Some(match op {
            "GetItem" => (200, items.get(&key(&table, &a["Key"])).map(|i| json!({ "Item": i })).unwrap_or(json!({}))),
            "PutItem" => {
                let at = key(&table, &a["Item"]);
                let held = items.get(&at);
                let allowed = match a["ConditionExpression"].as_str() {
                    Some("attribute_not_exists(k)") => held.is_none(),
                    Some("v = :was") => held.is_some_and(|h| h["v"] == a["ExpressionAttributeValues"][":was"]),
                    _ => true,
                };
                if !allowed { return Some(failed("ConditionalCheckFailedException")) }
                items.insert(at, a["Item"].clone());
                (200, json!({}))
            }
            "DeleteItem" => { items.remove(&key(&table, &a["Key"])); (200, json!({})) }
            "Query" => {
                let vals = &a["ExpressionAttributeValues"];
                let (p, prefix) = (vals[":p"]["S"].as_str().unwrap_or_default(), vals[":k"]["S"].as_str().unwrap_or(""));
                let mut hits: Vec<&Value> = items.iter().filter(|((t, ip, ik), _)| *t == table && ip == p && ik.starts_with(prefix)).map(|(_, i)| i).collect();
                if a["ScanIndexForward"] == false { hits.reverse() }
                let start = a["ExclusiveStartKey"]["k"]["S"].as_str().map(|after| hits.iter().position(|i| i["k"]["S"] == after).map(|x| x + 1).unwrap_or(0)).unwrap_or(0);
                let page = a["Limit"].as_u64().unwrap_or(25).min(25) as usize;
                let shown: Vec<Value> = hits.iter().skip(start).take(page).map(|i| (*i).clone()).collect();
                let mut out = json!({ "Items": shown, "Count": shown.len() });
                if start + page < hits.len() { out["LastEvaluatedKey"] = json!({ "p": { "S": p }, "k": shown.last().map(|i| i["k"].clone()).unwrap_or_default() }) }
                (200, out)
            }
            "BatchGetItem" => {
                let mut responses = serde_json::Map::new();
                for (t, ask) in a["RequestItems"].as_object().into_iter().flatten() {
                    let found: Vec<Value> = ask["Keys"].as_array().into_iter().flatten().filter_map(|k| items.get(&key(t, k)).cloned()).collect();
                    responses.insert(t.clone(), Value::Array(found));
                }
                (200, json!({ "Responses": responses, "UnprocessedKeys": {} }))
            }
            _ => return None,
        })
    }
}

/// GitHub's runner on a machine, played by the test: what it says of a job's steps, its lines as it writes them, each
/// step's whole log, and how the job ended, by the calls the runner makes (to the link its machine was given).
pub struct Runner { pub at: String }

impl Runner {
    /// The runner of the job handed out last for run `run` (its link, from the control plane's record of the run).
    pub fn of_run(served: &Served, run: &str) -> Option<Runner> {
        let r = served.store.under(&format!("run:{run}")).into_iter().next()?.1;
        let token = r["jobs"].as_array()?.iter().find_map(|j| j["token"].as_str())?.to_string();
        Some(Runner { at: format!("{}/runner/{token}/_apis/v1", served.url) })
    }
    fn post(&self, path: &str, body: Vec<u8>) -> u16 { self.exchange(path, body).0 }
    /// A call and what it answered: its status and its body.
    fn exchange(&self, path: &str, body: Vec<u8>) -> (u16, String) {
        let u = self.at.trim_start_matches("http://");
        let (host, rest) = u.split_once('/').unwrap();
        let mut s = TcpStream::connect(host).unwrap();
        s.write_all(format!("POST /{rest}{path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
        s.write_all(&body).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        (out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0), out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
    }
    /// It takes the job (asks for its message, as a runner does once it listens).
    pub fn take(&self) -> u16 { self.post("/Message/1", Vec::new()) }
    /// It asks for its next message, as a runner keeps doing while it runs its job: none (204), or one, as it came
    /// (`messageType`, and its `body`).
    pub fn message(&self) -> (u16, Value) {
        let (status, body) = self.exchange("/Message/1", Vec::new());
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
    /// Each step's record: (id, name, order, state, result, its log's number).
    pub fn steps(&self, steps: &[(&str, &str, u64, &str, Option<&str>, Option<u64>)]) -> u16 {
        let value: Vec<Value> = steps.iter().map(|(id, name, order, state, result, log)| json!({ "id": id, "type": "Task", "name": name, "order": order, "state": state, "result": result,
            "log": log.map(|n| json!({ "id": n })), "startTime": "2026-10-15T10:00:00Z", "finishTime": if *state == "completed" { json!("2026-10-15T10:00:07Z") } else { Value::Null } })).collect();
        self.post("/Timeline/scope/hub/plan/timeline", json!({ "value": value }).to_string().into_bytes())
    }
    /// Timeline records as given (the job's own, of type `Job`, beside its steps').
    pub fn records(&self, value: Vec<Value>) -> u16 { self.post("/Timeline/scope/hub/plan/timeline", json!({ "value": value }).to_string().into_bytes()) }
    /// Lines of a step as it sends them while it runs, from line `start` of the step's log.
    pub fn lines(&self, step: &str, start: u64, lines: &[String]) -> u16 {
        self.post(&format!("/TimeLineWebConsoleLog/scope/hub/plan/timeline/{step}"), json!({ "value": lines, "stepId": step, "startLine": start }).to_string().into_bytes())
    }
    /// A step's whole log, as log `n`.
    pub fn log(&self, n: u64, text: &str) -> u16 { self.post(&format!("/Logfiles/scope/hub/plan/{n}"), text.as_bytes().to_vec()) }
    /// The job is over.
    pub fn finish(&self, result: &str) -> u16 { self.post("/FinishJob/scope/hub/plan", json!({ "result": result }).to_string().into_bytes()) }

    /// The job is over, with its outputs (as its `outputs:` made them).
    pub fn finish_with(&self, result: &str, outputs: &[(&str, &str)]) -> u16 {
        let outputs: serde_json::Map<String, Value> = outputs.iter().map(|(k, v)| (k.to_string(), json!({ "value": v }))).collect();
        self.post("/FinishJob/scope/hub/plan", json!({ "result": result, "outputs": outputs }).to_string().into_bytes())
    }

    /// The runner of the job of run `run` shown as `name` (`build / compile`, `test (2)`), once it is started.
    pub fn of_job(served: &Served, run: &str, name: &str) -> Option<Runner> {
        let r = served.store.under(&format!("run:{run}")).into_iter().next()?.1;
        let token = r["jobs"].as_array()?.iter().find(|j| j["name"] == name && j["token"].is_string())?["token"].as_str()?.to_string();
        Some(Runner { at: format!("{}/runner/{token}/_apis/v1", served.url) })
    }

    /// What the runner was handed for its job (its message: steps, contexts).
    pub fn handed(&self, served: &Served) -> Value {
        let token = self.at.split("/runner/").nth(1).and_then(|r| r.split('/').next()).unwrap_or_default();
        served.store.under(&format!("rjob:{token}")).into_iter().next().map(|(_, v)| v["message"].clone()).unwrap_or_default()
    }
}

/// A control plane served as `serve` serves one, its store keeping each record's end beside it as a Durable Object's
/// or a Modal Dict's keeps it (`io::Ending`): what has passed its end goes at the upkeep's hourly `expire`. The test
/// reads the store as it is, the ends' records with the rest (`~e:`, `~x:`).
pub fn serve_with_ends(configure: impl Fn(&mut Config) + Send + 'static) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let aws = Aws::serve();
    let (store, machines) = (Shared(Default::default(), Default::default(), Default::default(), aws.faults.clone(), Default::default()), Machines(Default::default(), Default::default(), aws.faults.clone(), Default::default()));
    *aws.plane_url.lock().unwrap() = url.clone();
    *aws.plane_store.lock().unwrap() = Some(store.0.clone());
    SERVED.lock().unwrap().insert(url.clone(), store.0.clone());
    let github = GitHub::default();
    *aws.github.lock().unwrap() = Some(github.clone());
    *github.aws.lock().unwrap() = Some(aws.clone());
    let (key, run_key) = ("dashboard-key-for-tests-0001".to_string(), "superci_run_0123456789abcdef0123456789abcdef".to_string());
    aws.put(&format!("/superci/{PLANE}/DEVICE_KEY_4102444800_TEST"), &key);
    aws.put(&format!("/superci/{PLANE}/READ_KEY_4102444800_AGENT"), &format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes())));
    aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "on");
    let clock = Now::default();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let public: Arc<Mutex<Option<String>>> = Arc::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s, m, a, u, c, g, dn, pb, st) = (store.clone(), machines.clone(), aws.clone(), url.clone(), clock.clone(), github.clone(), down.clone(), public.clone(), stop.clone());
    block_on(io::Ending(store.clone()).put("plane_url", format!("\"{url}\""))).unwrap();
    std::thread::spawn(move || {
        AS_PLANE.with(|p| p.set(true));
        let cache = Cache::default();
        let ending = io::Ending(s);
        for stream in listener.incoming().flatten() {
            if st.load(std::sync::atomic::Ordering::SeqCst) { break }
            if dn.load(std::sync::atomic::Ordering::SeqCst) { drop(stream); continue }
            let mut config = Config::new(PLANE.to_string());
            let settings = a.settings(PLANE);
            config.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
            config.read_settings(&settings);
            configure(&mut config);
            let plane = ControlPlane { store: &ending, http: &g, clock: &c, timer: &NoTimer, config: &config, cache: &cache, containers: Some(&m) };
            let base = pb.lock().unwrap().clone().unwrap_or_else(|| u.clone());
            if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| answer(&plane, &base, stream, None, &a))) { a.faults.panicked(p) }
        }
    });
    let gitlab = github.gitlab.clone();
    let ends = Ends::of(&stop, &url, &aws);
    Served { plane: PLANE.to_string(), url, store, machines, aws, github, gitlab, key, run_key, clock, down, public, ends }
}

/// A server in front of `to` (a control plane, as a network or a control plane of another version stands between):
/// each request forwarded as it came, and its answer given as `answer` makes it of the request (its method and target)
/// and the answer (status and body). Its address. Each connection on its own thread.
pub fn front(to: &str, answer: impl Fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>) + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (to, answer) = (to.trim_start_matches("http://").to_string(), Arc::new(answer));
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let (to, answer) = (to.clone(), answer.clone());
        std::thread::spawn(move || {
            let Ok((method, target, headers, body)) = read(&stream) else { return };
            let mut out = vec![];
            if let Ok(mut s) = TcpStream::connect(&to) {
                let extra: String = headers.iter().filter(|(k, _)| !["host", "content-length", "connection"].contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = s.write_all(format!("{method} {target} HTTP/1.1\r\nhost: {to}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
                let _ = s.read_to_end(&mut out);
            }
            let at = out.windows(4).position(|w| w == b"\r\n\r\n").map(|a| a + 4).unwrap_or(out.len());
            let status = std::str::from_utf8(out.get(9..12).unwrap_or_default()).ok().and_then(|c| c.parse().ok()).unwrap_or(502);
            let (status, body) = answer(&method, &target, status, out[at..].to_vec());
            let _ = write(stream, status, &[("content-type".into(), "application/json".into())], &body);
        });
    });
    url
}

/// A runtime's timer that leaves work for after the answer, as AWS's function does (`Timer::later`): each with the
/// address of the request that left it (none for the upkeep's), kept until the test has it done (`Served::run_later`).
#[derive(Clone, Default)]
struct Deferring(Arc<Mutex<Vec<(Option<String>, String)>>>);
#[async_trait(?Send)]
impl Timer for Deferring {
    async fn wake_in(&self, _ms: u64) -> io::Result<()> { Ok(()) }
    fn later(&self, what: &str) -> bool { self.0.lock().unwrap().push((None, what.to_string())); true }
}

/// A control plane served as `serve` serves one, in a runtime that does some work after its answer (AWS's function:
/// a queued job's machine is asked for once GitHub has its answer, a launch template removed after its request):
/// that work is left until the test has it done (`Served::run_later`), as if it came a moment later.
pub fn serve_deferring(configure: impl Fn(&mut Config) + Send + 'static) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let aws = Aws::serve();
    let (store, machines) = (Shared(Default::default(), Default::default(), Default::default(), aws.faults.clone(), Default::default()), Machines(Default::default(), Default::default(), aws.faults.clone(), Default::default()));
    *aws.plane_url.lock().unwrap() = url.clone();
    *aws.plane_store.lock().unwrap() = Some(store.0.clone());
    SERVED.lock().unwrap().insert(url.clone(), store.0.clone());
    let github = GitHub::default();
    *aws.github.lock().unwrap() = Some(github.clone());
    *github.aws.lock().unwrap() = Some(aws.clone());
    let (key, run_key) = ("dashboard-key-for-tests-0001".to_string(), "superci_run_0123456789abcdef0123456789abcdef".to_string());
    aws.put(&format!("/superci/{PLANE}/DEVICE_KEY_4102444800_TEST"), &key);
    aws.put(&format!("/superci/{PLANE}/READ_KEY_4102444800_AGENT"), &format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes())));
    aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "on");
    let clock = Now::default();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let public: Arc<Mutex<Option<String>>> = Arc::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s, m, a, u, c, g, dn, pb, st) = (store.clone(), machines.clone(), aws.clone(), url.clone(), clock.clone(), github.clone(), down.clone(), public.clone(), stop.clone());
    block_on(store.put("plane_url", format!("\"{url}\""))).unwrap();
    std::thread::spawn(move || {
        AS_PLANE.with(|p| p.set(true));
        let (cache, left) = (Cache::default(), Deferring::default());
        for stream in listener.incoming().flatten() {
            if st.load(std::sync::atomic::Ordering::SeqCst) { break }
            if dn.load(std::sync::atomic::Ordering::SeqCst) { drop(stream); continue }
            let mut config = Config::new(PLANE.to_string());
            let settings = a.settings(PLANE);
            config.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
            config.read_settings(&settings);
            configure(&mut config);
            let plane = ControlPlane { store: &s, http: &g, clock: &c, timer: &left, config: &config, cache: &cache, containers: Some(&m) };
            let base = pb.lock().unwrap().clone().unwrap_or_else(|| u.clone());
            let Ok((method, target, headers, body)) = read(&stream) else { continue };
            if target == "/__testkit/later" {
                // (One thing done may leave another: each in its turn, with the address of what left it.)
                let mut done = vec![];
                loop {
                    let next = { let mut l = left.0.lock().unwrap(); if l.is_empty() { None } else { Some(l.remove(0)) } };
                    let Some((from, what)) = next else { break };
                    let from = from.unwrap_or_default();
                    let said = block_on(plane.later(&from, &what)).err();
                    for item in left.0.lock().unwrap().iter_mut() { if item.0.is_none() { item.0 = Some(from.clone()) } }
                    done.push(json!({ "what": what, "error": said }));
                }
                let _ = write(stream, 200, &[], Value::Array(done).to_string().as_bytes());
                continue
            }
            let (from, res) = if target == "/__testkit/upkeep" {
                (String::new(), match block_on(plane.alarm()) { Ok(()) => io::Response::new(200, "text/plain", "ok"), Err(e) => io::Response::new(500, "text/plain", e) })
            } else {
                let mut req = Request::new(&method, &format!("{base}{target}")).with_body(body);
                for (k, v) in headers { req = req.with_header(&k, &v) }
                (format!("{base}{target}"), block_on(plane.handle(req)))
            };
            for item in left.0.lock().unwrap().iter_mut() { if item.0.is_none() { item.0 = Some(from.clone()) } }
            let _ = write(stream, res.status, &res.headers, &res.body);
        }
    });
    let gitlab = github.gitlab.clone();
    let ends = Ends::of(&stop, &url, &aws);
    Served { plane: PLANE.to_string(), url, store, machines, aws, github, gitlab, key, run_key, clock, down, public, ends }
}

impl Served {
    /// The work a control plane served by `serve_deferring` left for after its answers, done now, each in its turn:
    /// what each was, and the error it ended with, if any.
    pub fn run_later(&self) -> Vec<Value> {
        let host = self.url.trim_start_matches("http://");
        let mut s = TcpStream::connect(host).unwrap();
        s.write_all(format!("POST /__testkit/later HTTP/1.1\r\nhost: {host}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out.split_once("\r\n\r\n").and_then(|(_, b)| serde_json::from_str::<Vec<Value>>(b).ok()).unwrap_or_default()
    }
}

impl Served {
    /// Its own machines refused from now on, with what their cloud said (Modal's refusal, say); `None`: started again.
    pub fn machines_refuse(&self, said: Option<&str>) { *self.machines.1.lock().unwrap() = said.map(str::to_string) }
}

/// A control plane older than some of its routes: what is asked at `url` (a control plane served here) is passed on
/// and answered as it answers, but for an address starting with one of `missing`, answered 404 as a version from
/// before that route answers. Its own address, to keep in a workspace in place of `url`.
pub fn without_routes(url: &str, missing: &[&str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (to, missing) = (url.trim_start_matches("http://").to_string(), missing.iter().map(|m| m.to_string()).collect::<Vec<_>>());
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let (to, missing) = (to.clone(), missing.clone());
        std::thread::spawn(move || {
            let Ok((method, target, headers, body)) = read(&stream) else { return };
            if missing.iter().any(|m| target.starts_with(m.as_str())) { let _ = write(stream, 404, &[("content-type".into(), "text/plain".into())], b"Not found"); return }
            let mut out = vec![];
            if let Ok(mut s) = TcpStream::connect(&to) {
                let extra: String = headers.iter().filter(|(k, _)| !["host", "content-length", "connection"].contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = s.write_all(format!("{method} {target} HTTP/1.1\r\nhost: {to}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
                let _ = s.read_to_end(&mut out);
            }
            let mut stream = stream;
            let _ = stream.write_all(&out);
        });
    });
    at
}

/// A field taken out of `v` by its path (`*`: each item or field).
fn leave_out(v: &mut Value, path: &[&str]) {
    match path {
        [] => {}
        [last] => { if let Some(o) = v.as_object_mut() { o.remove(*last); } }
        ["*", rest @ ..] => match v { Value::Array(a) => a.iter_mut().for_each(|i| leave_out(i, rest)), Value::Object(o) => o.values_mut().for_each(|i| leave_out(i, rest)), _ => {} },
        [first, rest @ ..] => if let Some(i) = v.get_mut(*first) { leave_out(i, rest) },
    }
}

impl Served {
    /// Another request writes the record this control plane is about to write over (a key under `prefix`) just
    /// before each of its next `times` writes over what it read (`Store::swap`): the record is then what `change` makes
    /// of what was there (`None`: taken out), and the write finds it changed. (`change` must not reach the store.)
    pub fn written_meanwhile(&self, prefix: &str, times: u32, change: impl Fn(Option<&str>) -> Option<String> + Send + Sync + 'static) {
        self.store.1.lock().unwrap().push((prefix.to_string(), times, Arc::new(change)))
    }

    /// From now on the store refuses to write any record under `prefix` (as a store over its limit does); `None`:
    /// writes everything again.
    pub fn store_refuses(&self, prefix: Option<&str>) {
        let mut r = self.store.2.lock().unwrap();
        match prefix { Some(p) => r.push(p.to_string()), None => r.clear() }
    }
}

/// A control plane too busy for some requests a while: what is asked at `url` (a control plane served here) is passed
/// on and answered as it answers, but the first `times` requests whose address starts with `busy` are answered 503
/// without reaching it (as a cloud answers while it is restarting it). Its own address, to keep in place of `url`.
pub fn busy_for(url: &str, busy: &str, times: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (to, busy, left) = (url.trim_start_matches("http://").to_string(), busy.to_string(), Arc::new(Mutex::new(times)));
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let (to, busy, left) = (to.clone(), busy.clone(), left.clone());
        std::thread::spawn(move || {
            let Ok((method, target, headers, body)) = read(&stream) else { return };
            if target.starts_with(busy.as_str()) {
                let mut l = left.lock().unwrap();
                if *l > 0 { *l -= 1; drop(l); let _ = write(stream, 503, &[("content-type".into(), "text/plain".into())], b"busy"); return }
            }
            let mut out = vec![];
            if let Ok(mut s) = TcpStream::connect(&to) {
                let extra: String = headers.iter().filter(|(k, _)| !["host", "content-length", "connection"].contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = s.write_all(format!("{method} {target} HTTP/1.1\r\nhost: {to}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
                let _ = s.read_to_end(&mut out);
            }
            let mut stream = stream;
            let _ = stream.write_all(&out);
        });
    });
    at
}
