//! GitLab (gitlab.com) as the control plane and the dashboard find it, for a project `acme/app` that a connection's
//! token maintains. Its jobs and the project are answered in the shape gitlab.com answered for a public project
//! (`fixtures/gitlab/{job,project}.json`, recorded and made Acme's); what only a token may ask (the user, the token's
//! scopes, runners, webhooks, a job's log) as GitLab's API documentation gives it. The test plays GitLab's side of a
//! job (`pending`, `running`, `finish`): its state here, and the job event its project's webhook sends (`Job events`
//! in GitLab's documentation), with the webhook's secret.
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use superci_core::io::{Request, Response};

pub const URL: &str = "https://gitlab.com";
pub const PROJECT: u64 = 250_833;
pub const PATH: &str = "acme/app";
pub const TOKEN: &str = "gitlab-token-for-tests";
pub const HOOK_SECRET: &str = "gitlab-hook-secret-for-tests";

#[derive(Default)]
struct Lab {
    jobs: BTreeMap<u64, Value>,
    logs: BTreeMap<u64, String>,
    /// Runners made for jobs: id to (its tags, paused).
    runners: BTreeMap<u64, (Vec<String>, bool)>,
    hooks: BTreeMap<u64, Value>,
    next: u64,
    /// Calls answered with an error (`fail`): the method, a part of the address, the status.
    failing: Vec<(String, String, u16)>,
    /// The token's scopes as GitLab says them, when not every one SuperCI asks for (`scopes`).
    scopes: Option<Vec<String>>,
    /// The token reaches no project (`reach_no_project`).
    no_project: bool,
    /// The token's user maintains no project (`maintains_none`).
    maintains_none: bool,
    /// What each runner's token ends with (`runner_tokens_end_with`).
    token_end: String,
}

#[derive(Clone, Default)]
pub struct GitLab { lab: Arc<Mutex<Lab>>, pub asked: Arc<Mutex<Vec<String>>> }

fn fixture(name: &str) -> Value {
    serde_json::from_str(match name { "project" => include_str!("../fixtures/gitlab/project.json"), _ => include_str!("../fixtures/gitlab/job.json") }).unwrap()
}

fn iso(ms: u64) -> String { format!("{}.{:03}Z", crate::github::iso(ms).trim_end_matches('Z'), ms % 1000) }

impl GitLab {
    /// The connection as the dashboard keeps it in the control plane's settings (`GITLAB`).
    pub fn connection() -> Value { json!({ "url": URL, "token": TOKEN, "hook_secret": HOOK_SECRET }) }

    /// A call (its method, and a part of its address) answered with this status from now on; `fail_none` clears them.
    pub fn fail(&self, method: &str, part: &str, status: u16) { self.lab.lock().unwrap().failing.push((method.into(), part.into(), status)) }

    pub fn fail_none(&self) { self.lab.lock().unwrap().failing.clear() }

    /// The token's scopes as GitLab says them from now on (a token made with fewer than SuperCI asks for).
    pub fn scopes(&self, scopes: &[&str]) { self.lab.lock().unwrap().scopes = Some(scopes.iter().map(|s| s.to_string()).collect()) }

    pub fn job(&self, id: u64) -> Value { self.lab.lock().unwrap().jobs.get(&id).cloned().unwrap_or_default() }

    /// A field of a job set as given (a tag's pipeline: `tag`, true), as GitLab would have it.
    pub fn set(&self, id: u64, field: &str, value: Value) { if let Some(j) = self.lab.lock().unwrap().jobs.get_mut(&id) { j[field] = value } }

