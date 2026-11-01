//! The control plane's Worker as Cloudflare runs it: the module `./build.sh` makes, under `wrangler dev` (workerd, the
//! runtime Cloudflare runs it in, on this computer), its Durable Object's storage and alarms as Cloudflare keeps them.
//! What it fetches from outside is answered by the test kit's stand-ins, and each runner's container by the test (see
//! the test kit's `worker`: nothing of the Worker's own is changed). Where Node is and the Worker is built; skipped
//! elsewhere.
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use superci_testkit::worker::{self, Setup, Worker};
use superci_testkit::GitHub;

const PLANE: &str = "cfplane00001";
/// A dashboard's key, as a deploy leaves one (good until 2100).
const KEY: &str = "dashboard-key-for-tests-0001";

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

/// The settings a deploy leaves: the dashboard's key, Cloudflare's containers on, the GitHub App made and installed.
fn deployed() -> Vec<(String, String)> {
    vec![("DEVICE_KEY_4102444800_TEST".into(), KEY.into()), ("CONTAINERS".into(), "on".into()), ("GITHUB_APP".into(), GitHub::app().to_string())]
}

/// The control plane's Worker, started with these settings (as its secrets), as a deploy binds it.
fn start(settings: &[(String, String)]) -> Option<Worker> { start_with(settings, true) }

/// The same, with its runners' binding or without (a Worker deployed before Cloudflare runners); and the class kept from
/// before 0.5.0 bound, to be asked.
fn start_with(settings: &[(String, String)], runners: bool) -> Option<Worker> { start_vars(settings, runners, &format!("LABEL = \"superci\"\nPLANE_ID = \"{PLANE}\"")) }

/// The same, with these plain settings (`[vars]` lines).
fn start_vars(settings: &[(String, String)], runners: bool, vars: &str) -> Option<Worker> {
    let bindings = format!("[[durable_objects.bindings]]\nname = \"PLANE\"\nclass_name = \"PlaneObject\"\n\n{}[[durable_objects.bindings]]\nname = \"LEGACY\"\nclass_name = \"Runner\"\n\n[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"PlaneObject\", \"JobRunner\", \"Runner\"]\n",
        if runners { "[[durable_objects.bindings]]\nname = \"RUNNER\"\nclass_name = \"JobRunner\"\n\n" } else { "" });
    Worker::start(&Setup { built: worker::built(env!("CARGO_MANIFEST_DIR")), bindings: &bindings, vars }, settings)
}

/// As the dashboard and GitHub ask the control plane.
trait Asked {
    fn dashboard(&self, method: &str, path: &str, body: &str) -> (u16, String);
    fn webhook(&self, sent: (Vec<u8>, String)) -> u16;
}

impl Asked for Worker {
    /// With the dashboard's key.
    fn dashboard(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        self.call(method, path, &[("authorization", &format!("Bearer {KEY}")), ("content-type", "application/json")], body.as_bytes())
    }

    /// A webhook GitHub sends, as it sends it: the Worker's answer.
    fn webhook(&self, (body, signature): (Vec<u8>, String)) -> u16 {
        self.call("POST", "/webhook", &[("content-type", "application/json"), ("x-github-event", "workflow_job"), ("x-hub-signature-256", &signature)], &body).0
    }
}

/// Its health and status, as Cloudflare runs it: the settings it was deployed with.
#[test]
fn the_worker_answers_with_the_settings_it_was_deployed_with() {
    let mut settings = deployed();
    settings.push(("NAME".into(), "AcmeCI".into()));
    let Some(w) = start(&settings) else { return };
    let (status, health) = w.get("/health");
    let health: Value = serde_json::from_str(&health).unwrap_or_default();
    assert!(status == 200 && health["plane"] == PLANE && health["github"] == true && health["runners"] == true, "{status} {health}");
    let (status, said) = w.dashboard("GET", "/status", "");
    assert_eq!(status, 200, "{said}");
}

