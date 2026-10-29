//! A headless control plane end to end against fake GitHub, STS and EC2: only the public documents and a yes/no health are
//! online; the dashboard's report of the AWS role connects it once; a signed queued job launches a spot machine with its runner
//! registration and the finished job terminates it.
use std::cell::RefCell;
use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use futures::executor::block_on;
use sha2::{Digest, Sha256};
use superci_core::crypto::{hex, hmac_sha256};
use superci_core::github::App;
use superci_core::plane::{role_arn, Launcher, Cache, Config, ControlPlane, Pool, Routing, Rule, RunnerImage};
use superci_core::io::{self, Clock, Containers, Http, Request, Response, Store, Timer};

/// The store: kept in memory. `1`: what another request does meanwhile, run before each operation (its name and
/// key), as a test sets it (see `meddling`); `2`: an operation it does not answer, once (see `refusing`).
#[derive(Default)]
struct Mem(RefCell<BTreeMap<String, String>>, Meddle, RefCell<Option<(String, String, usize)>>);
type Meddler = Box<dyn FnMut(&str, &str, &mut BTreeMap<String, String>)>;
#[derive(Default)]
struct Meddle(RefCell<Option<Meddler>>);
impl Mem {
    fn meddling(&self, f: impl FnMut(&str, &str, &mut BTreeMap<String, String>) + 'static) { *self.1 .0.borrow_mut() = Some(Box::new(f)) }
    /// An `op` of a key beginning with `prefix` not answered: the next one, or the one after `skip` of them.
    fn refusing(&self, op: &str, prefix: &str, skip: usize) { *self.2.borrow_mut() = Some((op.into(), prefix.into(), skip)) }
    fn before(&self, op: &str, k: &str) -> io::Result<()> {
        faults::op()?;
        let refused = {
            let mut r = self.2.borrow_mut();
            match r.as_mut() {
                Some((o, p, skip)) if o == op && k.starts_with(p.as_str()) => if *skip == 0 { *r = None; true } else { *skip -= 1; false },
                _ => false,
            }
        };
        if refused { return Err(format!("the store did not answer ({op} {k})")) }
        if let Some(f) = self.1 .0.borrow_mut().as_mut() { f(op, k, &mut self.0.borrow_mut()) }
        Ok(())
    }
}
#[async_trait(?Send)]
impl Store for Mem {
    async fn get(&self, k: &str) -> io::Result<Option<String>> { self.before("get", k)?; Ok(self.0.borrow().get(k).cloned()) }
    async fn put(&self, k: &str, v: String) -> io::Result<()> { self.before("put", k)?; self.0.borrow_mut().insert(k.into(), v); Ok(()) }
    async fn put_if_absent(&self, k: &str, v: String) -> io::Result<bool> { self.before("put_if_absent", k)?; let mut m = self.0.borrow_mut(); if m.contains_key(k) { return Ok(false) } m.insert(k.into(), v); Ok(true) }
    async fn swap(&self, k: &str, was: Option<&str>, v: String) -> io::Result<bool> {
        self.before("swap", k)?;
        let mut m = self.0.borrow_mut();
        if m.get(k).map(String::as_str) != was || faults::swaps_lose() { return Ok(false) }
        m.insert(k.into(), v);
        Ok(true)
    }
    async fn delete(&self, k: &str) -> io::Result<()> { self.before("delete", k)?; self.0.borrow_mut().remove(k); Ok(()) }
    async fn list(&self, p: &str) -> io::Result<Vec<(String, String)>> { self.before("list", p)?; Ok(self.0.borrow().iter().filter(|(k, _)| k.starts_with(p)).map(|(k, v)| (k.clone(), v.clone())).collect()) }
}

/// Faults for the sweep at the end of this file (`every_test_here_holds_with_each_call_and_store_operation_failing`):
/// on the thread a sweep runs a test on, each call to a fake cloud, each store operation and each wake asked of the
/// timer counted, and the one planned failed. Off everywhere else: nothing counted, nothing failed.
mod faults {
    use std::cell::RefCell;
    use superci_core::io::{self, Request, Response};

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Fault {
        /// The k-th call answered with its service's error, or its connection dropped.
        Call(usize),
        Dropped(usize),
        /// The k-th store operation not answered.
        Op(usize),
        /// Every call from the k-th on failing, every store operation from the k-th on: what a try again does not get past.
        CallsFrom(usize),
        OpsFrom(usize),
        /// Every swap from the k-th on losing, as to a writer that always gets there first.
        SwapsLoseFrom(usize),
    }

    #[derive(Default)]
    pub struct Plan { pub on: bool, pub fault: Option<Fault>, pub calls: usize, pub ops: usize, pub swaps: usize, depth: usize, pub cap: usize }
    thread_local! { pub static PLAN: RefCell<Plan> = RefCell::default(); }

    /// Counting on this thread, with `fault` planned, until `off`; a run that goes on far past what the clean run did
    /// (a test waiting for what a fault keeps from happening) is stopped at `cap` operations.
    pub fn on(fault: Option<Fault>, cap: usize) { PLAN.with(|p| *p.borrow_mut() = Plan { on: true, fault, cap, ..Plan::default() }) }
    /// The calls, operations and swaps counted, counting stopped.
    pub fn off() -> (usize, usize, usize) { PLAN.with(|p| { let mut p = p.borrow_mut(); p.on = false; (p.calls, p.ops, p.swaps) }) }

    fn runaway(p: &Plan) { if p.calls + p.ops > p.cap { panic!("{} calls and {} store operations: stopped", p.calls, p.ops) } }

    /// A call to a fake cloud, counted once however many fakes it passes through (the outermost counts): its failure
    /// when it is the one planned.
    pub struct Outer(bool);
    impl Drop for Outer { fn drop(&mut self) { if self.0 { PLAN.with(|p| p.borrow_mut().depth -= 1) } } }
    pub fn call(r: &Request) -> (Outer, Option<io::Result<Response>>) { call_to(&r.url) }
    /// A container started or stopped: refused when it is the call planned.
    pub fn container(what: &str) -> io::Result<()> {
        let (_outer, failed) = call_to(what);
        match failed { Some(Ok(_)) => Err(format!("{what}: the container service failed")), Some(Err(e)) => Err(e), None => Ok(()) }
    }
    fn call_to(url: &str) -> (Outer, Option<io::Result<Response>>) {
        PLAN.with(|p| {
            let mut p = p.borrow_mut();
            if !p.on { return (Outer(false), None) }
            p.depth += 1;
            if p.depth > 1 { return (Outer(true), None) }
            p.calls += 1;
            runaway(&p);
            let k = p.calls;
            let failed = match p.fault {
                Some(Fault::Dropped(n)) if n == k => Some(Err(format!("connection to {url} dropped"))),
                Some(Fault::Call(n)) if n == k => Some(Ok(failed_answer(url))),
                Some(Fault::CallsFrom(n)) if k >= n => Some(Ok(failed_answer(url))),
                _ => None,
            };
            (Outer(true), failed)
        })
    }

    /// A service's own error: AWS's query APIs answer in XML, its JSON APIs and the others in JSON.
    fn failed_answer(url: &str) -> Response {
        let json_api = url.contains("pricing.") || url.contains("lambda.") || url.contains("dynamodb.") || url.contains("monitoring.");
        if url.contains("amazonaws.com") && !json_api {
            return Response::new(500, "text/xml", "<Response><Errors><Error><Code>InternalError</Code><Message>An internal error has occurred</Message></Error></Errors></Response>");
        }
        Response::new(500, "application/json", serde_json::json!({ "__type": "InternalServerError", "message": "Server Error" }).to_string())
    }

    /// A store operation, or a wake asked of the timer: not answered when it is the one planned.
    pub fn op() -> io::Result<()> {
        PLAN.with(|p| {
            let mut p = p.borrow_mut();
            if !p.on { return Ok(()) }
            p.ops += 1;
            runaway(&p);
            let k = p.ops;
            match p.fault {
                Some(Fault::Op(n)) if n == k => Err(format!("the store did not answer (operation {k})")),
                Some(Fault::OpsFrom(n)) if k >= n => Err(format!("the store did not answer (operation {k})")),
                _ => Ok(()),
            }
        })
    }

    /// A swap that would be written: lost when it is one of those planned to lose.
    pub fn swaps_lose() -> bool {
        PLAN.with(|p| {
            let mut p = p.borrow_mut();
            if !p.on { return false }
            p.swaps += 1;
            matches!(p.fault, Some(Fault::SwapsLoseFrom(n)) if p.swaps >= n)
        })
    }
}

#[derive(Default)]
struct FakeClouds { calls: RefCell<Vec<(String, String, String)>>, headers: RefCell<Vec<(String, Vec<(String, String)>)>> }
#[async_trait(?Send)]
impl Http for FakeClouds {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        self.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
        self.headers.borrow_mut().push((r.url.clone(), r.headers.clone()));
        let json = |v: serde_json::Value| Ok(Response::new(201, "application/json", v.to_string()));
        let xml = |s: &str| Ok(Response::new(200, "text/xml", s.to_string()));
        let u = r.url.as_str();
        if u.ends_with("/app/installations") { return Ok(Response::new(200, "application/json", serde_json::json!([{ "id": 42, "account": { "login": "acme" }, "repository_selection": "all" }]).to_string())); }
        if u == "https://api.github.com/app/hook/config" && r.method == "PATCH" { return Ok(Response::new(200, "application/json", body.clone())); }
        if u.ends_with("/app/installations/42/access_tokens") { return json(serde_json::json!({ "token": "ghs_test" })); }
        // A second organization's App and installation.
        if u.ends_with("/app/installations/43/access_tokens") { return json(serde_json::json!({ "token": "ghs_beta" })); }
        if u.ends_with("/orgs/beta/actions/runners/generate-jitconfig") {
            assert_eq!(r.header("authorization"), Some("Bearer ghs_beta"), "registered with the second organization's own token");
            return json(serde_json::json!({ "encoded_jit_config": "JITBETA", "runner": { "id": 6 } }));
        }
        // A public repository's runs: one from the repository itself, one from a fork's pull request.
        if u == "https://api.github.com/repos/acme/site-public/actions/runs/3" { return Ok(Response::new(200, "application/json", serde_json::json!({ "event": "push", "repository": { "id": 1 }, "head_repository": { "id": 1 } }).to_string())); }
        if u == "https://api.github.com/repos/acme/docs-public/actions/runs/3" { return Ok(Response::new(200, "application/json", serde_json::json!({ "event": "pull_request", "repository": { "id": 2 }, "head_repository": { "id": 99 } }).to_string())); }
        if u.ends_with("/orgs/acme/actions/runners/generate-jitconfig") {
            // Runners 5, 101, 102, … in the order they are registered. A name GitHub has registered already, and not
            // deleted since (by that runner's id), is refused, as GitHub refuses it.
            let name = |b: &str| serde_json::from_str::<serde_json::Value>(b).ok().and_then(|v| v["name"].as_str().map(str::to_string));
            let calls = self.calls.borrow();
            let earlier = &calls[..calls.len() - 1];
            let registered: Vec<(usize, Option<String>)> = earlier.iter().enumerate().filter(|(_, c)| c.1.ends_with("/orgs/acme/actions/runners/generate-jitconfig")).map(|(i, c)| (i, name(&c.2))).collect();
            let id_of = |n: usize| if n == 0 { 5 } else { 100 + n as u64 };
            if let Some((k, (at, _))) = registered.iter().enumerate().rev().find(|(_, (_, n))| *n == name(&body)) {
                let deleted = earlier[at + 1..].iter().any(|c| c.0 == "DELETE" && c.1.ends_with(&format!("/actions/runners/{}", id_of(k))));
                if !deleted { return Ok(Response::new(409, "application/json", serde_json::json!({ "message": format!("Already exists - A runner with the name {} already exists.", name(&body).unwrap_or_default()), "status": "409" }).to_string())) }
            }
            return json(serde_json::json!({ "encoded_jit_config": "JITCONFIG+/=", "runner": { "id": id_of(registered.len()) } }));
        }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1977" {
            assert_eq!(r.header("private-token"), Some("glpat-test"));
            return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1977, "status": "pending", "tag_list": ["superci/cpu=2", "docker"] }).to_string()));
        }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1980" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1980, "status": "pending", "tag_list": ["superci/cpu=2"] }).to_string())); }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1979" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1979, "status": "pending", "tag_list": ["docker", "superci/cpu=2"] }).to_string())); }
        if u.starts_with("https://gitlab.example/api/v4/runners/") { return Ok(Response::new(if r.method == "DELETE" { 204 } else { 200 }, "application/json", "{}")); }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1978" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1978, "status": "pending", "tag_list": ["docker"] }).to_string())); }
        // Runners 55, 56, … in the order they are made.
        if u == "https://gitlab.example/api/v4/user/runners" {
            let n = 54 + self.calls.borrow().iter().filter(|c| c.1 == u).count();
            return json(serde_json::json!({ "id": n, "token": format!("glrt-runner{n}") }));
        }
        if u == "https://runners.acme.workers.dev/launch" { return json(serde_json::json!({ "id": "container-7", "kind": "standard-2" })); }
        if u == "https://runners.acme.workers.dev/stop" { return json(serde_json::json!({ "stopped": true })); }
        // AWS's Price List: a role from before it could read it, for on-demand machines.
        if u == "https://api.pricing.us-east-1.amazonaws.com/" && body.contains("instanceType") {
            return Ok(Response::new(400, "application/x-amz-json-1.1", serde_json::json!({ "__type": "AccessDeniedException", "message": "not authorized to perform: pricing:GetProducts" }).to_string()));
        }
        // AWS's Price List: gp3 storage in us-east-1.
        if u == "https://api.pricing.us-east-1.amazonaws.com/" && body.contains("gp3") {
            let product = serde_json::json!({ "terms": { "OnDemand": { "X": { "priceDimensions": { "Y": { "pricePerUnit": { "USD": "0.0800000000" } } } } } } }).to_string();
            return Ok(Response::new(200, "application/x-amz-json-1.1", serde_json::json!({ "PriceList": [product] }).to_string()));
        }
        if u.starts_with("https://sts.us-east-1.amazonaws.com/") { return xml("<AssumeRoleWithWebIdentityResponse><Credentials><AccessKeyId>ASIAX</AccessKeyId><SecretAccessKey>secret</SecretAccessKey><SessionToken>tok</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResponse>"); }
        // Oregon has no spot capacity left; Ohio has.
        if u == "https://ec2.us-west-2.amazonaws.com/" || u == "https://ec2.us-east-2.amazonaws.com/" {
            if body.contains("Action=DescribeImages") { return xml("<imagesSet><item><imageId>ami-there</imageId><creationDate>2026-09-01T00:00:00.000Z</creationDate></item></imagesSet>"); }
            if body.contains("Action=RunInstances") && u.contains("us-west-2") {
                return Ok(Response::new(500, "text/xml", "<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code><Message>There is no Spot capacity available that matches your request.</Message></Error></Errors></Response>"));
            }
            if body.contains("Action=RunInstances") { return xml("<RunInstancesResponse><instancesSet><item><instanceId>i-0ohio</instanceId></item></instancesSet></RunInstancesResponse>"); }
            if body.contains("Action=TerminateInstances") { return xml("<TerminateInstancesResponse/>"); }
            if body.contains("Action=DescribeSpotPriceHistory") { return xml("<spotPriceHistorySet><item><spotPrice>0.0500</spotPrice></item></spotPriceHistorySet>"); }
        }
        if u == "https://ec2.us-east-1.amazonaws.com/" {
            assert!(r.header("authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=ASIAX/"));
            if body.contains("Action=DescribeImages") { return xml("<imagesSet><item><imageId>ami-old</imageId><creationDate>2026-01-01T00:00:00.000Z</creationDate></item><item><imageId>ami-new</imageId><creationDate>2026-09-01T00:00:00.000Z</creationDate></item></imagesSet>"); }
            if body.contains("Action=RunInstances") { return xml("<RunInstancesResponse><instancesSet><item><instanceId>i-0abc</instanceId><placement><availabilityZone>us-east-1b</availabilityZone></placement></item></instancesSet></RunInstancesResponse>"); }
            if body.contains("Action=TerminateInstances") { return xml("<TerminateInstancesResponse/>"); }
            // Its zone's price: what it launched in (us-east-1b), not the lowest across zones.
            if body.contains("Action=DescribeSpotPriceHistory") && body.contains("AvailabilityZone=us-east-1b") { return xml("<spotPriceHistorySet><item><spotPrice>0.0650</spotPrice><timestamp>2026-09-01T00:00:00.000Z</timestamp></item></spotPriceHistorySet>"); }
            if body.contains("Action=DescribeSpotPriceHistory") { return xml("<spotPriceHistorySet><item><spotPrice>0.0710</spotPrice></item><item><spotPrice>0.0600</spotPrice></item></spotPriceHistorySet>"); }
        }
        // An account where no network of the control plane's own was made: AWS lists none.
        if u.starts_with("https://ec2.") && body.contains("Action=DescribeVpcs") { return xml("<DescribeVpcsResponse><vpcSet/></DescribeVpcsResponse>"); }
        Ok(Response::new(404, "text/plain", format!("unexpected {} {}", r.method, r.url)))
    }
}

struct FixedClock;
impl Clock for FixedClock { fn now_ms(&self) -> u64 { 1_790_000_000_000 } }
#[derive(Default)]
struct Wakes(RefCell<Vec<u64>>, RefCell<Vec<u64>>);
#[async_trait(?Send)]
impl Timer for Wakes {
    async fn wake_in(&self, ms: u64) -> io::Result<()> { faults::op()?; self.0.borrow_mut().push(ms); Ok(()) }
    async fn pause(&self, ms: u64) { self.1.borrow_mut().push(ms) }
}

const PLANE_URL: &str = "https://plane.acme.workers.dev";
const PLANE_ID: &str = "abc123def456";

fn app() -> App {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    // (One key for every App: making it is the slow part of a test, and the sweep runs each test many times.)
    static PEM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let pem = PEM.get_or_init(|| rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap().to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap().to_string()).clone();
    App { id: 99, slug: "superci-acme-abc123".into(), pem, webhook_secret: "whsec".into(), owner: "acme".into(), owner_is_org: true, host: None }
}

fn get(path: &str) -> Request { Request::new("GET", &format!("{PLANE_URL}{path}")) }

fn report(token: &str, role: &str) -> Request {
    Request::new("POST", &format!("{PLANE_URL}/aws/callback")).with_header("content-type", "application/json")
        .with_body(serde_json::json!({ "token": token, "accountId": "123456789012", "region": "us-east-1", "roleArn": role }).to_string())
}

fn webhook(action: &str, runner: Option<&str>) -> Request { job_event(action, 7, "acme/app", runner) }

fn job_event(action: &str, job: u64, repo: &str, runner: Option<&str>) -> Request { labelled(action, job, repo, runner, "superci") }

fn labelled(action: &str, job: u64, repo: &str, runner: Option<&str>, label: &str) -> Request {
    let payload = serde_json::json!({ "action": action, "workflow_job": { "id": job, "run_id": 3, "labels": [label], "runner_name": runner }, "repository": { "full_name": repo, "private": !repo.ends_with("-public") }, "installation": { "id": 42 } }).to_string();
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
    Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload)
}

fn json(r: &Response) -> serde_json::Value { serde_json::from_slice(&r.body).unwrap() }

/// A queued job of `branch` in acme/app (GitHub's id 77, default branch main), as GitHub sends it.
fn on_branch(job: u64, branch: &str) -> Request {
    let payload = serde_json::json!({ "action": "queued", "workflow_job": { "id": job, "run_id": 3, "labels": ["superci"], "head_branch": branch }, "repository": { "id": 77, "full_name": "acme/app", "private": true, "default_branch": "main" }, "installation": { "id": 42 } }).to_string();
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
    Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload)
}

#[test]
fn online_only_public_documents_and_a_yes_no_health() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let config = Config::new(PLANE_ID.into());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    for path in ["/", "/setup?token=x", "/api/status", "/github/start", "/aws/connect"] { assert_eq!(run(get(path)).status, 404, "{path}"); }
    assert_eq!(json(&run(get("/health"))), serde_json::json!({ "plane": PLANE_ID, "label": "superci", "github": false, "gitlab": false, "installed": false, "aws": false, "runners": false, "version": superci_core::plane::VERSION }));
    assert_eq!(json(&run(get("/.well-known/openid-configuration")))["issuer"], PLANE_URL);
    let jwk = &json(&run(get("/.well-known/jwks.json")))["keys"][0];
    assert_eq!((jwk["crv"].as_str(), jwk.get("d")), (Some("P-256"), None), "public key only");
    assert_eq!(run(report("anything", &role_arn("123456789012", PLANE_ID))).status, 409, "no connection in progress");
    assert_eq!(run(webhook("queued", None)).status, 401, "no App yet");
    assert_eq!(run(get("/status")).status, 404, "no dashboard key set: like any unknown path");
}

#[test]
fn aws_connects_once_from_the_stack_then_jobs_run_on_spot_machines() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.aws_region = Some("us-east-1".into());
    config.aws_connect_token = Some("connect-token-0001".into());
    config.device_keys = vec![(1_700_000_000_000, "an-expired-session-key".into()), (1_800_000_000_000, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let role = role_arn("123456789012", PLANE_ID);

    // The stack's report: the token, this control plane's own role, the chosen region; accepted once.
    assert_eq!(run(report("forged", &role)).status, 403);
    assert_eq!(run(report("connect-token-0001", "arn:aws:iam::123456789012:role/someone-elses")).status, 400);
    // An account that is not twelve digits, or another region than the connection's: refused alike.
    let said = |account: &str, region: &str| { let mut b = serde_json::json!({ "token": "connect-token-0001", "accountId": account, "region": region, "roleArn": role_arn(account, PLANE_ID) }); if account.is_empty() { b["roleArn"] = role.clone().into() } run(Request::new("POST", &format!("{PLANE_URL}/aws/callback")).with_header("content-type", "application/json").with_body(b.to_string())).status };
    assert_eq!((said("12345678901", "us-east-1"), said("12345678901x", "us-east-1"), said("123456789012", "eu-west-1")), (400, 400, 400));
    let ok = run(report("connect-token-0001", &role));
    assert_eq!((ok.status, json(&ok)["connected"].as_bool()), (200, Some(true)));
    assert_eq!(run(report("connect-token-0001", &role)).status, 409, "the token works once");
    assert_eq!(json(&run(get("/health"))), serde_json::json!({ "plane": PLANE_ID, "label": "superci", "github": true, "gitlab": false, "installed": true, "aws": true, "runners": true, "version": superci_core::plane::VERSION }));
    let sts = clouds.calls.borrow().iter().find(|c| c.1.contains("sts.")).unwrap().1.clone();
    let token = url::Url::parse(&sts).unwrap().query_pairs().find(|(k, _)| k == "WebIdentityToken").unwrap().1.to_string();
    let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!((claims["iss"].as_str(), claims["aud"].as_str(), claims["sub"].as_str()), (Some(PLANE_URL), Some("superci"), Some("plane:abc123def456")));

    // A forged webhook is refused; a queued job launches one spot machine with its runner registration.
    let mut forged = webhook("queued", None);
    forged.headers.retain(|(k, _)| k != "x-hub-signature-256");
    assert_eq!(run(forged).status, 401);
    assert_eq!(run(webhook("queued", None)).status, 202);
    let launch = clouds.calls.borrow().iter().find(|c| c.2.contains("Action=RunInstances")).cloned().expect("RunInstances");
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(launch.2.as_bytes()).into_owned().collect();
    let field = |n: &str| fields.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!((field("ImageId").as_str(), field("InstanceType").as_str(), field("InstanceMarketOptions.MarketType").as_str()), ("ami-new", "c8a.xlarge", "spot"));
    assert!(String::from_utf8(STANDARD.decode(field("UserData")).unwrap()).unwrap().contains("./run.sh --jitconfig JITCONFIG+/="));
    assert!(fields.iter().any(|(k, v)| k.starts_with("TagSpecification.1.Tag.") && v == PLANE_ID));
    // In AWS's console the machine is called by the CI's name, the repository and the job (nothing reads that name).
    assert!(fields.iter().any(|(k, v)| k.starts_with("TagSpecification.1.Tag.") && v == "superci-app-7"), "{fields:?}");
    assert_eq!(superci_core::plane::machine_name_in_aws("Acme CI", "acme/web.app", 42), "acme-ci-web.app-42");
    assert_eq!(superci_core::plane::machine_name_in_aws("SuperCI", "", 42), "superci-42");
    assert_eq!(run(webhook("queued", None)).status, 202, "a repeated delivery launches nothing more");
    store.0.borrow_mut().insert("job:77".into(), serde_json::json!({ "job_id": 77, "run_id": 3, "repo": "acme/app", "state": "launching", "at_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": "", "seen_in_progress": false }).to_string());
    run(job_event("queued", 77, "acme/app", None));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&store.0.borrow()["job:77"]).unwrap()["state"], "launched", "a claim stuck in launching is taken over");
    assert_eq!(clouds.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances")).count(), 2, "job 7 once, and the taken-over job 77");

    // Running, then finished: the machine that ran it is terminated.
    let runner = format!("superci-{PLANE_ID}-7");
    run(webhook("in_progress", Some(&runner)));
    run(webhook("completed", Some(&runner)));
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=TerminateInstances") && c.2.contains("i-0abc")));
    let job: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:7"]).unwrap();
    assert_eq!(job["state"], "done");
    let price = job["usd_per_hour"].as_f64().unwrap();
    let extras = superci_core::aws::extras_usd_per_hour(0.08, 60, 125, 3000, true);
    assert!((price - (0.065 + extras)).abs() < 1e-9, "its zone's spot price at launch, plus its disk and address: {price}");
    // Settled when it ended, at the prices AWS bills: a minute at least.
    let cost = job["cost_usd"].as_f64().unwrap();
    assert!((cost - (0.065 + extras) / 60.0).abs() < 1e-9 && job["cost_from"] == "prices", "{cost}");
    assert!(job["started_ms"].is_u64() && job["ended_ms"].is_u64());
    assert!(!wakes.0.borrow().is_empty());

    // The dashboard's view, with its session key only.
    assert_eq!(run(get("/status").with_header("authorization", "Bearer wrong-key-wrong-key")).status, 401);
    assert_eq!(run(get("/status").with_header("authorization", "Bearer an-expired-session-key")).status, 401, "expired");
    let status = json(&run(get("/status").with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!((status["app"]["slug"].as_str(), status["installations"][0]["account"].as_str(), status["aws"]["connected"].as_bool()), (Some("superci-acme-abc123"), Some("acme"), Some(true)));
    assert_eq!((status["jobs"][0]["job_id"].as_u64(), status["jobs"][0]["state"].as_str(), status["jobs"][0]["machine_id"].as_str(), status["jobs"][0]["cloud"].as_str()), (Some(7), Some("done"), Some("i-0abc"), Some("aws")));
}

#[test]
fn routing_set_in_the_control_plane_sends_jobs_to_a_cloudflare_agent_with_a_token_for_it_alone() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing = Routing { default: Some("cloudflare".into()), rules: vec![Rule { repo: "acme/heavy".into(), cloud: "modal".into() }], order: vec![], ..Default::default() };
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/health")))["runners"], true, "an agent counts as a connected cloud");

    // The workflow says only `runs-on: superci`; the policy picks Cloudflare.
    assert_eq!(run(job_event("queued", 8, "acme/app", None)).status, 202);
    let calls = clouds.calls.borrow().clone();
    let jit = calls.iter().find(|c| c.1.ends_with("/generate-jitconfig")).expect("runner registration");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&jit.2).unwrap()["labels"], serde_json::json!(["superci"]));
    let launch = calls.iter().find(|c| c.1 == "https://runners.acme.workers.dev/launch").expect("agent launch");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&launch.2).unwrap()["jit"], "JITCONFIG+/=");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&launch.2).unwrap().get("image_url"), None, "the small image unless set");
    assert!(!calls.iter().any(|c| c.2.contains("Action=RunInstances")), "nothing on AWS");
    let job: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:8"]).unwrap();
    assert_eq!((job["cloud"].as_str(), job["machine_id"].as_str(), job["state"].as_str()), (Some("cloudflare"), Some("container-7"), Some("launched")));

    // A rule naming a cloud that is not connected fails with a reason instead of running elsewhere.
    run(job_event("queued", 9, "acme/heavy", None));
    let failed: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:9"]).unwrap();
    assert_eq!(failed["state"], "failed");
    assert!(failed["error"].as_str().unwrap().contains("Modal is not connected"), "{}", failed["error"]);

    let runner = format!("superci-{PLANE_ID}-8");
    run(job_event("completed", 8, "acme/app", Some(&runner)));
    assert!(clouds.calls.borrow().iter().any(|c| c.1 == "https://runners.acme.workers.dev/stop" && c.2.contains("container-7")));
}

#[test]
fn a_control_plane_in_aws_uses_its_own_credentials_for_machines() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.aws_own = Some(("123456789012".into(), "us-east-1".into()));
    config.aws_own_creds = Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/health")))["aws"], true);
    run(webhook("queued", None));
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=RunInstances")));
    assert!(!clouds.calls.borrow().iter().any(|c| c.1.contains("sts.")), "no role to assume");
}

/// The fake clouds, with the control plane's own network in Ohio (two zones, b's spot price the lower, and no room
/// left there), and a role in Virginia from before it could read networks.
struct Networked(FakeClouds);
#[async_trait(?Send)]
impl Http for Networked {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        let xml = |s: &str| Ok(Response::new(200, "text/xml", s.to_string()));
        if r.url == "https://ec2.us-east-2.amazonaws.com/" {
            self.0.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
            if body.contains("Action=DescribeVpcs") && body.contains(&format!("Filter.1.Value.1={PLANE_ID}")) { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>") }
            if body.contains("Action=DescribeSubnets") { return xml("<subnetSet><item><subnetId>subnet-a</subnetId><availabilityZone>us-east-2a</availabilityZone></item><item><subnetId>subnet-b</subnetId><availabilityZone>us-east-2b</availabilityZone></item></subnetSet>") }
            if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>") }
            if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") {
                return xml("<spotPriceHistorySet><item><availabilityZone>us-east-2a</availabilityZone><spotPrice>0.0700</spotPrice></item><item><availabilityZone>us-east-2b</availabilityZone><spotPrice>0.0500</spotPrice></item></spotPriceHistorySet>")
            }
            if body.contains("Action=RunInstances") && body.contains("SubnetId=subnet-b") {
                return Ok(Response::new(500, "text/xml", "<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code><Message>no room</Message></Error></Errors></Response>"));
            }
            self.0.calls.borrow_mut().pop();
        }
        if r.url == "https://ec2.us-east-1.amazonaws.com/" && body.contains("Action=DescribeVpcs") {
            self.0.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
            return Ok(Response::new(403, "text/xml", "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized to perform: ec2:DescribeVpcs</Message></Error></Errors></Response>"));
        }
        self.0.send(r).await
    }
}

#[test]
fn machines_start_in_the_control_planes_own_network_cheapest_zone_first_else_the_default_one() {
    let (store, clouds, wakes, cache) = (Mem::default(), Networked(FakeClouds::default()), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-2".into()];
    // (Three types, so that each pool's place in the order shows.)
    config.instance_types = ["c8a.xlarge", "c7a.xlarge", "m7a.xlarge"].map(String::from).to_vec();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let tried = launches(&clouds.0);
    // Where a machine costs least for its speed first: the newest type in zone b (the cheaper zone), then the older
    // types there (still less than the newest in zone a), all out of room; then the newest in zone a. Each with its
    // subnet, the group that lets nothing in, and a public address.
    assert_eq!(tried.iter().map(|f| (field(f, "InstanceType").to_string(), field(f, "NetworkInterface.1.SubnetId").to_string())).collect::<Vec<_>>(),
        [("c8a.xlarge", "subnet-b"), ("c7a.xlarge", "subnet-b"), ("m7a.xlarge", "subnet-b"), ("c8a.xlarge", "subnet-a")].map(|(t, s)| (t.to_string(), s.to_string())));
    assert!(tried.iter().all(|f| field(f, "NetworkInterface.1.SecurityGroupId.1") == "sg-1" && field(f, "NetworkInterface.1.AssociatePublicIpAddress") == "true"));
    assert_eq!(job(&store, 7)["state"], "launched");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("denied:")));
    // The pool that had no room is remembered: the next job asks the other zone first, and wastes no request on it.
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    let next: Vec<String> = launches(&clouds.0).iter().skip(tried.len()).map(|f| field(f, "NetworkInterface.1.SubnetId")).collect();
    assert_eq!((next.as_slice(), job(&store, 8)["state"].as_str()), (["subnet-a".to_string()].as_slice(), Some("launched")), "{next:?}");

    // A role that cannot read networks: the region's default network, and what was refused noted for the dashboard.
    let (store, clouds, cache) = (Mem::default(), Networked(FakeClouds::default()), Cache::default());
    config.aws_regions = vec!["us-east-1".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let tried = launches(&clouds.0);
    assert!(!tried.is_empty() && tried.iter().all(|f| field(f, "NetworkInterface.1.SubnetId").is_empty()));
    assert_eq!(job(&store, 7)["state"], "launched");
    assert!(store.0.borrow().contains_key("denied:aws:ReadNetwork"));
}

#[test]
fn a_region_with_no_spot_capacity_left_sends_the_machine_to_the_next() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-west-2".into(), "us-east-2".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    run(webhook("queued", None));
    let tried: Vec<String> = clouds.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances")).map(|c| c.1.clone()).collect();
    // Every type in Oregon first (each out of capacity), then Ohio's first.
    assert_eq!(tried.last().map(String::as_str), Some("https://ec2.us-east-2.amazonaws.com/"));
    assert!(tried.len() > 1 && tried[..tried.len() - 1].iter().all(|u| u.contains("us-west-2")));
    let j = job(&store, 7);
    assert_eq!((j["machine_id"].as_str(), j["region"].as_str(), j["state"].as_str()), (Some("i-0ohio"), Some("us-east-2"), Some("launched")));
    // Its machine is ended where it is.
    run(webhook("completed", Some(&format!("superci-{PLANE_ID}-7"))));
    assert!(clouds.calls.borrow().iter().any(|c| c.1 == "https://ec2.us-east-2.amazonaws.com/" && c.2.contains("Action=TerminateInstances")));
}

#[test]
fn moving_hands_everything_to_the_new_control_plane_once_and_the_old_one_says_where() {
    let key = "dashboard-session-key-0123";
    let (old_store, new_store, clouds, wakes, cache, cache2) = (Mem::default(), Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), Cache::default());
    let mut old_config = Config::new(PLANE_ID.into());
    old_config.app = Some(app());
    old_config.device_keys = vec![(u64::MAX, key.into())];
    old_config.move_token = Some("move-token-out-0123456789".into());
    let mut new_config = Config::new("newplane0001".into());
    new_config.device_keys = vec![(u64::MAX, key.into())];
    new_config.move_token = Some("move-token-in-0123456789".into());
    let old = ControlPlane { store: &old_store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &old_config, cache: &cache, containers: None };
    const NEW_URL: &str = "https://superci.other-account.workers.dev";
    let call = |plane: &ControlPlane, url: &str, path: &str, token: Option<&str>, body: serde_json::Value| {
        let mut r = Request::new("POST", &format!("{url}{path}")).with_header("authorization", &format!("Bearer {key}")).with_body(body.to_string());
        if let Some(t) = token { r = r.with_header("x-move-token", t) }
        block_on(plane.handle(r))
    };
    // A finished job in the old one's history.
    old_store.0.borrow_mut().insert("job:41".into(), serde_json::json!({ "job_id": 41, "run_id": 4, "repo": "acme/app", "state": "done", "at_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());

    // Its settings, only with the token set through the cloud, and only once.
    assert_eq!(call(&old, PLANE_URL, "/move/export", None, serde_json::json!({})).status, 403);
    assert_eq!(call(&old, PLANE_URL, "/move/export", Some("move-token-wrong-0123456789"), serde_json::json!({})).status, 403);
    let out = json(&call(&old, PLANE_URL, "/move/export", Some("move-token-out-0123456789"), serde_json::json!({})));
    assert_eq!(out["app"]["slug"], "superci-acme-abc123");
    assert!(out["state"].as_array().unwrap().iter().any(|p| p[0] == "job:41"));
    assert_eq!(call(&old, PLANE_URL, "/move/export", Some("move-token-out-0123456789"), serde_json::json!({})).status, 403, "a token is taken once");

    // The new one, with the settings the dashboard handed it: the history comes in, then it takes GitHub's App.
    new_config.app = serde_json::from_value(out["app"].clone()).ok();
    let new = ControlPlane { store: &new_store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &new_config, cache: &cache2, containers: None };
    assert_eq!(json(&call(&new, NEW_URL, "/move/import", Some("move-token-in-0123456789"), serde_json::json!({ "state": out["state"] })))["taken"], 1);
    assert_eq!(json(&block_on(new.handle(Request::new("GET", &format!("{NEW_URL}/health")))))["standby"], true, "not in use before the switch");
    let claimed = json(&call(&new, NEW_URL, "/move/claim", None, serde_json::json!({ "from": PLANE_URL })));
    assert_eq!(claimed["github"], true);
    let patch = clouds.calls.borrow().iter().find(|c| c.1.ends_with("/app/hook/config")).cloned().expect("the App's webhook moved");
    assert_eq!((patch.0.as_str(), serde_json::from_str::<serde_json::Value>(&patch.2).unwrap()["url"].as_str()), ("PATCH", Some(&*format!("{NEW_URL}/webhook"))));

    // The old one says where it went.
    call(&old, PLANE_URL, "/move/away", None, serde_json::json!({ "to": NEW_URL }));
    assert_eq!(json(&block_on(old.handle(get("/health"))))["moved_to"], NEW_URL);
    assert!(json(&block_on(new.handle(Request::new("GET", &format!("{NEW_URL}/health"))))).get("standby").is_none(), "in use after it");
    // An event that still reaches the old one goes on to the new one, as it came.
    let late = webhook("queued", None);
    let sig = late.header("x-hub-signature-256").map(str::to_string);
    let mut forwarded = late.clone();
    forwarded.url = format!("{PLANE_URL}/webhook");
    // (With what a proxy on the way adds, and GitHub's own headers.)
    for (k, v) in [("content-type", "application/json"), ("user-agent", "GitHub-Hookshot/1a2b3c"), ("x-github-delivery", "d-1"), ("x-gitlab-instance", "https://gitlab.example"), ("x-forwarded-for", "203.0.113.9"), ("authorization", "Bearer someone-else")] {
        forwarded = forwarded.with_header(k, v);
    }
    block_on(old.handle(forwarded.clone()));
    let passed = clouds.calls.borrow().iter().rev().find(|c| c.1 == format!("{NEW_URL}/webhook")).cloned().expect("passed on");
    assert_eq!(passed.2, String::from_utf8(late.body.clone()).unwrap());
    assert!(sig.is_some());
    // GitHub's and GitLab's headers go on with it, its type and who sent it; nothing else.
    let sent: Vec<String> = clouds.headers.borrow().iter().rev().find(|(u, _)| *u == format!("{NEW_URL}/webhook")).map(|(_, h)| h.iter().map(|(k, _)| k.clone()).collect()).unwrap();
    for k in ["x-hub-signature-256", "x-github-event", "x-github-delivery", "x-gitlab-instance", "content-type", "user-agent"] { assert!(sent.iter().any(|s| s == k), "{k} in {sent:?}") }
    assert!(!sent.iter().any(|s| s == "x-forwarded-for" || s == "authorization"), "{sent:?}");
    // One not signed by GitHub is not passed on.
    let before = clouds.calls.borrow().len();
    let mut forged = forwarded.clone();
    forged.headers.retain(|(k, _)| k != "x-hub-signature-256");
    forged = forged.with_header("x-hub-signature-256", "sha256=00");
    assert_eq!(block_on(old.handle(forged)).status, 401);
    assert_eq!(clouds.calls.borrow().len(), before);
    assert!(new_store.0.borrow().contains_key("job:41"), "the history came along");
    // And back: the old one, claimed again, is in use again.
    call(&old, PLANE_URL, "/move/claim", None, serde_json::json!({ "from": NEW_URL }));
    assert!(json(&block_on(old.handle(get("/health")))).get("moved_to").is_none());
}

/// The fake clouds, with every machine refused for `code` (as EC2 says it, in its XML).
struct Refusing { clouds: FakeClouds, code: &'static str }
#[async_trait(?Send)]
impl Http for Refusing {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        if body.contains("Action=RunInstances") || body.contains("Action=CreateFleet") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body));
            return Ok(Response::new(400, "text/xml", format!("<Response><Errors><Error><Code>{}</Code><Message>gone</Message></Error></Errors></Response>", self.code)));
        }
        self.clouds.send(r).await
    }
}

#[test]
fn a_network_or_an_image_aws_says_is_gone_is_looked_up_anew_for_the_next_job() {
    for (code, action, anew) in [("InvalidSubnetID.NotFound", "Action=DescribeVpcs", true), ("InvalidGroup.NotFound", "Action=DescribeVpcs", true), ("InvalidSecurityGroupID.NotFound", "Action=DescribeVpcs", true),
        ("InvalidAMIID.NotFound", "Action=DescribeImages", true), ("InvalidAMIID.NotFound", "Action=DescribeVpcs", false), ("Unsupported", "Action=DescribeVpcs", false), ("Unsupported", "Action=DescribeImages", false)] {
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let http = Refusing { clouds: FakeClouds::default(), code };
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        let asked = || http.clouds.calls.borrow().iter().filter(|c| c.2.contains(action)).count();
        block_on(plane.handle(job_event("queued", 7, "acme/app", None)));
        let first = asked();
        assert!(first > 0, "{code}: {action} asked for the first job: {:?}", http.clouds.calls.borrow().iter().map(|c| c.2.split('&').next().unwrap_or_default().to_string()).collect::<Vec<_>>());
        block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
        // (Anything else AWS refuses for: what was looked up stays, for a while.)
        assert_eq!(asked() > first, anew, "{code}: {action} asked anew for the next job: {anew}");
    }
}

#[derive(Default)]
struct OwnContainers(RefCell<Vec<String>>);
#[async_trait(?Send)]
impl Containers for OwnContainers {
    async fn start(&self, name: &str, work: &superci_core::io::Work, _max: u32, size: superci_core::spec::Size) -> io::Result<String> {
        faults::container(name)?;
        let what = match work { superci_core::io::Work::GitHub { jit } => jit.clone(), superci_core::io::Work::GitLab { url, token } => format!("gitlab {url} {token}"),
            superci_core::io::Work::Fail { jit, why } => format!("fail {jit} {why}") };
        self.0.borrow_mut().push(format!("start {name} {what} {}cpu {}gb {}disk", size.cpu, size.ram_gb, size.disk_gb));
        Ok(name.to_string())
    }
    async fn start_from(&self, name: &str, work: &superci_core::io::Work, max: u32, size: superci_core::spec::Size, image: &str, secret: Option<&str>) -> io::Result<String> {
        self.start(name, work, max, size).await?;
        self.0.borrow_mut().push(format!("from {image} {}", secret.unwrap_or("public")));
        Ok(name.to_string())
    }
    async fn start_function(&self, name: &str, work: &superci_core::io::Work, _max: u32, size: superci_core::spec::Size, image: Option<(&str, Option<&str>)>, notice: &str) -> io::Result<String> {
        let superci_core::io::Work::GitHub { jit } = work else { return Err("not a GitHub job".into()) };
        self.0.borrow_mut().push(format!("function {name} {jit} {}cpu {}gb {} {notice}", size.cpu, size.ram_gb, image.map_or("github".into(), |(r, s)| format!("{r}|{}", s.unwrap_or("public")))));
        Ok(format!("fc-{name}"))
    }
    async fn stop(&self, id: &str) -> io::Result<()> { faults::container(id)?; self.0.borrow_mut().push(format!("stop {id}")); Ok(()) }
}

#[test]
fn a_cloudflare_control_plane_starts_containers_itself() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/health")))["runners"], true, "its own containers count as runners");
    run(webhook("queued", None));
    let name = format!("superci-{PLANE_ID}-7");
    // The standard machine there is its largest (CPU is billed only while used): 4 CPU, 12 GB, 20 GB.
    assert_eq!(own.0.borrow().first().cloned(), Some(format!("start {name} JITCONFIG+/= 4cpu 12gb 20disk")));
    let first = job(&store, 7);
    assert_eq!((first["cloud"].as_str(), first["machine_id"].as_str(), first["machine_type"].as_str()), (Some("cloudflare"), Some(name.as_str()), Some("4cpu-12gb")));
    let per_hour = first["usd_per_hour"].as_f64().unwrap();
    assert!((per_hour - (4.0 * 0.000020 + 12.0 * 0.0000025 + 20.0 * 0.00000007) * 3600.0).abs() < 1e-9, "{per_hour}");
    assert!(!clouds.calls.borrow().iter().any(|c| c.2.contains("RunInstances") || c.1.contains("/launch")), "no AWS, no agent");
    run(webhook("completed", Some(&name)));
    assert_eq!(own.0.borrow().last().cloned(), Some(format!("stop {name}")));
    // A label's size: 2 CPU gets 8 GB.
    run(labelled("queued", 8, "acme/app", None, "superci/cpu=2"));
    assert_eq!(own.0.borrow().last().cloned(), Some(format!("start superci-{PLANE_ID}-8 JITCONFIG+/= 2cpu 8gb 20disk")));
    // With no limit set, Cloudflare runs 20 at once: the 21st waits.
    for id in 9..29 { run(labelled("queued", id, "acme/app", None, "superci")); }
    assert_eq!((job(&store, 27)["state"].as_str(), job(&store, 28)["state"].as_str()), (Some("launched"), Some("waiting")));
    assert_eq!(job(&store, 28)["error"], "waiting: Cloudflare runs 20 at once");
}

#[test]
fn a_machine_that_never_begins_its_job_is_tried_again_then_the_job_fails() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(webhook("queued", None)));
    let starts = || own.0.borrow().iter().filter(|c| c.starts_with("start")).count();
    assert_eq!(starts(), 1);
    // Two minutes: still waiting for it.
    clock.0.set(clock.0.get() + 2 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(starts(), 1);
    // Over three: its container is stopped, its registration withdrawn, and it starts again; twice.
    for n in 2..=3 {
        clock.0.set(clock.0.get() + 4 * 60_000);
        block_on(plane.alarm()).unwrap();
        assert_eq!((starts(), job(&store, 7)["state"].as_str(), job(&store, 7)["retries"].as_u64()), (n, Some("launched"), Some(n as u64 - 1)));
        // What each machine before cost stays in the job's (one that never took it is paid all the same).
        let earlier = job(&store, 7)["earlier_usd"].as_f64().unwrap_or(0.0);
        assert!(earlier > 0.0, "machine {n}: {}", job(&store, 7));
    }
    assert!(own.0.borrow().iter().any(|c| c.starts_with("stop")) && clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1.contains("/actions/runners/")));
    // The third time it fails, saying why.
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((starts(), job(&store, 7)["state"].as_str(), job(&store, 7)["error"].as_str()), (3, Some("failed"), Some("Cloudflare did not start a machine for it, three times")));
}

/// GitHub as it answers while its Actions hand out no jobs: job 7 still queued, and each runner of ours online and idle
/// while `online` holds (else offline: its machine never came up).
fn github_holding(online: &std::cell::Cell<bool>) -> impl Fn(&Request, &str) -> Option<Response> + '_ {
    move |r: &Request, _: &str| {
        if r.method != "GET" { return None }
        if r.url == "https://api.github.com/repos/acme/app/actions/jobs/7" { return Some(Response::new(200, "application/json", serde_json::json!({ "id": 7, "status": "queued", "runner_name": null }).to_string())) }
        let id = r.url.strip_prefix("https://api.github.com/orgs/acme/actions/runners/")?.parse::<u64>().ok()?;
        Some(Response::new(200, "application/json", serde_json::json!({ "id": id, "status": if online.get() { "online" } else { "offline" }, "busy": false }).to_string()))
    }
}

#[test]
fn a_machine_whose_runner_waits_while_github_keeps_its_job_queued_is_ended_and_the_job_held_then_given_one_again() {
    let online = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), github_holding(&online));
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(webhook("queued", None)));
    let starts = || own.0.borrow().iter().filter(|c| c.starts_with("start")).count();
    let stops = || own.0.borrow().iter().filter(|c| c.starts_with("stop")).count();
    // Two minutes: its runner is up and GitHub has not handed it the job yet; waited for.
    clock.0.set(clock.0.get() + 2 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((starts(), stops(), job(&store, 7)["state"].as_str()), (1, 0, Some("launched")));
    // Over three: GitHub still keeps the job from a runner that waits for it. Its machine is ended (another would wait
    // the same), and the job is held with no machine, said as GitHub's doing; not counted as a machine that did not start.
    clock.0.set(clock.0.get() + 2 * 60_000);
    block_on(plane.alarm()).unwrap();
    let held = job(&store, 7);
    assert_eq!((starts(), stops(), held["state"].as_str(), held["retries"].as_u64()), (1, 1, Some("waiting"), Some(0)), "{held}");
    assert!(held["error"].as_str().unwrap().starts_with("waiting: GitHub had not handed this job to its runner 4 minutes after its machine was started, though the runner was ready for it"), "{held}");
    assert_eq!(held["held_until_ms"].as_u64(), Some(clock.0.get() + 10 * 60_000));
    assert!(held["earlier_usd"].as_f64().unwrap_or(0.0) > 0.0, "what its machine cost stays in the job's");
    assert!(clouds.0.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://api.github.com/orgs/acme/actions/runners/5"), "its runner withdrawn");
    // Held: no machine for ten minutes, however often the sweep comes by.
    for _ in 0..3 {
        clock.0.set(clock.0.get() + 3 * 60_000);
        block_on(plane.alarm()).unwrap();
        assert_eq!((starts(), job(&store, 7)["state"].as_str()), (1, Some("waiting")));
    }
    // Then a machine again, its hold over.
    clock.0.set(clock.0.get() + 2 * 60_000);
    block_on(plane.alarm()).unwrap();
    let again = job(&store, 7);
    assert_eq!((starts(), again["state"].as_str(), again.get("held_until_ms")), (2, Some("launched"), None), "{again}");
    // This one never comes up (its runner offline): started again as a machine that did not start, twice; then the
    // job fails, and GitHub, which still has it queued, is told so by a failing runner rather than keeping it a day.
    online.set(false);
    for n in 3..=4 {
        clock.0.set(clock.0.get() + 4 * 60_000);
        block_on(plane.alarm()).unwrap();
        assert_eq!((starts(), job(&store, 7)["state"].as_str(), job(&store, 7)["retries"].as_u64()), (n, Some("launched"), Some(n as u64 - 2)));
    }
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    let failed = job(&store, 7);
    assert_eq!((failed["state"].as_str(), failed["error"].as_str(), failed["failed_fast"].as_bool()), (Some("failed"), Some("Cloudflare did not start a machine for it, three times"), Some(true)), "{failed}");
    assert!(own.0.borrow().last().is_some_and(|c| c.starts_with(&format!("start superci-{PLANE_ID}-7-x fail "))), "{:?}", own.0.borrow());
}

#[test]
fn a_job_whose_machine_never_starts_while_github_cannot_be_asked_fails_here_alone() {
    // GitHub does not answer for the job (it may have ended, or been taken elsewhere): no failing runner is started for
    // it, as it might take another job with the label.
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(webhook("queued", None)));
    for _ in 0..3 { clock.0.set(clock.0.get() + 4 * 60_000); block_on(plane.alarm()).unwrap() }
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7).get("failed_fast")), (Some("failed"), None));
    assert!(!own.0.borrow().iter().any(|c| c.contains(" fail ")), "{:?}", own.0.borrow());
}

#[test]
fn a_start_cut_off_midway_is_placed_again_by_the_sweep() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    // The claim was made, then the runtime stopped before the machine was asked for.
    let claim = serde_json::json!({ "job_id": 7, "run_id": 70, "repo": "acme/app", "state": "launching", "at_ms": clock.0.get(), "runner": null, "runner_id": null,
        "installation_id": 42, "cloud": "", "machine_id": null, "machine_type": null, "error": null, "seen_in_progress": false, "label": "superci" });
    store.0.borrow_mut().insert("job:7".into(), claim.to_string());
    block_on(plane.alarm()).unwrap();
    assert_eq!((job(&store, 7)["state"].as_str(), own.0.borrow().len()), (Some("launching"), 0), "not yet: it may still be starting");
    clock.0.set(clock.0.get() + 3 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7)["retries"].as_u64()), (Some("launched"), Some(1)));
    assert!(own.0.borrow().iter().any(|c| c.starts_with("start")));
}

#[test]
fn a_runner_that_takes_a_job_whose_machine_failed_leaves_its_own_job_a_new_machine() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    run(job_event("queued", 7, "acme/app", None));
    run(job_event("queued", 8, "acme/app", None));
    // Job 7's machine failed; job 8's runner then takes job 7 (GitHub gives a waiting job to any runner with the label).
    let mut seven = job(&store, 7);
    seven["state"] = "failed".into();
    store.0.borrow_mut().insert("job:7".into(), seven.to_string());
    let starts = || own.0.borrow().iter().filter(|c| c.starts_with("start")).count();
    assert_eq!(starts(), 2);
    let runner8 = format!("superci-{PLANE_ID}-8");
    run(job_event("in_progress", 7, "acme/app", Some(&runner8)));
    // Job 7 runs on that runner; job 8 gets a machine of its own again.
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7)["runner"].as_str()), (Some("running"), Some(runner8.as_str())));
    assert_eq!((job(&store, 8)["state"].as_str(), starts()), (Some("launched"), 3));
    // Job 7 finishing ends that runner's machine and is recorded as done; job 8 stays with its new machine.
    run(job_event("completed", 7, "acme/app", Some(&runner8)));
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 8)["state"].as_str()), (Some("done"), Some("launched")));
}

fn job(store: &Mem, id: u64) -> serde_json::Value { serde_json::from_str(&store.0.borrow()[&format!("job:{id}")]).unwrap() }

fn own_aws() -> (Option<(String, String)>, Option<superci_core::aws::Credentials>) {
    (Some(("123456789012".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }))
}

/// The fields of each RunInstances call, in order.
fn launches(clouds: &FakeClouds) -> Vec<Vec<(String, String)>> {
    clouds.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances")).map(|c| url::form_urlencoded::parse(c.2.as_bytes()).into_owned().collect()).collect()
}
fn field(fields: &[(String, String)], n: &str) -> String { fields.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap_or_default() }

/// The fake clouds, where AWS has images of every kind (Windows' disk is 100 GB) and, with `no_gpus`, an account that
/// may run no GPU machines.
struct Kinds { clouds: FakeClouds, no_gpus: bool }
#[async_trait(?Send)]
impl Http for Kinds {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        if r.url == "https://ec2.us-east-1.amazonaws.com/" && body.contains("Action=DescribeImages") && body.contains("windows25") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body));
            return Ok(Response::new(200, "text/xml", "<imagesSet><item><imageId>ami-windows</imageId><creationDate>2026-09-01T00:00:00.000Z</creationDate><blockDeviceMapping><item><ebs><volumeSize>100</volumeSize></ebs></item></blockDeviceMapping></item></imagesSet>"));
        }
        if self.no_gpus && body.contains("Action=RunInstances") && body.contains("InstanceType=g") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body));
            return Ok(Response::new(400, "text/xml", "<Response><Errors><Error><Code>MaxSpotInstanceCountExceeded</Code><Message>Max spot instance count exceeded</Message></Error></Errors></Response>"));
        }
        self.clouds.send(r).await
    }
}

/// The fake clouds, in an account whose AWS organization allows no spot machines: a spot launch is answered as a
/// project of AWS's new sign-up answered one (recorded there on 12 October 2026; the fixture has stand-in ids).
struct NoSpot { clouds: FakeClouds, refusal: String }
impl NoSpot {
    fn new() -> NoSpot {
        let recording = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/fixtures/aws/project-paid/more.jsonl")).expect("the project's recording");
        let refusal = recording.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).find(|x| x["asked"].as_str().is_some_and(|a| a.contains("Action=RunInstances") && a.contains("MarketType=spot")))
            .map(|x| x["answer"].as_str().unwrap().to_string()).expect("a refused spot launch in the recording");
        NoSpot { clouds: FakeClouds::default(), refusal }
    }
}
#[async_trait(?Send)]
impl Http for NoSpot {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        if body.contains("Action=RunInstances") && body.contains("MarketType=spot") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body));
            return Ok(Response::new(403, "text/xml", self.refusal.clone()));
        }
        self.clouds.send(r).await
    }
}

#[test]
fn how_a_job_itself_ended_is_kept_apart_from_whether_a_machine_ran_it() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let (seven, eight) = (format!("superci-{PLANE_ID}-7"), format!("superci-{PLANE_ID}-8"));
    // A job whose tests failed: a machine ran it to its end (done), and the job itself failed.
    block_on(plane.handle(webhook("queued", None)));
    block_on(plane.handle(webhook("in_progress", Some(&seven))));
    block_on(plane.handle(concluded(7, &seven, "failure")));
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7)["conclusion"].as_str()), (Some("done"), Some("failure")), "{}", job(&store, 7));
    // Two jobs that ran on each other's machines (GitHub gives a waiting job to any runner with its label): each
    // job's ending is its own, whichever machine it ran on.
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    block_on(plane.handle(job_event("queued", 9, "acme/app", None)));
    let nine = format!("superci-{PLANE_ID}-9");
    block_on(plane.handle(job_event("in_progress", 9, "acme/app", Some(&eight))));
    block_on(plane.handle(job_event("in_progress", 8, "acme/app", Some(&nine))));
    // (Each machine's record says which job it took, for the list to name its row by.)
    assert_eq!((job(&store, 8)["ran"].as_u64(), job(&store, 9)["ran"].as_u64(), job(&store, 7).get("ran")), (Some(9), Some(8), None));
    block_on(plane.handle(concluded(9, &eight, "success")));
    block_on(plane.handle(concluded(8, &nine, "failure")));
    assert_eq!((job(&store, 8)["conclusion"].as_str(), job(&store, 9)["conclusion"].as_str()), (Some("failure"), Some("success")), "{} {}", job(&store, 8), job(&store, 9));
    assert_eq!((job(&store, 8)["state"].as_str(), job(&store, 9)["state"].as_str()), (Some("done"), Some("done")));
}

/// The fake clouds, with the App renamed on GitHub since it was made: GitHub says its address as it is now.
struct Renamed { clouds: FakeClouds }
#[async_trait(?Send)]
impl Http for Renamed {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        if r.method == "GET" && r.url == "https://api.github.com/app" {
            return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 99, "slug": "acme-ci", "name": "Acme CI", "permissions": { "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" } }).to_string()));
        }
        self.clouds.send(r).await
    }
}

#[test]
fn an_app_renamed_on_github_is_known_by_its_new_address() {
    let (store, clouds, wakes, cache) = (Mem::default(), Renamed { clouds: FakeClouds::default() }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    assert_eq!(app().slug, "superci-acme-abc123");
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let status = json(&block_on(plane.handle(get("/status").with_header("authorization", "Bearer dashboard-session-key-1"))));
    // What the pages make their links to the App from is the address GitHub says now, not the one it was made with.
    assert_eq!((status["apps"][0]["slug"].as_str(), status["app"]["slug"].as_str(), status["apps"][0]["id"].as_u64()), (Some("acme-ci"), Some("acme-ci"), Some(99)));
}

/// The fake clouds, in a region that has none of the newest machines (as Stockholm had none of c8a in October 2026).
struct OlderRegion { clouds: FakeClouds }
#[async_trait(?Send)]
impl Http for OlderRegion {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        if body.contains("Action=DescribeInstanceTypeOfferings") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
            let item = |t: &str| format!("<item><instanceType>{t}</instanceType><locationType>region</locationType><location>us-east-1</location></item>");
            let have: String = ["m7a.xlarge", "c7a.xlarge"].iter().filter(|t| body.contains(&format!("={t}"))).map(|t| item(t)).collect();
            return Ok(Response::new(200, "text/xml", format!("<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet>{have}</instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>")));
        }
        self.clouds.send(r).await
    }
}

#[test]
fn a_machine_type_the_region_does_not_have_is_not_asked_for_with_every_job() {
    let (store, clouds, wakes, cache) = (Mem::default(), OlderRegion { clouds: FakeClouds::default() }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    assert_eq!(config.instance_types, ["c8a.xlarge", "m8a.xlarge", "c7a.xlarge", "m7a.xlarge", "c8i.xlarge", "m8i.xlarge", "c7i.xlarge", "m7i.xlarge"]);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    // Each job's machine is asked for as the first type the region has (in the order they are preferred), and the
    // region is asked what it has once, not with each job.
    assert_eq!(launches(&clouds.clouds).iter().map(|l| field(l, "InstanceType")).collect::<Vec<_>>(), ["c7a.xlarge", "c7a.xlarge"]);
    assert_eq!(clouds.clouds.calls.borrow().iter().filter(|(_, _, body)| body.contains("Action=DescribeInstanceTypeOfferings")).count(), 1);
    assert_eq!((job(&store, 7)["machine_type"].as_str(), job(&store, 8)["state"].as_str()), (Some("c7a.xlarge"), Some("launched")));
}

/// The fake clouds, in an account whose own budget in AWS reached its limit and put its rule on the control
/// plane's role: every launch is refused. The refusal is the recorded one above with what AWS says of a role's own
/// policy in the organization's place ("identity-based policy", and the rule's name when `named`); that the rule's
/// name is in what AWS says was seen live on 13 October 2026.
struct ByBudget { clouds: FakeClouds, refusal: String, on: std::cell::Cell<bool> }
impl ByBudget {
    fn new(named: bool) -> ByBudget {
        let org = NoSpot::new().refusal;
        let policy = org.split("service control policy: ").nth(1).and_then(|rest| rest.split(|c: char| c.is_whitespace() || c == '<').next()).expect("the policy AWS named").to_string();
        let refusal = org.replace(&format!("in a service control policy: {policy}"), &if named { format!("in an identity-based policy: arn:aws:iam::111111111111:policy/superci-stop-{PLANE_ID}") } else { "in an identity-based policy".to_string() });
        assert!(refusal.contains("with an explicit deny in an identity-based policy") && !refusal.contains("service control policy"), "{refusal}");
        ByBudget { clouds: FakeClouds::default(), refusal, on: std::cell::Cell::new(true) }
    }
}
#[async_trait(?Send)]
impl Http for ByBudget {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        // (The rule lets one kind of machine through: the small one that fails a refused job at GitHub with why.)
        if self.on.get() && body.contains("Action=RunInstances") && !superci_core::aws::SAYS_WHY_TYPES.iter().any(|t| body.contains(&format!("InstanceType={t}&"))) {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body));
            return Ok(Response::new(403, "text/xml", self.refusal.clone()));
        }
        self.clouds.send(r).await
    }
}

#[test]
fn a_machine_refused_by_the_accounts_own_budget_is_said_as_that_and_is_no_missing_permission() {
    for named in [true, false] {
        let (store, clouds, wakes, cache) = (Mem::default(), ByBudget::new(named), Wakes::default(), Cache::default());
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
        config.routing.aws_budget = Some(superci_core::plane::AwsBudget { stop: true });
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        let status = || json(&block_on(plane.handle(get("/status").with_header("authorization", "Bearer dashboard-session-key-1"))));
        block_on(plane.handle(webhook("queued", None)));
        // The job says what stopped it and what to do; nothing is noted as a permission the role lacks; the status
        // says when, for the dashboard's row.
        let j = job(&store, 7);
        let said = format!("{} {}", j["error"].as_str().unwrap_or_default(), j["why"].as_str().unwrap_or_default());
        assert!(said.contains("AWS's budget for this account reached its limit and stopped new machines there: raise the account's limit on Runners in the SuperCI dashboard (that lifts it), or wait for next month"), "named {named}: {j}");
        assert!(block_on(store.get("denied:aws:LaunchOnlyTaggedMachines")).unwrap().is_none(), "named {named}");
        assert!(status()["aws_budget_stopped_ms"].as_u64().is_some());
        // And it fails at GitHub with that, not waiting there for a runner that never comes: the rule lets the one
        // small machine through that takes the job and says why.
        let small = launches(&clouds.clouds).into_iter().filter(|l| field(l, "InstanceType") == "t3a.small").count();
        assert!(j["failed_fast"] == true && small == 1, "named {named}: {small} small machines; {j}");
        // Lifted: the next job's machine starts, and the note goes.
        clouds.on.set(false);
        block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
        assert_eq!(job(&store, 8)["state"], "launched", "{}", job(&store, 8));
        assert!(status()["aws_budget_stopped_ms"].is_null());
    }
    // With no such budget, and AWS not naming the rule, a refusal by the role's own policy is what it was before: a
    // permission the role lacks.
    let (store, clouds, wakes, cache) = (Mem::default(), ByBudget::new(false), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    assert!(block_on(store.get("denied:aws:LaunchOnlyTaggedMachines")).unwrap().is_some() && !job(&store, 7).to_string().contains("AWS's budget"), "{}", job(&store, 7));
}

#[test]
fn an_organization_that_allows_no_spot_sends_the_job_on_demand_and_is_no_missing_permission() {
    let (store, clouds, wakes, cache) = (Mem::default(), NoSpot::new(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    // One spot launch asked for and refused (no second region is tried: the organization's rule is the account's
    // everywhere), then an on-demand one, which starts.
    let asked = launches(&clouds.clouds);
    assert_eq!(asked.iter().map(|l| field(l, "InstanceMarketOptions.MarketType")).collect::<Vec<_>>(), ["spot", ""]);
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool(), j["machine_id"].as_str()), (Some("launched"), Some(true), Some("i-0abc")), "{j}");
    // The job says why it is on-demand, in a word the dashboard reads.
    assert!(j["spot_refused"].as_str().is_some_and(|s| s.contains("SpotNotAllowedByOrganization")), "{j}");
    // And no permission of the control plane's is noted as missing: its role lacks nothing.
    assert!(block_on(store.get("denied:aws:LaunchOnlyTaggedMachines")).unwrap().is_none());
    // The next job within ten minutes does not ask for a spot machine again: straight to on-demand, saying the same.
    block_on(plane.handle(labelled("queued", 8, "acme/app", None, "superci")));
    let asked = launches(&clouds.clouds);
    assert_eq!(asked.iter().map(|l| field(l, "InstanceMarketOptions.MarketType")).collect::<Vec<_>>(), ["spot", "", ""], "no second spot try");
    let j = job(&store, 8);
    assert!(j["state"] == "launched" && j["on_demand"] == true && j["spot_refused"].as_str().is_some_and(|s| s.contains("SpotNotAllowedByOrganization")), "{j}");
    // Once that refusal is more than ten minutes old, the organization is asked again (its owner may have changed the rule).
    let noted: u64 = block_on(store.get("aws:spot-not-allowed")).unwrap().unwrap().parse().unwrap();
    block_on(store.put("aws:spot-not-allowed", (noted - 600_001).to_string())).unwrap();
    block_on(plane.handle(labelled("queued", 9, "acme/app", None, "superci")));
    assert_eq!(launches(&clouds.clouds).iter().map(|l| field(l, "InstanceMarketOptions.MarketType")).collect::<Vec<_>>(), ["spot", "", "", "spot", ""], "asked again after ten minutes");
}

#[test]
fn gpus_and_windows_get_their_own_images_types_and_prices() {
    let (store, clouds, wakes, cache) = (Mem::default(), Kinds { clouds: FakeClouds::default(), no_gpus: false }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    // A GPU: the image with NVIDIA's drivers, T4 machines first.
    run(labelled("queued", 1, "acme/app", None, "superci/gpu=any"));
    assert_eq!(job(&store, 1)["state"], "launched");
    let l = &launches(&clouds.clouds)[0];
    assert_eq!(field(l, "InstanceType"), "g4dn.xlarge");
    assert!(clouds.clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeImages") && c.2.contains("ubuntu24-gpu-x64")));
    // Windows: Windows' image, its disk at least the image's, a PowerShell start-up, the runner's own work folder, and
    // Windows' spot prices.
    run(labelled("queued", 2, "acme/app", None, "superci/os=windows"));
    assert_eq!(job(&store, 2)["state"], "launched");
    let l = &launches(&clouds.clouds)[1];
    assert_eq!((field(l, "ImageId").as_str(), field(l, "BlockDeviceMapping.1.Ebs.VolumeSize").as_str()), ("ami-windows", "100"));
    let user_data = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(field(l, "UserData")).unwrap()).unwrap();
    assert!(user_data.starts_with("<powershell>") && user_data.contains(r".\run.cmd --jitconfig JITCONFIG+/=") && user_data.contains("shutdown.exe /s /f /t 21600"), "{user_data}");
    // Six hours unless set; as set, up to what each cloud lets a machine live (AWS: five days; Modal: a day;
    // Cloudflare: six hours).
    use superci_core::plane::job_minutes;
    assert_eq!((job_minutes(None, "aws"), job_minutes(None, "cloudflare"), job_minutes(None, "modal")), (360, 360, 360));
    assert_eq!((job_minutes(Some(60), "aws"), job_minutes(Some(60), "cloudflare")), (60, 60));
    assert_eq!((job_minutes(Some(48 * 60), "aws"), job_minutes(Some(48 * 60), "modal"), job_minutes(Some(48 * 60), "cloudflare")), (2880, 1440, 360));
    assert_eq!(job_minutes(Some(30 * 24 * 60), "aws"), 7200);
    let jit = clouds.clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/generate-jitconfig")).last().cloned().unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&jit.2).unwrap()["work_folder"], "_work");
    assert!(clouds.clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeSpotPriceHistory") && c.2.contains("ProductDescription.1=Windows")));
    assert_eq!(job(&store, 2)["disk_gb"], 100);
    // Which Ubuntu: as asked (26.04 here, before GitHub's `ubuntu-latest` moves), for x64, arm64 and a GPU alike.
    let images = |n: u64, label: &str| { run(labelled("queued", n, "acme/app", None, label)); assert_eq!(job(&store, n)["state"], "launched", "{label}");
        clouds.clouds.calls.borrow().iter().rev().find(|c| c.2.contains("Action=DescribeImages")).map(|c| c.2.clone()).unwrap() };
    assert!(images(3, "superci/image=ubuntu26").contains("ubuntu26-full-x64"));
    assert!(images(4, "superci/image=ubuntu26/arch=arm64").contains("ubuntu26-full-arm64"));
    assert!(images(5, "superci/image=ubuntu26/gpu=any").contains("ubuntu26-gpu-x64"));
    assert!(images(6, "superci").contains("ubuntu24-full-x64"), "unset: 24.04 until 2026-11-19");
}

#[test]
fn a_gpu_on_modal_goes_to_its_sandbox_and_into_the_price() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "modal".into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/gpu=any")));
    let launch = clouds.calls.borrow().iter().find(|c| c.1 == "https://runners.acme.workers.dev/launch").cloned().expect("agent launch");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&launch.2).unwrap()["gpu"], "T4", "any GPU: the least costly");
    let price = job(&store, 1)["usd_per_hour"].as_f64().unwrap();
    assert!((price - superci_core::plane::modal_usd_per_hour(2, 8) - 0.5904).abs() < 1e-6, "{price}");
}

/// Two Modal workspaces are two places in the order (`modal`, and `modal:<workspace>` for the one signed in to beside
/// it), each with its own launcher, limit and spend: at the first one's limit, jobs go to the second's launcher at
/// Modal's prices; at both, a job fails saying each. A label naming Modal may go to either.
#[test]
fn a_second_modal_workspace_takes_the_jobs_once_the_first_reaches_its_limit() {
    use superci_core::plane::{job_minutes, kind_of};
    assert_eq!((kind_of("modal:acme-two"), kind_of("modal"), kind_of("aws-on-demand"), kind_of("machine:mac")), ("modal", "modal", "aws-on-demand", "machine"));
    // (A sandbox lives a day at most, in any workspace.)
    assert_eq!((job_minutes(Some(30 * 60), "modal:acme-two"), job_minutes(Some(30 * 60), "aws")), (24 * 60, 30 * 60));
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://acme--superci-runners.modal.run".into() }, Launcher { cloud: "modal:acme-two".into(), url: "https://runners.acme.workers.dev".into() }];
    let pool = |cloud: &str, usd: f64| Pool { regions: vec![], off: false, cloud: cloud.into(), max_jobs: None, monthly_usd: Some(usd), function: false };
    config.routing.order = vec![pool("modal", 1.0), pool("modal:acme-two", 25.0)];
    // An hour at $2 in the first workspace this month: over its $1.
    store.0.borrow_mut().insert("job:50".into(), serde_json::json!({ "job_id": 50, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_996_400_000u64, "ended_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "modal", "usd_per_hour": 2.0, "seen_in_progress": true }).to_string());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 51, "acme/app", None)));
    let launched = |url: &str| clouds.calls.borrow().iter().filter(|c| c.1 == format!("{url}/launch")).count();
    assert_eq!((job(&store, 51)["state"].as_str(), job(&store, 51)["cloud"].as_str()), (Some("launched"), Some("modal:acme-two")), "{}", job(&store, 51));
    assert_eq!((launched("https://acme--superci-runners.modal.run"), launched("https://runners.acme.workers.dev")), (0, 1));
    let price = job(&store, 51)["usd_per_hour"].as_f64().unwrap();
    assert!((price - superci_core::plane::modal_usd_per_hour(2, 8)).abs() < 1e-6, "Modal's price: {price}");
    // A label naming Modal: either workspace, as the order has them.
    block_on(plane.handle(labelled("queued", 52, "acme/app", None, "superci/provider=modal")));
    assert_eq!(job(&store, 52)["cloud"], "modal:acme-two", "{}", job(&store, 52));
    // The second at its limit too: nowhere, and each says why.
    store.0.borrow_mut().insert("job:53".into(), serde_json::json!({ "job_id": 53, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_996_400_000u64, "ended_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "modal:acme-two", "usd_per_hour": 30.0, "seen_in_progress": true }).to_string());
    block_on(plane.handle(job_event("queued", 54, "acme/app", None)));
    let e = job(&store, 54)["error"].as_str().unwrap_or_default().to_string();
    assert_eq!(job(&store, 54)["state"], "failed", "{e}");
    assert!(e.contains("Modal reached its $1 a month") && e.contains("modal:acme-two reached its $25 a month (`superci runners update modal:acme-two --monthly-usd=N` raises it)"), "{e}");
}

#[test]
fn an_image_of_the_control_planes_own_goes_to_modals_sandbox_by_its_name_and_elsewhere_is_refused() {
    let images = [("rust-ci".to_string(), RunnerImage { reference: "ghcr.io/acme/ci@sha256:0f1e".into(), modal_secret: Some("ghcr-read".into()) }), ("node".to_string(), RunnerImage { reference: "docker.io/acme/node-ci:22".into(), modal_secret: None })];
    // (Modal's launcher says back the image it started from; one from before images of one's own says none.)
    let old = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| (r.url == "https://runners.acme.workers.dev/launch").then(|| {
        let image = if old.get() { serde_json::Value::Null } else { serde_json::from_str::<serde_json::Value>(body).unwrap()["image"].clone() };
        Response::new(200, "application/json", serde_json::json!({ "id": "sb-7", "kind": "sandbox", "image": image }).to_string())
    }));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.images = images.clone().into_iter().collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // Modal's launcher is told where the image is, and the Secret for its registry; a public one has none.
    let launched = |n: usize| serde_json::from_str::<serde_json::Value>(&clouds.0.calls.borrow().iter().filter(|c| c.1 == "https://runners.acme.workers.dev/launch").nth(n).expect("launch").2).unwrap();
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/image=rust-ci/cpu=8")));
    assert_eq!(job(&store, 1)["state"], "launched", "{}", job(&store, 1));
    assert_eq!((launched(0)["image"].as_str(), launched(0)["image_secret"].as_str(), launched(0)["cpu"].as_u64()), (Some("ghcr.io/acme/ci@sha256:0f1e"), Some("ghcr-read"), Some(8)));
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/image=node")));
    assert_eq!((launched(1)["image"].as_str(), launched(1).get("image_secret")), (Some("docker.io/acme/node-ci:22"), None));
    block_on(plane.handle(labelled("queued", 3, "acme/app", None, "superci")));
    assert_eq!(launched(2).get("image"), None, "GitHub's runner image, as before");
    // A name the control plane does not have: failed at once, saying how to add it, with no machine started.
    block_on(plane.handle(labelled("queued", 4, "acme/app", None, "superci/image=gone")));
    let j = job(&store, 4);
    assert_eq!((j["state"].as_str(), j["error"].as_str()), (Some("failed"), Some("image=gone is no image of this control plane's: give it one with `superci runner_images create gone --reference=REGISTRY/IMAGE:TAG`")));
    // (Its one machine is the runner that fails it on GitHub, saying why: GitHub's runner image.)
    assert_eq!((clouds.0.calls.borrow().iter().filter(|c| c.1.ends_with("/launch")).count(), launched(3)["fail"].is_string(), launched(3).get("image")), (4, true, None));
    // A launcher from before images of one's own: its sandbox (of GitHub's runner image) is stopped, and the job says to update it.
    old.set(true);
    block_on(plane.handle(labelled("queued", 9, "acme/app", None, "superci/image=node")));
    let j = job(&store, 9);
    assert!(j["error"].as_str().is_some_and(|e| e.contains("the Modal launcher is from before images of your own, and would start GitHub's runner image in place of docker.io/acme/node-ci:22: update it (`superci runners create modal`)")), "{j}");
    assert!(clouds.0.calls.borrow().iter().any(|c| c.1 == "https://runners.acme.workers.dev/stop" && c.2.contains("sb-7")), "its sandbox stopped");
    let clouds = FakeClouds::default();
    // A Modal control plane's own sandbox starts from it too.
    let (store, own) = (Mem::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.containers, config.own_cloud) = (true, "modal".into());
    config.routing.images = images.clone().into_iter().collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(labelled("queued", 5, "acme/app", None, "superci/image=rust-ci")));
    assert_eq!(job(&store, 5)["state"], "launched", "{}", job(&store, 5));
    assert!(own.0.borrow().iter().any(|c| c == "from ghcr.io/acme/ci@sha256:0f1e ghcr-read"), "{:?}", own.0.borrow());
    // AWS's machines and Cloudflare's containers start from no such image: said, not tried.
    let (store, own) = (Mem::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.containers, config.own_cloud) = (true, "cloudflare".into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.images = images.into_iter().collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(labelled("queued", 6, "acme/app", None, "superci/image=rust-ci")));
    let j = job(&store, 6);
    assert_eq!(j["state"], "failed", "{j}");
    let e = j["error"].as_str().unwrap();
    assert_eq!(e, "nowhere to run image rust-ci: AWS runs no images of your own (image=rust-ci: Modal only); AWS on-demand runs no images of your own (image=rust-ci: Modal only); Cloudflare runs no images of your own (image=rust-ci: Modal only)");
    // (Only the runner that fails it on GitHub was started.)
    assert!(own.0.borrow().iter().all(|c| c.contains(" fail ")) && !clouds.calls.borrow().iter().any(|c| c.2.contains("RunInstances")), "{:?}", own.0.borrow());
}

/// A further Modal workspace's launcher (`modal:<workspace>`) is told the image and its Secret as the first one's is
/// (the Secret is read in that workspace); one from before images of one's own says to make it again with `--account`.
#[test]
fn an_image_of_ones_own_goes_to_a_further_modal_workspaces_launcher_too() {
    let old = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| (r.url == "https://acme-two--superci-runners.modal.run/launch").then(|| {
        let image = if old.get() { serde_json::Value::Null } else { serde_json::from_str::<serde_json::Value>(body).unwrap()["image"].clone() };
        Response::new(200, "application/json", serde_json::json!({ "id": "sb-8", "kind": "sandbox", "image": image }).to_string())
    }));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal:acme-two".into(), url: "https://acme-two--superci-runners.modal.run".into() }];
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "modal:acme-two".into(), max_jobs: None, monthly_usd: None, function: false }];
    config.routing.images = [("rust-ci".to_string(), RunnerImage { reference: "ghcr.io/acme/ci:2".into(), modal_secret: Some("ghcr-read".into()) })].into_iter().collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/image=rust-ci")));
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["cloud"].as_str()), (Some("launched"), Some("modal:acme-two")), "{}", job(&store, 1));
    let launch = clouds.0.calls.borrow().iter().find(|c| c.1 == "https://acme-two--superci-runners.modal.run/launch").map(|c| serde_json::from_str::<serde_json::Value>(&c.2).unwrap()).expect("launch");
    assert_eq!((launch["image"].as_str(), launch["image_secret"].as_str()), (Some("ghcr.io/acme/ci:2"), Some("ghcr-read")));
    old.set(true);
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/image=rust-ci")));
    let j = job(&store, 2);
    assert!(j["error"].as_str().is_some_and(|e| e.contains("the modal:acme-two launcher is from before images of your own, and would start GitHub's runner image in place of ghcr.io/acme/ci:2: update it (`superci runners create modal --account=acme-two`)")), "{j}");
    assert!(clouds.0.calls.borrow().iter().any(|c| c.1 == "https://acme-two--superci-runners.modal.run/stop" && c.2.contains("sb-8")), "its sandbox stopped");
}

#[test]
fn an_aws_account_with_no_gpu_quota_fails_gpu_jobs_at_once_saying_what_to_raise() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), Kinds { clouds: FakeClouds::default(), no_gpus: true }, Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "cloudflare".into(), max_jobs: None, monthly_usd: None, function: false }, Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/gpu=l4")));
    let j = job(&store, 1);
    assert_eq!(j["state"], "failed", "not tried again: {j}");
    assert!(j["error"].as_str().unwrap().contains("L-3819A6DF"), "{}", j["error"]);
    // A failing runner tells GitHub, on Cloudflare (the cheapest place), with the same words.
    let fail = own.0.borrow().iter().find(|c| c.starts_with("start") && c.contains("fail")).cloned();
    assert!(fail.is_some(), "{:?}", own.0.borrow());
}

#[test]
fn the_label_asks_for_the_machine_and_the_order_finds_a_place_that_can_run_it() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "cloudflare".into(), max_jobs: None, monthly_usd: None, function: false }, Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));

    // The default machine fits Cloudflare, first in the order.
    run(labelled("queued", 1, "acme/app", None, "superci"));
    assert_eq!(job(&store, 1)["cloud"], "cloudflare");
    // 8 CPUs do not (2 at most there): AWS, a type with 8 CPUs and 32 GB; the runner registers under the job's own label.
    run(labelled("queued", 2, "acme/app", None, "superci/cpu=8"));
    assert_eq!(job(&store, 2)["cloud"], "aws");
    assert_eq!(field(&launches(&clouds)[0], "InstanceType"), "m8a.2xlarge");
    let jit = clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/generate-jitconfig")).last().cloned().unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&jit.2).unwrap()["labels"], serde_json::json!(["superci/cpu=8"]));
    // arm64, more memory per CPU, a bigger disk, on-demand: an arm64 image and type, no spot.
    run(labelled("queued", 3, "acme/app", None, "superci/cpu=2/ram=16/disk=200/arch=arm64/spot=false"));
    let l = &launches(&clouds)[1];
    assert_eq!((field(l, "InstanceType").as_str(), field(l, "InstanceMarketOptions.MarketType").as_str(), field(l, "BlockDeviceMapping.1.Ebs.VolumeSize").as_str()), ("r8g.large", "", "200"));
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeImages") && c.2.contains("arm64")), "the arm64 image");
    // Pinned to a cloud by the label.
    run(labelled("queued", 4, "acme/app", None, "superci/provider=aws"));
    assert_eq!(job(&store, 4)["cloud"], "aws");
    // Nothing here runs macOS; a part that is not understood: each fails with why, nothing started.
    run(labelled("queued", 5, "acme/app", None, "superci/os=macos"));
    assert_eq!(job(&store, 5)["state"], "failed");
    assert!(job(&store, 5)["error"].as_str().unwrap().starts_with("nowhere to run macOS"), "{}", job(&store, 5)["error"]);
    run(labelled("queued", 6, "acme/app", None, "superci/cores=8"));
    assert!(job(&store, 6)["error"].as_str().unwrap().contains("“cores” is not a key a label has"));
    assert_eq!(launches(&clouds).len(), 3);
}

#[test]
fn limits_make_jobs_wait_or_move_down_the_order() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "cloudflare".into(), max_jobs: Some(1), monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));

    run(job_event("queued", 1, "acme/app", None));
    run(job_event("queued", 2, "acme/app", None));
    run(job_event("queued", 3, "acme/app", None));
    assert_eq!(job(&store, 1)["state"], "launched");
    assert_eq!((job(&store, 2)["state"].as_str(), job(&store, 2)["error"].as_str()), (Some("waiting"), Some("waiting: Cloudflare runs 1 at once")));
    assert!(wakes.0.borrow().contains(&20_000), "it looks again soon");
    // Job 3 is cancelled while it waits: nothing to stop.
    run(job_event("completed", 3, "acme/app", None));
    assert_eq!(job(&store, 3)["state"], "cancelled");
    // Job 1 finishes; the next look starts job 2.
    run(job_event("completed", 1, "acme/app", Some(&format!("superci-{PLANE_ID}-1"))));
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 2)["state"], "launched");
    assert_eq!(job(&store, 3)["state"], "cancelled");

    // A monthly cap reached: the job moves down the order, here to AWS's on-demand machines (a place of their own,
    // with their own limits).
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: Some(1.0), function: false }];
    let store = Mem::default();
    store.0.borrow_mut().insert("job:50".into(), serde_json::json!({ "job_id": 50, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_996_400_000u64, "ended_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "aws", "usd_per_hour": 2.0, "seen_in_progress": true }).to_string());
    let cache = Cache::default();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 51, "acme/app", None)));
    assert_eq!((job(&store, 51)["state"].as_str(), job(&store, 51)["on_demand"].as_bool()), (Some("launched"), Some(true)), "{}", job(&store, 51));
    assert_eq!(field(launches(&clouds).last().unwrap(), "InstanceMarketOptions.MarketType"), "");
    // With on-demand turned off there is nothing below: it fails with why.
    config.routing.order.push(Pool { regions: vec![], off: true, cloud: "aws-on-demand".into(), max_jobs: None, monthly_usd: None, function: false });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 52, "acme/app", None)));
    let e = job(&store, 52)["error"].as_str().unwrap().to_string();
    assert_eq!(job(&store, 52)["state"], "failed");
    assert!(e.contains("AWS reached its $1 a month") && e.contains("AWS on-demand is turned off"), "{e}");

    // A cloud account's limit is for its providers together: AWS's spot and on-demand machines. Under it, a job
    // starts; at it, neither takes one (said once), and the job fails with why.
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.account_usd.insert("aws".into(), 3.0);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 53, "acme/app", None)));
    assert_eq!(job(&store, 53)["state"], "launched", "{}", job(&store, 53));
    // (Job 50 cost $2 on spot machines; one more dollar on on-demand ones makes the account's three.)
    store.0.borrow_mut().insert("job:54".into(), serde_json::json!({ "job_id": 54, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_996_400_000u64, "ended_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "aws", "on_demand": true, "usd_per_hour": 1.0, "seen_in_progress": true }).to_string());
    block_on(plane.handle(job_event("queued", 55, "acme/app", None)));
    let e = job(&store, 55)["error"].as_str().unwrap().to_string();
    assert_eq!(job(&store, 55)["state"], "failed", "{e}");
    assert!(e.contains("nowhere to run") && e.matches("the AWS account reached its $3 a month (`superci accounts update aws --monthly-usd=N` raises it)").count() == 1, "{e}");
    // The whole CI's limit: at it no job starts anywhere, and the job says what to do.
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.monthly_usd = Some(3.0);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 56, "acme/app", None)));
    let e = job(&store, 56)["error"].as_str().unwrap().to_string();
    assert_eq!(job(&store, 56)["state"], "failed", "{e}");
    assert!(e.contains("SuperCI reached its limit of $3 a month ($3.") && e.contains("no job starts until next month, or until the limit is raised"), "{e}");
    // A dollar more room: it starts.
    config.routing.monthly_usd = Some(40.0);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 57, "acme/app", None)));
    assert_eq!(job(&store, 57)["state"], "launched", "{}", job(&store, 57));
    // The same for a day (UTC): those three dollars were spent today. The whole CI's limit for a day stops every
    // job until tomorrow; the AWS account's sends jobs to other accounts' providers, and with none they fail.
    config.routing.daily_usd = Some(3.0);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 58, "acme/app", None)));
    let e = job(&store, 58)["error"].as_str().unwrap().to_string();
    assert!(job(&store, 58)["state"] == "failed" && e.contains("SuperCI reached its limit of $3 a day ($3.") && e.contains("no job starts until tomorrow (UTC)"), "{e}");
    (config.routing.daily_usd, config.routing.monthly_usd) = (Some(3.5), None);
    config.routing.account_daily_usd.insert("aws".into(), 2.5);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 59, "acme/app", None)));
    let e = job(&store, 59)["error"].as_str().unwrap().to_string();
    assert!(job(&store, 59)["state"] == "failed" && e.matches("the AWS account reached its $2.50 a day (`superci accounts update aws --daily-usd=N` raises it)").count() == 1, "{e}");
    // What was spent on an earlier day of the month counts for the month, not for today.
    store.0.borrow_mut().insert("job:60".into(), serde_json::json!({ "job_id": 60, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_900_000_000u64, "launched_ms": 1_789_900_000_000u64, "ended_ms": 1_789_903_600_000u64,
        "installation_id": 42, "cloud": "aws", "usd_per_hour": 50.0, "seen_in_progress": true }).to_string());
    config.routing.account_daily_usd.insert("aws".into(), 4.0);
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 61, "acme/app", None)));
    assert_eq!(job(&store, 61)["state"], "launched", "{}", job(&store, 61));
    let status = json(&block_on(plane.handle(get("/status").with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert!(status["daily_limits"] == true && status["spend_today"]["aws"].as_f64().is_some_and(|t| t < 4.0) && status["spend"]["aws"].as_f64().is_some_and(|m| m > 50.0), "{}", status["spend_today"]);
}

struct MovingClock(std::cell::Cell<u64>);
impl Clock for MovingClock { fn now_ms(&self) -> u64 { self.0.get() } }

fn gitlab_event(job: u64, status: &str, token: &str) -> Request { gitlab_event_on(job, status, token, None) }

fn gitlab_event_on(job: u64, status: &str, token: &str, runner: Option<u64>) -> Request {
    let mut body = serde_json::json!({ "object_kind": "build", "build_id": job, "build_name": "test", "build_status": status, "pipeline_id": 2366, "project_id": 380,
        "project": { "id": 380, "path_with_namespace": "acme/app" } });
    if let Some(id) = runner { body["runner"] = serde_json::json!({ "id": id, "description": "superci" }) }
    let body = body.to_string();
    Request::new("POST", &format!("{PLANE_URL}/gitlab/webhook")).with_header("x-gitlab-token", token).with_header("x-gitlab-event", "Job Hook").with_body(body)
}

#[test]
fn gitlab_jobs_with_the_label_in_their_tags_get_a_machine_and_a_project_runner_of_their_own() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.containers = true;
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/health")))["gitlab"], true);
    assert_eq!(run(gitlab_event(1977, "pending", "wrong")).status, 401, "only with the hook's secret");

    // A runner from before each job had its own (one per tag set, its token kept): removed by the next sweep.
    store.0.borrow_mut().insert("glrunner:380:docker,superci/cpu=2".into(), serde_json::json!({ "id": 9, "token": "glrt-old" }).to_string());
    // Pending, tagged superci/cpu=2 (and docker): a project runner for those tags, and a machine of that size.
    assert_eq!(run(gitlab_event(1977, "pending", "hook-secret-0123456789")).status, 200);
    let created = clouds.calls.borrow().iter().find(|c| c.1.ends_with("/user/runners")).cloned().unwrap();
    let form: Vec<(String, String)> = url::form_urlencoded::parse(created.2.as_bytes()).into_owned().collect();
    assert!(form.contains(&("tag_list".into(), "docker,superci/cpu=2".into())) && form.contains(&("project_id".into(), "380".into())) && form.contains(&("run_untagged".into(), "false".into())));
    assert_eq!(own.0.borrow().first().cloned(), Some(format!("start superci-{PLANE_ID}-gl1977 gitlab https://gitlab.example glrt-runner55 2cpu 8gb 20disk")));
    let job = |id: &str| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("job:gl{id}")]).unwrap();
    assert_eq!((job("1977")["state"].as_str(), job("1977")["provider"].as_str(), job("1977")["repo"].as_str()), (Some("launched"), Some("gitlab"), Some("acme/app")));
    // The same delivery again: nothing new.
    run(gitlab_event(1977, "pending", "hook-secret-0123456789"));
    assert_eq!(clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/user/runners")).count(), 1);
    // Another job with the same tags: a runner of its own (a token is never handed to a second machine), and no token kept.
    run(gitlab_event(1979, "pending", "hook-secret-0123456789"));
    assert_eq!(clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/user/runners")).count(), 2);
    assert_eq!((job("1977")["runner_id"].as_u64(), job("1979")["runner_id"].as_u64()), (Some(55), Some(56)));
    assert!(store.0.borrow().contains_key("glrunner:55") && !store.0.borrow()["glrunner:55"].contains("glrt-"));
    // A job without the label: not ours.
    run(gitlab_event(1978, "pending", "hook-secret-0123456789"));
    assert!(!store.0.borrow().contains_key("job:gl1978"));
    // It began on runner 56, the one made for job 1979 (GitLab gives a job to any runner with its tags): that runner
    // is paused, so it takes no other job, and removed when the job ends. The record that follows that machine is
    // 1979's (so the sweep does not take a busy machine for one that never began); 1977's own machine, idle, is kept
    // for 1979, with no new one started for 1977.
    run(gitlab_event_on(1977, "running", "hook-secret-0123456789", Some(56)));
    assert_eq!((job("1979")["state"].as_str(), job("1979")["seen_in_progress"].as_bool(), job("1977")["state"].as_str()), (Some("running"), Some(true), Some("launched")));
    assert!(clouds.calls.borrow().iter().any(|c| c.0 == "PUT" && c.1 == "https://gitlab.example/api/v4/runners/56" && c.2 == "paused=true"));
    run(gitlab_event_on(1977, "success", "hook-secret-0123456789", Some(56)));
    assert_eq!((job("1979")["state"].as_str(), job("1977")["state"].as_str(), job("1977")["over"].as_bool()), (Some("done"), Some("launched"), Some(true)));
    assert!(clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/56") && !store.0.borrow().contains_key("glrunner:56"));
    // Job 1979 then runs on 1977's machine (runner 55).
    run(gitlab_event_on(1979, "running", "hook-secret-0123456789", Some(55)));
    assert_eq!((job("1977")["state"].as_str(), job("1977")["seen_in_progress"].as_bool()), (Some("running"), Some(true)));
    // A runner not made here is left alone.
    run(gitlab_event_on(1979, "running", "hook-secret-0123456789", Some(4242)));
    assert!(!clouds.calls.borrow().iter().any(|c| c.1.ends_with("/runners/4242")));
    // A job cancelled before any runner took it: its machine, idle, is ended by the sweep, and its runner removed.
    run(gitlab_event(1980, "pending", "hook-secret-0123456789"));
    assert_eq!(job("1980")["runner_id"], 57);
    run(gitlab_event(1980, "canceled", "hook-secret-0123456789"));
    assert_eq!(job("1980")["state"], "orphan");
    // The sweep: the old per-tag runner goes; runner 55 (its machine's time not up) stays.
    block_on(plane.alarm()).unwrap();
    assert!(clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/9") && !store.0.borrow().contains_key("glrunner:380:docker,superci/cpu=2"));
    assert!(store.0.borrow().contains_key("glrunner:55"));
    assert_eq!(job("1980")["state"], "swept");
    assert!(own.0.borrow().iter().any(|c| c == &format!("stop superci-{PLANE_ID}-gl1980")) && clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/57"));
}

/// Containers that cannot start the first `fails` times (Cloudflare's "temporarily unavailable").
struct Flaky { fails: std::cell::Cell<u32>, starts: std::cell::Cell<u32> }
#[async_trait(?Send)]
impl Containers for Flaky {
    async fn start(&self, name: &str, _work: &superci_core::io::Work, _max: u32, _size: superci_core::spec::Size) -> io::Result<String> {
        faults::container(name)?;
        self.starts.set(self.starts.get() + 1);
        if self.fails.get() > 0 { self.fails.set(self.fails.get() - 1); return Err("The container connection is temporarily unavailable, try again".into()) }
        Ok(name.to_string())
    }
    async fn stop(&self, id: &str) -> io::Result<()> { faults::container(id) }
}

#[test]
fn a_failing_runner_that_would_not_start_is_started_by_the_next_sweep() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let flaky = Flaky { fails: std::cell::Cell::new(1), starts: std::cell::Cell::new(0) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&flaky) };
    // macOS: nowhere to run it; its failing runner's container does not start the first time.
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/os=macos")));
    let j = job(&store, 1);
    assert_eq!((j["state"].as_str(), j["failed_fast"].as_bool()), (Some("failed"), None));
    assert!(j["fail_pending"].as_str().unwrap().starts_with("nowhere to run"));
    assert!(clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1.contains("/actions/runners/")), "its registration withdrawn");
    // The sweep (GitHub still has it queued): started now.
    block_on(plane.alarm()).unwrap();
    let j = job(&store, 1);
    assert_eq!((j["failed_fast"].as_bool(), j.get("fail_pending")), (Some(true), None));
    assert_eq!(flaky.starts.get(), 2);
}

#[test]
fn a_machine_that_fails_to_start_is_tried_again_and_a_job_with_no_place_fails_at_once() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let flaky = Flaky { fails: std::cell::Cell::new(1), starts: std::cell::Cell::new(0) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&flaky) };
    block_on(plane.handle(job_event("queued", 1, "acme/app", None)));
    assert_eq!(job(&store, 1)["state"], "waiting");
    assert!(job(&store, 1)["error"].as_str().unwrap().starts_with("waiting: starting its machine failed, trying again"));
    assert!(wakes.0.borrow().contains(&20_000));
    // The next look starts it.
    block_on(plane.alarm()).unwrap();
    assert_eq!((job(&store, 1)["state"].as_str(), flaky.starts.get()), (Some("launched"), 2));
    // Nowhere to run it (8 CPU; Cloudflare has 4 at most): failed at once, not tried again.
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/cpu=8")));
    assert_eq!(job(&store, 2)["state"], "failed");
}

#[test]
fn public_repositories_run_only_when_allowed_and_never_from_forks() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    // Not allowed: refused, nothing started, and it says where to allow it.
    block_on(plane.handle(job_event("queued", 1, "acme/site-public", None)));
    assert_eq!(job(&store, 1)["state"], "failed");
    assert!(job(&store, 1)["error"].as_str().unwrap().contains("allow it in the dashboard"));
    assert!(own.0.borrow().is_empty());

    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    config.routing.public_repos = vec!["acme/site-public".into(), "acme/docs-public".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    // Allowed, from the repository itself: runs.
    block_on(plane.handle(job_event("queued", 2, "acme/site-public", None)));
    assert_eq!(job(&store, 2)["state"], "launched");
    // Allowed, but the run is a fork's pull request: refused.
    block_on(plane.handle(job_event("queued", 3, "acme/docs-public", None)));
    assert_eq!((job(&store, 3)["state"].as_str(), job(&store, 3)["error"].as_str()), (Some("failed"), Some("a pull request from a fork in a public repository: not run on your clouds")));
    // GitHub cannot say where a run came from: not run.
    block_on(plane.handle(job_event("queued", 4, "acme/other-public", None)));
    assert_eq!(job(&store, 4)["state"], "failed");
    assert_eq!(own.0.borrow().iter().filter(|c| c.starts_with("start")).count(), 1);
}

#[test]
fn machines_larger_than_allowed_are_refused_and_on_demand_counts_toward_budgets() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/cpu=64")));
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["error"].as_str()), (Some("failed"), Some("it asks for 64 CPUs; the most allowed is 32 (set in the dashboard)")));
    // It fails at once on GitHub: a small machine (AWS, the only place here) whose runner says why and fails the job.
    assert_eq!(job(&store, 1)["failed_fast"], true);
    let launch = clouds.calls.borrow().iter().find(|c| c.2.contains("Action=RunInstances") && c.2.contains("InstanceType=t3a.small")).cloned().expect("a failing runner");
    let user_data: String = url::form_urlencoded::parse(launch.2.as_bytes()).find(|(k, _)| k == "UserData").map(|(_, v)| v.to_string()).unwrap();
    let script = String::from_utf8(STANDARD.decode(user_data).unwrap()).unwrap();
    assert!(script.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh") && script.contains("( sleep 600; poweroff )"));
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/cpu=8/spot=false")));
    assert_eq!(job(&store, 2)["state"], "launched");
    let price = job(&store, 2)["usd_per_hour"].as_f64().unwrap();
    assert!(price > 0.4 && price < 0.6, "{price}");
    // The price list was refused (a role from before it was asked for): estimated from the size, and the refusal kept
    // for the dashboard, until it is given.
    let denied: serde_json::Value = serde_json::from_str(&store.0.borrow()["denied:aws:Prices"]).unwrap();
    assert!(denied["error"].as_str().unwrap().starts_with("AccessDeniedException"));
}

#[test]
fn a_moved_control_plane_forwards_only_signed_events() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    store.0.borrow_mut().insert("moved_to".into(), "\"https://new.example\"".into());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let forged = Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", "sha256=00").with_body("{}");
    assert_eq!(block_on(plane.handle(forged)).status, 401);
    assert!(clouds.calls.borrow().iter().all(|c| !c.1.starts_with("https://new.example")));
    block_on(plane.handle(webhook("queued", None)));
    assert!(clouds.calls.borrow().iter().any(|c| c.1 == "https://new.example/webhook"));
}

#[test]
fn measured_costs_from_the_dashboard_settle_finished_cloudflare_jobs_only() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(1_800_000_000_000, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |id: u64, cloud: &str, state: &str| store.0.borrow_mut().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 1, "repo": "acme/app", "state": state, "at_ms": 1_789_999_000_000u64,
        "ended_ms": 1_789_999_300_000u64, "launched_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": cloud, "seen_in_progress": true, "usd_per_hour": 0.4 }).to_string());
    put(1, "cloudflare", "done");
    put(2, "aws", "done");
    put(3, "cloudflare", "running");
    let post = |key: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/costs")).with_header("authorization", &format!("Bearer {key}"))
        .with_body(serde_json::json!({ "measured": { "job:1": 0.0123, "job:2": 9.0, "job:3": 1.0, "job:9": 1.0, "plane_url": 1.0 } }).to_string())));
    assert_eq!(post("not-the-key-not-the-key").status, 401);
    let r = post("dashboard-session-key-1");
    assert_eq!(json(&r)["kept"], 1);
    assert_eq!((job(&store, 1)["cost_usd"].as_f64(), job(&store, 1)["cost_from"].as_str()), (Some(0.0123), Some("measured")));
    assert!(job(&store, 2)["cost_usd"].is_null() && job(&store, 3)["cost_usd"].is_null(), "AWS settles its own; a running job is not settled");
    let j: superci_core::plane::Job = serde_json::from_value(job(&store, 1)).unwrap();
    assert_eq!(superci_core::plane::job_usd(&j, 1_790_000_000_000), 0.0123);
}

#[test]
fn jobs_nothing_here_can_run_fail_at_once() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-0123".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let fails = || own.0.borrow().iter().filter(|c| c.contains(" fail ")).cloned().collect::<Vec<_>>();
    // macOS (nothing here runs it): a small container takes it and fails it, saying why (base64 of a GitHub ::error::).
    run(labelled("queued", 1, "acme/app", None, "superci/os=macos"));
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["failed_fast"].as_bool()), (Some("failed"), Some(true)));
    let f = fails();
    assert!(f.len() == 1 && f[0].starts_with(&format!("start superci-{PLANE_ID}-1-x fail JITCONFIG+/= ")) && f[0].ends_with(" 1cpu 4gb 8disk"), "{f:?}");
    let line = String::from_utf8(STANDARD.decode(f[0].split(' ').nth(4).unwrap()).unwrap()).unwrap();
    assert_eq!(line, "::error title=SuperCI could not run this job::SuperCI runs no macOS jobs yet. Use runs-on: macos-latest for GitHub's own.");
    // Refused for its repository (a public one): no failing runner (it could take another repository's job).
    run(labelled("queued", 2, "acme/site-public", None, "superci/os=macos"));
    assert_eq!((job(&store, 2)["state"].as_str(), fails().len()), (Some("failed"), 1));
    // A label that says something no label has: the job is told at once, with the keys there are, and what to change is its runs-on alone.
    run(labelled("queued", 4, "acme/app", None, "superci/cores=8"));
    assert!(job(&store, 4)["error"].as_str().unwrap().starts_with("“cores” is not a key a label has. A label says a machine as key=value pairs after its name"), "{}", job(&store, 4)["error"]);
    let line = String::from_utf8(STANDARD.decode(fails().last().unwrap().split(' ').nth(4).unwrap()).unwrap()).unwrap();
    assert!(line.starts_with("::error title=SuperCI could not run this job::“cores” is not a key a label has.") && line.ends_with("(runs-on: superci/cores=8).") && !line.contains("add a place"), "{line}");
}


#[test]
fn removed_aws_runners_take_no_jobs() {
    // Connected by role (a control plane elsewhere): forgotten on the dashboard's word, then no job goes to AWS.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": "arn:aws:iam::123456789012:role/r", "connected": true }).to_string());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let forget = |key: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/aws/forget")).with_header("authorization", &format!("Bearer {key}")).with_body("{}")));
    assert_eq!(forget("wrong-key-wrong-key").status, 401, "the dashboard's only");
    assert_eq!(forget("dashboard-session-key-1").status, 200);
    assert!(!store.0.borrow().contains_key("aws"));
    block_on(plane.handle(webhook("queued", None)));
    assert_eq!(job(&store, 7)["state"], "failed");
    assert!(launches(&clouds).is_empty());
    // A control plane in AWS with its AWS runners off: the same.
    let (store, clouds, cache) = (Mem::default(), FakeClouds::default(), Cache::default());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_runners_off = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    assert_eq!(json(&block_on(plane.handle(get("/health"))))["aws"], false);
    block_on(plane.handle(webhook("queued", None)));
    assert!(launches(&clouds).is_empty() && job(&store, 7)["state"] == "failed");
}

/// The fake clouds, where AWS prices GPU machines by zone in Virginia (b cheaper, but full), and Ohio allows no GPUs.
struct Zones(FakeClouds);
#[async_trait(?Send)]
impl Http for Zones {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        let xml = |s: &str| Ok(Response::new(200, "text/xml", s.to_string()));
        let err = |code: &str| Ok(Response::new(400, "text/xml", format!("<Response><Errors><Error><Code>{code}</Code><Message>no</Message></Error></Errors></Response>")));
        let ours = body.contains("InstanceType=g") || body.contains("InstanceType.1=g") || body.contains("Action=DescribeSpotPriceHistory");
        if ours && (r.url == "https://ec2.us-east-1.amazonaws.com/" || r.url == "https://ec2.us-east-2.amazonaws.com/") {
            self.0.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
            if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") {
                return xml("<spotPriceHistorySet><item><availabilityZone>us-east-1a</availabilityZone><spotPrice>0.30</spotPrice></item><item><availabilityZone>us-east-1b</availabilityZone><spotPrice>0.20</spotPrice></item></spotPriceHistorySet>");
            }
            if body.contains("Action=RunInstances") {
                if r.url.contains("us-east-2") { return err("MaxSpotInstanceCountExceeded") }
                if body.contains("AvailabilityZone=us-east-1b") || self.0.calls.borrow().iter().any(|c| c.2.contains("full-everywhere")) { return err("InsufficientInstanceCapacity") }
                return xml("<RunInstancesResponse><instancesSet><item><instanceId>i-0gpu</instanceId><placement><availabilityZone>us-east-1a</availabilityZone></placement></item></instancesSet></RunInstancesResponse>");
            }
            self.0.calls.borrow_mut().pop();
        }
        self.0.send(r).await
    }
}

#[test]
fn without_its_own_network_a_machine_still_tries_each_zone_and_a_full_region_is_not_called_a_quota() {
    let (store, clouds, wakes, cache) = (Mem::default(), Zones(FakeClouds::default()), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // Zone b first (cheaper), full for every type; then zone a.
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/gpu=any")));
    let tried = launches(&clouds.0);
    assert_eq!((tried.first().map(|f| field(f, "Placement.AvailabilityZone").to_string()), tried.last().map(|f| field(f, "Placement.AvailabilityZone").to_string())), (Some("us-east-1b".to_string()), Some("us-east-1a".to_string())));
    assert!(tried.iter().take_while(|f| field(f, "Placement.AvailabilityZone") == "us-east-1b").count() >= 1 && field(&tried[0], "InstanceType") == "g4dn.xlarge");
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["zone"].as_str()), (Some("launched"), Some("us-east-1a")));
    // Virginia full in every zone, Ohio allowing none: said as it is, not as a quota of 0.
    clouds.0.calls.borrow_mut().push(("".into(), "".into(), "full-everywhere".into()));
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/gpu=any")));
    let e = job(&store, 2)["error"].as_str().unwrap_or_default().to_string();
    assert!(e.contains("us-east-1: InsufficientInstanceCapacity") && e.contains("us-east-2: MaxSpotInstanceCountExceeded") && !e.contains("Service Quotas"), "{e}");
}

/// The fake clouds, with GitHub saying how run 3 stands (`done`) and taking requests to run a job again.
struct Runs { clouds: FakeClouds, done: std::cell::Cell<bool>, may_rerun: bool }
#[async_trait(?Send)]
impl Http for Runs {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        if r.url == "https://api.github.com/repos/acme/app/actions/runs/3" {
            return Ok(Response::new(200, "application/json", serde_json::json!({ "status": if self.done.get() { "completed" } else { "in_progress" } }).to_string()));
        }
        if r.url.ends_with("/rerun") || r.url.ends_with("/rerun-failed-jobs") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), String::new()));
            return Ok(if self.may_rerun { Response::new(201, "application/json", "{}") } else { Response::new(403, "application/json", r#"{"message":"Resource not accessible by integration"}"#) });
        }
        self.clouds.send(r).await
    }
}

#[test]
fn a_spot_machine_taken_back_fails_its_job_at_once_and_the_job_is_run_again_on_demand() {
    let (store, clouds, wakes, cache) = (Mem::default(), Runs { clouds: FakeClouds::default(), done: std::cell::Cell::new(false), may_rerun: true }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let notice = |job: u64, t: &str| run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner=superci-{PLANE_ID}-{job}&t={t}"))).status;
    let user_data = |n: usize| String::from_utf8(STANDARD.decode(field(&launches(&clouds.clouds)[n], "UserData")).unwrap()).unwrap();

    // A spot machine watches for AWS's notice and has a link of its own to say so.
    run(webhook("queued", None));
    let token = job(&store, 7)["notice"].as_str().unwrap().to_string();
    let script = user_data(0);
    assert!(script.contains("meta-data/spot/instance-action") && script.contains(&format!("{PLANE_URL}/interrupted?runner=superci-{PLANE_ID}-7&t={token}")) && script.contains("pkill -INT -f Runner.Listener"));
    assert!(script.find("spot/instance-action").unwrap() < script.find("./run.sh").unwrap(), "watching before the runner starts");
    // Only with its link.
    assert_eq!((notice(7, "wrong-wrong-wrong-wrong"), notice(8, &token)), (404, 404));
    assert_eq!(job(&store, 7).get("interrupted"), None);
    let runner = format!("superci-{PLANE_ID}-7");
    run(webhook("in_progress", Some(&runner)));
    assert_eq!(notice(7, &token), 200);
    assert_eq!(job(&store, 7)["interrupted"], true);
    // What the machine runs with the answer: the reason, as a GitHub error in the output of the step running now.
    let said = String::from_utf8(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner={runner}&t={token}"))).body).unwrap();
    assert!(said.contains("pgrep -f Runner.Worker") && said.contains("> /proc/$p/fd/1") && said.contains("::error title=SuperCI::AWS is taking this spot machine back (two minutes’ notice). SuperCI runs the job again by itself, off spot machines, once its run has finished."), "{said}");
    // Its job fails (the runner was stopped): to be run again once its run has finished.
    run(webhook("completed", Some(&runner)));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["rerun"]["state"].as_str(), j["rerun"]["job_id"].as_u64()), (Some("failed"), Some("pending"), Some(7)));
    assert!(j["error"].as_str().unwrap().starts_with("AWS took back its spot machine"));
    // The run still going: nothing asked yet. Finished: GitHub is asked to run that job again.
    block_on(plane.alarm()).unwrap();
    assert!(!clouds.clouds.calls.borrow().iter().any(|c| c.1.ends_with("/rerun")));
    clouds.done.set(true);
    block_on(plane.alarm()).unwrap();
    assert!(clouds.clouds.calls.borrow().iter().any(|c| c.0 == "POST" && c.1 == "https://api.github.com/repos/acme/app/actions/jobs/7/rerun"));
    assert_eq!(job(&store, 7)["rerun"]["state"], "asked");
    // The new attempt (a new job of the same run and name): an on-demand machine, with nothing to watch for.
    run(job_event("queued", 8, "acme/app", None));
    let l = &launches(&clouds.clouds)[1];
    assert_eq!(field(l, "InstanceMarketOptions.MarketType"), "");
    assert_eq!((job(&store, 8)["on_demand"].as_bool(), job(&store, 8).get("notice")), (Some(true), None));
    assert!(!user_data(1).contains("spot/instance-action"));
    // A later job of that run is on spot again (the note was for one job).
    run(job_event("queued", 9, "acme/app", None));
    assert_eq!(field(&launches(&clouds.clouds)[2], "InstanceMarketOptions.MarketType"), "spot");
    // A second machine taken back within a quarter of an hour: spot is paused, the next job starts on-demand.
    let token = job(&store, 9)["notice"].as_str().unwrap().to_string();
    assert_eq!(notice(9, &token), 200);
    run(job_event("queued", 10, "acme/app", None));
    assert_eq!(field(&launches(&clouds.clouds)[3], "InstanceMarketOptions.MarketType"), "");
    assert_eq!(job(&store, 10)["on_demand"], true);
}

/// Modal in Function mode (`Pool::function`): a job's runner is a preemptible Modal Function, at Function prices, with
/// a link of its own to say Modal preempted it. Then its job fails on GitHub (its runner gone), is run again once its
/// run has finished, on a sandbox (so once at most), and the month's count says both. Other jobs stay on Functions,
/// and AWS's spot machines are not paused for it.
#[test]
fn a_preempted_modal_function_is_run_again_once_on_a_sandbox_and_counted() {
    use superci_core::plane::{modal_function_usd_per_hour, modal_usd_per_hour};
    let (store, clouds, wakes, cache, own) = (Mem::default(), Runs { clouds: FakeClouds::default(), done: std::cell::Cell::new(false), may_rerun: true }, Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.containers, config.own_cloud) = (true, "modal".into());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    config.routing.order = vec![Pool { function: true, ..Pool::of("modal") }];
    config.routing.images = [("rust-ci".to_string(), RunnerImage { reference: "ghcr.io/acme/ci:1".into(), modal_secret: Some("ghcr-read".into()) })].into_iter().collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let status = || json(&run(get("/status").with_header("authorization", "Bearer dashboard-session-key-1")));
    let runner = format!("superci-{PLANE_ID}-7");

    // A Function, not a sandbox, priced as one, with the link it says a preemption with.
    run(labelled("queued", 7, "acme/app", None, "superci/cpu=8/ram=16"));
    let token = job(&store, 7)["notice"].as_str().unwrap().to_string();
    assert_eq!(own.0.borrow().first().cloned(), Some(format!("function {runner} JITCONFIG+/= 8cpu 16gb github {PLANE_URL}/interrupted?runner={runner}&t={token}")));
    let j = job(&store, 7);
    assert_eq!((j["machine_id"].as_str(), j["machine_type"].as_str()), (Some(format!("fc-{runner}").as_str()), Some("8cpu-16gb-function")));
    assert!((j["usd_per_hour"].as_f64().unwrap() - modal_function_usd_per_hour(8, 16)).abs() < 1e-9 && modal_function_usd_per_hour(8, 16) * 2.9 < modal_usd_per_hour(8, 16), "{j}");
    // An image of the control plane's own: its Function, by its address and Secret.
    run(labelled("queued", 6, "acme/app", None, "superci/image=rust-ci"));
    assert!(own.0.borrow().last().unwrap().contains(" ghcr.io/acme/ci:1|ghcr-read "), "{:?}", own.0.borrow());

    // Modal started the Function again on the same input: it says so with its link (only with it), runs no runner.
    run(webhook("in_progress", Some(&runner)));
    let notice = |t: &str| run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner={runner}&t={t}")));
    assert_eq!(notice("wrong-wrong-wrong-wrong").status, 404);
    let said = notice(&token);
    assert_eq!((said.status, said.body.is_empty()), (200, true), "no AWS script for a Function");
    assert_eq!(job(&store, 7)["interrupted"], true);
    assert_eq!(notice(&token).status, 200, "said twice: counted once");
    let keys = |prefix: &str| store.0.borrow().keys().filter(|k| k.starts_with(prefix)).count();
    assert_eq!((keys("modal:preempted:"), keys("spot:interrupted:")), (1, 0));
    assert_eq!(status()["preempted"], serde_json::json!({ "month": 1, "rerun": 0 }));

    // Its job fails on GitHub (its runner is gone): run again once its run has finished.
    run(webhook("completed", Some(&runner)));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["rerun"]["state"].as_str()), (Some("failed"), Some("pending")));
    assert_eq!(j["error"], "Modal preempted its Function; SuperCI runs the job again by itself, on a sandbox, once its run has finished (GitHub runs a job again only then)");
    block_on(plane.alarm()).unwrap();
    assert!(!clouds.clouds.calls.borrow().iter().any(|c| c.1.ends_with("/rerun")));
    clouds.done.set(true);
    block_on(plane.alarm()).unwrap();
    assert_eq!(clouds.clouds.calls.borrow().iter().filter(|c| c.0 == "POST" && c.1 == "https://api.github.com/repos/acme/app/actions/jobs/7/rerun").count(), 1);
    assert_eq!((job(&store, 7)["rerun"]["state"].as_str(), job(&store, 7)["error"].as_str()), (Some("asked"), Some("Modal preempted its Function; SuperCI ran the job again by itself, on a sandbox (the run's next attempt)")));
    block_on(plane.alarm()).unwrap();
    assert_eq!(status()["preempted"], serde_json::json!({ "month": 1, "rerun": 1 }), "asked once");

    // The new attempt: a sandbox (not preempted), at a sandbox's price, with no link. A later job: a Function again.
    run(labelled("queued", 8, "acme/app", None, "superci/cpu=8/ram=16"));
    assert_eq!(own.0.borrow().iter().filter(|c| c.starts_with(&format!("start superci-{PLANE_ID}-8 "))).count(), 1, "{:?}", own.0.borrow());
    let j = job(&store, 8);
    assert_eq!((j.get("notice"), j["machine_type"].as_str(), j["after_reclaim"].as_bool()), (None, Some("8cpu-16gb"), Some(true)));
    assert!((j["usd_per_hour"].as_f64().unwrap() - modal_usd_per_hour(8, 16)).abs() < 1e-9);
    run(labelled("queued", 9, "acme/app", None, "superci/cpu=8/ram=16"));
    assert!(own.0.borrow().last().unwrap().starts_with(&format!("function superci-{PLANE_ID}-9 ")));
    // Stopped by its call's id.
    run(labelled("completed", 9, "acme/app", Some(&format!("superci-{PLANE_ID}-9")), "superci/cpu=8/ram=16"));
    assert_eq!(own.0.borrow().last().cloned(), Some(format!("stop fc-superci-{PLANE_ID}-9")));
}

/// A further Modal workspace's launcher in Function mode is told so, with the link; one from before Function mode
/// (it says back a sandbox) has it stopped, and the job says to update it.
#[test]
fn a_modal_launcher_in_function_mode_starts_a_function_or_says_to_update_it() {
    let old = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _body: &str| (r.url == "https://runners.acme.workers.dev/launch").then(|| {
        Response::new(200, "application/json", serde_json::json!({ "id": if old.get() { "sb-7" } else { "fc-7" }, "kind": if old.get() { "sandbox" } else { "function" } }).to_string())
    }));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal:acme-two".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = vec![Pool { function: true, ..Pool::of("modal:acme-two") }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let calls = |url: &str| clouds.0.calls.borrow().iter().filter(|c| c.1 == url).map(|c| serde_json::from_str::<serde_json::Value>(&c.2).unwrap_or_default()).collect::<Vec<_>>();
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/cpu=4")));
    let token = job(&store, 1)["notice"].as_str().unwrap().to_string();
    let launch = &calls("https://runners.acme.workers.dev/launch")[0];
    assert_eq!((launch["function"].as_bool(), launch["notice"].as_str()), (Some(true), Some(format!("{PLANE_URL}/interrupted?runner=superci-{PLANE_ID}-1&t={token}").as_str())));
    assert_eq!((job(&store, 1)["machine_id"].as_str(), job(&store, 1)["machine_type"].as_str()), (Some("fc-7"), Some("function")));
    old.set(true);
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci/cpu=4")));
    assert_eq!(calls("https://runners.acme.workers.dev/stop"), vec![serde_json::json!({ "id": "sb-7" })]);
    let e = job(&store, 2)["error"].as_str().unwrap_or_default().to_string();
    assert!(e.contains("the modal:acme-two launcher is from before Function mode, and would start a sandbox: update it (`superci runners create modal --account=acme-two`)"), "{e}");
}

#[test]
fn an_app_that_may_not_run_jobs_again_says_so() {
    let (store, clouds, wakes, cache) = (Mem::default(), Runs { clouds: FakeClouds::default(), done: std::cell::Cell::new(true), may_rerun: false }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    run(webhook("queued", None));
    let token = job(&store, 7)["notice"].as_str().unwrap().to_string();
    let runner = format!("superci-{PLANE_ID}-7");
    run(webhook("in_progress", Some(&runner)));
    run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner={runner}&t={token}")));
    run(webhook("completed", Some(&runner)));
    block_on(plane.alarm()).unwrap();
    let j = job(&store, 7);
    assert!(j["rerun"]["state"].as_str().unwrap().contains("Actions: write") && j["error"].as_str().unwrap().contains("not run again"), "{j}");
    // Nothing left noted for a new attempt.
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("rerun:")));
}

#[test]
fn cloudflare_runners_get_githubs_full_image_by_its_address_when_set() {
    use superci_core::plane::image_address;
    assert_eq!(image_address(" https://images.example.com/ubuntu-24.04/ "), Some("https://images.example.com/ubuntu-24.04".into()));
    for bad in ["http://images.example.com/x", "https://x.com/a b", "https://x.com/$(reboot)", "https://x.com/a'b", "none", ""] { assert_eq!(image_address(bad), None, "{bad}") }
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    config.cloudflare_image = image_address("https://images.example.com/ubuntu-24.04");
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let launch = clouds.calls.borrow().iter().find(|c| c.1 == "https://runners.acme.workers.dev/launch").cloned().expect("agent launch");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&launch.2).unwrap()["image_url"], "https://images.example.com/ubuntu-24.04");
    assert_eq!(json(&block_on(plane.handle(get("/health"))))["plane"], PLANE_ID);
    // The start script: inside the image when one is set (and not for a runner that only fails a job), else as before.
    let start = superci_core::docker::start();
    assert!(start.contains(r#"[ -n "$SUPERCI_IMAGE" ] && [ -z "$SUPERCI_FAIL" ] && full_image"#) && start.contains("reader-x86_64") && start.contains("mount -t overlay"));
    assert!(start.find("full_image()").unwrap() < start.find(r#"exec /home/runner/run.sh --jitconfig "$SUPERCI_JIT""#).unwrap(), "the plain start stays, after it");
    // What the address pins is checked before anything of it runs: the index against the address, the reader
    // program against the index. An image that cannot be had fails the job saying so, not a job in the small image.
    let at = |what: &str| start.find(what).unwrap_or_else(|| panic!("{what}"));
    assert!(at("its index is not the one the address names") < at("reader-x86_64") && at("its reader program is not the one its index names") < at("nohup /tmp/image-reader"));
    assert!(at("SUPERCI_FAIL=$(printf") < at(r#"if [ -n "$SUPERCI_FAIL" ]; then"#) && start.contains("umount -R -l $root"));
    // The image's environment is read as names and values, never run.
    assert!(!start.contains(". /etc/environment") && start.contains("done < /etc/environment"));
}

#[test]
fn one_control_plane_serves_several_organizations_each_through_its_own_app() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    let mut beta = app();
    (beta.id, beta.slug, beta.owner, beta.webhook_secret) = (100, "superci-beta-abc123".into(), "beta".into(), "whsec-beta".into());
    config.more_apps = vec![beta];
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let event = |job: u64, repo: &str, installation: u64, secret: &[u8], action: &str, runner: Option<&str>| {
        let payload = serde_json::json!({ "action": action, "workflow_job": { "id": job, "run_id": 3, "labels": ["superci"], "runner_name": runner }, "repository": { "full_name": repo, "private": true }, "installation": { "id": installation } }).to_string();
        let sig = format!("sha256={}", hex(&hmac_sha256(secret, payload.as_bytes())));
        Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload)
    };
    // The second organization's job, signed by its own App: registered there, with its own installation's token.
    assert_eq!(block_on(plane.handle(event(21, "beta/api", 43, b"whsec-beta", "queued", None))).status, 202);
    let j = job(&store, 21);
    assert_eq!((j["state"].as_str(), j["app_id"].as_u64(), j["runner_id"].as_u64()), (Some("launched"), Some(100), Some(6)));
    assert!(own.0.borrow().iter().any(|c| c.contains("JITBETA")), "{:?}", own.0.borrow());
    // The first organization's, as before.
    block_on(plane.handle(event(22, "acme/app", 42, b"whsec", "queued", None)));
    assert_eq!((job(&store, 22)["app_id"].as_u64(), job(&store, 22)["runner_id"].as_u64()), (Some(99), Some(5)));
    // An App's signature only covers its own organization's repositories; no App's signature: refused.
    assert_eq!(block_on(plane.handle(event(23, "acme/app", 42, b"whsec-beta", "queued", None))).status, 202);
    assert!(!store.0.borrow().contains_key("job:23"), "another organization's App does not vouch for this one's jobs");
    assert_eq!(block_on(plane.handle(event(24, "beta/api", 43, b"wrong", "queued", None))).status, 401);
    // Its runner is withdrawn with its own App too (a job that ends: its machine stopped, nothing left).
    block_on(plane.handle(event(21, "beta/api", 43, b"whsec-beta", "completed", Some(&format!("superci-{PLANE_ID}-21")))));
    assert_eq!(job(&store, 21)["state"], "done");
}

/// The fake clouds, with some answers of its own first (such a call is recorded like the others).
struct With<F: Fn(&Request, &str) -> Option<Response>>(FakeClouds, F);
#[async_trait(?Send)]
impl<F: Fn(&Request, &str) -> Option<Response>> Http for With<F> {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let body = String::from_utf8_lossy(&r.body).to_string();
        if let Some(answer) = (self.1)(&r, &body) { self.0.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body)); return Ok(answer) }
        self.0.send(r).await
    }
}

fn aws_refuses(code: &str) -> Response { Response::new(400, "text/xml", format!("<Response><Errors><Error><Code>{code}</Code><Message>no</Message></Error></Errors></Response>")) }

/// A job's end as GitHub says it, with how it ended.
fn concluded(job: u64, runner: &str, conclusion: &str) -> Request {
    let payload = serde_json::json!({ "action": "completed", "workflow_job": { "id": job, "run_id": 3, "labels": ["superci"], "runner_name": runner, "conclusion": conclusion }, "repository": { "full_name": "acme/app", "private": true }, "installation": { "id": 42 } }).to_string();
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
    Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload)
}

fn user_data_of(launch: &[(String, String)]) -> String { String::from_utf8(STANDARD.decode(field(launch, "UserData")).unwrap()).unwrap() }

#[test]
fn with_no_room_for_a_spot_machine_in_any_region_the_job_gets_an_on_demand_one() {
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    // Spot in each region first (each type, each refused), then on-demand in the first region.
    let tried = launches(&clouds.0);
    let (spot, on_demand): (Vec<_>, Vec<_>) = tried.iter().partition(|l| field(l, "InstanceMarketOptions.MarketType") == "spot");
    assert!(spot.len() >= 2 && on_demand.len() == 1 && field(tried.last().unwrap(), "InstanceMarketOptions.MarketType").is_empty());
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool(), j["region"].as_str(), j.get("notice")), (Some("launched"), Some(true), Some("us-east-1"), None));
    assert_eq!(j["spot_refused"], "us-east-1: InsufficientInstanceCapacity; us-east-2: InsufficientInstanceCapacity");
    // An on-demand machine has nothing to watch for.
    assert!(user_data_of(spot[0]).contains("spot/instance-action") && !user_data_of(on_demand[0]).contains("spot/instance-action"));
}

#[test]
fn a_job_that_passed_as_the_notice_came_is_not_run_again_and_a_notice_follows_the_machine_to_the_job_it_took() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    let notice = |runner: &str, t: &str| run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner={runner}&t={t}"))).status;
    // AWS's notice as the job ended, and the job passed: done, nothing to run again.
    run(webhook("queued", None));
    run(webhook("in_progress", Some(&runner(7))));
    assert_eq!(notice(&runner(7), job(&store, 7)["notice"].as_str().unwrap()), 200);
    run(concluded(7, &runner(7), "success"));
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7).get("rerun")), (Some("done"), None));
    // Its runner's name is forgotten with it: the link no longer works.
    assert_eq!(notice(&runner(7), job(&store, 7)["notice"].as_str().unwrap()), 404);

    // Job 8's machine failed; job 9's runner takes job 8. The machine's notice is job 8's now.
    run(job_event("queued", 8, "acme/app", None));
    run(job_event("queued", 9, "acme/app", None));
    let mut eight = job(&store, 8);
    eight["state"] = "failed".into();
    store.0.borrow_mut().insert("job:8".into(), eight.to_string());
    let token = job(&store, 9)["notice"].as_str().unwrap().to_string();
    run(job_event("in_progress", 8, "acme/app", Some(&runner(9))));
    assert_eq!((job(&store, 8)["notice"].as_str(), job(&store, 8)["zone"].as_str(), job(&store, 8)["launched_ms"].as_u64()), (Some(token.as_str()), Some("us-east-1b"), Some(1_790_000_000_000)));
    assert_ne!(job(&store, 9)["notice"].as_str(), Some(token.as_str()), "job 9's new machine has a link of its own");
    assert_eq!(notice(&runner(9), &token), 200);
    assert_eq!((job(&store, 8)["interrupted"].as_bool(), job(&store, 9).get("interrupted")), (Some(true), None));
    run(concluded(8, &runner(9), "failure"));
    assert_eq!((job(&store, 8)["rerun"]["job_id"].as_u64(), job(&store, 8)["rerun"]["state"].as_str()), (Some(8), Some("pending")));
}

#[test]
fn several_jobs_of_one_run_taken_back_are_run_again_with_one_request() {
    let (store, clouds, wakes, cache) = (Mem::default(), Runs { clouds: FakeClouds::default(), done: std::cell::Cell::new(true), may_rerun: true }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    for id in [7, 8] {
        let runner = format!("superci-{PLANE_ID}-{id}");
        run(job_event("queued", id, "acme/app", None));
        run(job_event("in_progress", id, "acme/app", Some(&runner)));
        let token = job(&store, id)["notice"].as_str().unwrap().to_string();
        assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?runner={runner}&t={token}"))).status, 200);
        run(concluded(id, &runner, "failure"));
    }
    block_on(plane.alarm()).unwrap();
    let asked: Vec<String> = clouds.clouds.calls.borrow().iter().filter(|c| c.1.contains("rerun")).map(|c| c.1.clone()).collect();
    assert_eq!(asked, ["https://api.github.com/repos/acme/app/actions/runs/3/rerun-failed-jobs"], "GitHub takes one request per attempt");
    assert_eq!((job(&store, 7)["rerun"]["state"].as_str(), job(&store, 8)["rerun"]["state"].as_str()), (Some("asked"), Some("asked")));
    // Asked once, however often the sweep comes by.
    block_on(plane.alarm()).unwrap();
    assert_eq!(clouds.clouds.calls.borrow().iter().filter(|c| c.1.contains("rerun")).count(), 1);
}

#[test]
fn a_failing_runner_that_takes_another_job_is_said_on_it_and_that_jobs_machine_takes_the_first() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let flaky = Flaky { fails: std::cell::Cell::new(3), starts: std::cell::Cell::new(0) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&flaky) };
    let run = |r: Request| block_on(plane.handle(r));
    // Job 7's machine would not start, three times: it fails, and a failing runner is started to say so on GitHub.
    run(webhook("queued", None));
    for _ in 0..2 { block_on(plane.alarm()).unwrap() }
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7)["failed_fast"].as_bool(), flaky.starts.get()), (Some("failed"), Some(true), 4));
    let failing = format!("superci-{PLANE_ID}-7-x");
    assert!(store.0.borrow().contains_key(&format!("runner:{failing}")), "known by its name");
    // Job 8, with the same label, gets a machine; the failing runner takes it first and fails it.
    run(job_event("queued", 8, "acme/app", None));
    run(job_event("in_progress", 8, "acme/app", Some(&failing)));
    run(concluded(8, &failing, "failure"));
    let eight = job(&store, 8);
    assert_eq!((eight["state"].as_str(), eight["failed_fast"].as_bool()), (Some("launched"), Some(true)), "its machine stays, for job 7");
    assert!(eight["error"].as_str().unwrap().starts_with("failed by the runner started to fail another job"), "{eight}");
    assert_eq!(job(&store, 7)["state"], "failed", "untouched by another job's end");
    // Job 8's machine then takes job 7, still queued: it runs, and no machine is started again for job 8 (it is over).
    let runner8 = format!("superci-{PLANE_ID}-8");
    run(job_event("in_progress", 7, "acme/app", Some(&runner8)));
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7).get("failed_fast"), job(&store, 8)["state"].as_str(), flaky.starts.get()), (Some("running"), None, Some("failed"), 5));
}

#[test]
fn a_gitlab_job_whose_spot_machine_is_taken_back_is_run_again_on_demand() {
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        if r.url == "https://gitlab.example/api/v4/projects/380/jobs/1977/retry" { return Some(Response::new(201, "application/json", "{}")) }
        (r.url == "https://gitlab.example/api/v4/projects/380/jobs/1981").then(|| Response::new(200, "application/json", serde_json::json!({ "id": 1981, "status": "pending", "tag_list": ["superci/cpu=2", "docker"] }).to_string()))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let job = |id: u64| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("job:gl{id}")]).unwrap();
    let secret = "hook-secret-0123456789";
    // Its spot machine watches for AWS's notice, and would end GitLab's runner (the job then fails at once).
    run(gitlab_event(1977, "pending", secret));
    let token = job(1977)["notice"].as_str().unwrap().to_string();
    let script = user_data_of(&launches(&clouds.0)[0]);
    assert!(script.contains(&format!("{PLANE_URL}/interrupted?gl=1977&t={token}")) && script.contains("pkill -TERM -f /tmp/gitlab-runner"));
    assert!(script.find("spot/instance-action").unwrap() < script.find("GL_TOKEN=").unwrap(), "watching before the runner starts");
    run(gitlab_event_on(1977, "running", secret, Some(55)));
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?gl=1977&t=wrong-wrong-wrong-wrong"))).status, 404);
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?gl=1977&t={token}"))).status, 200);
    // Said again (its watch may say it twice): one record. An id that is no GitLab job's, or none: nothing here.
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?gl=1977&t={token}"))).status, 200);
    assert_eq!(store.0.borrow().keys().filter(|k| k.starts_with("spot:interrupted:")).count(), 1);
    for gl in ["", "19%2077", "AB"] { assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/interrupted?gl={gl}&t={token}"))).status, 404, "{gl}") }
    // Its machine says how its start went by its own job's id likewise.
    let hello = job(1977)["hello"].as_str().unwrap().to_string();
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/hello?gl=1977&t={hello}&ready=1789999999000"))).status, 200);
    for gl in ["", "AB"] { assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/hello?gl={gl}&t={hello}"))).status, 404, "{gl}") }
    // The job fails: GitLab is asked to run it again at once, and the new job gets an on-demand machine.
    run(gitlab_event_on(1977, "failed", secret, Some(55)));
    assert!(clouds.0.calls.borrow().iter().any(|c| c.0 == "POST" && c.1.ends_with("/projects/380/jobs/1977/retry")));
    assert_eq!((job(1977)["state"].as_str(), job(1977)["rerun"]["state"].as_str()), (Some("failed"), Some("asked")));
    assert!(job(1977)["error"].as_str().unwrap().starts_with("AWS took back its spot machine; SuperCI ran the job again by itself, off spot machines"));
    run(gitlab_event(1981, "pending", secret));
    assert_eq!((job(1981)["on_demand"].as_bool(), job(1981).get("notice")), (Some(true), None));
    assert_eq!(field(&launches(&clouds.0)[1], "InstanceMarketOptions.MarketType"), "");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("rerun:")), "the note was for that one job");
}

#[test]
fn a_network_of_the_accounts_own_takes_the_machines_and_nothing_else_does_when_it_is_wrong() {
    let subnets = |body: &str, both: bool| body.contains("Action=DescribeSubnets").then(|| Response::new(200, "text/xml", format!("<subnetSet><item><subnetId>subnet-0aaaaaaaa</subnetId><vpcId>vpc-theirs</vpcId><availabilityZone>us-east-1c</availabilityZone></item>{}</subnetSet>",
        if both { "<item><subnetId>subnet-0bbbbbbbb</subnetId><vpcId>vpc-theirs</vpcId><availabilityZone>us-east-1a</availabilityZone></item>" } else { "" })));
    let groups = |body: &str| body.contains("Action=DescribeSecurityGroups").then(|| Response::new(200, "text/xml", "<securityGroupInfo><item><groupId>sg-0cccccccc</groupId></item><item><groupId>sg-0dddddddd</groupId></item></securityGroupInfo>".to_string()));
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| subnets(body, true).or(groups(body)));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_networks.insert("us-east-1".into(), superci_core::aws::GivenNetwork { subnets: vec!["subnet-0aaaaaaaa".into(), "subnet-0bbbbbbbb".into()], security_groups: vec!["sg-0cccccccc".into(), "sg-0dddddddd".into()], private: true });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    // In its subnets (by zone), with its security groups, and no public address (a private subnet).
    let l = &launches(&clouds.0)[0];
    assert_eq!((field(l, "NetworkInterface.1.SubnetId"), field(l, "NetworkInterface.1.SecurityGroupId.1"), field(l, "NetworkInterface.1.SecurityGroupId.2"), field(l, "NetworkInterface.1.AssociatePublicIpAddress")),
        ("subnet-0bbbbbbbb".into(), "sg-0cccccccc".into(), "sg-0dddddddd".into(), "false".into()));
    assert!(!clouds.0.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeVpcs")), "its own network is not looked for");
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["private"].as_bool()), (Some("launched"), Some(true)));
    // No public address in its price: the spot price in its zone and its disk only.
    assert!((j["usd_per_hour"].as_f64().unwrap() - (0.065 + superci_core::aws::extras_usd_per_hour(0.096, 60, 125, 3000, false))).abs() < 0.02 && superci_core::aws::extras_usd_per_hour(0.08, 60, 125, 3000, true) - superci_core::aws::extras_usd_per_hour(0.08, 60, 125, 3000, false) == 0.005);

    // The setting changed in the dashboard: used by the next job at once (what was looked up before is not kept for it).
    let before = config.aws_networks.insert("us-east-1".into(), superci_core::aws::GivenNetwork { subnets: vec!["subnet-0aaaaaaaa".into()], security_groups: vec!["sg-0cccccccc".into()], private: false }).unwrap();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    let l = &launches(&clouds.0)[1];
    assert_eq!((field(l, "NetworkInterface.1.SubnetId"), field(l, "NetworkInterface.1.AssociatePublicIpAddress")), ("subnet-0aaaaaaaa".into(), "true".into()));
    config.aws_networks.insert("us-east-1".into(), before);

    // A subnet that is not there: said, and no machine anywhere else (never the default network).
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| subnets(body, false).or(groups(body)));
    let (store, cache) = (Mem::default(), Cache::default());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    assert!(launches(&clouds.0).is_empty());
    assert!(job(&store, 7)["error"].as_str().unwrap().contains("us-east-1: no subnet subnet-0bbbbbbbb there"), "{}", job(&store, 7));
}

#[test]
fn the_sweep_counts_a_machines_age_from_its_start_and_starts_none_again_for_a_job_github_no_longer_has_queued() {
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (r.url == "https://api.github.com/repos/acme/app/actions/jobs/7").then(|| Response::new(200, "application/json", r#"{"status":"completed"}"#.to_string())));
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(webhook("queued", None)));
    // It had waited half an hour for room before its machine started just now: the machine is new, not stale.
    let mut j = job(&store, 7);
    j["at_ms"] = (1_790_000_000_000u64 - 30 * 60_000).into();
    store.0.borrow_mut().insert("job:7".into(), j.to_string());
    clock.0.set(clock.0.get() + 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 7)["state"], "launched");
    // Its machine never began it, and GitHub no longer has the job queued (taken elsewhere or cancelled, the event
    // lost): the machine is ended, and none is started again.
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((job(&store, 7)["state"].as_str(), own.0.borrow().iter().filter(|c| c.starts_with("start")).count()), (Some("swept"), 1));
    assert!(own.0.borrow().iter().any(|c| c.starts_with("stop")) && !store.0.borrow().keys().any(|k| k.starts_with("runner:")));
}

#[test]
fn a_github_enterprise_server_is_asked_at_its_own_address() {
    // A stand-in for a GitHub Enterprise Server: its API is under /api/v3 of its own host.
    let ghes = "https://ghe.corp.example/api/v3";
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        let path = r.url.strip_prefix(ghes)?;
        let json = |status: u16, v: serde_json::Value| Response::new(status, "application/json", v.to_string());
        Some(match (r.method.as_str(), path) {
            ("POST", "/app/installations/77/access_tokens") => json(201, serde_json::json!({ "token": "ghs_corp" })),
            ("POST", "/orgs/corp/actions/runners/generate-jitconfig") => { assert_eq!(r.header("authorization"), Some("Bearer ghs_corp")); json(201, serde_json::json!({ "encoded_jit_config": "JITCORP", "runner": { "id": 9 } })) }
            ("GET", "/app/installations") => json(200, serde_json::json!([{ "id": 77, "account": { "login": "corp" }, "repository_selection": "all" }])),
            ("GET", "/app") => json(200, serde_json::json!({ "permissions": { "organization_self_hosted_runners": "write", "actions": "write", "metadata": "read" } })),
            ("GET", "/repos/corp/api/actions/jobs/32") => json(200, serde_json::json!({ "status": "completed" })),
            ("DELETE", "/orgs/corp/actions/runners/9") => Response::new(204, "application/json", ""),
            ("GET", "/app/hook/deliveries?per_page=100") => json(200, serde_json::json!([])),
            _ => Response::new(404, "text/plain", format!("unexpected {} {}", r.method, r.url)),
        })
    });
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(App { id: 7, owner: "corp".into(), slug: "superci-corp".into(), host: Some("ghe.corp.example".into()), ..app() });
    config.containers = true;
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let event = |job: u64, action: &str, runner: Option<&str>| {
        let payload = serde_json::json!({ "action": action, "workflow_job": { "id": job, "run_id": 3, "labels": ["superci"], "runner_name": runner }, "repository": { "full_name": "corp/api", "private": true }, "installation": { "id": 77 } }).to_string();
        let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
        Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_header("x-github-enterprise-host", "ghe.corp.example").with_body(payload)
    };
    // A job from it: its token and its runner's registration are asked of that server.
    assert_eq!(block_on(plane.handle(event(31, "queued", None))).status, 202);
    assert_eq!((job(&store, 31)["state"].as_str(), job(&store, 31)["runner_id"].as_u64()), (Some("launched"), Some(9)));
    assert!(own.0.borrow().iter().any(|c| c.contains("JITCORP")));
    block_on(plane.handle(event(31, "completed", Some(&format!("superci-{PLANE_ID}-31")))));
    assert_eq!(job(&store, 31)["state"], "done");
    // The dashboard is told where that GitHub is (its links go there).
    let status = json(&block_on(plane.handle(get("/status").with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert_eq!((status["apps"][0]["host"].as_str(), status["app"]["host"].as_str(), status["installations"][0]["account"].as_str()), (Some("ghe.corp.example"), Some("ghe.corp.example"), Some("corp")));
    // The sweep asks that server too: a machine that never began a job it no longer has queued is ended, its runner withdrawn there.
    block_on(plane.handle(event(32, "queued", None)));
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 32)["state"], "swept");
    let calls = clouds.0.calls.borrow();
    assert!(calls.iter().any(|c| c.0 == "DELETE" && c.1 == format!("{ghes}/orgs/corp/actions/runners/9")));
    // Nothing about it was asked of github.com.
    assert!(!calls.iter().any(|c| c.1.contains("api.github.com")), "{:?}", calls.iter().map(|c| c.1.clone()).filter(|u| u.contains("github.com")).collect::<Vec<_>>());
}

#[test]
fn a_job_whose_runner_was_taken_gets_a_machine_even_if_the_other_jobs_never_comes_up() {
    // GitHub says job 8 is still queued.
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (r.url == "https://api.github.com/repos/acme/app/actions/jobs/8").then(|| Response::new(200, "application/json", r#"{"status":"queued"}"#.to_string())));
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let starts = || own.0.borrow().iter().filter(|c| c.starts_with("start")).count();
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    run(job_event("queued", 7, "acme/app", None));
    run(job_event("queued", 8, "acme/app", None));
    // Job 8's runner takes job 7 (GitHub gives a job to any runner with its label): job 7's machine, still coming,
    // is for job 8 now.
    run(job_event("in_progress", 7, "acme/app", Some(&runner(8))));
    assert_eq!((job(&store, 8)["state"].as_str(), job(&store, 7)["state"].as_str(), job(&store, 7)["for_job"].as_u64()), (Some("running"), Some("launched"), Some(8)));
    // Job 7 ends there. Said twice (GitHub delivers again what timed out): job 7's machine is still kept for job 8.
    for _ in 0..2 { run(concluded(7, &runner(8), "success")); }
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7)["over"].as_bool(), job(&store, 8)["state"].as_str()), (Some("launched"), Some(true), Some("done")));
    // That machine never comes up, and job 8 still waits: another is started for it (not none, because job 7 is over).
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((starts(), job(&store, 7)["state"].as_str(), job(&store, 7)["retries"].as_u64()), (3, Some("launched"), Some(1)));
    // It takes job 8, which runs on it; nothing more is started for job 7.
    let again = job(&store, 7)["runner"].as_str().unwrap().to_string();
    run(job_event("in_progress", 8, "acme/app", Some(&again)));
    assert_eq!((job(&store, 8)["state"].as_str(), job(&store, 8)["runner"].as_str(), job(&store, 7)["state"].as_str(), starts()), (Some("running"), Some(again.as_str()), Some("done"), 3));

    // A machine left idle by a cancelled job that takes another job before the sweep comes is busy, not ended.
    run(job_event("queued", 9, "acme/app", None));
    run(job_event("queued", 10, "acme/app", None));
    run(job_event("completed", 9, "acme/app", None));
    assert_eq!(job(&store, 9)["state"], "orphan");
    run(job_event("in_progress", 10, "acme/app", Some(&runner(9))));
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 9)["state"], "running");
    assert!(!own.0.borrow().iter().any(|c| c == &format!("stop {}", runner(9))), "{:?}", own.0.borrow());
}

/// A clock on which every look takes eight seconds (AWS answering slowly).
struct Slow(std::cell::Cell<u64>);
impl Clock for Slow { fn now_ms(&self) -> u64 { self.0.set(self.0.get() + 8_000); self.0.get() } }

#[test]
fn out_of_time_while_spot_is_refused_the_next_try_starts_below_spot() {
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache, clock) = (Mem::default(), Wakes::default(), Cache::default(), Slow(std::cell::Cell::new(1_790_000_000_000)));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // One request has time for the first region only: refused there, the job waits, marked to pass spot over.
    block_on(plane.handle(webhook("queued", None)));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["no_spot"].as_bool(), j["spot_refused"].as_str()), (Some("waiting"), Some(true), Some("us-east-1: InsufficientInstanceCapacity")), "{j}");
    assert!(launches(&clouds.0).iter().all(|l| field(l, "InstanceMarketOptions.MarketType") == "spot"));
    // The next try starts at the place below spot (on-demand, by default), and gets its machine.
    block_on(plane.alarm()).unwrap();
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool(), j.get("notice")), (Some("launched"), Some(true), None), "{j}");
    assert_eq!(field(launches(&clouds.0).last().unwrap(), "InstanceMarketOptions.MarketType"), "");
}

#[test]
fn several_gitlabs_are_kept_apart_each_with_its_own_token_and_secret() {
    // A second GitLab (a company's own), whose job and runner ids happen to be the same as the first's.
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        let path = r.url.strip_prefix("https://gitlab.corp.example/api/v4")?;
        assert_eq!(r.header("private-token"), Some("glpat-corp"), "asked with its own token");
        let json = |status: u16, v: serde_json::Value| Response::new(status, "application/json", v.to_string());
        Some(match (r.method.as_str(), path) {
            ("GET", "/projects/7/jobs/1977") => json(200, serde_json::json!({ "id": 1977, "status": "pending", "tag_list": ["superci/cpu=2"] })),
            ("POST", "/user/runners") => json(201, serde_json::json!({ "id": 55, "token": "glrt-corp55" })),
            ("PUT", "/runners/55") => json(200, serde_json::json!({})),
            ("DELETE", "/runners/55") => Response::new(204, "application/json", ""),
            ("GET", "/projects?membership=true&min_access_level=40&simple=true&order_by=last_activity_at&per_page=40") => json(200, serde_json::json!([])),
            _ => Response::new(404, "text/plain", format!("unexpected {} {}", r.method, r.url)),
        })
    });
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.containers = true;
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    config.more_gitlabs = vec![superci_core::gitlab::GitLab { url: "https://gitlab.corp.example".into(), token: "glpat-corp".into(), hook_secret: "hook-secret-corp-0123456789".into(), id: "corp".into() }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let corp = |status: &str, runner: Option<u64>| {
        let mut body = serde_json::json!({ "object_kind": "build", "build_id": 1977, "build_name": "test", "build_status": status, "pipeline_id": 9, "project_id": 7, "project": { "id": 7, "path_with_namespace": "corp/api" } });
        if let Some(id) = runner { body["runner"] = serde_json::json!({ "id": id }) }
        Request::new("POST", &format!("{PLANE_URL}/gitlab/webhook")).with_header("x-gitlab-token", "hook-secret-corp-0123456789").with_body(body.to_string())
    };
    let record = |key: &str| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[key]).unwrap();
    // Job 1977 of each: two jobs, two runners, each made at its own GitLab; the event's secret says which it is from.
    run(gitlab_event(1977, "pending", "hook-secret-0123456789"));
    assert_eq!(run(corp("pending", None)).status, 200);
    assert_eq!((record("job:gl1977")["repo"].as_str(), record("job:gl1977").get("gitlab")), (Some("acme/app"), None));
    assert_eq!((record("job:glcorp-1977")["repo"].as_str(), record("job:glcorp-1977")["gitlab"].as_str(), record("job:glcorp-1977")["state"].as_str()), (Some("corp/api"), Some("corp"), Some("launched")));
    assert!(own.0.borrow().iter().any(|c| c == &format!("start superci-{PLANE_ID}-glcorp-1977 gitlab https://gitlab.corp.example glrt-corp55 2cpu 8gb 20disk")), "{:?}", own.0.borrow());
    assert!(store.0.borrow().contains_key("glrunner:55") && record("glrunner:corp-55")["gitlab"] == "corp");
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/gitlab/webhook")).with_header("x-gitlab-token", "hook-secret-nobody-0123456789").with_body("{}")).status, 401);
    // The second's job runs and ends on its runner 55: only its record and its runner change.
    run(corp("running", Some(55)));
    run(corp("success", Some(55)));
    assert_eq!((record("job:glcorp-1977")["state"].as_str(), record("job:gl1977")["state"].as_str()), (Some("done"), Some("launched")));
    assert!(store.0.borrow().contains_key("glrunner:55") && !store.0.borrow().contains_key("glrunner:corp-55"));
    assert!(!clouds.0.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/55"), "the first's runner 55 is left alone");
    // The dashboard sees both, and asks for the projects of one by its name.
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let status = json(&run(key(get("/status"))));
    assert_eq!((status["gitlabs"][0]["id"].as_str(), status["gitlabs"][1]["id"].as_str(), status["gitlabs"][1]["url"].as_str(), status["gitlab"]["url"].as_str()), (Some(""), Some("corp"), Some("https://gitlab.corp.example"), Some("https://gitlab.example")));
    assert_eq!(run(key(get("/gitlab/projects?g=corp"))).status, 200);
    assert_eq!(run(key(get("/gitlab/projects?g=nobody"))).status, 409);
    assert!(superci_core::gitlab::valid_id("corp") && !superci_core::gitlab::valid_id("9corp") && !superci_core::gitlab::valid_id("a-b") && !superci_core::gitlab::valid_id(""));
}

#[test]
fn with_no_spot_machine_the_job_goes_to_the_next_place_in_the_order() {
    // AWS has no spot machine anywhere. The order: AWS spot, Cloudflare, AWS on-demand.
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
    config.routing.order = ["aws", "cloudflare", "aws-on-demand"].iter().map(|c| Pool { regions: vec![], off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None, function: false }).collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    // The standard machine fits Cloudflare: the job runs there, and no on-demand machine is asked for.
    run(webhook("queued", None));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["cloud"].as_str(), j.get("on_demand"), j.get("notice")), (Some("launched"), Some("cloudflare"), None, None), "{j}");
    assert_eq!(j["spot_refused"], "us-east-1: InsufficientInstanceCapacity; us-east-2: InsufficientInstanceCapacity");
    assert!(launches(&clouds.0).iter().all(|l| field(l, "InstanceMarketOptions.MarketType") == "spot"), "only spot machines were asked for");
    assert!(own.0.borrow().iter().any(|c| c.starts_with(&format!("start superci-{PLANE_ID}-7 "))));
    // A machine Cloudflare cannot be (16 CPUs) passes it by, down to AWS on-demand.
    run(labelled("queued", 8, "acme/app", None, "superci/cpu=16"));
    let j = job(&store, 8);
    assert_eq!((j["state"].as_str(), j["cloud"].as_str(), j["on_demand"].as_bool()), (Some("launched"), Some("aws"), Some(true)), "{j}");
    assert_eq!(field(launches(&clouds.0).last().unwrap(), "InstanceMarketOptions.MarketType"), "");
    // While spot is paused (AWS took two machines back just now) it is not even asked: straight to the next place.
    for t in [1_789_999_990_000u64, 1_789_999_995_000] { store.0.borrow_mut().insert(format!("spot:interrupted:{t}:1"), t.to_string()); }
    let before = launches(&clouds.0).len();
    run(job_event("queued", 9, "acme/app", None));
    assert_eq!((job(&store, 9)["cloud"].as_str(), job(&store, 9).get("spot_refused")), (Some("cloudflare"), None));
    assert_eq!(launches(&clouds.0).len(), before, "AWS was not asked");
}

#[test]
fn each_place_of_awss_starts_its_machines_in_its_own_regions() {
    // Spot machines in Ohio only (none there now), on-demand ones in N. Virginia only.
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
    let mut spot = Pool::of("aws");
    spot.regions = vec!["us-east-2".into()];
    let mut on_demand = Pool::of("aws-on-demand");
    on_demand.regions = vec!["us-east-1".into(), "eu-north-1".into()];
    config.routing.order = vec![spot, on_demand];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool(), j["spot_refused"].as_str()), (Some("launched"), Some(true), Some("us-east-2: InsufficientInstanceCapacity")), "{j}");
    let regions: Vec<(String, String)> = clouds.0.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances"))
        .map(|c| (c.1.split('.').nth(1).unwrap_or_default().to_string(), if c.2.contains("MarketType=spot") { "spot".to_string() } else { "on-demand".to_string() })).collect();
    assert!(regions.iter().all(|(r, kind)| (kind == "spot" && r == "us-east-2") || (kind == "on-demand" && r == "us-east-1")), "{regions:?}");
    // (A region of its own the control plane is not set up in, Stockholm here, is passed over.)
    assert!(regions.iter().any(|(_, kind)| kind == "on-demand") && !regions.iter().any(|(r, _)| r == "eu-north-1"));
}

#[test]
fn with_on_demand_turned_off_and_nothing_else_in_the_order_the_job_waits_for_a_spot_machine() {
    let no_spot = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (no_spot.get() && body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None, function: false }, Pool { regions: vec![], off: true, cloud: "aws-on-demand".into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // No spot machine, and on-demand is off: the job waits (it does not fail), however many times it is tried.
    block_on(plane.handle(webhook("queued", None)));
    for _ in 0..4 { block_on(plane.alarm()).unwrap(); }
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["retries"].as_u64().unwrap_or(0), j.get("on_demand")), (Some("waiting"), 0, None), "{j}");
    assert!(j["error"].as_str().unwrap().starts_with("waiting: AWS has no spot machine for it now, and nothing else in the order can run it (us-east-1: InsufficientInstanceCapacity"), "{}", j["error"]);
    assert!(wakes.0.borrow().contains(&30_000) && launches(&clouds.0).iter().all(|l| field(l, "InstanceMarketOptions.MarketType") == "spot"));
    // A spot machine is there again: the next look starts the job on it.
    no_spot.set(false);
    block_on(plane.alarm()).unwrap();
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j.get("on_demand"), j["notice"].is_string(), j.get("spot_refused")), (Some("launched"), None, true, None), "{j}");
    // Spot paused, and still nothing else to run a job: a spot machine it is.
    for t in [1_789_999_990_000u64, 1_789_999_995_000] { store.0.borrow_mut().insert(format!("spot:interrupted:{t}:1"), t.to_string()); }
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    assert_eq!((job(&store, 8)["state"].as_str(), job(&store, 8).get("on_demand")), (Some("launched"), None), "{}", job(&store, 8));
    // A label that asks for on-demand, with on-demand off: refused, saying so.
    block_on(plane.handle(labelled("queued", 9, "acme/app", None, "superci/spot=false")));
    let e = job(&store, 9)["error"].as_str().unwrap().to_string();
    assert!(job(&store, 9)["state"] == "failed" && e.contains("AWS on-demand is turned off"), "{e}");
}

#[test]
fn a_provider_that_cannot_start_the_machine_sends_the_job_to_the_next_in_the_order() {
    // Cloudflare's launcher fails every launch (a hiccup of its own). The order: Cloudflare, then AWS.
    let down = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (down.get() && r.url == "https://runners.acme.workers.dev/launch").then(|| Response::new(503, "text/plain", "containers temporarily unavailable")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = ["cloudflare", "aws", "aws-on-demand"].iter().map(|c| Pool { regions: vec![], off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None, function: false }).collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    // The job starts on AWS (a spot machine) at once: it does not wait on Cloudflare.
    run(webhook("queued", None));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["cloud"].as_str(), j["retries"].as_u64().unwrap_or(0), j["notice"].is_string()), (Some("launched"), Some("aws"), 0, true), "{j}");
    assert_eq!(launches(&clouds.0).len(), 1);
    // Once started, where it failed before is forgotten.
    assert_eq!(j.get("failed_at"), None);

    // No provider can start it (AWS refuses the request too): each says why, it is tried again twice, then fails.
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
        if r.url == "https://runners.acme.workers.dev/launch" { return Some(Response::new(503, "text/plain", "containers temporarily unavailable")) }
        body.contains("Action=RunInstances").then(|| aws_refuses("UnauthorizedOperation"))
    });
    let store = Mem::default();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let j = job(&store, 7);
    let e = j["error"].as_str().unwrap().to_string();
    assert_eq!((j["state"].as_str(), j["retries"].as_u64()), (Some("waiting"), Some(1)), "{j}");
    assert!(e.contains("no provider could start its machine: Cloudflare: cloudflare launcher: 503 containers temporarily unavailable; AWS: UnauthorizedOperation") && !e.contains("AWS on-demand"), "said once for AWS: {e}");
    assert_eq!(j["failed_at"], serde_json::json!(["cloudflare", "aws", "aws-on-demand"]));
    block_on(plane.alarm()).unwrap();
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 7)["state"], "failed");
}

#[test]
fn a_provider_that_refuses_what_a_job_asks_is_passed_by_and_only_all_refusing_fails_it_at_once() {
    // Modal refuses GPUs (no payment method): a refusal of what is asked, not a hiccup. AWS is below it in the order.
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (r.url == "https://runners.acme.workers.dev/launch").then(|| Response::new(400, "application/json", serde_json::json!({ "detail": "GPUs need a payment method" }).to_string())));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = ["modal", "aws", "aws-on-demand"].iter().map(|c| Pool { regions: vec![], off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None, function: false }).collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/gpu=any")));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["cloud"].as_str()), (Some("launched"), Some("aws")), "{j}");
    assert!(clouds.0.calls.borrow().iter().any(|c| c.1 == "https://runners.acme.workers.dev/launch"), "Modal was asked first");
    // With AWS allowing no GPU machines either, every provider refuses what it asks: failed at once, not tried again.
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
        if r.url == "https://runners.acme.workers.dev/launch" { return Some(Response::new(400, "application/json", serde_json::json!({ "detail": "GPUs need a payment method" }).to_string())) }
        body.contains("Action=RunInstances").then(|| aws_refuses("VcpuLimitExceeded"))
    });
    let store = Mem::default();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/gpu=any")));
    let j = job(&store, 7);
    let e = j["error"].as_str().unwrap().to_string();
    assert_eq!((j["state"].as_str(), j["retries"].as_u64().unwrap_or(0)), (Some("failed"), 0), "{j}");
    assert!(e.starts_with("no provider could start its machine: Modal: modal launcher: 400 GPUs need a payment method; AWS lets this account run no GPU machines yet"), "{e}");
}

#[test]
fn no_gpu_allowance_is_said_at_once_even_when_spot_refuses_for_another_reason() {
    // As a new AWS account answered in a real run: spot refuses the GPU types for the zone ("Unsupported"), on-demand
    // for the account's limit (its allowance for GPU machines starts at 0).
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| body.contains("Action=RunInstances").then(|| aws_refuses(if body.contains("MarketType=spot") { "Unsupported" } else { "VcpuLimitExceeded" })));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/gpu=any")));
    let j = job(&store, 7);
    let e = j["error"].as_str().unwrap().to_string();
    assert_eq!((j["state"].as_str(), j["retries"].as_u64().unwrap_or(0)), (Some("failed"), 0), "at once, not after three tries: {j}");
    // The allowance to raise is the on-demand one: spot did not refuse for its allowance.
    assert!(e.starts_with("AWS lets this account run no GPU machines yet") && e.contains("L-DB2E81BA") && !e.contains("L-3819A6DF"), "{e}");
}

/// The fake clouds, with two jobs' logs: GitHub answers the first with where it is (to be fetched without the
/// token), and has none for the second; GitLab gives a job's trace.
struct Logs(FakeClouds);
#[async_trait(?Send)]
impl Http for Logs {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        match r.url.as_str() {
            "https://api.github.com/repos/acme/app/actions/jobs/1/logs" => {
                assert_eq!(r.header("authorization"), Some("Bearer ghs_test"), "asked with the App's token for the job's repository");
                let mut at = Response::new(302, "text/plain", "");
                at.headers.push(("Location".into(), "https://logs.example/blob?sig=abc".into()));
                Ok(at)
            }
            "https://logs.example/blob?sig=abc" => {
                assert!(r.header("authorization").is_none(), "the signed link is fetched without the token");
                Ok(Response::new(200, "text/plain", format!("{}Error: 3 tests failed\n", "a line of the build, forty characters\n".repeat(80))))
            }
            "https://api.github.com/repos/acme/app/actions/jobs/2/logs" => Ok(Response::new(404, "application/json", "{}")),
            "https://gitlab.example/api/v4/projects/380/jobs/1977/trace" => {
                assert_eq!(r.header("private-token"), Some("glpat-test"));
                Ok(Response::new(200, "text/plain", "$ make test\nok\n"))
            }
            _ => self.0.send(r).await,
        }
    }
}

#[test]
fn a_key_that_only_reads_sees_the_status_and_a_jobs_log_and_changes_nothing() {
    let (store, clouds, wakes, cache) = (Mem::default(), Logs(FakeClouds::default()), Wakes::default(), Cache::default());
    let hash = |key: &str| superci_core::crypto::sha256_hex(key.as_bytes());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    config.device_keys = vec![(1_800_000_000_000, "dashboard-session-key-1".into())];
    // As the runtimes read them from where they are stored: a name that says when it ends, the key's SHA-256.
    config.read_keys = [("READ_KEY_1800000000_AGENT", hash("superci_read_the-agents-key")), ("READ_KEY_1700000000_OLD", hash("superci_read_one-that-ended"))].iter()
        .filter_map(|(n, v)| superci_core::plane::read_key(n, v)).collect();
    assert_eq!(config.read_keys.len(), 2);
    assert!(superci_core::plane::read_key("READ_KEY_1800000000_AGENT", "").is_none() && superci_core::plane::read_key("READ_KEY_soon_AGENT", &hash("x")).is_none() && superci_core::plane::read_key("DEVICE_KEY_1800000000_AB", &hash("x")).is_none());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    for (id, state) in [(1u64, "failed"), (2, "running")] {
        store.0.borrow_mut().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 1, "repo": "acme/app", "state": state, "at_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
    }
    store.0.borrow_mut().insert("job:gl1977".into(), serde_json::json!({ "job_id": 1977, "run_id": 1, "repo": "acme/web", "state": "done", "at_ms": 1_789_999_000_000u64, "installation_id": 0, "cloud": "aws", "seen_in_progress": true, "provider": "gitlab", "project_id": 380 }).to_string());
    let ask = |method: &str, path: &str, key: &str| block_on(plane.handle(Request::new(method, &format!("{PLANE_URL}{path}")).with_header("authorization", &format!("Bearer {key}"))));
    let (dashboard, agent) = ("dashboard-session-key-1", "superci_read_the-agents-key");

    // It reads what the dashboard reads; one that ended, or is not known, reads nothing.
    let seen = ask("GET", "/status", agent);
    assert_eq!((seen.status, json(&seen)["jobs"].as_array().map(Vec::len)), (200, Some(3)));
    assert!(json(&seen)["read_keys"].is_null(), "which keys there are is the dashboard's to see");
    assert_eq!(json(&ask("GET", "/status", dashboard))["read_keys"], serde_json::json!([{ "name": "AGENT", "until_ms": 1_800_000_000_000u64 }, { "name": "OLD", "until_ms": 1_700_000_000_000u64 }]));
    // One that ended or was never made: not taken, said as that; with none, nothing here.
    for (key, status) in [("superci_read_one-that-ended", 401), ("superci_read_never-made-here", 401), ("", 404)] { assert_eq!(ask("GET", "/status", key).status, status, "{key}") }
    assert!(String::from_utf8_lossy(&ask("GET", "/status", "superci_read_one-that-ended").body).starts_with(superci_core::plane::KEY_REFUSED));
    // It changes nothing: what the dashboard's key may do is refused to it, as needing a device's key.
    for path in ["/costs", "/gitlab/projects", "/leave", "/aws/forget", "/move/export", "/github/uninstall", "/permissions/given"] {
        assert_eq!(ask("POST", path, agent).status, 403, "{path}");
    }
    assert_ne!(ask("POST", "/permissions/given", dashboard).status, 404);

    // A job's log: its end, from a line's start, and how much there is in all.
    let log = json(&ask("GET", "/job/log?id=1&bytes=1000", agent));
    let text = log["log"].as_str().unwrap();
    assert!(text.ends_with("Error: 3 tests failed\n") && text.starts_with("a line of the build") && text.len() <= 1000 && text.len() > 900);
    assert_eq!((log["truncated"].as_bool(), log["bytes"].as_u64()), (Some(true), Some(80 * 38 + 22)));
    let whole = json(&ask("GET", "/job/log?id=1", dashboard));
    assert_eq!((whole["truncated"].as_bool(), whole["log"].as_str().map(str::len)), (Some(false), Some(80 * 38 + 22)));
    // No log yet (the job still runs), no such job, and GitLab's trace.
    assert!(json(&ask("GET", "/job/log?id=2", agent))["error"].as_str().unwrap().starts_with("GitHub has no log for it yet"));
    assert!(json(&ask("GET", "/job/log?id=99", agent))["error"].as_str().unwrap().starts_with("This control plane has no such job"));
    assert_eq!(json(&ask("GET", "/job/log?id=1977&gl=", agent))["log"], "$ make test\nok\n");
    assert_eq!(ask("GET", "/job/log?id=1", "superci_read_one-that-ended").status, 401);
    assert_eq!(block_on(plane.handle(get("/job/log?id=1"))).status, 404);
}

/// The fake clouds, with what GitHub lists as waiting in acme/app: a run that waits (its jobs: one for here that
/// GitHub never told of, one for GitHub's own runners, one for here made ten seconds ago, one known here), and a run
/// under way with another job for here that waits.
struct Untold(FakeClouds);
#[async_trait(?Send)]
impl Http for Untold {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let job = |id: u64, label: &str, at: &str| serde_json::json!({ "id": id, "run_id": 90, "status": "queued", "name": format!("job {id}"), "workflow_name": "ci", "labels": [label], "created_at": at });
        let ok = |v: serde_json::Value| Ok(Response::new(200, "application/json", v.to_string()));
        match r.url.strip_prefix("https://api.github.com/repos/acme/app/actions/runs") {
            Some("?status=queued&per_page=10") => { self.0.calls.borrow_mut().push(("GET".into(), r.url.clone(), String::new())); ok(serde_json::json!({ "workflow_runs": [{ "id": 90, "repository": { "private": true } }] })) }
            Some("?status=in_progress&per_page=10") => { self.0.calls.borrow_mut().push(("GET".into(), r.url.clone(), String::new())); ok(serde_json::json!({ "workflow_runs": [{ "id": 91, "repository": { "private": true } }] })) }
            Some("/90/jobs?per_page=50") => ok(serde_json::json!({ "jobs": [job(9, "superci", "2026-09-21T14:11:20Z"), job(10, "ubuntu-latest", "2026-09-21T14:11:20Z"), job(11, "superci", "2026-09-21T14:13:10Z"), job(7, "superci", "2026-09-21T13:00:00Z"),
                { "id": 12, "run_id": 90, "status": "in_progress", "labels": ["superci"], "created_at": "2026-09-21T14:11:20Z" }] })),
            Some("/91/jobs?per_page=50") => ok(serde_json::json!({ "jobs": [job(13, "superci/cpu=2", "2026-09-21T14:11:20Z")] })),
            _ => self.0.send(r).await,
        }
    }
}

#[test]
fn a_job_github_never_told_of_is_found_by_asking_and_gets_its_machine() {
    let (store, clouds, wakes, cache) = (Mem::default(), Untold(FakeClouds::default()), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // A repository that had a job here an hour ago, one whose last was two days ago (a quiet one: asked about too,
    // or the first push after a weekend would rest on one event alone), and one whose last was three weeks ago (not).
    for (id, repo, ago_ms) in [(7u64, "acme/app", 3_600_000u64), (3, "acme/old", 2 * 86_400_000), (2, "acme/gone", 21 * 86_400_000)] {
        store.0.borrow_mut().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 1, "repo": repo, "state": "done", "at_ms": 1_790_000_000_000u64 - ago_ms, "ended_ms": 1_790_000_000_000u64 - ago_ms + 60_000,
            "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
    }
    let asked = |what: &str| clouds.0.calls.borrow().iter().filter(|c| c.1.contains(what)).count();
    block_on(plane.alarm()).unwrap();
    // The one for here that waits and was never told of gets a machine; one for other runners, one made a moment
    // ago (its event may be on its way), one known, and one that runs do not.
    assert_eq!(job(&store, 9)["label"], "superci");
    assert_eq!(launches(&clouds.0).len(), 1, "{}", job(&store, 9));
    for id in [10, 11, 12, 13] { assert!(!store.0.borrow().contains_key(&format!("job:{id}")), "{id}") }
    assert_eq!((asked("acme/app/actions/runs?status=queued"), asked("acme/app/actions/runs?status=in_progress"), asked("acme/old/actions/runs?status=queued"), asked("acme/gone")), (1, 0, 1, 0));
    // Not asked again within five minutes; asked again after.
    block_on(plane.alarm()).unwrap();
    assert_eq!(asked("acme/app/actions/runs?status=queued"), 1);
    store.0.borrow_mut().insert("untold_at".into(), (1_790_000_000_000u64 - 6 * 60_000).to_string());
    block_on(plane.alarm()).unwrap();
    assert_eq!((asked("acme/app/actions/runs?status=queued"), launches(&clouds.0).len()), (2, 1), "asked again; the job found before is known now");
    // Just after a move here: asked at every sweep, also about runs of which a job has started, and the sweep
    // comes every minute.
    store.0.borrow_mut().insert("claimed_at".into(), (1_790_000_000_000u64 - 60_000).to_string());
    block_on(plane.alarm()).unwrap();
    assert_eq!((asked("acme/app/actions/runs?status=queued"), asked("acme/app/actions/runs?status=in_progress")), (3, 1));
    assert_eq!(job(&store, 13)["label"], "superci/cpu=2");
    assert_eq!(wakes.0.borrow().last(), Some(&60_000));
}

#[test]
fn a_control_plane_answers_to_the_labels_set_for_it_the_new_one_and_the_ones_before() {
    use superci_core::spec::{valid_label, Spec};
    for ok in ["acmeci", "ci2", "acme"] { assert!(valid_label(ok).is_ok(), "{ok}") }
    for no in ["", "a", "2ci", "my-ci", "CI", "ubuntu", "linux", "x64", "ci/big", "ci=2", "averyveryverylongnameforalabel"] { assert!(valid_label(no).is_err(), "{no}") }
    let mut config = Config::new(PLANE_ID.into());
    assert_eq!(config.labels(), ["superci"]);
    // As the dashboard sets them: the new one first, the one before kept.
    config.set_labels(r#"["acmeci","superci"]"#);
    assert_eq!((config.label.as_str(), config.labels()), ("acmeci", vec!["acmeci".to_string(), "superci".to_string()]));
    assert_eq!(config.parse("acmeci/cpu=8/arch=arm64").unwrap().unwrap().cpu, Some(8));
    assert!(config.parse("superci").is_some() && config.parse("SuperCI/arch=arm64").is_some() && config.parse("acmeciplus").is_none() && config.parse("ubuntu-latest").is_none());
    // The longest label that fits is the one meant; what cannot be a label is left out; nothing usable leaves it as it was.
    assert!(Spec::parse_among("cibig/cpu=2", &["ci".into(), "cibig".into()]).unwrap().is_ok());
    config.set_labels(r#"["my-ci","linux"]"#);
    assert_eq!(config.labels(), ["acmeci", "superci"]);
    config.set_labels(r#"["superci"]"#);
    assert_eq!(config.labels(), ["superci"]);

    // A job naming either label gets a machine, registered under the label it named; the status says both.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.set_labels(r#"["acmeci","superci"]"#);
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/health")))["label"], "acmeci");
    run(labelled("queued", 21, "acme/app", None, "acmeci/cpu=2"));
    run(labelled("queued", 22, "acme/app", None, "superci"));
    run(labelled("queued", 23, "acme/app", None, "otherci"));
    assert_eq!((job(&store, 21)["label"].as_str(), job(&store, 22)["label"].as_str(), launches(&clouds).len()), (Some("acmeci/cpu=2"), Some("superci"), 2));
    assert!(!store.0.borrow().contains_key("job:23"));
    let status = json(&run(get("/status").with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!((status["label"].as_str(), status["labels"].clone()), (Some("acmeci"), serde_json::json!(["acmeci", "superci"])));
}

#[test]
fn a_control_plane_is_called_what_it_is_named_where_people_read_it() {
    use superci_core::spec::valid_name;
    for ok in ["AcmeCI", "Acme CI", "ci-2", "Io"] { assert!(valid_name(ok).is_ok(), "{ok}") }
    for no in ["", "A", "2CI", "Acme CI ", "Acme  CI", "Acme: CI", "Acme,CI", "A name far too long to be one"] { assert!(valid_name(no).is_err(), "{no}") }
    let mut config = Config::new(PLANE_ID.into());
    assert_eq!(config.name(), "SuperCI");
    config.set_name("Acme: CI");
    assert_eq!(config.name(), "SuperCI", "what cannot be a name leaves it as it was");
    config.set_name(" AcmeCI ");
    assert_eq!(config.name(), "AcmeCI");

    // The status says it; a job nothing can run says so under that name; the App for an organization starts with it.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    assert_eq!(json(&run(get("/status").with_header("authorization", "Bearer dashboard-session-key-1")))["name"], "AcmeCI");
    run(labelled("queued", 31, "acme/app", None, "superci"));
    let why = job(&store, 31)["error"].as_str().unwrap_or_default().to_string();
    assert!(why.contains("the AcmeCI dashboard") && !why.contains("SuperCI"), "{why}");
    let named = |called: &str| superci_core::github::app_manifest(PLANE_URL, &superci_core::github::Owner { org: true, login: "acme".into() }, called, "superci")["name"].as_str().unwrap().to_string();
    assert_eq!((named("AcmeCI").as_str(), named("SuperCI").as_str()), ("AcmeCI acme", "SuperCI acme"));
    config.set_name("SuperCI");
    assert!(config.name.is_none());

    // Its runners are named by it too, where developers read it (a job's log at GitHub says which runner took the
    // job): the name, then the control plane's id and the job. One that is not named keeps `superci`.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.set_name("Acme CI");
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 41, "acme/app", None)));
    assert_eq!((job(&store, 41)["state"].as_str(), job(&store, 41)["runner"].as_str()), (Some("launched"), Some(format!("acme-ci-{PLANE_ID}-41").as_str())), "{}", job(&store, 41));
    // Given another name later, the next runner is named by that; the one already out keeps its name and is still
    // known by it when its job starts.
    config.set_name("Globex");
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 42, "acme/app", None)));
    assert_eq!(job(&store, 42)["runner"], format!("globex-{PLANE_ID}-42"));
    block_on(plane.handle(job_event("in_progress", 41, "acme/app", Some(&format!("acme-ci-{PLANE_ID}-41")))));
    assert_eq!(job(&store, 41)["state"], "running", "{}", job(&store, 41));
}

#[test]
fn a_spot_request_refused_for_its_rate_to_the_end_goes_on_demand_and_says_why() {
    // Five jobs queued at once in an account AWS lets ask for few machines in a moment (seen on 13 October 2026):
    // the spot request is refused for its rate however often it is asked again.
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("RequestLimitExceeded")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    // Asked for the same pool again after each wait (not from the first pool anew), then no more: the job goes on
    // down the order to an on-demand machine, and keeps why it is not on a spot one.
    let tried = launches(&clouds.0);
    let spot: Vec<_> = tried.iter().filter(|l| field(l, "InstanceMarketOptions.MarketType") == "spot").collect();
    assert!(spot.len() == 1 + superci_core::aws::MAX_WAITS as usize && spot.iter().all(|l| l == &spot[0]), "{} spot requests", spot.len());
    assert_eq!(wakes.1.borrow().len(), superci_core::aws::MAX_WAITS as usize);
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool()), (Some("launched"), Some(true)), "{j}");
    assert!(j["spot_refused"].as_str().is_some_and(|s| s.contains("RequestLimitExceeded")), "{j}");
}

#[test]
fn a_launch_aws_refuses_for_its_request_rate_is_asked_again_in_place() {
    // Sixteen jobs queued at once: AWS lets a few launches through and refuses the rest for a moment.
    let refused = std::cell::Cell::new(0);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && refused.get() < 2).then(|| { refused.set(refused.get() + 1); aws_refuses("RequestLimitExceeded") }));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    // The same launch, a spot one, three times, with a short wait before the second and the third (longer each time).
    let tried = launches(&clouds.0);
    assert_eq!((tried.len(), tried.iter().all(|l| field(l, "InstanceMarketOptions.MarketType") == "spot"), tried[0] == tried[2]), (3, true, true));
    let pauses = wakes.1.borrow().clone();
    assert!(pauses.len() == 2 && pauses[0] >= 300 && pauses[1] > pauses[0] && pauses.iter().sum::<u64>() < 4_000, "{pauses:?}");
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["machine_id"].as_str(), j.get("on_demand"), j.get("spot_refused")), (Some("launched"), Some("i-0abc"), None, None));
}

#[test]
fn a_job_that_began_on_anothers_runner_gets_no_machine_again_and_an_ended_machine_stays_in_the_books() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let starts = || own.0.borrow().iter().filter(|c| c.starts_with("start")).count();
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    for id in [7, 8, 9] { run(job_event("queued", id, "acme/app", None)); }
    // Three jobs pass their runners around: job 8's takes job 9, job 9's takes job 7. Job 7's machine, still coming,
    // is for job 8 now.
    run(job_event("in_progress", 9, "acme/app", Some(&runner(8))));
    run(job_event("in_progress", 7, "acme/app", Some(&runner(9))));
    assert_eq!((job(&store, 7)["for_job"].as_u64(), job(&store, 7)["began"].as_bool(), job(&store, 8).get("began")), (Some(8), Some(true), None));
    // Job 9 ends on job 8's machine, which is ended with it, twenty seconds in.
    clock.0.set(clock.0.get() + 20_000);
    run(concluded(9, &runner(8), "success"));
    let ended = job(&store, 8);
    assert_eq!((ended["state"].as_str(), ended["machine_id"].as_str(), ended["ended_ms"].as_u64()), (Some("done"), Some(runner(8).as_str()), Some(1_790_000_020_000)));
    // Job 7's machine comes up and takes job 8, while job 7 still runs on job 9's. Job 7 needs no machine: none is
    // started (one was, and it idled until the sweep called job 7 failed).
    run(job_event("in_progress", 8, "acme/app", Some(&runner(7))));
    assert_eq!(starts(), 3);
    let (seven, eight) = (job(&store, 7), job(&store, 8));
    assert_eq!((eight["state"].as_str(), eight["runner"].as_str(), eight["machine_id"].as_str(), eight.get("ended_ms").filter(|e| !e.is_null())), (Some("running"), Some(runner(7).as_str()), Some(runner(7).as_str()), None));
    // The machine that ended with job 9's work is kept where there is room for it: with job 7, which is done.
    assert_eq!((seven["state"].as_str(), seven["machine_id"].as_str(), seven["ended_ms"].as_u64(), seven.get("for_job")), (Some("done"), Some(runner(8).as_str()), Some(1_790_000_020_000), None));
    // The rest end where they ran; nothing more is started, and nothing is left for the sweep to end.
    run(concluded(7, &runner(9), "success"));
    run(concluded(8, &runner(7), "success"));
    clock.0.set(clock.0.get() + 10 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(([7, 8, 9].map(|id| job(&store, id)["state"].as_str().map(str::to_string)), starts()), ([(); 3].map(|_| Some("done".to_string())), 3));
    assert_eq!(own.0.borrow().iter().filter(|c| c.starts_with("stop")).count(), 3, "{:?}", own.0.borrow());
}

#[test]
fn a_machine_nobody_waits_for_is_ended_at_the_first_sweep_after_it_had_time_to_come_up() {
    // What GitHub says of each job and of job 7's runner (5), changed as the test goes.
    let said: RefCell<BTreeMap<u64, serde_json::Value>> = RefCell::default();
    let busy = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        if let (Some(id), "GET") = (r.url.strip_prefix("https://api.github.com/orgs/acme/actions/runners/").and_then(|i| i.parse::<u64>().ok()), r.method.as_str()) { return Some(Response::new(200, "application/json", serde_json::json!({ "id": id, "busy": busy.get() }).to_string())) }
        let id: u64 = r.url.strip_prefix("https://api.github.com/repos/acme/app/actions/jobs/")?.parse().ok()?;
        Some(Response::new(200, "application/json", said.borrow().get(&id).cloned().unwrap_or(serde_json::json!({ "status": "queued" })).to_string()))
    });
    let (store, wakes, cache, own) = (Mem::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let sweep_after = |ms: u64| { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap() };
    let (starts, stops) = (|| own.0.borrow().iter().filter(|c| c.starts_with("start")).count(), || own.0.borrow().iter().filter(|c| c.starts_with("stop")).count());
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    let says = |id: u64, v: serde_json::Value| { said.borrow_mut().insert(id, v); };

    // Job 8's runner takes job 7; job 8 is cancelled before any runner takes it. Job 7's machine, kept for job 8,
    // has nothing to wait for: ended a minute in, not after three, and job 7 (running elsewhere) has not failed.
    run(job_event("queued", 7, "acme/app", None));
    run(job_event("queued", 8, "acme/app", None));
    run(job_event("in_progress", 7, "acme/app", Some(&runner(8))));
    run(job_event("completed", 8, "acme/app", None));
    says(8, serde_json::json!({ "status": "completed" }));
    sweep_after(30_000);
    assert_eq!((job(&store, 7)["state"].as_str(), stops()), (Some("launched"), 0), "not before it had time to come up");
    sweep_after(31_000);
    assert_eq!((job(&store, 7)["state"].as_str(), job(&store, 7).get("for_job"), stops(), starts()), (Some("done"), None, 1, 2));
    assert!(own.0.borrow().contains(&format!("stop {}", runner(7))));

    // A machine whose job GitHub has on it, the event lost: found running, as if told.
    run(job_event("queued", 17, "acme/app", None));
    says(17, serde_json::json!({ "status": "in_progress", "runner_name": runner(17) }));
    sweep_after(61_000);
    assert_eq!((job(&store, 17)["state"].as_str(), job(&store, 17)["seen_in_progress"].as_bool(), stops()), (Some("running"), Some(true), 1));
    run(concluded(17, &runner(17), "success"));

    // A machine GitHub says is busy, whatever job it took: not ended either.
    run(job_event("queued", 27, "acme/app", None));
    says(27, serde_json::json!({ "status": "in_progress", "runner_name": "someone-elses" }));
    busy.set(true);
    let before = stops();
    sweep_after(61_000);
    assert_eq!((job(&store, 27)["state"].as_str(), stops()), (Some("running"), before));
    busy.set(false);
    run(concluded(27, &runner(27), "success"));

    // Three jobs pass their runners around and what said which machine is for which is not enough: job 37's machine
    // is kept for job 38, job 38 runs on job 39's runner, and job 39 waits with no machine but job 37's. That one is
    // not ended: it is for job 39 now, and when it never comes up another is started.
    for id in [37, 38, 39] { run(job_event("queued", id, "acme/app", None)); }
    run(job_event("in_progress", 37, "acme/app", Some(&runner(38))));
    run(job_event("in_progress", 38, "acme/app", Some(&runner(39))));
    says(38, serde_json::json!({ "status": "in_progress", "runner_name": runner(39) }));
    let (before, started) = (stops(), starts());
    sweep_after(61_000);
    assert_eq!((job(&store, 37)["state"].as_str(), job(&store, 37)["for_job"].as_u64(), stops(), starts()), (Some("launched"), Some(39), before, started));
    sweep_after(3 * 60_000);
    assert_eq!((job(&store, 37)["state"].as_str(), job(&store, 37)["retries"].as_u64(), starts()), (Some("launched"), Some(1), started + 1));
    let again = job(&store, 37)["runner"].as_str().unwrap().to_string();
    run(job_event("in_progress", 39, "acme/app", Some(&again)));
    assert_eq!((job(&store, 37)["state"].as_str(), job(&store, 39)["began"].as_bool()), (Some("running"), Some(true)));

    // What was told of a job can be stale (a record from before this was kept, an event lost): GitHub is asked before
    // a machine is kept for it. Job 47 ended long ago and its record does not say so; job 48 was cancelled untold.
    run(job_event("queued", 47, "acme/app", None));
    let mut old = job(&store, 47);
    old["state"] = "done".into();
    store.0.borrow_mut().insert("job:47".into(), old.to_string());
    run(job_event("queued", 48, "acme/app", None));
    says(47, serde_json::json!({ "status": "completed" }));
    says(48, serde_json::json!({ "status": "completed" }));
    let (before, started) = (stops(), starts());
    sweep_after(61_000);
    assert_eq!((job(&store, 48)["state"].as_str(), job(&store, 47)["began"].as_bool(), stops(), starts()), (Some("swept"), Some(true), before + 1, started));
}

#[test]
fn an_aws_machines_disk_is_as_fast_as_its_size_deserves_unless_said_and_a_label_can_ask_for_memory_or_a_local_disk() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |id: u64, label: &str| { block_on(plane.handle(labelled("queued", id, "acme/app", None, label))); launches(&clouds).pop().unwrap() };
    let speed = |l: &[(String, String)]| (field(l, "BlockDeviceMapping.1.Ebs.Throughput").to_string(), field(l, "BlockDeviceMapping.1.Ebs.Iops").to_string());
    let s = |a: &str, b: &str| (a.to_string(), b.to_string());
    // By the machine's size: 31.25 MiB a second for each CPU, from what AWS includes (the standard machine's) up to
    // 1,000; IOPS as the speed needs.
    assert_eq!(speed(&run(1, "superci/cpu=8")), s("250", "3000"));
    assert_eq!(speed(&run(2, "superci")), s("125", "3000"));
    assert_eq!(speed(&run(3, "superci/cpu=16")), s("500", "3000"));
    assert_eq!(speed(&run(4, "superci/cpu=32")), s("1000", "4000"));
    // It is kept with the job when above what AWS includes, and is in the job's price: 250 MiB a second adds the
    // price of 62.5 GB of disk.
    assert_eq!((job(&store, 1)["disk_mibs"].as_u64(), job(&store, 1).get("disk_iops"), job(&store, 2).get("disk_mibs"), job(&store, 4)["disk_iops"].as_u64()), (Some(250), None, None, Some(4000)));
    let fast = job(&store, 1)["usd_per_hour"].as_f64().unwrap();
    // As a label says, within what a volume takes.
    assert_eq!(speed(&run(5, "superci/cpu=8/disk-speed=125")), s("125", "3000"));
    let plain = job(&store, 5)["usd_per_hour"].as_f64().unwrap();
    assert!((fast - plain - 125.0 / 2.0 * 0.08 / 730.0).abs() < 1e-6, "{fast} {plain}");
    assert_eq!(speed(&run(6, "superci/disk=200/disk-speed=2000/disk-iops=20000")), s("2000", "20000"));
    assert_eq!(speed(&run(7, "superci/disk-speed=750")), s("750", "3000"));
    // A job's files in memory, or on a machine's own NVMe disk (a machine type that has one), when its label asks.
    let ram = run(8, "superci/cpu=8/scratch=ramdisk");
    assert!(user_data_of(&ram).contains("mount -t tmpfs -o size=75% tmpfs") && field(&ram, "InstanceType") == "m8a.2xlarge");
    // (Its disk keeps what AWS includes: the job's writes go to memory. A speed said in the label is still given.)
    assert_eq!((speed(&ram), job(&store, 8)["ramdisk"].as_bool(), job(&store, 8).get("disk_mibs"), job(&store, 3).get("ramdisk")), (s("125", "3000"), Some(true), None, None));
    assert_eq!(speed(&run(13, "superci/cpu=8/disk-speed=500/scratch=ramdisk")), s("500", "3000"));
    let local = run(9, "superci/cpu=8/scratch=localdisk");
    assert!(user_data_of(&local).contains("/Instance Storage/") && !user_data_of(&local).contains("tmpfs") && field(&local, "InstanceType") == "m8id.2xlarge", "{}", field(&local, "InstanceType"));
    assert_eq!((speed(&local), job(&store, 9)["local_disk"].as_bool()), (s("125", "3000"), Some(true)));
    // A machine type that has a disk of its own uses it without being asked (a label naming such a family), and its
    // job says so; on a machine without one the same lines find none, and nothing is mounted.
    let pinned = run(10, "superci/cpu=8/family=m8id");
    assert!(field(&pinned, "InstanceType") == "m8id.2xlarge" && user_data_of(&pinned).contains("/Instance Storage/") && job(&store, 10)["local_disk"] == true);
    let plain = run(14, "superci/cpu=8");
    assert!(user_data_of(&plain).contains("/Instance Storage/") && !user_data_of(&plain).contains("tmpfs") && job(&store, 14).get("local_disk").is_none());
    // The default machine can say the speed for every job: what AWS includes, at no extra price.
    config.machine = superci_core::spec::Spec { disk_mibs: Some(125), ..Default::default() };
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 11, "acme/app", None, "superci/cpu=8")));
    assert_eq!(speed(&launches(&clouds).pop().unwrap()), s("125", "3000"));
    block_on(plane.handle(labelled("queued", 12, "acme/app", None, "superci/cpu=8/disk-speed=500")));
    assert_eq!(speed(&launches(&clouds).pop().unwrap()), s("500", "3000"), "a label says otherwise for its job");
}

#[test]
fn a_spot_machine_is_asked_for_where_it_costs_least_for_its_speed_and_never_where_its_type_is_not_offered() {
    // A day's real picture, 32 CPUs: the newest type is the cheapest and has no machine anywhere; the older
    // memory-heavy type is cheaper than the older standard one; no type is offered in zone e. Every request is refused.
    let prices = [("m8a.8xlarge", "us-east-1a", 0.60), ("m8a.8xlarge", "us-east-1b", 0.80), ("m7a.8xlarge", "us-east-1a", 0.70), ("m7a.8xlarge", "us-east-1b", 1.10),
        ("r8a.8xlarge", "us-east-1a", 0.85), ("r7a.8xlarge", "us-east-1a", 0.64), ("r7a.8xlarge", "us-east-1b", 0.66)];
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeVpcs") { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into()) }
        if body.contains("Action=DescribeSubnets") { return xml(format!("<subnetSet>{}</subnetSet>", ["a", "b", "e"].map(|z| format!("<item><subnetId>subnet-{z}</subnetId><availabilityZone>us-east-1{z}</availabilityZone></item>")).concat())) }
        if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>".into()) }
        if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") {
            return xml(format!("<spotPriceHistorySet>{}</spotPriceHistorySet>", prices.iter().map(|(t, z, p)| format!("<item><instanceType>{t}</instanceType><availabilityZone>{z}</availabilityZone><spotPrice>{p}</spotPrice></item>")).collect::<String>()))
        }
        (body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity"))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.max_cpu = Some(64);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/cpu=32")));
    let asked: Vec<(String, String)> = launches(&clouds.0).iter().filter(|l| field(l, "InstanceMarketOptions.MarketType") == "spot").map(|l| (field(l, "InstanceType").to_string(), field(l, "NetworkInterface.1.SubnetId").to_string())).collect();
    // By the price for the speed: the newest at $0.60 is as $0.50 of the older; the newest in its dearer zone ($0.80,
    // as $0.667) before the older one at $0.70. Nothing in zone e. The memory-heavy types are not asked: the general
    // ones have the memory asked for with as many CPUs.
    assert_eq!(asked[..4], [("m8a.8xlarge", "subnet-a"), ("m8a.8xlarge", "subnet-b"), ("m7a.8xlarge", "subnet-a"), ("m7a.8xlarge", "subnet-b")]
        .map(|(t, s)| (t.to_string(), s.to_string())), "{asked:?}");
    assert!(!asked.iter().any(|(t, _)| t.starts_with('r')), "{asked:?}");
    // (After them, the larger types that would also fit, of which no price was known: in their own order.)
    assert!(asked.iter().all(|(t, subnet)| subnet != "subnet-e" || !prices.iter().any(|(p, ..)| p == t)), "{asked:?}");
}

#[test]
fn every_spot_pool_is_asked_before_an_on_demand_machine_and_none_twice_in_a_minute() {
    // Three families in five zones: fifteen spot pools, and only the fourteenth asked has a machine. One request to
    // the control plane asks twelve (a runtime's time is short).
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeVpcs") { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into()) }
        if body.contains("Action=DescribeSubnets") { return xml(format!("<subnetSet>{}</subnetSet>", ["a", "b", "c", "d", "e"].map(|z| format!("<item><subnetId>subnet-{z}</subnetId><availabilityZone>us-east-1{z}</availabilityZone></item>")).concat())) }
        if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>".into()) }
        if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") { return xml("<spotPriceHistorySet/>".into()) }
        let has = body.contains("InstanceType=m7a.8xlarge") && body.contains("SubnetId=subnet-d");
        (body.contains("Action=RunInstances") && body.contains("MarketType=spot") && !has).then(|| aws_refuses("InsufficientInstanceCapacity"))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/cpu=32/family=m8a+r8a+m7a")));
    let asked = || launches(&clouds.0).iter().map(|l| (field(l, "InstanceType").to_string(), field(l, "NetworkInterface.1.SubnetId").to_string(), field(l, "InstanceMarketOptions.MarketType").to_string())).collect::<Vec<_>>();
    // Twelve asked, all on spot, none with a machine; three are left. No on-demand machine yet: the job goes on with
    // the rest in a second, and that is no start that failed.
    let first = asked();
    assert!(first.len() == 12 && first.iter().all(|(_, _, market)| market == "spot"), "{first:?}");
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["error"].as_str(), j["spot_rounds"].as_u64(), j.get("retries").and_then(|r| r.as_u64()).unwrap_or(0)), (Some("waiting"), Some("waiting: asking AWS's other spot pools for a machine"), Some(1), 0), "{j}");
    assert!(wakes.0.borrow().contains(&1_000));
    // The rest: the three not asked yet and no other (those that had none a moment ago are not asked again). The
    // second of them has a machine, on spot.
    block_on(plane.alarm()).unwrap();
    let all = asked();
    assert_eq!(all[12..], [("m7a.8xlarge", "subnet-c", "spot"), ("m7a.8xlarge", "subnet-d", "spot")].map(|(t, s, m)| (t.to_string(), s.to_string(), m.to_string())), "{all:?}");
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["machine_type"].as_str(), j.get("on_demand"), j.get("spot_rounds")), (Some("launched"), Some("m7a.8xlarge"), None, None), "{j}");
    // Another job queued beside it asks none of the pools that had no machine a moment ago: it starts with the one that has.
    block_on(plane.handle(labelled("queued", 8, "acme/app", None, "superci/cpu=32/family=m8a+r8a+m7a")));
    let all = asked();
    assert_eq!((all.len(), all[14].0.as_str(), all[14].1.as_str()), (15, "m7a.8xlarge", "subnet-d"), "{all:?}");
}

#[test]
fn a_machine_for_a_job_cancelled_before_any_runner_took_it_is_ended_at_once() {
    // GitHub lets a runner be taken back unless it is running a job (`busy`).
    let busy = std::cell::Cell::new(false);
    let store = Mem::default();
    // (While job 9's machine is being asked for, its job is cancelled: the event arrives in another request.)
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
        if r.method == "DELETE" && r.url.contains("/actions/runners/") { return Some(if busy.get() { Response::new(422, "application/json", r#"{"message":"Runner is still running a job"}"#.to_string()) } else { Response::new(204, "application/json", String::new()) }) }
        if body.contains("Action=RunInstances") && store.0.borrow().get("job:9").is_some_and(|j| j.contains(r#""state":"launching""#)) { store.0.borrow_mut().insert("gone:9".into(), "1790000000000".into()); }
        None
    });
    let (wakes, cache) = (Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let ended = || clouds.0.calls.borrow().iter().filter(|c| c.2.contains("Action=TerminateInstances")).count();
    let withdrawn = || clouds.0.calls.borrow().iter().filter(|c| c.0 == "DELETE" && c.1.contains("/actions/runners/")).count();

    // Its machine is up and idle when the job is cancelled: the runner is taken back from GitHub and the machine
    // ended in the same breath, not at a sweep a minute or two on.
    run(job_event("queued", 7, "acme/app", None));
    assert_eq!((job(&store, 7)["state"].as_str(), ended()), (Some("launched"), 0));
    run(job_event("completed", 7, "acme/app", None));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["began"].as_bool(), ended(), withdrawn(), j["ended_ms"].is_u64()), (Some("cancelled"), Some(true), 1, 1, true), "{j}");
    // GitHub will not give the runner back (it was handed another job a moment ago): the machine stays, as before.
    busy.set(true);
    run(job_event("queued", 8, "acme/app", None));
    run(job_event("completed", 8, "acme/app", None));
    assert_eq!((job(&store, 8)["state"].as_str(), ended()), (Some("orphan"), 1));
    busy.set(false);
    // Cancelled while its machine was still being asked for: the machine AWS has just given is ended at once.
    run(job_event("queued", 9, "acme/app", None));
    let j = job(&store, 9);
    assert_eq!((j["state"].as_str(), j["began"].as_bool(), j["machine_id"].as_str(), ended()), (Some("cancelled"), Some(true), Some("i-0abc"), 2), "{j}");
    assert!(!store.0.borrow().contains_key("gone:9"));
    // Cancelled while it waits (for room, or between two rounds of asking for a spot machine): cancelled, and noted,
    // so that a sweep about to start it from what it listed a moment ago starts nothing.
    store.0.borrow_mut().insert("job:11".into(), serde_json::json!({ "job_id": 11, "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": 1_790_000_000_000u64, "installation_id": 42, "cloud": "aws", "seen_in_progress": false, "label": "superci" }).to_string());
    run(job_event("completed", 11, "acme/app", None));
    assert!(job(&store, 11)["state"] == "cancelled" && store.0.borrow().contains_key("gone:11"));
    // And one whose cancelling was known before anything was asked of AWS gets no machine at all.
    store.0.borrow_mut().insert("gone:10".into(), "1790000000000".into());
    let before = launches(&clouds.0).len();
    run(job_event("queued", 10, "acme/app", None));
    assert_eq!((job(&store, 10)["state"].as_str(), job(&store, 10).get("machine_id").filter(|m| !m.is_null()), launches(&clouds.0).len()), (Some("cancelled"), None, before));
}

#[test]
fn a_jobs_start_is_kept_what_aws_was_asked_and_what_its_machine_said_of_itself() {
    // AWS has no spot machine of the first type in any zone; the second type starts.
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (body.contains("Action=RunInstances") && body.contains("InstanceType=c8a.xlarge")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    (config.aws_own, config.aws_own_creds) = own_aws();
    // (The types it had then: the second, c7a, starts.)
    config.instance_types = ["c8a.xlarge", "c7a.xlarge", "m7a.xlarge"].map(String::from).to_vec();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    store.0.borrow_mut().insert("plane_url".into(), format!("\"{PLANE_URL}\""));
    // GitHub made the job four seconds before this control plane heard of it.
    let payload = serde_json::json!({ "action": "queued", "workflow_job": { "id": 7, "run_id": 3, "labels": ["superci"], "runner_name": null, "created_at": "2026-09-21T14:13:16Z" }, "repository": { "full_name": "acme/app", "private": true }, "installation": { "id": 42 } }).to_string();
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
    run(Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["queued_ms"].as_u64(), j["machine_type"].as_str()), (Some("launched"), Some(1_789_999_996_000), Some("c7a.xlarge")), "{j}");
    // Every request for its machine, in order, with what AWS said: the first type refused, the next started.
    let asks: Vec<(String, String, bool)> = j["asks"].as_array().unwrap().iter().map(|a| (a["kind"].as_str().unwrap().to_string(), a["said"].as_str().unwrap().to_string(), a["spot"].as_bool().unwrap())).collect();
    assert!(asks.len() >= 2 && asks[..asks.len() - 1].iter().all(|a| *a == ("c8a.xlarge".to_string(), "InsufficientInstanceCapacity".to_string(), true)) && asks[asks.len() - 1] == ("c7a.xlarge".to_string(), "started".to_string(), true), "{asks:?}");
    // Its machine has a link of its own to say how its start went, in its start-up script.
    let token = j["hello"].as_str().unwrap().to_string();
    let script = user_data_of(&launches(&clouds.0).pop().unwrap());
    assert!(script.contains(&format!(r#"curl -s -m 5 -X POST "{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={token}&boot=$superci_up&script=$superci_t0"#)), "{script}");
    // Only with its link; and only times near now are taken.
    let hello = |t: &str, more: &str| run(Request::new("POST", &format!("{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={t}{more}"))).status;
    assert_eq!((hello("wrong-wrong-wrong-wrong", "&ready=1790000009000"), hello(&token, "&boot=6.61&script=1790000008000&read=1790000009000&ready=1790000009500")), (404, 200));
    clock.0.set(1_790_000_012_000);
    assert_eq!(hello(&token, "&listening=1790000011000&ready=5"), 200);
    // The status gives it with the job (kept apart from the job's own record, which a machine never writes), and not the link.
    let status = json(&run(Request::new("GET", &format!("{PLANE_URL}/status")).with_header("authorization", "Bearer dashboard-session-key-1")));
    let seen = &status["jobs"][0];
    assert_eq!(seen["machine"], serde_json::json!({ "boot_s": 6.61, "script_ms": 1_790_000_008_000u64, "read_ms": 1_790_000_009_000u64, "ready_ms": 1_790_000_009_500u64, "listening_ms": 1_790_000_011_000u64 }), "{seen}");
    assert!(seen.get("hello").is_none() && seen["asks"].as_array().is_some_and(|a| a.len() == asks.len()) && job(&store, 7).get("machine").is_none());
    // A runner slow to reach GitHub: its machine sends what the runner logged meanwhile (its waits, warnings and
    // errors), which is kept with the rest, as text and no more of it than a page shows. Only with its link.
    assert!(script.contains(r#"--data-binary @- "https://plane.acme.workers.dev/hello?runner="#) && script.contains("&log=runner") && script.contains("/home/runner/_diag/Runner_*.log"), "{script}");
    let log = "[2026-10-13 22:56:11Z INFO ConfigurationStore] HasCredentials()\n... 30 s ...\n[2026-10-13 22:56:41Z WARN GitHubActionsService] Attempt 1 of POST request failed (Socket Error: TryAgain).\u{7}\n";
    let send = |t: &str, body: String| run(Request::new("POST", &format!("{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={t}&log=runner")).with_body(body)).status;
    assert_eq!((send("wrong-wrong-wrong-wrong", log.to_string()), send(&token, format!("{log}{}", "x".repeat(40_000)))), (404, 200));
    let kept: serde_json::Value = serde_json::from_str(&store.0.borrow()["hello:7"]).unwrap();
    // What its runner writes of each step, sent as it is written (GitHub gives a job's log once it has ended): its
    // pages, each in pieces, read back in order as the job's log while GitHub has none. Only with its link.
    let page = |t: &str, file: &str, at: u64, body: &str| run(Request::new("POST", &format!("{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={t}&live={file}&at={at}")).with_body(body.to_string())).status;
    let (setup, tests) = ("aaaa_1111_1.log", "aaaa_2222_1.log");
    assert_eq!(page("wrong-wrong-wrong-wrong", setup, 0, "x"), 404);
    assert_eq!(page(&token, tests, 0, "2026-10-15T10:00:05.0000000Z ##[group]Run npm test\n"), 200);
    assert_eq!(page(&token, setup, 0, "2026-10-15T10:00:01.0000000Z Current runner version: '2.337.0'\n"), 200);
    assert_eq!(page(&token, tests, 55, "2026-10-15T10:00:06.0000000Z ok 1 - adds\n"), 200);
    assert_eq!(page(&token, "../escape", 0, "x"), 200, "a name that is not a page's is let be");
    assert_eq!(page(&token, "aaaa_0000_1.log", 0, "2026-10-15T10:00:01.0000010Z Current runner version: '2.337.0'\n2026-10-15T10:00:05.0000010Z ##[group]Run npm test\n2026-10-15T10:00:06.0000010Z ok 1 - adds\n"), 200, "the job's own page: every step's lines again, a moment later");
    let got = json(&run(Request::new("GET", &format!("{PLANE_URL}/job/log?id=7")).with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!(got["log"].as_str().unwrap_or_else(|| panic!("{got}")), "2026-10-15T10:00:01.0000000Z Current runner version: '2.337.0'\n2026-10-15T10:00:05.0000000Z ##[group]Run npm test\n2026-10-15T10:00:06.0000000Z ok 1 - adds\n");
    // The start of its runner's own log, once its job began: the job message in it gives its steps' shape (only with
    // its link), as the job's log says it.
    let message = include_str!("fixtures/runner/parallel.github.json");
    let worker = format!("[2026-10-15 10:00:02Z INFO Worker] Waiting to receive the job message from the channel.\n[2026-10-15 10:00:02Z INFO Worker] Job message:\n {message}\n[2026-10-15 10:00:02Z INFO Worker] Starting the job {{\n");
    let sent = |t: &str| run(Request::new("POST", &format!("{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={t}&worker=1")).with_body(worker.clone())).status;
    assert_eq!((sent("wrong-wrong-wrong-wrong"), sent(&token)), (404, 200));
    // A start of a log with no job message in it: taken, and nothing made of it. A page with nothing in it, or
    // named as no page is, is no page: nothing kept.
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/hello?runner=superci-{PLANE_ID}-7&t={token}&worker=1")).with_body("[2026-10-15 10:00:02Z INFO Worker] Waiting\n")).status, 200);
    assert_eq!(page(&token, "aaaa_3333_1.log", 0, ""), 200);
    assert_eq!(page(&token, &format!("{}.log", "a".repeat(120)), 0, "x"), 200);
    let got = json(&run(Request::new("GET", &format!("{PLANE_URL}/job/log?id=7&steps=1")).with_header("authorization", "Bearer dashboard-session-key-1")));
    let shape = got["shape"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert_eq!((shape.len(), &shape[0]["id"], &shape[0]["bg"], &shape[11]["control"], &shape[11]["group"]), (14, &serde_json::json!("server"), &serde_json::json!(true), &serde_json::json!("wait"), &serde_json::json!("__parallel_0")));
    assert_eq!(shape[11]["targets"], serde_json::json!(["__parallel_0_0", "second", "__parallel_0_2"]));
    let text = kept["log"].as_str().unwrap();
    assert!(text.starts_with("[2026-10-13 22:56:11Z INFO ConfigurationStore] HasCredentials()\n... 30 s ...\n[2026-10-13 22:56:41Z WARN") && !text.contains('\u{7}') && text.len() < 16_100 && text.lines().all(|l| l.len() <= 300), "{text}");
    assert_eq!(kept["listening_ms"], 1_790_000_011_000u64, "what it said before stays");
}

#[test]
fn a_job_waits_for_a_spot_machine_only_when_on_demand_was_refused_for_the_limit_alone_and_spot_was_asked() {
    // Spot has no machine; on-demand is refused for `on_demand` (the account's limit, or AWS's capacity).
    for (label, on_demand, waits) in [("superci", "VcpuLimitExceeded", true), ("superci", "InsufficientInstanceCapacity", false), ("superci/spot=false", "VcpuLimitExceeded", false)] {
        let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
            if !body.contains("Action=RunInstances") { return None }
            Some(aws_refuses(if body.contains("MarketType=spot") { "InsufficientInstanceCapacity" } else { on_demand }))
        });
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(labelled("queued", 9, "acme/app", None, label)));
        let e = job(&store, 9)["error"].as_str().unwrap_or_default().to_string();
        assert_eq!(e.starts_with("waiting: AWS has no spot machine for it now, and your AWS account's on-demand limit"), waits, "{label}, on-demand {on_demand}: {e}");
    }
}

#[test]
fn a_job_run_again_after_a_reclaim_goes_back_on_spot_when_the_on_demand_limit_cannot_take_it() {
    // An account whose on-demand limit is below what the job asks (AWS refuses every on-demand machine for it), with
    // spot machines to be had unless `short`; while `full`, the spot limit is used up too.
    let (short, full) = (std::cell::Cell::new(false), std::cell::Cell::new(false));
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| {
        if !body.contains("Action=RunInstances") { return None }
        if !body.contains("MarketType=spot") { return Some(aws_refuses("VcpuLimitExceeded")) }
        if full.get() { return Some(aws_refuses("MaxSpotInstanceCountExceeded")) }
        short.get().then(|| aws_refuses("InsufficientInstanceCapacity"))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let sweep_after = |ms: u64| { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap() };
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    let market = |n: usize| field(&launches(&clouds.0)[n], "InstanceMarketOptions.MarketType").to_string();

    // The new attempt of a job whose spot machine AWS took back is kept off spot machines. Its on-demand machine is
    // refused for the account's limit: it is asked on a spot machine again, at once, and says so.
    store.0.borrow_mut().insert("rerun:acme/app:3:".into(), "1".into());
    run(job_event("queued", 8, "acme/app", None));
    let j = job(&store, 8);
    assert_eq!((j["state"].as_str(), j["after_reclaim"].as_bool(), j["spot_again"].as_bool(), j.get("no_spot"), j.get("retries").and_then(|r| r.as_u64()).unwrap_or(0)), (Some("waiting"), Some(true), Some(true), None, 0), "{j}");
    assert!(j["error"].as_str().unwrap().starts_with("waiting: an on-demand machine for it does not fit your AWS account's on-demand limit; asking for a spot machine again (us-east-1 on-demand: VcpuLimitExceeded"), "{}", j["error"]);
    assert_eq!((launches(&clouds.0).len(), market(0).as_str()), (1, ""), "only an on-demand machine was asked for");
    assert!(wakes.0.borrow().contains(&2_000), "tried again within seconds: {:?}", wakes.0.borrow());
    sweep_after(2_000);
    let j = job(&store, 8);
    assert_eq!((j["state"].as_str(), j["machine_id"].as_str(), j.get("on_demand"), market(1).as_str()), (Some("launched"), Some("i-0abc"), None, "spot"), "{j}");
    assert!(j.get("spot_refused").is_none(), "what on-demand said is not kept as spot's refusal: {j}");

    // A job with no spot machine to be had now, whose on-demand machine the limit refuses: it waits for a spot
    // machine, however long, and is not counted as a start that failed.
    short.set(true);
    run(job_event("queued", 9, "acme/app", None));
    for _ in 0..4 { sweep_after(30_000); }
    let j = job(&store, 9);
    assert_eq!((j["state"].as_str(), j.get("retries").and_then(|r| r.as_u64()).unwrap_or(0)), (Some("waiting"), 0), "{j}");
    assert!(j["error"].as_str().unwrap().starts_with("waiting: AWS has no spot machine for it now, and your AWS account's on-demand limit does not let an on-demand one start (us-east-1: InsufficientInstanceCapacity"), "{}", j["error"]);
    short.set(false);
    sweep_after(30_000);
    assert_eq!((job(&store, 9)["state"].as_str(), job(&store, 9).get("on_demand")), (Some("launched"), None));

    // Both limits used up, and the only machines of its own ended a moment ago: AWS counts one a little longer than
    // it runs. That is what is said (nothing is promised to start "when one ends"), and it is tried again.
    for id in [8, 9] { run(job_event("in_progress", id, "acme/app", Some(&runner(id)))); run(concluded(id, &runner(id), "success")); }
    full.set(true);
    run(job_event("queued", 10, "acme/app", None));
    let j = job(&store, 10);
    assert_eq!((j["state"].as_str(), j.get("retries").and_then(|r| r.as_u64()).unwrap_or(0)), (Some("waiting"), 0), "{j}");
    assert!(j["error"].as_str().unwrap().starts_with("waiting: your AWS spot limit is reached while machines of SuperCI's that just ended still count toward it"), "{}", j["error"]);
    // Two minutes on, nothing of its own counts any more: the limits themselves are what refuses it.
    sweep_after(130_000);
    assert!(job(&store, 10)["error"].as_str().unwrap().starts_with("waiting: starting its machine failed, trying again (Your AWS account's limits do not let this machine start"), "{}", job(&store, 10)["error"]);

    // Asked on a spot machine again already, its label asking for no spot machine: the limits are what is said (job
    // 12's machine ended a moment ago: AWS still counts it).
    full.set(false);
    store.0.borrow_mut().insert("job:12".into(), serde_json::json!({ "job_id": 12, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": clock.0.get(), "installation_id": 42, "cloud": "aws",
        "machine_id": "i-12", "seen_in_progress": true, "ended_ms": clock.0.get() + 20_000 }).to_string());
    store.0.borrow_mut().insert("live:12".into(), String::new());
    // (Job 10, still up, out of the way.)
    store.0.borrow_mut().remove("live:10");
    store.0.borrow_mut().insert("job:11".into(), serde_json::json!({ "job_id": 11, "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": clock.0.get(), "installation_id": 42, "app_id": 99,
        "cloud": "", "seen_in_progress": false, "label": "superci/spot=false", "spot_again": true }).to_string());
    store.0.borrow_mut().insert("live:11".into(), String::new());
    sweep_after(30_000);
    assert!(job(&store, 11)["error"].as_str().is_some_and(|e| e.contains("AWS still counts a machine")), "{}", job(&store, 11));
}

#[test]
fn runs_are_counted_once_each_and_those_of_jobs_counted_before_are_counted_by_the_upkeep() {
    let (store, wakes, cache, clouds) = (Mem::default(), Wakes::default(), Cache::default(), FakeClouds::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let runs = || store.0.borrow().iter().filter(|(k, _)| k.starts_with("day:")).map(|(_, v)| serde_json::from_str::<serde_json::Value>(v).unwrap()["hours"].as_array().unwrap().iter().map(|h| h["runs"].as_u64().unwrap_or(0)).sum::<u64>()).sum::<u64>();
    // Three jobs: two of acme/app's run, one of acme/site's.
    for (id, repo) in [(31, "acme/app"), (32, "acme/app"), (33, "acme/site")] {
        let runner = format!("superci-{PLANE_ID}-{id}");
        run(job_event("queued", id, repo, None));
        run(job_event("in_progress", id, repo, Some(&runner)));
        run(job_event("completed", id, repo, Some(&runner)));
    }
    assert_eq!(runs(), 2, "each run once");
    // As a control plane before runs were counted left them: its jobs counted, its runs not.
    store.0.borrow_mut().retain(|k, _| !k.starts_with("counted-run:"));
    for (_, v) in store.0.borrow_mut().iter_mut().filter(|(k, _)| k.starts_with("day:")) {
        let mut d: serde_json::Value = serde_json::from_str(v).unwrap();
        for h in d["hours"].as_array_mut().unwrap() { h["runs"] = 0.into() }
        *v = d.to_string();
    }
    assert_eq!(runs(), 0);
    // The upkeep counts them, once (a second sweep adds nothing).
    clock.0.set(clock.0.get() + 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(runs(), 2);
    clock.0.set(clock.0.get() + 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(runs(), 2);
}

#[test]
fn a_job_sent_on_demand_by_the_spot_limit_notes_when_machines_just_ended_still_counted() {
    // Spot machines until `full` (the account's spot limit used up); on-demand ones always.
    let full = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (full.get() && body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("MaxSpotInstanceCountExceeded")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");
    // A spot machine's job ends; a moment later the spot limit refuses the next one: it waits on spot a while (the
    // machine that ended still counts until it has shut down), not on-demand at once.
    run(job_event("queued", 21, "acme/app", None));
    assert_eq!(job(&store, 21).get("on_demand"), None);
    run(job_event("in_progress", 21, "acme/app", Some(&runner(21))));
    run(concluded(21, &runner(21), "success"));
    full.set(true);
    clock.0.set(clock.0.get() + 10_000);
    run(job_event("queued", 22, "acme/app", None));
    let j = job(&store, 22);
    assert_eq!((j["state"].as_str(), j.get("on_demand"), j["spot_ending"].as_bool()), (Some("waiting"), None, Some(true)), "{j}");
    assert!(j["error"].as_str().unwrap().starts_with("waiting: your AWS spot limit is reached while machines of SuperCI's that just ended still count toward it"), "{j}");
    // Twenty seconds on, still at the limit: still waiting. The machine gone, the limit has room: a spot machine.
    clock.0.set(clock.0.get() + 20_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(job(&store, 22)["state"], "waiting");
    full.set(false);
    clock.0.set(clock.0.get() + 20_000);
    block_on(plane.alarm()).unwrap();
    let j = job(&store, 22);
    assert_eq!((j["state"].as_str(), j.get("on_demand"), j.get("spot_limit_wait"), j["spot_waited_ms"].as_u64()), (Some("launched"), None, None, Some(40_000)), "{j}");
    // At the limit for two minutes: on-demand then (as soon as nothing of its own ended within them).
    full.set(true);
    run(job_event("queued", 24, "acme/app", None));
    assert_eq!(job(&store, 24)["state"], "waiting");
    for _ in 0..7 { clock.0.set(clock.0.get() + 20_000); block_on(plane.alarm()).unwrap(); }
    let j = job(&store, 24);
    assert_eq!((j["state"].as_str(), j["on_demand"].as_bool()), (Some("launched"), Some(true)), "{j}");
    // Two minutes on, nothing of its own is still being ended: the limit alone refused it.
    clock.0.set(clock.0.get() + 130_000);
    run(job_event("queued", 23, "acme/app", None));
    let j = job(&store, 23);
    assert_eq!((j["on_demand"].as_bool(), j.get("spot_ending")), (Some(true), None), "{j}");
}

#[test]
fn at_the_aws_accounts_limit_a_job_waits_for_one_of_its_own_to_end_and_fails_plainly_when_it_can_never_fit() {
    // While `full`, AWS refuses every launch for the account's limits: spot for its spot limit, on-demand for its own.
    let full = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (full.get() && body.contains("Action=RunInstances")).then(|| aws_refuses(if body.contains("MarketType=spot") { "MaxSpotInstanceCountExceeded" } else { "VcpuLimitExceeded" })));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let sweep_after = |ms: u64| { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap() };
    let runner = |job: u64| format!("superci-{PLANE_ID}-{job}");

    // Nothing of this control plane's own runs there: the limit is below what the job asks, or something else in the
    // account uses it. Tried again twice like any start that failed, then it fails saying which limits to raise.
    run(job_event("queued", 7, "acme/app", None));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["retries"].as_u64()), (Some("waiting"), Some(1)));
    assert!(j["error"].as_str().unwrap().starts_with("waiting: starting its machine failed, trying again (Your AWS account's limits do not let this machine start"), "{}", j["error"]);
    sweep_after(20_000);
    sweep_after(20_000);
    let j = job(&store, 7);
    let why = j["error"].as_str().unwrap();
    assert_eq!(j["state"], "failed");
    assert!(why.starts_with("Your AWS account's limits do not let this machine start: it asks for 4 CPUs (us-east-1: MaxSpotInstanceCountExceeded; us-east-1 on-demand: VcpuLimitExceeded). Ask AWS for more in Service Quotas"), "{why}");
    assert!(why.contains("quotas/L-34B43A08") && why.contains("quotas/L-1216C47A"), "{why}");
    // The runner that says so on GitHub is asked for as a spot machine, then as an on-demand one (the spot limit may
    // be the very reason): a small machine either way.
    let failing: Vec<_> = launches(&clouds.0).into_iter().filter(|l| field(l, "InstanceType") == "t3a.small").collect();
    assert!(failing.iter().any(|l| field(l, "InstanceMarketOptions.MarketType") == "spot") && failing.iter().any(|l| field(l, "InstanceMarketOptions.MarketType").is_empty()), "{}", failing.len());

    // A job of its own is running there: the next one waits for it to end, as it would on GitHub's own runners. It
    // is not counted as a failed start, however long it waits.
    full.set(false);
    run(job_event("queued", 8, "acme/app", None));
    run(job_event("in_progress", 8, "acme/app", Some(&runner(8))));
    assert_eq!(job(&store, 8)["state"], "running");
    full.set(true);
    run(job_event("queued", 9, "acme/app", None));
    for _ in 0..4 { sweep_after(60_000); }
    let j = job(&store, 9);
    assert_eq!((j["state"].as_str(), j.get("retries").and_then(|r| r.as_u64()).unwrap_or(0)), (Some("waiting"), 0));
    assert!(j["error"].as_str().unwrap().starts_with("waiting: your AWS account's limit of running CPUs is in use by 1 of your job there; it starts when one ends (us-east-1: MaxSpotInstanceCountExceeded"), "{}", j["error"]);
    // The first ends and AWS has room again: the next sweep starts the one that waited.
    run(concluded(8, &runner(8), "success"));
    full.set(false);
    sweep_after(60_000);
    assert_eq!((job(&store, 9)["state"].as_str(), job(&store, 9)["machine_id"].as_str()), (Some("launched"), Some("i-0abc")));
}

/// AWS as a real account answered a request for one spot machine from any of several pools (an instant fleet), kept
/// as it was said (ids replaced): a machine in one pool; none anywhere; none, the account's limit having refused.
const FLEET_STARTED: &str = include_str!("fixtures/aws/fleet/started.xml");
const FLEET_NONE: &str = include_str!("fixtures/aws/fleet/none.xml");
const FLEET_LIMIT: &str = include_str!("fixtures/aws/fleet/limit.xml");
const FLEET_TEMPLATE: &str = include_str!("fixtures/aws/fleet/template.xml");
const FLEET_TEMPLATE_REMOVED: &str = include_str!("fixtures/aws/fleet/template-removed.xml");

/// A control plane's own network with a subnet in each of six zones, as the account the answers above came from has.
fn six_zones() -> superci_core::aws::Network {
    superci_core::aws::Network { vpc: "vpc-1".into(), security_group: "sg-1".into(), subnets: ["a", "b", "c", "d", "e", "f"].map(|z| (format!("us-east-1{z}"), format!("subnet-{z}"))).to_vec(), more_groups: vec![], private: false }
}

/// The fields of each call of an EC2 action, in order.
fn calls_of(clouds: &FakeClouds, action: &str) -> Vec<Vec<(String, String)>> {
    clouds.calls.borrow().iter().filter(|c| c.2.contains(&format!("Action={action}&")) || c.2.ends_with(&format!("Action={action}"))).map(|c| url::form_urlencoded::parse(c.2.as_bytes()).into_owned().collect()).collect()
}

#[test]
fn the_spot_pools_left_when_the_best_has_no_machine_are_asked_in_one_request() {
    use superci_core::aws::{run_instance, Launch, Told};
    let (_, creds) = own_aws();
    let creds = creds.unwrap();
    let network = six_zones();
    let tags = vec![("Name".to_string(), "acmeci app 7".to_string()), ("superci-plane".to_string(), PLANE_ID.to_string()), ("superci-job".to_string(), "7".to_string())];
    let ask = |types: &[&str], fleet_says: &'static str, fleet: bool| {
        let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
            let xml = |s: &str| Some(Response::new(200, "text/xml", s.to_string()));
            if body.contains("Action=DescribeSpotPriceHistory") { return xml("<spotPriceHistorySet/>") }
            if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
            if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE) }
            if body.contains("Action=CreateFleet") { return xml(fleet_says) }
            if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED) }
            None
        });
        let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        let launch = Launch { region: "us-east-1", network: Some(&network), image: "ami-new", types: &types, user_data: "#!/bin/bash\necho job\n", disk_gb: 120, disk_mibs: 1000, disk_iops: 4000, clock: Some(&FixedClock), skip: &[], tags: &tags,
            spot: true, os: "linux", avoid: &[], timer: None, jitter_ms: 0, fleet, store_disk: None };
        let mut told = Told::default();
        let machine = block_on(run_instance(&clouds, &creds, &launch, 1_790_000_000_000, &mut told));
        (machine.map(|m| (m.id, m.kind, m.zone)), told, clouds.0)
    };

    // 64 CPUs of three families in six zones: the best pool has no spot machine. The seventeen others are asked in
    // one request, and AWS gives one of them.
    let (machine, told, clouds) = ask(&["c8a.16xlarge", "m8a.16xlarge", "r8a.16xlarge"], FLEET_STARTED, true);
    assert_eq!(machine, Ok(("i-0123456789abcdef0".to_string(), "c8a.16xlarge".to_string(), "us-east-1b".to_string())));
    let one_by_one = launches(&clouds);
    assert_eq!((one_by_one.len(), field(&one_by_one[0], "InstanceType"), field(&one_by_one[0], "NetworkInterface.1.SubnetId")), (1, "c8a.16xlarge".to_string(), "subnet-a".to_string()), "the best pool alone is asked by itself");
    // The machine is what one asked for by itself would be: a launch template made for this one request says it,
    // tagged as the control plane's (its role may make and use no other).
    let template = &calls_of(&clouds, "CreateLaunchTemplate")[0];
    let t = |n: &str| field(template, &format!("LaunchTemplateData.{n}"));
    assert_eq!((t("ImageId"), t("InstanceInitiatedShutdownBehavior"), t("MetadataOptions.HttpTokens"), t("BlockDeviceMapping.1.Ebs.VolumeSize"), t("BlockDeviceMapping.1.Ebs.Throughput"), t("BlockDeviceMapping.1.Ebs.Iops")),
        ("ami-new".to_string(), "terminate".to_string(), "required".to_string(), "120".to_string(), "1000".to_string(), "4000".to_string()));
    assert_eq!(STANDARD.decode(t("UserData")).unwrap(), b"#!/bin/bash\necho job\n");
    assert_eq!((t("NetworkInterface.1.SecurityGroupId.1"), t("NetworkInterface.1.AssociatePublicIpAddress"), t("NetworkInterface.1.SubnetId")), ("sg-1".to_string(), "true".to_string(), String::new()), "the subnet is each pool's own");
    assert_eq!((t("TagSpecification.1.ResourceType"), t("TagSpecification.1.Tag.2.Key"), t("TagSpecification.1.Tag.2.Value")), ("instance".to_string(), "superci-plane".to_string(), PLANE_ID.to_string()));
    assert_eq!((field(template, "TagSpecification.1.ResourceType"), field(template, "TagSpecification.1.Tag.1.Key"), field(template, "TagSpecification.1.Tag.1.Value")), ("launch-template".to_string(), "superci-plane".to_string(), PLANE_ID.to_string()));
    let name = field(template, "LaunchTemplateName");
    assert_eq!(name, format!("superci-{PLANE_ID}-7-1790000000000"));
    // One spot machine, from the pool with the most to spare, the earlier in the order the better.
    let fleet = &calls_of(&clouds, "CreateFleet")[0];
    let f = |n: &str| field(fleet, n);
    assert_eq!((f("Type"), f("TargetCapacitySpecification.TotalTargetCapacity"), f("TargetCapacitySpecification.DefaultTargetCapacityType"), f("SpotOptions.AllocationStrategy"), f("SpotOptions.InstanceInterruptionBehavior")),
        ("instant".to_string(), "1".to_string(), "spot".to_string(), "price-capacity-optimized".to_string(), "terminate".to_string()));
    assert_eq!((f("LaunchTemplateConfigs.1.LaunchTemplateSpecification.LaunchTemplateName"), f("LaunchTemplateConfigs.1.LaunchTemplateSpecification.Version")), (name.clone(), "$Latest".to_string()));
    let pool = |n: usize| (f(&format!("LaunchTemplateConfigs.1.Overrides.{n}.InstanceType")), f(&format!("LaunchTemplateConfigs.1.Overrides.{n}.SubnetId")), f(&format!("LaunchTemplateConfigs.1.Overrides.{n}.Priority")));
    assert_eq!((pool(1), pool(6), pool(17), pool(18)), (("c8a.16xlarge".to_string(), "subnet-b".to_string(), "1".to_string()), ("m8a.16xlarge".to_string(), "subnet-a".to_string(), "6".to_string()),
        ("r8a.16xlarge".to_string(), "subnet-f".to_string(), "17".to_string()), (String::new(), String::new(), String::new())), "every other pool, in the order they would have been asked");
    // The template's one use is over: left to whoever asked to remove, when that is in nobody's way.
    assert!(calls_of(&clouds, "DeleteLaunchTemplate").is_empty());
    assert_eq!(told.template_left, Some(name));
    // What is kept of it: the one asked by itself, then the request for the seventeen.
    assert_eq!(told.asked.iter().map(|a| (a.0.as_str(), a.1.as_str(), a.2.as_str(), a.4)).collect::<Vec<_>>(), [("c8a.16xlarge", "us-east-1a", "InsufficientInstanceCapacity", 0), ("c8a.16xlarge", "us-east-1b", "started", 17)]);

    // No pool has a machine (two GPU types): said as AWS says it of one, and each pool's own answer is kept: none
    // now in ten, and a zone that does not offer the type (asked no more for a day).
    let (machine, told, clouds) = ask(&["g6.xlarge", "g5.xlarge"], FLEET_NONE, true);
    assert_eq!(machine, Err("InsufficientInstanceCapacity: AWS had no spot machine in any of 11 pools".to_string()));
    assert_eq!((launches(&clouds).len(), calls_of(&clouds, "CreateFleet").len()), (1, 1));
    let said = |word: &str| told.refused.iter().filter(|(_, _, w)| w == word).count();
    assert_eq!((said("InsufficientInstanceCapacity"), said("Unsupported")), (1 + 10, 2), "{:?}", told.refused);
    assert!(told.refused.iter().filter(|(_, _, w)| w == "Unsupported").all(|(_, zone, _)| zone == "us-east-1e") && told.refused.iter().all(|(kind, zone, _)| !kind.is_empty() && zone.starts_with("us-east-1")), "{:?}", told.refused);
    assert_eq!(told.asked.last().map(|a| (a.0.as_str(), a.2.as_str(), a.4)), Some(("", "InsufficientInstanceCapacity", 11)));

    // The account's limit of spot CPUs refused: said as that (no other pool would answer otherwise).
    let (machine, _, _) = ask(&["r8a.24xlarge", "m8a.24xlarge", "c8a.24xlarge"], FLEET_LIMIT, true);
    assert_eq!(machine, Err("MaxSpotInstanceCountExceeded: Max spot instance count exceeded".to_string()));

    // Two generations of machine: AWS goes by where most machines are spare, which is often the older type, and a
    // job is over sooner on the newer. So the pools as fast as the first choice are asked first, in one request,
    // and the slower ones in a second only when none of those has a machine. One template serves both.
    let newer_none: &'static str = Box::leak(FLEET_NONE.replace("g6.xlarge", "m8a.8xlarge").replace("g5.xlarge", "r8a.8xlarge").into_boxed_str());
    let older_started: &'static str = Box::leak(FLEET_STARTED.replace("c8a.16xlarge", "r7a.8xlarge").replace("m8a.16xlarge", "m7a.8xlarge").replace("r8a.16xlarge", "r7a.8xlarge").into_boxed_str());
    let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
        let xml = |s: &str| Some(Response::new(200, "text/xml", s.to_string()));
        if body.contains("Action=DescribeSpotPriceHistory") { return xml("<spotPriceHistorySet/>") }
        if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE) }
        if body.contains("Action=CreateFleet") { return xml(if body.contains("InstanceType=m7a.8xlarge") { older_started } else { newer_none }) }
        if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED) }
        None
    });
    let types: Vec<String> = ["m8a.8xlarge", "r8a.8xlarge", "m7a.8xlarge", "r7a.8xlarge"].map(String::from).to_vec();
    let launch = Launch { region: "us-east-1", network: Some(&network), image: "ami-new", types: &types, user_data: "#!/bin/bash\n", disk_gb: 60, disk_mibs: 125, disk_iops: 3000, clock: Some(&FixedClock), skip: &[], tags: &tags,
        spot: true, os: "linux", avoid: &[], timer: None, jitter_ms: 0, fleet: true, store_disk: None };
    let mut told = Told::default();
    let machine = block_on(run_instance(&clouds, &creds, &launch, 1_790_000_000_000, &mut told)).map(|m| (m.kind, m.zone));
    assert_eq!(machine, Ok(("r7a.8xlarge".to_string(), "us-east-1b".to_string())));
    let fleets = calls_of(&clouds.0, "CreateFleet");
    let kinds = |f: &Vec<(String, String)>| { let mut k: Vec<String> = f.iter().filter(|(n, _)| n.ends_with(".InstanceType")).map(|(_, v)| v.clone()).collect(); k.sort(); k.dedup(); (k, f.iter().filter(|(n, _)| n.ends_with(".InstanceType")).count()) };
    assert_eq!((fleets.len(), kinds(&fleets[0]), kinds(&fleets[1])), (2, (vec!["m8a.8xlarge".to_string(), "r8a.8xlarge".to_string()], 11), (vec!["m7a.8xlarge".to_string(), "r7a.8xlarge".to_string()], 12)));
    assert_eq!((calls_of(&clouds.0, "CreateLaunchTemplate").len(), launches(&clouds.0).len(), told.template_left.is_some()), (1, 1, true));
    assert_eq!(told.asked.iter().map(|a| (a.0.as_str(), a.2.as_str(), a.4)).collect::<Vec<_>>(), [("m8a.8xlarge", "InsufficientInstanceCapacity", 0), ("", "InsufficientInstanceCapacity", 11), ("r7a.8xlarge", "started", 12)]);

    // A larger size only when no pool of the size asked for has a machine: AWS, going by what is spare, would often
    // give the larger (dearer) one first.
    let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
        let xml = |s: &str| Some(Response::new(200, "text/xml", s.to_string()));
        if body.contains("Action=DescribeSpotPriceHistory") { return xml("<spotPriceHistorySet/>") }
        if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE) }
        if body.contains("Action=CreateFleet") { return xml(newer_none) }
        if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED) }
        None
    });
    let types: Vec<String> = ["m8a.large", "c8a.large", "m8a.xlarge", "c8a.xlarge"].map(String::from).to_vec();
    let launch = Launch { region: "us-east-1", network: Some(&network), image: "ami-new", types: &types, user_data: "#!/bin/bash\n", disk_gb: 60, disk_mibs: 125, disk_iops: 3000, clock: Some(&FixedClock), skip: &[], tags: &tags,
        spot: true, os: "linux", avoid: &[], timer: None, jitter_ms: 0, fleet: true, store_disk: None };
    let _ = block_on(run_instance(&clouds, &creds, &launch, 1_790_000_000_000, &mut Told::default()));
    let fleets = calls_of(&clouds.0, "CreateFleet");
    assert_eq!((fleets.len(), kinds(&fleets[0]).0, kinds(&fleets[1]).0), (2, vec!["c8a.large".to_string(), "m8a.large".to_string()], vec!["c8a.xlarge".to_string(), "m8a.xlarge".to_string()]));
    assert_eq!((superci_core::aws::cpus("m8a.large"), superci_core::aws::cpus("c8a.4xlarge"), superci_core::aws::cpus("r8a.xlarge")), (2, 16, 4));

    // Where the pools may not be asked in one request, or few are left, they are asked one by one, as before.
    let (machine, told, clouds) = ask(&["c8a.16xlarge", "m8a.16xlarge", "r8a.16xlarge"], FLEET_STARTED, false);
    assert!(machine.is_err() && calls_of(&clouds, "CreateFleet").is_empty() && launches(&clouds).len() == 12 && told.more);
    let (_, _, clouds) = ask(&["c8a.16xlarge"], FLEET_STARTED, true);
    assert_eq!((calls_of(&clouds, "CreateFleet").len(), launches(&clouds).len()), (1, 1), "six pools: one by itself, five in one request");
}

#[test]
fn a_job_gets_its_spot_machine_from_one_request_for_every_other_pool_and_a_role_that_may_not_asks_one_by_one() {
    let allowed = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeVpcs") { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into()) }
        if body.contains("Action=DescribeSubnets") { return xml(format!("<subnetSet>{}</subnetSet>", ["a", "b", "c", "d", "e", "f"].map(|z| format!("<item><subnetId>subnet-{z}</subnetId><availabilityZone>us-east-1{z}</availabilityZone></item>")).concat())) }
        if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>".into()) }
        if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") { return xml("<spotPriceHistorySet/>".into()) }
        if body.contains("Action=RunInstances") && body.contains("MarketType=spot") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        // A role from before this was asked for: AWS refuses the template, as it refuses anything a role may not do.
        if body.contains("Action=CreateLaunchTemplate") && !allowed.get() { return Some(Response::new(403, "text/xml", "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized to perform this operation. User: arn:aws:sts::123456789012:assumed-role/superci-plane-abc123def456/superci is not authorized to perform: ec2:CreateLaunchTemplate on resource: arn:aws:ec2:us-east-1:123456789012:launch-template/* because no identity-based policy allows the ec2:CreateLaunchTemplate action.</Message></Error></Errors></Response>".to_string())) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE.into()) }
        if body.contains("Action=CreateFleet") { return xml(FLEET_STARTED.into()) }
        if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED.into()) }
        None
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.max_cpu = Some(64);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a")));
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["machine_id"].as_str(), j["machine_type"].as_str(), j["zone"].as_str(), j.get("on_demand")), (Some("launched"), Some("i-0123456789abcdef0"), Some("c8a.16xlarge"), Some("us-east-1b"), None), "{j}");
    assert_eq!((launches(&clouds.0).len(), calls_of(&clouds.0, "CreateFleet").len()), (1, 1));
    // The launch template made for that request is removed (here at once: this runtime cannot go on after it has
    // answered), and nothing is left noted for a sweep.
    assert_eq!(calls_of(&clouds.0, "DeleteLaunchTemplate").iter().map(|c| field(c, "LaunchTemplateName")).collect::<Vec<_>>(), [format!("superci-{PLANE_ID}-7-1790000000000")]);
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("aws:template:")));
    // Its start says so: the first choice had none; AWS chose among the seventeen others.
    let asks: Vec<(&str, &str, &str, u64)> = j["asks"].as_array().unwrap().iter().map(|a| (a["kind"].as_str().unwrap(), a["zone"].as_str().unwrap_or_default(), a["said"].as_str().unwrap(), a["pools"].as_u64().unwrap_or(0))).collect();
    assert_eq!(asks, [("c8a.16xlarge", "us-east-1a", "InsufficientInstanceCapacity", 0), ("c8a.16xlarge", "us-east-1b", "started", 17)]);

    // A role that may not: the pools are asked one by one as before, the permission is said to be missing (the
    // dashboard offers it), and for ten minutes the next jobs do not try.
    allowed.set(false);
    block_on(plane.handle(labelled("queued", 8, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a")));
    let spot = || launches(&clouds.0).iter().filter(|l| field(l, "InstanceMarketOptions.MarketType") == "spot").count();
    assert_eq!((calls_of(&clouds.0, "CreateLaunchTemplate").len(), calls_of(&clouds.0, "CreateFleet").len()), (2, 1));
    assert!(spot() > 2, "asked one by one: {}", spot());
    assert!(store.0.borrow().contains_key("aws:no-fleet") && store.0.borrow().get("denied:aws:AskSpotPoolsAtOnce").is_some_and(|d| d.contains("ec2:CreateLaunchTemplate")));
    block_on(plane.handle(labelled("queued", 9, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a")));
    assert_eq!(calls_of(&clouds.0, "CreateLaunchTemplate").len(), 2, "not tried again so soon");
}

/// A runtime that can go on after it has answered (AWS Lambda): what it was left to do, in order.
#[derive(Default)]
struct Afterwards(Wakes, RefCell<Vec<String>>);
#[async_trait(?Send)]
impl Timer for Afterwards {
    async fn wake_in(&self, ms: u64) -> io::Result<()> { self.0.wake_in(ms).await }
    async fn pause(&self, ms: u64) { self.0.pause(ms).await }
    fn later(&self, what: &str) -> bool { self.1.borrow_mut().push(what.to_string()); true }
}

#[test]
fn a_queued_job_is_answered_at_once_and_its_machine_asked_for_after() {
    let (store, clouds, timer, cache) = (Mem::default(), FakeClouds::default(), Afterwards::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &timer, config: &config, cache: &cache, containers: None };
    let left = || timer.1.borrow_mut().drain(..).collect::<Vec<_>>();
    let address = format!("{PLANE_URL}/webhook");

    // GitHub waits ten seconds for an answer; AWS can take longer to give a machine. The event is answered with the
    // job claimed and nothing asked of any cloud yet.
    let r = block_on(plane.handle(job_event("queued", 7, "acme/app", None)));
    assert_eq!((r.status, clouds.calls.borrow().len(), job(&store, 7)["state"].as_str()), (202, 0, Some("launching")));
    let what = left();
    assert_eq!(what, ["start job:7"]);
    // Then its machine.
    block_on(plane.later(&address, &what[0])).unwrap();
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["machine_id"].as_str(), launches(&clouds).len()), (Some("launched"), Some("i-0abc"), 1), "{j}");
    assert!(STANDARD.decode(field(&launches(&clouds)[0], "UserData")).is_ok_and(|script| String::from_utf8_lossy(&script).contains(&format!("{PLANE_URL}/hello?runner="))), "with the address the event came to");
    // The same left twice (or a job a sweep has taken up meanwhile) starts nothing more.
    block_on(plane.later(&address, &what[0])).unwrap();
    assert_eq!(launches(&clouds).len(), 1);

    // What the start itself leaves for later is done in its turn: a launch template made for a request for many
    // spot pools is removed after the job has its machine, and the note of it with it.
    store.0.borrow_mut().insert("aws:template:us-east-1:superci-abc123def456-7-1".into(), "1790000000000".into());
    block_on(plane.later(&address, "template us-east-1 superci-abc123def456-7-1")).unwrap();
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DeleteLaunchTemplate") && c.2.contains("LaunchTemplateName=superci-abc123def456-7-1")));
    // (AWS here knows no such template and says an error this fake has no answer for: the note stays, for a sweep.)
    assert!(store.0.borrow().contains_key("aws:template:us-east-1:superci-abc123def456-7-1"));

    // Cancelled between the answer and the asking: no machine is asked for at all.
    block_on(plane.handle(job_event("queued", 8, "acme/app", None)));
    block_on(plane.handle(job_event("completed", 8, "acme/app", None)));
    for what in left() { block_on(plane.later(&address, &what)).unwrap() }
    let j = job(&store, 8);
    assert_eq!((j["state"].as_str(), j["machine_id"].is_null(), launches(&clouds).len()), (Some("cancelled"), true, 1), "{j}");
}

/// Requests run side by side on a real runtime: while one waits on a cloud or on the store, another is answered
/// whole. `Gate` makes that happen at one chosen step of a request under test: the step yields once, and what was
/// waiting for it (another event) runs to its end before the request goes on.
#[derive(Default)]
struct Gate { n: std::cell::Cell<usize>, at: std::cell::Cell<usize>, hit: std::cell::Cell<bool> }
struct Once(bool);
impl std::future::Future for Once {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.0 { return std::task::Poll::Ready(()) }
        self.0 = true;
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}
struct Until<'a>(&'a std::cell::Cell<bool>);
impl std::future::Future for Until<'_> {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.0.get() { return std::task::Poll::Ready(()) }
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}
impl Gate {
    async fn step(&self) {
        self.n.set(self.n.get() + 1);
        if self.n.get() == self.at.get() { self.hit.set(true); Once(false).await }
    }
}
struct SteppedStore<'a>(&'a Mem, &'a Gate);
#[async_trait(?Send)]
impl Store for SteppedStore<'_> {
    async fn get(&self, k: &str) -> io::Result<Option<String>> { self.1.step().await; self.0.get(k).await }
    async fn put(&self, k: &str, v: String) -> io::Result<()> { self.1.step().await; self.0.put(k, v).await }
    async fn put_if_absent(&self, k: &str, v: String) -> io::Result<bool> { self.1.step().await; self.0.put_if_absent(k, v).await }
    async fn swap(&self, k: &str, was: Option<&str>, v: String) -> io::Result<bool> { self.1.step().await; self.0.swap(k, was, v).await }
    async fn delete(&self, k: &str) -> io::Result<()> { self.1.step().await; self.0.delete(k).await }
    async fn list(&self, p: &str) -> io::Result<Vec<(String, String)>> { self.1.step().await; self.0.list(p).await }
}
struct SteppedHttp<'a, H: Http>(&'a H, &'a Gate);
#[async_trait(?Send)]
impl<H: Http> Http for SteppedHttp<'_, H> {
    async fn send(&self, r: Request) -> io::Result<Response> { self.1.step().await; self.0.send(r).await }
}

#[test]
fn a_job_cancelled_at_any_step_of_its_start_or_of_a_sweep_leaves_no_machine_and_stays_cancelled() {
    // Each way a job comes to its machine, with its cancelling arriving at every step of it in turn (each read or
    // write of the store, each request to GitHub or AWS). Whatever the step: once things have settled, every machine
    // AWS started for it is ended, and the job is cancelled and stays so.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Way { AtOnce, AfterTheAnswer, WaitingAtASweep, IdleAtASweep }
    let mut tried = 0;
    // (One App for every try: making its key is the slow part of a test.)
    let the_app = app();
    for way in [Way::AtOnce, Way::AfterTheAnswer, Way::WaitingAtASweep, Way::IdleAtASweep] {
        // (First with no cancelling, to count the steps; then once for each.)
        let mut steps = usize::MAX;
        let mut at = 0;
        // (What a runtime keeps between requests, GitHub's token among it, is kept between the tries too.)
        let cache = Cache::default();
        while at <= steps.min(400) {
            let (cancelled, started, ended) = (std::cell::Cell::new(false), RefCell::new(Vec::<String>::new()), RefCell::new(Vec::<String>::new()));
            let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
                let json = |status: u16, v: serde_json::Value| Some(Response::new(status, "application/json", v.to_string()));
                if body.contains("Action=RunInstances") {
                    let id = format!("i-{}", started.borrow().len() + 1);
                    started.borrow_mut().push(id.clone());
                    return Some(Response::new(200, "text/xml", format!("<RunInstancesResponse><instancesSet><item><instanceId>{id}</instanceId><placement><availabilityZone>us-east-1b</availabilityZone></placement></item></instancesSet></RunInstancesResponse>")))
                }
                if body.contains("Action=TerminateInstances") {
                    ended.borrow_mut().extend(url::form_urlencoded::parse(body.as_bytes()).filter(|(k, _)| k.starts_with("InstanceId")).map(|(_, v)| v.into_owned()));
                    return Some(Response::new(200, "text/xml", "<TerminateInstancesResponse/>".to_string()))
                }
                // GitHub as it is: the job is queued until it is cancelled; its runner, never given a job, can be taken back.
                if r.url == "https://api.github.com/repos/acme/app/actions/jobs/7" { return json(200, if cancelled.get() { serde_json::json!({ "status": "completed", "conclusion": "cancelled", "runner_name": null }) } else { serde_json::json!({ "status": "queued", "runner_name": null }) }) }
                if r.url.contains("/actions/runners/") && r.method == "DELETE" { return Some(Response::new(204, "application/json", String::new())) }
                if r.url.contains("/actions/runners/") && r.method == "GET" { return json(200, serde_json::json!({ "id": 5, "busy": false })) }
                None
            });
            let (store, gate, wakes, afterwards) = (Mem::default(), Gate::default(), Wakes::default(), Afterwards::default());
            let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
            let mut config = Config::new(PLANE_ID.into());
            config.app = Some(the_app.clone());
            (config.aws_own, config.aws_own_creds) = own_aws();
            let (stepped_store, stepped_http) = (SteppedStore(&store, &gate), SteppedHttp(&clouds, &gate));
            let timer: &dyn Timer = if way == Way::AfterTheAnswer { &afterwards } else { &wakes };
            let plane = ControlPlane { store: &stepped_store, http: &stepped_http, clock: &clock, timer, config: &config, cache: &cache, containers: None };
            // What comes before the request under test (no cancelling can arrive in it).
            match way {
                Way::AtOnce => {}
                Way::AfterTheAnswer | Way::IdleAtASweep => { block_on(plane.handle(job_event("queued", 7, "acme/app", None))); }
                Way::WaitingAtASweep => { store.0.borrow_mut().insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": 1_790_000_000_000u64, "installation_id": 42, "app_id": 99, "cloud": "", "seen_in_progress": false, "label": "superci" }).to_string()); }
            }
            if way == Way::IdleAtASweep { clock.0.set(clock.0.get() + 30_000) }
            let left = afterwards.1.borrow().first().cloned().unwrap_or_default();
            (gate.n.set(0), gate.at.set(if at == 0 { usize::MAX } else { at }), gate.hit.set(false));
            let under_test = async {
                match way {
                    Way::AtOnce => { plane.handle(job_event("queued", 7, "acme/app", None)).await; }
                    Way::AfterTheAnswer => plane.later(&format!("{PLANE_URL}/webhook"), &left).await.unwrap(),
                    Way::WaitingAtASweep | Way::IdleAtASweep => plane.alarm().await.unwrap(),
                }
                gate.hit.set(true);
            };
            let cancelling = async {
                if at == 0 { return }
                Until(&gate.hit).await;
                cancelled.set(true);
                plane.handle(job_event("completed", 7, "acme/app", None)).await;
            };
            block_on(futures::future::join(under_test, cancelling));
            if at == 0 { steps = gate.n.get(); at = 1; assert!(steps > 5, "{way:?}: {steps} steps"); println!("{way:?}: {steps} steps"); continue }
            gate.at.set(usize::MAX);
            // Settled: the sweeps of the next quarter of an hour.
            for ms in [1_000, 20_000, 61_000, 120_000, 600_000, 120_000] { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap(); }
            let j = job(&store, 7);
            let up: Vec<String> = started.borrow().iter().filter(|id| !ended.borrow().contains(id)).cloned().collect();
            assert!(up.is_empty(), "{way:?}, cancelled at step {at} of {steps}: {up:?} left up (started {:?}, ended {:?}); {j}", started.borrow(), ended.borrow());
            assert_eq!(j["state"].as_str(), Some("cancelled"), "{way:?}, cancelled at step {at} of {steps}: {j}");
            tried += 1;
            at += 1;
        }
    }
    assert!(tried > 60, "{tried}");
}

#[test]
fn a_job_that_begins_at_any_step_of_a_sweep_keeps_its_machine() {
    // The mirror of the test above: a machine is up and idle, a sweep is looking at it (a minute and a half on, it
    // asks GitHub whether its job still waits), and the runner takes the job at each step of that sweep in turn.
    // Whatever the step, the job is running after, on its machine, and the sweeps that follow leave it alone.
    let the_app = app();
    let (mut steps, mut at, cache) = (usize::MAX, 0, Cache::default());
    let runner = format!("superci-{PLANE_ID}-7");
    while at <= steps.min(400) {
        let (began, ended) = (std::cell::Cell::new(false), std::cell::Cell::new(0));
        let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
            let json = |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string()));
            if body.contains("Action=TerminateInstances") { ended.set(ended.get() + 1); return Some(Response::new(200, "text/xml", "<TerminateInstancesResponse/>".to_string())) }
            if r.url == "https://api.github.com/repos/acme/app/actions/jobs/7" { return json(if began.get() { serde_json::json!({ "status": "in_progress", "runner_name": runner }) } else { serde_json::json!({ "status": "queued", "runner_name": null }) }) }
            if r.url.contains("/actions/runners/") && r.method == "DELETE" { return Some(Response::new(if began.get() { 422 } else { 204 }, "application/json", r#"{"message":"Runner is still running a job"}"#.to_string())) }
            if r.url.contains("/actions/runners/") && r.method == "GET" { return json(serde_json::json!({ "id": 5, "busy": began.get() })) }
            None
        });
        let (store, gate, wakes) = (Mem::default(), Gate::default(), Wakes::default());
        let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(the_app.clone());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let (stepped_store, stepped_http) = (SteppedStore(&store, &gate), SteppedHttp(&clouds, &gate));
        let plane = ControlPlane { store: &stepped_store, http: &stepped_http, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(job_event("queued", 7, "acme/app", None)));
        clock.0.set(clock.0.get() + 100_000);
        (gate.n.set(0), gate.at.set(if at == 0 { usize::MAX } else { at }), gate.hit.set(false));
        let sweep = async { plane.alarm().await.unwrap(); gate.hit.set(true) };
        let beginning = async {
            if at == 0 { return }
            Until(&gate.hit).await;
            began.set(true);
            plane.handle(job_event("in_progress", 7, "acme/app", Some(&runner))).await;
        };
        block_on(futures::future::join(sweep, beginning));
        if at == 0 { steps = gate.n.get(); at = 1; assert!(steps > 5, "{steps} steps"); continue }
        gate.at.set(usize::MAX);
        for ms in [1_000, 61_000, 600_000] { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap(); }
        let j = job(&store, 7);
        assert_eq!((j["state"].as_str(), j["seen_in_progress"].as_bool(), j["machine_id"].as_str(), ended.get()), (Some("running"), Some(true), Some("i-0abc"), 0), "begun at step {at} of {steps}: {j}");
        at += 1;
    }
}

#[test]
fn a_job_that_runs_and_ends_at_any_step_of_a_sweep_stays_done() {
    // And a whole job within one step of a sweep: the runner takes it and it ends (GitHub then knows the job as
    // completed and its runner no more) while the sweep holds what it read of it before. Whatever the step, the job
    // is done as it ended, its machine ended once, and the sweep's older copy of it undoes none of that.
    let the_app = app();
    let (mut steps, mut at, cache) = (usize::MAX, 0, Cache::default());
    let runner = format!("superci-{PLANE_ID}-7");
    while at <= steps.min(400) {
        let (began, ended) = (std::cell::Cell::new(false), std::cell::Cell::new(0));
        let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
            let json = |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string()));
            if body.contains("Action=TerminateInstances") { ended.set(ended.get() + 1); return Some(Response::new(200, "text/xml", "<TerminateInstancesResponse/>".to_string())) }
            if r.url == "https://api.github.com/repos/acme/app/actions/jobs/7" { return json(if began.get() { serde_json::json!({ "status": "completed", "conclusion": "success", "runner_name": runner }) } else { serde_json::json!({ "status": "queued", "runner_name": null }) }) }
            if r.url.contains("/actions/runners/") && began.get() { return Some(Response::new(404, "application/json", r#"{"message":"Not Found"}"#.to_string())) }
            if r.url.contains("/actions/runners/") && r.method == "DELETE" { return Some(Response::new(204, "application/json", String::new())) }
            if r.url.contains("/actions/runners/") && r.method == "GET" { return json(serde_json::json!({ "id": 5, "busy": false })) }
            None
        });
        let (store, gate, wakes) = (Mem::default(), Gate::default(), Wakes::default());
        let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(the_app.clone());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let (stepped_store, stepped_http) = (SteppedStore(&store, &gate), SteppedHttp(&clouds, &gate));
        let plane = ControlPlane { store: &stepped_store, http: &stepped_http, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(job_event("queued", 7, "acme/app", None)));
        clock.0.set(clock.0.get() + 100_000);
        (gate.n.set(0), gate.at.set(if at == 0 { usize::MAX } else { at }), gate.hit.set(false));
        let sweep = async { plane.alarm().await.unwrap(); gate.hit.set(true) };
        let beginning = async {
            if at == 0 { return }
            Until(&gate.hit).await;
            began.set(true);
            plane.handle(job_event("in_progress", 7, "acme/app", Some(&runner))).await;
            plane.handle(concluded(7, &runner, "success")).await;
        };
        block_on(futures::future::join(sweep, beginning));
        if at == 0 { steps = gate.n.get(); at = 1; assert!(steps > 5, "{steps} steps"); continue }
        gate.at.set(usize::MAX);
        for ms in [1_000, 61_000, 600_000] { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap(); }
        let j = job(&store, 7);
        assert_eq!((j["state"].as_str(), j["conclusion"].as_str(), ended.get() >= 1), (Some("done"), Some("success"), true), "run and ended at step {at} of {steps}: {j}");
        at += 1;
    }
}

#[test]
fn a_gitlab_job_cancelled_at_any_step_of_its_start_leaves_no_machine_and_no_runner() {
    // GitLab's job, on an AWS machine: cancelled at every step of its start in turn. No note is passed here as for
    // GitHub's: the event ends the job's record, and the start under way, holding what it read before, finds it
    // ended, asks for nothing more and ends what it was given.
    let (mut steps, mut at, cache) = (usize::MAX, 0, Cache::default());
    while at <= steps.min(400) {
        let (cancelled, started, ended, runners) = (std::cell::Cell::new(false), RefCell::new(Vec::<String>::new()), RefCell::new(Vec::<String>::new()), RefCell::new(Vec::<(u64, bool)>::new()));
        let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
            let json = |status: u16, v: serde_json::Value| Some(Response::new(status, "application/json", v.to_string()));
            if body.contains("Action=RunInstances") {
                let id = format!("i-{}", started.borrow().len() + 1);
                started.borrow_mut().push(id.clone());
                return Some(Response::new(200, "text/xml", format!("<RunInstancesResponse><instancesSet><item><instanceId>{id}</instanceId><placement><availabilityZone>us-east-1b</availabilityZone></placement></item></instancesSet></RunInstancesResponse>")))
            }
            if body.contains("Action=TerminateInstances") {
                ended.borrow_mut().extend(url::form_urlencoded::parse(body.as_bytes()).filter(|(k, _)| k.starts_with("InstanceId")).map(|(_, v)| v.into_owned()));
                return Some(Response::new(200, "text/xml", "<TerminateInstancesResponse/>".to_string()))
            }
            if r.url == "https://gitlab.example/api/v4/projects/380/jobs/1977" { return json(200, serde_json::json!({ "id": 1977, "status": if cancelled.get() { "canceled" } else { "pending" }, "tag_list": ["superci/cpu=2", "docker"] })) }
            if r.url == "https://gitlab.example/api/v4/user/runners" { let id = 55 + runners.borrow().len() as u64; runners.borrow_mut().push((id, true)); return json(201, serde_json::json!({ "id": id, "token": format!("glrt-runner{id}") })) }
            if let Some(id) = r.url.strip_prefix("https://gitlab.example/api/v4/runners/").and_then(|id| id.parse::<u64>().ok()).filter(|_| r.method == "DELETE") {
                for runner in runners.borrow_mut().iter_mut().filter(|(r, _)| *r == id) { runner.1 = false }
                return Some(Response::new(204, "application/json", String::new()))
            }
            None
        });
        let (store, gate, wakes, idle) = (Mem::default(), Gate::default(), Wakes::default(), std::cell::Cell::new(false));
        let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
        let mut config = Config::new(PLANE_ID.into());
        config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
        (config.aws_own, config.aws_own_creds) = own_aws();
        let (stepped_store, stepped_http) = (SteppedStore(&store, &gate), SteppedHttp(&clouds, &gate));
        let plane = ControlPlane { store: &stepped_store, http: &stepped_http, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
        (gate.n.set(0), gate.at.set(if at == 0 { usize::MAX } else { at }), gate.hit.set(false));
        let start = async { plane.handle(gitlab_event(1977, "pending", "hook-secret-0123456789")).await; gate.hit.set(true) };
        let cancelling = async {
            if at == 0 { return }
            Until(&gate.hit).await;
            cancelled.set(true);
            idle.set(store.0.borrow().get("job:gl1977").is_some_and(|j| j.contains(r#""state":"launched""#)));
            plane.handle(gitlab_event(1977, "canceled", "hook-secret-0123456789")).await;
        };
        block_on(futures::future::join(start, cancelling));
        let j = serde_json::from_str::<serde_json::Value>(&store.0.borrow().get("job:gl1977").cloned().unwrap_or("null".into())).unwrap();
        if at == 0 { steps = gate.n.get(); at = 1; assert!(steps > 5 && j["state"] == "launched" && j["cloud"] == "aws", "{steps} steps: {j}"); continue }
        gate.at.set(usize::MAX);
        // At once, with no sweep waited for, while its machine was still being asked for. (A machine already up
        // and idle is another matter at GitLab, where a runner takes any job with its tags and cannot be asked
        // back as GitHub's can: it is ended by the next sweep, as before.)
        let up = || started.borrow().iter().filter(|id| !ended.borrow().contains(id)).cloned().collect::<Vec<_>>();
        let left = || runners.borrow().iter().filter(|(_, there)| *there).map(|(id, _)| *id).collect::<Vec<_>>();
        assert!(idle.get() || (up().is_empty() && left().is_empty()), "cancelled at step {at} of {steps}: machines {:?} left up, runners {:?} left; {j}", up(), left());
        for ms in [1_000, 61_000, 600_000] { clock.0.set(clock.0.get() + ms); block_on(plane.alarm()).unwrap(); }
        let j = serde_json::from_str::<serde_json::Value>(&store.0.borrow().get("job:gl1977").cloned().unwrap_or("null".into())).unwrap();
        assert!(up().is_empty() && left().is_empty(), "cancelled at step {at} of {steps}, after the sweeps: machines {:?} left up, runners {:?} left; {j}", up(), left());
        assert!(j.is_null() || j["state"] == "cancelled" || (idle.get() && j["state"] == "swept"), "cancelled at step {at} of {steps}: {j}");
        at += 1;
    }
}

#[test]
fn a_workflow_files_job_is_written_for_the_runner_as_github_writes_it() {
    use superci_core::workflow::{job_parts, parse};
    // What a workflow file decides of the message the runner is handed, held to two recordings: what real GitHub
    // sent for one workflow, and what an open-source stand-in for GitHub's service sent for one that uses most of
    // what a step and a job can say; and what real GitHub sent for steps run in the background, waited for, ended
    // and run in parallel groups. Every value, in its order, with its place in the file.
    for (workflow, path, recorded) in [
        (include_str!("fixtures/runner/record.yml"), ".github/workflows/record.yml", include_str!("fixtures/runner/record.github.json")),
        (include_str!("fixtures/runner/parallel.yml"), ".github/workflows/parallel.yml", include_str!("fixtures/runner/parallel.github.json")),
        (include_str!("fixtures/runner/parallel2.yml"), ".github/workflows/parallel2.yml", include_str!("fixtures/runner/parallel2.github.json")),
        (include_str!("fixtures/runner/three.yml"), ".github/workflows/ci.yml", include_str!("fixtures/runner/three.standin.json")),
    ] {
        let want: serde_json::Value = serde_json::from_str(recorded).unwrap();
        let got = job_parts(&parse(workflow).unwrap(), path, "test", &mut || String::new()).unwrap();
        for key in ["fileTable", "environmentVariables", "defaults", "jobOutputs", "jobServiceContainers", "jobContainer"] {
            assert_eq!(got.get(key), want.get(key), "{path}: {key}\n got {}\nwant {}", got.get(key).unwrap_or(&serde_json::Value::Null), want.get(key).unwrap_or(&serde_json::Value::Null));
        }
        let (got, want) = (got["steps"].as_array().unwrap(), want["steps"].as_array().unwrap());
        assert_eq!(got.len(), want.len(), "{path}");
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            let (mut g, mut w) = (g.clone(), w.clone());
            g.as_object_mut().unwrap().remove("id");
            // (The stand-in writes an empty `path` for an action at the top of its repository; GitHub writes none.)
            if w["reference"]["path"] == "" { w["reference"].as_object_mut().unwrap().remove("path"); }
            assert_eq!(serde_json::to_string(&g).unwrap(), serde_json::to_string(&w).unwrap(), "{path}: step {}", i + 1);
        }
    }
}

#[test]
fn the_runners_calls_for_a_job_are_answered_as_its_service_answers_them() {
    use superci_core::runner_service::{answer, job_message, access_token, Given, HandedJob, Ids};
    // A job handed to a runner, then every call a real runner made for one (recorded, in order): each is answered,
    // the runner takes its job once, and what it says of the job is kept: its steps, its lines, its logs, how it ended.
    let store = Mem::default();
    let given = Given { workflow: include_str!("fixtures/runner/three.yml"), path: ".github/workflows/ci.yml", job: "test", secrets: &[("A_SECRET".into(), "hunter2.hunter2".into())],
        vars: &[("A_VAR".into(), "fromcli".into())], github: serde_json::json!({ "event_name": "push", "repository": "acme/app", "ref": "refs/heads/main" }),
        service: "https://plane.example/runner/abc/", access_token: &access_token(1_790_000_000_000), ids: Ids::new(4), matrix: serde_json::Value::Null, strategy: serde_json::Value::Null, needs: serde_json::Value::Null, display: None, inputs: serde_json::Value::Null };
    let message = job_message(&given).unwrap();
    // (Every part GitHub's own message has, but GitHub's own bookkeeping.)
    let github: serde_json::Value = serde_json::from_str(include_str!("fixtures/runner/record.github.json")).unwrap();
    for key in ["environmentVariables", "fileTable", "steps", "variables", "messageType", "plan", "timeline", "jobId", "jobDisplayName", "jobName", "requestId", "lockedUntil", "resources", "contextData", "mask"] {
        assert!(message.get(key).is_some(), "{key}");
    }
    assert!(github.get("steps").is_some());
    assert_eq!((message["jobDisplayName"].as_str(), message["variables"]["A_SECRET"]["isSecret"].as_bool(), message["mask"][0]["value"].as_str()), (Some("The test job"), Some(true), Some("hunter2\\.hunter2")));
    assert_eq!(message["contextData"]["vars"], serde_json::json!({ "t": 2, "d": [{ "k": "A_VAR", "v": "fromcli" }] }));
    assert_eq!(message["resources"]["endpoints"][0]["data"]["FeedStreamUrl"].as_str().unwrap(), format!("wss://plane.example/runner/abc/_apis/v1/TimeLineWebConsoleLog/feedstream/{}/ws", message["timeline"]["id"].as_str().unwrap()));
    block_on(store.put("rjob:abc", serde_json::to_string(&HandedJob { message, ..Default::default() }).unwrap())).unwrap();
    let mut messages = vec![];
    for line in include_str!("fixtures/runner/runner-requests.jsonl").lines() {
        let r: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut req = Request::new(r["method"].as_str().unwrap(), &format!("https://plane.example/runner/abc{}", r["path"].as_str().unwrap()));
        req.body = r["request"].as_str().unwrap().as_bytes().to_vec();
        let res = block_on(answer(&store, "abc", &req, r["path"].as_str().unwrap())).unwrap();
        assert!(res.status == 200 || res.status == 204 || (res.status == 400 && r["path"].as_str().unwrap().contains("feedstream")), "{} {}: {}", r["method"], r["path"], res.status);
        if r["path"].as_str().unwrap().contains("/Message/") && r["method"] == "GET" { messages.push(res.status) }
    }
    assert_eq!(messages[0], 200, "the job, the first time");
    assert!(messages[1..].iter().all(|s| *s == 204), "nothing more after: {messages:?}");
    let job: HandedJob = serde_json::from_str(&store.0.borrow()["rjob:abc"]).unwrap();
    assert_eq!(job.result.as_deref(), Some("succeeded"));
    assert!(job.steps.iter().any(|s| s["type"] == "Job" && s["result"] == "succeeded") && job.lines > 0 && job.logs > 0, "{:?}", job.steps);
    assert!(store.0.borrow().keys().any(|k| k.starts_with("rline:abc:")) && store.0.borrow().keys().any(|k| k.starts_with("rlog:abc:")));
    // Another job's prefix, or none at all, is nothing here.
    let res = block_on(answer(&store, "other", &Request::new("GET", "https://plane.example/runner/other/_apis/connectionData"), "/_apis/connectionData")).unwrap();
    assert_eq!(res.status, 404);
}

#[test]
fn a_long_run_from_a_computers_log_is_given_with_its_steps_within_what_an_answer_may_be() {
    use superci_core::runner_service::HandedJob;
    // A job of a run from a computer, still running, its three steps' logs 2.5 MB each, the last one in colour (each
    // colour six bytes once written as JSON): asked with its steps, as the dashboard asks while it runs and for its
    // raw log. AWS's function URLs answer 502 for an answer over six MB, so each must stay within it.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let job = serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": 1_790_000_000_000u64, "runner": null, "runner_id": null, "installation_id": 0,
        "cloud": "aws", "machine_id": "i-0abc", "machine_type": null, "error": null, "seen_in_progress": true, "provider": "local", "local": "abc" });
    block_on(store.put("job:7", job.to_string())).unwrap();
    let steps: Vec<serde_json::Value> = (1..=3).map(|n| serde_json::json!({ "id": format!("0000000{n}-0000-0000-0000-000000000000"), "name": format!("Step {n}"), "type": "Task", "order": n, "log": n, "state": "completed" })).collect();
    block_on(store.put("rjob:abc", serde_json::to_string(&HandedJob { steps, ..Default::default() }).unwrap())).unwrap();
    for n in 1..=3u64 {
        let colour = if n == 3 { "\u{1b}[32m" } else { "" };
        let text = (0..25_000).map(|i| format!("{colour}step {n} line {i:05} {}", "x".repeat(if n == 3 { 74 } else { 80 }))).collect::<Vec<_>>().join("\n");
        block_on(store.put(&format!("rlog:abc:{n:04}:0000"), STANDARD.encode(text))).unwrap();
    }
    for bytes in [1_000_000, 4_000_000] {
        let r = block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/job/log?id=7&bytes={bytes}&steps=1")).with_header("authorization", "Bearer dashboard-session-key-1")));
        assert!(r.status == 200 && r.body.len() <= 5_500_000, "{bytes}: {} {} bytes", r.status, r.body.len());
        let got: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        // The newest of it kept, from a line's start: the log's end, and the last step's whole lines up to its end.
        let (log, last) = (got["log"].as_str().unwrap(), got["steps"][2]["text"].as_str().unwrap());
        assert!(log.trim_end().ends_with(&format!("step 3 line 24999 {}", "x".repeat(74))) && (log.starts_with("step ") || log.starts_with("\u{1b}[32mstep ")), "{}", &log[..80]);
        assert!(last.ends_with(&"x".repeat(74)) && last.starts_with("\u{1b}[32mstep 3 line "), "{}", &last[..80]);
        let shown: usize = got["steps"].as_array().unwrap().iter().map(|s| s["text"].as_str().unwrap().len()).sum();
        assert!(shown <= bytes && got["truncated"] == true && got["bytes"].as_u64() > Some(7_000_000), "{bytes}: {shown}");
    }
    // A short one is given whole, its steps' too.
    block_on(store.put("rlog:abc:0003:0000", STANDARD.encode("done\n"))).unwrap();
    for n in 1..=2u64 { block_on(store.put(&format!("rlog:abc:{n:04}:0000"), STANDARD.encode(format!("step {n}\n")))).unwrap(); }
    let got = json(&block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/job/log?id=7&steps=1")).with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert_eq!((got["log"].as_str(), got["truncated"].as_bool(), got["steps"][0]["text"].as_str()), (Some("step 1\nstep 2\ndone\n"), Some(false), Some("step 1\n")), "{got}");
}

#[test]
fn a_run_from_a_computer_gets_its_machine_and_its_runner_takes_the_job_from_the_control_plane() {
    use base64::Engine;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    // The working tree, in two parts (a gzipped tar cut in two), then the run. Nothing without the key.
    let tree = vec![7u8; 400_000];
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-0123456789abcdef&part=0")).with_body(tree[..350_000].to_vec())).status, 404);
    for (part, bytes) in [(0, &tree[..350_000]), (1, &tree[350_000..])] {
        assert_eq!(run(key(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-0123456789abcdef&part={part}")).with_body(bytes.to_vec()))).status, 200);
    }
    let body = serde_json::json!({ "workflow": include_str!("fixtures/runner/three.yml"), "path": ".github/workflows/ci.yml", "repository": "acme/app", "ref": "refs/heads/main", "sha": "abc",
        "actor": "someone", "secrets": [["A_SECRET", "hunter2hunter2"]], "vars": [["A_VAR", "fromcli"]], "tree": "tree-0123456789abcdef", "parts": 2 }).to_string();
    let made = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(body)));
    assert_eq!(made.status, 200, "{}", made.body_text());
    let run_id = json(&made)["run_id"].as_str().unwrap().to_string();
    // Its machine, as any job's, with a runner that takes its job from here and not from GitHub.
    let launched = launches(&clouds);
    assert_eq!(launched.len(), 1);
    let script = String::from_utf8(STANDARD.decode(field(&launched[0], "UserData")).unwrap()).unwrap();
    let jit = script.split("--jitconfig ").nth(1).unwrap().split_whitespace().next().unwrap();
    let config_of = |name: &str| -> serde_json::Value { let all: serde_json::Value = serde_json::from_slice(&STANDARD.decode(jit).unwrap()).unwrap(); serde_json::from_slice(&STANDARD.decode(all[name].as_str().unwrap()).unwrap()).unwrap() };
    let server = config_of(".runner")["ServerUrl"].as_str().unwrap().to_string();
    assert!(server.starts_with(&format!("{PLANE_URL}/runner/")) && config_of(".credentials")["Data"]["AuthorizationUrl"] == format!("{server}/_apis/v1/auth/"), "{server}");
    assert!(!clouds.calls.borrow().iter().any(|c| c.1.contains("generate-jitconfig")), "GitHub is not asked for a runner");
    let record: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{run_id}")]).unwrap();
    let job_id: u64 = record["jobs"][0]["job_id"].as_str().unwrap().parse().unwrap();
    assert!(job_id > 1 << 52 && job_id < 1 << 53 && job(&store, job_id)["provider"] == "local" && job(&store, job_id)["state"] == "launched" && job(&store, job_id)["name"] == "The test job");
    // Its checkout: the action that puts the tree in place, and the tree in its parts, as uploaded.
    let checkout = run(Request::new("GET", &format!("{server}/superci/checkout.tar.gz")));
    assert_eq!((checkout.status, &checkout.body[..2]), (200, &[0x1f, 0x8b][..]));
    let parts: Vec<u8> = [0, 1].iter().flat_map(|p| run(Request::new("GET", &format!("{server}/superci/tree?part={p}"))).body).collect();
    assert!(parts == tree);
    assert_eq!(run(Request::new("GET", &format!("{PLANE_URL}/runner/not-a-token-of-any-job-here/superci/tree?part=0"))).status, 404);
    // The runner's calls, as a real runner made them: it takes the job (the job has begun), and says how it ended.
    for line in include_str!("fixtures/runner/runner-requests.jsonl").lines() {
        let r: serde_json::Value = serde_json::from_str(line).unwrap();
        let path = r["path"].as_str().unwrap().trim_start_matches("/runner/server");
        let mut req = Request::new(r["method"].as_str().unwrap(), &format!("{server}{path}"));
        req.body = r["request"].as_str().unwrap().as_bytes().to_vec();
        let res = run(req);
        assert!([200, 204, 400].contains(&res.status), "{} {path}: {} {}", r["method"], res.status, res.body_text());
        if path.contains("/Message/") && r["method"] == "GET" && res.status == 200 { assert_eq!(job(&store, job_id)["state"], "running") }
    }
    let j = job(&store, job_id);
    assert_eq!((j["state"].as_str(), j["conclusion"].as_str()), (Some("done"), Some("success")), "{j}");
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=TerminateInstances")), "its machine is ended with it");
    // How it went, for the command that started it: its result and its lines; and its secrets are kept no more.
    let status = json(&run(key(Request::new("GET", &format!("{PLANE_URL}/runs/{run_id}")))));
    assert_eq!((status["over"].as_bool(), status["result"].as_str(), status["jobs"][0]["result"].as_str(), status["jobs"][0]["name"].as_str()), (Some(true), Some("success"), Some("success"), Some("The test job")), "{status}");
    assert!(status["jobs"][0]["lines"].as_array().is_some_and(|l| !l.is_empty()) && status["jobs"][0]["next"].as_u64() > Some(0), "{status}");
    let later = json(&run(key(Request::new("GET", &format!("{PLANE_URL}/runs/{run_id}?after={}", status["jobs"][0]["next"])))));
    assert!(later["jobs"][0]["lines"].as_array().is_some_and(|l| l.is_empty()));
    assert!(!store.0.borrow().values().any(|v| v.contains("hunter2hunter2")), "no secret is left in the store");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("rtree")), "nor its working tree, once fetched and the job over");
}

#[test]
fn a_key_for_coding_agents_starts_runs_within_the_control_planes_own_limits() {
    // A key that may start runs from a computer, with no limits of its own. It also reads.
    let key = "superci_run_0123456789abcdef0123456789abcdef";
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    config.read_keys = vec![superci_core::plane::read_key("READ_KEY_4102444800_AGENT", &format!("{}:runs", superci_core::crypto::sha256_hex(key.as_bytes()))).unwrap(),
        superci_core::plane::read_key("READ_KEY_4102444800_LOOKER", &superci_core::crypto::sha256_hex(b"superci_read_0123456789abcdef0123456789abcdef")).unwrap()];
    config.routing.max_cpu = Some(64);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |label: &str, k: &str| {
        let workflow = format!("jobs:\n  test:\n    runs-on: {label}\n    steps:\n      - run: echo hi\n");
        block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", &format!("Bearer {k}"))
            .with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string())))
    };
    // A key that only reads starts nothing; this one starts runs, of the size the control plane allows, four at once.
    assert_eq!(run("superci", "superci_read_0123456789abcdef0123456789abcdef").status, 403);
    let made = run("superci", key);
    assert_eq!(made.status, 200, "{}", made.body_text());
    let run_id = json(&made)["run_id"].as_str().unwrap().to_string();
    let record: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{run_id}")]).unwrap();
    assert_eq!(job(&store, record["jobs"][0]["job_id"].as_str().unwrap().parse().unwrap())["by"], "AGENT");
    for _ in 0..3 { let big = run("superci/cpu=64", key); assert_eq!(big.status, 200, "{}", big.body_text()); }
    // What its jobs kept with upload-artifact can be fetched from the computer: listed, then read through links to
    // the bucket, by whoever may look (a key that only reads too).
    let (at, e) = block_on(superci_core::cache::begin_artifact(&store, &run_id, "coverage", "us-east-1", None, 5)).unwrap();
    block_on(superci_core::cache::finish(&store, &at, &e, vec![("p0".into(), 7), ("p1".into(), 3)])).unwrap();
    let look = |path: &str| block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}{path}")).with_header("authorization", "Bearer superci_read_0123456789abcdef0123456789abcdef")));
    let listed = look(&format!("/runs/{run_id}/artifacts"));
    assert_eq!(json(&listed)["artifacts"].as_array().unwrap().iter().map(|a| (a["name"].as_str().unwrap().to_string(), a["size"].as_u64().unwrap())).collect::<Vec<_>>(), [("coverage".to_string(), 10)]);
    let got = json(&look(&format!("/runs/{run_id}/artifacts?name=coverage")));
    let urls: Vec<&str> = got["pieces"].as_array().unwrap().iter().map(|p| p["url"].as_str().unwrap()).collect();
    assert!(urls.len() == 2 && urls.iter().all(|u| u.contains(&format!("superci-plane-{PLANE_ID}-us-east-1")) && u.contains("X-Amz-Signature")), "{urls:?}");
    assert_eq!((look(&format!("/runs/{run_id}/artifacts?name=nope")).status, look("/runs/12345/artifacts").status), (404, 404));
    // The dashboard sees which keys may start runs.
    let status = json(&block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/status")).with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert_eq!(status["read_keys"], serde_json::json!([{ "name": "AGENT", "until_ms": 4_102_444_800_000u64, "runs": true }, { "name": "LOOKER", "until_ms": 4_102_444_800_000u64 }]));
    // It cancels a run it may start (from a computer), and no run of GitHub's, whatever its id: a GitHub job's run
    // known here is cancelled only with a dashboard's key.
    block_on(plane.handle(labelled("queued", 77, "acme/app", None, "superci")));
    let cancel = |id: &str, k: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs/{id}/cancel")).with_header("authorization", &format!("Bearer {k}")))).status;
    assert_eq!(cancel("3", key), 404, "a GitHub run is not a key for agents' to cancel");
    assert!(!clouds.calls.borrow().iter().any(|c| c.1.contains("/actions/runs/3/cancel")));
    assert_eq!(cancel(&run_id, key), 200);
    // A job cancelled before its machine took it keeps nothing of its secrets.
    let workflow = "jobs:\n  test:\n    runs-on: superci\n    steps:\n      - run: echo ${{ secrets.TOKEN }}\n";
    let made = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app", "secrets": [["TOKEN", "a-secret-value-0001"]] }).to_string())));
    assert_eq!(made.status, 200, "{}", made.body_text());
    let with_secret = json(&made)["run_id"].as_str().unwrap().to_string();
    assert!(store.0.borrow().values().any(|v| v.contains("a-secret-value-0001")), "kept while the run goes");
    assert_eq!(cancel(&with_secret, key), 200);
    assert!(!store.0.borrow().values().any(|v| v.contains("a-secret-value-0001")), "nothing of it left once cancelled");
    // A key of the form 0.18.0 made (with limits of its own) is no key now.
    assert!(superci_core::plane::read_key("READ_KEY_4102444800_OLD", &format!("{}:runs:16:10", superci_core::crypto::sha256_hex(key.as_bytes()))).is_none());
}

#[test]
fn the_secrets_a_ci_keeps_go_to_the_runs_from_a_computer_that_name_them_the_narrowest_under_each_name() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    // Kept as the dashboard writes them: one setting each, by name and scope.
    let kept = |name: &str, value: &str, scope: serde_json::Value| {
        let s: superci_core::secrets::Stored = serde_json::from_value(serde_json::json!({ "name": name, "value": value, "updated_ms": 1 }).as_object().unwrap().clone().into_iter().chain(scope.as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>().into()).unwrap();
        (superci_core::secrets::setting(name, &s.scope), serde_json::to_string(&s).unwrap())
    };
    config.read_settings(&[
        ("DEVICE_KEY_4102444800_abc".into(), "dashboard-session-key-1".into()),
        // (One whose name says no time it expires is no key.)
        ("DEVICE_KEY_soon_abc".into(), "dashboard-session-key-2".into()),
        kept("TOKEN", "token-all-0001", serde_json::json!({})),
        kept("TOKEN", "token-app-0002", serde_json::json!({ "repo": "Acme/App" })),
        kept("TOKEN", "token-prod-0003", serde_json::json!({ "environment": "production" })),
        kept("NPM_TOKEN", "npm-release-0004", serde_json::json!({ "branch": "release/*" })),
        kept("NPM_TOKEN", "npm-ci-0005", serde_json::json!({ "workflow": "ci.yml" })),
        kept("UNNAMED", "unnamed-0006", serde_json::json!({})),
        kept("OTHER", "other-0007", serde_json::json!({ "repo": "acme/web" })),
        ("SECRET_BROKEN_ALL".into(), "not json".into()),
        ("MOVE_TOKEN".into(), "move-token-0123456789".into()),
    ]);
    assert_eq!((config.secrets.len(), config.device_keys.len()), (7, 1));
    block_on(store.put("dkey:DEVICE_KEY_4102444800_abc", serde_json::json!({ "device": "Build Mac" }).to_string())).unwrap();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let workflow = "jobs:\n  test:\n    runs-on: superci\n    steps:\n      - run: echo ${{ secrets.token }} ${{ secrets.NPM_TOKEN }} ${{ secrets.MISSING }} ${{ secrets.OTHER }}\n  deploy:\n    runs-on: superci\n    environment:\n      name: ${{ 'production' }}\n    steps:\n      - run: echo ${{ secrets.TOKEN }}\n";
    let made = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app", "ref": "refs/heads/main", "secrets": [["npm_token", "npm-cli-0008"]] }).to_string())));
    assert_eq!(made.status, 200, "{}", made.body_text());
    // Named and neither kept for this run nor given: said (and empty, as on GitHub).
    assert_eq!(json(&made)["missing_secrets"], serde_json::json!(["MISSING", "OTHER"]));
    let run_id = json(&made)["run_id"].as_str().unwrap().to_string();
    let record: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{run_id}")]).unwrap();
    let secrets_of = |i: usize| -> Vec<(String, String)> {
        let h: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("rjob:{}", record["jobs"][i]["token"].as_str().unwrap())]).unwrap();
        let mut v: Vec<(String, String)> = h["message"]["variables"].as_object().unwrap().iter().filter(|(_, v)| v["isSecret"] == true).map(|(k, v)| (k.clone(), v["value"].as_str().unwrap().to_string())).collect();
        v.sort();
        v
    };
    // The repository's over the CI's; the value given with the run over any kept; the environment's for the job that
    // has it.
    assert_eq!(secrets_of(0), [("TOKEN".to_string(), "token-app-0002".to_string()), ("npm_token".into(), "npm-cli-0008".into())]);
    assert_eq!(secrets_of(1), [("TOKEN".to_string(), "token-prod-0003".to_string()), ("npm_token".into(), "npm-cli-0008".into())]);
    // What the run does not name, or may not have, is nowhere in it.
    assert!(!store.0.borrow().values().any(|v| v.contains("unnamed-0006") || v.contains("other-0007") || v.contains("npm-release-0004")));
    // Whoever looks after the CI sees names, scopes and each one's last use (the run, who started it), never a value;
    // nobody else sees them.
    let listed = block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/secrets")).with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!(listed.status, 200);
    assert!(!listed.body_text().contains("-000"), "{}", listed.body_text());
    let l = json(&listed);
    let rows: Vec<(String, String, Option<String>)> = l["secrets"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap().to_string(), s["said"].as_str().unwrap().to_string(), s["used"]["by"].as_str().map(str::to_string))).collect();
    assert_eq!(rows, [
        ("NPM_TOKEN".to_string(), "branch release/*".to_string(), None), ("NPM_TOKEN".into(), "workflow ci.yml".into(), Some("Build Mac".into())),
        ("OTHER".into(), "repo acme/web".into(), None),
        ("TOKEN".into(), "environment production".into(), Some("Build Mac".into())), ("TOKEN".into(), "every run".into(), Some("Build Mac".into())), ("TOKEN".into(), "repo Acme/App".into(), Some("Build Mac".into())),
        ("UNNAMED".into(), "every run".into(), None),
    ]);
    assert_eq!(l["secrets"][1]["used"]["run_id"], run_id.as_str());
    assert_eq!(block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/secrets")))).status, 404);
    // A move carries them along, as kept.
    let carried = json(&block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/move/export")).with_header("authorization", "Bearer dashboard-session-key-1").with_header("x-move-token", "move-token-0123456789"))));
    let names: Vec<&str> = carried["secrets"].as_array().unwrap().iter().map(|p| p[0].as_str().unwrap()).collect();
    assert!(names.len() == 7 && names.iter().all(|n| n.starts_with("SECRET_")), "{names:?}");
    assert!(carried["secrets"][0][1].as_str().unwrap().contains("-000"));
    // Cancelled, the run keeps nothing of them.
    let cancelled = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs/{run_id}/cancel")).with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!(cancelled.status, 200);
    assert!(!store.0.borrow().values().any(|v| v.contains("token-") || v.contains("npm-")), "nothing of them left once cancelled");
}

#[test]
fn a_run_from_a_computer_of_several_jobs_starts_each_once_what_it_needs_has_ended() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let workflow = "jobs:\n  build:\n    runs-on: superci\n    outputs:\n      version: ${{ steps.v.outputs.version }}\n    steps:\n      - id: v\n        run: echo version=1.2 >> $GITHUB_OUTPUT\n  test:\n    needs: build\n    if: needs.build.outputs.version == '1.2'\n    runs-on: superci/cpu=${{ matrix.cpu }}\n    strategy:\n      matrix:\n        cpu: [2, 4]\n    steps:\n      - run: echo ${{ matrix.cpu }}\n  report:\n    needs: test\n    if: failure()\n    runs-on: superci\n    steps:\n      - run: echo something failed\n";
    let made = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string())));
    assert_eq!(made.status, 200, "{}", made.body_text());
    // (A job that calls another repository's workflow is refused, with what does run.)
    let other = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": "jobs:\n  call:\n    uses: acme/shared/.github/workflows/ci.yml@v1\n", "path": ".github/workflows/call.yml", "repository": "acme/app" }).to_string())));
    assert!(other.status == 400 && other.body_text().contains("a workflow of another repository is not run from a computer yet"), "{}", other.body_text());
    let run_id = json(&made)["run_id"].as_str().unwrap().to_string();
    let record = || -> serde_json::Value { serde_json::from_str(&store.0.borrow()[&format!("run:{run_id}")]).unwrap() };
    let states = || record()["jobs"].as_array().unwrap().iter().map(|j| format!("{}={}{}", j["name"].as_str().or(j["key"].as_str()).unwrap(), j["state"].as_str().unwrap(), j["result"].as_str().map(|r| format!(":{r}")).unwrap_or_default())).collect::<Vec<_>>();
    // Only the build starts: the tests need it, the report needs them.
    assert_eq!(states(), ["build=started", "test=pending", "test=pending", "report=pending"]);
    assert_eq!(launches(&clouds).len(), 1);
    // A job's runner, as a real one: takes its job, and says it ended (with these outputs).
    let finish = |i: usize, outputs: &str| {
        let token = record()["jobs"][i]["token"].as_str().unwrap().to_string();
        for line in include_str!("fixtures/runner/runner-requests.jsonl").lines() {
            let r: serde_json::Value = serde_json::from_str(line).unwrap();
            let path = r["path"].as_str().unwrap().trim_start_matches("/runner/server");
            let mut req = Request::new(r["method"].as_str().unwrap(), &format!("{PLANE_URL}/runner/{token}{path}"));
            req.body = r["request"].as_str().unwrap().replace("\"outputs\":{}", &format!("\"outputs\":{outputs}")).into_bytes();
            run(req);
        }
    };
    finish(0, r#"{"version":{"value":"1.2"}}"#);
    // Its output known, both tests start (each on the machine its matrix asks for), knowing what they need.
    assert_eq!(states(), ["build=done:success", "test (2)=started", "test (4)=started", "report=pending"]);
    assert_eq!(launches(&clouds).iter().skip(1).map(|l| field(l, "InstanceType")).collect::<Vec<_>>(), ["m8a.large", "m8a.xlarge"]);
    let message = |i: usize| -> serde_json::Value { let h: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("rjob:{}", record()["jobs"][i]["token"].as_str().unwrap())]).unwrap(); h["message"].clone() };
    let m = message(1);
    assert_eq!((m["jobDisplayName"].as_str(), &m["contextData"]["matrix"]), (Some("test (2)"), &serde_json::json!({ "t": 2, "d": [{ "k": "cpu", "v": 2.0 }] })));
    assert_eq!(m["contextData"]["needs"], serde_json::json!({ "t": 2, "d": [{ "k": "build", "v": { "t": 2, "d": [{ "k": "outputs", "v": { "t": 2, "d": [{ "k": "version", "v": "1.2" }] } }, { "k": "result", "v": "success" }] } }] }));
    assert_eq!(message(2)["contextData"]["strategy"], serde_json::json!({ "t": 2, "d": [{ "k": "fail-fast", "v": true }, { "k": "job-index", "v": 1.0 }, { "k": "job-total", "v": 2.0 }, { "k": "max-parallel", "v": 2.0 }] }));
    // One test ending is not enough for the report; both ending well, it is skipped (it runs on a failure).
    finish(1, "{}");
    assert_eq!(states()[3], "report=pending");
    finish(2, "{}");
    assert_eq!(states(), ["build=done:success", "test (2)=done:success", "test (4)=done:success", "report=done:skipped"]);
    assert_eq!(launches(&clouds).len(), 3, "no machine for a job skipped");
    let status = json(&run(key(Request::new("GET", &format!("{PLANE_URL}/runs/{run_id}")))));
    assert_eq!((status["over"].as_bool(), status["result"].as_str(), status["jobs"].as_array().map(|j| j.len())), (Some(true), Some("success"), Some(4)));
    // A job asked for by name brings what it needs, and nothing else.
    let one = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app", "job": "test" }).to_string())));
    let rid = json(&one)["run_id"].as_str().unwrap().to_string();
    let rec: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{rid}")]).unwrap();
    assert_eq!(rec["jobs"].as_array().unwrap().iter().map(|j| j["key"].as_str().unwrap()).collect::<Vec<_>>(), ["build", "test", "test"]);
}

/// The fake clouds, with this version's `superci-cache` in Virginia's cache bucket, and GitHub's keys for its jobs'
/// tokens (`keys`).
struct WithCache { clouds: FakeClouds, keys: serde_json::Value }
#[async_trait(?Send)]
impl Http for WithCache {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        if r.url == format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/{}", superci_core::cache::program_key("x86_64")) && r.method == "HEAD" {
            assert!(r.header("authorization").is_some_and(|a| a.contains("/us-east-1/s3/aws4_request")));
            return Ok(Response::new(200, "application/octet-stream", ""));
        }
        if r.url == "https://token.actions.githubusercontent.com/.well-known/jwks" { return Ok(Response::new(200, "application/json", self.keys.to_string())) }
        if r.url == "https://api.github.com/repositories/77" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 77, "full_name": "acme/app" }).to_string())) }
        if r.url == "https://api.github.com/repos/acme/app" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 77, "default_branch": "main" }).to_string())) }
        self.clouds.send(r).await
    }
}

/// GitHub's key for its jobs' tokens (one for every test: making it is slow), as GitHub lists it.
fn github_jobs_key() -> (&'static rsa::RsaPrivateKey, serde_json::Value) {
    use rsa::traits::PublicKeyParts;
    static KEY: std::sync::OnceLock<rsa::RsaPrivateKey> = std::sync::OnceLock::new();
    let key = KEY.get_or_init(|| rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap());
    let b = |v: &[u8]| URL_SAFE_NO_PAD.encode(v);
    (key, serde_json::json!({ "keys": [{ "kty": "RSA", "kid": "gh1", "alg": "RS256", "n": b(&key.n().to_bytes_be()), "e": b(&key.e().to_bytes_be()) }] }))
}

/// A job's token as GitHub signs it: its cache scopes (`ac`) in a repository.
fn github_job_token(ac: &str, repo: &str) -> String {
    use rsa::signature::{SignatureEncoding, Signer};
    let b = |v: &[u8]| URL_SAFE_NO_PAD.encode(v);
    let claims = serde_json::json!({ "iss": "https://token.actions.githubusercontent.com", "ac": ac, "acsl": "10", "repository_id": repo, "run_id": "9", "nbf": 1_789_999_000, "exp": 1_790_020_000 });
    let unsigned = format!("{}.{}", b(br#"{"alg":"RS256","kid":"gh1","typ":"JWT"}"#), b(claims.to_string().as_bytes()));
    format!("{unsigned}.{}", b(&rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(github_jobs_key().0.clone()).sign(unsigned.as_bytes()).to_vec()))
}

#[test]
fn a_machine_keeps_githubs_cache_in_its_regions_bucket_as_its_jobs_token_allows() {
    let (_, keys) = github_jobs_key();
    let github_token = github_job_token;
    let b = |v: &[u8]| URL_SAFE_NO_PAD.encode(v);
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithCache { clouds: FakeClouds::default(), keys };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    run(webhook("queued", None));
    // The machine fetches superci-cache with a link that does only that, and has a link of its own to the cache.
    let launch = launches(&http.clouds).pop().expect("a machine");
    let script = String::from_utf8(STANDARD.decode(field(&launch, "UserData")).unwrap()).unwrap();
    // (The cache's part of its start is fetched from the machine's link: AWS takes 16 KB of start-up script.)
    let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    assert!(script.find("$c/cache|bash").unwrap() < script.find("cd /home/runner && ").unwrap(), "{script}");
    let cache = String::from_utf8(run(get(&format!("/cache/{link}/cache"))).body).unwrap();
    assert!(cache.starts_with(&format!("c='{PLANE_URL}/cache/{link}'")) && cache.contains(r#"curl -fsSL --retry 2 -m 30 -o $f "$c/program""#) && cache.contains("$f wrap /home/runner/externals") && cache.contains("TURBO_API=%s") && cache.contains("/etc/bazel.bazelrc") && !cache.contains("# "), "{cache}");
    assert_eq!(run(get("/cache/someone-elses-link-here/cache")).status, 404);
    let program = run(get(&format!("/cache/{link}/program")));
    let to = program.header("location").unwrap();
    assert_eq!(program.status, 302);
    assert!(to.starts_with(&format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/superci/cache-")) && to.contains("-x86_64?X-Amz-Algorithm=AWS4-HMAC-SHA256") && to.contains("X-Amz-Expires=600&X-Amz-Security-Token=tok"), "{to}");
    assert_eq!(run(get("/cache/someone-elses-link-here/program")).status, 404);
    let call = |what: &str, body: serde_json::Value| { let r = run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string())); (r.status, json(&r)) };
    let main = github_token(r#"[{"Scope":"refs/heads/main","Permission":3}]"#, "77");
    let pr = github_token(r#"[{"Scope":"refs/pull/5/merge","Permission":3},{"Scope":"refs/heads/main","Permission":1}]"#, "77");
    let v = "a1b2c3";

    // main saves: begun, links to write its pieces in Virginia's bucket, finished.
    assert_eq!(call("begin", serde_json::json!({ "token": main, "key": "deps-1", "version": v })), (200, serde_json::json!({ "ok": true })));
    let (status, signed) = call("sign", serde_json::json!({ "token": main, "key": "deps-1", "version": v, "names": ["b0", "b1"] }));
    assert_eq!(status, 200);
    let put = signed["urls"][0].as_str().unwrap();
    assert!(put.starts_with(&format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/c/gh77/")) && put.contains("/b0?X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{put}");
    assert_eq!(call("begin", serde_json::json!({ "token": main, "key": "deps-1", "version": v })).0, 409, "being saved already");
    assert_eq!(call("finish", serde_json::json!({ "token": main, "key": "deps-1", "version": v, "pieces": [["b0", 100], ["b1", 20]] })), (200, serde_json::json!({ "ok": true, "size": 120 })));

    // A pull request finds it in main (by a restore key), with links to read it; it may not write main's.
    let (status, hit) = call("lookup", serde_json::json!({ "token": pr, "key": "deps-2", "restore_keys": ["deps-"], "version": v }));
    assert_eq!((status, hit["hit"].as_bool(), hit["key"].as_str(), hit["size"].as_u64()), (200, Some(true), Some("deps-1"), Some(120)));
    assert!(hit["pieces"][1]["url"].as_str().unwrap().contains("/b1?") && hit["pieces"][1]["size"] == 20);
    assert_eq!(call("begin", serde_json::json!({ "token": pr, "key": "deps-1", "version": v })).0, 200, "its own scope: a new entry");
    // Another repository sees nothing of it.
    assert_eq!(call("lookup", serde_json::json!({ "token": github_token(r#"[{"Scope":"refs/heads/main","Permission":3}]"#, "78"), "key": "deps-1", "version": v })).1["hit"], false);

    // Refused: a token changed by hand, one signed by another key, no token, and a link no machine has.
    let mut forged: Vec<String> = main.split('.').map(str::to_string).collect();
    forged[1] = b(br#"{"iss":"https://token.actions.githubusercontent.com","ac":"[{\"Scope\":\"refs/heads/main\",\"Permission\":3}]","repository_id":"78","exp":1790020000}"#);
    assert_eq!(call("lookup", serde_json::json!({ "token": forged.join("."), "key": "deps-1", "version": v })).0, 403);
    assert_eq!(call("lookup", serde_json::json!({ "key": "deps-1", "version": v })).0, 403);
    let r = run(Request::new("POST", &format!("{PLANE_URL}/cache/not-a-machines-link-at-all/lookup")).with_body(serde_json::json!({ "token": main, "key": "deps-1", "version": v }).to_string()));
    assert_eq!(r.status, 404);

    // A run from a computer: its job's token (the control plane's) reads the repository's caches as GitHub's jobs
    // keep them, and writes its own scope only; its actions are told to use cache version 2, as GitHub's are.
    let made = run(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": "jobs:\n  t:\n    runs-on: superci\n    steps:\n      - run: echo hi\n", "path": ".github/workflows/ci.yml", "repository": "acme/app", "ref": "refs/heads/feature" }).to_string()));
    assert_eq!(made.status, 200, "{}", made.body_text());
    let handed: serde_json::Value = store.0.borrow().iter().find(|(k, _)| k.starts_with("rjob:")).map(|(_, v)| serde_json::from_str(v).unwrap()).unwrap();
    let message = &handed["message"];
    let local = message["resources"]["endpoints"][0]["authorization"]["parameters"]["AccessToken"].as_str().unwrap().to_string();
    assert_eq!(message["variables"]["actions_uses_cache_service_v2"]["value"], "true");
    let (status, hit) = call("lookup", serde_json::json!({ "token": local, "key": "deps-1", "version": v }));
    assert_eq!((status, hit["hit"].as_bool(), hit["key"].as_str()), (200, Some(true), Some("deps-1")), "main's, found from the feature branch's run");
    assert_eq!(call("begin", serde_json::json!({ "token": local, "key": "deps-local", "version": v })).0, 200);
    assert!(store.0.borrow().contains_key(&format!("cache:gh77:refs/superci/computer:{v}:deps-local")));
    assert_eq!(call("lookup", serde_json::json!({ "token": main, "key": "deps-local", "version": v })).1["hit"], false, "not main's");

    // Listed, by repository (named as GitHub names it), and one repository's entries; then taken out.
    let dash = |r: Request| run(r.with_header("authorization", "Bearer dashboard-session-key-1"));
    let listed = json(&dash(get("/caches")));
    assert_eq!((listed["on"].as_bool(), listed["size_gb"].as_u64(), listed["days"].as_u64()), (Some(true), Some(25), Some(7)));
    assert_eq!((listed["repos"][0]["name"].as_str(), listed["repos"][0]["entries"].as_u64(), listed["repos"][0]["bytes"].as_u64()), (Some("acme/app"), Some(1), Some(120)));
    assert_eq!(json(&dash(get("/caches?repo=acme/app")))["entries"][0]["key"], "deps-1");
    assert_eq!(run(get("/caches")).status, 404, "a key is needed");
    let removed = dash(Request::new("POST", &format!("{PLANE_URL}/caches/delete")).with_body(serde_json::json!({ "repo": "acme/app", "key": "deps-1" }).to_string()));
    assert_eq!(json(&removed)["removed"], 2, "main's, and the one the pull request began");
    assert!(http.clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1.contains("/c/gh77/") && c.1.ends_with("/b1")), "its pieces deleted from the bucket");
    assert_eq!(call("lookup", serde_json::json!({ "token": pr, "key": "deps-1", "version": v })).1["hit"], false);
    // A repository with nothing kept takes nothing out; one named by its namespace (as kept) is found by it.
    let delete = |repo: &str| json(&dash(Request::new("POST", &format!("{PLANE_URL}/caches/delete")).with_body(serde_json::json!({ "repo": repo }).to_string())))["removed"].clone();
    assert_eq!(delete("acme/nothing"), 0);
    assert_eq!(delete("gh77"), 1, "what is left of it (deps-2's entry, the pull request's)");
    for ns in ["gl1-5", "local-abc"] { assert_eq!(delete(ns), 0, "{ns}") }
}

#[test]
fn a_label_asking_for_more_than_allowed_is_said_so() {
    let routing = superci_core::plane::Routing::default();
    let spec = |cpu: Option<u32>, ram: Option<u32>| superci_core::spec::Spec { cpu, ram_gb: ram, ..Default::default() };
    assert!(routing.too_large(&spec(Some(32), Some(256))).is_none());
    assert_eq!(routing.too_large(&spec(Some(33), None)).as_deref(), Some("it asks for 33 CPUs; the most allowed is 32 (set in the dashboard)"));
    assert_eq!(routing.too_large(&spec(None, Some(257))).as_deref(), Some("it asks for 257 GB of memory; the most allowed is 256 GB (set in the dashboard)"));
    let small = superci_core::plane::Routing { max_cpu: Some(8), ..Default::default() };
    assert!(small.too_large(&spec(None, Some(65))).is_some() && small.too_large(&spec(Some(8), Some(64))).is_none());
}

/// A bucket with this version's `superci-cache` and `superci-layers` and objects put in it (kept with their headers),
/// the registries as they answered (`fixtures/registries`: Docker Hub, GHCR and ECR Public, recorded 2026-10-17), and
/// Lambda taking calls for the converter.
#[derive(Default)]
struct WithLayers { clouds: FakeClouds, objects: RefCell<BTreeMap<String, (Vec<u8>, Vec<(String, String)>)>>, converted: RefCell<Vec<serde_json::Value>>, deleted: RefCell<Vec<String>> }
const REGISTRIES: &str = include_str!("fixtures/registries/answers.jsonl");
const BUCKET: &str = "https://superci-plane-abc123def456-us-east-1.s3.us-east-1.amazonaws.com/";
#[async_trait(?Send)]
impl Http for WithLayers {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        if r.method == "HEAD" && (r.url == format!("{BUCKET}{}", superci_core::cache::program_key("x86_64")) || r.url == format!("{BUCKET}{}", superci_core::cache::layers_key("x86_64"))) {
            return Ok(Response::new(200, "application/octet-stream", ""));
        }
        if let Some(key) = r.url.strip_prefix(BUCKET).filter(|k| k.starts_with("layers/")) {
            assert!(r.header("authorization").is_some_and(|a| a.contains("/us-east-1/s3/aws4_request")));
            let mut objects = self.objects.borrow_mut();
            return Ok(match (r.method.as_str(), objects.get(key)) {
                ("GET", Some((body, _))) => Response::new(200, "application/json", body.clone()),
                ("HEAD", Some((body, headers))) => {
                    let mut res = Response::new(200, "application/octet-stream", "").with_header("content-length", &body.len().to_string());
                    if r.header("x-amz-checksum-mode") == Some("ENABLED") { for (k, v) in headers { res = res.with_header(k, v) } }
                    res
                }
                ("DELETE", _) => { objects.remove(key); self.deleted.borrow_mut().push(key.to_string()); Response::new(204, "text/plain", "") }
                _ => Response::new(404, "application/xml", "<Error><Code>NoSuchKey</Code></Error>"),
            });
        }
        if let Some(function) = r.url.strip_prefix("https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/").and_then(|f| f.strip_suffix("/invocations")) {
            assert_eq!((function, r.header("x-amz-invocation-type")), (format!("superci-plane-{PLANE_ID}-convert").as_str(), Some("Event")));
            assert!(r.header("authorization").is_some_and(|a| a.contains("/us-east-1/lambda/aws4_request")), "signed with its own role");
            self.converted.borrow_mut().push(serde_json::from_slice(&r.body).unwrap());
            return Ok(Response::new(202, "application/json", ""));
        }
        if r.url.contains("docker.io/") || r.url.starts_with("https://ghcr.io/") || r.url.starts_with("https://public.ecr.aws/") {
            // (A private repository answers alike whatever blob is asked for: as it answered for the one recorded.)
            let url = match r.url.split_once("/acme-private-xyz/app/blobs/") { Some((at, _)) => format!("{at}/acme-private-xyz/app/blobs/sha256:53f8f5e03afd86ade91b7aa57a749f5a3d1419c113be5d8c10e7ee61bb5ab887"), None => r.url.clone() };
            let answer = REGISTRIES.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|a| a["method"] == r.method.as_str() && a["url"] == url.as_str() && a["authorization"].as_str() == r.header("authorization") && a["range"].as_str() == r.header("range"))
                .unwrap_or_else(|| panic!("the registries were not asked {} {} ({:?})", r.method, r.url, r.header("authorization")));
            assert!(r.no_follow || r.url.contains("/token"), "a registry's redirect is an answer, not followed");
            let mut res = Response::new(answer["status"].as_u64().unwrap() as u16, "application/json", answer["body"].as_str().unwrap().to_string());
            for (k, v) in answer["headers"].as_object().unwrap() { res = res.with_header(k, v.as_str().unwrap()) }
            return Ok(res);
        }
        self.clouds.send(r).await
    }
}

impl WithLayers {
    /// What a machine uploading at a link would leave (S3 keeps the SHA-256 it was given with it).
    fn uploaded(&self, link: &serde_json::Value, body: &[u8]) {
        let key = link["url"].as_str().unwrap().strip_prefix(BUCKET).unwrap().split('?').next().unwrap().to_string();
        self.objects.borrow_mut().insert(key, (body.to_vec(), vec![("x-amz-checksum-sha256".into(), STANDARD.encode(Sha256::digest(body)))]));
    }
    /// The same for a layer whose bytes the test does not have: what S3 keeps once it took a body of that SHA-256.
    fn uploaded_as(&self, link: &serde_json::Value, digest: &str, size: usize) {
        let key = link["url"].as_str().unwrap().strip_prefix(BUCKET).unwrap().split('?').next().unwrap().to_string();
        let sum: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&digest[7 + 2 * i..9 + 2 * i], 16).unwrap()).collect();
        self.objects.borrow_mut().insert(key, (vec![0; size], vec![("x-amz-checksum-sha256".into(), STANDARD.encode(sum))]));
    }
    /// What the converter would write when done with a layer it was given (a blob from staging taken from there).
    fn converter_done(&self, job: &serde_json::Value, error: Option<&str>) {
        let done = match error {
            Some(e) => serde_json::json!({ "digest": job["digest"], "error": e }),
            None => serde_json::json!({ "digest": job["digest"], "size": job["size"], "diff_id": "sha256:d1ff", "erofs": { "key": job["erofs_key"], "size": 9000, "digest": "sha256:e0f5" }, "blob": { "key": job["blob_key"], "size": job["size"] } }),
        };
        let mut objects = self.objects.borrow_mut();
        if let Some(staged) = job["source"]["key"].as_str() { objects.remove(staged); }
        objects.insert(job["done_key"].as_str().unwrap().to_string(), (done.to_string().into_bytes(), vec![]));
    }
}

/// A queued job on Ubuntu 26.04 of a repository GitHub says the id and branch of (the layers it may read are its own).
fn layered(job: u64, repo_id: u64, repo: &str) -> Request {
    let payload = serde_json::json!({ "action": "queued", "workflow_job": { "id": job, "run_id": 3, "labels": ["superci/image=ubuntu26"], "head_branch": "main" }, "repository": { "id": repo_id, "full_name": repo, "private": true, "default_branch": "main" }, "installation": { "id": 42 } }).to_string();
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
    Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload)
}

#[test]
fn ubuntu_26_machines_keep_their_jobs_images_and_a_repository_that_used_some_has_them_from_the_start() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithLayers::default();
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let machine = |job_id: u64, label: &str| -> (String, Vec<(String, String)>) {
        run(labelled("queued", job_id, "acme/app", None, label));
        let launch = launches(&http.clouds).pop().expect("a machine");
        (String::from_utf8(STANDARD.decode(field(&launch, "UserData")).unwrap()).unwrap(), launch)
    };
    let link_of = |script: &str| script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    // A repository with nothing known yet: Docker exactly as it comes, no disk of the store's; its images kept after
    // the job (the program fetched then), before the machine goes off.
    let (script, launch) = machine(1, "superci/image=ubuntu26");
    assert!(!script.contains("/layers|bash") && script.contains("\n(systemctl restart docker &>/dev/null &)") && field(&launch, "BlockDeviceMapping.2.DeviceName").is_empty(), "{script}");
    let keep = script.find("$f keep --link=\"$c\" --most=180").expect("kept after the job");
    assert!(script.find("cd /home/runner && ").unwrap() < keep && keep < script.rfind("\npoweroff").unwrap() && script.contains("curl -fsSL --retry 2 -m 60 -o $f \"$c/layers-program\""), "{script}");
    assert_eq!(job(&store, 1)["layers"], serde_json::Value::Null);
    // After it, its machine says what it pulled: on the job's record, and for the repository's next machine.
    let first = link_of(&script);
    let kept = |link: &str, served: u64, pulled_bytes: u64| run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/layers-kept"))
        .with_body(serde_json::json!({ "served": served, "served_bytes": served * 1000, "pulled": 5, "pulled_bytes": pulled_bytes, "uploaded": 5, "uploaded_bytes": pulled_bytes, "seconds": 3.1 }).to_string()));
    // As the job ended, before keeping: what it pulled, on the job's record (for the run's summary); then what was kept,
    // which a late first word does not take back.
    let early = || run(Request::new("POST", &format!("{PLANE_URL}/cache/{first}/layers-kept")).with_body(serde_json::json!({ "served": 0, "pulled": 5, "pulled_bytes": 300 << 20, "keeping": true }).to_string()));
    assert_eq!(early().status, 200);
    assert_eq!((job(&store, 1)["images"]["keeping"].clone(), job(&store, 1)["images"]["pulled"].clone()), (serde_json::json!(true), serde_json::json!(5)));
    // (After its job: the runner's own record has gone by then; the machine's link knows its job.)
    assert_eq!(kept(&first, 0, 300 << 20).status, 200);
    assert_eq!(job(&store, 1)["images"]["pulled_bytes"], 300u64 << 20);
    assert_eq!(early().status, 200);
    assert_eq!((job(&store, 1)["images"]["keeping"].clone(), job(&store, 1)["images"]["uploaded"].clone()), (serde_json::Value::Null, serde_json::json!(5)));
    assert_eq!(kept("someone-elses-link-here", 0, 1).status, 404);
    // The next: the store from its start, on a disk of its own, in its price.
    let (script, _) = machine(2, "superci/image=ubuntu26");
    assert!(script.contains("/layers|bash||(systemctl restart docker &>/dev/null &)") && !script.contains("\n(systemctl restart docker &>/dev/null &)") && script.find("/layers|bash").unwrap() < script.find("cd /home/runner && ").unwrap() && script.contains("$f keep"), "{script}");
    assert_eq!(job(&store, 2)["layers"], true);
    // Again, after a run that used images.
    kept(&link_of(&script), 40, 4 << 30);
    let (script, launch) = machine(3, "superci/image=ubuntu26");
    assert!(script.contains("/layers|bash"));
    assert_eq!((field(&launch, "BlockDeviceMapping.2.DeviceName"), field(&launch, "BlockDeviceMapping.2.Ebs.VolumeSize"), field(&launch, "BlockDeviceMapping.2.Ebs.Throughput")), ("/dev/sdf".to_string(), "20".to_string(), "500".to_string()));
    assert_eq!(job(&store, 3)["layers"], true);
    let store_disk = superci_core::plane::store_disk_usd_per_hour(superci_core::aws::disk_gb_month("us-east-1"), true);
    assert!(store_disk > 0.01 && store_disk < 0.03, "{store_disk}");
    // Its link: the script, the program, the index with links to its files (and again for some).
    let link = link_of(&script);
    let get = |p: &str| run(Request::new("GET", &format!("{PLANE_URL}/cache/{link}/{p}")));
    let s = get("layers");
    let body = String::from_utf8(s.body.clone()).unwrap();
    assert!(s.status == 200 && body.contains(&format!("'{PLANE_URL}/cache/{link}/layers-program'")) && body.contains(&format!("$f setup --index-url='{PLANE_URL}/cache/{link}/layers.json' || (systemctl restart docker")) && body.starts_with("exec >>/var/log/superci-layers.log"), "{body}");
    let p = get("layers-program");
    assert!(p.status == 302 && p.header("location").unwrap().contains(&format!("/superci/layers-{}-x86_64?X-Amz-", superci_core::plane::VERSION)));
    assert_eq!(json(&get("layers.json")), serde_json::json!({ "layers": {}, "refresh": { "url": format!("{PLANE_URL}/cache/{link}/layers-links"), "token": "" } }));
    assert_eq!(run(Request::new("GET", &format!("{PLANE_URL}/cache/someone-elses-link-here/layers.json"))).status, 404);
    // A job never run before (a new workflow's): the store from its start too, as its repository's jobs used images;
    // once its own run used none, Docker as it comes for it. Another repository's new job: Docker as it comes.
    let labelled_named = |job_id: u64, repo: &str, workflow: &str, name: &str, label: &str| -> (String, Vec<(String, String)>) {
        let payload = serde_json::json!({ "action": "queued", "workflow_job": { "id": job_id, "run_id": 3, "labels": [label], "name": name, "workflow_name": workflow }, "repository": { "full_name": repo, "private": true }, "installation": { "id": 42 } }).to_string();
        let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", payload.as_bytes())));
        run(Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(payload));
        let launch = launches(&http.clouds).pop().expect("a machine");
        (String::from_utf8(STANDARD.decode(field(&launch, "UserData")).unwrap()).unwrap(), launch)
    };
    let named = |job_id: u64, repo: &str, workflow: &str, name: &str| labelled_named(job_id, repo, workflow, name, "superci/image=ubuntu26");
    let (script, _) = named(5, "acme/app", "End to end", "e2e");
    assert!(script.contains("/layers|bash"), "{script}");
    assert_eq!(job(&store, 5)["layers"], true);
    kept(&link_of(&script), 0, 1 << 20);
    let (script, _) = named(6, "acme/app", "End to end", "e2e");
    assert!(!script.contains("/layers|bash"), "{script}");
    let (script, _) = named(7, "acme/app", "Lint", "lint");
    assert!(script.contains("/layers|bash"), "a job using none leaves its repository's word as it was: {script}");
    let (script, _) = named(8, "acme/other", "End to end", "e2e");
    assert!(!script.contains("/layers|bash"), "{script}");
    // A label's `docker-cache`: false, Docker as it comes and nothing kept, whatever was used before; true, the store
    // from the start, whatever was.
    let (script, launch) = machine(9, "superci/image=ubuntu26/docker-cache=false");
    assert!(!script.contains("/layers|bash") && !script.contains("$f keep") && field(&launch, "BlockDeviceMapping.2.DeviceName").is_empty(), "{script}");
    let (script, _) = labelled_named(10, "acme/other", "End to end", "e2e", "superci/image=ubuntu26/docker-cache=true");
    assert!(script.contains("/layers|bash") && script.contains("$f keep"), "{script}");
    // Ubuntu 24.04 (Docker 28): as before, nothing of the store's.
    let (script, launch) = machine(4, "superci/image=ubuntu24");
    assert!(!script.contains("/layers|bash") && !script.contains("$f keep") && field(&launch, "BlockDeviceMapping.2.DeviceName").is_empty());
    assert_eq!(job(&store, 4)["layers"], serde_json::Value::Null);
}

#[test]
fn the_plan_for_a_jobs_machine_follows_what_its_last_run_used() {
    use superci_core::layers::{plan, Images};
    let now = 1_790_000_000_000;
    let used = |served_bytes: u64, pulled_bytes: u64, at: u64| Images { served_bytes, pulled_bytes, at, ..Default::default() };
    assert!(!plan(None, now), "nothing known: Docker as it comes");
    assert!(!plan(Some(&used(0, 0, now)), now), "no images used");
    assert!(plan(Some(&used(1 << 30, 0, now)), now) && plan(Some(&used(200 << 20, 56 << 20, now)), now));
    assert!(!plan(Some(&used(3 << 20, 70 << 20, now)), now), "small images: as fast pulled");
    assert!(!plan(Some(&used(1 << 30, 0, now - 8 * 86_400_000)), now), "a week old: not counted");
}

#[test]
fn machines_fill_the_layer_store_and_each_repository_reads_only_what_it_may() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithLayers::default();
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let link = || {
        let script = String::from_utf8(STANDARD.decode(field(&launches(&http.clouds).pop().expect("a machine"), "UserData")).unwrap()).unwrap();
        script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string()
    };
    run(layered(1, 77, "acme/app"));
    let app = link();
    run(layered(2, 88, "acme/other"));
    let other = link();
    let post = |link: &str, what: &str, body: serde_json::Value| run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string()));
    let get = |link: &str, what: &str| json(&run(Request::new("GET", &format!("{PLANE_URL}/cache/{link}/{what}"))));
    let digests = |index: &serde_json::Value| index["layers"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    // Alpine's layer, which Docker Hub serves to anyone (as it answered); a private one; one too large; not a digest.
    const ALPINE: &str = "sha256:53f8f5e03afd86ade91b7aa57a749f5a3d1419c113be5d8c10e7ee61bb5ab887";
    const REDIS: &str = "sha256:16333ee0c00fc65e025a2a4f839703ad37a74728832977fbdf080984de1b8e5a";
    let blob = b"a private layer's blob".repeat(100);
    let private = format!("sha256:{}", hex(&Sha256::digest(&blob)));
    let big = format!("sha256:{}", "b".repeat(64));
    let answers = json(&post(&app, "layers-propose", serde_json::json!({ "layers": [
        { "digest": ALPINE, "size": 3792075, "sources": [{ "registry": "ghcr.io", "repository": "actions/actions-runner" }, { "registry": "docker.io", "repository": "library/alpine" }] },
        { "digest": private, "size": blob.len(), "sources": [{ "registry": "docker.io", "repository": "acme-private-xyz/app" }, { "registry": "ghcr.io", "repository": "acme-private-xyz/app" }, { "registry": "registry.acme.internal", "repository": "app" }] },
        { "digest": REDIS, "size": 3626020, "sources": [{ "registry": "public.ecr.aws", "repository": "docker/library/redis" }] },
        { "digest": big, "size": 5u64 << 30 }, { "digest": "sha256:nope", "size": 1 }] })));
    // Every layer from the machine that pulled it, after its job; too large or not a digest, none.
    assert_eq!((answers["layers"][ALPINE]["do"].as_str(), answers["layers"][REDIS]["do"].as_str(), answers["layers"][&big]["do"].as_str(), answers["layers"].get("sha256:nope")), (Some("upload"), Some("upload"), Some("no"), None), "{answers}");
    assert!(http.converted.borrow().is_empty(), "nothing fetched from a registry");
    assert_eq!(answers["uploaded"], format!("{PLANE_URL}/cache/{app}/layers-uploaded"));
    // Alpine's, which Docker Hub serves to anyone: uploaded, converted from staging, and readable by every repository.
    let alpine_up = answers["layers"][ALPINE].clone();
    http.uploaded_as(&alpine_up, ALPINE, 3792075);
    assert_eq!(json(&post(&app, "layers-uploaded", serde_json::json!({ "id": alpine_up["id"] })))["do"], "converting");
    let given = http.converted.borrow()[0].clone();
    assert!(given["source"]["key"].as_str().unwrap().starts_with(&format!("layers/staging/{}-", &ALPINE[7..])), "{given}");
    assert_eq!((given["region"].as_str(), given["bucket"].as_str(), given["size"].as_u64()), (Some("us-east-1"), Some(format!("superci-plane-{PLANE_ID}-us-east-1").as_str()), Some(3792075)));
    assert_eq!((given["erofs_key"].as_str().unwrap(), given["blob_key"].as_str().unwrap(), given["done_key"].as_str().unwrap()), (&format!("layers/{}.erofs", &ALPINE[7..])[..], &format!("layers/{}.blob", &ALPINE[7..])[..], &format!("layers/done/{}.json", &ALPINE[7..])[..]));
    // Private: a link that takes only a body of its SHA-256.
    let up = answers["layers"][&private].clone();
    let sum = STANDARD.encode(Sha256::digest(&blob));
    assert_eq!((up["do"].as_str(), up["headers"]["x-amz-checksum-sha256"].as_str()), (Some("upload"), Some(sum.as_str())), "{up}");
    assert!(up["url"].as_str().unwrap().starts_with(&format!("{BUCKET}layers/staging/{}-", &private[7..])) && up["url"].as_str().unwrap().contains("X-Amz-SignedHeaders=host%3Bx-amz-checksum-sha256"), "{up}");
    // Offered by another repository while it is converted: public, nothing more to do. Something else at the link:
    // refused.
    assert_eq!(json(&post(&other, "layers-propose", serde_json::json!({ "layers": [{ "digest": ALPINE, "size": 3792075 }] })))["layers"][ALPINE]["do"], "have");
    http.uploaded(&up, &blob[1..]);
    assert_eq!(post(&app, "layers-uploaded", serde_json::json!({ "id": up["id"] })).status, 409);
    assert_eq!(post(&other, "layers-uploaded", serde_json::json!({ "id": up["id"] })).status, 404, "another repository's machine was not given it");
    http.uploaded(&up, &blob);
    assert_eq!(json(&post(&app, "layers-uploaded", serde_json::json!({ "id": up["id"] })))["do"], "converting");
    let staged = http.converted.borrow()[1].clone();
    assert!(staged["source"]["key"].as_str().unwrap().starts_with(&format!("layers/staging/{}-", &private[7..])) && staged["digest"] == private.as_str(), "{staged}");
    // The converter is done with both: the sweep records them. Each repository reads what it may.
    http.converter_done(&given, None);
    http.converter_done(&staged, None);
    block_on(plane.alarm()).unwrap();
    assert!(http.objects.borrow().keys().all(|k| !k.starts_with("layers/done/")), "what the converter said is read once");
    let index = get(&app, "layers.json");
    let mut both = vec![ALPINE.to_string(), private.clone()];
    both.sort();
    assert_eq!(digests(&index), both);
    // Each with its links signed, so that nothing is asked for while Docker pulls.
    let alpine = &index["layers"][ALPINE];
    assert_eq!((alpine["size"].as_u64(), alpine["erofs"]["size"].as_u64(), alpine["erofs"]["digest"].as_str()), (Some(3792075), Some(9000), Some("sha256:e0f5")), "{alpine}");
    assert!(alpine["erofs"]["url"].as_str().unwrap().starts_with(&format!("{BUCKET}layers/{}.erofs?X-Amz-", &ALPINE[7..])) && alpine["blob"]["url"].as_str().unwrap().contains(".blob?X-Amz-"), "{alpine}");
    assert_eq!(digests(&get(&other, "layers.json")), [ALPINE]);
    // What a job's run used, its next machine fetches ahead as its store starts (kept with the repository's record,
    // not on the job's own).
    assert_eq!(index["layers"][ALPINE]["ahead"], false, "nothing used yet");
    assert_eq!(post(&app, "layers-kept", serde_json::json!({ "served": 0, "pulled": 1, "used": [ALPINE, "not-a-digest"] })).status, 200);
    assert_eq!(get(&app, "layers.json")["layers"][ALPINE]["ahead"], true);
    assert_eq!(get(&other, "layers.json")["layers"][ALPINE]["ahead"], false, "another job's run is not this one's");
    assert!(job(&store, 1)["images"].get("used").is_none(), "{}", job(&store, 1)["images"]);
    let links = |link: &str| json(&post(link, "layers-links", serde_json::json!({ "digests": [ALPINE, private] })));
    let theirs = links(&other);
    assert_eq!(digests(&theirs), [ALPINE], "a private layer of another repository answers as one the store lacks");
    assert!(theirs["layers"][ALPINE]["erofs"]["url"].as_str().unwrap().starts_with(&format!("{BUCKET}layers/{}.erofs?X-Amz-", &ALPINE[7..])) && theirs["layers"][ALPINE]["blob"]["url"].as_str().unwrap().contains(".blob?X-Amz-"));
    assert_eq!(digests(&links(&app)), both);
    // The other repository has the private layer too: it shows it (an upload), then reads it, with no second conversion.
    let theirs = json(&post(&other, "layers-propose", serde_json::json!({ "layers": [{ "digest": private, "size": blob.len() }] })))["layers"][&private].clone();
    assert_eq!(theirs["do"], "upload");
    http.uploaded(&theirs, &blob);
    assert_eq!(json(&post(&other, "layers-uploaded", serde_json::json!({ "id": theirs["id"] })))["do"], "granted");
    assert_eq!(http.converted.borrow().len(), 2);
    assert!(http.objects.borrow().keys().all(|k| !k.starts_with("layers/staging/")), "its blob is not kept twice");
    assert_eq!(digests(&get(&other, "layers.json")), both);
    assert_eq!(json(&post(&app, "layers-propose", serde_json::json!({ "layers": [{ "digest": private, "size": blob.len() }] })))["layers"][&private]["do"], "have");
    // A layer the converter refused is not asked for again for a while.
    let odd_blob = b"an unusual tar".repeat(10);
    let odd = format!("sha256:{}", hex(&Sha256::digest(&odd_blob)));
    let propose_odd = || json(&post(&app, "layers-propose", serde_json::json!({ "layers": [{ "digest": odd, "size": odd_blob.len() }] })))["layers"][&odd].clone();
    let up = propose_odd();
    http.uploaded(&up, &odd_blob);
    post(&app, "layers-uploaded", serde_json::json!({ "id": up["id"] }));
    http.converter_done(&http.converted.borrow()[2].clone(), Some("mkfs.erofs refused the layer"));
    block_on(plane.alarm()).unwrap();
    assert_eq!(propose_odd()["do"], "no");
    // With as many conversions under way as a region takes, an uploaded layer waits in the control plane's queue (its
    // machine is gone soon after); a sweep starts it once there is room.
    let busy: Vec<String> = (0..superci_core::layers::AT_ONCE).map(|i| format!("lconv:us-east-1:sha256:{i:064x}")).collect();
    for k in &busy { store.0.borrow_mut().insert(k.clone(), serde_json::json!({ "at": clock.0.get(), "size": 1 }).to_string()); }
    let asked = http.converted.borrow().len();
    let redis = json(&post(&app, "layers-propose", serde_json::json!({ "layers": [{ "digest": REDIS, "size": 3626020, "sources": [{ "registry": "public.ecr.aws", "repository": "docker/library/redis" }] }] })))["layers"][REDIS].clone();
    http.uploaded_as(&redis, REDIS, 3626020);
    assert_eq!(json(&post(&app, "layers-uploaded", serde_json::json!({ "id": redis["id"] })))["do"], "converting");
    assert_eq!(http.converted.borrow().len(), asked, "not converted yet");
    let wanted: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("lwant:us-east-1:{REDIS}")]).unwrap();
    assert_eq!(wanted["public"]["registry"], "public.ecr.aws", "ECR Public's, which answers no HEAD for a blob: asked for its first byte");
    block_on(plane.alarm()).unwrap();
    assert_eq!(http.converted.borrow().len(), asked, "still no room");
    for k in &busy { store.0.borrow_mut().remove(k); }
    block_on(plane.alarm()).unwrap();
    assert_eq!(http.converted.borrow().last().unwrap()["digest"], REDIS);
    assert!(!store.0.borrow().contains_key(&format!("lwant:us-east-1:{REDIS}")) && store.0.borrow().contains_key(&format!("lconv:us-east-1:{REDIS}")));
    // Six days on, Alpine is used again; two days later the private layer (unused a week) goes with its files, and
    // Docker Hub is asked again whether Alpine is still public (its word is a week old).
    // (Each on a machine of its day: a machine's link lasts as long as its job may.)
    clock.0.set(clock.0.get() + 6 * 86_400_000 + 3_600_000);
    run(layered(3, 88, "acme/other"));
    assert_eq!(digests(&json(&post(&link(), "layers-links", serde_json::json!({ "digests": [ALPINE] })))), [ALPINE]);
    clock.0.set(clock.0.get() + 2 * 86_400_000);
    block_on(plane.alarm()).unwrap();
    assert!(http.deleted.borrow().contains(&format!("layers/{}.erofs", &private[7..])) && http.deleted.borrow().contains(&format!("layers/{}.blob", &private[7..])), "{:?}", http.deleted.borrow());
    run(layered(4, 77, "acme/app"));
    assert_eq!(digests(&get(&link(), "layers.json")), [ALPINE]);
    let alpine: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("layer:us-east-1:{ALPINE}")]).unwrap();
    assert_eq!(alpine["public"]["checked"].as_u64(), Some(clock.0.get()), "asked again");
}

#[test]
fn docker_images_are_left_to_docker_when_the_cache_keeps_none() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithLayers::default();
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.cache.images_off = true;
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/image=ubuntu26")));
    let launch = launches(&http.clouds).pop().expect("a machine");
    let script = String::from_utf8(STANDARD.decode(field(&launch, "UserData")).unwrap()).unwrap();
    assert!(script.contains("$c/cache|bash") && !script.contains("/layers|bash") && !script.contains("$f keep") && field(&launch, "BlockDeviceMapping.2.DeviceName").is_empty());
    let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    assert_eq!(block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/cache/{link}/layers.json")))).status, 404);
}

#[test]
fn a_cache_turned_off_leaves_jobs_to_githubs() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithCache { clouds: FakeClouds::default(), keys: serde_json::json!({ "keys": [] }) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.cache = superci_core::cache::Settings { off: true, ..Default::default() };
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let launch = launches(&http.clouds).pop().expect("a machine");
    assert!(!String::from_utf8(STANDARD.decode(field(&launch, "UserData")).unwrap()).unwrap().contains("superci-cache"));
}

#[test]
fn turbo_nx_and_bazel_keep_their_artifacts_by_the_branch_github_said_the_job_is_for() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithCache { clouds: FakeClouds::default(), keys: serde_json::json!({ "keys": [] }) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    // The machine link of each job's machine (the last one started).
    let link_of = |r: Request| {
        run(r);
        let script = String::from_utf8(STANDARD.decode(field(&launches(&http.clouds).pop().unwrap(), "UserData")).unwrap()).unwrap();
        let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
        let cache = String::from_utf8(run(Request::new("GET", &format!("{PLANE_URL}/cache/{link}/cache"))).body).unwrap();
        assert!(cache.contains("TURBO_API=%s") && cache.contains("NX_SELF_HOSTED_REMOTE_CACHE_SERVER=%s") && cache.contains("build --remote_cache=$u/bazel"));
        link
    };
    let (main, feature) = (link_of(on_branch(7, "main")), link_of(on_branch(8, "feature")));
    let call = |link: &str, what: &str, body: serde_json::Value| { let r = run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string())); (r.status, json(&r)) };
    // main keeps turbo's artifact for a hash, with what turbo keeps beside it.
    let (status, begun) = call(&main, "tool-begin", serde_json::json!({ "tool": "turbo", "hash": "8eccf22c040ebab6" }));
    assert_eq!(status, 200);
    assert!(begun["url"].as_str().unwrap().contains("/c/gh77/") && begun["url"].as_str().unwrap().contains("/p00000?X-Amz-"));
    assert_eq!(call(&main, "tool-finish", serde_json::json!({ "tool": "turbo", "hash": "8eccf22c040ebab6", "size": 300, "meta": "{\"duration\":\"12\"}" })).0, 200);
    // The feature branch reads it (main's, after its own) with what was kept beside it.
    let (_, hit) = call(&feature, "tool-get", serde_json::json!({ "tool": "turbo", "hash": "8eccf22c040ebab6" }));
    assert_eq!((hit["hit"].as_bool(), hit["size"].as_u64(), hit["meta"].as_str()), (Some(true), Some(300), Some("{\"duration\":\"12\"}")));
    // What the feature branch keeps is its own: main does not see it, so a branch cannot put an artifact where main reads.
    call(&feature, "tool-begin", serde_json::json!({ "tool": "bazelac", "hash": "aa11" }));
    call(&feature, "tool-finish", serde_json::json!({ "tool": "bazelac", "hash": "aa11", "size": 5 }));
    assert_eq!(call(&feature, "tool-get", serde_json::json!({ "tool": "bazelac", "hash": "aa11" })).1["hit"], true);
    assert_eq!(call(&main, "tool-get", serde_json::json!({ "tool": "bazelac", "hash": "aa11" })).1["hit"], false);
    assert!(store.0.borrow().contains_key("cache:gh77:refs/heads/feature:bazelac:aa11"));
    // Each tool's own: Nx does not find Turbo's; an unknown tool is refused.
    assert_eq!(call(&feature, "tool-get", serde_json::json!({ "tool": "nx", "hash": "8eccf22c040ebab6" })).1["hit"], false);
    assert_eq!(call(&feature, "tool-get", serde_json::json!({ "tool": "make", "hash": "x" })).0, 400);
}

#[test]
fn a_gitlab_jobs_cache_is_its_projects_by_the_branch_gitlab_said_once_the_job_began() {
    let program = format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/{}", superci_core::cache::program_key("x86_64"));
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| (r.method == "HEAD" && r.url == program).then(|| Response::new(200, "application/octet-stream", "")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let secret = "hook-secret-0123456789";
    run(gitlab_event(1977, "pending", secret));
    // GitLab's runner keeps its cache in "S3" at superci-cache on the machine.
    let script = user_data_of(&launches(&clouds.0)[0]);
    assert!(script.contains("CACHE_TYPE=s3 CACHE_SHARED=true") && script.contains("CACHE_S3_SERVER_ADDRESS=$i:8257") && script.find("CACHE_TYPE=s3").unwrap() < script.find("export GL_URL=").unwrap(), "{script}");
    let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    let call = |what: &str, body: serde_json::Value| { let r = run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string())); (r.status, json(&r)) };
    let key = "gl/project/380/default-non_protected/cache.zip";
    // Before its job began, nothing (which job the runner takes is GitLab's to say).
    assert_eq!(call("tool-begin", serde_json::json!({ "tool": "gitlab", "hash": key })).0, 403);
    let mut began = serde_json::json!({ "object_kind": "build", "build_id": 1977, "build_name": "test", "build_status": "running", "pipeline_id": 2366, "project_id": 380, "ref": "feature", "tag": false,
        "project": { "id": 380, "path_with_namespace": "acme/app", "default_branch": "main" }, "runner": { "id": 55 } });
    run(Request::new("POST", &format!("{PLANE_URL}/gitlab/webhook")).with_header("x-gitlab-token", secret).with_header("x-gitlab-event", "Job Hook").with_body(began.to_string()));
    let (status, begun) = call("tool-begin", serde_json::json!({ "tool": "gitlab", "hash": key }));
    assert_eq!(status, 200, "{begun}");
    let (status, more) = call("tool-sign", serde_json::json!({ "tool": "gitlab", "hash": key, "names": ["p00001"] }));
    assert!(status == 200 && more["urls"][0].as_str().unwrap().contains("/p00001?"));
    assert_eq!(call("tool-finish", serde_json::json!({ "tool": "gitlab", "hash": key, "pieces": [["p00000", 64], ["p00001", 6]] })).1["size"], 70);
    let ns = superci_core::cache::gitlab_ns("", 380);
    assert!(store.0.borrow().contains_key(&format!("cache:{ns}:refs/heads/feature:gitlab:{key}")), "the feature branch's, in its project's");
    assert_eq!(call("tool-get", serde_json::json!({ "tool": "gitlab", "hash": key })).1["pieces"].as_array().unwrap().len(), 2);
    assert_eq!(store.0.borrow().get(&format!("cachename:{ns}")).map(String::as_str), Some("acme/app"), "listed by the project's path");
    let _ = &mut began;
}

#[test]
fn a_gitlab_job_gitlab_never_told_of_gets_its_machine_from_the_sweep() {
    let hook = format!("{PLANE_URL}/gitlab/webhook");
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        let u = r.url.as_str();
        let json = |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string()));
        if u.starts_with("https://gitlab.example/api/v4/projects?membership=true") { return json(serde_json::json!([{ "id": 380, "path_with_namespace": "acme/app" }, { "id": 381, "path_with_namespace": "acme/other" }])) }
        if u == "https://gitlab.example/api/v4/projects/380/hooks" { return json(serde_json::json!([{ "id": 1, "url": hook }])) }
        if u == "https://gitlab.example/api/v4/projects/381/hooks" { return json(serde_json::json!([])) }
        if u.starts_with("https://gitlab.example/api/v4/projects/380/jobs?scope[]=pending") {
            return json(serde_json::json!([{ "id": 1977, "name": "test", "stage": "test", "ref": "main", "tag": false, "created_at": "2026-09-21T00:00:00Z", "pipeline": { "id": 9 } },
                { "id": 1990, "name": "new", "created_at": "2026-09-21T14:13:00Z" }]))
        }
        assert!(!u.contains("/projects/381/jobs"), "a project whose jobs do not come here is not asked");
        None
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(get("/health")));
    block_on(plane.alarm()).unwrap();
    // The job that waited unseen has its machine; the one made a moment ago waits for its event.
    let job: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:gl1977"]).unwrap();
    assert_eq!((job["state"].as_str(), job["project_id"].as_u64()), (Some("launched"), Some(380)));
    assert_eq!(launches(&clouds.0).len(), 1);
    assert!(!store.0.borrow().contains_key("job:gl1990"));
    // Asked again only minutes later.
    let asked = clouds.0.calls.borrow().iter().filter(|c| c.1.contains("scope[]=pending")).count();
    block_on(plane.alarm()).unwrap();
    assert_eq!(clouds.0.calls.borrow().iter().filter(|c| c.1.contains("scope[]=pending")).count(), asked);
}

/// A runtime that leaves what it can for after the answer, as the AWS control plane does.
struct Leaves(RefCell<Vec<String>>);
#[async_trait(?Send)]
impl Timer for Leaves {
    async fn wake_in(&self, _: u64) -> io::Result<()> { faults::op() }
    fn later(&self, what: &str) -> bool { self.0.borrow_mut().push(what.to_string()); true }
}

#[test]
fn a_job_started_after_a_sweep_gets_the_control_planes_own_links() {
    let (store, cache, leaves) = (Mem::default(), Cache::default(), Leaves(RefCell::new(vec![])));
    let http = WithCache { clouds: FakeClouds::default(), keys: serde_json::json!({ "keys": [] }) };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &leaves, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let left = leaves.0.borrow().clone();
    assert_eq!(left, ["start job:7"]);
    // A sweep has no request: the job's machine is started with what the runtime gives, which is no address.
    block_on(plane.later("", &left[0])).unwrap();
    let script = String::from_utf8(STANDARD.decode(field(&launches(&http.clouds).pop().unwrap(), "UserData")).unwrap()).unwrap();
    assert!(script.contains(&format!("c='{PLANE_URL}/cache/")) && script.contains(&format!("{PLANE_URL}/hello?runner=")), "{script}");
}

#[test]
fn a_gitlab_job_whose_events_never_come_is_seen_running_and_over_by_asking_gitlab() {
    let status = std::rc::Rc::new(std::cell::RefCell::new("pending"));
    let now = status.clone();
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        let json = |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string()));
        match r.url.as_str() {
            "https://gitlab.example/api/v4/projects/380/jobs/1977" => json(serde_json::json!({ "id": 1977, "status": *now.borrow(), "tag_list": ["superci/cpu=2"], "ref": "feature", "tag": false,
                "started_at": if *now.borrow() == "pending" { serde_json::Value::Null } else { "2026-10-14T15:22:39.000Z".into() },
                "finished_at": if *now.borrow() == "success" { serde_json::json!("2026-10-14T15:23:21.000Z") } else { serde_json::Value::Null },
                "runner": if *now.borrow() == "pending" { serde_json::Value::Null } else { serde_json::json!({ "id": 55 }) } })),
            u if u.starts_with("https://gitlab.example/api/v4/projects?membership=true") => json(serde_json::json!([])),
            _ => None,
        }
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let job = || serde_json::from_str::<serde_json::Value>(&store.0.borrow()["job:gl1977"]).unwrap();
    block_on(plane.handle(gitlab_event(1977, "pending", "hook-secret-0123456789")));
    assert_eq!(job()["state"], "launched");
    // GitLab's runner took it and GitLab sent no word of it: the next look at GitLab sees it running, and its branch is
    // kept with its runner (for the cache on its machine).
    *status.borrow_mut() = "running";
    store.0.borrow_mut().remove("gitlab:untold");
    block_on(plane.alarm()).unwrap();
    assert_eq!((job()["state"].as_str(), job()["seen_in_progress"].as_bool()), (Some("running"), Some(true)));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&store.0.borrow()["glrunner:55"]).unwrap()["running"]["ref"], "feature");
    // Over: so it is said, and its runner removed.
    *status.borrow_mut() = "success";
    store.0.borrow_mut().remove("gitlab:untold");
    block_on(plane.alarm()).unwrap();
    assert_eq!(job()["state"], "done");
    assert!(!store.0.borrow().contains_key("glrunner:55"));
    // Its time is GitLab's own, from when it began to when it ended.
    assert_eq!(job()["ended_ms"].as_u64().unwrap() - job()["started_ms"].as_u64().unwrap(), 42_000);
}

#[test]
fn a_gitlab_machine_whose_jobs_event_never_came_learns_its_job_by_asking_gitlab() {
    let program = format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/{}", superci_core::cache::program_key("x86_64"));
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        if r.method == "HEAD" && r.url == program { return Some(Response::new(200, "application/octet-stream", "")) }
        (r.url == "https://gitlab.example/api/v4/runners/55/jobs?status=running&per_page=1").then(|| Response::new(200, "application/json",
            serde_json::json!([{ "id": 1977, "ref": "feature", "tag": false, "project": { "id": 380, "path_with_namespace": "acme/app", "default_branch": "main" } }]).to_string()))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    run(gitlab_event(1977, "pending", "hook-secret-0123456789"));
    let script = user_data_of(&launches(&clouds.0)[0]);
    let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    let r = run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/tool-begin")).with_body(serde_json::json!({ "tool": "gitlab", "hash": "gl/project/380/k/cache.zip" }).to_string()));
    assert_eq!(r.status, 200, "{}", r.body_text());
    assert!(store.0.borrow().keys().any(|k| k.ends_with(":refs/heads/feature:gitlab:gl/project/380/k/cache.zip")));
}

#[test]
fn a_working_tree_goes_to_the_control_planes_bucket_in_one_and_its_checkout_is_sent_there() {
    let program = format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/{}", superci_core::cache::program_key("x86_64"));
    let ready = std::rc::Rc::new(std::cell::Cell::new(true));
    let is_ready = ready.clone();
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        let base = format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/bases/bkeptbefore01.tar.gz");
        if r.method == "HEAD" && r.url == base { return Some(Response::new(200, "application/octet-stream", "")) }
        if r.method == "HEAD" && r.url.contains("/bases/") { return Some(Response::new(404, "application/octet-stream", "")) }
        (r.method == "HEAD" && r.url == program).then(|| Response::new(if is_ready.get() { 200 } else { 404 }, "application/octet-stream", ""))
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r.with_header("authorization", "Bearer dashboard-session-key-1")));
    // A link to put the whole tree in the bucket: nothing of it goes through the store.
    let linked = json(&run(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-0123456789abcdef&part=0&link=1"))));
    let link = linked["url"].as_str().unwrap();
    assert!(link.starts_with(&format!("https://superci-plane-{PLANE_ID}-us-east-1.s3.us-east-1.amazonaws.com/runs/tree-0123456789abcdef/00000?")) && link.contains("X-Amz-Signature"), "{link}");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("rtree:")));
    let body = serde_json::json!({ "workflow": "jobs:\n  test:\n    runs-on: superci\n    steps:\n      - uses: actions/checkout@v4\n", "path": ".github/workflows/ci.yml", "repository": "acme/app",
        "tree": "tree-0123456789abcdef", "parts": 1 }).to_string();
    let made = run(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(body));
    assert_eq!(made.status, 200, "{}", made.body_text());
    // Its checkout fetches it from there, through a link of its own.
    let launched = launches(&clouds.0);
    let script = String::from_utf8(STANDARD.decode(field(&launched[0], "UserData")).unwrap()).unwrap();
    let jit = script.split("--jitconfig ").nth(1).unwrap().split_whitespace().next().unwrap();
    let all: serde_json::Value = serde_json::from_slice(&STANDARD.decode(jit).unwrap()).unwrap();
    let server = serde_json::from_slice::<serde_json::Value>(&STANDARD.decode(all[".runner"].as_str().unwrap()).unwrap()).unwrap()["ServerUrl"].as_str().unwrap().to_string();
    let fetched = block_on(plane.handle(Request::new("GET", &format!("{server}/superci/tree?part=0"))));
    assert_eq!(fetched.status, 302);
    let to = fetched.header("location").unwrap();
    assert!(to.contains("/runs/tree-0123456789abcdef/00000?") && to.contains("X-Amz-Signature"), "{to}");
    // A tree kept as a base for the next runs' (in the bucket under its name); then a run with only what changed since
    // it: the base, then the changes, as two parts; and when the base is gone, the computer is told so.
    let kept = json(&run(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-aaaaaaaaaaaaaaaa&part=0&link=1&keep=bkeptbefore01"))));
    assert!(kept["url"].as_str().unwrap().contains("/bases/bkeptbefore01.tar.gz?"), "{kept}");
    let changes = json(&run(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-bbbbbbbbbbbbbbbb&part=1&link=1&base=bkeptbefore01"))));
    assert!(changes["base"] == true && changes["parts"] == 2 && changes["url"].as_str().unwrap().contains("/runs/tree-bbbbbbbbbbbbbbbb/00001?"), "{changes}");
    assert!(store.0.borrow()["rtree-s3:tree-bbbbbbbbbbbbbbbb:00000"].ends_with("|bases/bkeptbefore01.tar.gz"));
    let gone = json(&run(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-cccccccccccccccc&part=1&link=1&base=bgonelongago01"))));
    assert!(gone["base"] == false && gone["url"].is_null(), "{gone}");
    // With no bucket ready, the tree goes through the control plane as before.
    ready.set(false);
    let cache2 = Cache::default();
    let plane2 = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache2, containers: None };
    let none = block_on(plane2.handle(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id=tree-fedcba9876543210&part=0&link=1")).with_header("authorization", "Bearer dashboard-session-key-1")));
    assert!(json(&none)["url"].is_null());
}

#[test]
fn usage_counts_every_job_of_a_period_its_minutes_and_what_the_code_host_would_have_charged() {
    use superci_core::plane::{hour_of, usage_of, Hour, Job};
    // (Each job's hour added up, as the control plane adds them; these jobs are of no run.)
    let usage = |jobs: &[Job], price: &dyn Fn(&Job) -> f64, now: u64, step_ms: u64, steps: usize| {
        let mut hours: std::collections::BTreeMap<u64, Hour> = std::collections::BTreeMap::new();
        for j in jobs.iter().filter(|j| j.at_ms <= now) { let (hour, h) = hour_of(j, price(j), now); hours.entry(hour).or_default().add(&h) }
        usage_of(&hours, now, step_ms, steps)
    };
    let now = 100 * 86_400_000u64;
    let job = |at_ago: u64, ran_s: Option<u64>, state: &str, usd: f64, provider: &str| Job { job_id: at_ago, at_ms: now - at_ago, state: state.into(), provider: provider.into(), cost_usd: Some(usd),
        started_ms: ran_s.map(|_| now - at_ago + 10_000), ended_ms: ran_s.map(|s| now - at_ago + 10_000 + s * 1000), ..Default::default() };
    let jobs = vec![
        job(3_600_000, Some(61), "done", 0.01, ""),          // an hour ago: 61 s, billed 2 minutes
        job(5 * 3_600_000, Some(30), "done", 0.02, "gitlab"), // five hours ago: 30 s, billed 1 minute
        job(7_200_000, None, "failed", 0.0, ""),              // two hours ago: never ran
        job(3 * 86_400_000, Some(600), "done", 0.05, ""),     // three days ago: in the week only
        job(40 * 86_400_000, Some(600), "done", 0.05, ""),    // forty days ago: in none
    ];
    let price = |j: &Job| if j.provider == "gitlab" { 0.01 } else { 0.006 };
    let day = usage(&jobs, &price, now, 3_600_000, 24);
    assert_eq!((day["jobs"].as_u64(), day["ran"].as_u64(), day["not_ran"].as_u64(), day["billed_minutes"].as_u64()), (Some(3), Some(2), Some(1), Some(3)));
    assert_eq!(day["machine_minutes"].as_f64(), Some(1.5));
    assert!((day["usd"].as_f64().unwrap() - 0.03).abs() < 1e-9 && (day["host_usd"].as_f64().unwrap() - (2.0 * 0.006 + 0.01)).abs() < 1e-9, "{day}");
    assert_eq!(day["wait_median_ms"].as_u64(), Some(10_000));
    // By the hour, the oldest first (the last is this hour: a job an hour ago is in the one before).
    let steps = day["steps"].as_array().unwrap();
    assert_eq!((steps.len(), steps[22]["ran"].as_u64(), steps[18]["ran"].as_u64(), steps[21]["not_ran"].as_u64()), (24, Some(1), Some(1), Some(1)));
    let week = usage(&jobs, &price, now, 86_400_000, 7);
    assert_eq!((week["jobs"].as_u64(), week["billed_minutes"].as_u64(), week["steps"].as_array().unwrap().len()), (Some(4), Some(13), 7));
    assert_eq!(usage(&jobs, &price, now, 86_400_000, 30)["jobs"].as_u64(), Some(4));
}

#[test]
fn a_store_without_ends_of_its_own_removes_a_record_at_its_end_and_not_one_written_again() {
    let store = superci_core::io::Ending(Mem::default());
    block_on(store.put_until("live:1", "a".into(), 1_000)).unwrap();
    block_on(store.put_until("live:2", "b".into(), 1_000)).unwrap();
    block_on(store.put("live:2", "b again, for good".into())).unwrap();
    block_on(store.put_until("live:3", "c".into(), 5_000)).unwrap();
    assert!(block_on(store.swap_until("live:3", Some("c"), "c later".into(), Some(9_000))).unwrap());
    assert_eq!(block_on(store.expire(999)).unwrap(), 0, "nothing is due yet");
    assert_eq!(block_on(store.expire(6_000)).unwrap(), 1);
    let left: Vec<String> = block_on(store.list("live:")).unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(left, ["live:2", "live:3"], "the one written again keeps no end; the one moved on keeps its newer end");
    assert_eq!(block_on(store.expire(9_000)).unwrap(), 1);
    assert_eq!(block_on(store.list("live:")).unwrap().len(), 1);
    assert!(store.0.0.borrow().keys().all(|k| !k.starts_with("~x:")), "every mark is gone once due");
    // The newest few, by key.
    for i in 0..5 { block_on(store.put(&format!("recent:{i:03}"), i.to_string())).unwrap() }
    assert_eq!(block_on(store.list_last("recent:", 2)).unwrap().iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["recent:003", "recent:004"]);
}

#[test]
fn jobs_kept_before_their_indexes_are_indexed_once_and_each_job_is_counted_in_its_days_totals_once() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(t));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let seed = |id: u64, state: &str, at: u64, took_s: Option<u64>, usd: Option<f64>| serde_json::json!({ "job_id": id, "run_id": 3, "repo": "acme/app", "label": "superci", "app_id": 99, "state": state, "at_ms": at, "installation_id": 42,
        "cloud": "aws", "usd_per_hour": 1.0, "seen_in_progress": true, "began": true, "launched_ms": at, "started_ms": at + 10_000, "runner": format!("superci-{PLANE_ID}-{id}"),
        "ended_ms": took_s.map(|s| at + 10_000 + s * 1000), "cost_usd": usd });
    // Kept before the indexes: two over today, one three days ago, one still running.
    for (id, j) in [(1, seed(1, "done", t - 2 * 3_600_000, Some(60), Some(0.10))), (2, seed(2, "done", t - 3_600_000, Some(30), Some(0.05))),
        (3, seed(3, "done", t - 3 * 86_400_000, Some(600), Some(0.20))), (4, seed(4, "running", t - 600_000, None, None))] {
        store.0.borrow_mut().insert(format!("job:{id}"), j.to_string());
        store.0.borrow_mut().insert(format!("runner:superci-{PLANE_ID}-{id}"), id.to_string());
    }
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let status = || json(&block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/status")).with_header("authorization", "Bearer dashboard-session-key-1"))));
    let before = status();
    block_on(plane.alarm()).unwrap();
    assert!(store.0.borrow().contains_key("indexed"));
    let keys = |p: &str| store.0.borrow().keys().filter(|k| k.starts_with(p)).cloned().collect::<Vec<_>>();
    assert_eq!(keys("live:"), ["live:4"], "only the running one is cared for");
    assert_eq!(keys("recent:").len(), 4);
    // The same answers from the indexes as from every job.
    let after = status();
    for k in ["spend", "spend_today"] { assert!((after[k]["aws"].as_f64().unwrap() - before[k]["aws"].as_f64().unwrap()).abs() < 1e-6, "{k}: {} vs {}", after[k], before[k]) }
    assert_eq!(after["usage"]["day"]["jobs"], before["usage"]["day"]["jobs"]);
    assert_eq!(after["usage"]["week"]["billed_minutes"], before["usage"]["week"]["billed_minutes"]);
    assert_eq!(after["jobs"].as_array().unwrap().iter().map(|j| j["job_id"].as_u64().unwrap()).collect::<Vec<_>>(), [4, 2, 1, 3], "the newest first");
    let today = |k: &str| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("day:{:06}", t / 86_400_000)]).unwrap()["spend"][k].as_f64();
    assert!((today("aws").unwrap() - 0.15).abs() < 1e-9, "the two over today, counted once: {:?}", today("aws"));
    block_on(plane.alarm()).unwrap();
    assert!((today("aws").unwrap() - 0.15).abs() < 1e-9, "not again at the next sweep");
    // The running one ends: counted once, however often it is said.
    for _ in 0..2 { block_on(plane.handle(job_event("completed", 4, "acme/app", Some(&format!("superci-{PLANE_ID}-4"))))); }
    let counted = job(&store, 4)["counted_usd"].as_f64().unwrap_or_else(|| panic!("{}", job(&store, 4)));
    assert!(counted > 0.0 && (today("aws").unwrap() - (0.15 + counted)).abs() < 1e-9, "{counted} {:?}", today("aws"));
    // Half an hour after it ended, nothing more is done for it: no longer among the live ones.
    clock.0.set(t + 31 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert!(keys("live:").is_empty(), "{:?}", keys("live:"));
}

#[test]
fn a_running_jobs_log_is_githubs_then_the_lines_its_machine_sent_after() {
    use superci_core::plane::{last_time, newer_lines};
    let github = b"2026-10-16T21:18:40.1000000Z step one\n2026-10-16T21:18:46.2000000Z heartbeat\n";
    let live = b"2026-10-16T21:18:40.1000000Z step one\n2026-10-16T21:18:46.2000000Z heartbeat\n2026-10-16T21:23:01.0000000Z heartbeat\n  its second line\n2026-10-16T21:28:01.0000000Z heartbeat\n";
    assert_eq!(last_time(github).as_deref(), Some("2026-10-16T21:18:46"));
    assert_eq!(String::from_utf8(newer_lines(live, "2026-10-16T21:18:46")).unwrap(), "2026-10-16T21:23:01.0000000Z heartbeat\n  its second line\n2026-10-16T21:28:01.0000000Z heartbeat\n", "a message's untimed lines go with it");
    assert!(newer_lines(live, "").is_empty() && last_time(b"no times\n").is_none());
}


#[test]
fn a_machine_that_keeps_its_jobs_images_is_not_ended_with_its_job_but_goes_off_by_itself() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithLayers::default();
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let ended = || http.clouds.calls.borrow().iter().filter(|c| c.2.contains("Action=TerminateInstances")).count();
    for (id, label) in [(1, "superci/image=ubuntu26"), (2, "superci/image=ubuntu24")] {
        run(if id == 1 { layered(1, 77, "acme/app") } else { labelled("queued", id, "acme/app", None, label) });
        let r = job(&store, id)["runner"].as_str().unwrap().to_string();
        run(labelled("in_progress", id, "acme/app", Some(&r), label));
        run(labelled("completed", id, "acme/app", Some(&r), label));
    }
    // Ubuntu 26.04's keeps its images after the job: not ended with it; Ubuntu 24.04's is, as before.
    assert_eq!((job(&store, 1)["keeps"].as_bool(), job(&store, 2)["keeps"].as_bool()), (Some(true), None));
    assert_eq!(ended(), 1);
    // What it pulled is offered then, its runner's record gone with its job: still its job's, and asked for.
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("runner:")), "{:?}", store.0.borrow().keys().collect::<Vec<_>>());
    let script = String::from_utf8(STANDARD.decode(field(&launches(&http.clouds)[0], "UserData")).unwrap()).unwrap();
    let link = script.split(&format!("c='{PLANE_URL}/cache/")).nth(1).unwrap().split('\'').next().unwrap().to_string();
    let digest = format!("sha256:{}", "c".repeat(64));
    let offered = json(&run(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/layers-propose")).with_body(serde_json::json!({ "layers": [{ "digest": digest, "size": 1000 }] }).to_string())));
    assert_eq!(offered["layers"][&digest]["do"], "upload", "{offered}");
    // Still there once its time is up: ended by the sweep.
    let k = store.0.borrow().keys().find(|k| k.starts_with("keeping:")).cloned().expect("noted");
    block_on(plane.alarm()).unwrap();
    assert_eq!(ended(), 1, "not yet");
    store.0.borrow_mut().insert(k.clone(), "0".into());
    block_on(plane.alarm()).unwrap();
    assert_eq!(ended(), 2);
    assert!(!store.0.borrow().contains_key(&k));
}

/// The fake clouds with a GitLab that has project 380 (acme/app), whose webhooks are kept as GitLab keeps them.
struct WithGitLabProjects { clouds: FakeClouds, hooks: RefCell<Vec<(u64, String)>> }
#[async_trait(?Send)]
impl Http for WithGitLabProjects {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        let json = |v: serde_json::Value| Ok(Response::new(200, "application/json", v.to_string()));
        if let Some(path) = r.url.strip_prefix("https://gitlab.example/api/v4") {
            self.clouds.calls.borrow_mut().push((r.method.clone(), r.url.clone(), String::from_utf8_lossy(&r.body).to_string()));
            let field = |name: &str| url::form_urlencoded::parse(&r.body).find(|(k, _)| k == name).map(|(_, v)| v.to_string()).unwrap_or_default();
            match (r.method.as_str(), path.split('?').next().unwrap_or_default()) {
                ("GET", "/projects") => return json(serde_json::json!([{ "id": 380, "path_with_namespace": "acme/app" }])),
                ("GET", "/projects/380/hooks") => return json(serde_json::Value::Array(self.hooks.borrow().iter().map(|(id, url)| serde_json::json!({ "id": id, "url": url })).collect())),
                ("POST", "/projects/380/hooks") => { let id = 9 + self.hooks.borrow().len() as u64; self.hooks.borrow_mut().push((id, field("url"))); return json(serde_json::json!({ "id": id })) }
                ("DELETE", p) if p.starts_with("/projects/380/hooks/") => { let id: u64 = p.rsplit('/').next().unwrap().parse().unwrap(); self.hooks.borrow_mut().retain(|(i, _)| *i != id); return Ok(Response::new(204, "text/plain", "")) }
                ("DELETE", p) if p.starts_with("/runners/") => return Ok(Response::new(204, "text/plain", "")),
                _ => {}
            }
        }
        self.clouds.send(r).await
    }
}

#[test]
fn gitlabs_projects_follow_a_move_and_leaving_takes_their_webhooks_and_its_runners_away() {
    const NEW_URL: &str = "https://superci.other-account.workers.dev";
    let key = "dashboard-session-key-0123";
    let gl = superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() };
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    // acme/app sends its jobs to the control plane before.
    let http = WithGitLabProjects { clouds: FakeClouds::default(), hooks: RefCell::new(vec![(8, format!("{PLANE_URL}/gitlab/webhook"))]) };
    let mut config = Config::new("newplane0001".into());
    config.device_keys = vec![(u64::MAX, key.into())];
    config.gitlab = Some(gl.clone());
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let call = |path: &str, body: serde_json::Value| block_on(plane.handle(Request::new("POST", &format!("{NEW_URL}{path}")).with_header("authorization", &format!("Bearer {key}")).with_body(body.to_string())));
    // The switch: its webhook here, the one before gone.
    let claimed = call("/move/claim", serde_json::json!({ "from": PLANE_URL }));
    assert_eq!(claimed.status, 200, "{}", claimed.body_text());
    assert_eq!(http.hooks.borrow().iter().map(|(_, u)| u.clone()).collect::<Vec<_>>(), [format!("{NEW_URL}/gitlab/webhook")]);
    // Leaving: the webhook goes, and the project runners made here (only this GitLab's).
    store.0.borrow_mut().insert("glrunner:56".into(), serde_json::json!({ "id": 56, "job": 1977, "gitlab": "" }).to_string());
    store.0.borrow_mut().insert("glrunner:other:57".into(), serde_json::json!({ "id": 57, "job": 1978, "gitlab": "other" }).to_string());
    let left = call("/leave", serde_json::json!({}));
    assert_eq!(left.status, 200, "{}", left.body_text());
    assert!(http.hooks.borrow().is_empty());
    assert!(http.clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/56"));
    assert!(!http.clouds.calls.borrow().iter().any(|c| c.1 == "https://gitlab.example/api/v4/runners/57"), "another GitLab's runner is not this one's to take");
    assert!(!store.0.borrow().contains_key("glrunner:56") && store.0.borrow().contains_key("glrunner:other:57"));
}

/// The fake clouds with CloudWatch answering what a machine sent (`sum` bytes), or refusing (a role from before).
struct Sent { clouds: FakeClouds, sum: Option<f64> }
#[async_trait(?Send)]
impl Http for Sent {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let (_outer, failed) = faults::call(&r); if let Some(f) = failed { return f }
        if r.url.starts_with("https://monitoring.") {
            return Ok(match self.sum {
                Some(s) => Response::new(200, "text/xml", format!("<GetMetricStatisticsResponse><GetMetricStatisticsResult><Datapoints><member><Sum>{}</Sum></member><member><Sum>{}</Sum></member></Datapoints></GetMetricStatisticsResult></GetMetricStatisticsResponse>", s / 2.0, s / 2.0)),
                None => Response::new(403, "text/xml", "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not authorized to perform: cloudwatch:GetMetricStatistics</Message></Error></ErrorResponse>"),
            })
        }
        self.clouds.send(r).await
    }
}

#[test]
fn what_a_machine_sent_to_the_internet_is_added_to_its_jobs_cost_once_cloudwatch_has_counted_it() {
    for (sum, said) in [(Some(2e9), Some(2.0)), (None, Some(-1.0))] {
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let http = Sent { clouds: FakeClouds::default(), sum };
        let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let plane = ControlPlane { store: &store, http: &http, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
        let run = |r: Request| block_on(plane.handle(r));
        run(webhook("queued", None));
        let runner = format!("superci-{PLANE_ID}-7");
        run(webhook("in_progress", Some(&runner)));
        run(webhook("completed", Some(&runner)));
        let before = job(&store, 7)["cost_usd"].as_f64().unwrap();
        // Not yet: CloudWatch counts a while after.
        block_on(plane.alarm()).unwrap();
        assert!(job(&store, 7).get("sent_gb").is_none());
        clock.0.set(clock.0.get() + 13 * 60_000);
        block_on(plane.alarm()).unwrap();
        let j = job(&store, 7);
        assert_eq!(j["sent_gb"].as_f64(), said, "{j}");
        let added = if said == Some(2.0) { 2.0 * superci_core::aws::DATA_OUT_USD_PER_GB } else { 0.0 };
        assert!((j["cost_usd"].as_f64().unwrap() - (before + added)).abs() < 1e-9, "{j}");
    }
}

/// A runner name GitHub has from a registration this control plane does not know of (an earlier deploy's, a lost
/// record): the job's runner registers under a name of its own instead of the job failing.
#[test]
fn a_runner_name_github_has_already_gets_a_name_of_its_own() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let taken = format!("superci-{PLANE_ID}-31");
    clouds.calls.borrow_mut().push(("POST".into(), "https://api.github.com/orgs/acme/actions/runners/generate-jitconfig".into(), serde_json::json!({ "name": taken }).to_string()));
    block_on(plane.handle(job_event("queued", 31, "acme/app", None)));
    let j = job(&store, 31);
    let runner = j["runner"].as_str().unwrap_or_default();
    assert!(j["state"] == "launched" && runner.starts_with(&format!("{taken}-r")), "{j}");
    assert_eq!(clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/generate-jitconfig")).count(), 3, "the earlier one, the refused one, its own");
}

/// What a job is told when AWS refuses its machine, by what AWS said: no spot room with on-demand at the account's
/// limit (it waits for spot), on-demand alone at the limit, a GPU machine with no quota at all, and no room anywhere.
#[test]
fn a_job_whose_machine_aws_refuses_is_told_why_by_what_aws_said() {
    let cases: [(&str, &str, &str, &str); 4] = [
        ("superci", "InsufficientInstanceCapacity", "VcpuLimitExceeded", "waiting: AWS has no spot machine for it now, and your AWS account's on-demand limit does not let an on-demand one start ("),
        ("superci/spot=false", "InsufficientInstanceCapacity", "VcpuLimitExceeded", "Your AWS account's limits do not let this machine start: it asks for 4 CPUs (us-east-1 on-demand: VcpuLimitExceeded; us-east-2 on-demand: VcpuLimitExceeded)"),
        ("superci/gpu=any", "MaxSpotInstanceCountExceeded", "VcpuLimitExceeded", "AWS lets this account run no GPU machines yet: ask AWS for more in Service Quotas, "),
        ("superci", "InsufficientInstanceCapacity", "InsufficientInstanceCapacity", "AWS had no room for it: "),
    ];
    for (label, spot, on_demand, said) in cases {
        let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| body.contains("Action=RunInstances").then(|| aws_refuses(if body.contains("MarketType=spot") { spot } else { on_demand })));
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(labelled("queued", 1, "acme/app", None, label)));
        let j = job(&store, 1);
        let e = j["error"].as_str().unwrap_or_default();
        assert!(e.contains(said), "{label} ({spot}, {on_demand}): {e}");
    }
}

/// How a job ends up when AWS refuses its machines, for every kind of job and every pair of answers (its spot
/// machine's, its on-demand one's), as AWS documents its errors: kept in `fixtures/launch-refusals.txt`, so a change in
/// any of them shows as a change there. (`SUPERCI_WRITE_FIXTURES=1` writes it anew.)
#[test]
fn every_pair_of_aws_refusals_ends_each_kind_of_job_as_the_table_says() {
    let answers = ["", "InsufficientInstanceCapacity", "VcpuLimitExceeded", "MaxSpotInstanceCountExceeded", "Unsupported", "InvalidParameterValue", "UnauthorizedOperation", "500"];
    // (Each kind of job in a thread of its own: 144 control planes in all.)
    // (The last: a control plane in two regions, the second with no room for anything.)
    let kinds = ["superci", "superci/spot=false", "superci/gpu=any", "superci/gpu=any/spot=false", "superci in us-east-1 and us-east-2"];
    let got: String = std::thread::scope(|s| kinds.map(|kind| s.spawn(move || {
        let (label, two) = match kind.split_once(" in ") { Some((l, _)) => (l, true), None => (kind, false) };
        let mut got = String::new();
        for spot in answers {
            for od in answers {
                if label.contains("spot=false") && !spot.is_empty() { continue }
                let clouds = With(FakeClouds::default(), move |r: &Request, body: &str| {
                    if !body.contains("Action=RunInstances") { return None }
                    if r.url.contains("us-east-2") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
                    match if body.contains("MarketType=spot") { spot } else { od } {
                        "" => None,
                        "500" => Some(Response::new(500, "text/xml", "<Response><Errors><Error><Code>InternalError</Code><Message>An internal error has occurred</Message></Error></Errors></Response>")),
                        code => Some(aws_refuses(code)),
                    }
                });
                let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
                let mut config = Config::new(PLANE_ID.into());
                config.app = Some(app());
                (config.aws_own, config.aws_own_creds) = own_aws();
                if two { config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()] }
                let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
                block_on(plane.handle(labelled("queued", 9, "acme/app", None, label)));
                let j = job(&store, 9);
                got.push_str(&format!("{kind} | spot {} | on-demand {} => {}{}\n", if spot.is_empty() { "started" } else { spot }, if od.is_empty() { "started" } else { od }, j["state"].as_str().unwrap_or_default(),
                    j["error"].as_str().map(|e| format!(" | {e}")).unwrap_or_default()));
            }
        }
        got
    })).into_iter().map(|h| h.join().unwrap()).collect());
    let at = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/launch-refusals.txt");
    if std::env::var_os("SUPERCI_WRITE_FIXTURES").is_some() { std::fs::write(at, &got).unwrap() }
    let want = std::fs::read_to_string(at).unwrap_or_default();
    for (w, g) in want.lines().zip(got.lines()) { assert_eq!(w, g) }
    assert_eq!(want.lines().count(), got.lines().count());
}

/// What a job's label asks of its AWS machine, for each kind of label: the machine it got and what it was asked with
/// (its types, disks, start script's parts). Kept in `fixtures/launch-labels.txt`, so a change shows as a change there.
/// (`SUPERCI_WRITE_FIXTURES=1` writes it anew.)
#[test]
fn each_kind_of_label_asks_aws_for_the_machine_the_table_says() {
    let labels = ["superci", "superci/cpu=8", "superci/cpu=2/ram=16", "superci/arch=arm64", "superci/os=windows", "superci/scratch=ramdisk", "superci/scratch=localdisk",
        "superci/family=c8id", "superci/family=m7gd/scratch=localdisk", "superci/disk=200", "superci/gpu=any", "superci/image=ubuntu26", "superci/image=ubuntu26/docker-cache=true",
        "superci/image=ubuntu26/docker-cache=false", "superci/image=ubuntu26/scratch=ramdisk/docker-cache=true", "superci/image=ubuntu26/docker-cache=true/disk=300", "superci/spot=false/cpu=16"];
    let got: String = std::thread::scope(|s| labels.map(|label| s.spawn(move || {
        let clouds = Kinds { clouds: FakeClouds::default(), no_gpus: false };
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(labelled("queued", 9, "acme/app", None, label)));
        let j = job(&store, 9);
        let asked = launches(&clouds.clouds).last().cloned();
        let (types, disks, script) = match &asked {
            Some(l) => {
                let types: Vec<String> = (1..=20).filter_map(|i| { let t = field(l, &format!("InstanceType.{i}")); (!t.is_empty()).then_some(t) }).chain([field(l, "InstanceType")]).filter(|t| !t.is_empty()).collect();
                let disks: Vec<String> = (1..=4).filter_map(|i| { let gb = field(l, &format!("BlockDeviceMapping.{i}.Ebs.VolumeSize")); (!gb.is_empty()).then(|| format!("{} {gb} GB", field(l, &format!("BlockDeviceMapping.{i}.DeviceName")))) }).collect();
                let script = user_data_of(l);
                let parts: Vec<&str> = [("ramdisk", "tmpfs"), ("nvme", "nvme"), ("hello", "/hello?"), ("notice", "/interrupted?"), ("layers", "superci-layers")].into_iter().filter(|(_, m)| script.contains(m)).map(|(n, _)| n).collect();
                (types.join(","), disks.join(", "), parts.join(" "))
            }
            None => Default::default(),
        };
        format!("{label} => {} {} {} cpu {} disk {} | ramdisk {} local {} layers {} | asked {types} | disks {disks} | script {script}{}\n", j["state"].as_str().unwrap_or_default(), j["machine_type"].as_str().unwrap_or("-"),
            asked.as_deref().map(|l| field(l, "InstanceMarketOptions.MarketType")).filter(|m| !m.is_empty()).unwrap_or_else(|| "on-demand".into()), j["cpu"], j["disk_gb"], j["ramdisk"], j["local_disk"], j["layers"],
            j["error"].as_str().map(|e| format!(" | {e}")).unwrap_or_default())
    })).into_iter().map(|h| h.join().unwrap()).collect());
    let at = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/launch-labels.txt");
    if std::env::var_os("SUPERCI_WRITE_FIXTURES").is_some() { std::fs::write(at, &got).unwrap() }
    let want = std::fs::read_to_string(at).unwrap_or_default();
    for (w, g) in want.lines().zip(got.lines()) { assert_eq!(w, g) }
    assert_eq!(want.lines().count(), got.lines().count());
}

/// A job no spot or on-demand pool has a machine for, among many (as AWS answered a fleet of them): asked again
/// twice, a minute apart, then failed with what AWS said; its start keeps the last two dozen requests. A type AWS does not offer in a zone is not asked
/// there again.
#[test]
fn a_job_no_pool_has_room_for_is_asked_three_times_then_fails_and_a_type_a_zone_lacks_is_not_asked_there_again() {
    let none: &'static str = Box::leak(FLEET_NONE.replace("g6.xlarge", "c8a.16xlarge").replace("g5.xlarge", "m8a.16xlarge").into_boxed_str());
    let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeVpcs") { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into()) }
        if body.contains("Action=DescribeSubnets") { return xml(format!("<subnetSet>{}</subnetSet>", ["a", "b", "c", "d", "e", "f"].map(|z| format!("<item><subnetId>subnet-{z}</subnetId><availabilityZone>us-east-1{z}</availabilityZone></item>")).concat())) }
        if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>".into()) }
        if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") { return xml("<spotPriceHistorySet/>".into()) }
        if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE.into()) }
        if body.contains("Action=CreateFleet") { return xml(none.into()) }
        if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED.into()) }
        None
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.max_cpu = Some(64);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a+c7a+m7a+r7a")));
    // Tried again twice, a minute apart, then failed.
    let mut said = vec![];
    for _ in 0..3 {
        let e = job(&store, 7)["error"].as_str().unwrap_or_default().to_string();
        said.push(e.split(" (").next().unwrap_or_default().to_string());
        clock.0.set(clock.0.get() + 61_000);
        block_on(plane.alarm()).unwrap();
    }
    let again = "waiting: starting its machine failed, trying again".to_string();
    assert_eq!(said, [again.clone(), again, "AWS had no room for it: us-east-1: InsufficientInstanceCapacity; us-east-1 on-demand: InsufficientInstanceCapacity".to_string()]);
    let j = job(&store, 7);
    assert_eq!((j["state"].as_str(), j["asks"].as_array().map(|a| a.len())), (Some("failed"), Some(24)), "the last two dozen requests: {j}");
    assert!(j["error"].as_str().unwrap().starts_with("AWS had no room for it: us-east-1: InsufficientInstanceCapacity; us-east-1 on-demand: InsufficientInstanceCapacity"), "{j}");
    // The zone that does not have the types: noted, and left out of the next job's requests.
    assert!(store.0.borrow().contains_key("aws:unoffered:us-east-1:c8a.16xlarge:us-east-1e") && store.0.borrow().contains_key("aws:unoffered:us-east-1:m8a.16xlarge:us-east-1e"));
    let before = clouds.0.calls.borrow().len();
    let asked_in = |from: usize, to: usize| clouds.0.calls.borrow()[from..to].iter().any(|c| {
        let f: Vec<(String, String)> = url::form_urlencoded::parse(c.2.as_bytes()).into_owned().collect();
        let at = |prefix: &str| (field(&f, &format!("{prefix}InstanceType")), field(&f, &format!("{prefix}SubnetId")));
        let mut pools = vec![at(""), at("NetworkInterface.1.")];
        for n in 1..=40 { pools.push(at(&format!("LaunchTemplateConfigs.1.Overrides.{n}."))) }
        pools.iter().any(|(t, s)| t == "c8a.16xlarge" && s == "subnet-e")
    });
    assert!(asked_in(0, before), "the first job asked it there");
    block_on(plane.handle(labelled("queued", 8, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a+c7a+m7a+r7a")));
    let after = clouds.0.calls.borrow().len();
    assert!(!asked_in(before, after), "c8a.16xlarge asked in us-east-1e again");
}

/// A request for many spot pools refused for something else than room (here its template gone, as AWS words that):
/// no answer about the pools, so the job goes on with them in rounds, before it is tried again.
#[test]
fn a_fleet_refused_for_something_else_than_room_goes_on_with_the_pools_in_rounds() {
    let refused: &'static str = Box::leak(FLEET_NONE.replace("g6.xlarge", "c8a.16xlarge").replace("g5.xlarge", "m8a.16xlarge").replace("<errorCode>InvalidFleetConfiguration</errorCode>", "<errorCode>InvalidLaunchTemplateId.NotFound</errorCode>").into_boxed_str());
    let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeVpcs") { return xml("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into()) }
        if body.contains("Action=DescribeSubnets") { return xml(format!("<subnetSet>{}</subnetSet>", ["a", "b", "c", "d", "e", "f"].map(|z| format!("<item><subnetId>subnet-{z}</subnetId><availabilityZone>us-east-1{z}</availabilityZone></item>")).concat())) }
        if body.contains("Action=DescribeSecurityGroups") { return xml("<securityGroupInfo><item><groupId>sg-1</groupId></item></securityGroupInfo>".into()) }
        if body.contains("Action=DescribeSpotPriceHistory") && !body.contains("AvailabilityZone=") { return xml("<spotPriceHistorySet/>".into()) }
        if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE.into()) }
        if body.contains("Action=CreateFleet") { return xml(refused.into()) }
        if body.contains("Action=DeleteLaunchTemplate") { return xml(FLEET_TEMPLATE_REMOVED.into()) }
        None
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(1_790_000_000_000));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.max_cpu = Some(64);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci/cpu=64/family=c8a+m8a+r8a+c7a+m7a+r7a")));
    let mut said = vec![];
    for _ in 0..5 {
        said.push(job(&store, 7)["error"].as_str().unwrap_or_default().split(" (").next().unwrap_or_default().to_string());
        clock.0.set(clock.0.get() + 61_000);
        block_on(plane.alarm()).unwrap();
    }
    let (more, again) = ("waiting: asking AWS's other spot pools for a machine".to_string(), "waiting: starting its machine failed, trying again".to_string());
    assert_eq!(said[..3], [more.clone(), more, again]);
    assert_eq!(job(&store, 7)["state"].as_str(), Some("failed"));
}


/// A day's real prices for 64 CPUs (us-east-1, 20 October 2026): m7a the cheapest for a job, c8a close behind for its
/// speed, m8a further off, r8a at nearly three times. The best pool alone has no machine: the request for many asks
/// first only the pools within a quarter of what a job costs on it, so AWS, which weighs room before price, cannot
/// pick r8a; the others come in a request of their own only when none of those has a machine. A pool priced over the
/// cheapest type's on-demand price is not asked at all, and said.
#[test]
fn the_pools_asked_together_first_are_those_near_the_best_price_for_a_job_and_none_over_on_demand() {
    use superci_core::aws::{run_instance, Launch, Told};
    let (_, creds) = own_aws();
    let creds = creds.unwrap();
    let network = six_zones();
    let tags = vec![("superci-plane".to_string(), PLANE_ID.to_string()), ("superci-job".to_string(), "7".to_string())];
    let prices = [("m7a.16xlarge", "us-east-1a", 0.73), ("m7a.16xlarge", "us-east-1b", 0.80), ("c8a.16xlarge", "us-east-1a", 0.97), ("c8a.16xlarge", "us-east-1b", 1.00),
        ("m8a.16xlarge", "us-east-1a", 1.13), ("r8a.16xlarge", "us-east-1a", 2.83), ("r8a.16xlarge", "us-east-1b", 2.90), ("r7a.16xlarge", "us-east-1a", 3.50)];
    let none: &'static str = Box::leak(include_str!("fixtures/aws/fleet/none.xml").to_string().into_boxed_str());
    let clouds = With(FakeClouds::default(), move |_: &Request, body: &str| {
        let xml = |s: String| Some(Response::new(200, "text/xml", s));
        if body.contains("Action=DescribeSpotPriceHistory") {
            return xml(format!("<spotPriceHistorySet>{}</spotPriceHistorySet>", prices.iter().map(|(t, z, p)| format!("<item><instanceType>{t}</instanceType><availabilityZone>{z}</availabilityZone><spotPrice>{p}</spotPrice></item>")).collect::<String>()))
        }
        if body.contains("Action=RunInstances") { return Some(aws_refuses("InsufficientInstanceCapacity")) }
        if body.contains("Action=CreateLaunchTemplate") { return xml(FLEET_TEMPLATE.to_string()) }
        if body.contains("Action=CreateFleet") { return xml(none.to_string()) }
        None
    });
    let types: Vec<String> = ["m7a.16xlarge", "c8a.16xlarge", "m8a.16xlarge", "r8a.16xlarge", "r7a.16xlarge"].map(String::from).to_vec();
    let launch = Launch { region: "us-east-1", network: Some(&network), image: "ami-new", types: &types, user_data: "#!/bin/bash\n", disk_gb: 60, disk_mibs: 125, disk_iops: 3000, clock: Some(&FixedClock), skip: &[], tags: &tags,
        spot: true, os: "linux", avoid: &[], timer: None, jitter_ms: 0, fleet: true, store_disk: None };
    let mut told = Told::default();
    let machine = block_on(run_instance(&clouds, &creds, &launch, 1_790_000_000_000, &mut told));
    assert!(machine.is_err());
    // The best for a job, by itself: m7a in zone a.
    let alone = launches(&clouds.0);
    assert_eq!((alone.len(), field(&alone[0], "InstanceType"), field(&alone[0], "NetworkInterface.1.SubnetId")), (1, "m7a.16xlarge".to_string(), "subnet-a".to_string()));
    let fleets = calls_of(&clouds.0, "CreateFleet");
    let pools = |f: &Vec<(String, String)>| f.iter().filter(|(n, _)| n.ends_with(".InstanceType")).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    // Within a quarter of $0.73 a job: m7a in zone b ($0.80), c8a in both ($0.97 and $1.00, as $0.81 and $0.83 for its
    // speed). Then the rest that is priced: m8a ($1.13, as $0.94) and r8a. r7a costs more than c8a would on-demand.
    assert_eq!(pools(&fleets[0]), ["m7a.16xlarge", "c8a.16xlarge", "c8a.16xlarge"], "{fleets:?}");
    assert_eq!(pools(&fleets[1])[..3], ["m8a.16xlarge", "r8a.16xlarge", "r8a.16xlarge"], "{fleets:?}");
    assert!(fleets.iter().all(|f| !pools(f).iter().any(|t| t == "r7a.16xlarge")), "{fleets:?}");
    assert_eq!(told.priced_out, ["r7a.16xlarge"]);
}

/// Two regions for spot machines: a job's machine is asked for first where it costs least, the first region counted
/// 15% cheaper (its cache is there). Ohio at 60% of N. Virginia's price is asked first; at 90%, N. Virginia still is.
#[test]
fn spot_goes_first_to_the_region_where_a_job_costs_least_with_the_first_favoured() {
    for (ohio, first) in [(0.6, "us-east-2"), (0.9, "us-east-1")] {
        let clouds = With(FakeClouds::default(), move |r: &Request, body: &str| {
            if !body.contains("Action=DescribeSpotPriceHistory") { return None }
            let (region, zones) = if r.url.contains("us-east-2") { ("us-east-2", ["a", "b"]) } else { ("us-east-1", ["a", "b"]) };
            let price = if region == "us-east-2" { 0.10 * ohio } else { 0.10 };
            let items = ["c8a.xlarge", "m8a.xlarge"].iter().flat_map(|t| zones.map(|z| format!("<item><instanceType>{t}</instanceType><availabilityZone>{region}{z}</availabilityZone><spotPrice>{price}</spotPrice></item>"))).collect::<String>();
            Some(Response::new(200, "text/xml", format!("<spotPriceHistorySet>{items}</spotPriceHistorySet>")))
        });
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
        config.instance_types = ["c8a.xlarge", "m8a.xlarge"].map(String::from).to_vec();
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(webhook("queued", None)));
        let asked: Vec<String> = clouds.0.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances")).map(|c| c.1.clone()).collect();
        assert!(asked.first().is_some_and(|url| url.contains(first)), "Ohio at {ohio}: {asked:?}");
        assert_eq!(job(&store, 7)["region"].as_str(), Some(first), "Ohio at {ohio}");
    }
}

/// Spot pools ordered across regions by what a job costs: N. Virginia's c8a pools lately had no machine (its r8a is
/// three times the price), Ohio's c8a has the price of N. Virginia's: Ohio's c8a is asked first, and r8a only once it
/// has no machine either, where r8a costs least (each region then whole, where what is left costs least; Ohio's c8a
/// not asked again). N. Virginia's c8a with room: asked first, in the first region, as before.
#[test]
fn spot_pools_near_the_cheapest_in_any_region_are_asked_before_a_dearer_type_in_the_first() {
    let now = FixedClock.now_ms();
    for (ohio_full, virginia_full, first, got, ohio_asks) in [(false, true, "us-east-2", "c8a.xlarge", 1), (true, true, "us-east-1", "r8a.xlarge", 2), (false, false, "us-east-1", "c8a.xlarge", 0)] {
        let clouds = With(FakeClouds::default(), move |r: &Request, body: &str| {
            let region = if r.url.contains("us-east-2") { "us-east-2" } else { "us-east-1" };
            if body.contains("Action=DescribeSpotPriceHistory") {
                let rows: &[(&str, f64)] = if region == "us-east-2" { &[("c8a.xlarge", 0.10), ("r8a.xlarge", 0.40)] } else { &[("c8a.xlarge", 0.10), ("r8a.xlarge", 0.30)] };
                let items = rows.iter().flat_map(|(t, p)| ["a", "b"].map(|z| format!("<item><instanceType>{t}</instanceType><availabilityZone>{region}{z}</availabilityZone><spotPrice>{p}</spotPrice></item>"))).collect::<String>();
                return Some(Response::new(200, "text/xml", format!("<spotPriceHistorySet>{items}</spotPriceHistorySet>")))
            }
            let full = body.contains("Action=RunInstances") && body.contains("c8a.xlarge") && ((region == "us-east-2" && ohio_full) || (region == "us-east-1" && virginia_full));
            full.then(|| Response::new(500, "text/xml", "<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code><Message>no room</Message></Error></Errors></Response>"))
        });
        let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
        // (N. Virginia's c8a pools: no machine five minutes ago, when another job asked.)
        if virginia_full { for z in ["a", "b"] { store.0.borrow_mut().insert(format!("aws:pool:us-east-1:c8a.xlarge:us-east-1{z}"), (now - 300_000).to_string()); } }
        let mut config = Config::new(PLANE_ID.into());
        config.app = Some(app());
        (config.aws_own, config.aws_own_creds) = own_aws();
        config.aws_regions = vec!["us-east-1".into(), "us-east-2".into()];
        config.instance_types = ["c8a.xlarge", "r8a.xlarge"].map(String::from).to_vec();
        let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
        block_on(plane.handle(webhook("queued", None)));
        let asked: Vec<(String, String)> = clouds.0.calls.borrow().iter().filter(|c| c.2.contains("Action=RunInstances")).map(|c| (if c.1.contains("us-east-2") { "us-east-2" } else { "us-east-1" }.to_string(),
            if c.2.contains("r8a.xlarge") { "r8a.xlarge" } else { "c8a.xlarge" }.to_string())).collect();
        let j = job(&store, 7);
        assert_eq!((j["region"].as_str(), j["machine_type"].as_str()), (Some(first), Some(got)), "Ohio full {ohio_full}, N. Virginia full {virginia_full}: {asked:?} {j}");
        if virginia_full { assert_eq!(asked[0], ("us-east-2".to_string(), "c8a.xlarge".to_string()), "{asked:?}") }
        assert_eq!(asked.iter().filter(|(r, t)| r == "us-east-2" && t == "c8a.xlarge").count(), ohio_asks, "Ohio's c8a pools asked at most once each: {asked:?}");
    }
}

/// Settings with an App, GitLab, a dashboard key and a move token.
fn odd_plane(config: &mut Config) {
    config.app = Some(app());
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    config.move_token = Some("move-token-0123456789".into());
}

#[test]
fn requests_that_name_nothing_here_or_cannot_be_read_are_said_as_that() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let post = |path: &str, body: &str| key(Request::new("POST", &format!("{PLANE_URL}{path}")).with_body(body.to_string()));
    // An address that is none: the control plane's failure, said.
    let r = run(Request::new("GET", "not an address"));
    assert!(r.status == 500 && !r.body_text().is_empty());
    // A key the dashboard was given that is kept under no name (set before names were kept): nothing to say hello to,
    // and no device listed.
    assert_eq!(run(post("/keys/hello", r#"{"device":"Build Mac"}"#)).status, 404);
    assert_eq!(json(&run(key(get("/keys"))))["devices"], serde_json::json!([]));
    // A machine saying something of a GitLab job this control plane does not have.
    for what in ["interrupted", "hello"] { assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/{what}?gl=ab-1&t=0123456789abcdef"))).status, 404, "{what}") }
    // GitLab's projects switched without saying which; an organization this control plane has no App of.
    let r = run(post("/gitlab/projects", "{}"));
    assert_eq!((r.status, r.body_text()), (500, "which project?".to_string()));
    assert_eq!(run(post("/github/uninstall", r#"{"app":5}"#)).body_text(), "no such App here");
    // Costs with nothing measured: none kept.
    assert_eq!(json(&run(post("/costs", "{}")))["kept"], 0);
    // Moving here: without the move token, refused; with it, what is no JSON is the asker's failure, said, and what is
    // not a pair of texts is passed over.
    assert_eq!(run(post("/move/import", "{}")).status, 403);
    let moving = |body: &str| run(post("/move/import", body).with_header("x-move-token", "move-token-0123456789"));
    assert_eq!(moving("not json").status, 500);
    store.0.borrow_mut().retain(|k, _| !k.starts_with("move_token_used:"));
    assert_eq!(json(&moving(r#"{"state":[[1,2],["other:1","x"],["glrunner:5","{\"id\":5}"]]}"#))["taken"], 1);
}

#[test]
fn a_job_whose_repository_github_gives_no_token_for_is_said_so_wherever_it_is_read() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    // (An installation GitHub no longer has: its token is refused.)
    store.0.borrow_mut().insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": 1_789_999_000_000u64, "installation_id": 999, "cloud": "aws", "seen_in_progress": true }).to_string());
    for (query, said) in [("link", "GitHub gave no token for its repository"), ("link=steps", "GitHub gave no token for its repository"), ("steps", "GitHub gave no token for its repository (is the App still installed there?)")] {
        let r = json(&run(key(get(&format!("/job/log?id=7&{query}")))));
        assert_eq!(r["error"], said, "{query}");
    }
    // Its run cancelled from the dashboard: GitHub cannot be asked.
    assert_eq!(json(&run(key(Request::new("POST", &format!("{PLANE_URL}/runs/3/cancel")))))["error"], "GitHub gave no token for its repository");
}

#[test]
fn an_app_installed_nowhere_has_no_public_repositories_and_nothing_to_uninstall_and_one_github_refuses_is_said_on_leaving() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let refused = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        if r.url.ends_with("/app/installations") {
            return Some(if refused.get() { Response::new(500, "application/json", r#"{"message":"Server Error"}"#) } else { Response::new(200, "application/json", "[]") })
        }
        if r.url.starts_with("https://gitlab.example/api/v4/projects?") { return Some(Response::new(200, "application/json", "[]")) }
        None
    });
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let mut beta = app();
    (beta.id, beta.owner) = (100, "beta".into());
    config.more_apps = vec![beta];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    assert_eq!(json(&run(key(get("/github/public-repos")))), serde_json::json!({ "repos": [], "failed": [] }));
    assert_eq!(json(&run(key(Request::new("POST", &format!("{PLANE_URL}/github/uninstall")).with_body(r#"{"app":100}"#))))["github_installations"], 0);
    // Leaving: GitHub fails for each App, said for each; GitLab's runner kept without its id is forgotten all the same.
    refused.set(true);
    store.0.borrow_mut().insert("glrunner:x".into(), serde_json::json!({ "gitlab": "" }).to_string());
    let left = json(&run(key(Request::new("POST", &format!("{PLANE_URL}/leave")))));
    assert_eq!((left["github_failed"].as_array().map(|f| f.len()), left["gitlab_runners"].as_u64()), (Some(2), Some(0)), "{left}");
    assert!(!store.0.borrow().contains_key("glrunner:x"));
}

#[test]
fn events_this_control_plane_does_nothing_with_are_answered_and_change_nothing() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let secret = "hook-secret-0123456789";
    // GitLab's: without its secret; of a status nothing is done for; over, of a job never known and on no runner.
    let mut unsigned = gitlab_event(1977, "pending", secret);
    unsigned.headers.retain(|(k, _)| k != "x-gitlab-token");
    assert_eq!(run(unsigned).status, 401);
    assert_eq!(run(gitlab_event(1977, "created", secret)).status, 200);
    assert_eq!(run(gitlab_event(1977, "success", secret)).status, 200);
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("job:") || k.starts_with("gone:")), "nothing kept of it");
    // GitHub's: an action nothing is done for; a signed event that is no JSON, the control plane's failure, said.
    assert_eq!(run(labelled("waiting", 7, "acme/app", None, "superci")).status, 202);
    let body = "not json";
    let sig = format!("sha256={}", hex(&hmac_sha256(b"whsec", body.as_bytes())));
    let r = run(Request::new("POST", &format!("{PLANE_URL}/webhook")).with_header("x-github-event", "workflow_job").with_header("x-hub-signature-256", &sig).with_body(body));
    assert!(r.status == 500 && !r.body_text().is_empty());
}

#[test]
fn a_gitlab_job_over_before_its_machine_came_is_cancelled_wherever_it_was() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let secret = "hook-secret-0123456789";
    let gl_job = |id: u64, state: &str, seen: bool| serde_json::json!({ "job_id": id, "run_id": 2366, "repo": "acme/app", "state": state, "at_ms": 1_789_999_000_000u64, "installation_id": 0, "cloud": "aws", "seen_in_progress": seen, "provider": "gitlab", "project_id": 380 }).to_string();
    let state = |id: u64| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("job:gl{id}")]).unwrap()["state"].as_str().unwrap().to_string();
    // Waiting for room, or its machine still being asked for: cancelled.
    store.0.borrow_mut().insert("job:gl1".into(), gl_job(1, "waiting", false));
    store.0.borrow_mut().insert("job:gl2".into(), gl_job(2, "launching", false));
    run(gitlab_event(1, "canceled", secret));
    run(gitlab_event(2, "canceled", secret));
    assert_eq!((state(1), state(2)), ("cancelled".to_string(), "cancelled".to_string()));
    // Running on another job's runner while its own record says it began already: that record is left as it is.
    store.0.borrow_mut().insert("glrunner:55".into(), serde_json::json!({ "id": 55, "job": 3, "gitlab": "" }).to_string());
    store.0.borrow_mut().insert("job:gl3".into(), gl_job(3, "launched", false));
    store.0.borrow_mut().insert("job:gl4".into(), gl_job(4, "running", true));
    run(gitlab_event_on(4, "running", secret, Some(55)));
    let four: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:gl4"]).unwrap();
    assert!(four["for_job"].is_null() && state(3) == "running", "{four}");
}

#[test]
fn a_damaged_signing_key_or_no_aws_credentials_fail_what_needs_them_saying_why() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    store.0.borrow_mut().insert("identity".into(), serde_json::json!({ "signing": { "private_b64": "!!", "kid": "k" } }).to_string());
    // Its public key cannot be read, nor anything signed: a launcher's token, the role's.
    let r = run(get("/.well-known/jwks.json"));
    assert!(r.status == 500 && !r.body_text().is_empty());
    run(job_event("queued", 7, "acme/app", None));
    assert!(job(&store, 7)["error"].as_str().is_some_and(|e| !e.is_empty()), "{}", job(&store, 7));
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": role_arn("123456789012", PLANE_ID), "connected": true }).to_string());
    run(job_event("queued", 8, "acme/app", None));
    assert!(job(&store, 8)["error"].as_str().is_some_and(|e| !e.is_empty()), "{}", job(&store, 8));
    // A control plane in AWS whose runtime gave it no credentials.
    let (store, mut config) = (Mem::default(), Config::new(PLANE_ID.into()));
    config.app = Some(app());
    config.aws_own = own_aws().0;
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 9, "acme/app", None)));
    assert!(job(&store, 9)["error"].as_str().is_some_and(|e| e.contains("no AWS credentials in this runtime")), "{}", job(&store, 9));
}

#[test]
fn many_jobs_kept_before_their_indexes_are_indexed_and_counted_over_several_sweeps_and_damaged_ones_are_passed_over() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    // More jobs than a sweep indexes at once (150), counted before runs were: each of a run of its own, one damaged.
    for id in 1..=160u64 {
        store.0.borrow_mut().insert(format!("job:{id:04}"), serde_json::json!({ "job_id": id, "run_id": id, "repo": "acme/app", "label": "superci", "state": "done", "at_ms": t - 3_600_000, "installation_id": 42,
            "cloud": "aws", "seen_in_progress": true, "began": true, "ended_ms": t - 3_000_000, "cost_usd": 0.01, "counted_usd": 0.01 }).to_string());
    }
    store.0.borrow_mut().insert("job:0050".into(), "not a job".into());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().contains_key("indexed") && store.0.borrow().contains_key("reindex-at"), "a part at a time");
    block_on(plane.alarm()).unwrap();
    // Each of them, the one a part ended at too.
    assert!(store.0.borrow().contains_key("indexed"));
    assert_eq!(store.0.borrow().keys().filter(|k| k.starts_with("recent:")).count(), 159);
    // Their runs counted a part at a time too, each once.
    let marks = || store.0.borrow().keys().filter(|k| k.starts_with("counted-run:")).count();
    assert!(!store.0.borrow().contains_key("runs-counted"));
    block_on(plane.alarm()).unwrap();
    assert!(store.0.borrow().contains_key("runs-counted"));
    assert_eq!(marks(), 159);
    // An index that points at a job no longer kept, or at a damaged one, and a damaged index: passed over where read.
    store.0.borrow_mut().insert("live:9999".into(), String::new());
    store.0.borrow_mut().insert("live:0050".into(), String::new());
    store.0.borrow_mut().insert("recent:x".into(), String::new());
    let status = json(&block_on(plane.handle(get("/status").with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert!(status["jobs"].as_array().is_some_and(|j| !j.is_empty()), "{status}");
}

/// A control plane in AWS with a dashboard key, for runs from a computer: what it answers to a new run.
fn run_plane(f: impl Fn(&dyn Fn(serde_json::Value) -> Response, &Mem)) {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let new_run = |body: serde_json::Value| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1").with_body(body.to_string())));
    f(&new_run, &store);
}

const LIB: &str = "on:\n  workflow_call:\n    inputs:\n      n:\n        type: number\n        required: true\njobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo ${{ inputs.n }}\n";

#[test]
fn a_runs_jobs_that_cannot_be_read_or_called_are_refused_saying_why() {
    run_plane(|new_run, _| {
        let refused = |workflow: &str, called: serde_json::Value| { let r = new_run(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app", "called": called })); (r.status, r.body_text()) };
        let calls = "jobs:\n  call:\n    uses: ./.github/workflows/lib.yml\n";
        for (workflow, called, said) in [
            (calls, serde_json::json!({}), "which was not sent with the run"),
            (calls, serde_json::json!({ ".github/workflows/lib.yml": "jobs: [" }), ".github/workflows/lib.yml: "),
            (calls, serde_json::json!({ ".github/workflows/lib.yml": "on:\n  workflow_call:\njobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    strategy:\n      matrix: [1, 2]\n" }), ".github/workflows/lib.yml's job a: the matrix is not a mapping"),
            ("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    strategy:\n      matrix:\n        os: linux\n", serde_json::json!({}), "the job a: the matrix's os is not a list"),
            // Read as the run starts: its name, how long it may take, whether it may fail, its machine, its matrix.
            ("jobs:\n  a:\n    name: ${{ ) }}\n    runs-on: superci\n    steps:\n      - run: echo\n", serde_json::json!({}), "unexpected RParen"),
            ("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    timeout-minutes: ${{ ) }}\n", serde_json::json!({}), "unexpected RParen"),
            ("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    timeout-minutes: soon\n", serde_json::json!({}), "the job a's timeout-minutes is not a number"),
            ("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    continue-on-error: ${{ ) }}\n", serde_json::json!({}), "unexpected RParen"),
            ("jobs:\n  a:\n    runs-on: superci/cpu=${{ ) }}\n    steps:\n      - run: echo\n", serde_json::json!({}), "unexpected RParen"),
            ("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    strategy:\n      matrix: ${{ ) }}\n", serde_json::json!({}), "the job a's matrix: "),
            // A job that needs one the file does not have; a call of a workflow that may not be called, with a matrix,
            // or whose job calls another in turn.
            ("jobs:\n  a:\n    needs: b\n    runs-on: superci\n    steps:\n      - run: echo\n", serde_json::json!({}), "the job a needs b, which .github/workflows/ci.yml does not have"),
            (calls, serde_json::json!({ ".github/workflows/lib.yml": "on: push\njobs:\n  a:\n    runs-on: superci\n" }), "which is not a workflow that may be called"),
            ("jobs:\n  call:\n    uses: ./.github/workflows/lib.yml\n    strategy:\n      matrix:\n        v: [1]\n", serde_json::json!({ ".github/workflows/lib.yml": LIB }), "calls a workflow with a matrix"),
            (calls, serde_json::json!({ ".github/workflows/lib.yml": "on:\n  workflow_call:\njobs:\n  a:\n    uses: ./.github/workflows/other.yml\n" }), "calls a workflow in turn"),
            // A caller whose `if` cannot be read.
            ("jobs:\n  call:\n    if: ${{ ) }}\n    uses: ./.github/workflows/lib.yml\n", serde_json::json!({ ".github/workflows/lib.yml": LIB }), "the job call's if: "),
            // An input given that cannot be read.
            ("jobs:\n  call:\n    uses: ./.github/workflows/lib.yml\n    with:\n      n: ${{ ) }}\n", serde_json::json!({ ".github/workflows/lib.yml": LIB }), "unexpected RParen"),
        ] {
            let (status, body) = refused(workflow, called);
            assert!(status == 400 && body.contains(said) && !body.is_empty(), "{workflow}: {status} {body}");
        }
    });
}

#[test]
fn a_runs_called_workflows_and_expressions_are_read_as_github_reads_them() {
    run_plane(|new_run, store| {
        let run = |workflow: &str, called: serde_json::Value| -> serde_json::Value {
            let r = new_run(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app", "called": called, "secrets": [["X", "secret-x"]] }));
            assert_eq!(r.status, 200, "{workflow}: {}", r.body_text());
            let id = json(&r)["run_id"].as_str().unwrap().to_string();
            serde_json::from_str(&store.0.borrow()[&format!("run:{id}")]).unwrap()
        };
        let states = |r: &serde_json::Value| r["jobs"].as_array().unwrap().iter().map(|j| format!("{}={}{}", j["name"].as_str().or(j["key"].as_str()).unwrap_or_default(), j["state"].as_str().unwrap(), j["result"].as_str().map(|r| format!(":{r}")).unwrap_or_default())).collect::<Vec<_>>();
        // A called workflow whose jobs need each other: named within the call.
        let r = run("jobs:\n  call:\n    uses: ./.github/workflows/two.yml\n", serde_json::json!({ ".github/workflows/two.yml": "on:\n  workflow_call:\njobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n  b:\n    needs: a\n    runs-on: superci\n    steps:\n      - run: echo\n" }));
        assert_eq!(r["jobs"][1]["needs"], serde_json::json!(["call/a"]));
        // A called workflow with no jobs: nothing to run, the run over at once.
        assert!(run("jobs:\n  call:\n    uses: ./.github/workflows/none.yml\n", serde_json::json!({ ".github/workflows/none.yml": "on:\n  workflow_call:\njobs: {}\n" }))["jobs"].as_array().unwrap().is_empty());
        // A caller that does not run, and one that does: its secrets as it names them (what is no text, or cannot be
        // read, is none), its machine in an environment named by an expression.
        let r = run("jobs:\n  skipped:\n    if: false\n    uses: ./.github/workflows/lib.yml\n  call:\n    if: true\n    uses: ./.github/workflows/lib.yml\n    with:\n      n: '3'\n    secrets:\n      A: ${{ secrets.X }}\n      B:\n        n: 1\n      C: ${{ ) }}\n      ? [odd]\n      : x\n",
            serde_json::json!({ ".github/workflows/lib.yml": LIB.replace("    runs-on: superci\n    steps:\n      - run: echo\n", "    runs-on: superci\n    steps:\n      - run: echo\n    environment: ${{ 1 }}\n") }));
        assert_eq!(states(&r), ["skipped / a=done:skipped", "call / a=started"]);
        assert_eq!(r["jobs"][1]["inputs"], serde_json::json!({ "n": 3.0 }));
        let token = r["jobs"][1]["token"].as_str().unwrap();
        let handed: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("rjob:{token}")]).unwrap();
        assert!(handed.to_string().contains("secret-x"), "the secret it names, given");
        // A matrix read as the run starts: of values written with an expression in them, or of none (failed, as GitHub fails it).
        let r = run("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    strategy:\n      matrix:\n        v: [\"x-${{ 1 }}\"]\n", serde_json::json!({}));
        assert_eq!(states(&r), ["a (x-1)=started"]);
        let r = run("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n    strategy:\n      matrix: ${{ fromJSON('{\"v\":[]}') }}\n", serde_json::json!({}));
        assert_eq!(states(&r), ["a (its matrix has no combinations)=done:failure"]);
        // A job of its own in an environment an expression names as no text (a number): named as its text.
        let r = run("jobs:\n  a:\n    runs-on: superci\n    environment: ${{ 1 }}\n    steps:\n      - run: echo\n", serde_json::json!({}));
        assert_eq!(states(&r), ["a=started"]);
    });
}

#[test]
fn a_damaged_or_gone_run_record_is_left_as_it_is_and_says_why_where_it_is_moved_on() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |k: &str, v: serde_json::Value| { store.0.borrow_mut().insert(k.into(), v.to_string()); };
    // A started job without its id or token; a run whose called workflow is no workflow; a job of a called workflow
    // the run does not have.
    put("run:1", serde_json::json!({ "at_ms": 1_790_000_000_000u64, "workflow": "jobs:\n  a:\n    runs-on: superci\n", "jobs": [{ "key": "a", "state": "started" }] }));
    put("run:2", serde_json::json!({ "at_ms": 1_790_000_000_000u64, "workflow": "jobs:\n  a:\n    runs-on: superci\n", "called": { "x.yml": "jobs: [" }, "jobs": [{ "key": "a", "state": "pending", "needs": [] }] }));
    put("run:3", serde_json::json!({ "at_ms": 1_790_000_000_000u64, "workflow": "jobs:\n  a:\n    runs-on: superci\n", "jobs": [{ "key": "lib/a", "job_key": "a", "source": "lib.yml", "call": "lib", "state": "pending", "needs": [] }] }));
    block_on(plane.alarm()).unwrap();
    for id in [1, 2, 3] { assert!(store.0.borrow().contains_key(&format!("run:{id}")), "run {id} kept") }
    assert!(!store.0.borrow().contains_key("run-error:1"));
}

#[test]
fn what_a_run_from_a_computer_is_asked_for_that_it_does_not_have_is_said_as_that() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    // A run of one job and its working tree, on a control plane with nowhere to run it: its job fails, its record stays.
    let tree = "tree-0123456789abcdef";
    let made = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n", "path": ".github/workflows/ci.yml", "repository": "acme/app", "tree": tree, "parts": 1 }).to_string())));
    assert_eq!(made.status, 200, "{}", made.body_text());
    let id = json(&made)["run_id"].as_str().unwrap().to_string();
    let record = || -> serde_json::Value { serde_json::from_str(&store.0.borrow()[&format!("run:{id}")]).unwrap() };
    let token = record()["jobs"][0]["token"].as_str().unwrap().to_string();
    let ask = |method: &str, path: &str| run(key(Request::new(method, &format!("{PLANE_URL}{path}"))));
    // Its working tree: a part with nothing kept is empty; a piece that is no base64, or a part in a bucket of an AWS
    // account this control plane does not have, is said.
    let part = || run(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/superci/tree?part=0")));
    assert_eq!((part().status, part().body.len()), (200, 0));
    store.0.borrow_mut().insert(format!("rtree:{tree}:00000:000"), "!!".into());
    assert_eq!(part().status, 500);
    store.0.borrow_mut().insert(format!("rtree-s3:{tree}:00000"), "us-east-1|runs/x/00000".into());
    assert_eq!(part().status, 409);
    // Lines from a runner of no job here; lines that say no step (the step is the address's).
    let lines = |t: &str| run(Request::new("POST", &format!("{PLANE_URL}/runner/{t}/_apis/distributedtask/hubs/Actions/plans/p/TimeLineWebConsoleLog/t/0a1b2c3d")).with_body(r#"{"value":["a line"]}"#));
    assert_eq!(lines("no-such-runner-0123456789").status, 404);
    assert_eq!(lines(&token).status, 200);
    assert!(store.0.borrow().iter().any(|(k, v)| k.starts_with(&format!("rline:{token}:")) && v.contains("0a1b2c3d")));
    // Its log, its artifacts, its status, its cancelling: of a run that is none, of a job that is none, of a step whose
    // lines are asked for.
    for path in ["/runs/x/log", "/runs/x/artifacts", "/runs/x", "/runs/x/cancel"] { assert_eq!(ask(if path.ends_with("cancel") { "POST" } else { "GET" }, path).status, 404, "{path}") }
    assert_eq!(ask("GET", &format!("/runs/{id}/log?job=5&step=0a1b2c3d")).status, 404);
    assert_eq!(ask("GET", &format!("/runs/{id}/log")).status, 400);
    assert_eq!(ask("GET", &format!("/runs/{id}/log?job=5&log=1")).status, 404);
    assert_eq!(ask("GET", &format!("/runs/{id}/log?job=0&step=0a1b2c3d&lines=1")).body_text(), "a line");
    store.0.borrow_mut().entry(format!("run:{id}")).and_modify(|v| { let mut r: serde_json::Value = serde_json::from_str(v).unwrap(); r["jobs"][0].as_object_mut().unwrap().remove("token"); *v = r.to_string() });
    assert_eq!(ask("GET", &format!("/runs/{id}/log?job=0&step=0a1b2c3d&lines=1")).status, 404, "a job with no runner's link has no lines");
    // An artifact asked of a control plane with no AWS account.
    store.0.borrow_mut().insert(format!("artifact:{id}:out"), serde_json::json!({ "id": "e1", "region": "us-east-1", "pieces": [["p00000", 5]], "size": 5, "made": 1, "done": true }).to_string());
    let a = ask("GET", &format!("/runs/{id}/artifacts?name=out"));
    assert_eq!((a.status, a.body_text()), (409, "this control plane has no AWS account".to_string()));
    // A run kept without its jobs, or damaged.
    store.0.borrow_mut().insert("run:77".into(), serde_json::json!({ "at_ms": 1_790_000_000_000u64 }).to_string());
    assert_eq!(json(&ask("GET", "/runs/77"))["jobs"], serde_json::json!([]));
    assert_eq!(json(&ask("POST", "/runs/77/cancel"))["cancelled"], true);
    store.0.borrow_mut().insert("run:78".into(), "not a run".into());
    assert_eq!(ask("POST", "/runs/78/cancel").status, 500);
}

/// A run of one job asked for on a control plane in AWS, with `meddle` doing what another request does meanwhile (see
/// `Mem::meddling`): what it answered, and its store.
fn run_meddled(workflow: &str, meddle: impl FnMut(&str, &str, &mut BTreeMap<String, String>) + 'static) -> (Response, Mem) {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    store.meddling(meddle);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string())));
    (r, store)
}

#[test]
fn a_run_taken_away_or_changed_by_another_request_while_it_is_moved_on_is_left_to_that_one() {
    let one = "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n";
    // Gone (the upkeep removed it) as its first job was to start: nothing more is done of it.
    let (r, store) = run_meddled(one, |op, k, m| if op == "swap" && k.starts_with("run:") { m.remove(k); });
    assert_eq!(r.status, 200, "{}", r.body_text());
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("job:")), "no job started");
    // A job whose start failed (its machine cannot be read), as another request takes the run away, damages it, ends
    // the job, or keeps writing it: its start is not given back to the run, or given back once it can be.
    let bad = "jobs:\n  a:\n    runs-on: superci/cpu=${{ ) }}\n    steps:\n      - run: echo\n";
    let after_its_runner_went = |f: fn(&mut serde_json::Value) -> Option<String>| {
        let gone = std::rc::Rc::new(std::cell::Cell::new(false));
        run_meddled(bad, move |op, k, m| {
            if op == "delete" && k.starts_with("rjob:") { gone.set(true) }
            if gone.get() && (op == "get" || op == "swap") && k.starts_with("run:") {
                let mut run: serde_json::Value = m.get(k).and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
                match f(&mut run) { Some(v) => { m.insert(k.to_string(), v); } None => { m.remove(k); } }
            }
        })
    };
    for f in [(|_: &mut serde_json::Value| None) as fn(&mut serde_json::Value) -> Option<String>, |_| Some("not a run".into()),
        |r| { r["jobs"][0]["state"] = "done".into(); Some(r.to_string()) }, |r| { r["changed"] = (r["changed"].as_u64().unwrap_or(0) + 1).into(); Some(r.to_string()) }] {
        let (r, _) = after_its_runner_went(f);
        assert!(r.status == 400 && r.body_text().contains("unexpected RParen"), "{} {}", r.status, r.body_text());
    }
    // Its cancelling, as the run is taken away: nothing here.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    store.0.borrow_mut().insert("run:9".into(), serde_json::json!({ "at_ms": 1_790_000_000_000u64, "jobs": [{ "key": "a", "state": "pending" }] }).to_string());
    store.meddling(|op, k, m| if op == "swap" && k == "run:9" { m.remove(k); });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    assert_eq!(block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs/9/cancel")).with_header("authorization", "Bearer dashboard-session-key-1"))).status, 404);
}

#[test]
fn the_upkeep_ends_what_waited_too_long_or_was_cut_off_and_lets_go_of_failing_runners_github_no_longer_wants() {
    let t = 1_790_000_000_000u64;
    let hook = format!("{PLANE_URL}/gitlab/webhook");
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        let (u, json) = (r.url.as_str(), |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string())));
        match u {
            "https://api.github.com/repos/acme/app/actions/jobs/5" => json(serde_json::json!({ "status": "in_progress" })),
            "https://api.github.com/repos/acme/app/actions/jobs/8" => json(serde_json::json!({ "status": "completed", "runner_name": null })),
            "https://api.github.com/orgs/acme/actions/runners/66" if r.method == "DELETE" => Some(Response::new(204, "application/json", "")),
            // Waiting jobs GitHub lists: one without its id.
            "https://api.github.com/repos/acme/app/actions/runs?status=queued&per_page=10" => json(serde_json::json!({ "workflow_runs": [{ "id": 90 }] })),
            "https://api.github.com/repos/acme/app/actions/runs/90/jobs?per_page=50" => json(serde_json::json!({ "jobs": [{ "status": "queued", "name": "no id" }] })),
            // GitLab's pending jobs: one without its id.
            _ if u.starts_with("https://gitlab.example/api/v4/projects?membership=true") => json(serde_json::json!([{ "id": 380, "path_with_namespace": "acme/app" }])),
            "https://gitlab.example/api/v4/projects/380/hooks" => json(serde_json::json!([{ "id": 1, "url": hook }])),
            _ if u.starts_with("https://gitlab.example/api/v4/projects/380/jobs?scope[]=pending") => json(serde_json::json!([{ "name": "no id" }])),
            _ => None,
        }
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |id: &str, j: serde_json::Value| {
        let mut base = serde_json::json!({ "job_id": 0, "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": t - 60_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": false, "label": "superci" });
        for (k, v) in j.as_object().unwrap() { base[k] = v.clone() }
        store.0.borrow_mut().insert(format!("job:{id}"), base.to_string());
    };
    // Waiting for room a day; its start cut off a third time; a failing runner GitHub no longer wants, for half an
    // hour, its job taken (5), or no token for its repository (6).
    put("1", serde_json::json!({ "job_id": 1, "at_ms": t - 25 * 3_600_000 }));
    put("2", serde_json::json!({ "job_id": 2, "state": "launching", "retries": 2, "launched_ms": t - 180_000 }));
    put("4", serde_json::json!({ "job_id": 4, "state": "failed", "fail_pending": "why", "ended_ms": t - 31 * 60_000 }));
    put("5", serde_json::json!({ "job_id": 5, "state": "failed", "fail_pending": "why", "ended_ms": t - 60_000 }));
    put("6", serde_json::json!({ "job_id": 6, "state": "failed", "fail_pending": "why", "ended_ms": t - 60_000, "installation_id": 999 }));
    // A machine whose job was cancelled before any runner took it, the note of it missed: its runner taken back.
    put("7", serde_json::json!({ "job_id": 7, "state": "launched", "launched_ms": t - 30_000, "machine_id": "i-7", "region": "us-east-1", "runner": "r-7", "runner_id": 66 }));
    // (Its address known, as after any request: its GitLab projects' webhooks are told apart by it.)
    block_on(plane.handle(get("/health")));
    store.0.borrow_mut().insert("gone:7".into(), t.to_string());
    // A machine whose job GitHub took elsewhere, with no runner of its own registered.
    put("8", serde_json::json!({ "job_id": 8, "state": "launched", "launched_ms": t - 9 * 60_000, "machine_id": "i-8", "region": "us-east-1" }));
    // A GitLab machine whose record says no project.
    put("gl9", serde_json::json!({ "job_id": 9, "state": "launched", "launched_ms": t - 9 * 60_000, "machine_id": "i-9", "region": "us-east-1", "provider": "gitlab" }));
    // A launch template note that names no template (passed over).
    store.0.borrow_mut().insert("aws:template:nothing".into(), (t - 2 * 86_400_000).to_string());
    block_on(plane.alarm()).unwrap();
    let state = |id: &str| { let j: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("job:{id}")]).unwrap(); (j["state"].as_str().unwrap_or_default().to_string(), j["fail_pending"].is_null()) };
    assert_eq!(state("1"), ("failed".to_string(), true));
    assert_eq!(state("2"), ("failed".to_string(), true));
    assert!(state("4").1 && state("5").1 && state("6").1, "let go of");
    assert_eq!(state("7").0, "cancelled");
    assert_eq!(state("8").0, "swept");
    assert_ne!(state("gl9").0, "launched");
    assert!(store.0.borrow().contains_key("aws:template:nothing"), "passed over");
}

#[test]
fn just_after_a_move_here_the_upkeep_comes_back_within_the_minute_and_looks_for_jobs_github_did_not_tell_of() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // No job yet: nothing to look for.
    block_on(plane.alarm()).unwrap();
    store.0.borrow_mut().insert("claimed_at".into(), "1790000000000".into());
    // Two jobs of one repository, kept before the indexes: asked of once.
    for id in [1, 2] { store.0.borrow_mut().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string()); }
    store.0.borrow_mut().remove("indexed");
    wakes.0.borrow_mut().clear();
    // (A runtime started anew: what the last one kept in memory, that the indexes were built, is not known.)
    let cache = Cache::default();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.alarm()).unwrap();
    assert_eq!(wakes.0.borrow().last(), Some(&60_000));
    assert_eq!(clouds.calls.borrow().iter().filter(|c| c.1.contains("/repos/acme/app/actions/runs?status=queued")).count(), 1);
}

/// AWS connected by the role the dashboard made for this control plane: its credentials asked of STS.
fn connect_role(store: &Mem) {
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": role_arn("123456789012", PLANE_ID), "connected": true }).to_string());
}

/// A machine's link to the cache, kept as `superci-cache`'s start gives it, for job 5001 of acme/app (GitHub's
/// repository 77) on its branch main, in `region`.
fn cache_machine(store: &Mem, link: &str, region: &str) {
    let mut m = store.0.borrow_mut();
    m.insert(format!("cachem:{link}"), serde_json::json!({ "region": region, "arch": "x86_64", "until": u64::MAX / 4, "runner": "a-runner", "job": "job:5001" }).to_string());
    m.insert("job:5001".into(), serde_json::json!({ "job_id": 5001, "run_id": 1, "repo": "acme/app", "repo_id": 77, "branch": "main", "default_branch": "main", "state": "running", "at_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
}

#[test]
fn a_machine_whose_region_makes_no_address_is_said_so_at_each_link_and_what_it_sends_amiss_is_passed_over() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithCache { clouds: FakeClouds::default(), keys: github_jobs_key().1 };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    connect_role(&store);
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let (bad, good) = ("machine-cache-link-0000000000bad", "machine-cache-link-000000000good");
    cache_machine(&store, bad, "us east-1");
    cache_machine(&store, good, "us-east-1");
    let call = |link: &str, what: &str, body: serde_json::Value| { let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string()))); (r.status, r.body_text()) };
    let main = github_job_token(r#"[{"Scope":"refs/heads/main","Permission":3}]"#, "77");
    let v = "a1b2c3";
    // Each link to its region's bucket cannot be made: the program, an entry's pieces to write and to read, a tool's.
    assert_eq!(block_on(plane.handle(get(&format!("/cache/{bad}/program")))).status, 500);
    assert_eq!(block_on(plane.handle(get(&format!("/cache/{bad}/layers-program")))).status, 500);
    assert_eq!(call(bad, "begin", serde_json::json!({ "token": main, "key": "deps-1", "version": v })).0, 200);
    assert_eq!(call(bad, "sign", serde_json::json!({ "token": main, "key": "deps-1", "version": v, "names": ["b0"] })).0, 500);
    // (Pieces it says that are no name and size are passed over.)
    let (status, said) = call(bad, "finish", serde_json::json!({ "token": main, "key": "deps-1", "version": v, "pieces": [["b0", "ten"], [1, 2], ["b1", 5]] }));
    assert_eq!((status, said.contains("\"size\":5")), (200, true), "{said}");
    assert_eq!(call(bad, "lookup", serde_json::json!({ "token": main, "key": "deps-1", "version": v })).0, 500);
    assert_eq!(call(bad, "tool-begin", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef" })).0, 500);
    assert_eq!(call(bad, "tool-sign", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef", "names": ["p00001"] })).0, 500);
    assert_eq!(call(bad, "tool-finish", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef", "pieces": [["p00000", 5], [1, 2]] })).0, 200);
    assert_eq!(call(bad, "tool-get", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef" })).0, 500);
    // A job that may only read the cache saves nothing; a body that is no JSON is said.
    let reader = github_job_token(r#"[{"Scope":"refs/heads/main","Permission":1}]"#, "77");
    assert_eq!(call(good, "begin", serde_json::json!({ "token": reader, "key": "deps-2", "version": v })).0, 403);
    let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{good}/lookup")).with_body("not json")));
    assert!(r.status == 500 && r.body_text().starts_with("not JSON"));
    // A GitLab machine whose job runs for a branch SuperCI does not keep.
    store.0.borrow_mut().insert("cachem:machine-cache-link-0000000gitlab".into(), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": "gl:61" }).to_string());
    store.0.borrow_mut().insert("glrunner:61".into(), serde_json::json!({ "id": 61, "gitlab": "", "running": { "job": 1, "project_id": 380, "project": "acme/app", "ref": "a:b", "tag": false } }).to_string());
    assert_eq!(call("machine-cache-link-0000000gitlab", "tool-get", serde_json::json!({ "tool": "gitlab", "hash": "0123456789abcdef" })).0, 403);
    // A path of the layer store's it does not know.
    assert_eq!(block_on(plane.handle(get(&format!("/cache/{good}/more/layers")))).status, 404);
}

#[test]
fn githubs_keys_that_are_no_json_or_a_damaged_signing_key_leave_a_jobs_token_unread_saying_why() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = With(FakeClouds::default(), |r: &Request, _: &str| (r.url == "https://token.actions.githubusercontent.com/.well-known/jwks").then(|| Response::new(200, "application/json", "not json")));
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let link = "machine-cache-link-000000000good";
    cache_machine(&store, link, "us-east-1");
    let lookup = |token: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/lookup")).with_body(serde_json::json!({ "token": token, "key": "k", "version": "v1" }).to_string())));
    let r = lookup(&github_job_token("[]", "77"));
    assert!(r.status == 500 && r.body_text().starts_with("GitHub's keys"), "{}", r.body_text());
    // A token of this control plane's (ES256), its signing key damaged.
    store.0.borrow_mut().insert("identity".into(), serde_json::json!({ "signing": { "private_b64": "!!", "kid": "k" } }).to_string());
    let es256 = format!("{}.e30.sig", URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256"}"#));
    assert_eq!(lookup(&es256).status, 500);
    // With no AWS account, no program to send a machine to.
    let (store, config) = (Mem::default(), Config::new(PLANE_ID.into()));
    cache_machine(&store, link, "us-east-1");
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    assert_eq!(block_on(plane.handle(get(&format!("/cache/{link}/program")))).status, 404);
}

#[test]
fn the_layer_store_passes_over_what_is_kept_damaged_and_says_what_a_machine_asks_amiss() {
    let t = 1_790_000_000_000u64;
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = WithLayers { clouds: FakeClouds::default(), objects: Default::default(), converted: Default::default(), deleted: Default::default() };
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let (good, bad) = ("machine-cache-link-000000000good", "machine-cache-link-0000000000bad");
    cache_machine(&store, good, "us-east-1");
    cache_machine(&store, bad, "us east-1");
    let digest = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    let layer = |made: u64, used: u64| serde_json::json!({ "size": 10, "diff_id": "d", "erofs": { "key": "layers/x.erofs", "size": 5 }, "blob": { "key": "layers/x.blob" }, "made": made, "used": used, "public": { "registry": "docker.io", "repository": "library/alpine", "checked": t } }).to_string();
    {
        let mut m = store.0.borrow_mut();
        m.insert(format!("layer:us-east-1:{}", digest('a')), layer(t, t - 2 * 86_400_000));
        m.insert(format!("layer:us-east-1:{}", digest('b')), "damaged".into());
        // On its way: being converted for anyone, or queued for anyone.
        m.insert(format!("lconv:us-east-1:{}", digest('c')), serde_json::json!({ "at": t, "size": 10, "public": { "registry": "docker.io", "repository": "library/alpine", "checked": t } }).to_string());
        m.insert(format!("lwant:us-east-1:{}", digest('d')), serde_json::json!({ "at": t, "size": 10, "source": {}, "public": { "registry": "docker.io", "repository": "library/alpine", "checked": t } }).to_string());
    }
    let call = |link: &str, what: &str, body: serde_json::Value| { let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string()))); (r.status, r.body_text()) };
    // The index passes over a damaged record; links for nothing asked are none.
    let index = block_on(plane.handle(get(&format!("/cache/{good}/layers.json"))));
    assert!(index.status == 200 && index.body_text().contains(&digest('a')) && !index.body_text().contains(&digest('b')), "{}", index.body_text());
    assert_eq!(call(good, "layers-links", serde_json::json!({})).0, 200);
    store.0.borrow_mut().insert(format!("layer:us-east-1:{}", digest('0')), layer(t, t));
    assert!(call(good, "layers-links", serde_json::json!({ "digests": [digest('0')] })).1.contains(&digest('0')));
    // Proposed: nothing; what is on its way already; from a machine whose region makes no address, what the store lacks.
    assert_eq!(call(good, "layers-propose", serde_json::json!({ "layers": [] })).0, 200);
    let (_, said) = call(good, "layers-propose", serde_json::json!({ "layers": [{ "digest": digest('c'), "size": 10 }, { "digest": digest('d'), "size": 10 }] }));
    assert_eq!(said.matches("\"have\"").count(), 2, "{said}");
    assert_eq!(call(bad, "layers-propose", serde_json::json!({ "layers": [{ "digest": digest('e'), "size": 10 }] })).0, 500);
    // What a job had from the store, said after it: a layer used long ago noted used (one used today left as it is),
    // one the store lacks passed over.
    store.0.borrow_mut().insert(format!("layer:us-east-1:{}", digest('i')), layer(t, t));
    assert_eq!(call(good, "layers-kept", serde_json::json!({ "served_digests": [digest('a'), digest('f'), digest('i')] })).0, 200);
    assert!(store.0.borrow()[&format!("layer:us-east-1:{}", digest('a'))].contains(&format!("\"used\":{t}")));
    // A machine whose job is not kept, keeping its images.
    store.0.borrow_mut().insert(format!("cachem:{bad}"), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": "", "job": "job:404" }).to_string());
    assert_eq!(call(bad, "layers-kept", serde_json::json!({ "keeping": true })).0, 404);
    assert_eq!(call(bad, "layers-kept", serde_json::json!({})).0, 404);
    // The upkeep: a conversion kept damaged goes; a queued one kept under no region, or damaged, is passed over.
    {
        let mut m = store.0.borrow_mut();
        m.insert(format!("lconv:us-east-1:{}", digest('g')), "damaged".into());
        m.insert("lwant:nowhere".into(), "{}".into());
        m.insert("lconv:nowhere".into(), "{}".into());
        m.insert(format!("lwant:us-east-1:{}", digest('h')), "damaged".into());
        // (Two queued in one region: its room counted once.)
        m.insert(format!("lwant:us-east-1:{}", digest('j')), serde_json::json!({ "at": t, "size": 10, "source": {} }).to_string());
        m.insert(format!("lwant:us-east-1:{}", digest('k')), serde_json::json!({ "at": t + 1, "size": 10, "source": {} }).to_string());
    }
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().contains_key(&format!("lconv:us-east-1:{}", digest('g'))));
}

#[test]
fn a_control_plane_not_in_aws_converts_no_layer_and_gives_its_index_without_links() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let config = Config::new(PLANE_ID.into());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let link = "machine-cache-link-000000000good";
    cache_machine(&store, link, "us-east-1");
    let a = format!("sha256:{}", "a".repeat(64));
    store.0.borrow_mut().insert(format!("layer:us-east-1:{a}"), serde_json::json!({ "size": 10, "diff_id": "d", "erofs": { "key": "layers/x.erofs", "size": 5 }, "blob": { "key": "layers/x.blob" }, "made": t, "used": t, "public": { "registry": "docker.io", "repository": "library/alpine", "checked": t } }).to_string());
    let index = block_on(plane.handle(get(&format!("/cache/{link}/layers.json"))));
    assert!(index.status == 200 && index.body_text().contains("\"url\":\"\""), "{}", index.body_text());
    // A layer queued as a control plane in AWS would queue it, on one connected by a role: not converted here.
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": role_arn("123456789012", PLANE_ID), "connected": true }).to_string());
    store.0.borrow_mut().insert(format!("lwant:us-east-1:{a}"), serde_json::json!({ "at": t, "size": 10, "source": {} }).to_string());
    block_on(plane.alarm()).unwrap();
    assert_eq!(store.0.borrow().get("layers-convert-error").map(String::as_str), Some("only a control plane on AWS converts layers"));
}

#[test]
fn a_runners_calls_that_bring_nothing_are_answered_as_its_service_answers_them() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let made = run(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n", "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string()));
    let id = json(&made)["run_id"].as_str().unwrap().to_string();
    let token = serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("run:{id}")]).unwrap()["jobs"][0]["token"].as_str().unwrap().to_string();
    let call = |method: &str, path: &str, body: &str| run(Request::new(method, &format!("{PLANE_URL}/runner/{token}{path}")).with_body(body.to_string()));
    // The cache's other calls, from a machine without SuperCI's cache: no entry. Actions to download: none asked.
    let r = call("POST", "/twirp/github.actions.results.api.v1.CacheService/DeleteCacheEntry", "{}");
    assert_eq!((r.status, json(&r)["code"].as_str()), (404, Some("not_found")));
    assert_eq!(json(&call("POST", "/_apis/v1/ActionDownloadInfo/p/j", "{}"))["actions"], serde_json::json!({}));
    // GitHub's artifacts, asked of a machine without SuperCI's cache: said as not done there.
    assert_eq!(call("POST", "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact", "{}").status, 501);
    // Steps' records with none in them; a log's content of nothing.
    assert_eq!(call("PATCH", "/_apis/v1/Timeline/p/t", r#"{"value":[]}"#).status, 200);
    assert_eq!(call("POST", "/_apis/v1/Logfiles/p/1", "").status, 200);
    assert!(!store.0.borrow().keys().any(|k| k.starts_with(&format!("rlog:{token}:"))));
    // Lines that say no step, sent another way than the runner sends them: the step is the address's.
    assert_eq!(call("PUT", "/_apis/v1/TimeLineWebConsoleLog/p/t/0a1b2c3d", r#"{"value":["a line"]}"#).status, 200);
    assert!(store.0.borrow().iter().any(|(k, v)| k.starts_with(&format!("rline:{token}:")) && v.contains("0a1b2c3d")));
}

#[test]
fn a_job_whose_label_app_gitlab_or_run_is_gone_fails_at_its_next_start_saying_which() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |key: &str, j: serde_json::Value| {
        let mut base = serde_json::json!({ "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": t - 60_000, "installation_id": 42, "cloud": "", "seen_in_progress": false });
        for (k, v) in j.as_object().unwrap() { base[k] = v.clone() }
        store.0.borrow_mut().insert(key.into(), base.to_string());
    };
    // A label that is no machine, or another control plane's; a run from a computer's job kept without its link; a
    // GitLab job of a connection removed; a GitHub job of an App removed; one whose spot machine AWS took back, of an
    // installation GitHub no longer gives a token for (asked again the next minute).
    put("job:1", serde_json::json!({ "job_id": 1, "label": "superci/cpu=many" }));
    put("job:2", serde_json::json!({ "job_id": 2, "label": "someone-else" }));
    put("job:3", serde_json::json!({ "job_id": 3, "label": "superci", "provider": "local" }));
    put("job:glgone-4", serde_json::json!({ "job_id": 4, "label": "superci", "provider": "gitlab", "gitlab": "gone", "project_id": 380 }));
    put("job:5", serde_json::json!({ "job_id": 5, "label": "superci", "app_id": 12345 }));
    put("job:6", serde_json::json!({ "job_id": 6, "label": "superci", "state": "done", "installation_id": 999, "began": true, "ended_ms": t - 30_000,
        "rerun": { "job_id": 6, "run_id": 3, "repo": "acme/app", "name": "test", "state": "pending", "at_ms": t - 60_000 } }));
    block_on(plane.alarm()).unwrap();
    let said = |key: &str| { let j: serde_json::Value = serde_json::from_str(&store.0.borrow()[key]).unwrap(); (j["state"].as_str().unwrap_or_default().to_string(), j["error"].as_str().unwrap_or_default().to_string()) };
    for (key, why) in [("job:1", "cpu"), ("job:2", "is not this control plane's label"), ("job:3", "a run from a computer without its address"), ("job:glgone-4", "its GitLab is not connected any more"), ("job:5", "no GitHub App")] {
        let (state, error) = said(key);
        // (Failed at once, or tried again twice when it may be a hiccup: saying why either way.)
        assert!(["failed", "waiting"].contains(&state.as_str()) && error.contains(why), "{key}: {state} {error}");
    }
    assert_eq!(said("job:6").0, "done");
    assert!(store.0.borrow()["job:6"].contains("\"pending\""), "asked again later");
    // What was left for after an answer: a template with no AWS account to remove it with; nothing known; a job gone.
    let (store2, config2) = (Mem::default(), Config::new(PLANE_ID.into()));
    let bare = ControlPlane { store: &store2, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config2, cache: &cache, containers: None };
    for what in ["template us-east-1 superci-abc123def456-7-1", "something else", "start job:404"] { block_on(bare.later(&format!("{PLANE_URL}/webhook"), what)).unwrap() }
    // A queued job's event delivered again while its machine is being asked for, a moment after: nothing more.
    put("job:7", serde_json::json!({ "job_id": 7, "label": "superci", "state": "launching", "at_ms": t - 10_000 }));
    let before = launches(&clouds).len();
    block_on(plane.handle(job_event("queued", 7, "acme/app", None)));
    assert_eq!((launches(&clouds).len(), said("job:7").0), (before, "launching".to_string()));
}

#[test]
fn containers_and_launchers_that_cannot_start_a_machine_say_why_and_a_gpu_sandbox_is_named_with_its_gpu() {
    // A Modal control plane's own sandbox for a GPU job: called by its size and GPU.
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.containers, config.own_cloud) = (true, "modal".into());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/gpu=t4")));
    assert!(job(&store, 1)["machine_type"].as_str().is_some_and(|t| t.ends_with("-t4")), "{}", job(&store, 1));
    // One whose runtime gave it no containers after all: said, for the job and for its failing runner.
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci")));
    assert!(job(&store, 2)["error"].as_str().is_some_and(|e| e.contains("no containers here")), "{}", job(&store, 2));
    block_on(plane.handle(labelled("queued", 3, "acme/app", None, "superci/os=macos")));
    assert_eq!(job(&store, 3)["state"], "failed");
    // Modal's launcher answering without an id, or refusing with what it said.
    let refusing = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| match r.url.as_str() {
        "https://modal.acme.example/launch" if refusing.get() => Some(Response::new(500, "application/json", r#"{"detail":"no GPUs without a payment method"}"#)),
        "https://modal.acme.example/launch" => Some(Response::new(200, "application/json", r#"{"kind":"sandbox"}"#)),
        _ => None,
    });
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://modal.acme.example".into() }];
    let (store, cache) = (Mem::default(), Cache::default());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 4, "acme/app", None, "superci")));
    assert!(job(&store, 4)["error"].as_str().is_some_and(|e| e.contains("launcher answered without an id")), "{}", job(&store, 4));
    refusing.set(true);
    block_on(plane.handle(labelled("queued", 5, "acme/app", None, "superci")));
    assert!(job(&store, 5)["error"].as_str().is_some_and(|e| e.contains("no GPUs without a payment method")), "{}", job(&store, 5));
}

#[test]
fn an_aws_machines_cost_is_settled_at_its_on_demand_price_and_left_an_estimate_when_aws_cannot_be_asked() {
    let t = 1_790_000_000_000u64;
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| {
        (r.url == "https://api.pricing.us-east-1.amazonaws.com/" && body.contains("instanceType")).then(|| {
            let product = serde_json::json!({ "terms": { "OnDemand": { "X": { "priceDimensions": { "Y": { "pricePerUnit": { "USD": "0.2000000000" } } } } } } }).to_string();
            Response::new(200, "application/x-amz-json-1.1", serde_json::json!({ "PriceList": [product] }).to_string())
        })
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let ran = |id: u64, j: serde_json::Value| {
        let mut base = serde_json::json!({ "job_id": id, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t - 600_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": true, "began": true,
            "machine_id": format!("i-{id}"), "machine_type": "m8a.xlarge", "region": "us-east-1", "on_demand": true, "launched_ms": t - 600_000, "runner": format!("superci-{PLANE_ID}-{id}") });
        for (k, v) in j.as_object().unwrap() { base[k] = v.clone() }
        store.0.borrow_mut().insert(format!("job:{id}"), base.to_string());
        store.0.borrow_mut().insert(format!("runner:superci-{PLANE_ID}-{id}"), id.to_string());
    };
    // An on-demand machine: its price from AWS's Price List.
    ran(1, serde_json::json!({}));
    block_on(plane.handle(job_event("completed", 1, "acme/app", Some(&format!("superci-{PLANE_ID}-1")))));
    assert_eq!(job(&store, 1)["cost_from"], "prices");
    // What it sent, counted a while after it ended: of a machine with no id, or once AWS is no longer connected.
    ran(2, serde_json::json!({ "state": "done", "ended_ms": t - 13 * 60_000, "cost_usd": 0.1, "cost_from": "prices" }));
    store.0.borrow_mut().entry("job:2".into()).and_modify(|v| { let mut j: serde_json::Value = serde_json::from_str(v).unwrap(); j.as_object_mut().unwrap().remove("machine_id"); *v = j.to_string() });
    block_on(plane.alarm()).unwrap();
    assert!(job(&store, 2)["sent_gb"].is_null());
    let (store2, config2) = (Mem::default(), { let mut c = Config::new(PLANE_ID.into()); c.app = Some(app()); c });
    let bare = ControlPlane { store: &store2, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config2, cache: &cache, containers: None };
    store2.0.borrow_mut().insert("job:3".into(), serde_json::json!({ "job_id": 3, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": t - 600_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": true,
        "machine_id": "i-3", "machine_type": "m8a.xlarge", "region": "us-east-1", "ended_ms": t - 13 * 60_000, "cost_from": "prices", "cost_usd": 0.1 }).to_string());
    block_on(bare.alarm()).unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&store2.0.borrow()["job:3"]).unwrap()["sent_gb"].is_null());
    // Ended while AWS is no longer connected: left an estimate.
    store2.0.borrow_mut().insert("job:4".into(), serde_json::json!({ "job_id": 4, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t - 600_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": true,
        "machine_id": "i-4", "machine_type": "m8a.xlarge", "region": "us-east-1", "runner": "r-4" }).to_string());
    store2.0.borrow_mut().insert("runner:r-4".into(), "4".into());
    block_on(bare.handle(job_event("completed", 4, "acme/app", Some("r-4"))));
    assert!(serde_json::from_str::<serde_json::Value>(&store2.0.borrow()["job:4"]).unwrap()["cost_from"].is_null());
}

#[test]
fn the_dashboards_requests_go_through_with_what_github_and_gitlab_say_and_whatever_they_refuse() {
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let failing = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| {
        let u = r.url.as_str();
        if failing.get() && (u.ends_with("/app/installations") || u.contains("/app/installations/")) { return Some(Response::new(500, "application/json", r#"{"message":"Server Error"}"#)) }
        if u.starts_with("https://gitlab.example/api/v4/projects?") { return Some(Response::new(200, "application/json", "[]")) }
        if u.contains("/hooks") { return Some(Response::new(200, "application/json", "[]")) }
        if r.method == "DELETE" && u.contains("/app/installations/") { return Some(Response::new(500, "application/json", r#"{"message":"Server Error"}"#)) }
        if u == "https://api.github.com/repos/acme/app/actions/jobs/7/logs" { return Some(Response::new(302, "text/plain", "").with_header("location", "https://logs.example/7")) }
        if u == "https://api.github.com/repos/acme/app/actions/runs/3/logs" { return Some(Response::new(302, "text/plain", "").with_header("location", "https://logs.example/run-3")) }
        if u == "https://moved.acme.example/webhook" { return Some(Response::new(202, "text/plain", "taken")) }
        None
    });
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let mut beta = app();
    (beta.id, beta.owner) = (100, "beta".into());
    config.more_apps = vec![beta];
    config.device_key_names = vec![("DEVICE_KEY_4102444800_abc".into(), "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let post = |path: &str, body: &str| run(key(Request::new("POST", &format!("{PLANE_URL}{path}")).with_body(body.to_string())));
    // The devices with keys; a project switched on by its id; the public repositories of every installation.
    assert_eq!(json(&run(key(get("/keys"))))["devices"][0]["name"], "DEVICE_KEY_4102444800_abc");
    post("/gitlab/projects", r#"{"id":380,"enabled":true}"#);
    assert!(run(key(get("/github/public-repos"))).status >= 200);
    // A job's whole log where GitHub keeps it, its own and its run's.
    store.0.borrow_mut().insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
    assert_eq!(json(&run(key(get("/job/log?id=7&link"))))["url"], "https://logs.example/7");
    assert_eq!(json(&run(key(get("/job/log?id=7&link=steps"))))["url"], "https://logs.example/run-3");
    // Moving here: a runner kept here already is not taken again.
    store.0.borrow_mut().insert("glrunner:5".into(), r#"{"id":5}"#.into());
    let moving = run(key(Request::new("POST", &format!("{PLANE_URL}/move/import")).with_header("x-move-token", "move-token-0123456789").with_body(r#"{"state":[["glrunner:5","{\"id\":5}"]]}"#)));
    assert_eq!(json(&moving)["taken"], 0);
    // Leaving with no GitLab runner kept: GitHub would not uninstall the App, said for each organization.
    store.0.borrow_mut().remove("glrunner:5");
    let left = json(&post("/leave", "{}"));
    assert_eq!((left["gitlab_runners"].as_u64(), left["github_failed"].as_array().map(|f| f.len())), (Some(0), Some(2)), "{left}");
    // An organization removed whose App GitHub no longer answers for.
    failing.set(true);
    assert!(json(&post("/github/uninstall", r#"{"app":100}"#))["error"].is_string());
    // Moved: a GitHub event that still arrives here goes on.
    store.0.borrow_mut().insert("moved_to".into(), "\"https://moved.acme.example\"".into());
    assert_eq!(run(webhook("queued", None)).body_text(), "taken");
}

/// The fake clouds with the control plane's bucket in Virginia: this version's `superci-cache` in it (so it is ready),
/// and what is put there kept, given back, and deleted.
fn with_bucket() -> With<impl Fn(&Request, &str) -> Option<Response>> {
    let objects: RefCell<BTreeMap<String, Vec<u8>>> = RefCell::default();
    With(FakeClouds::default(), move |r: &Request, _: &str| {
        let key = r.url.strip_prefix(BUCKET)?.split('?').next().unwrap_or_default().to_string();
        let mut objects = objects.borrow_mut();
        Some(match r.method.as_str() {
            "HEAD" if key == superci_core::cache::program_key("x86_64") => Response::new(200, "application/octet-stream", ""),
            "PUT" => { objects.insert(key, r.body.clone()); Response::new(200, "application/xml", "") }
            "GET" | "HEAD" => match objects.get(&key) { Some(b) => Response::new(200, "application/octet-stream", b.clone()), None => Response::new(404, "application/xml", "<Error><Code>NoSuchKey</Code></Error>") },
            "DELETE" => { objects.remove(&key); Response::new(204, "application/xml", "") }
            _ => return None,
        })
    })
}

#[test]
fn a_runs_working_tree_logs_and_pages_go_through_the_bucket_and_go_from_it_after_a_day() {
    let t = 1_790_000_000_000u64;
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clouds = with_bucket();
    let clock = MovingClock(std::cell::Cell::new(t));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    connect_role(&store);
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let tree = "tree-0123456789abcdef";
    let part = |q: &str| json(&run(key(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id={tree}&part=0&link{q}")))));
    // Its working tree: kept as a base; on a base not there, or there; a layer on bases, one of them not there.
    assert!(part("&keep=basename01").get("url").is_some_and(|u| u.is_string()));
    assert_eq!(part("&base=nothere01")["base"], false);
    // (The computer puts the base there.)
    block_on(clouds.send(Request::new("PUT", &format!("{BUCKET}bases/basename01.tar.gz")).with_body("base"))).unwrap();
    assert_eq!(part("&base=basename01")["parts"], 2);
    assert_eq!(part("&layer=layer0001&base=basename01,nothere01")["base"], false);
    assert_eq!(part("&layer=layer0001&base=basename01")["parts"], 2);
    // A run of one job on it; its runner fetches the tree from the bucket, sends a log there, and its machine sends a
    // page of a step as it goes.
    let made = run(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n", "path": ".github/workflows/ci.yml", "repository": "acme/app", "tree": tree, "parts": 2 }).to_string())));
    let id = json(&made)["run_id"].as_str().unwrap().to_string();
    let r: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{id}")]).unwrap();
    let (token, job_id) = (r["jobs"][0]["token"].as_str().unwrap().to_string(), r["jobs"][0]["job_id"].as_str().unwrap().to_string());
    assert_eq!(run(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/superci/tree?part=0"))).status, 302);
    store.0.borrow_mut().insert(format!("rtree-s3:{tree}:00001"), "us east-1|runs/x/00001".into());
    assert_eq!(run(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/superci/tree?part=1"))).status, 500);
    assert_eq!(json(&run(key(Request::new("POST", &format!("{PLANE_URL}/runs/tree?id={tree}&part=2")))))["stored"], 0);
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/runner/{token}/_apis/v1/Logfiles/p/1")).with_body("a whole log\n")).status, 200);
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/runner/{token}/_apis/v1/Logfiles/p/1")).with_body("and more\n")).status, 200);
    assert_eq!(run(key(get(&format!("/runs/{id}/log?job=0&log=1")))).body_text(), "a whole log\nand more\n");
    // (A piece kept damaged: said.)
    store.0.borrow_mut().insert(format!("rlog:{token}:0004:0000"), "!!".into());
    assert_eq!(run(key(get(&format!("/runs/{id}/log?job=0&log=4")))).status, 500);
    let j = job(&store, job_id.parse().unwrap());
    let (runner, hello) = (j["runner"].as_str().unwrap().to_string(), j["hello"].as_str().unwrap().to_string());
    let step = "0a1b2c3d-0000-0000-0000-000000000000";
    assert_eq!(run(Request::new("POST", &format!("{PLANE_URL}/hello?runner={runner}&t={hello}&live=tl_{step}_1.log&at=0")).with_body("2026-10-16T21:18:46.0000000Z a line\npart of")).status, 200);
    assert_eq!(run(key(get(&format!("/runs/{id}/log?job=0&step={step}")))).body_text(), "2026-10-16T21:18:46.0000000Z a line\n");
    // Its steps' whole logs, linked in the bucket.
    let steps = |s: serde_json::Value| { store.0.borrow_mut().entry(format!("rjob:{token}")).and_modify(|v| { let mut h: serde_json::Value = serde_json::from_str(v).unwrap(); h["steps"] = s; *v = h.to_string() }); };
    steps(serde_json::json!([{ "type": "Task", "log": 1, "order": 1, "state": "completed", "id": "s1" }, { "type": "Task", "log": 2, "order": 2, "state": "completed", "id": "s2" }]));
    assert_eq!(json(&run(key(get(&format!("/job/log?id={job_id}&link")))))["urls"].as_array().map(|u| u.len()), Some(2), "a step's log with no piece kept has no link");
    steps(serde_json::json!([{ "type": "Task", "log": 1, "order": 1, "state": "completed", "id": "s1" }, { "type": "Task", "order": 2, "state": "inProgress", "id": step }]));
    let read = json(&run(key(get(&format!("/job/log?id={job_id}&steps")))));
    assert!(read["steps"][0]["text"].as_str().is_some_and(|t| t.contains("a whole log")) && read["steps"][1]["text"].as_str().is_some_and(|t| t.contains("a line")), "{read}");
    // (A log kept in a region that makes no address: no link to it.)
    store.0.borrow_mut().insert(format!("rlog:{token}:0003:0000"), "s3:us east-1".into());
    steps(serde_json::json!([{ "type": "Task", "log": 3, "order": 1, "state": "completed", "id": "s3" }]));
    assert_eq!(run(key(get(&format!("/job/log?id={job_id}&link")))).status, 500);
    // A day on: what the run kept goes, from the bucket too.
    clock.0.set(t + 2 * 86_400_000);
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().keys().any(|k| k.starts_with(&format!("rlog:{token}")) || k.starts_with(&format!("rtree-s3:{tree}"))), "{:?}", store.0.borrow().keys().filter(|k| k.starts_with("r")).collect::<Vec<_>>());
}

#[test]
fn gitlab_events_for_a_control_plane_that_moved_or_claimed_by_another_request_are_left_alone() {
    let t = 1_790_000_000_000u64;
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| match r.url.as_str() {
        "https://moved.acme.example/gitlab/webhook" => Some(Response::new(200, "text/plain", "taken")),
        u if u.ends_with("/retry") => Some(Response::new(500, "application/json", r#"{"message":"500 Internal Server Error"}"#)),
        _ => None,
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let secret = "hook-secret-0123456789";
    // A job another request claimed as this one asked GitLab of it: known.
    store.meddling(|op, k, m| if op == "put_if_absent" && k == "job:gl1977" { m.insert(k.to_string(), "{}".into()); });
    assert_eq!(block_on(plane.handle(gitlab_event(1977, "pending", secret))).body_text(), "known");
    store.meddling(|_, _, _| {});
    // A job whose spot machine AWS took back, failed: GitLab refuses to run it again, said, and the note of it goes.
    store.0.borrow_mut().insert("glrunner:55".into(), serde_json::json!({ "id": 55, "job": 5, "gitlab": "" }).to_string());
    store.0.borrow_mut().insert("job:gl5".into(), serde_json::json!({ "job_id": 5, "run_id": 2366, "repo": "acme/app", "name": "test", "state": "running", "at_ms": t - 60_000, "installation_id": 0, "cloud": "aws",
        "seen_in_progress": true, "provider": "gitlab", "project_id": 380, "interrupted": true, "runner_id": 55 }).to_string());
    block_on(plane.handle(gitlab_event_on(5, "failed", secret, Some(55))));
    let five: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:gl5"]).unwrap();
    assert!(five["error"].as_str().is_some_and(|e| e.contains("not run again")), "{five}");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("rerun:")));
    // A runner kept damaged: passed over by the upkeep.
    store.0.borrow_mut().insert("glrunner:bad".into(), "not json".into());
    block_on(plane.alarm()).unwrap();
    assert!(store.0.borrow().contains_key("glrunner:bad"));
    // Moved: a GitLab event that still arrives goes on.
    store.0.borrow_mut().insert("moved_to".into(), "\"https://moved.acme.example\"".into());
    assert_eq!(block_on(plane.handle(gitlab_event(1978, "pending", secret))).body_text(), "taken");
}

#[test]
fn installations_runners_and_machines_with_nothing_or_nobody_left_are_said_as_that() {
    let t = 1_790_000_000_000u64;
    let empty = std::cell::Cell::new(false);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| match (r.method.as_str(), r.url.as_str()) {
        (_, u) if u.ends_with("/app/installations") && empty.get() => Some(Response::new(200, "application/json", "[]")),
        ("DELETE", u) if u.contains("/app/installations/") => Some(Response::new(204, "application/json", "")),
        (_, u) if u.starts_with("https://gitlab.example/api/v4/projects?") => Some(Response::new(200, "application/json", "[]")),
        (_, "https://api.github.com/repos/acme/app/actions/jobs/8") => Some(Response::new(200, "application/json", r#"{"status":"queued"}"#)),
        _ => None,
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    (config.aws_own, config.aws_own_creds) = own_aws();
    let mut beta = app();
    (beta.id, beta.owner) = (100, "beta".into());
    config.more_apps = vec![beta];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let post = |path: &str, body: &str| block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}{path}")).with_body(body.to_string()))));
    // An organization removed: its App uninstalled where it is installed.
    assert_eq!(json(&post("/github/uninstall", r#"{"app":100}"#))["github_installations"], 1);
    // A job of an App this control plane no longer has: no token for its log.
    store.0.borrow_mut().insert("job:9".into(), serde_json::json!({ "job_id": 9, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": t - 60_000, "installation_id": 42, "app_id": 12345, "cloud": "aws", "seen_in_progress": true }).to_string());
    assert_eq!(json(&block_on(plane.handle(key(get("/job/log?id=9&link")))))["error"], "GitHub gave no token for its repository");
    // A machine whose job GitHub still has queued, past its time: with no runner registered for it, it is not held
    // (only a ready runner is); a GitLab machine whose job ended unseen keeps its runner (none of GitHub's to take back).
    store.0.borrow_mut().insert("job:8".into(), serde_json::json!({ "job_id": 8, "run_id": 3, "repo": "acme/app", "state": "launched", "at_ms": t - 9 * 60_000, "launched_ms": t - 9 * 60_000, "installation_id": 42,
        "cloud": "aws", "machine_id": "i-8", "region": "us-east-1", "seen_in_progress": false, "label": "superci" }).to_string());
    store.0.borrow_mut().insert("job:gl10".into(), serde_json::json!({ "job_id": 10, "run_id": 3, "repo": "acme/app", "state": "launched", "at_ms": t - 30_000, "launched_ms": t - 30_000, "installation_id": 0,
        "cloud": "aws", "machine_id": "i-10", "region": "us-east-1", "seen_in_progress": false, "provider": "gitlab", "project_id": 380, "runner_id": 77 }).to_string());
    store.0.borrow_mut().insert("gone:gl10".into(), t.to_string());
    // (And one of GitHub's whose job ended unseen, with no runner registered to take back.)
    store.0.borrow_mut().insert("job:11".into(), serde_json::json!({ "job_id": 11, "run_id": 3, "repo": "acme/app", "state": "launched", "at_ms": t - 30_000, "launched_ms": t - 30_000, "installation_id": 42,
        "cloud": "aws", "machine_id": "i-11", "region": "us-east-1", "seen_in_progress": false, "label": "superci" }).to_string());
    store.0.borrow_mut().insert("gone:11".into(), t.to_string());
    block_on(plane.alarm()).unwrap();
    assert!(job(&store, 8)["held_until_ms"].is_null(), "{}", job(&store, 8));
    // Leaving: the App uninstalled where it is; then, installed nowhere.
    assert_eq!(json(&post("/leave", "{}"))["github_installations"], 2);
    empty.set(true);
    assert_eq!(json(&post("/leave", "{}"))["github_installations"], 0);
}

#[test]
fn a_runs_artifacts_and_its_tools_cache_refuse_what_cannot_be_kept_and_say_what_is_there() {
    let t = 1_790_000_000_000u64;
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let http = with_bucket();
    let mut config = Config::new(PLANE_ID.into());
    connect_role(&store);
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &http, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // The control plane's own key, as a run's job's token is signed with it; a run of one job, on a machine.
    let key = superci_core::crypto::SigningKeyStore::generate();
    store.0.borrow_mut().insert("identity".into(), serde_json::json!({ "signing": key }).to_string());
    store.0.borrow_mut().insert("run:41".into(), serde_json::json!({ "at_ms": t, "repository": "acme/app", "ref": "refs/heads/main", "cache": { "ns": "gh77", "default_branch": "main" }, "jobs": [{ "key": "a", "state": "started", "job_id": "9" }] }).to_string());
    store.0.borrow_mut().insert("job:9".into(), serde_json::json!({ "job_id": 9, "run_id": 41, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 0, "cloud": "aws", "seen_in_progress": true, "provider": "local", "name": "a" }).to_string());
    let (good, bad) = ("machine-cache-link-000000000good", "machine-cache-link-0000000000bad");
    for (link, region) in [(good, "us-east-1"), (bad, "us east-1")] {
        store.0.borrow_mut().insert(format!("cachem:{link}"), serde_json::json!({ "region": region, "arch": "x86_64", "until": u64::MAX / 4, "runner": "", "job": "job:9" }).to_string());
    }
    let token = |scp: bool| {
        let mut claims = superci_core::cache::local_claims("gh77", "computer", "refs/heads/main", "main", "41", "9");
        if !scp { claims.as_object_mut().unwrap().remove("scp"); }
        (claims["iss"], claims["aud"], claims["exp"]) = (PLANE_URL.into(), "superci-cache".into(), 4_102_444_800u64.into());
        key.jwt(&claims).unwrap()
    };
    let call = |link: &str, what: &str, mut body: serde_json::Value| { body["token"] = token(true).into(); let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{link}/{what}")).with_body(body.to_string()))); (r.status, r.body_text()) };
    // An artifact's name GitHub would not take; one begun twice; its pieces said amiss; one of no pieces taken out.
    assert_eq!(call(good, "artifact-begin", serde_json::json!({ "name": "a/b" })).0, 400);
    assert_eq!(call(good, "artifact-begin", serde_json::json!({ "name": "report" })).0, 200);
    assert_eq!(call(good, "artifact-begin", serde_json::json!({ "name": "report" })).0, 409);
    assert_eq!(call(good, "artifact-sign", serde_json::json!({ "name": "report", "names": ["p1"] })).0, 200);
    assert!(call(good, "artifact-finish", serde_json::json!({ "name": "report", "pieces": [["p0", "x"], [1, 2], ["p1", 3]] })).1.contains(r#""size":3"#));
    assert_eq!(call(good, "artifact-get", serde_json::json!({ "name": "report" })).0, 200);
    assert_eq!(call(good, "artifact-delete", serde_json::json!({ "name": "report" })).0, 200);
    store.0.borrow_mut().insert("artifact:41:empty".into(), serde_json::json!({ "id": "e", "region": "us-east-1", "made": t, "done": true }).to_string());
    assert_eq!(call(good, "artifact-delete", serde_json::json!({ "name": "empty" })).0, 200);
    // Begun by a token of the control plane's that says no job: kept by none.
    let r = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/cache/{good}/artifact-begin")).with_body(serde_json::json!({ "token": token(false), "name": "unkept" }).to_string())));
    assert_eq!(r.status, 200);
    // From a machine whose region makes no address: no link to write or read its pieces.
    assert_eq!(call(bad, "artifact-begin", serde_json::json!({ "name": "far" })).0, 200);
    assert_eq!(call(bad, "artifact-sign", serde_json::json!({ "name": "far", "names": ["p0"] })).0, 500);
    assert_eq!(call(bad, "artifact-finish", serde_json::json!({ "name": "far", "pieces": [["p0", 1]] })).0, 200);
    assert_eq!(call(bad, "artifact-get", serde_json::json!({ "name": "far" })).0, 500);
    assert_eq!(block_on(plane.handle(get("/runs/41/artifacts?name=far").with_header("authorization", "Bearer dashboard-session-key-1"))).status, 500);
    // A tool's artifact begun twice; its pieces said amiss (the job is a run's: its machine reads the run).
    assert_eq!(call(good, "tool-begin", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef" })).0, 200);
    assert_eq!(call(good, "tool-begin", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef" })).0, 409);
    assert_eq!(call(good, "tool-finish", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef", "pieces": [["p00000", 5], [1, 2], ["p00001", "x"]] })).0, 200);
    // The dashboard takes the repository's entries out (their pieces from the bucket too); a month on, what is left goes.
    assert_eq!(block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/caches/delete")).with_header("authorization", "Bearer dashboard-session-key-1").with_body(r#"{"repo":"gh77"}"#))).status, 200);
    store.0.borrow_mut().insert("cache:gh77:refs/heads/main:v:left".into(), serde_json::json!({ "id": "l", "region": "us-east-1", "made": 0, "done": false }).to_string());
    store.0.borrow_mut().insert("cache:gh77:refs/heads/main:v:old".into(), serde_json::json!({ "id": "o", "region": "us-east-1", "made": 0, "used": 0, "done": true, "pieces": [["p0", 1]] }).to_string());
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().contains_key("cache:gh77:refs/heads/main:v:old"));
    // The run gone: a tool on its machine reads nothing.
    store.0.borrow_mut().remove("run:41");
    assert_eq!(call(good, "tool-get", serde_json::json!({ "tool": "turbo", "hash": "0123456789abcdef" })).0, 403);
    // Its artifacts taken out at the next sweep; then one of no pieces alone, with nothing to delete from the bucket.
    store.0.borrow_mut().insert("cache-swept".into(), "0".into());
    block_on(plane.alarm()).unwrap();
    store.0.borrow_mut().insert("artifact:41:bare".into(), serde_json::json!({ "id": "b", "region": "us-east-1", "made": t, "done": true }).to_string());
    store.0.borrow_mut().insert("cache-swept".into(), "0".into());
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().contains_key("artifact:41:bare"));
    assert_eq!(store.0.borrow().get("cache-sweep-error"), None);
}

/// What a runner sends for a run's job, as a real one sent it (`fixtures/runner`): up to `upto` of its requests (all:
/// it takes the job and ends it), its ending said as `result`, or cut short.
fn runner_says(plane: &ControlPlane, token: &str, upto: usize, result: &str) {
    for line in include_str!("fixtures/runner/runner-requests.jsonl").lines().take(upto) {
        let r: serde_json::Value = serde_json::from_str(line).unwrap();
        let path = r["path"].as_str().unwrap().trim_start_matches("/runner/server");
        let mut req = Request::new(r["method"].as_str().unwrap(), &format!("{PLANE_URL}/runner/{token}{path}"));
        req.body = r["request"].as_str().unwrap().replace("\"result\":\"succeeded\"", &format!("\"result\":\"{result}\"")).into_bytes();
        if result == "cut" && path.contains("/FinishJob/") { req.body = br#"{"requestId":4,"result":"failed","outp"#.to_vec() }
        block_on(plane.handle(req));
    }
}

#[test]
fn a_runs_jobs_end_as_their_runners_say_or_when_they_run_out_of_time_or_are_cancelled() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(t));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    let new_run = |workflow: &str| -> (String, String) {
        let r = block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string()))));
        let id = json(&r)["run_id"].as_str().unwrap().to_string();
        let token = serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("run:{id}")]).unwrap()["jobs"][0]["token"].as_str().unwrap().to_string();
        (id, token)
    };
    let result = |id: &str| serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("run:{id}")]).unwrap()["jobs"][0]["result"].as_str().unwrap_or_default().to_string();
    let one = "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n";
    // Skipped, failed, cancelled, as its runner says; failed, said in a body cut short.
    for (said, want) in [("skipped", "skipped"), ("failed", "failure"), ("canceled", "cancelled"), ("cut", "failure")] {
        let (id, token) = new_run(one);
        runner_says(&plane, &token, usize::MAX, said);
        assert_eq!(result(&id), want, "{said}");
    }
    // Past its timeout-minutes: its machine ended, and it fails.
    let (id, token) = new_run("jobs:\n  a:\n    runs-on: superci\n    timeout-minutes: 1\n    steps:\n      - run: echo\n");
    runner_says(&plane, &token, 8, "succeeded");
    clock.0.set(clock.0.get() + 2 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(result(&id), "failure");
    // Cancelled while its runner has it, and the runner never stops: its machine ended after the grace time.
    let (id, token) = new_run(one);
    runner_says(&plane, &token, 8, "succeeded");
    assert_eq!(json(&block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/runs/{id}/cancel"))))))["cancelled"], true);
    // (Its runner, asking for its next message, is told to stop.)
    let told = block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/_apis/v1/Message/p?sessionId=s"))));
    assert!(told.body_text().contains("JobCancellation"), "{}", told.body_text());
    clock.0.set(clock.0.get() + 10 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!(result(&id), "cancelled");
    // A run whose record is damaged as its job ends: said where the run's errors are kept.
    let (id, token) = new_run(one);
    runner_says(&plane, &token, 8, "succeeded");
    store.0.borrow_mut().insert(format!("run:{id}"), "not a run".into());
    runner_says(&plane, &token, usize::MAX, "succeeded");
    assert!(store.0.borrow().contains_key(&format!("run-error:{id}")));
}

#[test]
fn a_role_aws_will_not_let_this_control_plane_take_leaves_what_needs_it_undone_or_estimated() {
    let t = 1_790_000_000_000u64;
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| r.url.starts_with("https://sts.").then(|| Response::new(403, "text/xml", "<ErrorResponse><Error><Code>AccessDenied</Code><Message>Not authorized to perform sts:AssumeRoleWithWebIdentity</Message></Error></ErrorResponse>")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": role_arn("123456789012", PLANE_ID), "connected": true }).to_string());
    // A machine that ran a job: its cost left as estimated, and what it sent not counted.
    store.0.borrow_mut().insert("job:1".into(), serde_json::json!({ "job_id": 1, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t - 600_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": true,
        "machine_id": "i-1", "machine_type": "m8a.xlarge", "region": "us-east-1", "runner": "r-1" }).to_string());
    store.0.borrow_mut().insert("runner:r-1".into(), "1".into());
    block_on(plane.handle(job_event("completed", 1, "acme/app", Some("r-1"))));
    assert!(job(&store, 1)["cost_from"].is_null());
    store.0.borrow_mut().insert("job:2".into(), serde_json::json!({ "job_id": 2, "run_id": 3, "repo": "acme/app", "state": "done", "at_ms": t - 600_000, "installation_id": 42, "cloud": "aws", "seen_in_progress": true,
        "machine_id": "i-2", "machine_type": "m8a.xlarge", "region": "us-east-1", "ended_ms": t - 13 * 60_000, "cost_from": "prices", "cost_usd": 0.1 }).to_string());
    block_on(plane.alarm()).unwrap();
    assert!(job(&store, 2)["sent_gb"].is_null());
    // A launch template left for after an answer: not removed, said.
    assert!(block_on(plane.later(&format!("{PLANE_URL}/webhook"), "template us-east-1 superci-abc123def456-7-1")).is_err());
    // A run's job's log and pages in the bucket, its working tree there, its artifact: none of it read or linked.
    {
        let mut m = store.0.borrow_mut();
        m.insert("run:41".into(), serde_json::json!({ "at_ms": t, "jobs": [{ "key": "a", "state": "started", "job_id": "9", "token": "tok-0123456789abcdef" }] }).to_string());
        m.insert("job:9".into(), serde_json::json!({ "job_id": 9, "run_id": 41, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 0, "cloud": "aws", "seen_in_progress": true, "provider": "local", "local": "tok-0123456789abcdef" }).to_string());
        m.insert("rjob:tok-0123456789abcdef".into(), serde_json::json!({ "message": {}, "job_id": 9, "at_ms": t, "tree": "tree-0123456789abcdef", "steps": [{ "type": "Task", "id": "s1", "order": 1, "log": 1, "state": "completed" }] }).to_string());
        m.insert("rlog:tok-0123456789abcdef:0001:0000".into(), "s3:us-east-1".into());
        m.insert("rtree-s3:tree-0123456789abcdef:00000".into(), "us-east-1|runs/x/00000".into());
        m.insert("glive:7:tl_a_1.log:0000000000000".into(), "s3:us-east-1:5".into());
        m.insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
        m.insert("artifact:41:out".into(), serde_json::json!({ "id": "e", "region": "us-east-1", "made": t, "done": true, "pieces": [["p0", 1]] }).to_string());
        m.insert("cachem:machine-cache-link-000000000good".into(), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": "", "job": "job:9" }).to_string());
        m.insert("cache:gh77:refs/heads/main:v:k".into(), serde_json::json!({ "id": "c", "region": "us-east-1", "made": t, "done": true, "pieces": [["p0", 1]] }).to_string());
    }
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    for path in ["/runs/41/log?job=0&log=1", "/job/log?id=9&link", "/runs/41/artifacts?name=out"] {
        assert_eq!(block_on(plane.handle(key(get(path)))).status, 500, "{path}");
    }
    // (A GitHub job's pages its machine sent: left out of its log, which is read from GitHub.)
    assert_eq!(block_on(plane.handle(key(get("/job/log?id=7")))).status, 200);
    assert_eq!(block_on(plane.handle(get("/runner/tok-0123456789abcdef/superci/tree?part=0"))).status, 500);
    for path in ["/cache/machine-cache-link-000000000good/program", "/cache/machine-cache-link-000000000good/layers.json"] { assert_eq!(block_on(plane.handle(get(path))).status, 500, "{path}") }
    assert_eq!(block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/caches/delete")).with_body(r#"{"repo":"gh77"}"#)))).status, 500);
    // A job nothing here can run: its failing runner not started in AWS either.
    block_on(plane.handle(labelled("queued", 3, "acme/app", None, "superci/os=macos")));
    assert_eq!(job(&store, 3)["state"], "failed");
}

#[test]
fn a_gpu_job_refused_spot_for_the_accounts_limit_with_on_demand_turned_off_says_which_limit_to_raise() {
    let (store, clouds, wakes, cache) = (Mem::default(), Kinds { clouds: FakeClouds::default(), no_gpus: true }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { regions: vec![], off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None, function: false }, Pool { regions: vec![], off: true, cloud: superci_core::plane::AWS_ON_DEMAND.into(), max_jobs: None, monthly_usd: None, function: false }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/gpu=l4")));
    let e = job(&store, 1)["error"].as_str().unwrap_or_default().to_string();
    assert!(e.contains("L-3819A6DF") && !e.contains("L-DB2E81BA"), "{e}");
}

#[test]
fn logs_and_trees_kept_in_a_bucket_this_control_plane_can_no_longer_reach_are_passed_over_or_said() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let clock = MovingClock(std::cell::Cell::new(t));
    let mut config = Config::new(PLANE_ID.into());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    // A run's job: a step's log in the bucket (no AWS account connected now) and one in the store; a step still going.
    let step = "0a1b2c3d-0000-0000-0000-000000000000";
    {
        let mut m = store.0.borrow_mut();
        m.insert("run:41".into(), serde_json::json!({ "at_ms": t, "jobs": [{ "key": "a", "state": "started", "job_id": "9", "token": "tok-0123456789abcdef" }] }).to_string());
        m.insert("job:9".into(), serde_json::json!({ "job_id": 9, "run_id": 41, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 0, "cloud": "aws", "seen_in_progress": true, "provider": "local", "local": "tok-0123456789abcdef" }).to_string());
        m.insert("rjob:tok-0123456789abcdef".into(), serde_json::json!({ "message": {}, "job_id": 9, "at_ms": t, "steps": [{ "type": "Task", "id": step, "name": "Going", "order": 1, "state": "inProgress" }, { "type": "Task", "id": "s2", "name": "Done", "order": 2, "log": 2, "state": "completed" }] }).to_string());
        m.insert("rlog:tok-0123456789abcdef:0002:0000".into(), "s3:us-east-1".into());
        m.insert("rlog:tok-0123456789abcdef:0003:0000".into(), "s3:us east-1".into());
        // A page its machine sent of the step going, partly: no whole line yet; one in the bucket.
        m.insert(format!("glive:9:tl_{step}_1.log:0000000000000:000"), STANDARD.encode("2026-10-16T21:18:46.0000000Z no end"));
        m.insert("glive:9:tl_job_1.log:0000000000000".into(), "s3:us-east-1:5".into());
    }
    assert_eq!(block_on(plane.handle(key(get("/runs/41/log?job=0&log=2")))).body.len(), 0, "in a bucket it cannot read: nothing");
    let steps = json(&block_on(plane.handle(key(get("/job/log?id=9&steps")))));
    assert_eq!(steps["steps"].as_array().map(|s| s.len()), Some(2), "{steps}");
    assert_eq!(json(&block_on(plane.handle(key(get("/job/log?id=9&link")))))["urls"], serde_json::Value::Null);
    // Its steps not said yet: its log is the lines its runner sent.
    store.0.borrow_mut().entry("rjob:tok-0123456789abcdef".into()).and_modify(|v| { let mut h: serde_json::Value = serde_json::from_str(v).unwrap(); h["steps"] = serde_json::json!([]); *v = h.to_string() });
    store.0.borrow_mut().insert("rline:tok-0123456789abcdef:1790000000000001".into(), serde_json::json!({ "step": "s", "lines": ["sent as it went"] }).to_string());
    assert!(json(&block_on(plane.handle(key(get("/job/log?id=9")))))["log"].as_str().is_some_and(|l| l.contains("sent as it went")));
    // A tree kept in the bucket, in the old way (its region alone), gone after a day though AWS cannot be asked.
    store.0.borrow_mut().insert("rtree-s3:tree-0123456789abcdef:00000".into(), "us-east-1".into());
    store.0.borrow_mut().insert("rtree-at:tree-0123456789abcdef".into(), t.to_string());
    clock.0.set(t + 2 * 86_400_000);
    block_on(plane.alarm()).unwrap();
    assert!(!store.0.borrow().contains_key("rtree-s3:tree-0123456789abcdef:00000"));
}

#[test]
fn a_github_jobs_pages_without_whole_lines_or_times_that_are_no_times_are_read_as_they_are() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    store.0.borrow_mut().insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
    // Two pages: one with lines whose times are no times (the same line in both), one with no whole line.
    let odd = "2026-1X-16T21:18:46.0000000Z odd time\n";
    store.0.borrow_mut().insert("glive:7:tl_a_1.log:0000000000000:000".into(), STANDARD.encode(odd));
    store.0.borrow_mut().insert("glive:7:tl_b_1.log:0000000000000:000".into(), STANDARD.encode(format!("{odd}{odd}")));
    store.0.borrow_mut().insert("glive:7:tl_c_1.log:0000000000000:000".into(), STANDARD.encode("2026-10-16T21:18:46.0000000Z cut"));
    let log = json(&block_on(plane.handle(get("/job/log?id=7").with_header("authorization", "Bearer dashboard-session-key-1"))));
    assert!(log["log"].as_str().is_some_and(|l| l.contains("odd time") && !l.contains(" cut")), "{log}");
}

#[test]
fn a_job_claimed_as_the_store_fails_to_index_it_is_claimed_again_and_gets_its_machine() {
    // GitLab's job: its event answered with the store's failure; the look for jobs GitLab did not tell of finds it.
    let hook = format!("{PLANE_URL}/gitlab/webhook");
    let clouds = With(FakeClouds::default(), move |r: &Request, _: &str| {
        let (u, json) = (r.url.as_str(), |v: serde_json::Value| Some(Response::new(200, "application/json", v.to_string())));
        if u.starts_with("https://gitlab.example/api/v4/projects?membership=true") { return json(serde_json::json!([{ "id": 380, "path_with_namespace": "acme/app" }])) }
        if u == "https://gitlab.example/api/v4/projects/380/hooks" { return json(serde_json::json!([{ "id": 1, "url": hook }])) }
        if u.starts_with("https://gitlab.example/api/v4/projects/380/jobs?scope[]=pending") { return json(serde_json::json!([{ "id": 1980, "name": "test", "created_at": "2026-09-21T00:00:00Z", "pipeline": { "id": 9 } }])) }
        None
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.gitlab = Some(superci_core::gitlab::GitLab { url: "https://gitlab.example".into(), token: "glpat-test".into(), hook_secret: "hook-secret-0123456789".into(), id: String::new() });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    // (Its address known, as after any request: its projects' webhooks are told apart by it.)
    block_on(plane.handle(get("/health")));
    block_on(plane.alarm()).unwrap();
    store.0.borrow_mut().remove("gitlab:untold");
    store.0.borrow_mut().retain(|k, _| !k.contains("gl1980"));
    store.refusing("put", "live:", 0);
    assert_eq!(block_on(plane.handle(gitlab_event(1980, "pending", "hook-secret-0123456789"))).status, 500);
    block_on(plane.alarm()).unwrap();
    let j: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:gl1980"]).unwrap();
    assert_eq!(j["state"], "launched", "{j}");

    // A run of three jobs, the second's record not written: it, and the third reserved with it, started at the next move on.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.alarm()).unwrap();
    // (The first job's record written; then the second's not.)
    store.refusing("put", "job:", 1);
    let three = "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n  b:\n    runs-on: superci\n    steps:\n      - run: echo\n  c:\n    runs-on: superci\n    steps:\n      - run: echo\n";
    let made = block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": three, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string())));
    let id = json(&made)["run_id"].as_str().unwrap().to_string();
    let states = || serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("run:{id}")]).unwrap()["jobs"].as_array().unwrap().iter().map(|j| j["state"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(states(), ["started", "pending", "pending"]);
    block_on(plane.alarm()).unwrap();
    let run: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("run:{id}")]).unwrap();
    for j in run["jobs"].as_array().unwrap() { assert_eq!(job(&store, j["job_id"].as_str().unwrap().parse().unwrap())["state"], "launched", "{run}") }
}

#[test]
fn a_runs_cancelling_passes_over_what_its_record_lacks_and_a_runner_that_ends_before_it_takes_its_job_is_heard() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let key = |r: Request| r.with_header("authorization", "Bearer dashboard-session-key-1");
    // Started jobs: one kept without its link; one whose handed job is gone; one never taken whose record is gone; one over.
    store.0.borrow_mut().insert("run:5".into(), serde_json::json!({ "at_ms": t, "workflow": "jobs:\n  a:\n    runs-on: superci\n", "jobs": [
        { "key": "a", "state": "started" }, { "key": "a", "state": "started", "token": "tok-gone-000000000001", "job_id": "11" },
        { "key": "a", "state": "started", "token": "tok-here-000000000002", "job_id": "12" }, { "key": "a", "state": "done", "result": "success" }] }).to_string());
    store.0.borrow_mut().insert("rjob:tok-here-000000000002".into(), serde_json::json!({ "message": {}, "job_id": 12, "at_ms": t }).to_string());
    assert_eq!(json(&block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/runs/5/cancel"))))))["cancelled"], true);
    assert!(store.0.borrow()["rjob:tok-here-000000000002"].contains("cancelled"));
    // A GitHub run, cancelled at GitHub (with the App's token for its repository).
    store.0.borrow_mut().insert("job:7".into(), serde_json::json!({ "job_id": 7, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": t, "installation_id": 42, "cloud": "aws", "seen_in_progress": true }).to_string());
    assert!(block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/runs/3/cancel"))))).status < 500);
    // A run's job whose runner says it ended before it took the job, then takes it: begun, and over as it said.
    let made = block_on(plane.handle(key(Request::new("POST", &format!("{PLANE_URL}/runs")).with_body(serde_json::json!({ "workflow": "jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n", "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string()))));
    let id = json(&made)["run_id"].as_str().unwrap().to_string();
    let token = serde_json::from_str::<serde_json::Value>(&store.0.borrow()[&format!("run:{id}")]).unwrap()["jobs"][0]["token"].as_str().unwrap().to_string();
    let finish = include_str!("fixtures/runner/runner-requests.jsonl").lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).find(|r| r["path"].as_str().unwrap().contains("/FinishJob/")).unwrap();
    let mut req = Request::new("POST", &format!("{PLANE_URL}/runner/{token}{}", finish["path"].as_str().unwrap()));
    req.body = finish["request"].as_str().unwrap().as_bytes().to_vec();
    block_on(plane.handle(req));
    block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/_apis/v1/Message/p?sessionId=s"))));
    let handed: serde_json::Value = serde_json::from_str(&store.0.borrow()[&format!("rjob:{token}")]).unwrap();
    assert!(handed["noted_end"] == true && handed["noted_taken"] == true, "{handed}");
    // Its working tree: none sent with the run; one still on its way from the computer.
    assert_eq!(block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/superci/tree?part=0")))).status, 404);
    store.0.borrow_mut().entry(format!("rjob:{token}")).and_modify(|v| { let mut h: serde_json::Value = serde_json::from_str(v).unwrap(); h["tree"] = "tree-0123456789abcdef".into(); *v = h.to_string() });
    store.0.borrow_mut().insert("rtree-later:tree-0123456789abcdef".into(), t.to_string());
    assert_eq!(block_on(plane.handle(Request::new("GET", &format!("{PLANE_URL}/runner/{token}/superci/tree?part=0")))).status, 425);
}

#[test]
fn machines_whose_jobs_went_elsewhere_hand_themselves_on_or_end_as_github_and_gitlab_say() {
    let t = 1_790_000_000_000u64;
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| match r.url.as_str() {
        "https://api.github.com/repos/acme/app/actions/jobs/21" => Some(Response::new(200, "application/json", r#"{"status":"completed"}"#)),
        _ => None,
    });
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    odd_plane(&mut config);
    (config.aws_own, config.aws_own_creds) = own_aws();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |key: &str, j: serde_json::Value| {
        let mut base = serde_json::json!({ "run_id": 3, "repo": "acme/app", "state": "launched", "at_ms": t - 9 * 60_000, "launched_ms": t - 9 * 60_000, "installation_id": 42, "cloud": "aws", "machine_id": "i-1", "region": "us-east-1", "seen_in_progress": false, "label": "superci" });
        for (k, v) in j.as_object().unwrap() { base[k] = v.clone() }
        store.0.borrow_mut().insert(key.into(), base.to_string());
    };
    // A GitLab machine past its time whose job GitLab still has pending: ended, and the job tried again.
    put("job:gl1977", serde_json::json!({ "job_id": 1977, "provider": "gitlab", "project_id": 380, "installation_id": 0, "runner_id": 55 }));
    // Failing runners' machines (their jobs over), with jobs of the same runners waiting: for one with no token to ask
    // GitHub with, none is handed on; for one with, the waiting job GitHub says is over, its record gone meanwhile.
    put("job:20", serde_json::json!({ "job_id": 20, "failed_fast": true, "installation_id": 999 }));
    put("job:22", serde_json::json!({ "job_id": 22, "state": "running", "installation_id": 999, "launched_ms": t }));
    put("job:23", serde_json::json!({ "job_id": 23, "failed_fast": true }));
    put("job:21", serde_json::json!({ "job_id": 21, "state": "running", "launched_ms": t }));
    store.meddling(|op, k, m| if op == "get" && k == "job:21" { m.remove(k); });
    block_on(plane.alarm()).unwrap();
    let gl: serde_json::Value = serde_json::from_str(&store.0.borrow()["job:gl1977"]).unwrap();
    assert_ne!(gl["machine_id"], "i-1", "{gl}");
    assert_eq!(job(&store, 20)["state"], "swept");
}

#[test]
fn launchers_and_failing_runners_start_where_they_can_and_say_what_they_could_not() {
    // A failing runner started by Cloudflare's launcher; a job on Modal's, priced as Modal prices it.
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| match r.url.as_str() {
        "https://modal.acme.example/launch" => Some(Response::new(200, "application/json", r#"{"id":"sb-1","kind":"sandbox"}"#)),
        "https://broken.acme.example/launch" => Some(Response::new(200, "application/json", "not json")),
        _ => None,
    });
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci/os=macos")));
    assert_eq!(job(&store, 1)["failed_fast"], true, "{}", job(&store, 1));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "modal".into(), url: "https://modal.acme.example".into() }];
    let (store, cache) = (Mem::default(), Cache::default());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci")));
    assert!(job(&store, 2)["usd_per_hour"].as_f64().is_some_and(|p| p > 0.0) && job(&store, 2)["machine_id"] == "sb-1", "{}", job(&store, 2));
    // A launcher whose answer is no JSON; a job cancelled before its start was asked for.
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.launchers = vec![Launcher { cloud: "cloudflare".into(), url: "https://broken.acme.example".into() }];
    let (store, cache) = (Mem::default(), Cache::default());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 3, "acme/app", None, "superci")));
    assert!(job(&store, 3)["error"].as_str().is_some_and(|e| e.contains("launcher")), "{}", job(&store, 3));
    store.0.borrow_mut().insert("gone:4".into(), "1790000000000".into());
    block_on(plane.handle(labelled("queued", 4, "acme/app", None, "superci")));
    assert_eq!(job(&store, 4)["state"], "cancelled");
    // A failing runner whose configuration from GitHub is no runner's: not started in AWS.
    let clouds = With(FakeClouds::default(), |r: &Request, body: &str| (r.url.ends_with("/generate-jitconfig") && body.contains("-x\"")).then(|| Response::new(201, "application/json", r#"{"encoded_jit_config":"not a config!","runner":{"id":9}}"#)));
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    let (store, cache) = (Mem::default(), Cache::default());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 5, "acme/app", None, "superci/os=macos")));
    assert!(job(&store, 5)["fail_pending"].is_string(), "{}", job(&store, 5));
}

#[test]
fn a_runs_repository_environment_and_signing_key_are_read_as_they_are_or_said() {
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (r.url == "https://api.github.com/repos/acme/app").then(|| Response::new(200, "application/json", r#"{"default_branch":"main"}"#)));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.device_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let new_run = |workflow: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/runs")).with_header("authorization", "Bearer dashboard-session-key-1")
        .with_body(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string())));
    // A repository GitHub says no id of: its cache is the run's own; an environment named as it is, with no secrets kept.
    let r = new_run("jobs:\n  a:\n    runs-on: superci\n    environment: production\n    steps:\n      - run: echo\n");
    assert_eq!(json(&r)["cache_own"], true, "{}", r.body_text());
    // The control plane's signing key damaged: no token for a job's cache, so the job does not start.
    store.0.borrow_mut().insert("identity".into(), serde_json::json!({ "signing": { "private_b64": "!!", "kid": "k" } }).to_string());
    assert_eq!(new_run("jobs:\n  a:\n    runs-on: superci\n    steps:\n      - run: echo\n").status, 400);
}

#[test]
fn a_failing_runner_that_takes_a_job_waiting_for_room_fails_it_and_its_own_job_gets_a_failing_runner_again() {
    let t = 1_790_000_000_000u64;
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let failing = format!("superci-{PLANE_ID}-1-x");
    store.0.borrow_mut().insert("job:1".into(), serde_json::json!({ "job_id": 1, "run_id": 3, "repo": "acme/app", "state": "failed", "at_ms": t, "installation_id": 42, "cloud": "", "seen_in_progress": false, "failed_fast": true, "error": "nowhere to run it" }).to_string());
    store.0.borrow_mut().insert(format!("runner:{failing}"), "1".into());
    store.0.borrow_mut().insert("job:2".into(), serde_json::json!({ "job_id": 2, "run_id": 3, "repo": "acme/app", "state": "waiting", "at_ms": t, "installation_id": 42, "cloud": "", "seen_in_progress": false }).to_string());
    block_on(plane.handle(job_event("completed", 2, "acme/app", Some(&failing))));
    assert_eq!((job(&store, 2)["state"].as_str(), job(&store, 1)["fail_pending"].as_str()), (Some("failed"), Some("nowhere to run it")));
}

/// A test of this file run under the sweep: its name, and itself.
type Swept = (&'static str, fn());
macro_rules! tests { ($($t:ident),* $(,)?) => { &[$((stringify!($t), $t as fn())),*] } }

/// Every other test of this file, as written, run again for each call to a fake cloud and each store operation it
/// made, that one failing (a call answered with its service's error, or dropped; a store operation, or a wake asked
/// of the timer, not answered), then with every call from each on failing, every store operation from each on, and
/// every swap from each on lost. A test's own checks may not hold then (that is what the fault is for), and it stops
/// where the first does not; what must hold is the control plane: whatever fails, it never panics. (The clean runs
/// alone unless `SUPERCI_SWEEP_FAULTS=1`, as the sweeps of the CLI's flows.)
#[test]
fn every_test_here_holds_with_each_call_and_store_operation_failing() {
    let tests: &[Swept] = tests![
        online_only_public_documents_and_a_yes_no_health,
        aws_connects_once_from_the_stack_then_jobs_run_on_spot_machines,
        routing_set_in_the_control_plane_sends_jobs_to_a_cloudflare_agent_with_a_token_for_it_alone,
        a_control_plane_in_aws_uses_its_own_credentials_for_machines,
        machines_start_in_the_control_planes_own_network_cheapest_zone_first_else_the_default_one,
        a_region_with_no_spot_capacity_left_sends_the_machine_to_the_next,
        moving_hands_everything_to_the_new_control_plane_once_and_the_old_one_says_where,
        a_network_or_an_image_aws_says_is_gone_is_looked_up_anew_for_the_next_job,
        a_cloudflare_control_plane_starts_containers_itself,
        a_machine_that_never_begins_its_job_is_tried_again_then_the_job_fails,
        a_machine_whose_runner_waits_while_github_keeps_its_job_queued_is_ended_and_the_job_held_then_given_one_again,
        a_job_whose_machine_never_starts_while_github_cannot_be_asked_fails_here_alone,
        a_start_cut_off_midway_is_placed_again_by_the_sweep,
        a_runner_that_takes_a_job_whose_machine_failed_leaves_its_own_job_a_new_machine,
        how_a_job_itself_ended_is_kept_apart_from_whether_a_machine_ran_it,
        an_app_renamed_on_github_is_known_by_its_new_address,
        a_machine_type_the_region_does_not_have_is_not_asked_for_with_every_job,
        a_machine_refused_by_the_accounts_own_budget_is_said_as_that_and_is_no_missing_permission,
        an_organization_that_allows_no_spot_sends_the_job_on_demand_and_is_no_missing_permission,
        gpus_and_windows_get_their_own_images_types_and_prices,
        a_gpu_on_modal_goes_to_its_sandbox_and_into_the_price,
        an_image_of_the_control_planes_own_goes_to_modals_sandbox_by_its_name_and_elsewhere_is_refused,
        a_second_modal_workspace_takes_the_jobs_once_the_first_reaches_its_limit,
        an_image_of_ones_own_goes_to_a_further_modal_workspaces_launcher_too,
        an_aws_account_with_no_gpu_quota_fails_gpu_jobs_at_once_saying_what_to_raise,
        the_label_asks_for_the_machine_and_the_order_finds_a_place_that_can_run_it,
        limits_make_jobs_wait_or_move_down_the_order,
        gitlab_jobs_with_the_label_in_their_tags_get_a_machine_and_a_project_runner_of_their_own,
        a_failing_runner_that_would_not_start_is_started_by_the_next_sweep,
        a_machine_that_fails_to_start_is_tried_again_and_a_job_with_no_place_fails_at_once,
        public_repositories_run_only_when_allowed_and_never_from_forks,
        machines_larger_than_allowed_are_refused_and_on_demand_counts_toward_budgets,
        a_moved_control_plane_forwards_only_signed_events,
        measured_costs_from_the_dashboard_settle_finished_cloudflare_jobs_only,
        jobs_nothing_here_can_run_fail_at_once,
        removed_aws_runners_take_no_jobs,
        without_its_own_network_a_machine_still_tries_each_zone_and_a_full_region_is_not_called_a_quota,
        a_spot_machine_taken_back_fails_its_job_at_once_and_the_job_is_run_again_on_demand,
        an_app_that_may_not_run_jobs_again_says_so,
        cloudflare_runners_get_githubs_full_image_by_its_address_when_set,
        one_control_plane_serves_several_organizations_each_through_its_own_app,
        with_no_room_for_a_spot_machine_in_any_region_the_job_gets_an_on_demand_one,
        a_job_that_passed_as_the_notice_came_is_not_run_again_and_a_notice_follows_the_machine_to_the_job_it_took,
        several_jobs_of_one_run_taken_back_are_run_again_with_one_request,
        a_failing_runner_that_takes_another_job_is_said_on_it_and_that_jobs_machine_takes_the_first,
        a_gitlab_job_whose_spot_machine_is_taken_back_is_run_again_on_demand,
        a_network_of_the_accounts_own_takes_the_machines_and_nothing_else_does_when_it_is_wrong,
        the_sweep_counts_a_machines_age_from_its_start_and_starts_none_again_for_a_job_github_no_longer_has_queued,
        a_github_enterprise_server_is_asked_at_its_own_address,
        a_job_whose_runner_was_taken_gets_a_machine_even_if_the_other_jobs_never_comes_up,
        out_of_time_while_spot_is_refused_the_next_try_starts_below_spot,
        several_gitlabs_are_kept_apart_each_with_its_own_token_and_secret,
        with_no_spot_machine_the_job_goes_to_the_next_place_in_the_order,
        each_place_of_awss_starts_its_machines_in_its_own_regions,
        with_on_demand_turned_off_and_nothing_else_in_the_order_the_job_waits_for_a_spot_machine,
        a_provider_that_cannot_start_the_machine_sends_the_job_to_the_next_in_the_order,
        a_provider_that_refuses_what_a_job_asks_is_passed_by_and_only_all_refusing_fails_it_at_once,
        no_gpu_allowance_is_said_at_once_even_when_spot_refuses_for_another_reason,
        a_key_that_only_reads_sees_the_status_and_a_jobs_log_and_changes_nothing,
        a_job_github_never_told_of_is_found_by_asking_and_gets_its_machine,
        a_control_plane_answers_to_the_labels_set_for_it_the_new_one_and_the_ones_before,
        a_control_plane_is_called_what_it_is_named_where_people_read_it,
        a_spot_request_refused_for_its_rate_to_the_end_goes_on_demand_and_says_why,
        a_launch_aws_refuses_for_its_request_rate_is_asked_again_in_place,
        a_job_that_began_on_anothers_runner_gets_no_machine_again_and_an_ended_machine_stays_in_the_books,
        a_machine_nobody_waits_for_is_ended_at_the_first_sweep_after_it_had_time_to_come_up,
        an_aws_machines_disk_is_as_fast_as_its_size_deserves_unless_said_and_a_label_can_ask_for_memory_or_a_local_disk,
        a_spot_machine_is_asked_for_where_it_costs_least_for_its_speed_and_never_where_its_type_is_not_offered,
        every_spot_pool_is_asked_before_an_on_demand_machine_and_none_twice_in_a_minute,
        a_machine_for_a_job_cancelled_before_any_runner_took_it_is_ended_at_once,
        a_jobs_start_is_kept_what_aws_was_asked_and_what_its_machine_said_of_itself,
        a_job_waits_for_a_spot_machine_only_when_on_demand_was_refused_for_the_limit_alone_and_spot_was_asked,
        a_job_run_again_after_a_reclaim_goes_back_on_spot_when_the_on_demand_limit_cannot_take_it,
        runs_are_counted_once_each_and_those_of_jobs_counted_before_are_counted_by_the_upkeep,
        a_job_sent_on_demand_by_the_spot_limit_notes_when_machines_just_ended_still_counted,
        at_the_aws_accounts_limit_a_job_waits_for_one_of_its_own_to_end_and_fails_plainly_when_it_can_never_fit,
        the_spot_pools_left_when_the_best_has_no_machine_are_asked_in_one_request,
        a_job_gets_its_spot_machine_from_one_request_for_every_other_pool_and_a_role_that_may_not_asks_one_by_one,
        a_queued_job_is_answered_at_once_and_its_machine_asked_for_after,
        a_workflow_files_job_is_written_for_the_runner_as_github_writes_it,
        the_runners_calls_for_a_job_are_answered_as_its_service_answers_them,
        a_long_run_from_a_computers_log_is_given_with_its_steps_within_what_an_answer_may_be,
        a_run_from_a_computer_gets_its_machine_and_its_runner_takes_the_job_from_the_control_plane,
        a_key_for_coding_agents_starts_runs_within_the_control_planes_own_limits,
        the_secrets_a_ci_keeps_go_to_the_runs_from_a_computer_that_name_them_the_narrowest_under_each_name,
        a_run_from_a_computer_of_several_jobs_starts_each_once_what_it_needs_has_ended,
        a_machine_keeps_githubs_cache_in_its_regions_bucket_as_its_jobs_token_allows,
        a_label_asking_for_more_than_allowed_is_said_so,
        ubuntu_26_machines_keep_their_jobs_images_and_a_repository_that_used_some_has_them_from_the_start,
        the_plan_for_a_jobs_machine_follows_what_its_last_run_used,
        machines_fill_the_layer_store_and_each_repository_reads_only_what_it_may,
        docker_images_are_left_to_docker_when_the_cache_keeps_none,
        a_cache_turned_off_leaves_jobs_to_githubs,
        turbo_nx_and_bazel_keep_their_artifacts_by_the_branch_github_said_the_job_is_for,
        a_gitlab_jobs_cache_is_its_projects_by_the_branch_gitlab_said_once_the_job_began,
        a_gitlab_job_gitlab_never_told_of_gets_its_machine_from_the_sweep,
        a_job_started_after_a_sweep_gets_the_control_planes_own_links,
        a_gitlab_job_whose_events_never_come_is_seen_running_and_over_by_asking_gitlab,
        a_gitlab_machine_whose_jobs_event_never_came_learns_its_job_by_asking_gitlab,
        a_working_tree_goes_to_the_control_planes_bucket_in_one_and_its_checkout_is_sent_there,
        usage_counts_every_job_of_a_period_its_minutes_and_what_the_code_host_would_have_charged,
        a_store_without_ends_of_its_own_removes_a_record_at_its_end_and_not_one_written_again,
        jobs_kept_before_their_indexes_are_indexed_once_and_each_job_is_counted_in_its_days_totals_once,
        a_running_jobs_log_is_githubs_then_the_lines_its_machine_sent_after,
        a_machine_that_keeps_its_jobs_images_is_not_ended_with_its_job_but_goes_off_by_itself,
        gitlabs_projects_follow_a_move_and_leaving_takes_their_webhooks_and_its_runners_away,
        what_a_machine_sent_to_the_internet_is_added_to_its_jobs_cost_once_cloudwatch_has_counted_it,
        a_runner_name_github_has_already_gets_a_name_of_its_own,
        a_job_whose_machine_aws_refuses_is_told_why_by_what_aws_said,
        every_pair_of_aws_refusals_ends_each_kind_of_job_as_the_table_says,
        each_kind_of_label_asks_aws_for_the_machine_the_table_says,
        a_job_no_pool_has_room_for_is_asked_three_times_then_fails_and_a_type_a_zone_lacks_is_not_asked_there_again,
        a_fleet_refused_for_something_else_than_room_goes_on_with_the_pools_in_rounds,
        the_pools_asked_together_first_are_those_near_the_best_price_for_a_job_and_none_over_on_demand,
        spot_goes_first_to_the_region_where_a_job_costs_least_with_the_first_favoured,
        requests_that_name_nothing_here_or_cannot_be_read_are_said_as_that,
        a_job_whose_repository_github_gives_no_token_for_is_said_so_wherever_it_is_read,
        an_app_installed_nowhere_has_no_public_repositories_and_nothing_to_uninstall_and_one_github_refuses_is_said_on_leaving,
        events_this_control_plane_does_nothing_with_are_answered_and_change_nothing,
        a_gitlab_job_over_before_its_machine_came_is_cancelled_wherever_it_was,
        a_damaged_signing_key_or_no_aws_credentials_fail_what_needs_them_saying_why,
        many_jobs_kept_before_their_indexes_are_indexed_and_counted_over_several_sweeps_and_damaged_ones_are_passed_over,
        a_runs_jobs_that_cannot_be_read_or_called_are_refused_saying_why,
        a_runs_called_workflows_and_expressions_are_read_as_github_reads_them,
        a_damaged_or_gone_run_record_is_left_as_it_is_and_says_why_where_it_is_moved_on,
        what_a_run_from_a_computer_is_asked_for_that_it_does_not_have_is_said_as_that,
        a_run_taken_away_or_changed_by_another_request_while_it_is_moved_on_is_left_to_that_one,
        the_upkeep_ends_what_waited_too_long_or_was_cut_off_and_lets_go_of_failing_runners_github_no_longer_wants,
        just_after_a_move_here_the_upkeep_comes_back_within_the_minute_and_looks_for_jobs_github_did_not_tell_of,
        a_machine_whose_region_makes_no_address_is_said_so_at_each_link_and_what_it_sends_amiss_is_passed_over,
        githubs_keys_that_are_no_json_or_a_damaged_signing_key_leave_a_jobs_token_unread_saying_why,
        the_layer_store_passes_over_what_is_kept_damaged_and_says_what_a_machine_asks_amiss,
        a_control_plane_not_in_aws_converts_no_layer_and_gives_its_index_without_links,
        a_runners_calls_that_bring_nothing_are_answered_as_its_service_answers_them,
        a_job_whose_label_app_gitlab_or_run_is_gone_fails_at_its_next_start_saying_which,
        containers_and_launchers_that_cannot_start_a_machine_say_why_and_a_gpu_sandbox_is_named_with_its_gpu,
        an_aws_machines_cost_is_settled_at_its_on_demand_price_and_left_an_estimate_when_aws_cannot_be_asked,
        the_dashboards_requests_go_through_with_what_github_and_gitlab_say_and_whatever_they_refuse,
        a_runs_working_tree_logs_and_pages_go_through_the_bucket_and_go_from_it_after_a_day,
        gitlab_events_for_a_control_plane_that_moved_or_claimed_by_another_request_are_left_alone,
        installations_runners_and_machines_with_nothing_or_nobody_left_are_said_as_that,
        a_runs_artifacts_and_its_tools_cache_refuse_what_cannot_be_kept_and_say_what_is_there,
        a_runs_jobs_end_as_their_runners_say_or_when_they_run_out_of_time_or_are_cancelled,
        a_role_aws_will_not_let_this_control_plane_take_leaves_what_needs_it_undone_or_estimated,
        a_gpu_job_refused_spot_for_the_accounts_limit_with_on_demand_turned_off_says_which_limit_to_raise,
        logs_and_trees_kept_in_a_bucket_this_control_plane_can_no_longer_reach_are_passed_over_or_said,
        a_github_jobs_pages_without_whole_lines_or_times_that_are_no_times_are_read_as_they_are,
        a_job_claimed_as_the_store_fails_to_index_it_is_claimed_again_and_gets_its_machine,
        a_runs_cancelling_passes_over_what_its_record_lacks_and_a_runner_that_ends_before_it_takes_its_job_is_heard,
        machines_whose_jobs_went_elsewhere_hand_themselves_on_or_end_as_github_and_gitlab_say,
        launchers_and_failing_runners_start_where_they_can_and_say_what_they_could_not,
        a_runs_repository_environment_and_signing_key_are_read_as_they_are_or_said,
        a_failing_runner_that_takes_a_job_waiting_for_room_fails_it_and_its_own_job_gets_a_failing_runner_again,
        spot_pools_near_the_cheapest_in_any_region_are_asked_before_a_dearer_type_in_the_first,
        a_preempted_modal_function_is_run_again_once_on_a_sandbox_and_counted,
        a_modal_launcher_in_function_mode_starts_a_function_or_says_to_update_it
    ];
    // (Each test of this file is in it but those that already step through every point of a flow themselves.)
    let stepping = ["a_job_cancelled_at_any_step_of_its_start_or_of_a_sweep_leaves_no_machine_and_stays_cancelled", "a_job_that_begins_at_any_step_of_a_sweep_keeps_its_machine",
        "a_job_that_runs_and_ends_at_any_step_of_a_sweep_stays_done", "a_gitlab_job_cancelled_at_any_step_of_its_start_leaves_no_machine_and_no_runner", "every_test_here_holds_with_each_call_and_store_operation_failing"];
    let all: Vec<&str> = include_str!("plane.rs").split("#[test]\nfn ").skip(1).filter_map(|t| t.split('(').next()).collect();
    let missing: Vec<&&str> = all.iter().filter(|t| !stepping.contains(t) && !tests.iter().any(|(n, _)| n == *t)).collect();
    assert!(missing.is_empty(), "tests not in the sweep: {missing:?}");
    if std::env::var("SUPERCI_SWEEP_FAULTS").ok().as_deref() != Some("1") { return }
    panics_noted();
    let slice = {
        let said = std::env::var("SUPERCI_SWEEP_SLICE").unwrap_or_default();
        let (k, n) = said.split_once('/').and_then(|(k, n)| Some((k.parse::<usize>().ok()?, n.parse::<usize>().ok()?))).unwrap_or((1, 1));
        if n == 0 || k == 0 || k > n { (1, 1) } else { (k, n) }
    };
    let next = std::sync::Mutex::new(0usize);
    let found: std::sync::Mutex<Vec<String>> = Default::default();
    let runs = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    std::thread::scope(|s| for _ in 0..threads {
        s.spawn(|| loop {
            let i = { let mut n = next.lock().unwrap(); *n += 1; *n - 1 };
            let Some(&(name, test)) = tests.get(i) else { break };
            let (clean, (calls, ops, swaps)) = (run_faulted(test, None, usize::MAX), faults::off());
            if let Some(p) = clean { found.lock().unwrap().push(format!("{name}, clean: {p}")); continue }
            use faults::Fault::*;
            // (A slice of them, as the CLI's sweeps take one: `SUPERCI_SWEEP_SLICE=k/n`, every n-th from the k-th.)
            let plan = (1..=calls).flat_map(|k| [Call(k), Dropped(k), CallsFrom(k)]).chain((1..=ops).flat_map(|k| [Op(k), OpsFrom(k)])).chain((1..=swaps).map(SwapsLoseFrom))
                .enumerate().filter(|(i, _)| i % slice.1 == slice.0 - 1).map(|(_, f)| f);
            for fault in plan {
                runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(p) = run_faulted(test, Some(fault), 4 * (calls + ops) + 1_000).filter(|p| p.starts_with("the control plane")) {
                    found.lock().unwrap().push(format!("{name}, {fault:?}: {p}"))
                }
                faults::off();
            }
        });
    });
    let found = found.into_inner().unwrap();
    eprintln!("{} tests, {} faulted runs", tests.len(), runs.into_inner());
    assert!(found.is_empty(), "{} runs broke what must hold:\n  {}", found.len(), found.join("\n  "));
}

thread_local! { static PANICKED: RefCell<Option<String>> = const { RefCell::new(None) }; }

/// Panics on a thread a sweep runs on are noted there (where, and what they said), not printed.
fn panics_noted() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let before = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !faults::PLAN.with(|p| p.borrow().on) { return before(info) }
            let at = info.location().map(|l| l.file().to_string()).unwrap_or_default();
            let said = info.payload().downcast_ref::<String>().cloned().or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            let whose = if at.contains("/tests/") { "the test" } else { "the control plane" };
            PANICKED.with(|p| *p.borrow_mut() = Some(format!("{whose} panicked at {at}: {said}")));
        }));
    });
}

/// `test` run once with `fault` planned: where it panicked, if it did.
fn run_faulted(test: fn(), fault: Option<faults::Fault>, cap: usize) -> Option<String> {
    PANICKED.with(|p| *p.borrow_mut() = None);
    faults::on(fault, cap);
    let ended = std::panic::catch_unwind(test);
    ended.err().map(|_| PANICKED.with(|p| p.borrow_mut().take()).unwrap_or_else(|| "a panic".into()))
}
