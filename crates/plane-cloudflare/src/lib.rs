//! A SuperCI control plane on Cloudflare: the Worker forwards every request to one Durable Object, whose storage, alarms and
//! outbound fetch back the runtime-neutral control plane (superci-core). With Cloudflare runners added, it also starts
//! the jobs' containers itself: one `JobRunner` Durable Object with a container per job, sized for it (no separate agent).
use async_trait::async_trait;
use superci_core::plane::{Cache, Config, ControlPlane};
use superci_core::io::{self, Clock, Containers, Http, Store, Timer, Work};
use superci_core::spec::Size;
use js_sys::{Array, Function, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use worker::*;

/// The header by which the Worker vouches for a dashboard session's key (with until when it holds, ms). The Worker
/// sees a new key's secret within seconds; the Durable Object keeps the secrets it started with until it restarts.
/// One from outside is dropped: only the Worker sets it.
const VOUCHED: &str = "x-superci-key-until";

/// The header by which the Worker hands the Durable Object the settings as they are now (it reads them fresh; the
/// Durable Object keeps the ones it started with until it restarts, which can take half a minute after a change).
/// One from outside is dropped: only the Worker sets it.
const SETTINGS: &str = "x-superci-settings";
const SETTING_NAMES: [&str; 13] = ["GITHUB_APP", "GITLAB", "AGENTS", "ROUTING", "MACHINE", "CONTAINERS", "AWS_CONNECT", "AWS_REGIONS", "AWS_NETWORKS", "CF_LOCATION", "CF_IMAGE", "MOVE_TOKEN", "INSTANCE_TYPES"];

/// JSON with every character outside ASCII escaped, so it can travel in a header.
fn ascii_json(v: &serde_json::Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() { out.push(c) } else { for u in c.encode_utf16(&mut [0; 2]) { out.push_str(&format!("\\u{:04x}", u)) } }
    }
    out
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let stub = env.durable_object("PLANE")?.id_from_name("plane")?.get_stub()?;
    let headers = req.headers().clone();
    headers.delete(VOUCHED)?;
    // Only the Worker says what the settings are: anything of that name from outside is dropped.
    for name in headers.keys().filter(|k| k.starts_with(SETTINGS)).collect::<Vec<_>>() { headers.delete(&name)? }
    let mut settings = serde_json::Map::new();
    for n in SETTING_NAMES { if let Ok(v) = env.secret(n) { settings.insert(n.into(), v.to_string().into()); } }
    // One secret for each further organization's GitHub App: GITHUB_APP_<its id>.
    for n in js_sys::Object::keys(env.unchecked_ref::<js_sys::Object>()).iter().filter_map(|n| n.as_string()).filter(|n| n.starts_with("GITHUB_APP_") || n.starts_with("GITLAB_")) {
        if let Ok(v) = env.secret(&n) { settings.insert(n, v.to_string().into()); }
    }
    // In pieces: a header's value has a size limit, and each further organization's App adds its key.
    let all = ascii_json(&serde_json::Value::Object(settings));
    for (i, piece) in all.as_bytes().chunks(8_000).enumerate() { headers.set(&format!("{SETTINGS}-{i}"), &String::from_utf8_lossy(piece))? }
    let bearer = headers.get("authorization")?.and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)).unwrap_or_default();
    if bearer.len() >= 16 {
        let now = js_sys::Date::now() as u64;
        if let Some((until, _)) = dashboard_keys(&env).into_iter().find(|(until, k)| *until > now && superci_core::crypto::safe_eq(k.as_bytes(), bearer.as_bytes())) {
            headers.set(VOUCHED, &until.to_string())?;
        }
    }
    let mut init = RequestInit::new();
    init.with_method(req.method()).with_headers(headers);
    if req.method() != Method::Get && req.method() != Method::Head {
        init.with_body(Some(js_sys::Uint8Array::from(req.bytes().await?.as_slice()).into()));
    }
    stub.fetch_with_request(Request::new_with_init(&req.url()?.to_string(), &init)?).await
}

