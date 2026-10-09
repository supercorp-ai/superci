//! Cloudflare's API as the dashboard needs it: which accounts a token reaches, deploy or update the control plane's Worker (embedded in this
//! binary), its workers.dev address, and its secrets (written from this machine; Cloudflare never returns them).
use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use crate::Result;

const WORKER_JS: &[u8] = include_bytes!(env!("SUPERCI_WORKER_JS"));
const WORKER_WASM: &[u8] = include_bytes!(env!("SUPERCI_WORKER_WASM"));
const RUNNERS_JS: &[u8] = include_bytes!(env!("SUPERCI_RUNNERS_JS"));
const RUNNERS_WASM: &[u8] = include_bytes!(env!("SUPERCI_RUNNERS_WASM"));
const API: &str = "https://api.cloudflare.com/client/v4";
const COMPATIBILITY_DATE: &str = "2026-09-01";
/// Durable Object classes: PlaneObject since v1; Runner (containers of one size) v2; JobRunner (containers sized per
/// job, Cloudflare's per-container scheduling) in place of Runner since v3.
const MIGRATION_TAG: &str = "v3";

pub struct Cloudflare { agent: ureq::Agent, token: String }

pub struct Deployed { pub url: String, pub plane_id: String }

/// What a new control plane's deploy does, in order, as the dashboard shows it (the last step is the dashboard's own wait).
pub const DEPLOY_STEPS: [&str; 3] = ["Uploading the Worker", "Giving it its address", "Waiting for it to answer"];

/// The container application of a Worker's jobs (sized per job).
fn jobs_app(script: &str) -> String { format!("{script}-jobs") }

/// The Durable Object classes a Worker gets: `extra` (the control plane's own) and JobRunner, from where it is (its
/// migration tag) to v3. Runner (one size, before) stays declared: Cloudflare will not delete a class an earlier version
/// bound.
fn migration(existing: bool, tag: Option<&str>, extra: &[&str]) -> Option<Value> {
    match (existing, tag) {
        (false, _) | (true, None) => Some(json!({ "new_tag": MIGRATION_TAG, "new_sqlite_classes": extra.iter().copied().chain(["JobRunner"]).collect::<Vec<_>>() })),
        (true, Some("v1")) => Some(json!({ "old_tag": "v1", "new_tag": MIGRATION_TAG, "new_sqlite_classes": ["JobRunner"] })),
        (true, Some("v2")) => Some(json!({ "old_tag": "v2", "new_tag": MIGRATION_TAG, "new_sqlite_classes": ["JobRunner"] })),
        _ => None,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(120))).build().into()
}

impl Cloudflare {
    pub fn new(token: &str) -> Self { Cloudflare { agent: agent(), token: token.trim().to_string() } }

