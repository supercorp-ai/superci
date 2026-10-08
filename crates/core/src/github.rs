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
pub fn app_manifest(plane_url: &str, owner: &Owner, label: &str, plane_id: &str) -> serde_json::Value {
    // A long login is cut, with a mark of the whole of it (two organizations may begin alike).
    let login = if owner.login.len() > 15 { format!("{}-{}", owner.login.chars().take(10).collect::<String>(), &crate::crypto::sha256_hex(owner.login.to_ascii_lowercase().as_bytes())[..4]) } else { owner.login.clone() };
    let mut name: String = format!("SuperCI {login}").chars().take(27).collect();
    name.push(' ');
    name.extend(plane_id.chars().take(6)); // names are unique across GitHub, at most 34 characters
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

/// Where the manifest form is posted: the organization's or the user's own App settings.
pub fn manifest_target(host: Option<&str>, owner: &Owner, state: &str) -> String {
    let web = web_base(host);
    let base = if owner.org { format!("{web}/organizations/{}/settings/apps/new", owner.login) } else { format!("{web}/settings/apps/new") };
    format!("{base}?state={state}")
}

async fn gh(http: &dyn Http, api: &str, method: &str, path: &str, token: Option<&str>, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
    let mut req = Request::new(method, &format!("{api}{path}"))
        .with_header("accept", "application/vnd.github+json")
        .with_header("user-agent", "superci-plane")
        .with_header("x-github-api-version", "2022-11-28");
    if let Some(t) = token { req = req.with_header("authorization", &format!("Bearer {t}")); }
    if let Some(b) = body { req = req.with_header("content-type", "application/json").with_body(b.to_string()); }
    let r = http.send(req).await?;
    if r.status >= 300 { return Err(format!("GitHub {method} {path}: {} {}", r.status, r.body_text().chars().take(300).collect::<String>())); }
    if r.status == 204 || r.body.is_empty() { return Ok(serde_json::Value::Null); }
    serde_json::from_slice(&r.body).map_err(|e| format!("GitHub {path}: {e}"))
}

/// The manifest flow's last step: the code GitHub redirected back with becomes the App's id, key and webhook secret.
pub async fn convert_manifest_code(http: &dyn Http, host: Option<&str>, code: &str) -> Result<App> {
    if code.len() < 8 || code.len() > 100 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') { return Err("invalid manifest code".into()); }
    let r = gh(http, &api_base(host), "POST", &format!("/app-manifests/{code}/conversions"), None, None).await?;
    let s = |k: &str| r[k].as_str().map(str::to_string).ok_or_else(|| format!("App conversion without {k}"));
    Ok(App { id: r["id"].as_u64().ok_or("App conversion without id")?, slug: s("slug")?, pem: s("pem")?, webhook_secret: s("webhook_secret")?,
        owner: r["owner"]["login"].as_str().unwrap_or_default().to_string(), owner_is_org: r["owner"]["type"] == "Organization", host: host.map(str::to_string) })
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
    let r = gh(http, &app.api(), "GET", "/app/installations", Some(&app_jwt(app, now_ms)?), None).await?;
    Ok(r.as_array().map(|a| a.iter().map(|i| Installation {
        id: i["id"].as_u64().unwrap_or_default(),
        account: i["account"]["login"].as_str().unwrap_or_default().to_string(),
        selection: i["repository_selection"].as_str().unwrap_or_default().to_string(),
        permissions: i["permissions"].clone(),
    }).collect()).unwrap_or_default())
}

/// The public repositories an installation can see (its jobs come only from repositories it is installed on).
pub async fn public_repositories(http: &dyn Http, api: &str, token: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    for page in 1..=10 {
        let r = gh(http, api, "GET", &format!("/installation/repositories?per_page=100&page={page}"), Some(token), None).await?;
        let repos = r["repositories"].as_array().cloned().unwrap_or_default();
        out.extend(repos.iter().filter(|x| x["private"] == false).filter_map(|x| x["full_name"].as_str().map(str::to_string)));
        if repos.len() < 100 { break }
    }
    Ok(out)
}

/// The permissions the App asks for (as set in its settings; each installation accepts them).
pub async fn app_permissions(http: &dyn Http, app: &App, now_ms: u64) -> Result<serde_json::Value> {
    Ok(gh(http, &app.api(), "GET", "/app", Some(&app_jwt(app, now_ms)?), None).await?["permissions"].clone())
}

pub async fn installation_token(http: &dyn Http, app: &App, installation_id: u64, now_ms: u64) -> Result<String> {
    let r = gh(http, &app.api(), "POST", &format!("/app/installations/{installation_id}/access_tokens"), Some(&app_jwt(app, now_ms)?), Some(serde_json::json!({}))).await?;
    r["token"].as_str().map(str::to_string).ok_or_else(|| "installation token missing".into())
}

/// A job's log, whole, as GitHub keeps it once the job has ended. GitHub answers with where it is (a link good for a
/// minute, to be fetched without the token); a client that follows by itself arrives with the log.
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
    let r = gh(http, api, "POST", &path, Some(token), Some(serde_json::json!({ "name": name, "runner_group_id": 1, "labels": labels, "work_folder": work_folder }))).await?;
    Ok((r["encoded_jit_config"].as_str().ok_or("no runner config")?.to_string(), r["runner"]["id"].as_u64().unwrap_or_default()))
}

