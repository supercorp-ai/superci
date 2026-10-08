//! A SuperCI control plane on AWS Lambda (provided.al2023, arm64): GitHub's webhooks arrive through the function's
//! URL; state is one DynamoDB table; secrets (the GitHub App, runner agents, dashboard keys) are SecureString parameters
//! under /superci/<id>/ in SSM Parameter Store, written by the dashboard; a schedule calls the sweep every two
//! minutes. Machines in this account are started with the function's own role, so AWS needs no OpenID Connect here.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::executor::block_on;
use serde_json::{json, Value};

use superci_core::aws::{self, Credentials};
use superci_core::io::{self, Clock, Http, Request, Response, Store, Timer};
use superci_core::plane::{Agent, Cache, Config, ControlPlane};

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(25))).build().into()
}

struct Blocking(ureq::Agent);

#[async_trait(?Send)]
impl Http for Blocking {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let mut builder = ureq::http::Request::builder().method(r.method.as_str()).uri(r.url.as_str());
        for (k, v) in &r.headers { builder = builder.header(k.as_str(), v.as_str()) }
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = self.0.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        Ok(Response { status: response.status().as_u16(), headers, body: response.body_mut().read_to_vec().map_err(|e| e.to_string())? })
    }
}

/// The function's own credentials (from its role, in the environment).
fn own_credentials() -> Credentials {
    let var = |n: &str| std::env::var(n).unwrap_or_default();
    Credentials { access_key_id: var("AWS_ACCESS_KEY_ID"), secret_access_key: var("AWS_SECRET_ACCESS_KEY"), session_token: std::env::var("AWS_SESSION_TOKEN").ok(), expires_at_ms: u64::MAX }
}

/// State in one DynamoDB table: partition "s", sort key = the store's key, attribute v = its value.
struct Dynamo<'a> { http: &'a Blocking, table: String, region: String }

impl Dynamo<'_> {
    async fn call(&self, op: &str, body: Value) -> io::Result<Value> {
        aws::json_call(self.http, "POST", &format!("https://dynamodb.{}.amazonaws.com/", self.region), &self.region, "dynamodb", Some(&format!("DynamoDB_20120810.{op}")),
            "application/x-amz-json-1.0", Some(&body), &own_credentials(), now_ms()).await
    }
    fn key(&self, k: &str) -> Value { json!({ "p": { "S": "s" }, "k": { "S": k } }) }
}

#[async_trait(?Send)]
impl Store for Dynamo<'_> {
    async fn get(&self, key: &str) -> io::Result<Option<String>> {
        let v = self.call("GetItem", json!({ "TableName": self.table, "Key": self.key(key), "ConsistentRead": true })).await?;
        Ok(v["Item"]["v"]["S"].as_str().map(str::to_string))
    }
    async fn put(&self, key: &str, value: String) -> io::Result<()> {
        self.call("PutItem", json!({ "TableName": self.table, "Item": { "p": { "S": "s" }, "k": { "S": key }, "v": { "S": value } } })).await.map(|_| ())
    }
    /// Requests run in parallel here: DynamoDB's condition makes the claim atomic.
    async fn put_if_absent(&self, key: &str, value: String) -> io::Result<bool> {
        let r = self.call("PutItem", json!({ "TableName": self.table, "Item": { "p": { "S": "s" }, "k": { "S": key }, "v": { "S": value } }, "ConditionExpression": "attribute_not_exists(k)" })).await;
        match r { Ok(_) => Ok(true), Err(e) if e.starts_with("ConditionalCheckFailedException") => Ok(false), Err(e) => Err(e) }
    }
    async fn delete(&self, key: &str) -> io::Result<()> {
        self.call("DeleteItem", json!({ "TableName": self.table, "Key": self.key(key) })).await.map(|_| ())
    }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let (mut out, mut start) = (vec![], Value::Null);
        loop {
            let mut q = json!({ "TableName": self.table, "KeyConditionExpression": "p = :p AND begins_with(k, :k)", "ExpressionAttributeValues": { ":p": { "S": "s" }, ":k": { "S": prefix } }, "ConsistentRead": true });
            if !start.is_null() { q["ExclusiveStartKey"] = start }
            let v = self.call("Query", q).await?;
            for item in v["Items"].as_array().into_iter().flatten() {
                if let (Some(k), Some(val)) = (item["k"]["S"].as_str(), item["v"]["S"].as_str()) { out.push((k.to_string(), val.to_string())) }
            }
            start = v["LastEvaluatedKey"].clone();
            if start.is_null() { return Ok(out) }
        }
    }
}