/// A GitHub job, as Cloudflare runs it: queued, its runner's container started (sized, its runner configured just in
/// time, kept running for the job's time), the job taken by it and finished, the container ended.
#[test]
fn a_github_job_runs_in_a_container_of_its_own() {
    let Some(w) = start(&deployed()) else { return };
    let (id, queued) = w.github.queue("build", "superci", now_ms());
    assert_eq!(w.webhook(queued) / 100, 2, "{}", w.log());
    w.until("a container started", || !w.containers.ops("start").is_empty() && !w.containers.ops("keep").is_empty());
    let start = &w.containers.ops("start")[0];
    let options = &start["options"];
    assert!(options["env"]["SUPERCI_JIT"].as_str().is_some_and(|j| !j.is_empty()) && options["image"].as_str().is_some_and(|i| i.contains("superci-runner")) && options["enableInternet"] == true, "{start}");
    assert!(options["instance"]["vcpu"].as_f64().is_some_and(|c| c > 0.0), "{start}");
    w.webhook(w.github.start(id));
    w.webhook(w.github.finish(id, "success", "done\n", now_ms()));
    w.until("its container ended", || !w.containers.ops("destroy").is_empty());
}

/// What the dashboard keeps in the Worker's secrets is read as the other control planes read their settings: its
/// device keys listed by name, the secrets kept for runs from a computer listed (without their values).
#[test]
fn the_keys_and_secrets_kept_in_the_workers_secrets_are_listed() {
    let mut settings = deployed();
    let secret = json!({ "name": "NPM_TOKEN", "value": "npm_0123456789", "repo": "acme/app", "updated_ms": 1_760_000_000_000u64 });
    settings.push(("SECRET_NPM_TOKEN_REPO_ACME_APP".into(), secret.to_string()));
    let Some(w) = start(&settings) else { return };
    let (status, keys) = w.dashboard("GET", "/keys", "");
    let keys: Value = serde_json::from_str(&keys).unwrap_or_default();
    assert!(status == 200 && keys["devices"].as_array().is_some_and(|d| d.iter().any(|k| k["name"] == "DEVICE_KEY_4102444800_TEST")), "{status} {keys}");
    let (status, secrets) = w.dashboard("GET", "/secrets", "");
    let secrets: Value = serde_json::from_str(&secrets).unwrap_or_default();
    assert!(status == 200 && secrets["secrets"].as_array().is_some_and(|s| s.iter().any(|s| s["name"] == "NPM_TOKEN" && s.get("value").is_none())), "{status} {secrets}");
}