    /// The accounts this token reaches, as (id, name).
    pub fn accounts(&self) -> Result<Vec<(String, String)>> {
        let accounts = self.call("GET", "/accounts?per_page=50", None)?;
        let list: Vec<(String, String)> = accounts.as_array().map(|a| a.iter().map(|x| (x["id"].as_str().unwrap_or_default().to_string(), x["name"].as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default();
        if list.is_empty() { return Err("the token reaches no Cloudflare account (it needs Account Settings: Read)".into()) }
        Ok(list)
    }

    /// The control planes in these accounts: Workers with the control plane's Durable Object class and a PLANE_ID.
    pub fn find_planes(&self, accounts: &[(String, String)]) -> Result<Vec<crate::plane::Plane>> {
        // Every account at once; in each, its Workers and its workers.dev subdomain at once; then each candidate's id
        // and label from its own /health (quick), or from its settings when it does not answer.
        let found: Vec<Result<Vec<crate::plane::Plane>>> = std::thread::scope(|s| {
            let hs: Vec<_> = accounts.iter().map(|(a, account_name)| s.spawn(move || -> Result<Vec<crate::plane::Plane>> {
                let (scripts, sub) = std::thread::scope(|s| {
                    let scripts = s.spawn(|| self.call("GET", &format!("/accounts/{a}/workers/scripts"), None));
                    let sub = s.spawn(|| self.call("GET", &format!("/accounts/{a}/workers/subdomain"), None));
                    (scripts.join().unwrap_or_else(|_| Err("stopped".into())), sub.join().unwrap_or_else(|_| Err("stopped".into())))
                });
                let candidates: Vec<String> = scripts?.as_array().into_iter().flatten()
                    .filter(|s| s["named_handlers"].as_array().is_some_and(|h| h.iter().any(|h| h["name"] == "PlaneObject")))
                    .filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
                if candidates.is_empty() { return Ok(vec![]) }
                let sub = sub?["subdomain"].as_str().unwrap_or_default().to_string();
                Ok(std::thread::scope(|s| {
                    let hs: Vec<_> = candidates.iter().map(|script| { let sub = &sub; s.spawn(move || -> Option<crate::plane::Plane> {
                        let url = format!("https://{script}.{sub}.workers.dev");
                        let (plane_id, label) = match health(&url).filter(|h| h["plane"].is_string()) {
                            Some(h) => (h["plane"].as_str()?.to_string(), h["label"].as_str().unwrap_or("superci").to_string()),
                            None => {
                                let settings = self.call("GET", &format!("/accounts/{a}/workers/scripts/{script}/settings"), None).ok()?;
                                let text = |n: &str| settings["bindings"].as_array().and_then(|bs| bs.iter().find(|b| b["name"] == n).and_then(|b| b["text"].as_str().map(str::to_string)));
                                (text("PLANE_ID")?, text("LABEL").unwrap_or_else(|| "superci".into()))
                            }
                        };
                        Some(crate::plane::Plane::Cloudflare { account_id: a.clone(), account_name: account_name.clone(), url, script: script.clone(), plane_id, label })
                    }) }).collect();
                    hs.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
                }))
            })).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("stopped".into()))).collect()
        });
        let mut planes = vec![];
        for f in found { planes.extend(f?) }
        Ok(planes)
    }

    /// One API call; Cloudflare's envelope `{ success, errors, result }` becomes the result or an error.
    fn call(&self, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<Value> {
        let url = format!("{API}{path}");
        let auth = format!("Bearer {}", self.token);
        let response = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("authorization", &auth).call(),
            ("DELETE", _) => self.agent.delete(&url).header("authorization", &auth).call(),
            (m, Some((content_type, bytes))) => {
                let req = if m == "PUT" { self.agent.put(&url) } else { self.agent.post(&url) };
                req.header("authorization", &auth).header("content-type", content_type).send(&bytes[..])
            }
            (m, None) => {
                let req = if m == "PUT" { self.agent.put(&url) } else { self.agent.post(&url) };
                req.header("authorization", &auth).send_empty()
            }
        };
        let mut response = response.map_err(|e| format!("{method} {path}: {e}"))?;
        let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|_| format!("{method} {path}: {}", text.chars().take(200).collect::<String>()))?;
        if v["success"] == true { return Ok(v["result"].clone()) }
        let errors = v["errors"].as_array().map(|es| es.iter().map(|e| format!("{} ({})", e["message"].as_str().unwrap_or("?"), e["code"])).collect::<Vec<_>>().join("; ")).unwrap_or_default();
        Err(format!("{method} {path}: {errors}"))
    }