    /// The job event GitLab sends of a job as it stands now.
    fn event(&self, id: u64) -> Vec<u8> {
        let (j, p) = (self.job(id), fixture("project"));
        let runner = { let lab = self.lab.lock().unwrap(); j["runner"]["id"].as_u64().map(|r| json!({ "id": r, "description": format!("runner {r}"), "runner_type": "project_type", "active": true, "is_shared": false, "tags": lab.runners.get(&r).map(|x| x.0.clone()).unwrap_or_default() })).unwrap_or(Value::Null) };
        json!({ "object_kind": "build", "ref": j["ref"], "tag": false, "before_sha": "0000000000000000000000000000000000000000", "sha": j["commit"]["id"], "retries_count": 0,
            "build_id": id, "build_name": j["name"], "build_stage": j["stage"], "build_status": j["status"], "build_created_at": j["created_at"], "build_started_at": j["started_at"],
            "build_finished_at": j["finished_at"], "build_duration": j["duration"], "build_queued_duration": j["queued_duration"], "build_allow_failure": false, "build_failure_reason": j["failure_reason"],
            "pipeline_id": j["pipeline"]["id"], "runner": runner, "project_id": PROJECT, "project_name": "Acme / app",
            "user": { "id": 1, "name": "Mona Lisa", "username": "mona", "avatar_url": j["user"]["avatar_url"], "email": "[REDACTED]" },
            "commit": { "id": j["pipeline"]["id"], "name": null, "sha": j["commit"]["id"], "message": j["commit"]["message"], "author_name": "Mona Lisa", "author_email": "mona@example.com", "status": j["status"] },
            "repository": { "name": "app", "url": format!("git@gitlab.com:{PATH}.git"), "description": p["description"], "homepage": p["web_url"], "git_http_url": p["http_url_to_repo"], "git_ssh_url": p["ssh_url_to_repo"], "visibility_level": 0 },
            "project": { "id": PROJECT, "name": "app", "description": p["description"], "web_url": p["web_url"], "avatar_url": null, "git_ssh_url": p["ssh_url_to_repo"], "git_http_url": p["http_url_to_repo"],
                "namespace": "Acme", "visibility_level": 0, "path_with_namespace": PATH, "default_branch": p["default_branch"], "ci_config_path": "" },
            "environment": null }).to_string().into_bytes()
    }

    /// A job of `acme/app` waiting for a runner with these tags: its id, and the event.
    pub fn pending(&self, name: &str, tags: &[&str], now_ms: u64) -> (u64, Vec<u8>) {
        let id = {
            let mut lab = self.lab.lock().unwrap();
            lab.next += 1;
            let id = 17_000_000_000 + lab.next;
            let mut j = fixture("job");
            [("id", json!(id)), ("name", json!(name)), ("stage", json!("test")), ("tag_list", json!(tags)), ("status", json!("pending")), ("created_at", json!(iso(now_ms))),
                ("started_at", Value::Null), ("finished_at", Value::Null), ("duration", Value::Null), ("queued_duration", Value::Null), ("failure_reason", Value::Null), ("runner", Value::Null),
                ("web_url", json!(format!("{URL}/{PATH}/-/jobs/{id}")))].into_iter().for_each(|(k, v)| j[k] = v);
            lab.jobs.insert(id, j);
            id
        };
        (id, self.event(id))
    }

    /// The runner made for it took it.
    pub fn running(&self, id: u64, now_ms: u64) -> Vec<u8> {
        {
            let mut lab = self.lab.lock().unwrap();
            let runner = *lab.runners.keys().next_back().expect("a runner made for it");
            let j = lab.jobs.get_mut(&id).unwrap();
            [("status", json!("running")), ("started_at", json!(iso(now_ms))), ("queued_duration", json!(1.2)), ("runner", json!({ "id": runner, "description": format!("runner {runner}"), "active": true, "paused": false, "is_shared": false, "runner_type": "project_type", "name": "gitlab-runner", "online": true, "status": "online" }))].into_iter().for_each(|(k, v)| j[k] = v);
        }
        self.event(id)
    }

    /// Taken by this runner (any made here), as GitLab gives a job to any runner with its tags: its event.
    pub fn running_on(&self, id: u64, runner: u64, now_ms: u64) -> Vec<u8> {
        {
            let mut lab = self.lab.lock().unwrap();
            let j = lab.jobs.get_mut(&id).unwrap();
            [("status", json!("running")), ("started_at", json!(iso(now_ms))), ("queued_duration", json!(1.2)), ("runner", json!({ "id": runner, "description": format!("runner {runner}"), "active": true, "paused": false, "is_shared": false, "runner_type": "project_type", "name": "gitlab-runner", "online": true, "status": "online" }))].into_iter().for_each(|(k, v)| j[k] = v);
        }
        self.event(id)
    }

