//! Cloudflare's API as the dashboard needs it: which accounts a token reaches, deploy or update the control plane's Worker (embedded in this
//! binary), its workers.dev address, and its secrets (written from this machine; Cloudflare never returns them).
use std::collections::HashMap;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use crate::Result;

use superci_programs_planes::{RUNNERS_JS, RUNNERS_WASM, WORKER_JS, WORKER_WASM};
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

/// A new control plane's id: random (in a debug build, `SUPERCI_NEW_PLANE_ID` when it is one: the tests', so that
/// what a recording answered of it fits).
pub fn new_plane_id() -> String {
    #[cfg(debug_assertions)]
    if let Some(id) = std::env::var("SUPERCI_NEW_PLANE_ID").ok().filter(|i| i.len() == 12 && i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())) { return id }
    superci_core::crypto::random_id(12)
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
                    (scripts.join().unwrap_or(Err("stopped".into())), sub.join().unwrap_or(Err("stopped".into())))
                });
                let candidates: Vec<String> = scripts?.as_array().into_iter().flatten()
                    .filter(|s| s["named_handlers"].as_array().is_some_and(|h| h.iter().any(|h| h["name"] == "PlaneObject")))
                    .filter_map(|s| s["id"].as_str().map(str::to_string)).collect();
                if candidates.is_empty() { return Ok(vec![]) }
                let sub = sub?["subdomain"].as_str().unwrap_or_default().to_string();
                Ok(std::thread::scope(|s| {
                    let hs: Vec<_> = candidates.iter().map(|script| { let sub = &sub; s.spawn(move || -> Option<crate::plane::Plane> {
                        let url = format!("https://{script}.{sub}.workers.dev");
                        let answered = health(&url).and_then(|h| Some((h["plane"].as_str()?.to_string(), h["label"].as_str().unwrap_or("superci").to_string())));
                        let (plane_id, label) = match answered {
                            Some(said) => said,
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
            hs.into_iter().map(|h| h.join().unwrap_or(Err("stopped".into()))).collect()
        });
        let mut planes = vec![];
        for f in found { planes.extend(f?) }
        Ok(planes)
    }

    /// One API call; Cloudflare's envelope `{ success, errors, result }` becomes the result or an error.
    fn call(&self, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<Value> {
        let url = crate::aws::stand_in(&format!("{API}{path}"));
        let auth = format!("Bearer {}", self.token);
        // (What was asked, for a recording: JSON as it is; anything else, a Worker's upload say, by its size only.)
        #[cfg(debug_assertions)]
        let asked: Vec<u8> = match &body { Some((t, b)) if t.starts_with("application/json") => b.clone(), Some((t, b)) => format!("({t}, {} bytes)", b.len()).into_bytes(), None => vec![] };
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
        #[cfg(debug_assertions)]
        let (status, headers): (u16, Vec<(String, String)>) = (response.status().as_u16(), response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect());
        let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
        #[cfg(debug_assertions)]
        crate::aws::record(&superci_core::io::Request::new(method, &format!("{API}{path}")).with_body(asked), status, &headers, text.as_bytes());
        let v: Value = serde_json::from_str(&text).map_err(|_| format!("{method} {path}: {}", text.chars().take(200).collect::<String>()))?;
        if v["success"] == true { return Ok(v["result"].clone()) }
        let errors = v["errors"].as_array().map(|es| es.iter().map(|e| format!("{} ({})", e["message"].as_str().unwrap_or("?"), e["code"])).collect::<Vec<_>>().join("; ")).unwrap_or_default();
        Err(format!("{method} {path}: {errors}"))
    }

    /// One call as it is given (`superci debug cloudflare`, in a debug build only).
    #[cfg(debug_assertions)]
    pub fn debug_call(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> Result<Value> { self.call(method, path, body.map(|b| ("application/json", b))) }

    /// One query to Cloudflare's GraphQL Analytics API.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let asked = json!({ "query": query, "variables": variables }).to_string();
        let mut r = self.agent.post(&crate::aws::stand_in(&format!("{API}/graphql"))).header("authorization", &format!("Bearer {}", self.token)).header("content-type", "application/json")
            .send(asked.as_str()).map_err(|e| format!("analytics: {e}"))?;
        let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
        #[cfg(debug_assertions)]
        crate::aws::record(&superci_core::io::Request::new("POST", &format!("{API}/graphql")).with_body(asked.into_bytes()), r.status().as_u16(), &[], text.as_bytes());
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
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
        let plane_id = old_id.clone().unwrap_or_else(new_plane_id);
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
        let mut attempt = 0;
        loop {
            let mut tries = 0;
            let v = loop {
                match self.json("POST", &format!("/accounts/{a}/containers/image-preparations"), json!({ "image": image })) {
                    // The registry can take a few seconds before a just-copied image is visible.
                    Err(e) if e.contains("does not exist") || e.contains("DOESNT_CONTAIN") => {
                        crate::aws::pause(3000);
                        tries += 1;
                        if tries == 10 { return Err("Cloudflare does not see the runner image yet; try again in a minute".into()) }
                    }
                    r => break r?,
                }
            };
            match v["status"].as_str() {
                Some("ready") => return Ok(image),
                Some("error") => return Err(format!("Cloudflare could not prepare the runner image: {}", v["reason"].as_str().unwrap_or("no reason given"))),
                _ => { if attempt % 6 == 0 { eprintln!("Cloudflare is preparing the runner image for its machines…") } crate::aws::pause(5000) }
            }
            attempt += 1;
            if attempt == 120 { return Err("Cloudflare took over 10 minutes to prepare the runner image; try again later".into()) }
        }
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

    /// The Cloudflare launcher for one control plane: a Worker trusting only that control plane's tokens, with one
    /// container per job, sized for it. Safe to repeat. Returns its URL.
    pub fn deploy_runners(&self, a: &str, plane_url: &str, plane_id: &str) -> Result<String> {
        let name = format!("superci-runners-{plane_id}");
        // The control plane's public keys, pinned in the agent (a Worker cannot fetch another on the same workers.dev).
        let mut r = agent().get(&crate::aws::stand_in(&format!("{plane_url}/.well-known/jwks.json"))).call().map_err(|e| e.to_string())?;
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

    /// Removes a control plane's separate Cloudflare launcher: its container applications and its Worker.
    pub fn delete_runners(&self, a: &str, plane_id: &str) -> Result<()> {
        let name = format!("superci-runners-{plane_id}");
        self.delete_app(a, &name)?;
        self.delete_app(a, &jobs_app(&name))?;
        self.call("DELETE", &format!("/accounts/{a}/workers/scripts/{name}?force=true"), None).map(|_| ())
    }

    /// Deletes a control plane from the account: its container application (its containers stop), its Worker with its
    /// storage, and a launcher made for it. Safe to repeat.
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
        let mut attempt = 0;
        while let Err(e) = self.json("POST", &format!("/accounts/{a}/workers/scripts/{name}/subdomain"), json!({ "enabled": true, "previews_enabled": false })) {
            crate::aws::pause((2 + attempt) * 1000);
            attempt += 1;
            if attempt == 10 { return Err(e) }
        }
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

/// The port Cloudflare's sign-in comes back to: `OAUTH_PORT`; in a debug build, `SUPERCI_OAUTH_PORT` (a test's dashboard
/// on a port of its own, its Cloudflare the test kit's).
pub fn oauth_port() -> u16 {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var("SUPERCI_OAUTH_PORT").ok().and_then(|p| p.parse().ok()) { return p }
    OAUTH_PORT
}

pub fn oauth_redirect() -> String { format!("http://localhost:{}/oauth/callback", oauth_port()) }

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
    let mut r = agent().post(&crate::aws::stand_in("https://dash.cloudflare.com/oauth2/token")).header("content-type", "application/x-www-form-urlencoded").send(body.clone()).map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    #[cfg(debug_assertions)]
    crate::aws::record(&superci_core::io::Request::new("POST", "https://dash.cloudflare.com/oauth2/token").with_body(body.into_bytes()), status, &[], text.as_bytes());
    let _ = status;
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    let Some(access) = v["access_token"].as_str() else { return Err(format!("Cloudflare sign-in: {}", text.chars().take(200).collect::<String>())) };
    Ok(Session { access: access.into(), refresh: v["refresh_token"].as_str().map(str::to_string),
        expires_at_ms: now_ms() + v["expires_in"].as_u64().unwrap_or(3600) * 1000 })
}

/// Asks Cloudflare to end a sign-in (its token to renew with), as `wrangler logout` does. Whether Cloudflare said yes.
pub fn end_sign_in(refresh: &str) -> bool {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs([("client_id", OAUTH_CLIENT), ("token_type_hint", "refresh_token"), ("token", refresh)]).finish();
    let Ok(r) = quick().post(&crate::aws::stand_in("https://dash.cloudflare.com/oauth2/revoke")).header("content-type", "application/x-www-form-urlencoded").send(body.clone()) else { return false };
    #[cfg(debug_assertions)]
    let mut r = r;
    #[cfg(debug_assertions)]
    crate::aws::record(&superci_core::io::Request::new("POST", "https://dash.cloudflare.com/oauth2/revoke").with_body(body.into_bytes()), r.status().as_u16(), &[], r.body_mut().read_to_string().unwrap_or_default().as_bytes());
    r.status().is_success()
}

pub fn exchange(p: Pending, code: &str) -> Result<Session> {
    token_call(&[("grant_type", "authorization_code"), ("code", code), ("client_id", OAUTH_CLIENT), ("redirect_uri", &oauth_redirect()), ("code_verifier", &p.verifier)])
}

/// How it is said that a kept sign-in is over: Cloudflare no longer renews it (its error follows in brackets).
pub const EXPIRED: &str = "the Cloudflare sign-in expired: sign in again";

/// Whether an error says the sign-in itself is over: Cloudflare no longer renews it, or no longer takes its token.
pub fn over(error: &str) -> bool { error.contains(EXPIRED) || error.contains("(9109)") || error.contains("Invalid access token") }

impl Session {
    /// A token given to SuperCI by name (SUPERCI_CLOUDFLARE_TOKEN, for a machine with no browser) never expires here.
    pub fn from_token(token: &str) -> Self { Session { access: token.trim().into(), refresh: None, expires_at_ms: now_ms() + 365 * 86_400_000 } }

    /// A kept sign-in whose time is up and that nothing renews (Cloudflare is not asked), for tests.
    #[cfg(test)]
    pub fn expired_for_test() -> Self { Session { access: "test".into(), refresh: None, expires_at_ms: 0 } }

    /// What renews it, to ask Cloudflare to end it (`superci logout`).
    pub fn refresh_token(&self) -> Option<&str> { self.refresh.as_deref() }


    /// Whether its token is near its end (`client` would renew it).
    pub fn due(&self) -> bool { self.expires_at_ms < now_ms() + 300_000 }

    /// A client whose token is good for a few more minutes, renewing it when needed.
    pub fn client(&mut self) -> Result<Cloudflare> {
        if self.due() {
            let refresh = self.refresh.clone().ok_or(EXPIRED)?;
            let renewed = token_call(&[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", OAUTH_CLIENT)])
                // Cloudflare refusing what renews it: the sign-in is over. Cloudflare not answering leaves it as it is.
                .map_err(|e| if e.contains("invalid_grant") || e.contains("invalid_token") { format!("{EXPIRED} ({e})") } else { e })?;
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
    // One miss is not "not answering": a dropped connection or a passing error is asked again once, at once. (Not
    // after a time-out: a page never waits long on one that does not answer.)
    let began = std::time::Instant::now();
    health_once(plane_url).or_else(|| (began.elapsed() < Duration::from_secs(3)).then(|| health_once(plane_url)).flatten())
}

fn health_once(plane_url: &str) -> Option<Value> {
    let mut r = quick().get(&crate::aws::stand_in(&format!("{plane_url}/health"))).call().ok()?;
    if r.status().as_u16() != 200 { return None }
    serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()
}

/// What the key a folder keeps for its control planes begins with. Such a key is never sent anywhere as itself: each
/// control plane is handed, and asked with, a key of its own made from it and the control plane's address
/// (`plane_key`). So no control plane, and nobody who can read its settings in its cloud, holds a key that another
/// control plane takes. A key that only reads (`SUPERCI_KEY`) is one control plane's already and is sent as it is.
pub const FOLDER_KEY: &str = "superci-folder-";

pub fn plane_key(key: &str, plane_url: &str) -> String {
    if !key.starts_with(FOLDER_KEY) { return key.to_string() }
    superci_core::crypto::sha256_hex(format!("{key}\n{}", plane_url.trim_end_matches('/')).as_bytes())
}

/// Something the control plane looks up for the dashboard (GitLab's projects), asked with this session's key.
pub fn plane_get(plane_url: &str, key: &str, path: &str) -> Result<Value> {
    let key = &plane_key(key, plane_url);
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(40))).build().into();
    let mut r = agent.get(&crate::aws::stand_in(&format!("{plane_url}{path}"))).header("authorization", &format!("Bearer {key}")).call().map_err(|e| e.to_string())?;
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    // (409: what it cannot do as things stand, said as it is.)
    if r.status().as_u16() == 409 { return Err(format!("{}.", text.trim_end_matches('.'))) }
    if r.status().as_u16() != 200 { return Err(format!("the control plane answered {}: {}", r.status(), text.chars().take(200).collect::<String>())) }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// A change the control plane makes itself (no restart), asked with this session's key.
pub fn plane_post(plane_url: &str, key: &str, path: &str, body: &Value) -> Result<Value> {
    let key = &plane_key(key, plane_url);
    let mut r = quick().post(&crate::aws::stand_in(&format!("{plane_url}{path}"))).header("authorization", &format!("Bearer {key}")).header("content-type", "application/json")
        .send(body.to_string()).map_err(|e| e.to_string())?;
    if r.status().as_u16() != 200 { return Err(format!("the control plane answered {}", r.status())) }
    serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// A step of moving between control planes: with this session's key and the plane's one-time move token. Its status
/// too (403: the token is not there yet, or was used).
pub fn plane_move_call(plane_url: &str, key: &str, token: &str, path: &str, body: &Value) -> Result<(u16, Value)> {
    let key = &plane_key(key, plane_url);
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into();
    let mut r = agent.post(&crate::aws::stand_in(&format!("{plane_url}{path}"))).header("authorization", &format!("Bearer {key}")).header("x-move-token", token)
        .header("content-type", "application/json").send(body.to_string()).map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// The control plane's view for the dashboard (jobs, the App, the AWS connection), read with this session's key.
pub fn status(plane_url: &str, key: &str) -> Option<Value> { status_of(plane_url, key, "") }

/// The same, to see a setting take: without where the App is installed (which GitHub is asked for each time).
pub fn status_light(plane_url: &str, key: &str) -> Option<Value> { status_of(plane_url, key, "?light=1") }

/// Why a control plane that answers gave no status for this key: its words when it said it does not take the key
/// (401), else None (it answered otherwise, or not at all).
pub fn refused(plane_url: &str, key: &str) -> Option<String> {
    let key = &plane_key(key, plane_url);
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(10))).build().into();
    let mut r = agent.get(&crate::aws::stand_in(&format!("{plane_url}/status?light=1"))).header("authorization", &format!("Bearer {key}")).call().ok()?;
    let text = r.body_mut().read_to_string().ok()?;
    match r.status().as_u16() { 401 | 404 => Some(text), _ => None }
}

fn status_of(plane_url: &str, key: &str, query: &str) -> Option<Value> {
    let key = &plane_key(key, plane_url);
    // (Waited for longer than its health: a status is many reads of the control plane's store, and a busy one, or one
    // just started, takes more than `quick` allows. One that does not answer at all is told by its health.)
    let reading: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(30))).build().into();
    let mut r = reading.get(&crate::aws::stand_in(&format!("{plane_url}/status{query}"))).header("authorization", &format!("Bearer {key}")).call().ok()?;
    if r.status().as_u16() != 200 { return None }
    serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{now_ms, watch, Idle};
    use std::io::{Read, Write};
    use std::time::Instant;

    /// A Cloudflare launcher (deployed for this check only, trusting a key made here) starts a runner with a
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
mod answers {
    use super::*;
    use crate::aws::clouds::{self, answer, cf, cut, with};

    const API_AT: &str = "/api.cloudflare.com/client/v4";

    /// Cloudflare's refusal, as its API says one.
    fn refusal(message: &str) -> Vec<u8> { answer(400, &json!({ "success": false, "errors": [{ "code": 10000, "message": message }], "result": null }).to_string()) }

    /// GitHub's runner image already in the account's registry (`acc`): what copying it asks, answered.
    fn image_there(method: &str, path: &str) -> Option<Vec<u8>> {
        match (method, path) {
            ("GET", "/ghcr.io/v2/actions/actions-runner/manifests/latest") => Some(answer(200, r#"{"schemaVersion":2,"config":{"digest":"sha256:c","size":1},"layers":[]}"#)),
            ("POST", p) if p.ends_with("/registry.cloudflare.com/credentials") => Some(cf(json!({ "username": "u", "password": "p" }))),
            ("HEAD", "/registry.cloudflare.com/v2/acc/superci-runner/manifests/gh-latest") => Some(answer(200, "")),
            _ => None,
        }
    }

    /// A control plane slower to say its status than `quick` allows (a busy one: each is many reads of its store) is
    /// waited for: its status is read, not taken for a key it refused.
    #[test]
    fn a_busy_control_planes_status_is_waited_for_longer_than_its_health() {
        let at = clouds::serve(|_, path, _| match path {
            "/acme--superci-plane-p1.modal.run/status" => { std::thread::sleep(Duration::from_secs(9)); answer(200, r#"{"jobs":[]}"#) }
            _ => answer(404, "no"),
        });
        with(&at, || assert_eq!(status("https://acme--superci-plane-p1.modal.run", "k"), Some(json!({ "jobs": [] }))));
    }

    #[test]
    fn a_worker_moves_to_v3_from_where_it_is() {
        let m = |existing, tag| migration(existing, tag, &["PlaneObject"]);
        assert_eq!(m(false, Some("v2")).unwrap()["new_sqlite_classes"], json!(["PlaneObject", "JobRunner"]));
        assert_eq!(m(true, None).unwrap()["new_sqlite_classes"], json!(["PlaneObject", "JobRunner"]));
        assert_eq!(m(true, Some("v1")).unwrap()["old_tag"], "v1");
        assert_eq!(m(true, Some("v2")).unwrap()["old_tag"], "v2");
        assert_eq!(m(true, Some("v3")), None);
    }

    #[test]
    fn control_planes_are_found_by_their_health_or_else_by_their_settings() {
        let at = clouds::serve(|_, path, _| {
            let settings = |bindings: Value| cf(json!({ "bindings": bindings }));
            match path.strip_prefix(API_AT).unwrap_or(path) {
                "/accounts/a1/workers/scripts" => cf(json!([
                    { "id": "p1", "named_handlers": [{ "name": "PlaneObject" }] }, { "id": "p2", "named_handlers": [{ "name": "PlaneObject" }] },
                    { "id": "p3", "named_handlers": [{ "name": "PlaneObject" }] }, { "id": "p4", "named_handlers": [{ "name": "PlaneObject" }] },
                    { "id": "site", "named_handlers": [{ "name": "Other" }] }])),
                "/accounts/a1/workers/subdomain" => cf(json!({ "subdomain": "acme" })),
                // One answers as a control plane; one answers, but not as one (its settings say which it is); one
                // does not answer and its settings cannot be read; one has settings without an id.
                "/p1.acme.workers.dev/health" => answer(200, r#"{"plane":"plane0000001","label":"Ours"}"#),
                "/p2.acme.workers.dev/health" => answer(200, r#"{"ok":true}"#),
                "/accounts/a1/workers/scripts/p2/settings" => settings(json!([{ "name": "PLANE_ID", "text": "plane0000002" }])),
                "/accounts/a1/workers/scripts/p3/settings" => refusal("Internal server error"),
                "/accounts/a1/workers/scripts/p4/settings" => settings(json!([{ "name": "LABEL", "text": "x" }])),
                _ => answer(500, "no"),
            }
        });
        with(&at, || {
            let c = Cloudflare::new("t");
            let mut found: Vec<(String, String)> = c.find_planes(&[("a1".into(), "Acme".into())]).unwrap().into_iter().map(|p| match p {
                crate::plane::Plane::Cloudflare { plane_id, label, .. } => (plane_id, label),
                _ => unreachable!(),
            }).collect();
            found.sort();
            assert_eq!(found, [("plane0000001".to_string(), "Ours".to_string()), ("plane0000002".to_string(), "superci".to_string())]);
            assert!(c.find_planes(&[]).unwrap().is_empty());
        });
    }

    #[test]
    fn an_answer_cut_off_or_with_a_header_in_other_bytes_is_read_as_far_as_it_goes() {
        // (A header's value that is not text is left out of what a recording keeps.)
        let at = clouds::serve(|_, path, _| match path {
            p if p.ends_with("/accounts?per_page=50") => {
                let body = json!({ "success": true, "errors": [], "result": [{ "id": "a1", "name": "Acme" }] }).to_string();
                let mut a = format!("HTTP/1.1 200 X\r\ncontent-type: application/json\r\nx-odd: ").into_bytes();
                a.extend_from_slice(b"\xe9t\xe9");
                a.extend_from_slice(format!("\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
                a
            }
            _ => cut(200),
        });
        with(&at, || {
            let c = Cloudflare::new("t");
            assert_eq!(c.accounts().unwrap(), [("a1".to_string(), "Acme".to_string())]);
            assert!(c.secret_names("a1", "superci").is_err());
        });
    }

    #[test]
    fn analytics_not_reached_cut_off_or_not_json_is_an_error_and_no_groups_cost_nothing() {
        with(&clouds::closed(), || assert!(Cloudflare::new("t").container_costs("a1", &[], 0, 1).unwrap_err().starts_with("analytics: ")));
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let k = n.clone();
        let at = clouds::serve(move |_, _, _| match k.fetch_add(1, std::sync::atomic::Ordering::SeqCst) { 0 => cut(200), 1 => answer(200, "not json"), _ => answer(200, r#"{"data":{"viewer":{"accounts":[{}]}}}"#) });
        with(&at, || {
            let c = Cloudflare::new("t");
            assert!(c.container_costs("a1", &[], 0, 1).is_err());
            assert!(c.container_costs("a1", &[], 0, 1).is_err());
            assert!(c.container_costs("a1", &["x".into()], 0, 1).unwrap().is_empty());
        });
    }

    /// A Worker at v2 whose container applications cannot be listed: its update stops there, for the control plane and
    /// for a launcher alike.
    #[test]
    fn a_v2_worker_whose_applications_cannot_be_listed_is_not_updated() {
        let at = clouds::serve(|method, path, _| {
            if let Some(a) = image_there(method, path) { return a }
            match path.strip_prefix(API_AT).unwrap_or(path) {
                "/accounts/acc/workers/scripts/superci/settings" | "/accounts/acc/workers/scripts/superci-runners-p1/settings" => cf(json!({ "bindings": [] })),
                "/accounts/acc/workers/scripts" => cf(json!([{ "id": "superci", "migration_tag": "v2" }, { "id": "superci-runners-p1", "migration_tag": "v2" }])),
                "/accounts/acc/containers/image-preparations" => cf(json!({ "status": "ready" })),
                "/plane.acme.workers.dev/.well-known/jwks.json" => answer(200, r#"{"keys":[]}"#),
                "/accounts/acc/containers/applications" => refusal("Applications are not available"),
                _ => answer(500, "no"),
            }
        });
        with(&at, || {
            let c = Cloudflare::new("t");
            assert!(c.deploy("acc", "superci", "superci", false).err().unwrap().contains("Applications are not available"));
            assert!(c.deploy_runners("acc", "https://plane.acme.workers.dev", "p1").unwrap_err().contains("Applications are not available"));
        });
    }

    #[test]
    fn a_launchers_control_plane_whose_keys_are_cut_off_gets_no_launcher() {
        with(&clouds::serve(|_, _, _| cut(200)), || assert!(Cloudflare::new("t").deploy_runners("acc", "https://plane.acme.workers.dev", "p1").is_err()));
    }

    /// The runner image: not seen by Cloudflare after ten tries, or not prepared in ten minutes, is said as that.
    #[test]
    fn a_runner_image_cloudflare_never_sees_or_never_prepares_is_given_up_on() {
        for (status, said) in [(None, "does not see the runner image yet"), (Some("preparing"), "took over 10 minutes")] {
            let at = clouds::serve(move |method, path, _| {
                if let Some(a) = image_there(method, path) { return a }
                match status {
                    None => answer(404, r#"{"success":false,"errors":[{"code":1,"message":"image does not exist"}]}"#),
                    Some(s) => cf(json!({ "status": s })),
                }
            });
            with(&at, || assert!(Cloudflare::new("t").runner_image("acc").unwrap_err().contains(said), "{said}"));
        }
    }

    #[test]
    fn container_applications_without_the_runners_namespace_are_not_made() {
        let at = clouds::serve(|_, path, _| if path.ends_with("/containers/applications") { cf(json!([])) } else { cf(json!([{ "script": "other", "class": "JobRunner", "id": "n1" }])) });
        with(&at, || assert!(Cloudflare::new("t").jobs_application("acc", "superci-jobs", "superci").unwrap_err().contains("did not appear")));
    }

    #[test]
    fn registry_credentials_without_a_user_or_a_password_are_refused() {
        for (given, said) in [(json!({ "password": "p" }), "no registry user"), (json!({ "username": "u" }), "no registry password")] {
            with(&clouds::serve(move |_, _, _| cf(given.clone())), || assert_eq!(Cloudflare::new("t").registry_push_credentials("acc").unwrap_err(), said));
        }
    }

    /// A workers.dev address turned on after a refusal; then the account's subdomain not read, or none yet.
    #[test]
    fn a_workers_dev_address_is_asked_again_and_the_accounts_subdomain_must_be_there() {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let k = n.clone();
        let at = clouds::serve(move |_, path, _| {
            if path.ends_with("/scripts/superci/subdomain") { return if k.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % 2 == 0 { refusal("does not exist") } else { cf(json!({})) } }
            if k.load(std::sync::atomic::Ordering::SeqCst) <= 2 { refusal("Subdomain unavailable") } else { cf(json!({})) }
        });
        with(&at, || {
            let c = Cloudflare::new("t");
            assert!(c.workers_dev("acc", "superci").unwrap_err().contains("Subdomain unavailable"));
            assert!(c.workers_dev("acc", "superci").unwrap_err().contains("no workers.dev subdomain"));
        });
        // Refused ten times: the last refusal is the answer.
        with(&clouds::serve(|_, _, _| refusal("Subdomain unavailable")), || assert!(Cloudflare::new("t").workers_dev("acc", "superci").unwrap_err().contains("Subdomain unavailable")));
    }

    #[test]
    fn a_launcher_whose_applications_cannot_be_listed_is_not_removed() {
        with(&clouds::serve(|_, _, _| refusal("Applications are not available")), || assert!(Cloudflare::new("t").delete_runners("acc", "p1").unwrap_err().contains("Applications are not available")));
    }

    #[test]
    fn a_workers_secrets_are_named() {
        with(&clouds::serve(|_, _, _| cf(json!([{ "name": "GITHUB_APP" }, { "type": "secret_text" }]))), || assert_eq!(Cloudflare::new("t").secret_names("acc", "superci").unwrap(), ["GITHUB_APP"]));
    }

    #[test]
    fn a_sign_in_cloudflare_does_not_answer_or_answers_cut_off_is_an_error_and_not_ended() {
        let (_, p) = authorize();
        with(&clouds::closed(), || {
            assert!(exchange(Pending { verifier: p.verifier.clone(), state: p.state.clone() }, "code").is_err());
            assert!(!end_sign_in("refresh"));
        });
        with(&clouds::serve(|_, _, _| cut(200)), || assert!(exchange(p, "code").is_err()));
    }

    /// A control plane's answers cut off midway, not JSON, or not given at all: nothing taken from them.
    #[test]
    fn a_control_planes_answers_cut_off_or_not_json_are_not_taken() {
        let plane = clouds::serve(|_, _, _| cut(200));
        assert_eq!(health(&plane), None);
        assert!(plane_get(&plane, "k", "/x").is_err());
        assert!(plane_post(&plane, "k", "/x", &json!({})).is_err());
        assert!(plane_move_call(&plane, "k", "t", "/x", &json!({})).is_err());
        assert_eq!(refused(&plane, "k"), None);
        assert_eq!(status(&plane, "k"), None);
        let not_json = clouds::serve(|_, _, _| answer(200, "not json"));
        assert!(plane_get(&not_json, "k", "/x").is_err());
        assert!(plane_post(&not_json, "k", "/x", &json!({})).is_err());
        assert_eq!(refused(&clouds::closed(), "k"), None);
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