    /// One query to Cloudflare's GraphQL Analytics API.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let mut r = self.agent.post(&format!("{API}/graphql")).header("authorization", &format!("Bearer {}", self.token)).header("content-type", "application/json")
            .send(json!({ "query": query, "variables": variables }).to_string()).map_err(|e| format!("analytics: {e}"))?;
        let v: Value = serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if let Some(e) = v["errors"].as_array().filter(|e| !e.is_empty()) { return Err(format!("analytics: {}", e[0]["message"].as_str().unwrap_or("error"))) }
        Ok(v["data"].clone())
    }

    /// Each container's usage, as Cloudflare meters it (by its Durable Object id), between two times: its cost, and the
    /// memory GiB-seconds metered (to tell whether Cloudflare has all of it yet).
    pub fn container_costs(&self, account: &str, ids: &[String], from_ms: u64, to_ms: u64) -> Result<HashMap<String, (f64, f64)>> {
        let iso = superci_core::aws::amz_iso;
        let q = "query($a:String,$since:Time,$until:Time,$ids:[string!]){viewer{accounts(filter:{accountTag:$a}){g:containersUsageAdaptiveGroups(limit:10000,filter:{datetime_geq:$since,datetime_leq:$until,instanceId_in:$ids}){dimensions{instanceId} sum{cpuTimeSec allocatedMemory allocatedDisk txBytes}}}}}";
        let data = self.graphql(q, json!({ "a": account, "since": iso(from_ms), "until": iso(to_ms), "ids": ids }))?;
        let mut costs = HashMap::new();
        for g in data["viewer"]["accounts"][0]["g"].as_array().into_iter().flatten() {
            let n = |k: &str| g["sum"][k].as_f64().unwrap_or(0.0);
            let usd = superci_core::plane::cloudflare_measured_usd(n("cpuTimeSec"), n("allocatedMemory"), n("allocatedDisk"), n("txBytes"));
            let e = costs.entry(g["dimensions"]["instanceId"].as_str().unwrap_or_default().to_string()).or_insert((0.0, 0.0));
            e.0 += usd;
            e.1 += n("allocatedMemory") / 1_073_741_824.0;
        }
        Ok(costs)
    }

    /// This month's containers in the account, as Cloudflare meters them: their cost at list prices, and what is billed
    /// after the usage Workers Paid includes (shared by every container in the account).
    pub fn containers_month(&self, account: &str, now_ms: u64) -> Result<(f64, f64)> {
        let iso = superci_core::aws::amz_iso;
        let q = "query($a:String,$since:Time,$until:Time){viewer{accounts(filter:{accountTag:$a}){g:containersUsageAdaptiveGroups(limit:1,filter:{datetime_geq:$since,datetime_leq:$until}){sum{cpuTimeSec allocatedMemory allocatedDisk txBytes}}}}}";
        let data = self.graphql(q, json!({ "a": account, "since": iso(superci_core::plane::month_start_ms(now_ms)), "until": iso(now_ms) }))?;
        let sum = &data["viewer"]["accounts"][0]["g"][0]["sum"];
        let n = |k: &str| sum[k].as_f64().unwrap_or(0.0);
        Ok((superci_core::plane::cloudflare_measured_usd(n("cpuTimeSec"), n("allocatedMemory"), n("allocatedDisk"), n("txBytes")),
            superci_core::plane::cloudflare_billed_usd(n("cpuTimeSec"), n("allocatedMemory"), n("allocatedDisk"), n("txBytes"))))
    }

    fn json(&self, method: &str, path: &str, body: Value) -> Result<Value> {
        self.call(method, path, Some(("application/json", body.to_string().into_bytes())))
    }

    /// Deploys the control plane, or updates it: an existing control plane keeps its id, its state (Durable Object) and its secrets.
    /// With Cloudflare runners (`runners`), its jobs' containers come along: their image and their container application.
    pub fn deploy(&self, a: &str, name: &str, label: &str, runners: bool) -> Result<Deployed> { self.deploy_with(a, name, label, runners, &|_| {}) }

    /// The same, telling `step` which of [`DEPLOY_STEPS`] it has started (0, then 1).
    pub fn deploy_with(&self, a: &str, name: &str, label: &str, runners: bool, step: &dyn Fn(usize)) -> Result<Deployed> {
        step(0);
        let existing = self.call("GET", &format!("/accounts/{a}/workers/scripts/{name}/settings"), None).ok();
        let old_id = existing.as_ref().and_then(|s| s["bindings"].as_array().and_then(|bs| bs.iter().find(|b| b["name"] == "PLANE_ID").and_then(|b| b["text"].as_str().map(str::to_string))));
        let plane_id = old_id.clone().unwrap_or_else(|| superci_core::crypto::random_id(12));
        let image = if runners { Some(self.runner_image(a)?) } else { None };
        let mut metadata = json!({
            "main_module": "index.js",
            "compatibility_date": COMPATIBILITY_DATE,
            "bindings": [
                { "type": "durable_object_namespace", "name": "PLANE", "class_name": "PlaneObject" },
                { "type": "durable_object_namespace", "name": "RUNNER", "class_name": "JobRunner" },
                { "type": "plain_text", "name": "PLANE_ID", "text": plane_id },
                { "type": "plain_text", "name": "LABEL", "text": label },
            ],
            // Secrets set from this machine (the GitHub App, the AWS connect token) stay across updates.
            "keep_bindings": ["secret_text"],
        });
        if let Some(image) = &image { metadata["containers"] = json!([{ "name": jobs_app(name), "class_name": "JobRunner", "images": { "runner": image } }]) }
        let tag = self.migration_tag(a, name);
        if let Some(m) = migration(existing.is_some(), tag.as_deref(), &["PlaneObject"]) { metadata["migrations"] = m }
        // Containers of one size from before: their application goes (its class stays, empty).
        if tag.as_deref() == Some("v2") { self.delete_app(a, &format!("{name}-runners"))? }
        let (content_type, body) = script_upload(&metadata);
        self.call("PUT", &format!("/accounts/{a}/workers/scripts/{name}"), Some((&content_type, body)))?;
        if image.is_some() { self.jobs_application(a, &jobs_app(name), name)? }
        step(1);
        let url = self.workers_dev(a, name)?;
        Ok(Deployed { url, plane_id })
    }

    fn migration_tag(&self, a: &str, name: &str) -> Option<String> {
        self.call("GET", &format!("/accounts/{a}/workers/scripts"), None).ok()
            .and_then(|l| l.as_array().and_then(|l| l.iter().find(|s| s["id"] == name).and_then(|s| s["migration_tag"].as_str().map(str::to_string))))
    }

    /// GitHub's runner image in this account's registry, pinned by digest and prepared for containers to start from.
    pub fn runner_image(&self, a: &str) -> Result<String> {
        let image = crate::image::copy_runner_image(self, a)?;
        // Cloudflare unpacks it once for its machines; the first time takes a few minutes.
        for attempt in 0..120 {
            let mut v = None;
            for _ in 0..10 {
                match self.json("POST", &format!("/accounts/{a}/containers/image-preparations"), json!({ "image": image })) {
                    // The registry can take a few seconds before a just-copied image is visible.
                    Err(e) if e.contains("does not exist") || e.contains("DOESNT_CONTAIN") => std::thread::sleep(Duration::from_secs(3)),
                    r => { v = Some(r?); break }
                }
            }
            let v = v.ok_or("Cloudflare does not see the runner image yet; try again in a minute")?;
            match v["status"].as_str() {
                Some("ready") => return Ok(image),
                Some("error") => return Err(format!("Cloudflare could not prepare the runner image: {}", v["reason"].as_str().unwrap_or("no reason given"))),
                _ => { if attempt % 6 == 0 { eprintln!("Cloudflare is preparing the runner image for its machines…") } std::thread::sleep(Duration::from_secs(5)) }
            }
        }
        Err("Cloudflare took over 10 minutes to prepare the runner image; try again later".into())
    }

    /// The container application for a Worker's `JobRunner` class, with per-container scheduling (each container's image
    /// and size come from the Worker when it starts one). Safe to repeat.
    fn jobs_application(&self, a: &str, app_name: &str, script: &str) -> Result<()> {
        let apps = self.call("GET", &format!("/accounts/{a}/containers/applications"), None)?;
        if apps.as_array().into_iter().flatten().any(|app| app["name"] == app_name) { return Ok(()) }
        let namespaces = self.call("GET", &format!("/accounts/{a}/workers/durable_objects/namespaces?per_page=1000"), None)?;
        let namespace_id = namespaces.as_array().into_iter().flatten().find(|n| n["script"] == script && n["class"] == "JobRunner")
            .and_then(|n| n["id"].as_str().map(str::to_string)).ok_or("the runner containers' Durable Object namespace did not appear")?;
        self.json("POST", &format!("/accounts/{a}/containers/applications"), json!({
            "name": app_name, "scheduling_policy": "durable_object", "durable_objects": { "namespace_id": namespace_id } }))?;
        Ok(())
    }

    /// Removes a container application by name (its running containers stop), if there is one.
    fn delete_app(&self, a: &str, app_name: &str) -> Result<()> {
        let apps = self.call("GET", &format!("/accounts/{a}/containers/applications"), None)?;
        for app in apps.as_array().into_iter().flatten().filter(|app| app["name"] == app_name) {
            if let Some(id) = app["id"].as_str() { self.call("DELETE", &format!("/accounts/{a}/containers/applications/{id}"), None)?; }
        }
        Ok(())
    }

    /// The Cloudflare runner agent for one control plane: a Worker trusting only that control plane's tokens, with one
    /// container per job, sized for it. Safe to repeat. Returns its URL.
    pub fn deploy_runners(&self, a: &str, plane_url: &str, plane_id: &str) -> Result<String> {
        let name = format!("superci-runners-{plane_id}");
        // The control plane's public keys, pinned in the agent (a Worker cannot fetch another on the same workers.dev).
        let mut r = agent().get(&format!("{plane_url}/.well-known/jwks.json")).call().map_err(|e| e.to_string())?;
        let keys = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !serde_json::from_str::<Value>(&keys).is_ok_and(|k| k["keys"].is_array()) { return Err("the control plane did not publish its keys".into()) }
        let existing = self.call("GET", &format!("/accounts/{a}/workers/scripts/{name}/settings"), None).is_ok();
        let image = self.runner_image(a)?;
        let mut metadata = json!({
            "main_module": "index.js",
            "compatibility_date": COMPATIBILITY_DATE,
            "bindings": [
                { "type": "durable_object_namespace", "name": "RUNNER", "class_name": "JobRunner" },
                { "type": "plain_text", "name": "TRUSTED_ISSUER", "text": plane_url },
                { "type": "plain_text", "name": "TRUSTED_SUBJECT", "text": format!("plane:{plane_id}") },
                { "type": "plain_text", "name": "TRUSTED_KEYS", "text": keys },
            ],
            "containers": [{ "name": jobs_app(&name), "class_name": "JobRunner", "images": { "runner": image } }],
        });
        let tag = self.migration_tag(a, &name);
        if let Some(m) = migration(existing, tag.as_deref(), &[]) { metadata["migrations"] = m }
        // Its containers of one size from before (an application named like the Worker) go.
        if tag.as_deref() == Some("v2") { self.delete_app(a, &name)? }
        let (content_type, body) = upload_parts(&metadata, RUNNERS_JS, RUNNERS_WASM);
        self.call("PUT", &format!("/accounts/{a}/workers/scripts/{name}"), Some((&content_type, body)))?;
        let url = self.workers_dev(a, &name)?;
        self.jobs_application(a, &jobs_app(&name), &name)?;
        Ok(url)
    }

    /// Removes a control plane's separate Cloudflare runner agent: its container applications and its Worker.
    pub fn delete_runners(&self, a: &str, plane_id: &str) -> Result<()> {
        let name = format!("superci-runners-{plane_id}");
        self.delete_app(a, &name)?;
        self.delete_app(a, &jobs_app(&name))?;
        self.call("DELETE", &format!("/accounts/{a}/workers/scripts/{name}?force=true"), None).map(|_| ())
    }

    /// Deletes a control plane from the account: its container application (its containers stop), its Worker with its
    /// storage, and a runner agent made for it. Safe to repeat.
    pub fn delete_plane(&self, a: &str, script: &str, plane_id: &str) -> Result<()> {
        self.delete_app(a, &jobs_app(script))?;
        match self.call("DELETE", &format!("/accounts/{a}/workers/scripts/{script}?force=true"), None) {
            Ok(_) => {}
            Err(e) if e.contains("10007") || e.to_lowercase().contains("not found") => {}
            Err(e) => return Err(e),
        }
        let _ = self.delete_runners(a, plane_id);
        Ok(())
    }

    /// Short-lived credentials to push to the account's Cloudflare registry (user name, password).
    pub fn registry_push_credentials(&self, a: &str) -> Result<(String, String)> {
        let v = self.json("POST", &format!("/accounts/{a}/containers/registries/registry.cloudflare.com/credentials"), json!({ "expiration_minutes": 30, "permissions": ["push", "pull"] }))?;
        Ok((v["username"].as_str().ok_or("no registry user")?.to_string(), v["password"].as_str().ok_or("no registry password")?.to_string()))
    }

    /// Turns on a Worker's workers.dev address and returns it (the API can answer "does not exist" for a few seconds
    /// after the first upload).
    fn workers_dev(&self, a: &str, name: &str) -> Result<String> {
        let mut last = String::new();
        for attempt in 0..10 {
            match self.json("POST", &format!("/accounts/{a}/workers/scripts/{name}/subdomain"), json!({ "enabled": true, "previews_enabled": false })) {
                Ok(_) => { last.clear(); break }
                Err(e) => { last = e; std::thread::sleep(Duration::from_secs(2 + attempt)) }
            }
        }
        if !last.is_empty() { return Err(last) }
        let sub = self.call("GET", &format!("/accounts/{a}/workers/subdomain"), None)?["subdomain"].as_str().ok_or("this account has no workers.dev subdomain yet: open Workers in Cloudflare's dashboard once")?.to_string();
        Ok(format!("https://{name}.{sub}.workers.dev"))
    }

    /// The names of a Worker's secrets (Cloudflare never returns their values).
    pub fn secret_names(&self, a: &str, script: &str) -> Result<Vec<String>> {
        Ok(self.call("GET", &format!("/accounts/{a}/workers/scripts/{script}/secrets"), None)?.as_array().into_iter().flatten().filter_map(|s| s["name"].as_str().map(str::to_string)).collect())
    }

    pub fn delete_secret(&self, a: &str, script: &str, name: &str) -> Result<()> {
        self.call("DELETE", &format!("/accounts/{a}/workers/scripts/{script}/secrets/{name}"), None).map(|_| ())
    }

    /// A secret on the control plane's Worker (encrypted by Cloudflare; not readable back, not even through the API).
    pub fn put_secret(&self, a: &str, script: &str, name: &str, value: &str) -> Result<()> {
        self.json("PUT", &format!("/accounts/{a}/workers/scripts/{script}/secrets"), json!({ "name": name, "text": value, "type": "secret_text" })).map(|_| ())
    }
}