    /// Its log so far, while it runs.
    pub fn trace(&self, id: u64, log: &str) { self.lab.lock().unwrap().logs.insert(id, log.to_string()); }

    /// It ended (`success`, `failed`), with its log.
    pub fn finish(&self, id: u64, status: &str, log: &str, now_ms: u64) -> Vec<u8> {
        {
            let mut lab = self.lab.lock().unwrap();
            let j = lab.jobs.get_mut(&id).unwrap();
            [("status", json!(status)), ("finished_at", json!(iso(now_ms))), ("duration", json!(7.5)), ("failure_reason", if status == "failed" { json!("script_failure") } else { Value::Null })].into_iter().for_each(|(k, v)| j[k] = v);
            lab.logs.insert(id, log.to_string());
        }
        self.event(id)
    }

    pub(crate) fn answer(&self, r: &Request) -> Option<Response> {
        let url = url::Url::parse(&r.url).ok()?;
        if url.host_str() != Some("gitlab.com") { return None }
        self.asked.lock().unwrap().push(format!("{} {}", r.method, r.url));
        if r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("private-token")).map(|(_, v)| v.as_str()) != Some(TOKEN) {
            return Some(Response::new(401, "application/json", json!({ "message": "401 Unauthorized" }).to_string()))
        }
        let ok = |status: u16, v: Value| Some(Response::new(status, "application/json", v.to_string()));
        let form: BTreeMap<String, String> = url::form_urlencoded::parse(&r.body).into_owned().collect();
        let path = url.path().trim_start_matches("/api/v4/").to_string();
        let segs: Vec<&str> = path.split('/').collect();
        let project = PROJECT.to_string();
        let mut lab = self.lab.lock().unwrap();
        if let Some((_, _, status)) = lab.failing.iter().find(|(m, part, _)| *m == r.method && r.url.contains(part.as_str())) {
            return Some(Response::new(*status, "application/json", json!({ "message": format!("{status} refused") }).to_string()))
        }
        match (r.method.as_str(), segs.as_slice()) {
            // Who the token is, and what it may do (docs.gitlab.com/api/users, /api/personal_access_tokens).
            ("GET", ["user"]) => ok(200, json!({ "id": 1, "username": "mona", "name": "Mona Lisa", "state": "active", "locked": false, "avatar_url": "https://www.gravatar.com/avatar/00000000000000000000000000000000?s=80&d=identicon",
                "web_url": format!("{URL}/mona"), "created_at": "2026-01-01T00:00:00.000Z", "bot": false, "public_email": null, "email": "mona@example.com", "is_admin": false })),
            ("GET", ["personal_access_tokens", "self"]) => ok(200, json!({ "id": 4, "name": "SuperCI", "revoked": false, "created_at": "2026-10-01T10:00:00.000Z", "description": "",
                "scopes": lab.scopes.clone().unwrap_or_else(|| vec!["api".into(), "create_runner".into(), "manage_runner".into()]), "user_id": 1, "last_used_at": "2026-10-15T10:00:00.000Z", "active": true, "expires_at": "2027-10-01" })),
            ("GET", ["projects"]) if lab.no_project => ok(200, json!([])),
            ("GET", ["projects"]) if lab.maintains_none => ok(200, json!([])),
            ("GET", ["projects"]) => {
                let p = fixture("project");
                ok(200, json!([{ "id": PROJECT, "description": p["description"], "name": "app", "name_with_namespace": "Acme / app", "path": "app", "path_with_namespace": PATH,
                    "created_at": p["created_at"], "default_branch": p["default_branch"], "web_url": p["web_url"], "last_activity_at": p["last_activity_at"], "namespace": p["namespace"] }]))
            }
            // Its webhooks (docs.gitlab.com/api/project_webhooks).
            ("GET", ["projects", id, "hooks"]) if *id == project => ok(200, Value::Array(lab.hooks.values().cloned().collect())),
            ("POST", ["projects", id, "hooks"]) if *id == project => {
                lab.next += 1;
                let hid = lab.next;
                let hook = json!({ "id": hid, "url": form.get("url"), "name": "", "description": "", "project_id": PROJECT, "push_events": false, "job_events": true, "enable_ssl_verification": true,
                    "alert_status": "executable", "disabled_until": null, "created_at": "2026-10-15T10:00:00.000Z" });
                lab.hooks.insert(hid, hook.clone());
                ok(201, hook)
            }
            ("PUT", ["projects", id, "hooks", hid]) if *id == project => lab.hooks.get(&hid.parse().ok()?).cloned().map(|h| Response::new(200, "application/json", h.to_string())),
            ("DELETE", ["projects", id, "hooks", hid]) if *id == project => { lab.hooks.remove(&hid.parse().ok()?)?; Some(Response::new(204, "text/plain", "")) }
            // Its jobs (docs.gitlab.com/api/jobs).
            ("GET", ["projects", id, "jobs"]) if *id == project => ok(200, Value::Array(lab.jobs.values().filter(|j| j["status"] == "pending").cloned().collect())),
            ("GET", ["projects", id, "jobs", job]) if *id == project => lab.jobs.get(&job.parse().ok()?).cloned().map(|j| Response::new(200, "application/json", j.to_string())),
            ("GET", ["projects", id, "jobs", job, "trace"]) if *id == project => {
                let job: u64 = job.parse().ok()?;
                Some(Response::new(200, "text/plain", lab.logs.get(&job).cloned().unwrap_or_default()))
            }
            ("POST", ["projects", id, "jobs", job, "cancel" | "retry"]) if *id == project => lab.jobs.get(&job.parse().ok()?).cloned().map(|j| Response::new(201, "application/json", j.to_string())),
            // Runners made for its jobs (docs.gitlab.com/api/users#create-a-runner-linked-to-a-user, /api/runners).
            ("POST", ["user", "runners"]) => {
                lab.next += 1;
                let rid = 55_000_000 + lab.next;
                let tags: Vec<String> = form.get("tag_list").map(|t| t.split(',').map(str::to_string).collect()).unwrap_or_default();
                lab.runners.insert(rid, (tags, false));
                ok(201, json!({ "id": rid, "token": format!("runner-token-for-tests-{rid}{}", lab.token_end), "token_expires_at": null }))
            }
            ("PUT", ["runners", rid]) => {
                let rid: u64 = rid.parse().ok()?;
                let runner = lab.runners.get_mut(&rid)?;
                runner.1 = form.get("paused").is_some_and(|p| p == "true");
                ok(200, json!({ "id": rid, "paused": runner.1, "active": !runner.1, "is_shared": false, "runner_type": "project_type", "tag_list": runner.0 }))
            }
            ("DELETE", ["runners", rid]) => { lab.runners.remove(&rid.parse().ok()?); Some(Response::new(204, "text/plain", "")) }
            ("GET", ["runners", rid, "jobs"]) => {
                let rid: u64 = rid.parse().ok()?;
                // (As GitLab lists a runner's jobs: each with its project.)
                let p = fixture("project");
                ok(200, Value::Array(lab.jobs.values().filter(|j| j["runner"]["id"] == rid && j["status"] == "running").cloned()
                    .map(|mut j| { j["project"] = json!({ "id": PROJECT, "name": "app", "path_with_namespace": PATH, "default_branch": p["default_branch"] }); j }).collect()))
            }
            _ => None,
        }
    }

    /// From now on the token reaches no project (`false`: `acme/app` again), as GitLab lists them then: none.
    pub fn reach_no_project(&self, none: bool) { self.lab.lock().unwrap().no_project = none }
    /// From now on the token's user maintains no project: GitLab lists none to turn on.
    pub fn maintains_none(&self) { self.lab.lock().unwrap().maintains_none = true }
    /// From now on each runner made is given a token that ends with `end` (one GitLab might give, of characters a
    /// shell reads).
    pub fn runner_tokens_end_with(&self, end: &str) { self.lab.lock().unwrap().token_end = end.to_string() }
}
