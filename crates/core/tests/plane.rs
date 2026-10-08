//! A headless control plane end to end against fake GitHub, STS and EC2: only the public documents and a yes/no health are
//! online; the dashboard's report of the AWS role connects it once; a signed queued job launches a spot machine with its runner
//! registration and the finished job terminates it.
use std::cell::RefCell;
use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use futures::executor::block_on;
use superci_core::crypto::{hex, hmac_sha256};
use superci_core::github::App;
use superci_core::plane::{role_arn, Agent, Cache, Config, ControlPlane, Pool, Routing, Rule};
use superci_core::io::{self, Clock, Containers, Http, Request, Response, Store, Timer};

#[derive(Default)]
struct Mem(RefCell<BTreeMap<String, String>>);
#[async_trait(?Send)]
impl Store for Mem {
    async fn get(&self, k: &str) -> io::Result<Option<String>> { Ok(self.0.borrow().get(k).cloned()) }
    async fn put(&self, k: &str, v: String) -> io::Result<()> { self.0.borrow_mut().insert(k.into(), v); Ok(()) }
    async fn put_if_absent(&self, k: &str, v: String) -> io::Result<bool> { let mut m = self.0.borrow_mut(); if m.contains_key(k) { return Ok(false) } m.insert(k.into(), v); Ok(true) }
    async fn delete(&self, k: &str) -> io::Result<()> { self.0.borrow_mut().remove(k); Ok(()) }
    async fn list(&self, p: &str) -> io::Result<Vec<(String, String)>> { Ok(self.0.borrow().iter().filter(|(k, _)| k.starts_with(p)).map(|(k, v)| (k.clone(), v.clone())).collect()) }
}

#[derive(Default)]
struct FakeClouds { calls: RefCell<Vec<(String, String, String)>> }
#[async_trait(?Send)]
impl Http for FakeClouds {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let body = String::from_utf8_lossy(&r.body).to_string();
        self.calls.borrow_mut().push((r.method.clone(), r.url.clone(), body.clone()));
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
        if u.ends_with("/orgs/acme/actions/runners/generate-jitconfig") { return json(serde_json::json!({ "encoded_jit_config": "JITCONFIG+/=", "runner": { "id": 5 } })); }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1977" {
            assert_eq!(r.header("private-token"), Some("glpat-test"));
            return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1977, "status": "pending", "tag_list": ["superci-2cpu", "docker"] }).to_string()));
        }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1980" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1980, "status": "pending", "tag_list": ["superci-2cpu"] }).to_string())); }
        if u == "https://gitlab.example/api/v4/projects/380/jobs/1979" { return Ok(Response::new(200, "application/json", serde_json::json!({ "id": 1979, "status": "pending", "tag_list": ["docker", "superci-2cpu"] }).to_string())); }
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
struct Wakes(RefCell<Vec<u64>>);
#[async_trait(?Send)]
impl Timer for Wakes { async fn wake_in(&self, ms: u64) -> io::Result<()> { self.0.borrow_mut().push(ms); Ok(()) } }

const PLANE_URL: &str = "https://plane.acme.workers.dev";
const PLANE_ID: &str = "abc123def456";