/// multipart/form-data with the script's metadata and its two modules.
fn script_upload(metadata: &Value) -> (String, Vec<u8>) { upload_parts(metadata, WORKER_JS, WORKER_WASM) }

fn upload_parts(metadata: &Value, js: &[u8], wasm: &[u8]) -> (String, Vec<u8>) {
    let mut seed = [0u8; 12];
    getrandom::getrandom(&mut seed).expect("randomness");
    let boundary = format!("superci{}", URL_SAFE_NO_PAD.encode(seed));
    let mut body = Vec::new();
    let mut part = |name: &str, filename: Option<&str>, content_type: &str, bytes: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"{}\r\nContent-Type: {content_type}\r\n\r\n",
            filename.map(|f| format!("; filename=\"{f}\"")).unwrap_or_default()).as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    part("metadata", None, "application/json", metadata.to_string().as_bytes());
    part("index.js", Some("index.js"), "application/javascript+module", js);
    part("index_bg.wasm", Some("index_bg.wasm"), "application/wasm", wasm);
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Cloudflare's browser sign-in. Cloudflare has no OAuth apps for other tools, so this is the public client `wrangler
/// login` uses (its consent screen says Wrangler), with the one redirect it allows: http://localhost:8976/oauth/callback.
const OAUTH_CLIENT: &str = "54d11594-84e4-41aa-b438-e81b8fa78ee7";
pub const OAUTH_PORT: u16 = 8976;

