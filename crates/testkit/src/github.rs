//! GitHub as the control plane finds it, for an organization `acme` with its repository `acme/app` and SuperCI's App
//! installed on it. Repositories, runs and jobs are answered in the shape GitHub answered for a public repository
//! (`fixtures/github/{repo,run,job}.json`, recorded with `gh api` and made Acme's); the App's own calls (its
//! installations, their tokens, a runner's registration, webhook deliveries), which only an App can make, as GitHub's
//! documentation gives them. The test plays GitHub's side of a job (`queue`, `start`, `finish`): its state here, and
//! the webhook GitHub sends of each change, signed with the App's secret.
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use superci_core::io::{self, Http, Request, Response};

pub const OWNER: &str = "acme";
pub const REPO: &str = "acme/app";
pub const APP_ID: u64 = 1001;
pub const INSTALLATION: u64 = 42;
pub const WEBHOOK_SECRET: &str = "webhook-secret-for-tests";
/// Where GitHub's logs are (a link it gives for a minute, as to its blob store).
const LOGS_AT: &str = "https://productionresultssa0.blob.core.windows.net/actions-results/logs";

#[derive(Default)]
struct Hub {
    /// S3 refuses what is put (as it refuses a link that ended).
    refuse_puts: bool,
    /// Calls answered with an error (`fail`): the method and a part of the address, and the answer.
    failing: Vec<(String, String, u16, String)>,
    /// Answers given as they are, with their headers, to a method on a URL with this part (see `answer_with`).
    scripted: Vec<(String, String, u16, Vec<(String, String)>, String)>,
    jobs: BTreeMap<u64, Value>,
    logs: BTreeMap<u64, String>,
    /// Each run's logs, once it has ended (GitHub's zip: a folder for each job, a file for each step).
    run_logs: BTreeMap<u64, Vec<u8>>,
    /// The runners registered (each machine's, by the control plane): id to name.
    runners: BTreeMap<u64, String>,
    /// The runners whose machines came up and that wait for work (`ready`): online. A runner is offline until then, as
    /// GitHub has a just-in-time runner until it connects.
    online: std::collections::BTreeSet<u64>,
    next: u64,
    /// `acme/app` is public (anyone can open a pull request that runs code there).
    public: bool,
    /// The App is installed on chosen repositories only (not all of the organization's), or nowhere yet.
    selected: bool,
    nowhere: bool,
    /// An App made before it asked for Actions: its permissions and its installations' without `actions`.
    older_app: bool,
    /// Its installation has not accepted the App's newest permissions (the App asks for them).
    older_install: bool,
    /// How many times GitHub's key set for its jobs' tokens was asked for.
    jwks_asked: u64,
    /// The App was uninstalled.
    uninstalled: bool,
    /// Each event delivered to the App's webhook, as GitHub keeps them (docs.github.com/rest/apps/webhooks), newest last;
    /// and the deliveries asked to be sent again.
    deliveries: Vec<Value>,
    attempts: Vec<u64>,
    /// How many of those a test has sent again (`to_send_again`).
    resent: usize,
    /// S3's objects, by bucket and key (`bucket/key`), as the control plane's buckets keep them.
    objects: BTreeMap<String, Vec<u8>>,
    /// Each object's headers S3 keeps with it (`x-amz-meta-*`, its checksum), given back on a HEAD.
    meta: BTreeMap<String, Vec<(String, String)>>,
    /// The buckets made (`PUT` on a bucket's root).
    buckets: std::collections::BTreeSet<String>,
    /// Answers given as they are to a method on exactly this path (see `answer_at`).
    exact: Vec<(String, String, u16, String)>,
}

#[derive(Clone, Default)]
pub struct GitHub { hub: Arc<Mutex<Hub>>, pub asked: Arc<Mutex<Vec<String>>>, pub unanswered: Arc<Mutex<Vec<String>>>, /** gitlab.com, beside it. */ pub gitlab: crate::GitLab,
    /** AWS's stand-in, for what a control plane asks of AWS (but S3, kept here). */ pub aws: Arc<Mutex<Option<crate::Aws>>>,
    /** What happens as a call is made (`during`): another request's doing meanwhile. */ pub during: Arc<Mutex<Vec<(String, String, Arc<dyn Fn() + Send + Sync>)>>> }

fn fixture(name: &str) -> Value {
    serde_json::from_str(match name {
        "repo" => include_str!("../fixtures/github/repo.json"),
        "run" => include_str!("../fixtures/github/run.json"),
        "org" => include_str!("../fixtures/github/org.json"),
        _ => include_str!("../fixtures/github/job.json"),
    }).unwrap()
}

pub(crate) fn iso(ms: u64) -> String {
    let (secs, days) = (ms / 1000, ms / 86_400_000);
    // (Days since 1970 to a date: Howard Hinnant's civil_from_days.)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600 % 24, secs / 60 % 60, secs % 60)
}

impl GitHub {
    /// Makes `acme/app` public, or private again.
    pub fn set_public(&self, public: bool) { self.hub.lock().unwrap().public = public }

    /// The App installed on chosen repositories only, or (`nowhere`) not installed anywhere yet.
    pub fn set_installed(&self, selected: bool, nowhere: bool) { let mut h = self.hub.lock().unwrap(); (h.selected, h.nowhere) = (selected, nowhere) }

    /// The App as one made before it asked for Actions (write): it asks for less, and its installations accepted less.
    pub fn set_older_app(&self, older: bool) { self.hub.lock().unwrap().older_app = older }

    /// Its installation has not accepted the App's newest permissions: the App asks for them, the installation lacks them.
    pub fn set_older_install(&self, older: bool) { self.hub.lock().unwrap().older_install = older }

    /// An object put in a bucket (`bucket/key`), as the control plane's deploy leaves its program there.
    pub fn put_object(&self, key: &str, bytes: &[u8]) { self.hub.lock().unwrap().objects.insert(key.to_string(), bytes.to_vec()); }