fn app() -> App {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let pem = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap().to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap().to_string();
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
    config.dashboard_keys = vec![(1_700_000_000_000, "an-expired-session-key".into()), (1_800_000_000_000, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    let role = role_arn("123456789012", PLANE_ID);

    // The stack's report: the token, this control plane's own role, the chosen region; accepted once.
    assert_eq!(run(report("forged", &role)).status, 403);
    assert_eq!(run(report("connect-token-0001", "arn:aws:iam::123456789012:role/someone-elses")).status, 400);
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
    let extras = superci_core::aws::extras_usd_per_hour(0.08, 60, true);
    assert!((price - (0.065 + extras)).abs() < 1e-9, "its zone's spot price at launch, plus its disk and address: {price}");
    // Settled when it ended, at the prices AWS bills: a minute at least.
    let cost = job["cost_usd"].as_f64().unwrap();
    assert!((cost - (0.065 + extras) / 60.0).abs() < 1e-9 && job["cost_from"] == "prices", "{cost}");
    assert!(job["started_ms"].is_u64() && job["ended_ms"].is_u64());
    assert!(!wakes.0.borrow().is_empty());

    // The dashboard's view, with its session key only.
    assert_eq!(run(get("/status").with_header("authorization", "Bearer wrong-key-wrong-key")).status, 404);
    assert_eq!(run(get("/status").with_header("authorization", "Bearer an-expired-session-key")).status, 404, "expired");
    let status = json(&run(get("/status").with_header("authorization", "Bearer dashboard-session-key-1")));
    assert_eq!((status["app"]["slug"].as_str(), status["installations"][0]["account"].as_str(), status["aws"]["connected"].as_bool()), (Some("superci-acme-abc123"), Some("acme"), Some(true)));
    assert_eq!((status["jobs"][0]["job_id"].as_u64(), status["jobs"][0]["state"].as_str(), status["jobs"][0]["machine_id"].as_str(), status["jobs"][0]["cloud"].as_str()), (Some(7), Some("done"), Some("i-0abc"), Some("aws")));
}

#[test]
fn routing_set_in_the_control_plane_sends_jobs_to_a_cloudflare_agent_with_a_token_for_it_alone() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.agents = vec![Agent { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
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
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(webhook("queued", None)));
    let tried = launches(&clouds.0);
    // Zone b first (cheaper), out of room; then zone a. Each with its subnet, the group that lets nothing in, and a public address.
    assert_eq!(tried.iter().map(|f| field(f, "NetworkInterface.1.SubnetId")).take(2).collect::<Vec<_>>(), ["subnet-b", "subnet-a"]);
    assert!(tried.iter().all(|f| field(f, "NetworkInterface.1.SecurityGroupId.1") == "sg-1" && field(f, "NetworkInterface.1.AssociatePublicIpAddress") == "true"));
    assert_eq!(job(&store, 7)["state"], "launched");
    assert!(!store.0.borrow().keys().any(|k| k.starts_with("denied:")));

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
    old_config.dashboard_keys = vec![(u64::MAX, key.into())];
    old_config.move_token = Some("move-token-out-0123456789".into());
    let mut new_config = Config::new("newplane0001".into());
    new_config.dashboard_keys = vec![(u64::MAX, key.into())];
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
    block_on(old.handle(forwarded));
    let passed = clouds.calls.borrow().iter().rev().find(|c| c.1 == format!("{NEW_URL}/webhook")).cloned().expect("passed on");
    assert_eq!(passed.2, String::from_utf8(late.body.clone()).unwrap());
    assert!(sig.is_some());
    assert!(new_store.0.borrow().contains_key("job:41"), "the history came along");
    // And back: the old one, claimed again, is in use again.
    call(&old, PLANE_URL, "/move/claim", None, serde_json::json!({ "from": NEW_URL }));
    assert!(json(&block_on(old.handle(get("/health")))).get("moved_to").is_none());
}

#[derive(Default)]
struct OwnContainers(RefCell<Vec<String>>);
#[async_trait(?Send)]
impl Containers for OwnContainers {
    async fn start(&self, name: &str, work: &superci_core::io::Work, _max: u32, size: superci_core::spec::Size) -> io::Result<String> {
        let what = match work { superci_core::io::Work::GitHub { jit } => jit.clone(), superci_core::io::Work::GitLab { url, token } => format!("gitlab {url} {token}"),
            superci_core::io::Work::Fail { jit, why } => format!("fail {jit} {why}") };
        self.0.borrow_mut().push(format!("start {name} {what} {}cpu {}gb {}disk", size.cpu, size.ram_gb, size.disk_gb));
        Ok(name.to_string())
    }
    async fn stop(&self, id: &str) -> io::Result<()> { self.0.borrow_mut().push(format!("stop {id}")); Ok(()) }
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
    run(labelled("queued", 8, "acme/app", None, "superci-2cpu"));
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
    }
    assert!(own.0.borrow().iter().any(|c| c.starts_with("stop")) && clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1.contains("/actions/runners/")));
    // The third time it fails, saying why.
    clock.0.set(clock.0.get() + 4 * 60_000);
    block_on(plane.alarm()).unwrap();
    assert_eq!((starts(), job(&store, 7)["state"].as_str(), job(&store, 7)["error"].as_str()), (3, Some("failed"), Some("Cloudflare did not start a machine for it, three times")));
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

#[test]
fn gpus_and_windows_get_their_own_images_types_and_prices() {
    let (store, clouds, wakes, cache) = (Mem::default(), Kinds { clouds: FakeClouds::default(), no_gpus: false }, Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let run = |r: Request| block_on(plane.handle(r));
    // A GPU: the image with NVIDIA's drivers, T4 machines first.
    run(labelled("queued", 1, "acme/app", None, "superci-gpu"));
    assert_eq!(job(&store, 1)["state"], "launched");
    let l = &launches(&clouds.clouds)[0];
    assert_eq!(field(l, "InstanceType"), "g4dn.xlarge");
    assert!(clouds.clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeImages") && c.2.contains("ubuntu24-gpu-x64")));
    // Windows: Windows' image, its disk at least the image's, a PowerShell start-up, the runner's own work folder, and
    // Windows' spot prices.
    run(labelled("queued", 2, "acme/app", None, "superci-windows"));
    assert_eq!(job(&store, 2)["state"], "launched");
    let l = &launches(&clouds.clouds)[1];
    assert_eq!((field(l, "ImageId").as_str(), field(l, "BlockDeviceMapping.1.Ebs.VolumeSize").as_str()), ("ami-windows", "100"));
    let user_data = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(field(l, "UserData")).unwrap()).unwrap();
    assert!(user_data.starts_with("<powershell>") && user_data.contains(r".\run.cmd --jitconfig JITCONFIG+/=") && user_data.contains("shutdown.exe /s /f /t 21600"), "{user_data}");
    let jit = clouds.clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/generate-jitconfig")).last().cloned().unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&jit.2).unwrap()["work_folder"], "_work");
    assert!(clouds.clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeSpotPriceHistory") && c.2.contains("ProductDescription.1=Windows")));
    assert_eq!(job(&store, 2)["disk_gb"], 100);
}

#[test]
fn a_gpu_on_modal_goes_to_its_sandbox_and_into_the_price() {
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.agents = vec![Agent { cloud: "modal".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = vec![Pool { off: false, cloud: "modal".into(), max_jobs: None, monthly_usd: None }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci-gpu")));
    let launch = clouds.calls.borrow().iter().find(|c| c.1 == "https://runners.acme.workers.dev/launch").cloned().expect("agent launch");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&launch.2).unwrap()["gpu"], "T4", "any GPU: the least costly");
    let price = job(&store, 1)["usd_per_hour"].as_f64().unwrap();
    assert!((price - superci_core::plane::modal_usd_per_hour(2, 8) - 0.5904).abs() < 1e-6, "{price}");
}

#[test]
fn an_aws_account_with_no_gpu_quota_fails_gpu_jobs_at_once_saying_what_to_raise() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), Kinds { clouds: FakeClouds::default(), no_gpus: true }, Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { off: false, cloud: "cloudflare".into(), max_jobs: None, monthly_usd: None }, Pool { off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci-l4")));
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
    config.routing.order = vec![Pool { off: false, cloud: "cloudflare".into(), max_jobs: None, monthly_usd: None }, Pool { off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None }];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));

    // The default machine fits Cloudflare, first in the order.
    run(labelled("queued", 1, "acme/app", None, "superci"));
    assert_eq!(job(&store, 1)["cloud"], "cloudflare");
    // 8 CPUs do not (2 at most there): AWS, a type with 8 CPUs and 32 GB; the runner registers under the job's own label.
    run(labelled("queued", 2, "acme/app", None, "superci-8cpu"));
    assert_eq!(job(&store, 2)["cloud"], "aws");
    assert_eq!(field(&launches(&clouds)[0], "InstanceType"), "m8a.2xlarge");
    let jit = clouds.calls.borrow().iter().filter(|c| c.1.ends_with("/generate-jitconfig")).last().cloned().unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&jit.2).unwrap()["labels"], serde_json::json!(["superci-8cpu"]));
    // arm64, more memory per CPU, a bigger disk, on-demand: an arm64 image and type, no spot.
    run(labelled("queued", 3, "acme/app", None, "superci-arm64-2cpu-16gb-200disk-ondemand"));
    let l = &launches(&clouds)[1];
    assert_eq!((field(l, "InstanceType").as_str(), field(l, "InstanceMarketOptions.MarketType").as_str(), field(l, "BlockDeviceMapping.1.Ebs.VolumeSize").as_str()), ("r8g.large", "", "200"));
    assert!(clouds.calls.borrow().iter().any(|c| c.2.contains("Action=DescribeImages") && c.2.contains("arm64")), "the arm64 image");
    // Pinned to a cloud by the label.
    run(labelled("queued", 4, "acme/app", None, "superci-aws"));
    assert_eq!(job(&store, 4)["cloud"], "aws");
    // Nothing here runs macOS; a part that is not understood: each fails with why, nothing started.
    run(labelled("queued", 5, "acme/app", None, "superci-macos"));
    assert_eq!(job(&store, 5)["state"], "failed");
    assert!(job(&store, 5)["error"].as_str().unwrap().starts_with("nowhere to run macos"), "{}", job(&store, 5)["error"]);
    run(labelled("queued", 6, "acme/app", None, "superci-8cores"));
    assert!(job(&store, 6)["error"].as_str().unwrap().contains("8cores"));
    assert_eq!(launches(&clouds).len(), 3);
}