/// The GitHub Apps of further organizations, from their secrets' values (one that is the first App again, or not an
/// App, is left out).
fn more_apps(values: impl Iterator<Item = String>, first: Option<&superci_core::github::App>) -> Vec<superci_core::github::App> {
    let mut apps: Vec<superci_core::github::App> = values.filter_map(|v| serde_json::from_str(&v).ok()).collect();
    apps.retain(|a: &superci_core::github::App| first.is_none_or(|f| f.id != a.id));
    apps.sort_by_key(|a| a.id);
    apps.dedup_by_key(|a| a.id);
    apps
}

/// The dashboard sessions' keys: one secret each, DASHBOARD_KEY_<expires, unix seconds>_<random>.
fn dashboard_keys(env: &Env) -> Vec<(u64, String)> {
    let names = js_sys::Object::keys(env.unchecked_ref::<js_sys::Object>());
    names.iter().filter_map(|n| n.as_string()).filter_map(|n| {
        let until: u64 = n.strip_prefix("DASHBOARD_KEY_")?.split('_').next()?.parse().ok()?;
        Some((until * 1000, env.secret(&n).ok()?.to_string()))
    }).collect()
}

/// Keys that only read: one secret each, READ_KEY_<expires, unix seconds>_<NAME>, holding the key's SHA-256.
fn read_keys(env: &Env) -> Vec<(u64, String, String)> {
    let names = js_sys::Object::keys(env.unchecked_ref::<js_sys::Object>());
    names.iter().filter_map(|n| n.as_string()).filter(|n| n.starts_with("READ_KEY_")).filter_map(|n| superci_core::plane::read_key(&n, &env.secret(&n).ok()?.to_string())).collect()
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
        for (k, v) in work.json().as_object().into_iter().flatten() { body[k] = v.clone() }
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
        let object = if is_object_id(name) { namespace.id_from_string(name) } else { namespace.id_from_name(name) }.map_err(|e| e.to_string())?;
        let id = object.to_string();
        let stub = if location == "auto" || is_object_id(name) { object.get_stub() } else { object.get_stub_with_location_hint(location) }.map_err(|e| e.to_string())?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_body(Some(JsValue::from_str(&body.to_string())));
        let req = Request::new_with_init(&format!("https://runner/{what}"), &init).map_err(|e| e.to_string())?;
        let mut r = stub.fetch_with_request(req).await.map_err(|e| e.to_string())?;
        if r.status_code() >= 300 { return Err(format!("runner container: {} {}", r.status_code(), r.text().await.unwrap_or_default())); }
        Ok(id)
    }
}

/// A Durable Object's id (64 hex digits), not a name.
fn is_object_id(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) }

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
        if self.get(key).await?.is_some() { return Ok(false); }
        self.put(key, value).await.map(|_| true)
    }
    async fn delete(&self, key: &str) -> io::Result<()> {
        self.0.storage().delete(key).await.map(|_| ()).map_err(|e| e.to_string())
    }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let map = self.0.storage().list_with_options(ListOptions::new().prefix(prefix)).await.map_err(|e| e.to_string())?;
        let mut out = vec![];
        for entry in map.entries() {
            let pair: js_sys::Array = entry.map_err(|e| format!("{e:?}"))?.into();
            if let (Some(k), Some(v)) = (pair.get(0).as_string(), pair.get(1).as_string()) { out.push((k, v)); }
        }
        Ok(out)
    }
}