    /// An object put at a link as a machine puts it: at the link's bucket and key, with the headers S3 keeps.
    pub fn put_at(&self, link: &str, bytes: &[u8], headers: &[(&str, &str)]) {
        let url = url::Url::parse(link).unwrap();
        let bucket = url.host_str().unwrap().split_once(".s3.").unwrap().0;
        let key = format!("{bucket}/{}", url.path().trim_start_matches('/'));
        let mut h = self.hub.lock().unwrap();
        h.objects.insert(key.clone(), bytes.to_vec());
        h.meta.insert(key, headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect());
    }

    /// From now on S3 refuses what is put in a bucket (`false`: takes it again).
    pub fn refuse_puts(&self, refuse: bool) { self.hub.lock().unwrap().refuse_puts = refuse }

    /// A call (its method, and a part of its address) answered with this error from now on; `fail_none` clears them.
    pub fn fail(&self, method: &str, part: &str, status: u16, message: &str) { self.hub.lock().unwrap().failing.push((method.into(), part.into(), status, json!({ "message": message, "documentation_url": "https://docs.github.com/rest", "status": status.to_string() }).to_string())) }

    pub fn fail_none(&self) { let mut h = self.hub.lock().unwrap(); h.failing.clear(); h.scripted.clear() }

    /// What any address asked with `method` on a URL with `part` in it answers (a registry, say): this status, these
    /// headers, this body. Taken back with `fail_none`.
    pub fn answer_with(&self, method: &str, part: &str, status: u16, headers: &[(&str, &str)], body: &str) {
        self.hub.lock().unwrap().scripted.push((method.into(), part.into(), status, headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body.into()))
    }

    /// How many times GitHub's key set for its jobs' tokens was asked for.
    pub fn jwks_asked(&self) -> u64 { self.hub.lock().unwrap().jwks_asked }

    /// Whether the App was uninstalled.
    pub fn uninstalled(&self) -> bool { self.hub.lock().unwrap().uninstalled }

    /// An event delivered (`body`) to `url`, and what was answered (status 0: GitHub could not reach it), as GitHub
    /// keeps it among the App's deliveries.
    pub fn delivered(&self, body: &[u8], status: u16, answer: &str, url: &str, now_ms: u64) {
        let payload: Value = serde_json::from_slice(body).unwrap_or_default();
        let mut h = self.hub.lock().unwrap();
        let id = 40_000_000_000 + h.deliveries.len() as u64 + 1;
        h.deliveries.push(json!({ "id": id, "guid": format!("0b989ba4-242f-11e5-81e1-{id:012x}"), "delivered_at": iso(now_ms), "redelivery": false, "duration": 0.27,
            "status": if status == 0 { "Invalid HTTP Response: 503".to_string() } else if status < 300 { "OK".to_string() } else { format!("Invalid HTTP Response: {status}") }, "status_code": status,
            "event": "workflow_job", "action": payload["action"], "installation_id": INSTALLATION, "repository_id": payload["repository"]["id"], "throttled_at": null, "url": url,
            "request": { "headers": { "X-GitHub-Event": "workflow_job", "Content-Type": "application/json" }, "payload": payload },
            "response": { "headers": { "Content-Type": "application/json" }, "payload": answer } }));
    }

    /// The deliveries asked to be sent again.
    pub fn redelivered(&self) -> Vec<u64> { self.hub.lock().unwrap().attempts.clone() }

    /// The deliveries asked to be sent again since this was last asked, each as GitHub sends it again (its payload,
    /// signed): for a test to deliver, as GitHub does.
    pub fn to_send_again(&self) -> Vec<(Vec<u8>, String)> {
        let mut h = self.hub.lock().unwrap();
        let asked: Vec<u64> = h.attempts[h.resent..].to_vec();
        h.resent = h.attempts.len();
        let payloads: Vec<Value> = asked.iter().filter_map(|id| h.deliveries.iter().find(|d| d["id"] == *id).map(|d| d["request"]["payload"].clone())).collect();
        drop(h);
        payloads.iter().map(|p| self.signed(p)).collect()
    }

    /// The runners registered here (by the control plane), by name.
    pub fn runner_names(&self) -> Vec<String> { self.hub.lock().unwrap().runners.values().cloned().collect() }

    /// An object taken out of its bucket (as the bucket's own rule removes old ones).
    pub fn remove_object(&self, key: &str) { self.hub.lock().unwrap().objects.remove(key); }

    /// The buckets' objects' keys (`bucket/key`) and sizes.
    pub fn objects(&self) -> Vec<(String, usize)> { self.hub.lock().unwrap().objects.iter().map(|(k, v)| (k.clone(), v.len())).collect() }

    fn repo(&self) -> Value {
        let mut r = fixture("repo");
        if self.hub.lock().unwrap().public { r["private"] = json!(false); r["visibility"] = json!("public") }
        r
    }

    /// Cancelled before any runner took it: its webhook.
    pub fn cancel(&self, id: u64, now_ms: u64) -> (Vec<u8>, String) {
        if let Some(j) = self.hub.lock().unwrap().jobs.get_mut(&id) { [("status", json!("completed")), ("conclusion", json!("cancelled")), ("completed_at", json!(iso(now_ms)))].into_iter().for_each(|(k, v)| j[k] = v); }
        self.event("completed", id)
    }

    /// Ended at GitHub without the webhook arriving (taken by another runner and done, as far as GitHub says).
    pub fn ended_untold(&self, id: u64) {
        if let Some(j) = self.hub.lock().unwrap().jobs.get_mut(&id) { [("status", json!("completed")), ("conclusion", json!("success")), ("runner_name", json!("someone-else")), ("runner_id", json!(7))].into_iter().for_each(|(k, v)| j[k] = v); }
    }

    /// The App as the dashboard keeps it in the control plane's settings (`GITHUB_APP`), with a key made for this
    /// test process (GitHub checks nothing of it here; the control plane signs with it).
    pub fn app() -> Value {
        static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let pem = KEY.get_or_init(|| {
            let out = std::process::Command::new("openssl").args(["genrsa", "2048"]).output().expect("openssl, to make the App's key");
            String::from_utf8(out.stdout).unwrap()
        });
        json!({ "id": APP_ID, "slug": "superci-acme", "pem": pem, "webhook_secret": WEBHOOK_SECRET, "owner": OWNER, "owner_is_org": true })
    }