pub fn oauth_redirect() -> String { format!("http://localhost:{OAUTH_PORT}/oauth/callback") }

pub struct Pending { verifier: String, pub state: String }

/// A sign-in with Cloudflare: its token, when that ends, and what renews it (kept with SuperCI's sign-ins: store.rs).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Session { access: String, refresh: Option<String>, expires_at_ms: u64 }

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

pub fn authorize() -> (String, Pending) {
    // What it needs, asked once (`permissions::CLOUDFLARE`): read which accounts you have and their usage, change
    // Workers scripts (with their secrets and workers.dev address), and containers.
    let scopes = superci_core::permissions::cloudflare_scopes();
    use sha2::{Digest, Sha256};
    let verifier = superci_core::crypto::random_token(48);
    let challenge = superci_core::crypto::b64url(&Sha256::digest(verifier.as_bytes()));
    let state = superci_core::crypto::random_token(16);
    let q = url::form_urlencoded::Serializer::new(String::new()).extend_pairs([
        ("response_type", "code"), ("client_id", OAUTH_CLIENT), ("redirect_uri", &oauth_redirect()), ("scope", &scopes),
        ("state", &state), ("code_challenge", &challenge), ("code_challenge_method", "S256"),
    ]).finish().replace('+', "%20");
    (format!("https://dash.cloudflare.com/oauth2/auth?{q}"), Pending { verifier, state })
}