#[async_trait(?Send)]
impl Http for FetchHttp {
    async fn send(&self, r: io::Request) -> io::Result<io::Response> {
        let headers = Headers::new();
        for (k, v) in &r.headers { headers.append(k, v).map_err(|e| e.to_string())?; }
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
}

impl PlaneObject {
    /// The control plane's settings (`PLANE_ID`, `LABEL`) and the secrets the dashboard sets from the user's machine:
    /// `GITHUB_APP` (the App's id, slug, owner, key and webhook secret) and `AWS_CONNECT` (region and one-time token).
    /// The settings: as the Worker handed them with this request (fresh), else as this object started with.
    fn config(&self, fresh: Option<&serde_json::Value>) -> Config {
        let var = |n: &str| self.env.var(n).map(|v| v.to_string()).ok().filter(|v| !v.is_empty());
        let secret = |n: &str| match fresh {
            Some(f) => f[n].as_str().map(str::to_string),
            None => self.env.secret(n).map(|v| v.to_string()).ok(),
        }.filter(|v| !v.is_empty());
        let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
        if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase(); }
        if let Some(v) = var("INSTANCE_TYPES") { c.instance_types = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(); }
        c.dashboard_keys = dashboard_keys(&self.env);
        c.read_keys = read_keys(&self.env);
        c.agents = secret("AGENTS").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        c.routing = secret("ROUTING").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        c.gitlab = secret("GITLAB").and_then(|v| serde_json::from_str(&v).ok());
        c.machine = secret("MACHINE").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        c.containers = secret("CONTAINERS").is_some_and(|v| v == "on");
        c.app = secret("GITHUB_APP").and_then(|v| serde_json::from_str(&v).ok());
        let names: Vec<String> = match fresh {
            Some(f) => f.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
            None => js_sys::Object::keys(self.env.unchecked_ref::<js_sys::Object>()).iter().filter_map(|n| n.as_string()).collect(),
        };
        c.more_apps = more_apps(names.iter().filter(|n| n.starts_with("GITHUB_APP_")).filter_map(|n| secret(n)), c.app.as_ref());
        // One secret for each further GitLab connection: GITLAB_<its name>.
        c.more_gitlabs = names.iter().filter(|n| n.starts_with("GITLAB_")).filter_map(|n| secret(n)).filter_map(|v| serde_json::from_str::<superci_core::gitlab::GitLab>(&v).ok())
            .filter(|g| superci_core::gitlab::valid_id(&g.id)).collect();
        if let Some(v) = secret("AWS_CONNECT").and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok()) {
            c.aws_region = v["region"].as_str().map(str::to_string);
            c.aws_connect_token = v["token"].as_str().map(str::to_string);
        }
        c.move_token = secret("MOVE_TOKEN");
        if let Some(l) = secret("CF_LOCATION") { c.cloudflare_location = l }
        c.cloudflare_image = secret("CF_IMAGE").and_then(|i| superci_core::plane::image_address(&i));
        c.aws_regions = secret("AWS_REGIONS").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        c.aws_networks = secret("AWS_NETWORKS").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
        c
    }
}

impl DurableObject for PlaneObject {
    fn new(state: State, env: Env) -> Self {
        PlaneObject { state, env, cache: Cache::default() }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let body = if req.method() == Method::Get || req.method() == Method::Head { vec![] } else { req.bytes().await.unwrap_or_default() };
        let request = io::Request { method: req.method().to_string(), url: req.url()?.to_string(), headers: req.headers().entries().collect(), body, no_follow: false };
        let mut all = String::new();
        for i in 0.. { match req.headers().get(&format!("{SETTINGS}-{i}"))? { Some(piece) => all.push_str(&piece), None => break } }
        let fresh = serde_json::from_str::<serde_json::Value>(&all).ok();
        let mut config = self.config(fresh.as_ref());
        // A key the Worker vouched for (it reads the secrets fresh).
        if let (Some(until), Some(key)) = (req.headers().get(VOUCHED)?.and_then(|u| u.parse::<u64>().ok()), req.headers().get("authorization")?.and_then(|a| a.strip_prefix("Bearer ").map(str::to_string))) {
            config.dashboard_keys.push((until, key));
        }
        let (store, timer) = (DoStore(&self.state), DoTimer(&self.state));
        let plane = ControlPlane { store: &store, http: &FetchHttp, clock: &WorkerClock, timer: &timer, config: &config, cache: &self.cache, containers: Some(&Runners(&self.env, config.cloudflare_location.clone(), config.cloudflare_image.clone())) };
        let r = plane.handle(request).await;
        let headers = Headers::new();
        for (k, v) in &r.headers { headers.append(k, v)?; }
        Ok(Response::from_bytes(r.body)?.with_status(r.status).with_headers(headers))
    }