    /// A token GitHub gives a job (`ACTIONS_RUNTIME_TOKEN`), with these claims, signed with the key GitHub's key set
    /// (`token.actions.githubusercontent.com/.well-known/jwks`, here) has.
    pub fn job_token(claims: &Value) -> String {
        let mut c = claims.clone();
        c["iss"] = json!("https://token.actions.githubusercontent.com");
        superci_core::crypto::rs256_jwt(&c, Self::app()["pem"].as_str().unwrap()).unwrap()
    }

    /// GitHub's key set for its jobs' tokens: the public half of the key above.
    fn job_keys() -> Value {
        let mut openssl = std::process::Command::new("openssl").args(["rsa", "-noout", "-modulus"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().expect("openssl");
        { use std::io::Write; openssl.stdin.take().unwrap().write_all(Self::app()["pem"].as_str().unwrap().as_bytes()).unwrap(); }
        let out = String::from_utf8(openssl.wait_with_output().unwrap().stdout).unwrap();
        let hex = out.trim().trim_start_matches("Modulus=");
        let n: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        json!({ "keys": [{ "kty": "RSA", "alg": "RS256", "use": "sig", "n": superci_core::crypto::b64url(&n), "e": "AQAB" }] })
    }

    /// GitHub's record of job `id` as it stands.
    pub fn job(&self, id: u64) -> Value { self.hub.lock().unwrap().jobs.get(&id).cloned().unwrap_or_default() }

    /// A webhook's payload as given, signed as GitHub signs it (one GitHub would not send as it is, say).
    pub fn signed(&self, payload: &Value) -> (Vec<u8>, String) {
        let body = payload.to_string().into_bytes();
        let signature = format!("sha256={}", superci_core::crypto::hex(&superci_core::crypto::hmac_sha256(WEBHOOK_SECRET.as_bytes(), &body)));
        (body, signature)
    }

    /// The webhook GitHub sends of a job's change (`action`), as it sends it.
    fn event(&self, action: &str, id: u64) -> (Vec<u8>, String) {
        let payload = json!({ "action": action, "workflow_job": self.job(id), "repository": self.repo(), "organization": { "login": OWNER, "id": 9_919 },
            "installation": { "id": INSTALLATION, "node_id": "MDIzOkludGVncmF0aW9uSW5zdGFsbGF0aW9uNDI=" }, "sender": { "login": "octocat", "id": 1, "type": "User" } });
        let body = payload.to_string().into_bytes();
        let signature = format!("sha256={}", superci_core::crypto::hex(&superci_core::crypto::hmac_sha256(WEBHOOK_SECRET.as_bytes(), &body)));
        (body, signature)
    }

    /// A job queued in a run of `acme/app`, labelled `label`: its number, and the webhook to send.
    pub fn queue(&self, name: &str, label: &str, now_ms: u64) -> (u64, (Vec<u8>, String)) {
        let id = {
            let mut h = self.hub.lock().unwrap();
            h.next += 1;
            let (id, run) = (90_000_000_000 + h.next, 30_000_000_000 + h.next);
            let mut j = fixture("job");
            [("id", json!(id)), ("run_id", json!(run)), ("name", json!(name)), ("labels", json!([label])), ("status", json!("queued")), ("conclusion", Value::Null),
                ("created_at", json!(iso(now_ms))), ("started_at", json!(iso(now_ms))), ("completed_at", Value::Null), ("runner_id", Value::Null), ("runner_name", Value::Null),
                ("runner_group_id", Value::Null), ("runner_group_name", Value::Null), ("steps", json!([])), ("workflow_name", json!("Build and Test")), ("run_attempt", json!(1)),
                ("url", json!(format!("https://api.github.com/repos/{REPO}/actions/jobs/{id}"))), ("html_url", json!(format!("https://github.com/{REPO}/actions/runs/{run}/job/{id}"))),
                ("run_url", json!(format!("https://api.github.com/repos/{REPO}/actions/runs/{run}")))].into_iter().for_each(|(k, v)| j[k] = v);
            h.jobs.insert(id, j);
            id
        };
        (id, self.event("queued", id))
    }

    /// Its runner took it: the runner the control plane registered last (the machine started for it).
    pub fn start(&self, id: u64) -> (Vec<u8>, String) {
        {
            let mut h = self.hub.lock().unwrap();
            let (rid, name) = h.runners.iter().next_back().map(|(i, n)| (*i, n.clone())).expect("a runner registered for it");
            let j = h.jobs.get_mut(&id).unwrap();
            [("status", json!("in_progress")), ("runner_id", json!(rid)), ("runner_name", json!(name)), ("runner_group_id", json!(1)), ("runner_group_name", json!("Default"))].into_iter().for_each(|(k, v)| j[k] = v);
        }
        self.event("in_progress", id)
    }

    /// The runner of this name (registered here) is up and waits for work: online, as its machine came up, whether or
    /// not GitHub hands it a job.
    pub fn ready(&self, runner: &str) {
        let mut h = self.hub.lock().unwrap();
        let rid = h.runners.iter().find(|(_, n)| n.as_str() == runner).map(|(i, _)| *i).expect("that runner registered");
        h.online.insert(rid);
    }

    /// Taken by the runner of this name (any registered here), as GitHub hands a job to any runner with its label:
    /// its webhook, which a test may leave undelivered.
    pub fn start_on(&self, id: u64, runner: &str) -> (Vec<u8>, String) {
        {
            let mut h = self.hub.lock().unwrap();
            let rid = h.runners.iter().find(|(_, n)| n.as_str() == runner).map(|(i, _)| *i).expect("that runner registered");
            let j = h.jobs.get_mut(&id).unwrap();
            [("status", json!("in_progress")), ("runner_id", json!(rid)), ("runner_name", json!(runner)), ("runner_group_id", json!(1)), ("runner_group_name", json!("Default"))].into_iter().for_each(|(k, v)| j[k] = v);
        }
        self.event("in_progress", id)
    }

    /// Taken by a runner that is none of this control plane's (another CI's, GitHub's own): its webhook.
    pub fn start_elsewhere(&self, id: u64, runner: &str) -> (Vec<u8>, String) {
        {
            let mut h = self.hub.lock().unwrap();
            let j = h.jobs.get_mut(&id).unwrap();
            [("status", json!("in_progress")), ("runner_id", json!(999_999)), ("runner_name", json!(runner)), ("runner_group_id", json!(1)), ("runner_group_name", json!("Default"))].into_iter().for_each(|(k, v)| j[k] = v);
        }
        self.event("in_progress", id)
    }

    /// It ended (`success`, `failure`), with its whole log.
    pub fn finish(&self, id: u64, conclusion: &str, log: &str, now_ms: u64) -> (Vec<u8>, String) {
        {
            let mut h = self.hub.lock().unwrap();
            let j = h.jobs.get_mut(&id).unwrap();
            [("status", json!("completed")), ("conclusion", json!(conclusion)), ("completed_at", json!(iso(now_ms))),
                ("steps", json!([{ "name": "Set up job", "status": "completed", "conclusion": "success", "number": 1, "started_at": j["started_at"], "completed_at": iso(now_ms) },
                    { "name": "Run make test", "status": "completed", "conclusion": conclusion, "number": 2, "started_at": j["started_at"], "completed_at": iso(now_ms) }]))].into_iter().for_each(|(k, v)| j[k] = v);
            h.logs.insert(id, log.to_string());
        }
        self.event("completed", id)
    }

    /// Its steps as GitHub has them now (their names, numbers and times), for a job that ran some side by side.
    /// Its branch and commit (another than the fixture's `main` and its commit).
    pub fn set_commit(&self, id: u64, branch: &str, sha: &str) {
        let mut h = self.hub.lock().unwrap();
        let j = h.jobs.get_mut(&id).unwrap();
        (j["head_branch"], j["head_sha"]) = (branch.into(), sha.into());
    }

    /// Its run's workflow (another than the fixture's), by its id.
    pub fn set_workflow(&self, id: u64, workflow_id: u64) { self.hub.lock().unwrap().jobs.get_mut(&id).unwrap()["workflow_id"] = workflow_id.into() }

    pub fn set_steps(&self, id: u64, steps: Value) { self.hub.lock().unwrap().jobs.get_mut(&id).unwrap()["steps"] = steps }

    /// Its run ended, with these logs (GitHub keeps a run's logs, each step's apart, once the whole run has ended).
    pub fn run_ended(&self, id: u64, zip: Vec<u8>) {
        let mut h = self.hub.lock().unwrap();
        let run = h.jobs[&id]["run_id"].as_u64().unwrap();
        h.run_logs.insert(run, zip);
    }

    fn answer(&self, r: &Request) -> Option<Response> {
        let url = url::Url::parse(&r.url).ok()?;
        let path = url.path().to_string();
        let ok = |status: u16, v: Value| Some(Response::new(status, "application/json; charset=utf-8", v.to_string()));
        if r.url.starts_with(LOGS_AT) {
            if let Some(run) = path.rsplit('/').next()?.strip_prefix("run-") { return self.hub.lock().unwrap().run_logs.get(&run.parse().ok()?).map(|z| Response::new(200, "application/zip", z.clone())) }
            let id: u64 = path.rsplit('/').next()?.parse().ok()?;
            return self.hub.lock().unwrap().logs.get(&id).map(|l| Response::new(200, "text/plain", l.clone()));
        }
        // S3, at `<bucket>.s3.<region>.amazonaws.com` (a link signed or a call signed alike): put, read, and asked of.
        if let Some(bucket) = url.host_str().and_then(|h| h.strip_suffix(".amazonaws.com")).and_then(|h| h.split_once(".s3.")).map(|(b, _)| b.to_string()) {
            let key = format!("{bucket}/{}", path.trim_start_matches('/'));
            let mut h = self.hub.lock().unwrap();
            let missing = || Response::new(404, "application/xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message><Key>{}</Key></Error>", path.trim_start_matches('/')));
            let root = path.trim_start_matches('/').is_empty();
            // (Its keys, as ListObjectsV2 answers: those under `prefix`.)
            // (Its rules, as `?lifecycle` sets them: taken.)
            if root && url.query_pairs().any(|(k, _)| k == "lifecycle") { return Some(Response::new(200, "application/xml", "")) }
            let made = h.buckets.contains(&bucket) || h.objects.keys().any(|k| k.starts_with(&format!("{bucket}/")));
            if root && !made && r.method != "PUT" { return Some(Response::new(404, "application/xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message><BucketName>{bucket}</BucketName></Error>"))) }
            if root && r.method == "HEAD" { return Some(Response::new(200, "application/xml", "")) }
            if root && r.method == "GET" {
                let prefix = url.query_pairs().find(|(k, _)| k == "prefix").map(|(_, v)| v.into_owned()).unwrap_or_default();
                let keys: Vec<String> = h.objects.keys().filter_map(|k| k.strip_prefix(&format!("{bucket}/"))).filter(|k| k.starts_with(&prefix)).map(str::to_string).collect();
                let contents: String = keys.iter().map(|k| format!("<Contents><Key>{k}</Key><Size>{}</Size><LastModified>2026-10-15T10:00:00.000Z</LastModified></Contents>", h.objects[&format!("{bucket}/{k}")].len())).collect();
                return Some(Response::new(200, "application/xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult><Name>{bucket}</Name><Prefix>{prefix}</Prefix><KeyCount>{}</KeyCount><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>", keys.len())))
            }
            // (A bucket made: once; again, as S3 says to its owner.)
            if root && r.method == "PUT" {
                if !h.buckets.insert(bucket.clone()) { return Some(Response::new(409, "application/xml", "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>BucketAlreadyOwnedByYou</Code><Message>Your previous request to create the named bucket succeeded and you already own it.</Message></Error>")) }
                return Some(Response::new(200, "application/xml", ""))
            }
            if root && r.method == "DELETE" { h.buckets.remove(&bucket); return Some(Response::new(204, "application/xml", "")) }
            if !root && r.method == "PUT" && r.header("x-amz-copy-source").is_none() && !h.refuse_puts {
                let kept: Vec<(String, String)> = r.headers.iter().filter(|(k, _)| { let k = k.to_ascii_lowercase(); k.starts_with("x-amz-meta-") || k == "x-amz-checksum-sha256" }).map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
                h.meta.insert(key.clone(), kept);
            }
            return Some(match r.method.as_str() {
                "PUT" if h.refuse_puts => Response::new(403, "application/xml", "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>"),
                // (A copy within S3: `x-amz-copy-source: /bucket/key`.)
                "PUT" => match r.header("x-amz-copy-source").map(|c| c.trim_start_matches('/').to_string()) {
                    Some(from) => match h.objects.get(&from).cloned() {
                        Some(b) => { h.objects.insert(key, b); Response::new(200, "application/xml", "<CopyObjectResult><ETag>\"copied\"</ETag></CopyObjectResult>") }
                        None => missing(),
                    },
                    None => { h.objects.insert(key, r.body.clone()); Response::new(200, "application/xml", "") }
                },
                "HEAD" => match h.objects.get(&key) {
                    Some(b) => {
                        let mut res = Response::new(200, "application/octet-stream", "").with_header("content-length", &b.len().to_string());
                        for (k, v) in h.meta.get(&key).cloned().unwrap_or_default() { res = res.with_header(&k, &v) }
                        res
                    }
                    None => Response::new(404, "application/xml", ""),
                },
                // (A range of it, as S3 answers one: 206 with those bytes.)
                "GET" => match (h.objects.get(&key), r.header("range").and_then(|v| v.strip_prefix("bytes=")).and_then(|v| v.split_once('-'))) {
                    (Some(b), Some((from, to))) => {
                        let from = from.parse::<usize>().unwrap_or(0).min(b.len());
                        let to = to.parse::<usize>().map(|t| t + 1).unwrap_or(b.len()).clamp(from, b.len());
                        Response::new(206, "application/octet-stream", b[from..to].to_vec()).with_header("content-range", &format!("bytes {from}-{}/{}", to.saturating_sub(1), b.len()))
                    }
                    (Some(b), None) => Response::new(200, "application/octet-stream", b.clone()),
                    (None, _) => missing(),
                },
                "DELETE" => { h.objects.remove(&key); Response::new(204, "application/xml", "") }
                _ => return None,
            })
        }
        if url.host_str() == Some("token.actions.githubusercontent.com") && path == "/.well-known/jwks" {
            self.hub.lock().unwrap().jwks_asked += 1;
            return ok(200, Self::job_keys())
        }
        // STS, a control plane assuming the role it was given (as AWS's documentation answers it; no recording has
        // a control plane's own call): credentials for an hour.
        if url.host_str().is_some_and(|h| h.starts_with("sts.") && h.ends_with(".amazonaws.com")) && (r.url.contains("Action=AssumeRoleWithWebIdentity") || String::from_utf8_lossy(&r.body).contains("Action=AssumeRoleWithWebIdentity")) {
            return Some(Response::new(200, "text/xml", "<AssumeRoleWithWebIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIAEXAMPLEPLANE0001</AccessKeyId><SecretAccessKey>example-secret</SecretAccessKey><SessionToken>example-session-token</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::111111111111:assumed-role/superci-plane/plane</Arn><AssumedRoleId>AROAEXAMPLE:plane</AssumedRoleId></AssumedRoleUser></AssumeRoleWithWebIdentityResult><ResponseMetadata><RequestId>00000000-0000-4000-8000-000000000000</RequestId></ResponseMetadata></AssumeRoleWithWebIdentityResponse>"))
        }
        // The rest of AWS: as AWS's stand-in answers the CLI (its recordings, SSM kept).
        if let (Some(h), Some(aws)) = (url.host_str().filter(|h| h.ends_with(".amazonaws.com")), self.aws.lock().unwrap().clone()) {
            let _ = h;
            let target = r.header("x-amz-target").unwrap_or_default().to_string();
            let (status, headers, answer) = aws.handle_full(&r.method, &r.url, &target, &String::from_utf8_lossy(&r.body));
            let mut res = Response::new(status, if answer.starts_with('<') { "text/xml" } else { "application/x-amz-json-1.1" }, answer);
            for (k, v) in headers { res = res.with_header(&k, &v) }
            return Some(res)
        }
        if url.host_str() != Some("api.github.com") { return None }
        let permissions = if self.hub.lock().unwrap().older_app { json!({ "metadata": "read", "organization_self_hosted_runners": "write" }) } else { json!({ "actions": "write", "metadata": "read", "organization_self_hosted_runners": "write" }) };
        let installed = if self.hub.lock().unwrap().older_install { json!({ "metadata": "read", "organization_self_hosted_runners": "write" }) } else { permissions.clone() };
        let account = json!({ "login": OWNER, "id": 9_919, "type": "Organization", "avatar_url": "https://avatars.githubusercontent.com/u/9919?v=4", "html_url": format!("https://github.com/{OWNER}") });
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let mut h = self.hub.lock().unwrap();
        match (r.method.as_str(), segs.as_slice()) {
            // The App's own (docs.github.com/rest/apps).
            ("GET", ["app"]) => ok(200, json!({ "id": APP_ID, "slug": "superci-acme", "node_id": "A_kwDOAAAA", "owner": account, "name": "SuperCI acme", "description": "",
                "external_url": "https://superci.dev", "html_url": "https://github.com/apps/superci-acme", "created_at": "2026-10-01T10:00:00Z", "updated_at": "2026-10-01T10:00:00Z",
                "permissions": permissions, "events": ["workflow_job"], "installations_count": 1 })),
            ("GET", ["app", "installations"]) if h.nowhere => ok(200, json!([])),
            ("GET", ["app", "installations"]) => ok(200, json!([{ "id": INSTALLATION, "account": account, "repository_selection": if h.selected { "selected" } else { "all" }, "access_tokens_url": format!("https://api.github.com/app/installations/{INSTALLATION}/access_tokens"),
                "repositories_url": "https://api.github.com/installation/repositories", "html_url": format!("https://github.com/organizations/{OWNER}/settings/installations/{INSTALLATION}"),
                "app_id": APP_ID, "app_slug": "superci-acme", "target_id": 9_919, "target_type": "Organization", "permissions": installed, "events": ["workflow_job"],
                "created_at": "2026-10-01T10:00:00Z", "updated_at": "2026-10-01T10:00:00Z", "single_file_name": null, "suspended_at": null }])),
            ("POST", ["app", "installations", id, "access_tokens"]) if *id == INSTALLATION.to_string() => ok(201, json!({ "token": "ghs_16C7e42F292c6912E7710c838347Ae178B4a", "expires_at": "2099-01-01T00:00:00Z",
                "permissions": permissions, "repository_selection": "all" })),
            // The App uninstalled (docs.github.com/rest/apps/apps#delete-an-installation-for-the-authenticated-app).
            ("DELETE", ["app", "installations", id]) if *id == INSTALLATION.to_string() => { h.uninstalled = true; Some(Response::new(204, "text/plain", "")) }
            // The App's deliveries: newest first, each without what was sent and answered; one whole; sent again.
            ("GET", ["app", "hook", "deliveries"]) => ok(200, Value::Array(h.deliveries.iter().rev().map(|d| { let mut d = d.clone(); let o = d.as_object_mut().unwrap(); o.remove("request"); o.remove("response"); d }).collect())),
            ("GET", ["app", "hook", "deliveries", id]) => match h.deliveries.iter().find(|d| d["id"].to_string() == *id) { Some(d) => ok(200, d.clone()), None => ok(404, json!({ "message": "Not Found", "status": "404" })) },
            ("POST", ["app", "hook", "deliveries", id, "attempts"]) => { let id: u64 = id.parse().ok()?; h.attempts.push(id); ok(202, json!({})) }
            // The runner image maker's public list of builds, as GitHub served it (`fixtures/aws/public/image-list.jsonl`).
            ("GET", ["repos", "runs-on", "runner-images-for-aws", "releases"]) => {
                let recorded: Value = serde_json::from_str(include_str!("../../cli/fixtures/aws/public/image-list.jsonl").lines().next().unwrap()).unwrap();
                Some(Response::new(recorded["status"].as_u64().unwrap() as u16, "application/json; charset=utf-8", recorded["answer"].as_str().unwrap().to_string()))
            }
            // Who a name is, and the App made from the dashboard's manifest (docs.github.com/rest/apps/apps#create-a-github-app-from-a-manifest).
            ("GET", ["users", OWNER]) => ok(200, fixture("org")),
            // A person's own account (docs.github.com/rest/users/users#get-a-user).
            ("GET", ["users", "octocat"]) => ok(200, json!({ "login": "octocat", "id": 1, "type": "User", "site_admin": false, "name": "The Octocat", "html_url": "https://github.com/octocat" })),
            ("GET", ["users", _]) => ok(404, json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest/users/users#get-a-user", "status": "404" })),
            ("POST", ["app-manifests", _, "conversions"]) => {
                let app = Self::app();
                ok(201, json!({ "id": APP_ID, "slug": "superci-acme", "node_id": "A_kwDOAAAA", "owner": account, "name": "SuperCI acme", "description": null,
                    "external_url": "https://superci.dev", "html_url": "https://github.com/apps/superci-acme", "created_at": "2026-10-15T10:00:00Z", "updated_at": "2026-10-15T10:00:00Z",
                    "permissions": permissions, "events": ["workflow_job"], "client_id": "Iv1.8a61f9b3a7aba766", "client_secret": "1726be1638095a19edd134c77bde3aa2ece1e5d8",
                    "webhook_secret": WEBHOOK_SECRET, "pem": app["pem"] }))
            }
            ("PATCH", ["app", "hook", "config"]) => ok(200, json!({ "content_type": "json", "insecure_ssl": "0", "url": serde_json::from_slice::<Value>(&r.body).ok()?["url"] })),
            // A repository by its id (docs.github.com/rest/repos/repos#get-a-repository, as `/repositories/{id}`).
            ("GET", ["repositories", id]) => { drop(h); let r = self.repo(); if r["id"].to_string() == *id { ok(200, r) } else { ok(404, json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest/repos/repos#get-a-repository", "status": "404" })) } }
            ("GET", ["installation", "repositories"]) => { let selection = if h.selected { "selected" } else { "all" }; drop(h); ok(200, json!({ "total_count": 1, "repository_selection": selection, "repositories": [self.repo()] })) }
            // A runner for one job (docs.github.com/rest/actions/self-hosted-runners).
            // (A personal account's App registers its runners in each repository: the same, there.)
            ("POST", ["orgs", OWNER, "actions", "runners", "generate-jitconfig"] | ["repos", OWNER, "app", "actions", "runners", "generate-jitconfig"]) => {
                let body: Value = serde_json::from_slice(&r.body).ok()?;
                let name = body["name"].as_str()?.to_string();
                // A name it has already (not deleted) is refused, as GitHub refuses it.
                if h.runners.values().any(|n| *n == name) { return ok(409, json!({ "message": format!("Already exists - A runner with the name {name} already exists."), "documentation_url": "https://docs.github.com/rest/actions/self-hosted-runners#create-configuration-for-a-just-in-time-runner-for-an-organization", "status": "409" })) }
                h.next += 1;
                let id = 20 + h.next;
                h.runners.insert(id, name.clone());
                let labels: Vec<Value> = body["labels"].as_array()?.iter().enumerate().map(|(i, l)| json!({ "id": i + 1, "name": l, "type": "custom" })).collect();
                ok(201, json!({ "runner": { "id": id, "name": name, "os": "unknown", "status": "offline", "busy": false, "labels": labels },
                    // (As GitHub's: its files by name, `.runner` saying where the runner reaches GitHub: its broker and service,
                    // and an address no host could be, which the machine leaves out.)
                    "encoded_jit_config": superci_core::crypto::b64(json!({ "runner": id, ".runner": superci_core::crypto::b64(json!({ "AgentId": id, "ServerUrlV2": "https://broker.actions.githubusercontent.com/", "ServerUrl": "https://pipelinesghubeus1.actions.githubusercontent.com/abc/", "serverUrl": "https://pipelinesghubeus1.actions.githubusercontent.com/abc/", "serverUrlV2": "https://bad_host.example/" }).to_string().as_bytes()) }).to_string().as_bytes()) }))
            }
            ("GET", ["orgs", OWNER, "actions", "runners", id] | ["repos", OWNER, "app", "actions", "runners", id]) => {
                let id: u64 = id.parse().ok()?;
                let name = h.runners.get(&id)?.clone();
                let busy = h.jobs.values().any(|j| j["runner_id"] == id && j["status"] == "in_progress");
                let online = busy || h.online.contains(&id);
                ok(200, json!({ "id": id, "name": name, "os": "linux", "status": if online { "online" } else { "offline" }, "busy": busy, "labels": [] }))
            }
            ("DELETE", ["orgs", OWNER, "actions", "runners", id] | ["repos", OWNER, "app", "actions", "runners", id]) => { let id: u64 = id.parse().ok()?; h.runners.remove(&id); h.online.remove(&id); Some(Response::new(204, "text/plain", "")) }
            // Its repository, runs and jobs (in the shape recorded).
            ("GET", ["repos", o, n]) if format!("{o}/{n}") == REPO => { drop(h); ok(200, self.repo()) }
            ("GET", ["repos", o, n, "actions", "jobs", id]) if format!("{o}/{n}") == REPO => h.jobs.get(&id.parse().ok()?).cloned().map(|j| Response::new(200, "application/json", j.to_string())),
            // A run cancelled, as its Cancel button does: GitHub accepts it (its jobs end as `cancel` then says).
            ("POST", ["repos", o, n, "actions", "runs", run, "cancel"]) if format!("{o}/{n}") == REPO => {
                let run: u64 = run.parse().ok()?;
                if !h.jobs.values().any(|j| j["run_id"] == run) { return ok(404, json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest/actions/workflow-runs#cancel-a-workflow-run", "status": "404" })) }
                ok(202, json!({}))
            }
            ("GET", ["repos", o, n, "actions", "runs", run, "logs"] | ["repos", o, n, "actions", "runs", run, "attempts", _, "logs"]) if format!("{o}/{n}") == REPO => {
                let run: u64 = run.parse().ok()?;
                if !h.run_logs.contains_key(&run) { return ok(404, json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest/actions/workflow-runs#download-workflow-run-attempt-logs", "status": "404" })) }
                Some(Response::new(302, "text/plain", "").with_header("location", &format!("{LOGS_AT}/run-{run}")))
            }
            ("GET", ["repos", o, n, "actions", "jobs", id, "logs"]) if format!("{o}/{n}") == REPO => {
                let id: u64 = id.parse().ok()?;
                if !h.logs.contains_key(&id) { return ok(404, json!({ "message": "Not Found", "documentation_url": "https://docs.github.com/rest/actions/workflow-jobs#download-job-logs-for-a-workflow-run", "status": "404" })) }
                Some(Response::new(302, "text/plain", "").with_header("location", &format!("{LOGS_AT}/{id}")))
            }
            ("GET", ["repos", o, n, "actions", "runs", id]) if format!("{o}/{n}") == REPO => {
                let id: u64 = id.parse().ok()?;
                let jobs: Vec<&Value> = h.jobs.values().filter(|j| j["run_id"] == id).collect();
                let first = jobs.first()?;
                let mut run = fixture("run");
                let status = if jobs.iter().all(|j| j["status"] == "completed") { "completed" } else if jobs.iter().any(|j| j["status"] == "in_progress") { "in_progress" } else { "queued" };
                if first.get("workflow_id").is_some() { run["workflow_id"] = first["workflow_id"].clone() }
                [("id", json!(id)), ("status", json!(status)), ("conclusion", if status == "completed" { first["conclusion"].clone() } else { Value::Null }),
                    ("head_sha", first["head_sha"].clone()), ("head_branch", first["head_branch"].clone()),
                    ("run_attempt", json!(1)), ("created_at", first["created_at"].clone()), ("run_started_at", first["created_at"].clone()), ("updated_at", first["created_at"].clone())].into_iter().for_each(|(k, v)| run[k] = v);
                ok(200, run)
            }
            ("GET", ["repos", o, n, "actions", "runs", id, "jobs"]) if format!("{o}/{n}") == REPO => {
                let id: u64 = id.parse().ok()?;
                let jobs: Vec<Value> = h.jobs.values().filter(|j| j["run_id"] == id).cloned().collect();
                ok(200, json!({ "total_count": jobs.len(), "jobs": jobs }))
            }
            ("GET", ["repos", o, n, "actions", "runs"]) if format!("{o}/{n}") == REPO => {
                let want = url.query_pairs().find(|(k, _)| k == "status").map(|(_, v)| v.into_owned()).unwrap_or_default();
                let runs: Vec<u64> = h.jobs.values().filter(|j| want.is_empty() || j["status"] == want.as_str()).filter_map(|j| j["run_id"].as_u64()).collect();
                drop(h);
                let list: Vec<Value> = runs.into_iter().filter_map(|id| self.answer(&Request::new("GET", &format!("https://api.github.com/repos/{REPO}/actions/runs/{id}")))).filter_map(|r| serde_json::from_slice(&r.body).ok()).collect();
                ok(200, json!({ "total_count": list.len(), "workflow_runs": list }))
            }
            ("POST", ["repos", o, n, "actions", "jobs", _, "rerun"]) | ("POST", ["repos", o, n, "actions", "runs", _, "rerun-failed-jobs"]) if format!("{o}/{n}") == REPO => ok(201, json!({})),
            _ => None,
        }
    }

    /// `f` run each time a call with this method is made to an address with `part` in it (of GitHub's, GitLab's, S3's,
    /// AWS's), before it is answered: what another request does meanwhile.
    pub fn during(&self, method: &str, part: &str, f: impl Fn() + Send + Sync + 'static) { self.during.lock().unwrap().push((method.to_string(), part.to_string(), Arc::new(f))) }

    /// What a call with `method` to exactly this path (`/app`, not `/app/installations`) answers from now on: this
    /// status, this body.
    pub fn answer_at(&self, method: &str, path: &str, status: u16, body: &str) { self.hub.lock().unwrap().exact.push((method.into(), path.into(), status, body.into())) }
}

#[async_trait(?Send)]
impl Http for GitHub {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let hooks: Vec<Arc<dyn Fn() + Send + Sync>> = self.during.lock().unwrap().iter().filter(|(m, part, _)| *m == r.method && r.url.contains(part.as_str())).map(|(_, _, f)| f.clone()).collect();
        for f in hooks { f() }
        // (Counted for a sweep, which may fail it: answered with its service's error, or its connection dropped.)
        let faults = self.aws.lock().unwrap().as_ref().map(|a| a.faults.clone());
        if let Some(faults) = faults {
            let target = r.header("x-amz-target").unwrap_or_default().to_string();
            let doing = crate::action(&target, &String::from_utf8_lossy(&r.body));
            match faults.call(format!("{} {} {doing}", r.method, r.url).trim_end().to_string()) {
                Some(true) => return Err(format!("{} {}: the connection closed before an answer", r.method, r.url)),
                Some(false) => {
                    let (status, headers, body) = crate::sweep::failed_answer(&r.url, &target);
                    let mut res = Response::new(status, "application/json", body);
                    res.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
                    for (k, v) in headers { res = res.with_header(&k, &v) }
                    return Ok(res)
                }
                None => {}
            }
        }
        if let Some(answer) = self.gitlab.answer(&r) { return Ok(answer) }
        // Another control plane served on this computer (one moved to): asked as it is, as a deployed one would be.
        if r.url.starts_with("http://127.0.0.1:") { return Ok(local(&r)) }
        self.asked.lock().unwrap().push(format!("{} {}", r.method, r.url));
        let failed = self.hub.lock().unwrap().failing.iter().find(|(m, part, ..)| *m == r.method && r.url.contains(part.as_str())).map(|(_, _, s, a)| (*s, a.clone()));
        if let Some((status, answer)) = failed { return Ok(Response::new(status, "application/json", answer)) }
        let exact = url::Url::parse(&r.url).ok().and_then(|u| self.hub.lock().unwrap().exact.iter().find(|(m, p, ..)| *m == r.method && u.path() == p).map(|(_, _, s, b)| (*s, b.clone())));
        if let Some((status, body)) = exact { return Ok(Response::new(status, "application/json", body)) }
        let scripted = self.hub.lock().unwrap().scripted.iter().find(|(m, part, ..)| *m == r.method && r.url.contains(part.as_str())).cloned();
        if let Some((_, _, status, headers, body)) = scripted {
            let mut res = Response::new(status, "application/json", body);
            res = headers.iter().fold(res, |res, (k, v)| res.with_header(k, v));
            return Ok(res)
        }
        if let Some(answer) = launcher(&r, self.asked.lock().unwrap().len()) { return Ok(answer) }
        Ok(self.answer(&r).unwrap_or_else(|| {
            self.unanswered.lock().unwrap().push(format!("{} {}", r.method, r.url));
            Response::new(404, "text/plain", format!("not here: {} {}", r.method, r.url))
        }))
    }
}

/// A launcher in Modal (`*.modal.run`) or Cloudflare (`*.workers.dev`), as it answers a control plane: a sandbox or
/// container started for a job (Modal runs no Docker for a GitLab job), one stopped.
fn launcher(r: &Request, n: usize) -> Option<Response> {
    let url = url::Url::parse(&r.url).ok()?;
    let host = url.host_str()?;
    let modal = host.ends_with(".modal.run");
    if !(modal || host.ends_with(".workers.dev")) || r.method != "POST" { return None }
    let body: Value = serde_json::from_slice(&r.body).unwrap_or_default();
    let ok = |v: Value| Some(Response::new(200, "application/json", v.to_string()));
    match url.path() {
        "/launch" if modal && body["gitlab"].is_object() => Some(Response::new(400, "application/json", json!({ "detail": "Modal runs no Docker for GitLab jobs" }).to_string())),
        "/launch" if modal => ok(json!({ "id": format!("sb-{n:022}"), "kind": "sandbox" })),
        "/launch" => ok(json!({ "id": format!("{n:064x}"), "kind": "standard-2" })),
        "/stop" => ok(json!({ "stopped": true })),
        _ => None,
    }
}

/// A request to a server on this computer, made as it is (its method, headers and body), and its answer.
fn local(r: &Request) -> Response {
    use std::io::{Read, Write};
    let url = url::Url::parse(&r.url).unwrap();
    let host = format!("{}:{}", url.host_str().unwrap_or("127.0.0.1"), url.port().unwrap_or(80));
    let Ok(mut s) = std::net::TcpStream::connect(&host) else { return Response::new(502, "text/plain", "nothing answers there") };
    let path = format!("{}{}", url.path(), url.query().map(|q| format!("?{q}")).unwrap_or_default());
    let mut head = format!("{} {path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {}\r\nconnection: close\r\n", r.method, r.body.len());
    r.headers.iter().for_each(|(k, v)| head.push_str(&format!("{k}: {v}\r\n")));
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes()).and_then(|_| s.write_all(&r.body));
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(502);
    Response::new(status, "text/plain", text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}
