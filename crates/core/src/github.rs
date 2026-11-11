//! The GitHub side of a control plane: the private App it creates for itself (manifest flow), App and installation tokens,
//! just-in-time runner registrations, and which `workflow_job` events are its own. Organizations get org-level runners
//! (the narrow `organization_self_hosted_runners` permission); personal accounts can only have repository-level
//! runners, which need `administration: write`.
use serde::{Deserialize, Serialize};

use crate::spec::Spec;
use crate::crypto::rs256_jwt;
use crate::io::{Http, Request, Result};

/// Where GitHub is: github.com (no host), a GitHub Enterprise Server ("github.example.com"), or GitHub Enterprise
/// Cloud with data residency ("acme.ghe.com"). Its API: api.github.com, HOST/api/v3, or api.SUBDOMAIN.ghe.com.
pub fn api_base(host: Option<&str>) -> String {
    match host {
        None => "https://api.github.com".into(),
        Some(h) if h.ends_with(".ghe.com") => format!("https://api.{h}"),
        Some(h) => format!("https://{h}/api/v3"),
    }
}

/// Its pages (an App's settings, a job).
pub fn web_base(host: Option<&str>) -> String { format!("https://{}", host.unwrap_or("github.com")) }

/// Where an App is installed from: GitHub Enterprise Server keeps Apps' pages under /github-apps.
pub fn install_link(host: Option<&str>, slug: &str) -> String {
    let apps = match host { Some(h) if !h.ends_with(".ghe.com") => "github-apps", _ => "apps" };
    format!("{}/{apps}/{slug}/installations/new", web_base(host))
}