fn token_call(fields: &[(&str, &str)]) -> Result<Session> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
    let mut r = agent().post("https://dash.cloudflare.com/oauth2/token").header("content-type", "application/x-www-form-urlencoded").send(body).map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    let Some(access) = v["access_token"].as_str() else { return Err(format!("Cloudflare sign-in: {}", text.chars().take(200).collect::<String>())) };
    Ok(Session { access: access.into(), refresh: v["refresh_token"].as_str().map(str::to_string),
        expires_at_ms: now_ms() + v["expires_in"].as_u64().unwrap_or(3600) * 1000 })
}

/// Asks Cloudflare to end a sign-in (its token to renew with), as `wrangler logout` does. Whether Cloudflare said yes.
pub fn end_sign_in(refresh: &str) -> bool {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs([("client_id", OAUTH_CLIENT), ("token_type_hint", "refresh_token"), ("token", refresh)]).finish();
    quick().post("https://dash.cloudflare.com/oauth2/revoke").header("content-type", "application/x-www-form-urlencoded").send(body).is_ok_and(|r| r.status().is_success())
}

pub fn exchange(p: Pending, code: &str) -> Result<Session> {
    token_call(&[("grant_type", "authorization_code"), ("code", code), ("client_id", OAUTH_CLIENT), ("redirect_uri", &oauth_redirect()), ("code_verifier", &p.verifier)])
}