/// What only the Worker may say is not taken from outside: settings handed in a request, a key it would vouch for. Its
/// own keys: a dashboard's (not one that ended, nor a name that is not one), and one that only reads.
#[test]
fn only_the_worker_says_what_its_settings_and_keys_are() {
    let mut settings = deployed();
    settings.push(("DEVICE_KEY_1000000000_OLD".into(), "an-ended-key-000000001".into()));
    settings.push(("DEVICE_KEY_soon_X".into(), "a-key-of-no-time-00001".into()));
    let run_key = "superci_run_0123456789abcdef0123456789abcdef";
    settings.push(("READ_KEY_4102444800_AGENT".into(), format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes()))));
    let Some(w) = start(&settings) else { return };
    // Settings handed in from outside: not taken.
    let (_, health) = w.call("GET", "/health", &[("x-superci-settings-0", r#"{"CONTAINERS":"off"}"#)], b"");
    assert!(health.contains("\"runners\":true"), "{health}");
    // A key the Worker would vouch for, said from outside; a key too short to be one; one that ended; one whose name
    // says no time: refused. Its own: taken; one that only reads: taken where reading is all.
    for (bearer, until) in [("not-a-key-but-long-enough", Some("4102444800000")), ("short", None), ("an-ended-key-000000001", None), ("a-key-of-no-time-00001", None)] {
        let mut headers = vec![("authorization".to_string(), format!("Bearer {bearer}"))];
        if let Some(u) = until { headers.push(("x-superci-key-until".into(), u.into())) }
        let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_ne!(w.call("GET", "/keys", &headers, b"").0, 200, "{bearer}");
    }
    assert_eq!(w.dashboard("GET", "/keys", "").0, 200);
    assert_eq!(w.call("GET", "/caches", &[("authorization", &format!("Bearer {run_key}"))], b"").0, 200);
    // (A HEAD, handed on without a body: answered by the control plane, which has no HEAD of its health.)
    assert_eq!(w.call("HEAD", "/health", &[], b"").0, 404);
}

/// Each further organization's GitHub App and each further GitLab connection, as their secrets: handed to the Durable
/// Object in pieces (a header holds only so much); one that is the first App again, or not one, or a connection with a
/// name that is not one, left out. Read the same way by its upkeep.
#[test]
fn further_apps_and_gitlab_connections_are_read_from_their_secrets() {
    let mut settings = deployed();
    let first = GitHub::app();
    for id in 2..=6u64 {
        let mut app = first.clone();
        app["id"] = json!(first["id"].as_u64().unwrap() + id);
        app["slug"] = json!(format!("superci-org{id}"));
        settings.push((format!("GITHUB_APP_{}", app["id"]), app.to_string()));
    }
    settings.push((format!("GITHUB_APP_{}", first["id"]), first.to_string()));
    settings.push(("GITHUB_APP_999".into(), "not an app".into()));
    settings.push(("GITLAB".into(), superci_testkit::GitLab::connection().to_string()));
    let mut second = superci_testkit::GitLab::connection();
    second["id"] = json!("second");
    settings.push(("GITLAB_SECOND".into(), second.to_string()));
    second["id"] = json!("Not A Name");
    settings.push(("GITLAB_BAD".into(), second.to_string()));
    let Some(w) = start(&settings) else { return };
    let (status, said) = w.dashboard("GET", "/status", "");
    let status_of: Value = serde_json::from_str(&said).unwrap_or_default();
    assert!(status == 200 && status_of["apps"].as_array().is_some_and(|a| a.len() == 6), "{status} {said}");
    assert!(status_of["gitlabs"].as_array().is_some_and(|g| g.len() == 2), "{said}");
    // Its upkeep reads its settings as it started with them.
    assert_eq!(w.alarm(None), (200, "ok".to_string()), "{}", w.log());
}

/// A runner's container, each way Cloudflare's may go, as the control plane's Durable Object asks it.
#[test]
fn a_runners_container_goes_each_way_cloudflares_may() {
    let Some(w) = start(&deployed()) else { return };
    worker::each_way_a_container_goes(&w);
}

/// A job's log, as the dashboard reads it from GitHub (a link GitHub answers with, followed without the token), once
/// the job ended in its container.
#[test]
fn a_finished_jobs_log_is_read_from_github() {
    let Some(w) = start(&deployed()) else { return };
    let (id, queued) = w.github.queue("build", "superci", now_ms());
    w.webhook(queued);
    w.until("a container started", || !w.containers.ops("start").is_empty());
    w.webhook(w.github.start(id));
    w.webhook(w.github.finish(id, "success", "built it\n", now_ms()));
    let (status, log) = w.dashboard("GET", &format!("/job/log?id={id}"), "");
    assert!(status == 200 && log.contains("built it"), "{status} {log}");
}

/// A Worker deployed before Cloudflare runners (no runners' binding) with them turned on, and one that starts its
/// runners where it is told: a job that cannot start is said; one placed as told starts.
#[test]
fn runners_start_where_told_and_a_worker_without_them_says_so() {
    let Some(w) = start_with(&deployed(), false) else { return };
    let (_, queued) = w.github.queue("build", "superci", now_ms());
    w.webhook(queued);
    w.until("the job said as not started", || { let s = w.dashboard("GET", "/status", "").1; s.contains("starting its machine failed") && s.contains("RUNNER") });
    assert!(w.containers.ops("start").is_empty());
    drop(w);
    let mut settings = deployed();
    settings.push(("CF_LOCATION".into(), "enam".into()));
    let Some(w) = start(&settings) else { return };
    let (_, queued) = w.github.queue("build", "superci", now_ms());
    w.webhook(queued);
    w.until("a container started", || !w.containers.ops("start").is_empty());
}

/// A GitHub job's whole way through the Worker (queued, its container started, taken, finished, its container
/// ended, the upkeep after) with each call it makes failing in turn: GitHub's, and Cloudflare's container service's.
/// Nothing panics, and the Worker still answers for its state.
#[test]
fn the_workers_job_holds_with_each_call_and_store_operation_failing() {
    worker::sweep("the Worker's job", || start(&deployed()), |w| {
        let (id, queued) = w.github.queue("build", "superci", now_ms());
        w.webhook(queued);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while w.containers.ops("start").is_empty() && std::time::Instant::now() < until { std::thread::sleep(std::time::Duration::from_millis(100)) }
        // (Taken and finished where a runner was registered for it: a failure before leaves the job waiting.)
        if !w.github.runner_names().is_empty() {
            w.webhook(w.github.start(id));
            w.webhook(w.github.finish(id, "success", "done\n", now_ms()));
        }
        let _ = w.alarm(None);
        let (status, said) = w.dashboard("GET", "/status", "");
        if status == 200 { vec![] } else { vec![format!("its status: {status} {said}")] }
    });
}

/// A GitLab job, as Cloudflare runs it: its event taken, its runner's container started with GitLab's runner (its
/// token and address in its environment), the job run and finished (its runner ends by itself after its one job).
#[test]
fn a_gitlab_job_runs_in_a_container_of_its_own() {
    let mut settings = deployed();
    settings.push(("GITLAB".into(), superci_testkit::GitLab::connection().to_string()));
    let Some(w) = start(&settings) else { return };
    let gitlab = w.github.gitlab.clone();
    let hook = |body: Vec<u8>| w.call("POST", "/gitlab/webhook", &[("content-type", "application/json"), ("x-gitlab-event", "Job Hook"), ("x-gitlab-token", superci_testkit::gitlab::HOOK_SECRET)], &body).0;
    let (id, pending) = gitlab.pending("gl-build", &["superci"], now_ms());
    assert_eq!(hook(pending) / 100, 2, "{}", w.log());
    w.until("its container started", || !w.containers.ops("start").is_empty());
    let start = w.containers.ops("start").pop().unwrap();
    assert!(start["options"]["env"]["GL_TOKEN"].as_str().is_some_and(|t| !t.is_empty()) && start["options"]["env"]["GL_URL"].as_str().is_some(), "{start}");
    hook(gitlab.running(id, now_ms()));
    hook(gitlab.finish(id, "success", "built it\n", now_ms()));
    let done = || serde_json::from_str::<Value>(&w.dashboard("GET", "/status", "").1).unwrap_or_default()["jobs"].as_array().into_iter().flatten().any(|j| j["name"] == "gl-build" && j["state"] == "done");
    w.until("the job done", done);
}

/// A run from a computer, as Cloudflare runs it: started with a run key, its job's runner started in a container of
/// its own; the runner, taking its job, is handed the secret the run's workflow names (kept in the Worker's secrets).
#[test]
fn a_run_from_a_computer_is_handed_its_secrets() {
    use base64::Engine;
    let run_key = "superci_run_0123456789abcdef0123456789abcdef";
    let mut settings = deployed();
    settings.push(("READ_KEY_4102444800_AGENT".into(), format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes()))));
    settings.push(("SECRET_NPM_TOKEN_ALL".into(), json!({ "name": "NPM_TOKEN", "value": "npm-token-kept-for-runs", "updated_ms": 1_760_000_000_000u64 }).to_string()));
    let Some(w) = start(&settings) else { return };
    let workflow = "jobs:\n  publish:\n    runs-on: superci\n    steps:\n      - run: npm publish\n        env:\n          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}\n";
    let (status, said) = w.call("POST", "/runs", &[("authorization", &format!("Bearer {run_key}")), ("content-type", "application/json")],
        json!({ "workflow": workflow, "path": ".github/workflows/publish.yml", "repository": "acme/app", "ref": "refs/heads/main" }).to_string().as_bytes());
    assert_eq!(status, 200, "{said}\n{}", w.log());
    w.until("its job's container started", || !w.containers.ops("start").is_empty());
    // (The runner's link, as its configuration gives it: `.runner`'s ServerUrl.)
    let b64 = |s: &str| -> Value { serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(s).unwrap()).unwrap() };
    let jit = b64(w.containers.ops("start").pop().unwrap()["options"]["env"]["SUPERCI_JIT"].as_str().unwrap());
    let root = b64(jit[".runner"].as_str().unwrap())["ServerUrl"].as_str().unwrap().to_string();
    let runner = superci_testkit::Runner { at: format!("{root}/_apis/v1") };
    let (status, message) = runner.message();
    assert!(status == 200 && message.to_string().contains("npm-token-kept-for-runs"), "{status} {message}");
    // Asking again, with its job taken: nothing more, after the control plane held the answer a while (as GitHub's
    // broker holds a runner's long poll).
    let asked = std::time::Instant::now();
    assert_eq!(runner.message().0, 204);
    assert!(asked.elapsed() >= std::time::Duration::from_secs(15), "{:?}", asked.elapsed());
}

