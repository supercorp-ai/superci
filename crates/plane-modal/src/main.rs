//! A SuperCI control plane on Modal. Modal runs Python: a small web endpoint (plane.py, deployed by the dashboard)
//! starts this program once per container and hands it each request, one JSON line on stdin; this answers on stdout.
//! What only Modal's Python client can do, this asks for on stdout and waits for the answer on stdin: the state (a
//! Modal Dict), wake-ups (a schedule calls the sweep), and the jobs' sandboxes (started by the control plane itself).
//! Settings and secrets (the GitHub App, dashboard keys, …) come with each message, from a second Dict the dashboard
//! writes. Requests are handled one at a time, like a Durable Object.
use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::executor::block_on;
use serde_json::{json, Value};

use superci_core::io::{self, Clock, Containers, Http, Request, Response, Store, Timer, Work};
use superci_core::plane::{Agent, Cache, Config, ControlPlane};
use superci_core::spec::Size;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// The line protocol with plane.py: a question out, its answer in.
struct Bridge { input: RefCell<std::io::StdinLock<'static>>, output: RefCell<std::io::Stdout> }

impl Bridge {
    fn send(&self, v: &Value) {
        let mut out = self.output.borrow_mut();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }
    fn read(&self) -> Option<Value> {
        let mut line = String::new();
        if self.input.borrow_mut().read_line(&mut line).ok()? == 0 { return None }
        serde_json::from_str(&line).ok()
    }
    /// Asks plane.py for something only Modal's client can do.
    fn ask(&self, mut q: Value) -> io::Result<Value> {
        q["type"] = "ask".into();
        self.send(&q);
        let a = self.read().ok_or("plane.py went away")?;
        match a["error"].as_str() { Some(e) => Err(e.to_string()), None => Ok(a) }
    }
}

#[async_trait(?Send)]
impl Store for Bridge {
    async fn get(&self, key: &str) -> io::Result<Option<String>> { Ok(self.ask(json!({ "op": "get", "key": key }))?["value"].as_str().map(str::to_string)) }
    async fn put(&self, key: &str, value: String) -> io::Result<()> { self.ask(json!({ "op": "put", "key": key, "value": value })).map(|_| ()) }
    async fn put_if_absent(&self, key: &str, value: String) -> io::Result<bool> {
        Ok(self.ask(json!({ "op": "put_if_absent", "key": key, "value": value }))?["created"] == true)
    }
    async fn delete(&self, key: &str) -> io::Result<()> { self.ask(json!({ "op": "delete", "key": key })).map(|_| ()) }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let a = self.ask(json!({ "op": "list", "prefix": prefix }))?;
        Ok(a["items"].as_array().into_iter().flatten().filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string()))).collect())
    }
}

#[async_trait(?Send)]
impl Timer for Bridge {
    /// The schedule calls the sweep every minute; plane.py runs it only when one is due.
    async fn wake_in(&self, ms: u64) -> io::Result<()> { self.ask(json!({ "op": "wake", "at_ms": now_ms() + ms })).map(|_| ()) }
}

/// The jobs' sandboxes, started by plane.py with Modal's client (no token needed inside Modal).
struct Sandboxes<'a>(&'a Bridge);

#[async_trait(?Send)]
impl Containers for Sandboxes<'_> {
    async fn start(&self, name: &str, work: &Work, max_minutes: u32, size: Size) -> io::Result<String> {
        // Placement keeps GitLab jobs (Docker) away from Modal's sandboxes.
        let (jit, fail) = match work { Work::GitHub { jit } => (jit, None), Work::Fail { jit, why } => (jit, Some(why)), Work::GitLab { .. } => return Err("Modal runs no Docker for GitLab jobs".into()) };
        let a = self.0.ask(json!({ "op": "sandbox_start", "name": name, "jit": jit, "fail": fail, "max_minutes": max_minutes, "cpu": size.cpu, "ram_gb": size.ram_gb, "disk_gb": size.disk_gb, "gpu": size.gpu.map(str::to_uppercase) }))?;
        a["id"].as_str().map(str::to_string).ok_or_else(|| "no sandbox id".to_string())
    }
    async fn stop(&self, id: &str) -> io::Result<()> { self.0.ask(json!({ "op": "sandbox_stop", "id": id })).map(|_| ()) }
}

struct Blocking(ureq::Agent);