impl Session {
    /// A token given to SuperCI by name (SUPERCI_CLOUDFLARE_TOKEN, for a machine with no browser) never expires here.
    pub fn from_token(token: &str) -> Self { Session { access: token.trim().into(), refresh: None, expires_at_ms: now_ms() + 365 * 86_400_000 } }

    /// What renews it, to ask Cloudflare to end it (`superci logout`).
    pub fn refresh_token(&self) -> Option<&str> { self.refresh.as_deref() }


    /// A client whose token is good for a few more minutes, renewing it when needed.
    pub fn client(&mut self) -> Result<Cloudflare> {
        if self.expires_at_ms < now_ms() + 300_000 {
            let refresh = self.refresh.clone().ok_or("the Cloudflare sign-in expired: sign in again")?;
            let renewed = token_call(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", OAUTH_CLIENT)])
                // Cloudflare refusing what renews it: the sign-in is over. Cloudflare not answering leaves it as it is.
                .map_err(|e| if e.contains("invalid_grant") || e.contains("invalid_token") { format!("the Cloudflare sign-in expired: sign in again ({e})") } else { e })?;
            // Cloudflare may give a new one to renew with, or leave the one before in place.
            *self = Session { refresh: renewed.refresh.or(Some(refresh)), ..renewed };
        }
        Ok(Cloudflare::new(&self.access))
    }
}

/// The control plane's yes/no health: { github, installed, aws }.
/// For looking at a control plane: a short time limit, so a page never waits long on one that does not answer.
fn quick() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(8))).build().into()
}

pub fn health(plane_url: &str) -> Option<Value> {
    let mut r = quick().get(&format!("{plane_url}/health")).call().ok()?;
    if r.status().as_u16() != 200 { return None }
    serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()
}

/// Something the control plane looks up for the dashboard (GitLab's projects), asked with this session's key.
pub fn plane_get(plane_url: &str, key: &str, path: &str) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(40))).build().into();
    let mut r = agent.get(&format!("{plane_url}{path}")).header("authorization", &format!("Bearer {key}")).call().map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if r.status().as_u16() != 200 { return Err(format!("the control plane answered {}: {}", r.status(), text.chars().take(200).collect::<String>())) }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// A change the control plane makes itself (no restart), asked with this session's key.