/// Webhook deliveries that failed recently (the runtime was restarting, or timed out): their ids, for redelivery.
pub async fn failed_deliveries(http: &dyn Http, app: &App, since_ms: u64, now_ms: u64) -> Result<Vec<(u64, String)>> {
    let r = gh(http, &app.api(), "GET", "/app/hook/deliveries?per_page=100", Some(&app_jwt(app, now_ms)?), None).await?;
    Ok(r.as_array().into_iter().flatten().filter(|d| {
        let code = d["status_code"].as_u64().unwrap_or(0);
        let at = d["delivered_at"].as_str().and_then(crate::aws::parse_iso_ms).unwrap_or(0);
        d["event"] == "workflow_job" && (code == 0 || code >= 500) && d["redelivery"] != true && at >= since_ms
    }).filter_map(|d| Some((d["id"].as_u64()?, d["guid"].as_str()?.to_string()))).collect())
}

/// Points the App's webhook at a control plane (moving to another one keeps the App and its installations).
pub async fn set_webhook_url(http: &dyn Http, app: &App, url: &str, now_ms: u64) -> Result<()> {
    gh(http, &app.api(), "PATCH", "/app/hook/config", Some(&app_jwt(app, now_ms)?), Some(serde_json::json!({ "url": url, "content_type": "json" }))).await.map(|_| ())
}

/// Removes the App from an account it is installed on (stopping SuperCI: its jobs no longer come).
pub async fn delete_installation(http: &dyn Http, app: &App, installation_id: u64, now_ms: u64) -> Result<()> {
    gh(http, &app.api(), "DELETE", &format!("/app/installations/{installation_id}"), Some(&app_jwt(app, now_ms)?), None).await.map(|_| ())
}

pub async fn redeliver(http: &dyn Http, app: &App, delivery_id: u64, now_ms: u64) -> Result<()> {
    gh(http, &app.api(), "POST", &format!("/app/hook/deliveries/{delivery_id}/attempts"), Some(&app_jwt(app, now_ms)?), None).await.map(|_| ())
}