    async fn alarm(&self) -> Result<Response> {
        let config = self.config(None);
        let (store, timer) = (DoStore(&self.state), DoTimer(&self.state));
        let plane = ControlPlane { store: &store, http: &FetchHttp, clock: &WorkerClock, timer: &timer, config: &config, cache: &self.cache, containers: Some(&Runners(&self.env, config.cloudflare_location.clone(), config.cloudflare_image.clone())) };
        plane.alarm().await.map_err(Error::RustError)?;
        Response::ok("ok")
    }
}

/// One job's runner: GitHub's runner image in a container, with a just-in-time configuration that runs exactly one job;
/// it ends when the job ends, when asked, or at its time bound.
#[durable_object]
pub struct JobRunner { state: State }

impl DurableObject for JobRunner {
    fn new(state: State, _env: Env) -> Self { JobRunner { state } }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let container = self.state.container().ok_or("no container for this runner")?;
        let body: serde_json::Value = req.json().await?;
        if req.path() == "/stop" {
            if container.running() { container.destroy(None).await? }
            self.state.storage().delete_alarm().await?;
            return Response::ok("stopped")
        }
        let jit = body["jit"].as_str().unwrap_or_default();
        if jit.is_empty() && !body["gitlab"].is_object() { return Response::error("no runner configuration", 400) }
        // A start is always a fresh runner: one left from a start that failed half-way goes first.
        if container.running() { let _ = container.destroy(Some("starting again")).await; }
        if let Err(e) = start_runner(&container, jit, &body).await { console_error!("{e}"); return Response::error(e.to_string(), 502) }
        let minutes = body["max_minutes"].as_u64().unwrap_or(70);
        self.state.storage().set_alarm(std::time::Duration::from_secs(minutes * 60)).await?;
        Response::ok("started")
    }

    async fn alarm(&self) -> Result<Response> {
        if let Some(c) = self.state.container() { if c.running() { c.destroy(Some("time bound")).await? } }
        Response::ok("ended")
    }
}