struct SystemClock;
impl Clock for SystemClock { fn now_ms(&self) -> u64 { now_ms() } }

/// The schedule calls the sweep every two minutes whatever is asked, so a wake-up needs nothing here.
struct Scheduled;
#[async_trait(?Send)]
impl Timer for Scheduled { async fn wake_in(&self, _ms: u64) -> io::Result<()> { Ok(()) } }

/// The secrets under /superci/<id>/, read at most every 15 seconds.
struct Secrets { at: Option<Instant>, values: Vec<(String, String)> }

impl Secrets {
    /// `fresh`: a dashboard key not seen yet arrived (a new session just set it): read again now, at most every second.
    fn refresh(&mut self, http: &Blocking, region: &str, plane_id: &str, fresh: bool) -> io::Result<()> {
        let within = if fresh { Duration::from_secs(1) } else { Duration::from_secs(15) };
        if self.at.is_some_and(|t| t.elapsed() < within) { return Ok(()) }
        let (mut values, mut next) = (vec![], None::<String>);
        loop {
            let mut body = json!({ "Path": format!("/superci/{plane_id}/"), "Recursive": false, "WithDecryption": true, "MaxResults": 10 });
            if let Some(n) = &next { body["NextToken"] = json!(n) }
            let v = block_on(aws::json_call(http, "POST", &format!("https://ssm.{region}.amazonaws.com/"), region, "ssm", Some("AmazonSSM.GetParametersByPath"), "application/x-amz-json-1.1", Some(&body), &own_credentials(), now_ms()))?;
            for p in v["Parameters"].as_array().into_iter().flatten() {
                if let (Some(n), Some(val)) = (p["Name"].as_str(), p["Value"].as_str()) { values.push((n.rsplit('/').next().unwrap_or(n).to_string(), val.to_string())) }
            }
            next = v["NextToken"].as_str().map(str::to_string);
            if next.is_none() { break }
        }
        self.values = values;
        self.at = Some(Instant::now());
        Ok(())
    }
    fn get(&self, name: &str) -> Option<&str> { self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()) }
}

fn config(secrets: &Secrets, account_id: &str) -> Config {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
    if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase() }
    if let Some(v) = var("INSTANCE_TYPES") { c.instance_types = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() }
    c.app = secrets.get("GITHUB_APP").and_then(|v| serde_json::from_str(v).ok());
    // One parameter for each further organization's GitHub App: GITHUB_APP_<its id>.
    c.more_apps = secrets.values.iter().filter(|(n, _)| n.starts_with("GITHUB_APP_")).filter_map(|(_, v)| serde_json::from_str::<superci_core::github::App>(v).ok())
        .filter(|a| c.app.as_ref().is_none_or(|f| f.id != a.id)).collect();
    c.agents = secrets.get("AGENTS").and_then(|v| serde_json::from_str::<Vec<Agent>>(v).ok()).unwrap_or_default();
    c.routing = secrets.get("ROUTING").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    c.gitlab = secrets.get("GITLAB").and_then(|v| serde_json::from_str(v).ok());
    // One parameter for each further GitLab connection: GITLAB_<its name>.
    c.more_gitlabs = secrets.values.iter().filter(|(n, _)| n.starts_with("GITLAB_")).filter_map(|(_, v)| serde_json::from_str::<superci_core::gitlab::GitLab>(v).ok())
        .filter(|g| superci_core::gitlab::valid_id(&g.id)).collect();
    c.machine = secrets.get("MACHINE").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    // One parameter per dashboard session: DASHBOARD_KEY_<expires, unix seconds>_<random>.
    c.dashboard_keys = secrets.values.iter().filter_map(|(n, v)| Some((n.strip_prefix("DASHBOARD_KEY_")?.split('_').next()?.parse::<u64>().ok()? * 1000, v.clone()))).collect();
    // One parameter per key that only reads: READ_KEY_<expires, unix seconds>_<NAME>, holding the key's SHA-256.
    c.read_keys = secrets.values.iter().filter_map(|(n, v)| superci_core::plane::read_key(n, v)).collect();
    c.move_token = secrets.get("MOVE_TOKEN").map(str::to_string);
    if let Some(l) = secrets.get("CF_LOCATION") { c.cloudflare_location = l.to_string() }
    c.cloudflare_image = secrets.get("CF_IMAGE").and_then(superci_core::plane::image_address);
    c.aws_regions = secrets.get("AWS_REGIONS").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    c.aws_networks = secrets.get("AWS_NETWORKS").and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default();
    c.aws_own = Some((account_id.to_string(), var("RUNNER_REGION").or(var("AWS_REGION")).unwrap_or_default()));
    c.aws_own_creds = Some(own_credentials());
    c.aws_runners_off = secrets.get("AWS_RUNNERS") == Some("off");
    c
}