#[test]
fn limits_make_jobs_wait_or_move_down_the_order() {
    let (store, clouds, wakes, cache, own) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default(), OwnContainers::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.containers = true;
    config.routing.order = vec![Pool { off: false, cloud: "cloudflare".into(), max_jobs: Some(1), monthly_usd: None }];
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
    config.routing.order = vec![Pool { off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: Some(1.0) }];
    let store = Mem::default();
    store.0.borrow_mut().insert("job:50".into(), serde_json::json!({ "job_id": 50, "run_id": 1, "repo": "acme/app", "state": "done", "at_ms": 1_789_996_400_000u64, "ended_ms": 1_790_000_000_000u64,
        "installation_id": 42, "cloud": "aws", "usd_per_hour": 2.0, "seen_in_progress": true }).to_string());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 51, "acme/app", None)));
    assert_eq!((job(&store, 51)["state"].as_str(), job(&store, 51)["on_demand"].as_bool()), (Some("launched"), Some(true)), "{}", job(&store, 51));
    assert_eq!(field(launches(&clouds).last().unwrap(), "InstanceMarketOptions.MarketType"), "");
    // With on-demand turned off there is nothing below: it fails with why.
    config.routing.order.push(Pool { off: true, cloud: "aws-on-demand".into(), max_jobs: None, monthly_usd: None });
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(job_event("queued", 52, "acme/app", None)));
    let e = job(&store, 52)["error"].as_str().unwrap().to_string();
    assert_eq!(job(&store, 52)["state"], "failed");
    assert!(e.contains("AWS reached its $1 a month") && e.contains("AWS on-demand is turned off"), "{e}");
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
    store.0.borrow_mut().insert("glrunner:380:docker,superci-2cpu".into(), serde_json::json!({ "id": 9, "token": "glrt-old" }).to_string());
    // Pending, tagged superci-2cpu (and docker): a project runner for those tags, and a machine of that size.
    assert_eq!(run(gitlab_event(1977, "pending", "hook-secret-0123456789")).status, 200);
    let created = clouds.calls.borrow().iter().find(|c| c.1.ends_with("/user/runners")).cloned().unwrap();
    let form: Vec<(String, String)> = url::form_urlencoded::parse(created.2.as_bytes()).into_owned().collect();
    assert!(form.contains(&("tag_list".into(), "docker,superci-2cpu".into())) && form.contains(&("project_id".into(), "380".into())) && form.contains(&("run_untagged".into(), "false".into())));
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
    assert!(clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/9") && !store.0.borrow().contains_key("glrunner:380:docker,superci-2cpu"));
    assert!(store.0.borrow().contains_key("glrunner:55"));
    assert_eq!(job("1980")["state"], "swept");
    assert!(own.0.borrow().iter().any(|c| c == &format!("stop superci-{PLANE_ID}-gl1980")) && clouds.calls.borrow().iter().any(|c| c.0 == "DELETE" && c.1 == "https://gitlab.example/api/v4/runners/57"));
}