/// A workflow run's status at GitHub ("queued", "in_progress", "completed").
pub async fn run_status(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<String> {
    let v = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs/{run_id}"), Some(token), None).await?;
    v["status"].as_str().map(str::to_string).ok_or_else(|| "no run status".into())
}

/// Runs one job of a finished run again (and the jobs that depend on it), as a new attempt. Needs Actions: write;
/// GitHub refuses while the run is still going.
pub async fn rerun_job(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<()> {
    gh(http, api, "POST", &format!("/repos/{repo}/actions/jobs/{job_id}/rerun"), Some(token), Some(serde_json::json!({}))).await.map(|_| ())
}

/// Runs a finished run's failed jobs again (and the jobs that depend on them), as one new attempt: for a run with
/// several jobs to run again (GitHub takes one request per attempt).
pub async fn rerun_failed_jobs(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<()> {
    gh(http, api, "POST", &format!("/repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs"), Some(token), Some(serde_json::json!({}))).await.map(|_| ())
}

/// A job's status at GitHub ("queued", "in_progress", "completed", …).
pub async fn job_status(http: &dyn Http, api: &str, token: &str, repo: &str, job_id: u64) -> Result<String> {
    let v = gh(http, api, "GET", &format!("/repos/{repo}/actions/jobs/{job_id}"), Some(token), None).await?;
    v["status"].as_str().map(str::to_string).ok_or_else(|| "no job status".into())
}

pub async fn delete_runner(http: &dyn Http, api: &str, token: &str, owner: &Owner, repo: &str, runner_id: u64) -> Result<()> {
    let path = if owner.org { format!("/orgs/{}/actions/runners/{runner_id}", owner.login) } else { format!("/repos/{repo}/actions/runners/{runner_id}") };
    gh(http, api, "DELETE", &path, Some(token), None).await.map(|_| ())
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobEvent {
    pub action: String,
    pub job_id: u64,
    pub run_id: u64,
    pub repo: String,
    pub installation_id: u64,
    pub runner_name: Option<String>,
    /// How it ended ("success", "failure", "cancelled"), once it has.
    pub conclusion: Option<String>,
    /// The job's name and its workflow's, as the workflow file names them.
    pub name: String,
    pub workflow: String,
    /// The job's own label (`superci` or `superci-8cpu-…`, as written): its runner registers under it.
    pub label: String,
    /// The machine that label asks for, or why it is not understood.
    pub spec: std::result::Result<Spec, String>,
    /// A public repository (or GitHub did not say it is private): anyone can open a pull request that runs code.
    pub public: bool,
}

/// A `workflow_job` event this control plane should act on: in a repository of the App's owner, labelled with the control
/// plane's label or a machine of it (`superci-8cpu-arm64`; see spec.rs), plus only GitHub's own self-hosted, system and
/// architecture labels, which a job may also list.
pub fn our_job(payload: &serde_json::Value, base: &str, owner_login: &str) -> Option<JobEvent> {
    let job = payload.get("workflow_job")?;
    let repo = payload["repository"]["full_name"].as_str()?;
    let installation_id = payload["installation"]["id"].as_u64()?;
    if !repo.split('/').next()?.eq_ignore_ascii_case(owner_login) { return None; }
    let labels: Vec<&str> = job["labels"].as_array()?.iter().filter_map(|l| l.as_str()).collect();
    let mut ours = labels.iter().filter_map(|l| Spec::parse(l, base).map(|s| (l.to_string(), s)));
    let (label, spec) = ours.next()?;
    if ours.next().is_some() { return None; }
    let github_own = ["self-hosted", "linux", "macos", "x64", "arm64"];
    if !labels.iter().all(|l| l.eq_ignore_ascii_case(&label) || github_own.contains(&l.to_ascii_lowercase().as_str())) { return None; }
    Some(JobEvent {
        action: payload["action"].as_str()?.to_string(),
        job_id: job["id"].as_u64()?,
        run_id: job["run_id"].as_u64().unwrap_or_default(),
        repo: repo.to_string(),
        installation_id,
        runner_name: job["runner_name"].as_str().map(str::to_string),
        conclusion: job["conclusion"].as_str().map(str::to_string),
        name: job["name"].as_str().unwrap_or_default().to_string(),
        workflow: job["workflow_name"].as_str().unwrap_or_default().to_string(),
        label,
        spec,
        public: payload["repository"]["private"] != true,
    })
}

/// Whether a workflow run's code may come from outside the repository: a pull request from a fork, or a
/// `pull_request_target` run (started by a pull request, with the repository's secrets). Asked of GitHub (Actions: read).
pub async fn run_from_outside(http: &dyn Http, api: &str, token: &str, repo: &str, run_id: u64) -> Result<Option<String>> {
    let r = gh(http, api, "GET", &format!("/repos/{repo}/actions/runs/{run_id}"), Some(token), None).await?;
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
        let org = app_manifest("https://h.example.workers.dev", &Owner { org: true, login: "acme-corp".into() }, "superci", "abc123xyz");
        assert_eq!(org["default_permissions"], serde_json::json!({ "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" }));
        assert_eq!(org["hook_attributes"]["url"], "https://h.example.workers.dev/webhook");
        assert_eq!(org["public"], false);
        assert!(org["name"].as_str().unwrap().len() <= 34 && org["name"].as_str().unwrap().ends_with(" abc123"));
        let user = app_manifest("https://h", &Owner { org: false, login: "a-very-long-user-name-that-goes-on-and-on".into() }, "x", "abc123");
        assert_eq!(user["default_permissions"]["administration"], "write");
        assert!(user["name"].as_str().unwrap().chars().count() <= 34);
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
        let sized = our_job(&event(serde_json::json!(["self-hosted", "SuperCI-8cpu-arm64"]), "acme/app"), "superci", "acme").unwrap();
        assert_eq!(sized.label, "SuperCI-8cpu-arm64");
        assert_eq!(sized.spec.as_ref().unwrap().cpu, Some(8));
        assert!(our_job(&event(serde_json::json!(["superci-8cores"]), "acme/app"), "superci", "acme").unwrap().spec.is_err());
        assert!(our_job(&event(serde_json::json!(["superci", "superci-8cpu"]), "acme/app"), "superci", "acme").is_none());
        assert!(our_job(&event(serde_json::json!(["superci"]), "other/app"), "superci", "acme").is_none());
        assert!(valid_login("acme-corp") && !valid_login("-x") && !valid_login("a b") && !valid_login(""));
    }
}