/// A function URL event (payload 2.0) as a request.
fn request(event: &Value) -> Request {
    let ctx = &event["requestContext"];
    let query = event["rawQueryString"].as_str().filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("https://{}{}{query}", ctx["domainName"].as_str().unwrap_or_default(), event["rawPath"].as_str().unwrap_or("/"));
    let body = event["body"].as_str().unwrap_or_default();
    let body = if event["isBase64Encoded"] == true { STANDARD.decode(body).unwrap_or_default() } else { body.as_bytes().to_vec() };
    let mut r = Request::new(ctx["http"]["method"].as_str().unwrap_or("GET"), &url).with_body(body);
    for (k, v) in event["headers"].as_object().into_iter().flatten() { if let Some(v) = v.as_str() { r.headers.push((k.to_ascii_lowercase(), v.to_string())) } }
    r
}

fn response(r: Response) -> Value {
    let headers: serde_json::Map<String, Value> = r.headers.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    json!({ "statusCode": r.status, "headers": headers, "body": STANDARD.encode(&r.body), "isBase64Encoded": true })
}

fn main() {
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").expect("runs in AWS Lambda");
    let region = std::env::var("AWS_REGION").unwrap_or_default();
    let (http, runtime) = (Blocking(agent()), ureq::Agent::from(ureq::Agent::config_builder().http_status_as_error(false).build()));
    let table = std::env::var("TABLE").unwrap_or_default();
    let plane_id = std::env::var("PLANE_ID").unwrap_or_default();
    let cache = Cache::default();
    let mut secrets = Secrets { at: None, values: vec![] };
    loop {
        let Ok(mut next) = runtime.get(&format!("http://{api}/2018-06-01/runtime/invocation/next")).call() else { continue };
        let id = next.headers().get("lambda-runtime-aws-request-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let account_id = next.headers().get("lambda-runtime-invoked-function-arn").and_then(|v| v.to_str().ok()).and_then(|a| a.split(':').nth(4)).unwrap_or_default().to_string();
        let event: Value = next.body_mut().read_json().unwrap_or_default();
        let outcome: io::Result<Value> = (|| {
            // Whoever comes with a key (the dashboard, a command) gets the settings as they are now, not as they
            // were up to fifteen seconds ago: a change is seen at once, and the next one starts from it. A key not
            // known yet is a new session's, set a moment ago. Webhooks and the sweep carry none, and use what was read.
            let bearer = event["headers"]["authorization"].as_str().and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
            let unknown = bearer.len() >= 16;
            // A move's steps need what the dashboard wrote a moment ago (the App, GitLab): read fresh for them too.
            let moving = event["rawPath"].as_str().is_some_and(|p| p.starts_with("/move/"));
            secrets.refresh(&http, &region, &plane_id, unknown || moving)?;
            let config = config(&secrets, &account_id);
            let store = Dynamo { http: &http, table: table.clone(), region: region.clone() };
            let plane = ControlPlane { store: &store, http: &http, clock: &SystemClock, timer: &Scheduled, config: &config, cache: &cache, containers: None };
            if event["superci"] == "sweep" { return block_on(plane.alarm()).map(|_| json!({ "swept": true })) }
            Ok(response(block_on(plane.handle(request(&event)))))
        })();
        let _ = match outcome {
            Ok(v) => runtime.post(&format!("http://{api}/2018-06-01/runtime/invocation/{id}/response")).send_json(v),
            Err(e) => { eprintln!("superci: {e}"); runtime.post(&format!("http://{api}/2018-06-01/runtime/invocation/{id}/error")).send_json(json!({ "errorMessage": e, "errorType": "superci" })) }
        };
    }
}