/// Containers that cannot start the first `fails` times (Cloudflare's "temporarily unavailable").
struct Flaky { fails: std::cell::Cell<u32>, starts: std::cell::Cell<u32> }
#[async_trait(?Send)]
impl Containers for Flaky {
    async fn start(&self, name: &str, _work: &superci_core::io::Work, _max: u32, _size: superci_core::spec::Size) -> io::Result<String> {
        self.starts.set(self.starts.get() + 1);
        if self.fails.get() > 0 { self.fails.set(self.fails.get() - 1); return Err("The container connection is temporarily unavailable, try again".into()) }
        Ok(name.to_string())
    }
    async fn stop(&self, _id: &str) -> io::Result<()> { Ok(()) }
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
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci-macos")));
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
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci-8cpu")));
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
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci-64cpu")));
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["error"].as_str()), (Some("failed"), Some("it asks for 64 CPUs; the most allowed is 32 (set in the dashboard)")));
    // It fails at once on GitHub: a small machine (AWS, the only place here) whose runner says why and fails the job.
    assert_eq!(job(&store, 1)["failed_fast"], true);
    let launch = clouds.calls.borrow().iter().find(|c| c.2.contains("Action=RunInstances") && c.2.contains("InstanceType=t3a.small")).cloned().expect("a failing runner");
    let user_data: String = url::form_urlencoded::parse(launch.2.as_bytes()).find(|(k, _)| k == "UserData").map(|(_, v)| v.to_string()).unwrap();
    let script = String::from_utf8(STANDARD.decode(user_data).unwrap()).unwrap();
    assert!(script.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh") && script.contains("( sleep 600; poweroff )"));
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci-8cpu-ondemand")));
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
    config.dashboard_keys = vec![(1_800_000_000_000, "dashboard-session-key-1".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let put = |id: u64, cloud: &str, state: &str| store.0.borrow_mut().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 1, "repo": "acme/app", "state": state, "at_ms": 1_789_999_000_000u64,
        "ended_ms": 1_789_999_300_000u64, "launched_ms": 1_789_999_000_000u64, "installation_id": 42, "cloud": cloud, "seen_in_progress": true, "usd_per_hour": 0.4 }).to_string());
    put(1, "cloudflare", "done");
    put(2, "aws", "done");
    put(3, "cloudflare", "running");
    let post = |key: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/costs")).with_header("authorization", &format!("Bearer {key}"))
        .with_body(serde_json::json!({ "measured": { "job:1": 0.0123, "job:2": 9.0, "job:3": 1.0, "job:9": 1.0, "plane_url": 1.0 } }).to_string())));
    assert_eq!(post("not-the-key-not-the-key").status, 404);
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
    config.dashboard_keys = vec![(u64::MAX, "dashboard-session-key-0123".into())];
    let plane = ControlPlane { store: &store, http: &clouds, clock: &clock, timer: &wakes, config: &config, cache: &cache, containers: Some(&own) };
    let run = |r: Request| block_on(plane.handle(r));
    let fails = || own.0.borrow().iter().filter(|c| c.contains(" fail ")).cloned().collect::<Vec<_>>();
    // macOS (nothing here runs it): a small container takes it and fails it, saying why (base64 of a GitHub ::error::).
    run(labelled("queued", 1, "acme/app", None, "superci-macos"));
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["failed_fast"].as_bool()), (Some("failed"), Some(true)));
    let f = fails();
    assert!(f.len() == 1 && f[0].starts_with(&format!("start superci-{PLANE_ID}-1-x fail JITCONFIG+/= ")) && f[0].ends_with(" 1cpu 4gb 8disk"), "{f:?}");
    let line = String::from_utf8(STANDARD.decode(f[0].split(' ').nth(4).unwrap()).unwrap()).unwrap();
    assert_eq!(line, "::error title=SuperCI could not run this job::SuperCI runs no macOS jobs yet. Use runs-on: macos-latest for GitHub's own.");
    // Refused for its repository (a public one): no failing runner (it could take another repository's job).
    run(labelled("queued", 2, "acme/site-public", None, "superci-macos"));
    assert_eq!((job(&store, 2)["state"].as_str(), fails().len()), (Some("failed"), 1));
}


