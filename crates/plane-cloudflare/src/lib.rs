//! A SuperCI control plane on Cloudflare: the Worker forwards every request to one Durable Object, whose storage, alarms and
//! outbound fetch back the runtime-neutral control plane (superci-core). With Cloudflare runners added, it also starts
//! the jobs' containers itself: one `JobRunner` Durable Object with a container per job, sized for it (no separate launcher).
use async_trait::async_trait;
use superci_core::plane::{Cache, Config, ControlPlane};
use superci_core::io::{self, Clock, Containers, Http, Store, Timer, Work, Ending};
use superci_core::spec::Size;
use wasm_bindgen::{JsCast, JsValue};
use worker::*;

// (The jobs' runners, as the launcher's Worker has them.)
#[path = "../../runners-cloudflare/src/job_runner.rs"]
mod job_runner;
pub use job_runner::{JobRunner, Runner};
use job_runner::is_object_id;

/// The header by which the Worker vouches for a dashboard session's key (with until when it holds, ms). The Worker
/// sees a new key's secret within seconds; the Durable Object keeps the secrets it started with until it restarts.
/// One from outside is dropped: only the Worker sets it.
const VOUCHED: &str = "x-superci-key-until";

/// The header by which the Worker hands the Durable Object the settings as they are now (it reads them fresh; the
/// Durable Object keeps the ones it started with until it restarts, which can take half a minute after a change).
/// One from outside is dropped: only the Worker sets it.
const SETTINGS: &str = "x-superci-settings";
const SETTING_NAMES: [&str; 16] = ["LABELS", "NAME", "GITHUB_APP", "GITLAB", "LAUNCHERS", "ROUTING", "MACHINE", "CONTAINERS", "AWS_CONNECT", "AWS_REGIONS", "AWS_NETWORKS", "AWS_RUNNERS", "CF_LOCATION", "CF_IMAGE", "MOVE_TOKEN", "INSTANCE_TYPES"];

/// JSON with every character outside ASCII escaped, so it can travel in a header.
fn ascii_json(v: &serde_json::Value) -> String {
    v.to_string().chars().map(|c| if c.is_ascii() { c.to_string() } else { c.encode_utf16(&mut [0; 2]).iter().map(|u| format!("\\u{u:04x}")).collect() }).collect()
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let stub = env.durable_object("PLANE")?.id_from_name("plane").expect("a Durable Object named").get_stub().expect("a Durable Object's stub");
    // (Headers named and valued here, all ASCII: always taken.)
    let header = |r: Result<()>| r.expect("a header of the Worker's own");
    let headers = req.headers().clone();
    header(headers.delete(VOUCHED));
    // Only the Worker says what the settings are: anything of that name from outside is dropped.
    headers.keys().filter(|k| k.starts_with(SETTINGS)).collect::<Vec<_>>().iter().for_each(|name| header(headers.delete(name)));
    let mut settings: serde_json::Map<String, serde_json::Value> = SETTING_NAMES.iter().filter_map(|n| Some((n.to_string(), env.secret(n).ok()?.to_string().into()))).collect();
    // One secret for each further organization's GitHub App (GITHUB_APP_<its id>) and GitLab connection; and those
    // kept for runs from a computer (SECRET_<name>_<scope>), only where they are read (a run made, their list, a move).
    let path = req.path();
    let kept = path == "/runs" || path == "/secrets" || path == "/move/export";
    for n in js_sys::Object::keys(env.unchecked_ref::<js_sys::Object>()).iter().filter_map(|n| n.as_string()).filter(|n| n.starts_with("GITHUB_APP_") || n.starts_with("GITLAB_") || (kept && n.starts_with("SECRET_"))) {
        if let Ok(v) = env.secret(&n) { settings.insert(n, v.to_string().into()); }
    }
    // In pieces: a header's value has a size limit, and each further organization's App adds its key.
    let all = ascii_json(&serde_json::Value::Object(settings));
    all.as_bytes().chunks(8_000).enumerate().for_each(|(i, piece)| header(headers.set(&format!("{SETTINGS}-{i}"), &String::from_utf8_lossy(piece))));
    let bearer = headers.get("authorization").expect("a header's name").and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)).unwrap_or_default();
    if bearer.len() >= 16 {
        let now = js_sys::Date::now() as u64;
        if let Some((until, _)) = device_keys(&env).into_iter().find(|(until, k)| *until > now && superci_core::crypto::safe_eq(k.as_bytes(), bearer.as_bytes())) {
            header(headers.set(VOUCHED, &until.to_string()));
        }
    }
    let mut init = RequestInit::new();
    init.with_method(req.method()).with_headers(headers);
    if req.method() != Method::Get && req.method() != Method::Head {
        // (A body that broke on its way in: handed on as it came, empty.)
        init.with_body(Some(js_sys::Uint8Array::from(req.bytes().await.unwrap_or_default().as_slice()).into()));
    }
    stub.fetch_with_request(Request::new_with_init(&req.url().expect("a request's own URL").to_string(), &init).expect("the same request")).await
}