#[async_trait(?Send)]
impl Http for Blocking {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let mut builder = ureq::http::Request::builder().method(r.method.as_str()).uri(r.url.as_str());
        for (k, v) in &r.headers { builder = builder.header(k.as_str(), v.as_str()) }
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = self.0.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        Ok(Response { status: response.status().as_u16(), headers, body: response.body_mut().read_to_vec().map_err(|e| e.to_string())? })
    }
}

struct SystemClock;
impl Clock for SystemClock { fn now_ms(&self) -> u64 { now_ms() } }

/// The control plane's settings: its id and label (from the deploy), the rest from the dashboard's Dict.
fn config(secrets: &Value) -> Config {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    let secret = |n: &str| secrets[n].as_str();
    let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
    if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase() }
    if let Some(v) = secret("LABELS") { c.set_labels(v) }
    if let Some(v) = secret("NAME") { c.set_name(v) }
    c.own_cloud = "modal".into();
    c.containers = secret("CONTAINERS") == Some("on");
    c.app = secret("GITHUB_APP").and_then(|v| serde_json::from_str(v).ok());
    // One entry for each further organization's GitHub App: GITHUB_APP_<its id> (emptied when it is removed).
    c.more_apps = secrets.as_object().into_iter().flatten().filter(|(n, _)| n.starts_with("GITHUB_APP_"))
        .filter_map(|(_, v)| serde_json::from_str::<superci_core::github::App>(v.as_str()?).ok())
        .filter(|a| c.app.as_ref().is_none_or(|f| f.id != a.id)).collect();
    c.agents = secret("AGENTS").and_then(|v| serde_json::from_str::<Vec<Agent>>(v).ok()).unwrap_or_default();
    c.routing = secret("ROUTING").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    c.gitlab = secret("GITLAB").and_then(|v| serde_json::from_str(v).ok());
    // One entry for each further GitLab connection: GITLAB_<its name> (emptied when it is removed).
    c.more_gitlabs = secrets.as_object().into_iter().flatten().filter(|(n, _)| n.starts_with("GITLAB_"))
        .filter_map(|(_, v)| serde_json::from_str::<superci_core::gitlab::GitLab>(v.as_str()?).ok()).filter(|g| superci_core::gitlab::valid_id(&g.id)).collect();
    c.move_token = secret("MOVE_TOKEN").map(str::to_string);
    if let Some(l) = secret("CF_LOCATION") { c.cloudflare_location = l.to_string() }
    c.cloudflare_image = secret("CF_IMAGE").and_then(superci_core::plane::image_address);
    c.machine = secret("MACHINE").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    // One entry per dashboard session: DASHBOARD_KEY_<expires, unix seconds>_<random>.
    c.dashboard_keys = secrets.as_object().into_iter().flatten()
        .filter_map(|(n, v)| Some((n.strip_prefix("DASHBOARD_KEY_")?.split('_').next()?.parse::<u64>().ok()? * 1000, v.as_str()?.to_string()))).collect();
    // One entry per key that only reads: READ_KEY_<expires, unix seconds>_<NAME>, holding the key's SHA-256.
    c.read_keys = secrets.as_object().into_iter().flatten().filter_map(|(n, v)| superci_core::plane::read_key(n, v.as_str()?)).collect();
    c
}

fn main() {
    let bridge = Bridge { input: RefCell::new(std::io::stdin().lock()), output: RefCell::new(std::io::stdout()) };
    let http = Blocking(ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(25))).build().into());
    let cache = Cache::default();
    while let Some(msg) = bridge.read() {
        let config = config(&msg["secrets"]);
        let sandboxes = Sandboxes(&bridge);
        let plane = ControlPlane { store: &bridge, http: &http, clock: &SystemClock, timer: &bridge, config: &config, cache: &cache, containers: Some(&sandboxes) };
        if msg["kind"] == "alarm" {
            let r = block_on(plane.alarm());
            if let Err(e) = &r { eprintln!("superci: {e}") }
            bridge.send(&json!({ "type": "done", "error": r.err() }));
            continue;
        }
        let mut r = Request::new(msg["method"].as_str().unwrap_or("GET"), msg["url"].as_str().unwrap_or("/")).with_body(STANDARD.decode(msg["body"].as_str().unwrap_or_default()).unwrap_or_default());
        for (k, v) in msg["headers"].as_object().into_iter().flatten() { if let Some(v) = v.as_str() { r.headers.push((k.to_ascii_lowercase(), v.to_string())) } }
        let response = block_on(plane.handle(r));
        let headers: serde_json::Map<String, Value> = response.headers.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
        bridge.send(&json!({ "type": "response", "status": response.status, "headers": headers, "body": STANDARD.encode(&response.body) }));
    }
}