pub fn plane_post(plane_url: &str, key: &str, path: &str, body: &Value) -> Result<Value> {
    let mut r = quick().post(&format!("{plane_url}{path}")).header("authorization", &format!("Bearer {key}")).header("content-type", "application/json")
        .send(body.to_string()).map_err(|e| e.to_string())?;
    if r.status().as_u16() != 200 { return Err(format!("the control plane answered {}", r.status())) }
    serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// A step of moving between control planes: with this session's key and the plane's one-time move token. Its status
/// too (403: the token is not there yet, or was used).
pub fn plane_move_call(plane_url: &str, key: &str, token: &str, path: &str, body: &Value) -> Result<(u16, Value)> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into();
    let mut r = agent.post(&format!("{plane_url}{path}")).header("authorization", &format!("Bearer {key}")).header("x-move-token", token)
        .header("content-type", "application/json").send(body.to_string()).map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// The control plane's view for the dashboard (jobs, the App, the AWS connection), read with this session's key.
pub fn status(plane_url: &str, key: &str) -> Option<Value> { status_of(plane_url, key, "") }

/// The same, to see a setting take: without where the App is installed (which GitHub is asked for each time).
pub fn status_light(plane_url: &str, key: &str) -> Option<Value> { status_of(plane_url, key, "?light=1") }

fn status_of(plane_url: &str, key: &str, query: &str) -> Option<Value> {
    let mut r = quick().get(&format!("{plane_url}/status{query}")).header("authorization", &format!("Bearer {key}")).call().ok()?;
    if r.status().as_u16() != 200 { return None }
    serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{now_ms, watch, Idle};
    use std::io::{Read, Write};
    use std::time::Instant;

    /// A Cloudflare runner agent (deployed for this check only, trusting a key made here) starts a runner with a
    /// 3-minute bound; nobody stops it, and its container ends by itself (SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_LIVE_CF_ACCOUNT).
    #[test]
    #[ignore]
    fn live_cloudflare_runner_ends_at_its_time_bound() {
        let cf = Cloudflare::new(&std::env::var("SUPERCI_CLOUDFLARE_TOKEN").expect("SUPERCI_CLOUDFLARE_TOKEN"));
        let account = std::env::var("SUPERCI_LIVE_CF_ACCOUNT").expect("SUPERCI_LIVE_CF_ACCOUNT");
        let key = superci_core::crypto::SigningKeyStore::generate();
        // The "control plane": its keys served here, as the dashboard reads them when it sets an agent up.
        let jwks = json!({ "keys": [key.public_jwk().unwrap()] }).to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let plane_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{jwks}", jwks.len());
        });
        let plane_id = format!("leak{}", superci_core::crypto::random_id(6));
        struct Gone<'a>(&'a Cloudflare, &'a str, &'a str);
        impl Drop for Gone<'_> { fn drop(&mut self) { match self.0.delete_runners(self.1, self.2) { Ok(()) => eprintln!("test agent removed"), Err(e) => eprintln!("REMOVE BY HAND: superci-runners-{}: {e}", self.2) } } }
        let url = cf.deploy_runners(&account, &plane_url, &plane_id).unwrap();
        let _gone = Gone(&cf, &account, &plane_id);
        eprintln!("test agent at {url}");
        let idle = Idle::new(&format!("superci-leaktest-cf-{}", superci_core::crypto::random_id(6)));
        let body = json!({ "name": format!("leaktest-{plane_id}"), "job": 1, "repo": "leaktest", "max_minutes": 3, "cpu": 1, "ram_gb": 4, "disk_gb": 8, "location": "enam", "jit": idle.jit });
        // A new Worker takes a little while to answer everywhere.
        let mut launched = None;
        for _ in 0..24 {
            let secs = now_ms() / 1000;
            let token = key.jwt(&json!({ "iss": plane_url, "sub": format!("plane:{plane_id}"), "aud": url, "iat": secs, "exp": secs + 120 })).unwrap();
            match agent().post(&format!("{url}/launch")).header("authorization", &format!("Bearer {token}")).header("content-type", "application/json").send(body.to_string()) {
                Ok(mut r) if r.status().as_u16() == 200 => { launched = Some(r.body_mut().read_to_string().unwrap_or_default()); break }
                Ok(mut r) => eprintln!("launch: {} {}", r.status(), r.body_mut().read_to_string().unwrap_or_default().chars().take(160).collect::<String>()),
                Err(e) => eprintln!("launch: {e}"),
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        eprintln!("launched: {}", launched.expect("the test agent never started the runner"));
        let start = Instant::now();
        let mut online = false;
        let ended = watch("runner", start, Duration::from_secs(10 * 60), || {
            let s = idle.status().unwrap_or_else(|| "removed".into());
            online |= s == "online";
            let ended = online && s != "online";
            (s, ended)
        });
        let ended = ended.expect("the runner was still online after 10 minutes");
        assert!(ended >= Duration::from_secs(150) && ended <= Duration::from_secs(7 * 60), "ended after {ended:?}");
    }
}

#[cfg(test)]
mod metering {
    /// Reads Cloudflare's metering for containers by id (SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_LIVE_CF_ACCOUNT, SUPERCI_LIVE_CF_IDS:
    /// comma-separated Durable Object ids, SUPERCI_LIVE_CF_FROM / _TO: ms).
    #[test]
    #[ignore]
    fn live_cloudflare_container_costs() {
        let cf = super::Cloudflare::new(&std::env::var("SUPERCI_CLOUDFLARE_TOKEN").unwrap());
        let ids: Vec<String> = std::env::var("SUPERCI_LIVE_CF_IDS").unwrap().split(',').map(str::to_string).collect();
        let n = |k: &str| std::env::var(k).unwrap().parse::<u64>().unwrap();
        let costs = cf.container_costs(&std::env::var("SUPERCI_LIVE_CF_ACCOUNT").unwrap(), &ids, n("SUPERCI_LIVE_CF_FROM"), n("SUPERCI_LIVE_CF_TO")).unwrap();
        for (id, (usd, gib_s)) in &costs { eprintln!("{id} ${usd:.6} ({gib_s:.0} GiB-s)") }
        assert!(!costs.is_empty());
        let (metered, billed) = cf.containers_month(&std::env::var("SUPERCI_LIVE_CF_ACCOUNT").unwrap(), crate::live::now_ms()).unwrap();
        eprintln!("this month: ${metered:.4} metered, ${billed:.4} billed after included usage");
    }
}