/// The dashboard sessions' keys: one secret each, DEVICE_KEY_<expires, unix seconds>_<random>.
fn device_keys(env: &Env) -> Vec<(u64, String)> {
    let names = js_sys::Object::keys(env.unchecked_ref::<js_sys::Object>());
    names.iter().filter_map(|n| n.as_string()).filter_map(|n| {
        let until: u64 = n.strip_prefix("DEVICE_KEY_")?.split('_').next().expect("a split's first piece").parse().ok()?;
        Some((until * 1000, env.secret(&n).ok()?.to_string()))
    }).collect()
}

#[durable_object]
pub struct PlaneObject {
    state: State,
    env: Env,
    cache: Cache,
}

struct DoStore<'a>(&'a State);
struct FetchHttp;
struct WorkerClock;
struct DoTimer<'a>(&'a State);
/// The jobs' containers, one `JobRunner` Durable Object each (named by the runner), placed in eastern North America,
/// near GitHub.
/// The Worker's bindings, where containers start, and where GitHub's full image is published (if set).
struct Runners<'a>(&'a Env, String, Option<String>);

#[async_trait(?Send)]
impl Containers for Runners<'_> {
    async fn start(&self, name: &str, work: &Work, max_minutes: u32, size: Size) -> io::Result<String> {
        let mut body = serde_json::json!({ "max_minutes": max_minutes, "cpu": size.cpu, "ram_gb": size.ram_gb, "disk_gb": size.disk_gb });
        body.as_object_mut().expect("an object, just made").extend(work.json().as_object().cloned().unwrap_or_default());
        if let Some(i) = &self.2 { body["image_url"] = i.clone().into() }
        // Its id is its Durable Object's: Cloudflare's usage analytics count each container by it (`instanceId`).
        self.call(name, "start", body).await
    }
    async fn stop(&self, id: &str) -> io::Result<()> { self.call(id, "stop", serde_json::json!({})).await.map(|_| ()) }
}

impl Runners<'_> {
    /// One runner's Durable Object, by its name or (once started) its id; the object's id.
    async fn call(&self, name: &str, what: &str, body: serde_json::Value) -> io::Result<String> {
        let namespace = self.0.durable_object("RUNNER").map_err(|e| e.to_string())?;
        // Where its container starts (a hint, used when the object is made): as set, or where Cloudflare chooses.
        let location = &self.1;
        // (By its id only once its start made it: an id of this namespace's. A moved control plane brings only jobs that
        // ended.)
        let by_id = is_object_id(name);
        let object = if by_id { namespace.id_from_string(name) } else { namespace.id_from_name(name) }.expect("a Durable Object named, or one of this namespace's");
        let id = object.to_string();
        let stub = if location == "auto" || by_id { object.get_stub().expect("a Durable Object's stub") } else { object.get_stub_with_location_hint(location).expect("a stub where Cloudflare has a location (as its settings are read)") };
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_body(Some(JsValue::from_str(&body.to_string())));
        let req = Request::new_with_init(&format!("https://runner/{what}"), &init).expect("a request to a runner");
        let mut r = stub.fetch_with_request(req).await.map_err(|e| e.to_string())?;
        if r.status_code() >= 300 { return Err(format!("runner container: {} {}", r.status_code(), r.text().await.unwrap_or_default())); }
        Ok(id)
    }
}

