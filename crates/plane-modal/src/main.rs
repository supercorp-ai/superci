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
use superci_core::plane::{Cache, Config, ControlPlane};
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
    /// A hundred at a time, read side by side by plane.py: one by one, each is a call to Modal's API, and the status a
    /// page shows asks for some 150 (over ten seconds).
    async fn get_many(&self, keys: &[String]) -> io::Result<Vec<Option<String>>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(100) {
            let a = self.ask(json!({ "op": "get_many", "keys": chunk }))?;
            let values = a["values"].as_array().filter(|v| v.len() == chunk.len()).ok_or("plane.py did not say each record asked for")?;
            out.extend(values.iter().map(|v| v.as_str().map(str::to_string)));
        }
        Ok(out)
    }
    /// What the old one Dict still holds, moved into each kind's (see `modal_plane.py`).
    async fn migrate(&self, budget: usize) -> io::Result<bool> { Ok(self.ask(json!({ "op": "migrate", "budget": budget }))?["done"] == true) }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let a = self.ask(json!({ "op": "list", "prefix": prefix }))?;
        Ok(a["items"].as_array().into_iter().flatten().filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string()))).collect())
    }
}

#[async_trait(?Send)]
impl Timer for Bridge {
    /// The schedule calls the sweep every minute; plane.py runs it only when one is due.
    async fn wake_in(&self, ms: u64) -> io::Result<()> { self.ask(json!({ "op": "wake", "at_ms": now_ms() + ms })).map(|_| ()) }
    async fn pause(&self, ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)) }
    /// plane.py holds the answer itself, once it has let go of the program: the program answers one request at a time,
    /// and a runner asking for its messages would hold every other request behind it (its job's end too).
    async fn hold(&self, _ms: u64) {}
}

/// The jobs' sandboxes, started by plane.py with Modal's client (no token needed inside Modal).
struct Sandboxes<'a>(&'a Bridge);

impl Sandboxes<'_> {
    /// One, from GitHub's runner image or an image of the control plane's own (`image`, with the Modal Secret for its
    /// registry).
    /// As a preemptible Modal Function in place of a sandbox when `notice` is given: the link it says a preemption with.
    fn sandbox(&self, name: &str, work: &Work, max_minutes: u32, size: Size, image: Option<&str>, secret: Option<&str>, notice: Option<&str>) -> io::Result<String> {
        // Placement keeps GitLab jobs (Docker) away from Modal's sandboxes.
        let (jit, fail) = match work { Work::GitHub { jit } => (jit, None), Work::Fail { jit, why } => (jit, Some(why)), Work::GitLab { .. } => return Err("Modal runs no Docker for GitLab jobs".into()) };
        let mut q = json!({ "op": "sandbox_start", "name": name, "jit": jit, "fail": fail, "max_minutes": max_minutes, "cpu": size.cpu, "ram_gb": size.ram_gb, "disk_gb": size.disk_gb, "gpu": size.gpu.map(str::to_uppercase), "image": image, "image_secret": secret });
        if let Some(n) = notice { q["function"] = true.into(); q["notice"] = n.into() }
        let a = self.0.ask(q)?;
        a["id"].as_str().map(str::to_string).ok_or_else(|| "no sandbox id".to_string())
    }
}

#[async_trait(?Send)]
impl Containers for Sandboxes<'_> {
    async fn start(&self, name: &str, work: &Work, max_minutes: u32, size: Size) -> io::Result<String> { self.sandbox(name, work, max_minutes, size, None, None, None) }
    async fn start_from(&self, name: &str, work: &Work, max_minutes: u32, size: Size, image: &str, secret: Option<&str>) -> io::Result<String> { self.sandbox(name, work, max_minutes, size, Some(image), secret, None) }
    async fn start_function(&self, name: &str, work: &Work, max_minutes: u32, size: Size, image: Option<(&str, Option<&str>)>, notice: &str) -> io::Result<String> {
        self.sandbox(name, work, max_minutes, size, image.map(|i| i.0), image.and_then(|i| i.1), Some(notice))
    }
    async fn stop(&self, id: &str) -> io::Result<()> { self.0.ask(json!({ "op": "sandbox_stop", "id": id })).map(|_| ()) }
}

/// The client: one that follows redirects, and one that gives them back as they are (asked of a request for where
/// GitHub keeps a log: the link is the answer, not what is there).
struct Blocking(ureq::Agent, ureq::Agent);

#[async_trait(?Send)]
impl Http for Blocking {
    async fn send(&self, r: Request) -> io::Result<Response> {
        // (In a test, the clouds' stand-in: see `io::stand_in`.)
        let url = io::stand_in(&r.url);
        let builder = r.headers.iter().fold(ureq::http::Request::builder().method(r.method.as_str()).uri(url.as_str()), |b, (k, v)| b.header(k.as_str(), v.as_str()));
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = if r.no_follow { &self.1 } else { &self.0 }.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        Ok(Response { status: response.status().as_u16(), headers, body: response.body_mut().read_to_vec().map_err(|e| e.to_string())? })
    }
}

struct SystemClock;
impl Clock for SystemClock { fn now_ms(&self) -> u64 { now_ms() } }

/// The control plane's settings: its id and label (from the deploy), the rest from the dashboard's Dict.
fn config(secrets: &Value) -> Config {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
    if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase() }
    c.own_cloud = "modal".into();
    c.containers = secrets["CONTAINERS"].as_str() == Some("on");
    let all: Vec<(String, String)> = secrets.as_object().into_iter().flatten().filter_map(|(n, v)| Some((n.clone(), v.as_str()?.to_string()))).collect();
    c.read_settings(&all);
    c
}

fn main() {
    let bridge = Bridge { input: RefCell::new(std::io::stdin().lock()), output: RefCell::new(std::io::stdout()) };
    let agent = |redirects: u32| ureq::Agent::config_builder().http_status_as_error(false).max_redirects(redirects).timeout_global(Some(Duration::from_secs(25))).build().into();
    let http = Blocking(agent(10), agent(0));
    let cache = Cache::default();
    while let Some(msg) = bridge.read() {
        let config = config(&msg["secrets"]);
        let sandboxes = Sandboxes(&bridge);
        let store = superci_core::io::Ending(&bridge);
        let plane = ControlPlane { store: &store, http: &http, clock: &SystemClock, timer: &bridge, config: &config, cache: &cache, containers: Some(&sandboxes) };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A GitLab job is never placed on Modal (its jobs need Docker), and were one given, nothing is asked of plane.py.
    #[test]
    fn a_gitlab_job_is_refused_a_sandbox() {
        let bridge = Bridge { input: RefCell::new(std::io::stdin().lock()), output: RefCell::new(std::io::stdout()) };
        let work = Work::GitLab { url: "https://gitlab.com".into(), token: "glrt-x".into() };
        let said = block_on(Sandboxes(&bridge).start("superci-1", &work, 10, Size { cpu: 2, ram_gb: 8, disk_gb: 14, gpu: None }));
        assert_eq!(said, Err("Modal runs no Docker for GitLab jobs".to_string()));
    }
}