#[test]
fn removed_aws_runners_take_no_jobs() {
    // Connected by role (a control plane elsewhere): forgotten on the dashboard's word, then no job goes to AWS.
    let (store, clouds, wakes, cache) = (Mem::default(), FakeClouds::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    config.dashboard_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
    store.0.borrow_mut().insert("aws".into(), serde_json::json!({ "account_id": "123456789012", "region": "us-east-1", "role_arn": "arn:aws:iam::123456789012:role/r", "connected": true }).to_string());
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    let forget = |key: &str| block_on(plane.handle(Request::new("POST", &format!("{PLANE_URL}/aws/forget")).with_header("authorization", &format!("Bearer {key}")).with_body("{}")));
    assert_eq!(forget("wrong-key-wrong-key").status, 404, "the dashboard's only");
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
    // Zone b first (cheaper), full; then zone a.
    block_on(plane.handle(labelled("queued", 1, "acme/app", None, "superci-gpu")));
    let tried = launches(&clouds.0);
    assert_eq!(tried.iter().map(|f| field(f, "Placement.AvailabilityZone")).take(2).collect::<Vec<_>>(), ["us-east-1b", "us-east-1a"]);
    assert_eq!((job(&store, 1)["state"].as_str(), job(&store, 1)["zone"].as_str()), (Some("launched"), Some("us-east-1a")));
    // Virginia full in every zone, Ohio allowing none: said as it is, not as a quota of 0.
    clouds.0.calls.borrow_mut().push(("".into(), "".into(), "full-everywhere".into()));
    block_on(plane.handle(labelled("queued", 2, "acme/app", None, "superci-gpu")));
    let e = job(&store, 2)["error"].as_str().unwrap_or_default().to_string();
    assert!(e.contains("us-east-1: InsufficientInstanceCapacity") && e.contains("us-east-2: MaxSpotInstanceCountExceeded") && !e.contains("Service Quotas"), "{e}");
}

/// The fake clouds, with GitHub saying how run 3 stands (`done`) and taking requests to run a job again.
struct Runs { clouds: FakeClouds, done: std::cell::Cell<bool>, may_rerun: bool }
#[async_trait(?Send)]
impl Http for Runs {
    async fn send(&self, r: Request) -> io::Result<Response> {
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
    config.agents = vec![Agent { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
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
        (r.url == "https://gitlab.example/api/v4/projects/380/jobs/1981").then(|| Response::new(200, "application/json", serde_json::json!({ "id": 1981, "status": "pending", "tag_list": ["superci-2cpu", "docker"] }).to_string()))
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
    // The job fails: GitLab is asked to run it again at once, and the new job gets an on-demand machine.
    run(gitlab_event_on(1977, "failed", secret, Some(55)));
    assert!(clouds.0.calls.borrow().iter().any(|c| c.0 == "POST" && c.1.ends_with("/projects/380/jobs/1977/retry")));
    assert_eq!((job(1977)["state"].as_str(), job(1977)["rerun"]["state"].as_str()), (Some("failed"), Some("asked")));
    assert!(job(1977)["error"].as_str().unwrap().starts_with("AWS took back its spot machine; run again"));
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
    assert!((j["usd_per_hour"].as_f64().unwrap() - (0.065 + superci_core::aws::extras_usd_per_hour(0.096, 60, false))).abs() < 0.02 && superci_core::aws::extras_usd_per_hour(0.08, 60, true) - superci_core::aws::extras_usd_per_hour(0.08, 60, false) == 0.005);

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
    config.dashboard_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
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
            ("GET", "/projects/7/jobs/1977") => json(200, serde_json::json!({ "id": 1977, "status": "pending", "tag_list": ["superci-2cpu"] })),
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
    config.dashboard_keys = vec![(u64::MAX, "dashboard-session-key-1".into())];
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
    assert_eq!(run(key(get("/gitlab/projects?g=nobody"))).status, 500);
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
    config.routing.order = ["aws", "cloudflare", "aws-on-demand"].iter().map(|c| Pool { off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None }).collect();
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
    run(labelled("queued", 8, "acme/app", None, "superci-16cpu"));
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
fn with_on_demand_turned_off_and_nothing_else_in_the_order_the_job_waits_for_a_spot_machine() {
    let no_spot = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |_: &Request, body: &str| (no_spot.get() && body.contains("Action=RunInstances") && body.contains("MarketType=spot")).then(|| aws_refuses("InsufficientInstanceCapacity")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.routing.order = vec![Pool { off: false, cloud: "aws".into(), max_jobs: None, monthly_usd: None }, Pool { off: true, cloud: "aws-on-demand".into(), max_jobs: None, monthly_usd: None }];
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
    block_on(plane.handle(labelled("queued", 9, "acme/app", None, "superci-ondemand")));
    let e = job(&store, 9)["error"].as_str().unwrap().to_string();
    assert!(job(&store, 9)["state"] == "failed" && e.contains("AWS on-demand is turned off"), "{e}");
}

#[test]
fn a_provider_that_cannot_start_the_machine_sends_the_job_to_the_next_in_the_order() {
    // Cloudflare's runner agent fails every launch (a hiccup of its own). The order: Cloudflare, then AWS.
    let down = std::cell::Cell::new(true);
    let clouds = With(FakeClouds::default(), |r: &Request, _: &str| (down.get() && r.url == "https://runners.acme.workers.dev/launch").then(|| Response::new(503, "text/plain", "containers temporarily unavailable")));
    let (store, wakes, cache) = (Mem::default(), Wakes::default(), Cache::default());
    let mut config = Config::new(PLANE_ID.into());
    config.app = Some(app());
    (config.aws_own, config.aws_own_creds) = own_aws();
    config.agents = vec![Agent { cloud: "cloudflare".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = ["cloudflare", "aws", "aws-on-demand"].iter().map(|c| Pool { off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None }).collect();
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
    assert!(e.contains("no provider could start its machine: Cloudflare: cloudflare runner agent: 503 containers temporarily unavailable; AWS: UnauthorizedOperation") && !e.contains("AWS on-demand"), "said once for AWS: {e}");
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
    config.agents = vec![Agent { cloud: "modal".into(), url: "https://runners.acme.workers.dev".into() }];
    config.routing.order = ["modal", "aws", "aws-on-demand"].iter().map(|c| Pool { off: false, cloud: c.to_string(), max_jobs: None, monthly_usd: None }).collect();
    let plane = ControlPlane { store: &store, http: &clouds, clock: &FixedClock, timer: &wakes, config: &config, cache: &cache, containers: None };
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci-gpu")));
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
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci-gpu")));
    let j = job(&store, 7);
    let e = j["error"].as_str().unwrap().to_string();
    assert_eq!((j["state"].as_str(), j["retries"].as_u64().unwrap_or(0)), (Some("failed"), 0), "{j}");
    assert!(e.starts_with("no provider could start its machine: Modal: modal runner agent: 400 GPUs need a payment method; AWS: AWS lets this account run no GPU machines yet"), "{e}");
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
    block_on(plane.handle(labelled("queued", 7, "acme/app", None, "superci-gpu")));
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
    config.dashboard_keys = vec![(1_800_000_000_000, "dashboard-session-key-1".into())];
    // As the runtimes read them from where they are stored: a name that says when it ends, the key's SHA-256.
    config.read_keys = [("READ_KEY_1800000000_AGENT", hash("superci_read_the-agents-key")), ("READ_KEY_1700000000_OLD", hash("superci_read_one-that-ended"))].iter()
        .filter_map(|(n, v)| superci_core::plane::read_key(n, v)).collect();
    assert_eq!(config.read_keys.len(), 2);
    assert!(superci_core::plane::read_key("READ_KEY_1800000000_AGENT", "").is_none() && superci_core::plane::read_key("READ_KEY_soon_AGENT", &hash("x")).is_none() && superci_core::plane::read_key("DASHBOARD_KEY_1800000000_AB", &hash("x")).is_none());
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
    for key in ["superci_read_one-that-ended", "superci_read_never-made-here", ""] { assert_eq!(ask("GET", "/status", key).status, 404, "{key}") }
    // It changes nothing: what the dashboard's key may do is like any unknown path to it.
    for path in ["/costs", "/gitlab/projects", "/leave", "/aws/forget", "/move/export", "/github/uninstall", "/permissions/given"] {
        assert_eq!(ask("POST", path, agent).status, 404, "{path}");
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
    assert_eq!(ask("GET", "/job/log?id=1", "superci_read_one-that-ended").status, 404);
    assert_eq!(block_on(plane.handle(get("/job/log?id=1"))).status, 404);
}