#[async_trait(?Send)]
impl Store for DoStore<'_> {
    async fn get(&self, key: &str) -> io::Result<Option<String>> {
        self.0.storage().get::<String>(key).await.map_err(|e| e.to_string())
    }
    async fn put(&self, key: &str, value: String) -> io::Result<()> {
        self.0.storage().put(key, value).await.map_err(|e| e.to_string())
    }
    /// A Durable Object runs one event at a time between storage calls, so read-then-write is atomic here.
    async fn put_if_absent(&self, key: &str, value: String) -> io::Result<bool> {
        match self.get(key).await { Ok(None) => self.put(key, value).await.map(|_| true), read => read.map(|_| false) }
    }
    async fn delete(&self, key: &str) -> io::Result<()> {
        self.0.storage().delete(key).await.map(|_| ()).map_err(|e| e.to_string())
    }
    /// The last `n` by key, read from the end (the storage keeps keys in order).
    async fn list_last(&self, prefix: &str, n: usize) -> io::Result<Vec<(String, String)>> {
        let map = self.0.storage().list_with_options(ListOptions::new().prefix(prefix).reverse(true).limit(n)).await.map_err(|e| e.to_string())?;
        let mut out = pairs(&map);
        out.reverse();
        Ok(out)
    }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let map = self.0.storage().list_with_options(ListOptions::new().prefix(prefix)).await.map_err(|e| e.to_string())?;
        Ok(pairs(&map))
    }
}

/// A listing's keys and values, in its order (all strings: what this store puts).
fn pairs(map: &js_sys::Map) -> Vec<(String, String)> {
    let mut out = vec![];
    map.for_each(&mut |v, k| out.push((k.as_string().expect("a key"), v.as_string().expect("a value this store put"))));
    out
}

#[async_trait(?Send)]
impl Http for FetchHttp {
    async fn send(&self, r: io::Request) -> io::Result<io::Response> {
        let headers = Headers::new();
        r.headers.iter().try_for_each(|(k, v)| headers.append(k, v)).map_err(|e| e.to_string())?;
        let mut init = RequestInit::new();
        init.with_method(Method::from(r.method.clone())).with_headers(headers);
        // One request asks for its redirect back (a job's log: GitHub answers with a signed link on another site,
        // to be fetched without the token); every other is followed by the runtime, as before.
        if r.no_follow { init.with_redirect(RequestRedirect::Manual); }
        if !r.body.is_empty() { init.with_body(Some(js_sys::Uint8Array::from(r.body.as_slice()).into())); }
        let req = Request::new_with_init(&r.url, &init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;
        let status = resp.status_code();
        let headers = resp.headers().entries().collect();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        Ok(io::Response { status, headers, body })
    }
}

impl Clock for WorkerClock {
    fn now_ms(&self) -> u64 { Date::now().as_millis() }
}

#[async_trait(?Send)]
impl Timer for DoTimer<'_> {
    async fn wake_in(&self, ms: u64) -> io::Result<()> {
        let storage = self.0.storage();
        let due = Date::now().as_millis() + ms;
        if let Ok(Some(at)) = storage.get_alarm().await { if (at as u64) <= due { return Ok(()); } }
        storage.set_alarm(std::time::Duration::from_millis(ms)).await.map_err(|e| e.to_string())
    }
    async fn pause(&self, ms: u64) { worker::Delay::from(std::time::Duration::from_millis(ms)).await }
}

