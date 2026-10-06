//! A SuperCI runner agent on Cloudflare: one container per job (GitHub's runner, registered just in time for that
//! job), started when a control plane asks. It trusts one control plane the way AWS does: a token that control plane
//! signed (ES256; its public keys pinned here when the dashboard set this agent up), for this agent's URL, from its
//! subject, unexpired.
//! Each runner is a Durable Object with a container, sized for its job and placed in eastern North America (near GitHub);
//! it ends when the job ends, when asked, or at its time bound.
use js_sys::{Array, Function, Object, Reflect};
use superci_core::crypto::verify_es256_jwt;
use wasm_bindgen::{JsCast, JsValue};
use worker::*;

fn now_secs() -> u64 { (Date::now().as_millis() / 1000) as u64 }

/// The caller is the trusted control plane, or an error saying why not.
fn verify(req: &Request, env: &Env) -> std::result::Result<(), String> {
    let issuer = env.var("TRUSTED_ISSUER").map(|v| v.to_string()).map_err(|_| "no trusted control plane")?;
    let subject = env.var("TRUSTED_SUBJECT").map(|v| v.to_string()).map_err(|_| "no trusted control plane")?;
    let token = req.headers().get("authorization").ok().flatten().and_then(|v| v.strip_prefix("Bearer ").map(str::to_string)).ok_or("no token")?;
    let keys: serde_json::Value = serde_json::from_str(&env.var("TRUSTED_KEYS").map(|v| v.to_string()).map_err(|_| "no trusted keys")?).map_err(|e| e.to_string())?;
    let audience = req.url().map_err(|e| e.to_string())?.origin().ascii_serialization();
    for key in keys["keys"].as_array().into_iter().flatten() {
        if let Ok(claims) = verify_es256_jwt(&token, key, now_secs()) {
            if claims["iss"] == issuer.as_str() && claims["aud"] == audience.as_str() && claims["sub"] == subject.as_str() { return Ok(()) }
            return Err("token for another agent or control plane".into());
        }
    }
    Err("bad token".into())
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    if req.method() == Method::Get && path == "/health" { return Response::from_json(&serde_json::json!({ "agent": "cloudflare" })) }
    if req.method() != Method::Post || !["/launch", "/stop"].contains(&path.as_str()) { return Response::error("SuperCI runner agent: nothing here", 404) }
    if let Err(e) = verify(&req, &env) { return Response::error(format!("not allowed: {e}"), 401) }
    let body: serde_json::Value = req.json().await?;
    let id = if path == "/launch" { body["name"].as_str() } else { body["id"].as_str() }.ok_or("no runner name")?.to_string();
    // Where its container starts, as the control plane says (Cloudflare chooses for "auto"); the hint counts when made.
    let location = body["location"].as_str().unwrap_or("enam").to_string();
    let namespace = env.durable_object("RUNNER")?;
    // A runner is named at its start; then known by its Durable Object's id, which Cloudflare's usage analytics count
    // its container by (`instanceId`), so its cost can be read as billed.
    let by_id = id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit());
    let object = if by_id { namespace.id_from_string(&id)? } else { namespace.id_from_name(&id)? };
    let stub = if location == "auto" || by_id { object.get_stub()? } else { object.get_stub_with_location_hint(&location)? };
    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_body(Some(JsValue::from_str(&body.to_string())));
    let mut r = stub.fetch_with_request(Request::new_with_init(&format!("https://runner{path}"), &init)?).await?;
    if path != "/launch" || r.status_code() != 200 { return Ok(r) }
    let mut started: serde_json::Value = r.json().await?;
    started["id"] = object.to_string().into();
    Response::from_json(&started)
}

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
            return Response::from_json(&serde_json::json!({ "stopped": true }))
        }
        let jit = body["jit"].as_str().unwrap_or_default();
        if jit.is_empty() && !body["gitlab"].is_object() { return Response::error("no runner configuration", 400) }
        let name = body["name"].as_str().unwrap_or_default();
        // A start is always a fresh runner: one left from a start that failed half-way goes first.
        if container.running() { let _ = container.destroy(Some("starting again")).await; }
        if let Err(e) = start_runner(&container, jit, &body).await { console_error!("{e}"); return Response::error(e.to_string(), 502) }
        let minutes = body["max_minutes"].as_u64().unwrap_or(70);
        self.state.storage().set_alarm(std::time::Duration::from_secs(minutes * 60)).await?;
        Response::from_json(&serde_json::json!({ "id": name, "kind": "container" }))
    }

    /// The time bound: whatever still runs ends.
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