/// A GitHub host as typed ("https://GitHub.example.com/" too): its name alone, or none for github.com itself. An
/// error for what is not a host name.
pub fn host_of(typed: &str) -> Result<Option<String>> {
    let h = typed.trim().trim_start_matches("https://").trim_end_matches('/').to_ascii_lowercase();
    if h.is_empty() || h == "github.com" || h == "www.github.com" { return Ok(None) }
    let ok = h.len() <= 253 && h.contains('.') && !h.starts_with(['.', '-']) && !h.ends_with(['.', '-']) && !h.contains("..")
        && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    if ok { Ok(Some(h)) } else { Err(format!("{typed} is not a host name (like github.example.com)")) }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Owner {
    pub org: bool,
    pub login: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct App {
    pub id: u64,
    pub slug: String,
    pub pem: String,
    pub webhook_secret: String,
    pub owner: String,
    pub owner_is_org: bool,
    /// Where its GitHub is, when not github.com (see `api_base`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl App {
    pub fn api(&self) -> String { api_base(self.host.as_deref()) }
}

pub fn valid_login(login: &str) -> bool {
    !login.is_empty() && login.len() <= 39 && login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !login.starts_with('-')
}

/// The App a control plane asks GitHub to create: private, webhook to the control plane, only the permissions runners need.
/// The App's name starts with what the control plane is called (SuperCI, or its own name: AcmeCI acme …); the person
/// can change it on GitHub's page before it is made.
pub fn app_manifest(plane_url: &str, owner: &Owner, called: &str, label: &str) -> serde_json::Value {
    let name = app_name(called, &owner.login);
    let permissions = if owner.org {
        serde_json::json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" })
    } else {
        serde_json::json!({ "administration": "write", "actions": "write", "metadata": "read" })
    };
    serde_json::json!({
        "name": name,
        "url": plane_url,
        "description": format!("Runs GitHub Actions jobs labelled `{label}` on your own clouds (plane: {plane_url})."),
        "hook_attributes": { "url": format!("{plane_url}/webhook"), "active": true },
        "redirect_url": format!("{plane_url}/github/callback"),
        "setup_url": format!("{plane_url}/github/installed"),
        "setup_on_update": true,
        "public": false,
        "default_permissions": permissions,
        "default_events": ["workflow_job"],
    })
}

/// What the App is called on GitHub: the CI's name and whose jobs it is for ("AcmeCI acme"). It is what owners read
/// in the organization's list of Apps, and what a job run again by SuperCI says ran it (`acmeci-acme[bot]`). Names
/// are unique across all of GitHub and 34 characters at most. This one can only be taken by an App of the same CI
/// for the same account (one from before, never deleted there): GitHub then says so on the page where the App is
/// created, and another name typed there does as well (the App is known here by its id).
pub fn app_name(called: &str, login: &str) -> String {
    // (`called` is the CI's name: SuperCI unless named.)
    let brand = called;
    // A long login is cut, with a mark of the whole of it (two organizations may begin alike).
    let login = if login.len() > 15 { format!("{}-{}", login.chars().take(10).collect::<String>(), &crate::crypto::sha256_hex(login.to_ascii_lowercase().as_bytes())[..4]) } else { login.to_string() };
    let brand: String = brand.chars().take(33usize.saturating_sub(login.chars().count())).collect();
    format!("{} {login}", brand.trim_end())
}

/// Where the manifest form is posted: the organization's or the user's own App settings.
pub fn manifest_target(host: Option<&str>, owner: &Owner, state: &str) -> String {
    let web = web_base(host);
    let base = if owner.org { format!("{web}/organizations/{}/settings/apps/new", owner.login) } else { format!("{web}/settings/apps/new") };
    format!("{base}?state={state}")
}

async fn gh(http: &dyn Http, api: &str, method: &str, path: &str, token: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
    let mut req = Request::new(method, &format!("{api}{path}"))
        .with_header("accept", "application/vnd.github+json")
        .with_header("user-agent", "superci-plane")
        .with_header("x-github-api-version", "2022-11-28")
        .with_header("authorization", &format!("Bearer {token}"));
    if let Some(b) = body { req = req.with_header("content-type", "application/json").with_body(b.to_string()); }
    let r = http.send(req).await?;
    if r.status >= 300 { return Err(format!("GitHub {method} {path}: {} {}", r.status, r.body_text().chars().take(300).collect::<String>())); }
    if r.status == 204 || r.body.is_empty() { return Ok(serde_json::Value::Null); }
    serde_json::from_slice(&r.body).map_err(|e| format!("GitHub {path}: {e}"))
}

/// A call made with the App's own key (a JWT of it, good for minutes).
async fn as_app(http: &dyn Http, app: &App, method: &str, path: &str, now_ms: u64, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
    gh(http, &app.api(), method, path, &app_jwt(app, now_ms)?, body).await
}

pub fn app_jwt(app: &App, now_ms: u64) -> Result<String> {
    let iat = now_ms / 1000 - 60;
    rs256_jwt(&serde_json::json!({ "iat": iat, "exp": iat + 540, "iss": app.id.to_string() }), &app.pem)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Installation {
    pub id: u64,
    pub account: String,
    pub selection: String,
    /// The permissions this installation was given (accepted): name to "read" or "write".
    #[serde(default)]
    pub permissions: serde_json::Value,
}

pub async fn installations(http: &dyn Http, app: &App, now_ms: u64) -> Result<Vec<Installation>> {
    let r = as_app(http, app, "GET", "/app/installations", now_ms, None).await?;
    Ok(r.as_array().map(|a| a.iter().map(|i| Installation {
        id: i["id"].as_u64().unwrap_or_default(),
        account: i["account"]["login"].as_str().unwrap_or_default().to_string(),
        selection: i["repository_selection"].as_str().unwrap_or_default().to_string(),
        permissions: i["permissions"].clone(),
    }).collect()).unwrap_or_default())
}

/// The public repositories an installation can see (its jobs come only from repositories it is installed on).
pub async fn public_repositories(http: &dyn Http, api: &str, token: &str) -> Result<Vec<String>> {
    // (Ten pages at most: a thousand repositories.)
    let (mut out, mut page) = (vec![], 0);
    loop {
        page += 1;
        let r = gh(http, api, "GET", &format!("/installation/repositories?per_page=100&page={page}"), token, None).await?;
        let repos = r["repositories"].as_array().cloned().unwrap_or_default();
        out.extend(repos.iter().filter(|x| x["private"] == false).filter_map(|x| x["full_name"].as_str().map(str::to_string)));
        if repos.len() < 100 || page == 10 { return Ok(out) }
    }
}

/// The App as GitHub has it now: what it asks for, and its name and address (`slug`), which its owner can change on
/// GitHub at any time.
pub async fn app_now(http: &dyn Http, app: &App, now_ms: u64) -> Result<serde_json::Value> {
    as_app(http, app, "GET", "/app", now_ms, None).await
}

pub async fn installation_token(http: &dyn Http, app: &App, installation_id: u64, now_ms: u64) -> Result<String> {
    let r = as_app(http, app, "POST", &format!("/app/installations/{installation_id}/access_tokens"), now_ms, Some(serde_json::json!({}))).await?;
    r["token"].as_str().map(str::to_string).ok_or_else(|| "installation token missing".into())
}

/// A job's log, whole, as GitHub keeps it once the job has ended. GitHub answers with where it is (a link good for a
/// minute, to be fetched without the token); a client that follows by itself arrives with the log.
/// Where GitHub keeps a job's whole log: a link of its own, good for a minute or so (any size, nothing through here).
pub async fn job_log_link(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<String> {
    logs_link(http, &format!("{api}/repos/{repo}/actions/jobs/{job_id}/logs"), token).await
}

/// Where GitHub keeps the logs of a run's attempt (all of them: the latest), as a zip with a folder for each job and a
/// file for each step in it, named by the step's number and name: how steps run side by side are told apart.
pub async fn run_logs_link(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64, attempt: Option<u32>) -> Result<String> {
    let at = attempt.map(|n| format!("/attempts/{n}")).unwrap_or_default();
    logs_link(http, &format!("{api}/repos/{repo}/actions/runs/{run_id}{at}/logs"), token).await
}

async fn logs_link(http: &dyn Http, url: &str, token: &str) -> Result<String> {
    let ask = Request::new("GET", url)
        .with_header("accept", "application/vnd.github+json").with_header("user-agent", "superci-plane").with_header("x-github-api-version", "2022-11-28")
        .with_header("authorization", &format!("Bearer {token}")).without_following();
    let r = http.send(ask).await?;
    match r.status {
        301 | 302 | 303 | 307 | 308 => r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()).ok_or("GitHub did not say where the log is".into()),
        404 => Err("GitHub has no log for it yet (a job's log is kept once it has ended), or no longer".into()),
        s => Err(format!("GitHub would not give its log: {s}")),
    }
}

pub async fn job_log(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<Vec<u8>> {
    let ask = Request::new("GET", &format!("{api}/repos/{repo}/actions/jobs/{job_id}/logs"))
        .with_header("accept", "application/vnd.github+json").with_header("user-agent", "superci-plane").with_header("x-github-api-version", "2022-11-28")
        .with_header("authorization", &format!("Bearer {token}")).without_following();
    let mut r = http.send(ask).await?;
    if matches!(r.status, 301 | 302 | 303 | 307 | 308) {
        let at = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()).ok_or("GitHub did not say where the log is")?;
        r = http.send(Request::new("GET", &at).with_header("user-agent", "superci-plane")).await?;
    }
    match r.status {
        200 => Ok(r.body),
        404 => Err("GitHub has no log for it yet (a job's log is kept once it has ended), or no longer".into()),
        s => Err(format!("GitHub would not give its log: {s} {}", r.body_text().chars().take(200).collect::<String>())),
    }
}

/// A just-in-time runner for exactly one job: org-level for organizations (runner group 1 is the default group),
/// repository level for personal accounts. `work_folder`: where it checks out code (/home/runner/work, as on GitHub's
/// hosted runners).
pub async fn jit_config(http: &dyn Http, api: &str, token: &str, owner: &Owner, repo: &str, name: &str, labels: &[&str], work_folder: &str) -> Result<(String, u64)> {
    let path = if owner.org { format!("/orgs/{}/actions/runners/generate-jitconfig", owner.login) } else { format!("/repos/{repo}/actions/runners/generate-jitconfig") };
    let r = gh(http, api, "POST", &path, token, Some(serde_json::json!({ "name": name, "runner_group_id": 1, "labels": labels, "work_folder": work_folder }))).await?;
    Ok((r["encoded_jit_config"].as_str().ok_or("no runner config")?.to_string(), r["runner"]["id"].as_u64().unwrap_or_default()))
}

/// Webhook deliveries that failed recently (the runtime was restarting, or timed out): their ids, for redelivery.
pub async fn failed_deliveries(http: &dyn Http, app: &App, since_ms: u64, now_ms: u64) -> Result<Vec<(u64, String)>> {
    let r = as_app(http, app, "GET", "/app/hook/deliveries?per_page=100", now_ms, None).await?;
    Ok(r.as_array().into_iter().flatten().filter(|d| {
        let code = d["status_code"].as_u64().unwrap_or(0);
        let at = d["delivered_at"].as_str().and_then(crate::aws::parse_iso_ms).unwrap_or(0);
        d["event"] == "workflow_job" && (code == 0 || code >= 500) && d["redelivery"] != true && at >= since_ms
    }).filter_map(|d| Some((d["id"].as_u64()?, d["guid"].as_str()?.to_string()))).collect())
}

/// A repository's jobs that wait for a runner, as GitHub lists them now (not as it told of them): each job as its
/// event would have carried it, and whether the repository is private. `started_too`: also the waiting jobs of runs
/// of which another job has started.
pub async fn waiting_jobs(http: &dyn Http, api: &str, token: &str, repo: &str, started_too: bool) -> Result<Vec<(serde_json::Value, bool)>> {
    let mut out = waiting_in(http, api, token, repo, "queued").await?;
    if started_too { out.extend(waiting_in(http, api, token, repo, "in_progress").await?) }
    Ok(out)
}

/// The jobs that wait for a runner in a repository's runs of a status.
async fn waiting_in(http: &dyn Http, api: &str, token: &str, repo: &str, status: &str) -> Result<Vec<(serde_json::Value, bool)>> {
    let mut out = vec![];
    let runs = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs?status={status}&per_page=10"), token, None).await?;
    for run in runs["workflow_runs"].as_array().into_iter().flatten() {
        let Some(id) = run["id"].as_u64() else { continue };
        let jobs = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs/{id}/jobs?per_page=50"), token, None).await?;
        out.extend(jobs["jobs"].as_array().into_iter().flatten().filter(|j| j["status"] == "queued").map(|job| (job.clone(), run["repository"]["private"] == true)));
    }
    Ok(out)
}

/// What GitHub says of the events it sent this App lately (the newest hundred): for each, what it was, when, where
/// it answered from (the status), and whether it was a second sending. With `id`: one of them whole, with the address
/// it was sent to and what was answered.
pub async fn deliveries(http: &dyn Http, app: &App, id: Option<u64>, now_ms: u64) -> Result<serde_json::Value> {
    match id {
        Some(id) => {
            let d = as_app(http, app, "GET", &format!("/app/hook/deliveries/{id}"), now_ms, None).await?;
            let job = &d["request"]["payload"]["workflow_job"];
            Ok(serde_json::json!({ "id": d["id"], "event": d["event"], "action": d["action"], "delivered_at": d["delivered_at"], "status": d["status"], "status_code": d["status_code"], "redelivery": d["redelivery"], "duration": d["duration"],
                "url": d["url"], "job": job["id"], "job_name": job["name"], "labels": job["labels"], "answer": d["response"]["payload"].as_str().map(|p| p.chars().take(600).collect::<String>()) }))
        }
        None => {
            let list = as_app(http, app, "GET", "/app/hook/deliveries?per_page=100", now_ms, None).await?;
            Ok(serde_json::Value::Array(list.as_array().into_iter().flatten().map(|d| serde_json::json!({ "id": d["id"], "event": d["event"], "action": d["action"], "delivered_at": d["delivered_at"], "status": d["status"], "status_code": d["status_code"], "redelivery": d["redelivery"], "duration": d["duration"] })).collect()))
        }
    }
}

/// Points the App's webhook at a control plane (moving to another one keeps the App and its installations).
pub async fn set_webhook_url(http: &dyn Http, app: &App, url: &str, now_ms: u64) -> Result<()> {
    as_app(http, app, "PATCH", "/app/hook/config", now_ms, Some(serde_json::json!({ "url": url, "content_type": "json" }))).await.map(|_| ())
}

/// Removes the App from an account it is installed on (stopping SuperCI: its jobs no longer come).
pub async fn delete_installation(http: &dyn Http, app: &App, installation_id: u64, now_ms: u64) -> Result<()> {
    as_app(http, app, "DELETE", &format!("/app/installations/{installation_id}"), now_ms, None).await.map(|_| ())
}

pub async fn redeliver(http: &dyn Http, app: &App, delivery_id: u64, now_ms: u64) -> Result<()> {
    as_app(http, app, "POST", &format!("/app/hook/deliveries/{delivery_id}/attempts"), now_ms, None).await.map(|_| ())
}

/// A workflow run's status at GitHub ("queued", "in_progress", "completed"), its conclusion once completed
/// ("success", "failure", …), and a newer run of its workflow on its branch, for another commit (a newer push), that
/// is still going (queued or in progress), if one is.
pub async fn run_status(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<(String, Option<String>, Option<u64>)> {
    let v = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs/{run_id}"), token, None).await?;
    let status = v["status"].as_str().map(str::to_string).ok_or("no run status")?;
    let conclusion = v["conclusion"].as_str().map(str::to_string);
    let branch = v["head_branch"].as_str().unwrap_or_default();
    let newer = if branch.is_empty() { None } else {
        let query = format!("branch={}&event={}&per_page=50", url::form_urlencoded::byte_serialize(branch.as_bytes()).collect::<String>(), v["event"].as_str().unwrap_or_default());
        let runs = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs?{query}"), token, None).await?;
        runs["workflow_runs"].as_array().into_iter().flatten()
            .find(|r| r["workflow_id"] == v["workflow_id"] && r["head_branch"] == v["head_branch"] && r["head_sha"] != v["head_sha"] && r["id"].as_u64().is_some_and(|id| id > run_id) && r["status"] != "completed")
            .and_then(|r| r["id"].as_u64())
    };
    Ok((status, conclusion, newer))
}

/// Runs one job of a finished run again (and the jobs that depend on it), as a new attempt. Needs Actions: write;
/// GitHub refuses while the run is still going.
pub async fn rerun_job(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<()> {
    gh(http, api, "POST", &format!("/repos/{repo}/actions/jobs/{job_id}/rerun"), token, Some(serde_json::json!({}))).await.map(|_| ())
}

/// A workflow run cancelled, as its Cancel button does (its jobs' runners are told to stop; those not started end).
pub async fn cancel_run(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<()> {
    gh(http, api, "POST", &format!("/repos/{repo}/actions/runs/{run_id}/cancel"), token, Some(serde_json::json!({}))).await.map(|_| ())
}

/// Runs a finished run's failed jobs again (and the jobs that depend on them), as one new attempt: for a run with
/// several jobs to run again (GitHub takes one request per attempt).
pub async fn rerun_failed_jobs(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<()> {
    gh(http, api, "POST", &format!("/repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs"), token, Some(serde_json::json!({}))).await.map(|_| ())
}

/// A job's status at GitHub ("queued", "in_progress", "completed", …).
/// A repository as GitHub gives it (its id, its default branch, …).
pub async fn repository(http: &dyn Http, api: &str, token: &str, repo: &str) -> Result<serde_json::Value> {
    if repo.split('/').count() != 2 || !repo.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b)) { return Err("not a repository".into()) }
    gh(http, api, "GET", &format!("/repos/{repo}"), token, None).await
}

/// A repository by GitHub's id of it.
pub async fn repository_by_id(http: &dyn Http, api: &str, token: &str, id: &str) -> Result<serde_json::Value> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return Err("not a repository id".into()) }
    gh(http, api, "GET", &format!("/repositories/{id}"), token, None).await
}

/// A job's steps as GitHub has them now: each one's name, status ("queued", "in_progress", "completed"), conclusion,
/// and when it began and ended.
pub async fn job_steps(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<Vec<serde_json::Value>> {
    let v = gh(http, api, "GET", &format!("/repos/{repo}/actions/jobs/{job_id}"), token, None).await?;
    Ok(v["steps"].as_array().cloned().unwrap_or_default().into_iter().map(|s| serde_json::json!({ "name": s["name"], "status": s["status"], "conclusion": s["conclusion"], "number": s["number"],
        "started_at": s["started_at"], "completed_at": s["completed_at"] })).collect())
}

pub async fn job_status(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<String> {
    job_state(http, api, token, repo, job_id).await.map(|(status, _)| status)
}

/// A job's status ("queued", "in_progress", "completed") and the runner it began on, if it has begun.
pub async fn job_state(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<(String, Option<String>)> {
    let v = gh(http, api, "GET", &format!("/repos/{repo}/actions/jobs/{job_id}"), token, None).await?;
    let status = v["status"].as_str().ok_or("no job status")?.to_string();
    Ok((status, v["runner_name"].as_str().filter(|n| !n.is_empty()).map(str::to_string)))
}

/// Whether a runner is running a job now. One GitHub no longer knows (it removes a just-in-time runner once its job
/// has ended) is not.
pub async fn runner_busy(http: &dyn Http, api: &str, token: &str, owner: &Owner, repo: &str, runner_id: u64) -> Result<bool> {
    runner_state(http, api, token, owner, repo, runner_id).await.map(|s| s.is_some_and(|(_, busy)| busy))
}

/// A runner as GitHub has it: whether it is online (its machine up and asking for work), and whether it is running a
/// job. None: GitHub no longer knows it.
pub async fn runner_state(http: &dyn Http, api: &str, token: &str, owner: &Owner, repo: &str, runner_id: u64) -> Result<Option<(bool, bool)>> {
    let path = if owner.org { format!("/orgs/{}/actions/runners/{runner_id}", owner.login) } else { format!("/repos/{repo}/actions/runners/{runner_id}") };
    match gh(http, api, "GET", &path, token, None).await {
        Ok(v) => Ok(Some((v["status"] == "online", v["busy"].as_bool().unwrap_or(false)))),
        Err(e) if e.contains(": 404 ") => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn delete_runner(http: &dyn Http, api: &str, token: &str, owner: &Owner, repo: &str, runner_id: u64) -> Result<()> {
    let path = if owner.org { format!("/orgs/{}/actions/runners/{runner_id}", owner.login) } else { format!("/repos/{repo}/actions/runners/{runner_id}") };
    gh(http, api, "DELETE", &path, token, None).await.map(|_| ())
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobEvent {
    pub action: String,
    pub job_id: u64,
    pub run_id: u64,
    /// Which attempt of its run it is in (1, then 2 when the run or a job of it is run again).
    pub attempt: u32,
    /// When GitHub made the job (ms), as it says: how long it took to tell of it is seen by this.
    pub created_ms: Option<u64>,
    pub repo: String,
    pub installation_id: u64,
    pub runner_name: Option<String>,
    /// How it ended ("success", "failure", "cancelled"), once it has.
    pub conclusion: Option<String>,
    /// The job's name and its workflow's, as the workflow file names them.
    pub name: String,
    pub workflow: String,
    /// The job's own label (`superci` or `superci/cpu=8-…`, as written): its runner registers under it.
    pub label: String,
    /// The machine that label asks for, or why it is not understood.
    pub spec: std::result::Result<Spec, String>,
    /// A public repository (or GitHub did not say it is private): anyone can open a pull request that runs code.
    pub public: bool,
    /// GitHub's id of its repository, the branch it runs for, and the repository's default branch (for the cache).
    pub repo_id: Option<u64>,
    pub branch: Option<String>,
    pub default_branch: Option<String>,
}

/// A `workflow_job` event this control plane should act on: in a repository of the App's owner, labelled with the control
/// plane's label or a machine of it (`superci/cpu=8/arch=arm64`; see spec.rs), plus only GitHub's own self-hosted, system and
/// architecture labels, which a job may also list.
pub fn our_job(payload: &serde_json::Value, base: &str, owner_login: &str) -> Option<JobEvent> { our_job_among(payload, &[base.to_string()], owner_login) }

/// The same, for a control plane that answers to several labels (its own, and ones it keeps answering to).
pub fn our_job_among(payload: &serde_json::Value, bases: &[String], owner_login: &str) -> Option<JobEvent> {
    let job = payload.get("workflow_job")?;
    let repo = payload["repository"]["full_name"].as_str()?;
    let installation_id = payload["installation"]["id"].as_u64()?;
    if !repo.split('/').next().unwrap_or_default().eq_ignore_ascii_case(owner_login) { return None; }
    let labels: Vec<&str> = job["labels"].as_array()?.iter().filter_map(|l| l.as_str()).collect();
    let mut ours = labels.iter().filter_map(|l| Spec::parse_among(l, bases).map(|s| (l.to_string(), s)));
    let (label, spec) = ours.next()?;
    if ours.next().is_some() { return None; }
    let github_own = ["self-hosted", "linux", "macos", "x64", "arm64"];
    if !labels.iter().all(|l| l.eq_ignore_ascii_case(&label) || github_own.contains(&l.to_ascii_lowercase().as_str())) { return None; }
    Some(JobEvent {
        action: payload["action"].as_str()?.to_string(),
        job_id: job["id"].as_u64()?,
        run_id: job["run_id"].as_u64().unwrap_or_default(),
        attempt: job["run_attempt"].as_u64().unwrap_or_default() as u32,
        created_ms: job["created_at"].as_str().and_then(crate::aws::parse_iso_ms),
        repo: repo.to_string(),
        installation_id,
        runner_name: job["runner_name"].as_str().map(str::to_string),
        conclusion: job["conclusion"].as_str().map(str::to_string),
        name: job["name"].as_str().unwrap_or_default().to_string(),
        workflow: job["workflow_name"].as_str().unwrap_or_default().to_string(),
        label,
        spec,
        public: payload["repository"]["private"] != true,
        repo_id: payload["repository"]["id"].as_u64(),
        branch: job["head_branch"].as_str().map(str::to_string),
        default_branch: payload["repository"]["default_branch"].as_str().map(str::to_string),
    })
}

/// Whether a workflow run's code may come from outside the repository: a pull request from a fork, or a
/// `pull_request_target` run (started by a pull request, with the repository's secrets). Asked of GitHub (Actions: read).
pub async fn run_from_outside(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<Option<String>> {
    let r = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs/{run_id}"), token, None).await?;
    let (base, head) = (r["repository"]["id"].as_u64(), r["head_repository"]["id"].as_u64());
    if base.is_none() { return Err("GitHub did not say where the run came from".into()) }
    if head != base { return Ok(Some("a pull request from a fork".into())) }
    if r["event"] == "pull_request_target" { return Ok(Some("a pull_request_target run".into())) }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_for_org_and_user() {
        let org = app_manifest("https://h.example.workers.dev", &Owner { org: true, login: "acme-corp".into() }, "SuperCI", "superci");
        assert_eq!(org["default_permissions"], serde_json::json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" }));
        assert_eq!(org["hook_attributes"]["url"], "https://h.example.workers.dev/webhook");
        assert_eq!(org["public"], false);
        // Named after the CI and whose jobs it is for, with nothing else: what owners read among their Apps.
        assert_eq!(org["name"], "SuperCI acme-corp");
        let user = app_manifest("https://h", &Owner { org: false, login: "a-very-long-user-name-that-goes-on-and-on".into() }, "Acme CI", "acme");
        assert_eq!(user["default_permissions"]["administration"], "write");
        // A long login is cut with a mark of the whole of it; the name is 34 characters at most, whatever the CI is called.
        assert_eq!(user["name"], format!("Acme CI a-very-lon-{}", &crate::crypto::sha256_hex(b"a-very-long-user-name-that-goes-on-and-on")[..4]));
        assert_eq!(app_name("A CI With A Very Long Name", "a-very-long-user-name-that-goes-on-and-on").chars().count(), 34);
        assert_eq!((app_name("SuperCI", "acme").as_str(), app_name("Acme CI", "acme").as_str()), ("SuperCI acme", "Acme CI acme"));
        assert_eq!(manifest_target(None, &Owner { org: true, login: "o".into() }, "s1"), "https://github.com/organizations/o/settings/apps/new?state=s1");
        assert_eq!(manifest_target(None, &Owner { org: false, login: "u".into() }, "s1"), "https://github.com/settings/apps/new?state=s1");
    }

    #[test]
    fn github_elsewhere_has_its_own_addresses() {
        // A GitHub Enterprise Server: its API under /api/v3, its Apps' pages under /github-apps.
        let ghes = Some("github.example.com");
        assert_eq!((api_base(ghes), web_base(ghes)), ("https://github.example.com/api/v3".to_string(), "https://github.example.com".to_string()));
        assert_eq!(install_link(ghes, "superci-acme"), "https://github.example.com/github-apps/superci-acme/installations/new");
        assert_eq!(manifest_target(ghes, &Owner { org: true, login: "o".into() }, "s1"), "https://github.example.com/organizations/o/settings/apps/new?state=s1");
        // GitHub Enterprise Cloud with data residency: api.SUBDOMAIN.ghe.com, pages as on github.com.
        let ghe = Some("acme.ghe.com");
        assert_eq!((api_base(ghe), install_link(ghe, "a")), ("https://api.acme.ghe.com".to_string(), "https://acme.ghe.com/apps/a/installations/new".to_string()));
        assert_eq!((api_base(None), install_link(None, "a")), ("https://api.github.com".to_string(), "https://github.com/apps/a/installations/new".to_string()));
        // As typed.
        assert_eq!(host_of(" https://GitHub.Example.com/ ").unwrap(), Some("github.example.com".to_string()));
        assert_eq!((host_of("").unwrap(), host_of("github.com").unwrap(), host_of("https://github.com/").unwrap()), (None, None, None));
        for bad in ["localhost", "github.example.com/path", "a b.com", "exa$mple.com", "-x.com", "x..com", "user@host.com"] { assert!(host_of(bad).is_err(), "{bad}") }
    }

    #[test]
    fn which_jobs_are_ours() {
        let event = |labels: serde_json::Value, repo: &str| serde_json::json!({ "action": "queued", "workflow_job": { "id": 7, "run_id": 3, "labels": labels, "runner_name": null }, "repository": { "full_name": repo }, "installation": { "id": 42 } });
        let ours = our_job(&event(serde_json::json!(["superci"]), "Acme/app"), "superci", "acme").unwrap();
        assert_eq!((ours.job_id, ours.installation_id, ours.repo.as_str(), ours.runner_name), (7, 42, "Acme/app", None));
        assert!(our_job(&event(serde_json::json!(["self-hosted", "SuperCI"]), "acme/app"), "superci", "acme").is_some());
        assert!(our_job(&event(serde_json::json!(["ubuntu-latest"]), "acme/app"), "superci", "acme").is_none());
        assert!(our_job(&event(serde_json::json!(["superci", "gpu"]), "acme/app"), "superci", "acme").is_none());
        let sized = our_job(&event(serde_json::json!(["self-hosted", "SuperCI/cpu=8/arch=arm64"]), "acme/app"), "superci", "acme").unwrap();
        assert_eq!(sized.label, "SuperCI/cpu=8/arch=arm64");
        assert_eq!(sized.spec.as_ref().unwrap().cpu, Some(8));
        assert!(our_job(&event(serde_json::json!(["superci/cores=8"]), "acme/app"), "superci", "acme").unwrap().spec.is_err());
        assert!(our_job(&event(serde_json::json!(["superci-8"]), "acme/app"), "superci", "acme").is_none(), "another label that begins alike is not this one");
        assert!(our_job(&event(serde_json::json!(["superci", "superci/cpu=8"]), "acme/app"), "superci", "acme").is_none());
        assert!(our_job(&event(serde_json::json!(["superci"]), "other/app"), "superci", "acme").is_none());
        assert!(valid_login("acme-corp") && !valid_login("-x") && !valid_login("a b") && !valid_login(""));
    }

    /// GitHub as a test has it answer each request: an answer, or a connection that failed.
    struct Answers(fn(&Request) -> Result<crate::io::Response>);
    #[async_trait::async_trait(?Send)]
    impl Http for Answers {
        async fn send(&self, r: Request) -> Result<crate::io::Response> { (self.0)(&r) }
    }
    fn answer(status: u16, body: serde_json::Value) -> Result<crate::io::Response> { Ok(crate::io::Response::new(status, "application/json", body.to_string())) }
    fn run<T>(f: impl std::future::Future<Output = T>) -> T { futures::executor::block_on(f) }
    const API: &str = "https://api.github.com";

    #[test]
    fn what_github_answers_amiss_is_said_as_that() {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let app = |pem: String| App { id: 1, slug: "a".into(), pem, webhook_secret: "s".into(), owner: "acme".into(), owner_is_org: true, host: None };
        // An App whose key is none signs nothing; an answer that is no JSON; a token that is not in it.
        assert!(run(installations(&Answers(|_| answer(200, serde_json::json!([]))), &app("not a key".into()), 1_790_000_000_000)).is_err());
        assert!(run(deliveries(&Answers(|_| answer(200, serde_json::json!([]))), &app("not a key".into()), Some(6), 1_790_000_000_000)).is_err());
        assert!(run(deliveries(&Answers(|_| answer(200, serde_json::json!([]))), &app("not a key".into()), None, 1_790_000_000_000)).is_err());
        assert_eq!(run(run_status(&Answers(|_| answer(200, serde_json::json!({}))), API, "t", "acme/app", 3)).unwrap_err(), "no run status");
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap().to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap().to_string();
        let real = app(key);
        assert!(run(installations(&Answers(|_| Ok(crate::io::Response::new(200, "application/json", "not json"))), &real, 1_790_000_000_000)).unwrap_err().starts_with("GitHub /app/installations: "));
        assert_eq!(run(installation_token(&Answers(|_| answer(201, serde_json::json!({}))), &real, 42, 1_790_000_000_000)).unwrap_err(), "installation token missing");
        // Deliveries without their ids are passed over.
        let failed = run(failed_deliveries(&Answers(|_| answer(200, serde_json::json!([{ "event": "workflow_job", "status_code": 502, "delivered_at": "2026-09-21T00:00:00Z" }, { "id": 5, "event": "workflow_job", "status_code": 502, "delivered_at": "2026-09-21T00:00:00Z" },
            { "id": 6, "guid": "g-6", "event": "workflow_job", "status_code": 502, "delivered_at": "2026-09-21T00:00:00Z" }]))), &real, 0, 1_790_000_000_000)).unwrap();
        assert_eq!(failed, [(6, "g-6".to_string())]);
        // What GitHub says of its deliveries, one and all; the App's webhook moved, an installation removed, a
        // delivery sent again.
        let deliveries_said = Answers(|r| answer(200, if r.url.ends_with("/deliveries/6") { serde_json::json!({ "id": 6, "request": { "payload": { "workflow_job": { "id": 7 } } } }) } else { serde_json::json!([{ "id": 6 }]) }));
        assert_eq!(run(deliveries(&deliveries_said, &real, Some(6), 1_790_000_000_000)).unwrap()["job"], 7);
        assert_eq!(run(deliveries(&deliveries_said, &real, None, 1_790_000_000_000)).unwrap()[0]["id"], 6);
        let fine = Answers(|_| answer(200, serde_json::json!({})));
        run(set_webhook_url(&fine, &real, "https://p/webhook", 1_790_000_000_000)).unwrap();
        run(delete_installation(&fine, &real, 42, 1_790_000_000_000)).unwrap();
        run(redeliver(&fine, &real, 6, 1_790_000_000_000)).unwrap();
        // A job's log: not reached, none yet, refused, or moved without saying where.
        let gone = |r: fn(&Request) -> Result<crate::io::Response>| run(job_log(&Answers(r), API, "t", "acme/app", 7));
        assert!(gone(|_| Err("connection reset".into())).is_err());
        assert!(gone(|_| answer(404, serde_json::json!({}))).unwrap_err().contains("no log for it yet"));
        assert!(gone(|_| Ok(crate::io::Response::new(302, "text/plain", ""))).unwrap_err().contains("did not say where"));
        let link = |r: fn(&Request) -> Result<crate::io::Response>| run(job_log_link(&Answers(r), API, "t", "acme/app", 7));
        assert!(link(|_| answer(404, serde_json::json!({}))).unwrap_err().contains("no log for it yet"));
        assert_eq!(link(|_| answer(500, serde_json::json!({}))).unwrap_err(), "GitHub would not give its log: 500");
        assert!(link(|_| Ok(crate::io::Response::new(302, "text/plain", ""))).unwrap_err().contains("did not say where"));
        // A runner made with no configuration; a job with no status.
        assert_eq!(run(jit_config(&Answers(|_| answer(201, serde_json::json!({}))), API, "t", &Owner { org: true, login: "acme".into() }, "acme/app", "r", &["superci"], "/w")).unwrap_err(), "no runner config");
        assert_eq!(run(job_state(&Answers(|_| answer(200, serde_json::json!({}))), API, "t", "acme/app", 7)).unwrap_err(), "no job status");
    }

    #[test]
    fn a_repositorys_waiting_jobs_and_public_repositories_are_read_page_after_page() {
        // A run listed without its id; a run with no job waiting.
        let waiting = run(waiting_jobs(&Answers(|r| if r.url.contains("/runs?") { answer(200, serde_json::json!({ "workflow_runs": [{ "name": "no id" }, { "id": 9 }] })) } else { answer(200, serde_json::json!({ "jobs": [{ "id": 1, "status": "in_progress" }] })) }), API, "t", "acme/app", true)).unwrap();
        assert!(waiting.is_empty());
        assert!(run(waiting_jobs(&Answers(|_| answer(200, serde_json::json!({ "workflow_runs": [] }))), API, "t", "acme/app", false)).unwrap().is_empty());
        // A thousand repositories at most, a hundred a page; fewer, the last page.
        let full = run(public_repositories(&Answers(|_| answer(200, serde_json::json!({ "repositories": vec![serde_json::json!({ "full_name": "acme/x", "private": false }); 100] }))), API, "t")).unwrap();
        assert_eq!(full.len(), 1000);
        assert_eq!(run(public_repositories(&Answers(|_| answer(200, serde_json::json!({ "repositories": [{ "full_name": "acme/x", "private": false }, { "full_name": "acme/y", "private": true }] }))), API, "t")).unwrap(), ["acme/x"]);
    }

    #[test]
    fn an_event_missing_what_a_job_has_is_none_of_ours() {
        let full = serde_json::json!({ "action": "queued", "workflow_job": { "id": 7, "labels": ["superci"] }, "repository": { "full_name": "acme/app" }, "installation": { "id": 42 } });
        assert!(our_job(&full, "superci", "acme").is_some());
        for (at, field) in [(None, "workflow_job"), (Some("repository"), "full_name"), (None, "installation"), (Some("workflow_job"), "labels"), (None, "action"), (Some("workflow_job"), "id")] {
            let mut e = full.clone();
            match at { Some(a) => { e[a].as_object_mut().unwrap().remove(field); } None => { e.as_object_mut().unwrap().remove(field); } }
            assert!(our_job(&e, "superci", "acme").is_none(), "{at:?} {field}");
        }
    }
}