impl PlaneObject {
    /// The control plane's settings (`PLANE_ID`, `LABEL`) and the secrets the dashboard sets from the user's machine,
    /// read as every control plane reads its settings (`Config::read_settings`): as the Worker handed them with this
    /// request (fresh), else as this object started with them. Its keys are always its own (the Worker hands none).
    fn config(&self, fresh: Option<&std::collections::BTreeMap<String, String>>) -> Config {
        let var = |n: &str| self.env.var(n).map(|v| v.to_string()).ok().filter(|v| !v.is_empty());
        let names: Vec<String> = js_sys::Object::keys(self.env.unchecked_ref::<js_sys::Object>()).iter().filter_map(|n| n.as_string()).collect();
        let own = |n: &String| self.env.secret(n).ok().map(|v| (n.clone(), v.to_string()));
        let key = |n: &str| n.starts_with("DEVICE_KEY_") || n.starts_with("READ_KEY_");
        let mut all: Vec<(String, String)> = match fresh {
            Some(f) => f.iter().filter(|(k, _)| !key(k)).map(|(k, v)| (k.clone(), v.clone())).collect(),
            None => names.iter().filter(|n| !key(n)).filter_map(own).collect(),
        };
        all.extend(names.iter().filter(|n| key(n)).filter_map(own));
        let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
        if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase(); }
        if let Some(v) = var("INSTANCE_TYPES") { c.instance_types = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(); }
        c.read_settings(&all);
        let setting = |n: &str| all.iter().find(|(k, v)| k == n && !v.is_empty()).map(|(_, v)| v.as_str());
        c.containers = setting("CONTAINERS") == Some("on");
        // (AWS_CONNECT: the region and one-time token the dashboard gives an AWS account's connecting.)
        if let Some(v) = setting("AWS_CONNECT").and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok()) {
            c.aws_region = v["region"].as_str().map(str::to_string);
            c.aws_connect_token = v["token"].as_str().map(str::to_string);
        }
        c
    }
}

impl DurableObject for PlaneObject {
    fn new(state: State, env: Env) -> Self {
        PlaneObject { state, env, cache: Cache::default() }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let body = if req.method() == Method::Get || req.method() == Method::Head { vec![] } else { req.bytes().await.unwrap_or_default() };
        let request = io::Request { method: req.method().to_string(), url: req.url().expect("a request's own URL").to_string(), headers: req.headers().entries().collect(), body, no_follow: false };
        let header = |n: &str| req.headers().get(n).expect("a header's name");
        let all: String = (0..).map_while(|i| header(&format!("{SETTINGS}-{i}"))).collect();
        let fresh = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&all).ok();
        let mut config = self.config(fresh.as_ref());
        // A key the Worker vouched for (it reads the secrets fresh).
        if let (Some(until), Some(key)) = (header(VOUCHED).and_then(|u| u.parse::<u64>().ok()), header("authorization").and_then(|a| a.strip_prefix("Bearer ").map(str::to_string))) {
            config.device_keys.push((until, key));
        }
        let (store, timer) = (Ending(DoStore(&self.state)), DoTimer(&self.state));
        let plane = ControlPlane { store: &store, http: &FetchHttp, clock: &WorkerClock, timer: &timer, config: &config, cache: &self.cache, containers: Some(&Runners(&self.env, config.cloudflare_location.clone(), config.cloudflare_image.clone())) };
        let r = plane.handle(request).await;
        let headers = Headers::new();
        r.headers.iter().for_each(|(k, v)| headers.append(k, v).expect("a header the control plane answers with"));
        Ok(Response::from_bytes(r.body).expect("a body of bytes").with_status(r.status).with_headers(headers))
    }

    async fn alarm(&self) -> Result<Response> {
        let config = self.config(None);
        let (store, timer) = (Ending(DoStore(&self.state)), DoTimer(&self.state));
        let plane = ControlPlane { store: &store, http: &FetchHttp, clock: &WorkerClock, timer: &timer, config: &config, cache: &self.cache, containers: Some(&Runners(&self.env, config.cloudflare_location.clone(), config.cloudflare_image.clone())) };
        plane.alarm().await.map_err(Error::RustError)?;
        Response::ok("ok")
    }
}