/// Starts GitHub's runner: `/home/runner/run.sh` with a just-in-time configuration runs exactly one job. The container
/// is sized for the job, from the runner image this Worker names (Cloudflare's per-container scheduling).
async fn start_runner(container: &Container, jit: &str, body: &serde_json::Value) -> Result<()> {
    let options = Object::new();
    if body["gitlab"].is_object() { gitlab_options(&options, body)? }
    else {
        // GitHub's runner, with Docker that works here (see superci_core::docker).
        Reflect::set(&options, &"entrypoint".into(), &Array::of3(&"bash".into(), &"-c".into(), &superci_core::docker::start().into()))?;
        let env = Object::new();
        Reflect::set(&env, &"SUPERCI_JIT".into(), &jit.into())?;
        Reflect::set(&env, &"SUPERCI_DOCKER".into(), &superci_core::docker::WRAPPER.into())?;
        // A failing runner (a job nothing here can run): why, for its job-started hook.
        if let Some(why) = body["fail"].as_str() { Reflect::set(&env, &"SUPERCI_FAIL".into(), &why.into())?; }
        // GitHub's full image, loaded as it is read (checked again here: it goes into the start script's environment).
        if let Some(i) = body["image_url"].as_str().and_then(superci_core::plane::image_address) { Reflect::set(&env, &"SUPERCI_IMAGE".into(), &i.into())?; }
        Reflect::set(&options, &"env".into(), &env)?;
    }
    Reflect::set(&options, &"enableInternet".into(), &JsValue::TRUE)?;
    let image = Reflect::get(container.as_ref(), &"images".into()).ok().filter(|i| i.is_object()).and_then(|i| Reflect::get(&i, &"runner".into()).ok()).filter(|i| !i.is_undefined());
    let image = image.ok_or("this Worker names no runner image: update the control plane from the dashboard")?;
    Reflect::set(&options, &"image".into(), &image)?;
    let n = |k: &str, d: u64| body[k].as_u64().unwrap_or(d) as f64;
    let instance = Object::new();
    Reflect::set(&instance, &"vcpu".into(), &n("cpu", 4).into())?;
    Reflect::set(&instance, &"memoryMib".into(), &(n("ram_gb", 12) * 1024.0).into())?;
    Reflect::set(&instance, &"diskMb".into(), &(n("disk_gb", 20) * 1000.0).into())?;
    Reflect::set(&options, &"instance".into(), &instance)?;
    let start: Function = Reflect::get(container.as_ref(), &"start".into())?.dyn_into()?;
    // A start that fails (an image or size Cloudflare refuses) says so, rather than leaving the job waiting.
    let started = start.call1(container.as_ref(), &options).map_err(|e| Error::RustError(format!("the container did not start: {}", js_message(&e))))?;
    if let Ok(promise) = started.dyn_into::<js_sys::Promise>() {
        wasm_bindgen_futures::JsFuture::from(promise).await.map_err(|e| Error::RustError(format!("the container did not start: {}", js_message(&e))))?;
    }
    // Cloudflare stops a container soon after its Durable Object goes idle unless told otherwise: it runs until the job
    // ends (or its time bound; Cloudflare's longest is 6 hours).
    let minutes = body["max_minutes"].as_u64().unwrap_or(70).min(360);
    // Cloudflare can answer "temporarily unavailable" right after a start: asked again, three times, a second apart.
    let keep: Function = Reflect::get(container.as_ref(), &"setInactivityTimeout".into())?.dyn_into()?;
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 { Delay::from(std::time::Duration::from_secs(1)).await }
        let kept = match keep.call1(container.as_ref(), &((minutes * 60_000) as f64).into()) { Ok(v) => v, Err(e) => { last = js_message(&e); continue } };
        match kept.dyn_into::<js_sys::Promise>() {
            Ok(promise) => match wasm_bindgen_futures::JsFuture::from(promise).await { Ok(_) => return Ok(()), Err(e) => last = js_message(&e) },
            Err(_) => return Ok(()),
        }
    }
    Err(Error::RustError(format!("the container cannot be kept running: {last}")))
}

/// What a JavaScript error says (its message), not how it is built.
fn js_message(e: &wasm_bindgen::JsValue) -> String {
    use wasm_bindgen::JsCast;
    match e.dyn_ref::<js_sys::Error>() {
        Some(err) => String::from(err.message()),
        None => e.as_string().unwrap_or_else(|| js_sys::JSON::stringify(e).map(String::from).unwrap_or_else(|_| "an error without a message".into())),
    }
}

/// The containers of one size from before 0.5.0: the class stays (Cloudflare keeps its namespace), with nothing in it.
#[durable_object]
pub struct Runner { _state: State }

impl DurableObject for Runner {
    fn new(state: State, _env: Env) -> Self { Runner { _state: state } }
    async fn fetch(&self, _req: Request) -> Result<Response> { Response::error("superseded by JobRunner", 410) }
}

/// GitLab's runner for one job, in Docker on the container's own network (no bridge networks here). The token and URL go
/// in the environment, not the command line.
fn gitlab_options(options: &Object, body: &serde_json::Value) -> Result<()> {
    let gl = &body["gitlab"];
    Reflect::set(options, &"entrypoint".into(), &Array::of3(&"bash".into(), &"-c".into(), &superci_core::gitlab::RUNNER_SCRIPT.into()))?;
    let env = Object::new();
    Reflect::set(&env, &"GL_URL".into(), &gl["url"].as_str().unwrap_or_default().into())?;
    Reflect::set(&env, &"GL_TOKEN".into(), &gl["token"].as_str().unwrap_or_default().into())?;
    Reflect::set(&env, &"DOCKERD_FLAGS".into(), &"--iptables=false --ip6tables=false --ip-forward=false".into())?;
    Reflect::set(&env, &"GL_RUNNER_FLAGS".into(), &"--docker-network-mode host".into())?;
    Reflect::set(options, &"env".into(), &env)?;
    Ok(())
}