/// Its settings as they are set: GitHub's full image for its containers, Cloudflare choosing where they start, the instance types it may use and no label of its own (its
/// plain settings), an AWS account's connecting under way; a setting that is not a secret's text (a deploy's object)
/// left out, as a key or an App.
#[test]
fn its_settings_are_read_as_they_are_set() {
    let mut settings = deployed();
    settings.extend([("CF_IMAGE".into(), "https://images.example.com/full/0123456789abcdef".into()), ("CF_LOCATION".into(), "auto".into()),
        ("AWS_CONNECT".into(), json!({ "region": "eu-west-1", "token": "connect-token-0123456789" }).to_string())]);
    let vars = format!("PLANE_ID = \"{PLANE}\"\nINSTANCE_TYPES = \"m7i.large, ,c7i.large\"\nGITHUB_APP_77 = {{ id = 77 }}\nDEVICE_KEY_4102444800_OBJECT = {{ key = \"an-object-not-a-key-01\" }}");
    let Some(w) = start_vars(&settings, true, &vars) else { return };
    let (status, said) = w.dashboard("GET", "/status", "");
    assert!(status == 200 && said.contains(r#""cloudflare_location":"auto""#), "{status} {said}");
    let (id, queued) = w.github.queue("build", "superci", now_ms());
    w.webhook(queued);
    w.until("a container started", || !w.containers.ops("start").is_empty());
    let start = w.containers.ops("start").pop().unwrap();
    assert!(start["options"]["env"]["SUPERCI_IMAGE"].as_str().is_some_and(|i| i.contains("images.example.com")), "{start}");
    w.webhook(w.github.start(id));
    // The AWS account's connecting: a report with another token is not this control plane's.
    let (status, said) = w.call("POST", "/aws/callback", &[("content-type", "application/json")], json!({ "token": "another-token-0123456789" }).to_string().as_bytes());
    assert!(status == 403 && said.contains("not this control plane's stack"), "{status} {said}");
}

/// A Worker deployed without its control plane's Durable Object bound: every request fails, none taken for done.
#[test]
fn a_worker_without_its_control_plane_fails_every_request() {
    let bindings = "[[durable_objects.bindings]]\nname = \"RUNNER\"\nclass_name = \"JobRunner\"\n\n[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"PlaneObject\", \"JobRunner\", \"Runner\"]\n";
    let Some(w) = Worker::start(&Setup { built: worker::built(env!("CARGO_MANIFEST_DIR")), bindings, vars: &format!("PLANE_ID = \"{PLANE}\"") }, &deployed()) else { return };
    let (status, said) = w.get("/health");
    assert_eq!(status, 500, "{said}");
}

/// A move away: the secrets it keeps handed on with its one-time move token (one with letters outside ASCII, handed
/// from the Worker to its Durable Object escaped, as they are), and where it moves to only an https address. Its
/// containers set to nothing: off.
#[test]
fn a_move_away_hands_on_the_secrets_it_keeps() {
    let mut settings = deployed();
    settings.retain(|(k, _)| k != "CONTAINERS");
    settings.extend([("CONTAINERS".into(), "".into()), ("MOVE_TOKEN".into(), "move-token-0123456789".into()),
        ("SECRET_NPM_TOKEN_ALL".into(), json!({ "name": "NPM_TOKEN", "value": "npm-token-ąžuolas-𝄞", "updated_ms": 1_760_000_000_000u64 }).to_string())]);
    let Some(w) = start(&settings) else { return };
    assert!(w.get("/health").1.contains("\"runners\":false"));
    let (status, said) = w.call("POST", "/move/export", &[("authorization", &format!("Bearer {KEY}")), ("x-move-token", "move-token-0123456789")], b"{}");
    let secrets: Value = serde_json::from_str(&said).unwrap_or_default();
    assert!(status == 200 && secrets["secrets"].to_string().contains("npm-token-ąžuolas-𝄞"), "{status} {said}");
    // Moved away only to an https address: as Cloudflare runs it (a release build), not even one on this computer.
    for to in ["http://127.0.0.1:8977", "ftp://plane.example.com"] {
        let (status, said) = w.dashboard("POST", "/move/away", &json!({ "to": to }).to_string());
        assert!(status == 400 && said.contains("where to?"), "{to}: {status} {said}");
    }
    let (status, said) = w.dashboard("POST", "/move/away", &json!({ "to": "https://superci.globex.workers.dev" }).to_string());
    assert!(status == 200 && said.contains("globex"), "{status} {said}");
}

/// A job whose runner cannot start, in each way it cannot: a Worker without containers, a start Cloudflare refuses at
/// once. Said on the job.
#[test]
fn a_runner_that_cannot_start_is_said_on_its_job() {
    let Some(w) = start(&deployed()) else { return };
    let said = |what: &'static str| { let w = &w; move || w.dashboard("GET", "/status", "").1.contains(what) };
    w.containers_are(json!({ "noContainer": true }));
    w.webhook(w.github.queue("build", "superci", now_ms()).1);
    w.until("no container, said", said("no container for this runner"));
    w.containers_are(json!({ "throwStart": "no such image" }));
    w.webhook(w.github.queue("test", "superci", now_ms()).1);
    w.until("the start refused, said", said("no such image"));
}

/// Its Durable Object's storage failing, each operation in turn, while a job comes and its upkeep runs: nothing
/// panics, what failed is said, and once the storage is back it answers for its state.
#[test]
fn the_control_plane_holds_with_its_storage_failing() {
    let Some(w) = start(&deployed()) else { return };
    let mut done = 0;
    for op in ["get", "put", "delete", "list", "setAlarm"] {
        w.containers_are(json!({ "failStorage": [op] }));
        let (id, queued) = w.github.queue(&format!("build-{op}"), "superci", now_ms());
        w.webhook(queued);
        // (Taken and finished where a runner was registered for it.)
        if w.github.runner_names().len() > done {
            done = w.github.runner_names().len();
            w.webhook(w.github.start(id));
            w.webhook(w.github.finish(id, "success", "done\n", now_ms()));
        }
        let (status, said) = w.dashboard("GET", "/status", "");
        let alarm = w.alarm(None);
        let failed = format!("storage {op} failed");
        assert!((status == 200 || said.contains(&failed)) && (alarm.0 == 200 || alarm.1.contains(&failed)), "{op}: {status} {said}; {alarm:?}");
        assert!(op == "delete" || said.contains(&failed) || alarm.1.contains(&failed), "{op}: its failing never said");
    }
    w.containers_are(json!({}));
    assert_eq!(w.dashboard("GET", "/status", "").0, 200);
    let log = w.log();
    assert!(!log.contains("panicked at") && !log.contains("RuntimeError"), "{log}");
}

/// GitLab answering in each way it can fail to (not there, an address that is not one, a token that cannot go in a
/// header, an answer cut off): the job said as not started, the Worker still answering.
#[test]
fn gitlab_failing_to_answer_is_said() {
    use std::io::{Read, Write};
    let cut = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cut_at = format!("http://{}", cut.local_addr().unwrap());
    std::thread::spawn(move || for s in cut.incoming() {
        let Ok(mut s) = s else { continue };
        let _ = s.read(&mut [0; 8192]);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"id\":");
    });
    let closed = "http://127.0.0.1:1".to_string();
    for (url, token) in [(closed.as_str(), None), ("http://[::1", None), (superci_testkit::gitlab::URL, Some("glpat-a\nb")), (cut_at.as_str(), None)] {
        let mut conn = superci_testkit::GitLab::connection();
        conn["url"] = json!(url);
        if let Some(t) = token { conn["token"] = json!(t) }
        let mut settings = deployed();
        settings.push(("GITLAB".into(), conn.to_string()));
        let Some(w) = start(&settings) else { return };
        let (_, pending) = w.github.gitlab.pending("gl-build", &["superci"], now_ms());
        let (status, said) = w.call("POST", "/gitlab/webhook", &[("content-type", "application/json"), ("x-gitlab-event", "Job Hook"), ("x-gitlab-token", superci_testkit::gitlab::HOOK_SECRET)], &pending);
        assert!(status == 500 && !said.trim().is_empty(), "{url} {token:?}: {status} {said}");
        assert_eq!(w.dashboard("GET", "/status", "").0, 200);
    }
}
