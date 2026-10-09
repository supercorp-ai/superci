//! GitLab CI (gitlab.com or your own GitLab), next to GitHub. A project's webhook sends job events here; a job whose tags
//! name this control plane's label (`tags: [superci-8cpu]`) gets a machine, as a GitHub job does. GitLab has no
//! runner registered for one job: a project runner per set of tags is created once (its token kept in the control
//! plane's state) and each machine runs `gitlab-runner run-single --max-builds 1` with it, takes the next job with those
//! tags, and ends. Jobs run in Docker (their `image:`), as on GitLab's own runners; a Mac runs macOS jobs on itself.
use serde::{Deserialize, Serialize};

use crate::io::{Http, Request};
use crate::io::Result;

/// The connection, set from the dashboard: where GitLab is, a token (scopes api, create_runner, manage_runner) and the
/// secret its webhooks carry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GitLab {
    pub url: String, pub token: String, pub hook_secret: String,
    /// Which of a control plane's GitLab connections this is: nothing for the first, a short name for each further
    /// one (another GitLab, or another account's token on the same one).
    #[serde(default, skip_serializing_if = "String::is_empty")] pub id: String,
}

/// A further connection's name: a few lowercase letters and digits, a letter first.
pub fn valid_id(id: &str) -> bool {
    (3..=12).contains(&id.len()) && id.starts_with(|c: char| c.is_ascii_lowercase()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A job or runner of a connection, as records and names tell them apart (GitLab's ids are unique only within one
/// GitLab): "1977" in the first connection, "ab12cd-1977" in a further one.
pub fn local(connection: &str, id: u64) -> String {
    if connection.is_empty() { id.to_string() } else { format!("{connection}-{id}") }
}

/// A job event from a project's webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct JobEvent { pub job_id: u64, pub status: String, pub project_id: u64, pub project: String, pub pipeline_id: u64, pub name: String, pub stage: String, pub runner_id: Option<u64> }

/// A job event, if the body is one (`object_kind: build`).
pub fn job_event(body: &[u8]) -> Option<JobEvent> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if v["object_kind"] != "build" { return None }
    Some(JobEvent {
        job_id: v["build_id"].as_u64()?, status: v["build_status"].as_str()?.to_string(), project_id: v["project_id"].as_u64().or(v["project"]["id"].as_u64())?,
        project: v["project"]["path_with_namespace"].as_str().or(v["project_name"].as_str()).unwrap_or_default().to_string(),
        pipeline_id: v["pipeline_id"].as_u64().or(v["commit"]["id"].as_u64()).unwrap_or(0), name: v["build_name"].as_str().unwrap_or_default().to_string(),
        stage: v["build_stage"].as_str().unwrap_or_default().to_string(), runner_id: v["runner"]["id"].as_u64(),
    })
}

async fn api(http: &dyn Http, gl: &GitLab, method: &str, path: &str, form: Option<&[(&str, &str)]>) -> Result<serde_json::Value> {
    let mut r = Request::new(method, &format!("{}/api/v4{path}", gl.url.trim_end_matches('/'))).with_header("private-token", &gl.token).with_header("user-agent", "superci");
    if let Some(form) = form {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form.iter().copied()).finish();
        r = r.with_header("content-type", "application/x-www-form-urlencoded").with_body(body);
    }
    let resp = http.send(r).await?;
    if resp.status >= 300 { return Err(format!("GitLab {method} {path}: {} {}", resp.status, resp.body_text().chars().take(200).collect::<String>())) }
    serde_json::from_slice(&resp.body).map_err(|e| format!("GitLab {path}: {e}"))
}

/// A job's log so far (GitLab's trace).
pub async fn job_trace(http: &dyn Http, gl: &GitLab, project_id: u64, job_id: u64) -> Result<Vec<u8>> {
    let r = Request::new("GET", &format!("{}/api/v4/projects/{project_id}/jobs/{job_id}/trace", gl.url.trim_end_matches('/'))).with_header("private-token", &gl.token).with_header("user-agent", "superci");
    let resp = http.send(r).await?;
    if resp.status >= 300 { return Err(format!("GitLab would not give its log: {} {}", resp.status, resp.body_text().chars().take(200).collect::<String>())) }
    Ok(resp.body)
}

/// A job's tags and status (the webhook does not carry its tags).
pub async fn job(http: &dyn Http, gl: &GitLab, project_id: u64, job_id: u64) -> Result<(Vec<String>, String)> {
    let v = api(http, gl, "GET", &format!("/projects/{project_id}/jobs/{job_id}"), None).await?;
    Ok((v["tag_list"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect(), v["status"].as_str().unwrap_or_default().to_string()))
}

/// A project runner for one job's tags (locked to the project, no untagged jobs); its id and token. GitLab has no
/// runner for exactly one job, so each job gets its own, paused once it takes a job and removed after.
pub async fn create_runner(http: &dyn Http, gl: &GitLab, project_id: u64, tags: &[String], called: &str, plane_id: &str, job_id: u64) -> Result<(u64, String)> {
    let tags = tags.join(",");
    let description = format!("{called} {plane_id}: job {job_id} ({tags})");
    let project = project_id.to_string();
    let v = api(http, gl, "POST", "/user/runners", Some(&[("runner_type", "project_type"), ("project_id", &project), ("tag_list", &tags), ("run_untagged", "false"),
        ("locked", "true"), ("description", &description)])).await?;
    Ok((v["id"].as_u64().ok_or("GitLab made no runner")?, v["token"].as_str().ok_or("GitLab gave no runner token")?.to_string()))
}

/// Removes a runner SuperCI made (stopping SuperCI).
/// The scopes of the connection's token (GitLab: personal, project and group access tokens alike).
pub async fn token_scopes(http: &dyn Http, gl: &GitLab) -> Result<Vec<String>> {
    let v = api(http, gl, "GET", "/personal_access_tokens/self", None).await?;
    Ok(v["scopes"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect())
}

/// Cancels a pending job (one nothing here can run: GitLab would keep it pending).
pub async fn cancel_job(http: &dyn Http, gl: &GitLab, project_id: u64, job_id: u64) -> Result<()> {
    api(http, gl, "POST", &format!("/projects/{project_id}/jobs/{job_id}/cancel"), None).await.map(|_| ())
}

/// Runs a finished job again (GitLab makes a new job with the same name in its pipeline).
pub async fn retry_job(http: &dyn Http, gl: &GitLab, project_id: u64, job_id: u64) -> Result<()> {
    api(http, gl, "POST", &format!("/projects/{project_id}/jobs/{job_id}/retry"), None).await.map(|_| ())
}

/// Pauses a runner: it takes no more jobs (the one it runs goes on), so its token, if a job read it, gets no other.
pub async fn pause_runner(http: &dyn Http, gl: &GitLab, runner_id: u64) -> Result<()> {
    api(http, gl, "PUT", &format!("/runners/{runner_id}"), Some(&[("paused", "true")])).await.map(|_| ())
}

pub async fn delete_runner(http: &dyn Http, gl: &GitLab, runner_id: u64) -> Result<()> {
    let r = http.send(Request::new("DELETE", &format!("{}/api/v4/runners/{runner_id}", gl.url.trim_end_matches('/'))).with_header("private-token", &gl.token)).await?;
    if r.status >= 300 && r.status != 404 { return Err(format!("GitLab removing a runner: {}", r.status)) }
    Ok(())
}

/// A project as the dashboard shows it: whether it sends its jobs here (a webhook to this control plane), and whether
/// GitLab has paused that webhook after failed deliveries (its `alert_status`, and until when).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Project { pub id: u64, pub path: String, pub enabled: bool, pub hook_status: Option<String>, pub hook_paused_until: Option<String> }

/// The projects this token can add webhooks to (Maintainer or more, most recently active first).
pub async fn projects(http: &dyn Http, gl: &GitLab, hook_url: &str) -> Result<Vec<Project>> {
    let list = api(http, gl, "GET", "/projects?membership=true&min_access_level=40&simple=true&order_by=last_activity_at&per_page=40", None).await?;
    let mut out = vec![];
    for p in list.as_array().into_iter().flatten() {
        let Some(id) = p["id"].as_u64() else { continue };
        let hooks = api(http, gl, "GET", &format!("/projects/{id}/hooks"), None).await.unwrap_or_default();
        let ours = hooks.as_array().into_iter().flatten().find(|h| h["url"] == hook_url).cloned();
        out.push(Project { id, path: p["path_with_namespace"].as_str().unwrap_or_default().to_string(), enabled: ours.is_some(),
            hook_status: ours.as_ref().and_then(|h| h["alert_status"].as_str().map(str::to_string)), hook_paused_until: ours.as_ref().and_then(|h| h["disabled_until"].as_str().map(str::to_string)) });
    }
    Ok(out)
}

/// A project's webhooks that point here, by id.
async fn our_hooks(http: &dyn Http, gl: &GitLab, project_id: u64, hook_url: &str) -> Result<Vec<u64>> {
    let hooks = api(http, gl, "GET", &format!("/projects/{project_id}/hooks"), None).await?;
    Ok(hooks.as_array().into_iter().flatten().filter(|h| h["url"] == hook_url).filter_map(|h| h["id"].as_u64()).collect())
}

/// Sends a project's job events here (a webhook with the secret), or stops; a webhook that is there gets the current
/// secret.
pub async fn set_project(http: &dyn Http, gl: &GitLab, project_id: u64, hook_url: &str, enabled: bool) -> Result<()> {
    let existing = our_hooks(http, gl, project_id, hook_url).await?;
    let fields = [("url", hook_url), ("job_events", "true"), ("push_events", "false"), ("enable_ssl_verification", "true"), ("token", gl.hook_secret.as_str())];
    match (enabled, existing.as_slice()) {
        (true, []) => { api(http, gl, "POST", &format!("/projects/{project_id}/hooks"), Some(&fields)).await?; }
        (true, ids) => for id in ids { api(http, gl, "PUT", &format!("/projects/{project_id}/hooks/{id}"), Some(&fields)).await?; },
        (false, ids) => for id in ids {
            let r = http.send(Request::new("DELETE", &format!("{}/api/v4/projects/{project_id}/hooks/{id}", gl.url.trim_end_matches('/'))).with_header("private-token", &gl.token)).await?;
            if r.status >= 300 && r.status != 404 { return Err(format!("GitLab removing a webhook: {}", r.status)) }
        },
    }
    Ok(())
}

/// What a Linux machine runs for a GitLab job: GitLab's runner (downloaded at start), Docker (started if it is not
/// running), one job, then it ends. GL_URL and GL_TOKEN come in the environment; DOCKERD_FLAGS and GL_RUNNER_FLAGS say
/// how this machine runs Docker (Cloudflare's containers: no bridge networks, so the jobs' containers use the host's).
pub const RUNNER_SCRIPT: &str = r#"set -e
case "$(uname -m)" in aarch64|arm64) a=arm64 ;; *) a=amd64 ;; esac
s=""; [ "$(id -u)" = 0 ] || s=sudo
curl -fsSL --retry 3 -o /tmp/gitlab-runner "https://gitlab-runner-downloads.s3.amazonaws.com/latest/binaries/gitlab-runner-linux-$a"
chmod +x /tmp/gitlab-runner
if ! docker info >/dev/null 2>&1; then
  $s sh -c "dockerd $DOCKERD_FLAGS > /tmp/dockerd.log 2>&1 &"
  for i in $(seq 1 150); do docker info >/dev/null 2>&1 && break; sleep 0.2; done
fi
exec /tmp/gitlab-runner run-single -u "$GL_URL" -t "$GL_TOKEN" --executor docker --docker-image ubuntu:24.04 --max-builds 1 --wait-timeout 300 --builds-dir /tmp/builds --cache-dir /tmp/cache $GL_RUNNER_FLAGS
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_events_from_webhooks() {
        let body = serde_json::json!({ "object_kind": "build", "build_id": 1977, "build_name": "test", "build_status": "pending", "pipeline_id": 2366,
            "project_id": 380, "project_name": "gitlab-org/gitlab-test", "project": { "id": 380, "path_with_namespace": "gitlab-org/gitlab-test" } }).to_string();
        assert_eq!(job_event(body.as_bytes()), Some(JobEvent { job_id: 1977, status: "pending".into(), project_id: 380, project: "gitlab-org/gitlab-test".into(), pipeline_id: 2366, name: "test".into(), stage: String::new(), runner_id: None }));
        assert_eq!(job_event(br#"{"object_kind":"push"}"#), None);
    }
}
