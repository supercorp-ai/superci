//! A SuperCI control plane on AWS Lambda (provided.al2023, arm64): GitHub's webhooks arrive through the function's
//! URL; state is one DynamoDB table; secrets (the GitHub App, launchers, dashboard keys) are SecureString parameters
//! under /superci/<id>/ in SSM Parameter Store, written by the dashboard; a schedule calls the sweep every minute,
//! and the function calls itself for a sweep sooner than that (see `Scheduled`). Machines in this account are
//! started with the function's own role, so AWS needs no OpenID Connect here.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::executor::block_on;
use serde_json::{json, Value};

use superci_core::aws::{self, Credentials};
use superci_core::io::{self, Clock, Http, Request, Response, Store, Timer};
use superci_core::plane::{Cache, Config, ControlPlane};

mod convert;

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

fn agent(redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).max_redirects(redirects).timeout_global(Some(Duration::from_secs(25))).build().into()
}

/// The client: one that follows redirects, and one that gives them back as they are (asked of a request for where
/// GitHub keeps a log: the link is the answer, not what is there).
struct Blocking(ureq::Agent, ureq::Agent);

#[async_trait(?Send)]
impl Http for Blocking {
    async fn send(&self, r: Request) -> io::Result<Response> {
        // (In a test, the clouds' stand-in: see `io::stand_in`.)
        let url = io::stand_in(&r.url);
        let mut builder = ureq::http::Request::builder().method(r.method.as_str()).uri(url.as_str());
        for (k, v) in &r.headers { if url == r.url || !k.eq_ignore_ascii_case("host") { builder = builder.header(k.as_str(), v.as_str()) } }
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = if r.no_follow { &self.1 } else { &self.0 }.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        Ok(Response { status: response.status().as_u16(), headers, body: response.body_mut().read_to_vec().map_err(|e| e.to_string())? })
    }
}

/// The function's own credentials (from its role, in the environment).
fn own_credentials() -> Credentials {
    let var = |n: &str| std::env::var(n).unwrap_or_default();
    Credentials { access_key_id: var("AWS_ACCESS_KEY_ID"), secret_access_key: var("AWS_SECRET_ACCESS_KEY"), session_token: std::env::var("AWS_SESSION_TOKEN").ok(), expires_at_ms: u64::MAX }
}

/// What DynamoDB says when a call may be made again: too much at once, or a passing fault of its own.
const AGAIN: [&str; 6] = ["ThrottlingException", "ProvisionedThroughputExceededException", "RequestLimitExceeded", "InternalServerError", "ServiceUnavailable", "HTTP 5"];

/// State in one DynamoDB table: each kind of record in a partition of its own (`p`: the key up to its first colon,
/// `job` for `job:1717`), the key itself as the sort key (`k`), the record as text (`v`), and its end in seconds (`x`,
/// the table's time to live) when it has one. Before 0.19.0 every record was in the one partition `s`: until those
/// are moved (`migrate`, a little at each sweep; then `layout` says so), what is not found in its own is looked for
/// there too.
struct Dynamo<'a> { http: &'a dyn Http, table: String, region: String, old: std::cell::Cell<Option<bool>> }

/// The partition a key is kept in: its kind.
fn part(key: &str) -> &str { key.split(':').next().unwrap_or(key) }

/// Where the layout is said to be this one (once nothing is left in the old partition).
const LAYOUT: &str = "layout";

impl Dynamo<'_> {
    /// One call, asked again when DynamoDB says to (throttled, or a passing error of its own), as AWS's own SDKs do:
    /// up to ten times, each wait twice the last at most (from 50 ms to 5 s), a random part of it.
    async fn call(&self, op: &str, body: Value) -> io::Result<Value> {
        let (mut most_ms, mut tries) = (50u64, 0);
        loop {
            tries += 1;
            if tries == 10 { break }
            match aws::json_call(self.http, "POST", &format!("https://dynamodb.{}.amazonaws.com/", self.region), &self.region, "dynamodb", Some(&format!("DynamoDB_20120810.{op}")),
                "application/x-amz-json-1.0", Some(&body), &own_credentials(), now_ms()).await {
                Err(e) if AGAIN.iter().any(|kind| e.starts_with(kind)) => {
                    let r = superci_core::crypto::random_bytes(2);
                    std::thread::sleep(std::time::Duration::from_millis(u16::from_le_bytes([r[0], r[1]]) as u64 % most_ms + 1));
                    most_ms = (most_ms * 2).min(5000);
                }
                done => return done,
            }
        }
        aws::json_call(self.http, "POST", &format!("https://dynamodb.{}.amazonaws.com/", self.region), &self.region, "dynamodb", Some(&format!("DynamoDB_20120810.{op}")),
            "application/x-amz-json-1.0", Some(&body), &own_credentials(), now_ms()).await
    }
    fn key(&self, k: &str) -> Value { json!({ "p": { "S": part(k) }, "k": { "S": k } }) }
    fn old_key(&self, k: &str) -> Value { json!({ "p": { "S": "s" }, "k": { "S": k } }) }
    fn item(&self, k: &str, value: String, until_ms: Option<u64>) -> Value {
        let mut item = json!({ "p": { "S": part(k) }, "k": { "S": k }, "v": { "S": value } });
        if let Some(until) = until_ms { item["x"] = json!({ "N": (until / 1000).to_string() }) }
        item
    }
    /// Whether records may still be in the old partition (asked once per request).
    async fn old(&self) -> io::Result<bool> {
        if let Some(old) = self.old.get() { return Ok(old) }
        let v = self.call("GetItem", json!({ "TableName": self.table, "Key": self.key(LAYOUT), "ConsistentRead": true })).await?;
        let old = v["Item"]["v"]["S"].as_str() != Some("kinds");
        self.old.set(Some(old));
        Ok(old)
    }
    async fn get_old(&self, key: &str) -> io::Result<Option<String>> {
        if !self.old().await? { return Ok(None) }
        let v = self.call("GetItem", json!({ "TableName": self.table, "Key": self.old_key(key), "ConsistentRead": true })).await?;
        Ok(v["Item"]["v"]["S"].as_str().map(str::to_string))
    }
    async fn query(&self, partition: &str, prefix: &str, last: Option<usize>) -> io::Result<Vec<(String, String)>> {
        let (mut out, mut start) = (vec![], Value::Null);
        loop {
            let mut q = json!({ "TableName": self.table, "KeyConditionExpression": "p = :p AND begins_with(k, :k)", "ExpressionAttributeValues": { ":p": { "S": partition }, ":k": { "S": prefix } }, "ConsistentRead": true });
            if let Some(n) = last { q["ScanIndexForward"] = false.into(); q["Limit"] = (n - out.len()).into() }
            if !start.is_null() { q["ExclusiveStartKey"] = start }
            let v = self.call("Query", q).await?;
            for item in v["Items"].as_array().into_iter().flatten() {
                if let (Some(k), Some(val)) = (item["k"]["S"].as_str(), item["v"]["S"].as_str()) { out.push((k.to_string(), val.to_string())) }
            }
            start = v["LastEvaluatedKey"].clone();
            if start.is_null() || last.is_some_and(|n| out.len() >= n) { break }
        }
        if last.is_some() { out.reverse() }
        Ok(out)
    }
    /// A conditional write: whether it was made.
    async fn put_when(&self, put: Value) -> io::Result<bool> {
        match self.call("PutItem", put).await { Ok(_) => Ok(true), Err(e) if e.starts_with("ConditionalCheckFailedException") => Ok(false), Err(e) => Err(e) }
    }
}

#[async_trait(?Send)]
impl Store for Dynamo<'_> {
    async fn get(&self, key: &str) -> io::Result<Option<String>> {
        let v = self.call("GetItem", json!({ "TableName": self.table, "Key": self.key(key), "ConsistentRead": true })).await?;
        match v["Item"]["v"]["S"].as_str() { Some(s) => Ok(Some(s.to_string())), None => self.get_old(key).await }
    }
    async fn put(&self, key: &str, value: String) -> io::Result<()> {
        self.call("PutItem", json!({ "TableName": self.table, "Item": self.item(key, value, None) })).await.map(|_| ())
    }
    /// Requests run in parallel here: DynamoDB's condition makes the claim atomic.
    async fn put_if_absent(&self, key: &str, value: String) -> io::Result<bool> {
        if self.get_old(key).await?.is_some() { return Ok(false) }
        self.put_when(json!({ "TableName": self.table, "Item": self.item(key, value, None), "ConditionExpression": "attribute_not_exists(k)" })).await
    }
    async fn swap(&self, key: &str, was: Option<&str>, value: String) -> io::Result<bool> { self.swap_until(key, was, value, None).await }
    async fn delete(&self, key: &str) -> io::Result<()> {
        self.call("DeleteItem", json!({ "TableName": self.table, "Key": self.key(key) })).await?;
        if self.old().await? { self.call("DeleteItem", json!({ "TableName": self.table, "Key": self.old_key(key) })).await?; }
        Ok(())
    }
    async fn list(&self, prefix: &str) -> io::Result<Vec<(String, String)>> {
        let mut out = self.query(part(prefix), prefix, None).await?;
        if self.old().await? {
            let have: std::collections::HashSet<String> = out.iter().map(|(k, _)| k.clone()).collect();
            out.extend(self.query("s", prefix, None).await?.into_iter().filter(|(k, _)| !have.contains(k)));
            out.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Ok(out)
    }
    async fn put_until(&self, key: &str, value: String, until_ms: u64) -> io::Result<()> {
        self.call("PutItem", json!({ "TableName": self.table, "Item": self.item(key, value, Some(until_ms)) })).await.map(|_| ())
    }
    /// The same for a write over what was read: only while the value is still that one. (A record still in the old
    /// partition is written in its own, only while that one is not there yet.)
    async fn swap_until(&self, key: &str, was: Option<&str>, value: String, until_ms: Option<u64>) -> io::Result<bool> {
        let mut put = json!({ "TableName": self.table, "Item": self.item(key, value, until_ms) });
        let in_old = match was { Some(was) if self.old().await? => {
            let here = self.call("GetItem", json!({ "TableName": self.table, "Key": self.key(key), "ConsistentRead": true })).await?;
            here["Item"].is_null() && self.get_old(key).await?.as_deref() == Some(was)
        } _ => false };
        match was {
            Some(was) if !in_old => { put["ConditionExpression"] = "v = :was".into(); put["ExpressionAttributeValues"] = json!({ ":was": { "S": was } }) }
            _ => put["ConditionExpression"] = "attribute_not_exists(k)".into(),
        }
        let made = self.put_when(put).await?;
        if made && in_old { self.call("DeleteItem", json!({ "TableName": self.table, "Key": self.old_key(key) })).await?; }
        Ok(made)
    }
    async fn list_last(&self, prefix: &str, n: usize) -> io::Result<Vec<(String, String)>> {
        if n == 0 { return Ok(vec![]) }
        if self.old().await? {
            let mut all = self.list(prefix).await?;
            return Ok(all.split_off(all.len().saturating_sub(n)))
        }
        self.query(part(prefix), prefix, Some(n)).await
    }
    /// A hundred at a time (DynamoDB's most), asked again for what it leaves for later (what it still leaves after
    /// eight times, read one by one).
    async fn get_many(&self, keys: &[String]) -> io::Result<Vec<Option<String>>> {
        let mut found: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut left: std::collections::HashSet<String> = std::collections::HashSet::new();
        for chunk in keys.chunks(100) {
            let mut want: Vec<Value> = chunk.iter().collect::<std::collections::BTreeSet<_>>().into_iter().map(|k| self.key(k)).collect();
            let mut asked = 0;
            loop {
                asked += 1;
                let v = self.call("BatchGetItem", json!({ "RequestItems": { &self.table: { "Keys": want, "ConsistentRead": true } } })).await?;
                for item in v["Responses"][&self.table].as_array().into_iter().flatten() {
                    if let (Some(k), Some(val)) = (item["k"]["S"].as_str(), item["v"]["S"].as_str()) { found.insert(k.to_string(), val.to_string()); }
                }
                want = v["UnprocessedKeys"][&self.table]["Keys"].as_array().cloned().unwrap_or_default();
                if want.is_empty() || asked == 8 { break }
            }
            left.extend(want.iter().filter_map(|k| k["k"]["S"].as_str().map(str::to_string)));
        }
        let mut out = Vec::with_capacity(keys.len());
        for k in keys { out.push(match found.get(k) { Some(v) => Some(v.clone()), None if left.contains(k) => self.get(k).await?, None => self.get_old(k).await? }) }
        Ok(out)
    }
    /// Moves what is left in the old partition into each record's own (not over one written there since), then says
    /// the layout is this one.
    async fn migrate(&self, budget: usize) -> io::Result<bool> {
        if !self.old().await? { return Ok(true) }
        let v = self.call("Query", json!({ "TableName": self.table, "KeyConditionExpression": "p = :p", "ExpressionAttributeValues": { ":p": { "S": "s" } }, "Limit": budget.max(1), "ConsistentRead": true })).await?;
        let items = v["Items"].as_array().cloned().unwrap_or_default();
        for item in &items {
            let (Some(k), Some(val)) = (item["k"]["S"].as_str(), item["v"]["S"].as_str()) else { continue };
            self.put_when(json!({ "TableName": self.table, "Item": self.item(k, val.to_string(), None), "ConditionExpression": "attribute_not_exists(k)" })).await?;
            self.call("DeleteItem", json!({ "TableName": self.table, "Key": self.old_key(k) })).await?;
        }
        if !items.is_empty() || !v["LastEvaluatedKey"].is_null() { return Ok(false) }
        self.put(LAYOUT, "kinds".into()).await?;
        self.old.set(Some(false));
        Ok(true)
    }
}

struct SystemClock;
impl Clock for SystemClock { fn now_ms(&self) -> u64 { now_ms() } }

/// A wake-up asked for within this long is the function's own doing; a later one is left to the schedule.
const SOON_MS: u64 = 30_000;

/// The schedule calls the sweep every minute, which is late for what is asked for sooner: a start that failed and
/// is tried again, jobs waiting for room (twenty seconds). For those the function calls itself, not waiting for the
/// answer; that call waits out the time and sweeps (see `main`). One such call is on its way at a time, however
/// many jobs ask (claimed in the table): a sweep is for every job, and asks for the next itself while any waits.
/// Where it may not call itself (a role from before), the schedule does it, as it did.
struct Scheduled<'a> { http: &'a dyn Http, store: &'a Dynamo<'a>, region: &'a str, function: &'a str,
    /// What is to be done once the request at hand is answered (see `main`).
    later: &'a std::cell::RefCell<Vec<String>> }

/// Where a sweep that is going notes itself (when it began, ms).
const SWEEPING: &str = "aws:sweeping";

/// Whether a sweep noted as going since `since` was cut off: it began longer ago than a call may run (`runs_for`, this
/// call's whole time: the function's own, as every call's), and half a minute more. Never sooner than a minute and a
/// half, as before the function's time was raised: a sweep still going is never taken for cut off, and two never start
/// a machine for the same job.
fn cut_off(since: u64, now: u64, runs_for: u64) -> bool { now.saturating_sub(since) >= (runs_for + 30_000).max(90_000) }

/// Where a sweep that is on its way is noted (when it is for, ms). One still there well after its time never came.
const WAKING: &str = "aws:waking";

#[async_trait(?Send)]
impl Timer for Scheduled<'_> {
    async fn wake_in(&self, ms: u64) -> io::Result<()> {
        if ms > SOON_MS || self.function.is_empty() { return Ok(()) }
        let at = now_ms() + ms;
        // (Nothing here fails what asked: the schedule's sweep comes within the minute whatever happens.)
        if !self.store.put_if_absent(WAKING, at.to_string()).await.unwrap_or(false) {
            let coming = self.store.get(WAKING).await.ok().flatten().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            if now_ms() < coming + SOON_MS { return Ok(()) }
            let _ = self.store.put(WAKING, at.to_string()).await;
        }
        let body = json!({ "superci": "wake", "at": at }).to_string();
        let url = format!("https://lambda.{}.amazonaws.com/2015-03-31/functions/{}/invocations", self.region, self.function);
        let headers = vec![("content-type".to_string(), "application/json".to_string()), ("x-amz-invocation-type".to_string(), "Event".to_string())];
        let sent = match aws::sigv4("POST", &url, &headers, body.as_bytes(), &own_credentials(), self.region, "lambda", &aws::amz_date(now_ms())) {
            Ok(signed) => { let mut r = Request::new("POST", &url).with_body(body); r.headers = signed; self.http.send(r).await.is_ok_and(|r| superci_core::io::done(r.status)) }
            Err(_) => false,
        };
        if !sent { let _ = self.store.delete(WAKING).await; }
        Ok(())
    }
    /// A Lambda answers one request at a time: it simply waits.
    async fn pause(&self, ms: u64) { std::thread::sleep(Duration::from_millis(ms)) }
    /// Done by `main` once the answer is given: the function goes on running until it asks for its next request.
    fn later(&self, what: &str) -> bool { self.later.borrow_mut().push(what.to_string()); true }
}

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
}

fn config(secrets: &Secrets, account_id: &str) -> Config {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    let mut c = Config::new(var("PLANE_ID").unwrap_or_default());
    if let Some(v) = var("LABEL") { c.label = v.to_ascii_lowercase() }
    if let Some(v) = var("INSTANCE_TYPES") { c.instance_types = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() }
    c.read_settings(&secrets.values);
    c.aws_own = Some((account_id.to_string(), var("RUNNER_REGION").or(var("AWS_REGION")).unwrap_or_default()));
    c.aws_own_creds = Some(own_credentials());
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

/// The converter's loop (see `convert`): one layer a request, answered with what was made.
fn converter(api: &str) -> ! {
    let (http, runtime) = (Blocking(agent(10), agent(0)), ureq::Agent::from(ureq::Agent::config_builder().http_status_as_error(false).build()));
    let mkfs = format!("{}/mkfs.erofs", std::env::var("LAMBDA_TASK_ROOT").unwrap_or_else(|_| "/var/task".into()));
    loop {
        let Ok(mut next) = runtime.get(&format!("http://{api}/2018-06-01/runtime/invocation/next")).call() else { continue };
        let id = next.headers().get("lambda-runtime-aws-request-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let event: Value = next.body_mut().read_json().unwrap_or_default();
        let done = match serde_json::from_value::<convert::Job>(event) {
            Ok(job) => { let done = convert::convert(&http, &own_credentials(), &job, &mkfs, &std::env::temp_dir()); eprintln!("superci: {done}"); done }
            Err(e) => json!({ "error": format!("not a layer to convert: {e}") }),
        };
        let _ = runtime.post(&format!("http://{api}/2018-06-01/runtime/invocation/{id}/response")).send_json(done);
    }
}

fn main() {
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").expect("runs in AWS Lambda");
    if std::env::var("SUPERCI_CONVERTER").is_ok_and(|v| v == "1") { converter(&api) }
    let region = std::env::var("AWS_REGION").unwrap_or_default();
    let (http, runtime) = (Blocking(agent(10), agent(0)), ureq::Agent::from(ureq::Agent::config_builder().http_status_as_error(false).build()));
    let table = std::env::var("TABLE").unwrap_or_default();
    let plane_id = std::env::var("PLANE_ID").unwrap_or_default();
    let function = std::env::var("AWS_LAMBDA_FUNCTION_NAME").unwrap_or_default();
    let cache = Cache::default();
    let mut secrets = Secrets { at: None, values: vec![] };
    loop {
        let Ok(mut next) = runtime.get(&format!("http://{api}/2018-06-01/runtime/invocation/next")).call() else { continue };
        let id = next.headers().get("lambda-runtime-aws-request-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let account_id = next.headers().get("lambda-runtime-invoked-function-arn").and_then(|v| v.to_str().ok()).and_then(|a| a.split(':').nth(4)).unwrap_or_default().to_string();
        // How long this call may run (the function's time, as set: Lambda says when it ends).
        let runs_for = next.headers().get("lambda-runtime-deadline-ms").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).map_or(0, |end| end.saturating_sub(now_ms()));
        let event: Value = next.body_mut().read_json().unwrap_or_default();
        // Whoever comes with a key (the dashboard, a command) gets the settings as they are now, not as they
        // were up to fifteen seconds ago: a change is seen at once, and the next one starts from it. A key not
        // known yet is a new session's, set a moment ago. Webhooks and the sweep carry none, and use what was read.
        let bearer = event["headers"]["authorization"].as_str().and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
        let unknown = bearer.len() >= 16;
        // A move's steps need what the dashboard wrote a moment ago (the App, GitLab): read fresh for them too.
        let moving = event["rawPath"].as_str().is_some_and(|p| p.starts_with("/move/"));
        let refreshed = secrets.refresh(&http, &region, &plane_id, unknown || moving);
        let config = config(&secrets, &account_id);
        let store = Dynamo { http: &http, table: table.clone(), region: region.clone(), old: std::cell::Cell::new(None) };
        let later = std::cell::RefCell::new(vec![]);
        let timer = Scheduled { http: &http, store: &store, region: &region, function: &function, later: &later };
        let plane = ControlPlane { store: &store, http: &http, clock: &SystemClock, timer: &timer, config: &config, cache: &cache, containers: None };
        let outcome: io::Result<Value> = (|| {
            refreshed?;
            // One sweep at a time: the schedule's and the function's own could otherwise each start a machine for
            // the same waiting job. One that finds another going leaves it to that one (which asks for the next
            // itself); a note older than the function may run is of a sweep that was cut off.
            let sweep = || -> io::Result<Value> {
                let now = now_ms();
                if !block_on(store.put_if_absent(SWEEPING, now.to_string())).unwrap_or(true) {
                    let since = block_on(store.get(SWEEPING)).ok().flatten().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    if !cut_off(since, now, runs_for) { return Ok(json!({ "swept": false })) }
                    let _ = block_on(store.put(SWEEPING, now.to_string()));
                }
                let done = block_on(plane.alarm()).map(|_| json!({ "swept": true }));
                let _ = block_on(store.delete(SWEEPING));
                done
            };
            if event["superci"] == "sweep" { return sweep() }
            // Its own call for a sweep sooner than the schedule's: the time is waited out here, then the sweep.
            if event["superci"] == "wake" {
                let at = event["at"].as_u64().unwrap_or(0);
                std::thread::sleep(Duration::from_millis(at.saturating_sub(now_ms()).min(SOON_MS)));
                let _ = block_on(store.delete(WAKING));
                return sweep()
            }
            Ok(response(block_on(plane.handle(request(&event)))))
        })();
        let _ = match outcome {
            Ok(v) => runtime.post(&format!("http://{api}/2018-06-01/runtime/invocation/{id}/response")).send_json(v),
            Err(e) => { eprintln!("superci: {e}"); runtime.post(&format!("http://{api}/2018-06-01/runtime/invocation/{id}/error")).send_json(json!({ "errorMessage": e, "errorType": "superci" })) }
        };
        // What was left for after the answer (a queued job's machine: GitHub has its answer, and AWS is asked now).
        // The function runs on until it asks for its next request, and takes no other meanwhile.
        let url = request(&event).url;
        // (One thing done may leave another: each in its turn, until none is left.)
        loop {
            let next = { let mut left = later.borrow_mut(); if left.is_empty() { None } else { Some(left.remove(0)) } };
            let Some(what) = next else { break };
            if let Err(e) = block_on(plane.later(&url, &what)) { eprintln!("superci: {what}: {e}") }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The DynamoDB store against a stand-in for DynamoDB's own API (its calls and answers as AWS gives them, kept in
    //! memory): what every store promises, the partitions it keeps records in, ends, and moving the layout before
    //! 0.19.0 over while it is read.
    use super::*;

    #[test]
    fn a_sweep_is_taken_as_cut_off_only_after_a_call_could_have_run_and_half_a_minute_more() {
        use super::cut_off;
        // The function's time of 60 s (and none said): a minute and a half, as it was.
        assert_eq!((cut_off(0, 89_999, 60_000), cut_off(0, 90_000, 60_000), cut_off(0, 90_000, 0)), (false, true, true));
        // 180 s: a sweep 100 s along is still going; one 210 s along was cut off.
        assert_eq!((cut_off(0, 100_000, 180_000), cut_off(0, 209_999, 180_000), cut_off(0, 210_000, 180_000)), (false, false, true));
        // A note from the future (another clock) is going.
        assert!(!cut_off(5_000, 1_000, 180_000));
    }

    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// DynamoDB in memory: items by (partition, key), with the calls the store makes. It answers a page of two items at
    /// a time (so the store pages), leaves some keys of a batch for later the first time, and says it is throttled
    /// once in a while (so the store asks again).
    #[derive(Default)]
    struct FakeDynamo { items: RefCell<BTreeMap<(String, String), Value>>, calls: RefCell<u64>, unprocessed_once: RefCell<bool>,
        /// The call (counted from 1) answered with an error the store does not ask again for.
        fail_at: RefCell<Option<u64>>,
        /// A query's next page said to be empty, more after it (as DynamoDB may answer).
        empty_page_once: RefCell<bool>,
        /// Throttled ever more (every call): what asking again does not get past.
        throttled: RefCell<bool>,
        /// Every batch leaves all but its first key for later.
        unprocessed_always: RefCell<bool> }

    #[async_trait(?Send)]
    impl Http for FakeDynamo {
        async fn send(&self, r: Request) -> io::Result<Response> {
            let n = { let mut c = self.calls.borrow_mut(); *c += 1; *c };
            let op = r.header("x-amz-target").unwrap_or_default().trim_start_matches("DynamoDB_20120810.").to_string();
            let fail = |kind: &str| Ok(Response::new(400, "application/x-amz-json-1.0", json!({ "__type": format!("com.amazonaws.dynamodb.v20120810#{kind}"), "message": kind }).to_string()));
            if n % 13 == 0 || *self.throttled.borrow() { return fail("ThrottlingException") }
            if *self.fail_at.borrow() == Some(n) { return fail("ValidationException") }
            let b: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            let key = |k: &Value| (k["p"]["S"].as_str().unwrap_or_default().to_string(), k["k"]["S"].as_str().unwrap_or_default().to_string());
            let ok = |v: Value| Ok(Response::new(200, "application/x-amz-json-1.0", v.to_string()));
            let mut items = self.items.borrow_mut();
            match op.as_str() {
                "GetItem" => ok(match items.get(&key(&b["Key"])) { Some(i) => json!({ "Item": i }), None => json!({}) }),
                "PutItem" => {
                    let at = key(&b["Item"]);
                    let held = items.get(&at);
                    let allowed = match b["ConditionExpression"].as_str() {
                        Some("attribute_not_exists(k)") => held.is_none(),
                        Some("v = :was") => held.is_some_and(|h| h["v"] == b["ExpressionAttributeValues"][":was"]),
                        _ => true,
                    };
                    if !allowed { return fail("ConditionalCheckFailedException") }
                    items.insert(at, b["Item"].clone());
                    ok(json!({}))
                }
                "DeleteItem" => { items.remove(&key(&b["Key"])); ok(json!({})) }
                "Query" => {
                    let vals = &b["ExpressionAttributeValues"];
                    let (p, prefix) = (vals[":p"]["S"].as_str().unwrap_or_default(), vals[":k"]["S"].as_str().unwrap_or(""));
                    let mut hits: Vec<Value> = items.iter().filter(|((ip, ik), _)| ip == p && ik.starts_with(prefix)).map(|(_, i)| i.clone()).collect();
                    if b["ScanIndexForward"] == false { hits.reverse() }
                    let after = b["ExclusiveStartKey"]["k"]["S"].as_str().map(str::to_string);
                    let forward = b["ScanIndexForward"] != false;
                    let start = after.map(|a| hits.iter().position(|i| i["k"]["S"].as_str() == Some(a.as_str())).map(|x| x + 1).unwrap_or(0)).unwrap_or(0);
                    if self.empty_page_once.replace(false) { return ok(json!({ "Items": [], "LastEvaluatedKey": { "p": { "S": p }, "k": { "S": "" } } })) }
                    let page = b["Limit"].as_u64().unwrap_or(2).min(2) as usize;
                    let shown: Vec<Value> = hits.iter().skip(start).take(page).cloned().collect();
                    let more = start + page < hits.len();
                    let _ = forward;
                    ok(if more { json!({ "Items": shown, "LastEvaluatedKey": { "p": { "S": p }, "k": shown.last().unwrap()["k"] } }) } else { json!({ "Items": shown }) })
                }
                "BatchGetItem" => {
                    let table = b["RequestItems"].as_object().unwrap().keys().next().unwrap().clone();
                    let keys = b["RequestItems"][&table]["Keys"].as_array().cloned().unwrap_or_default();
                    let first = !*self.unprocessed_once.borrow();
                    *self.unprocessed_once.borrow_mut() = true;
                    let (now, later) = if (first || *self.unprocessed_always.borrow()) && keys.len() > 1 { keys.split_at(1) } else { keys.split_at(keys.len()) };
                    let found: Vec<Value> = now.iter().filter_map(|k| items.get(&key(k)).cloned()).collect();
                    ok(json!({ "Responses": { &table: found }, "UnprocessedKeys": if later.is_empty() { json!({}) } else { json!({ &table: { "Keys": later } }) } }))
                }
                other => panic!("a call the store does not make: {other}"),
            }
        }
    }

    fn store(fake: &FakeDynamo) -> Dynamo<'_> { Dynamo { http: fake, table: "superci-plane-t".into(), region: "us-east-1".into(), old: std::cell::Cell::new(None) } }

    /// Lambda taking (or refusing) the function's calls to itself; DynamoDB as above.
    #[derive(Default)]
    struct Lambda { refuse: std::cell::Cell<bool>, called: RefCell<Vec<Request>> }
    #[async_trait(?Send)]
    impl Http for Lambda {
        async fn send(&self, r: Request) -> io::Result<Response> {
            self.called.borrow_mut().push(r);
            Ok(Response::new(if self.refuse.get() { 403 } else { 202 }, "application/json", "{}"))
        }
    }

    #[test]
    fn a_sweep_sooner_than_the_schedules_is_one_call_to_itself_at_a_time() {
        let fake = FakeDynamo::default();
        fake.items.borrow_mut().insert(("layout".into(), "layout".into()), json!({ "p": { "S": "layout" }, "k": { "S": "layout" }, "v": { "S": "kinds" } }));
        let s = store(&fake);
        let (lambda, later) = (Lambda::default(), RefCell::new(vec![]));
        let timer = Scheduled { http: &lambda, store: &s, region: "us-east-1", function: "superci-plane-t", later: &later };
        // Later than half a minute: the schedule's minute does it.
        block_on(timer.wake_in(60_000)).unwrap();
        assert!(lambda.called.borrow().is_empty());
        // Sooner: one call, not waited for, signed, saying when.
        block_on(timer.wake_in(20_000)).unwrap();
        let called = lambda.called.borrow()[0].clone();
        assert_eq!((called.url.as_str(), called.header("x-amz-invocation-type")), ("https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-t/invocations", Some("Event")));
        assert!(called.header("authorization").is_some_and(|a| a.contains("/us-east-1/lambda/aws4_request")));
        let at = serde_json::from_slice::<Value>(&called.body).unwrap()["at"].as_u64().unwrap();
        assert!(at >= now_ms() + 19_000 && at <= now_ms() + 20_000);
        // Another asked for meanwhile: the one on its way does for both.
        block_on(timer.wake_in(1_000)).unwrap();
        assert_eq!(lambda.called.borrow().len(), 1);
        // One noted long ago never came: a call again.
        block_on(s.put(WAKING, (now_ms() - 120_000).to_string())).unwrap();
        block_on(timer.wake_in(1_000)).unwrap();
        assert_eq!(lambda.called.borrow().len(), 2);
        // Refused (a role from before, which may not call itself): its note taken away, the schedule does it.
        block_on(s.delete(WAKING)).unwrap();
        lambda.refuse.set(true);
        block_on(timer.wake_in(1_000)).unwrap();
        assert_eq!(block_on(s.get(WAKING)).unwrap(), None);
        // A function with no name (not on Lambda) calls nothing; what is left for later is kept for after the answer.
        let none = Scheduled { http: &lambda, store: &s, region: "us-east-1", function: "", later: &later };
        block_on(none.wake_in(1_000)).unwrap();
        assert_eq!(lambda.called.borrow().len(), 3);
        assert!(timer.later("start job:1"));
        assert_eq!(*later.borrow(), ["start job:1"]);
        let began = Instant::now();
        block_on(timer.pause(30));
        assert!(began.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn it_keeps_each_kind_of_record_in_a_partition_of_its_own_and_does_what_every_store_does() {
        let fake = FakeDynamo::default();
        fake.items.borrow_mut().insert(("layout".into(), "layout".into()), json!({ "p": { "S": "layout" }, "k": { "S": "layout" }, "v": { "S": "kinds" } }));
        let s = store(&fake);
        block_on(s.put("job:1", "one".into())).unwrap();
        block_on(s.put("job:2", "two".into())).unwrap();
        block_on(s.put("job:3", "three".into())).unwrap();
        block_on(s.put("live:2", String::new())).unwrap();
        assert!(fake.items.borrow().contains_key(&("job".to_string(), "job:1".to_string())) && fake.items.borrow().contains_key(&("live".to_string(), "live:2".to_string())));
        assert_eq!(block_on(s.get("job:2")).unwrap().as_deref(), Some("two"));
        assert_eq!(block_on(s.list("job:")).unwrap().len(), 3, "every page of it");
        assert_eq!(block_on(s.list_last("job:", 2)).unwrap().iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["job:2", "job:3"]);
        assert_eq!(block_on(s.get_many(&["job:3".into(), "job:9".into(), "job:1".into()])).unwrap(), [Some("three".to_string()), None, Some("one".to_string())], "what a batch left for later is asked again");
        assert!(!block_on(s.put_if_absent("job:1", "again".into())).unwrap() && block_on(s.put_if_absent("job:4", "four".into())).unwrap());
        assert!(!block_on(s.swap("job:1", Some("not this"), "x".into())).unwrap() && block_on(s.swap("job:1", Some("one"), "uno".into())).unwrap());
        block_on(s.put_until("rline:t:1", "lines".into(), 1_790_000_000_000)).unwrap();
        assert_eq!(fake.items.borrow()[&("rline".to_string(), "rline:t:1".to_string())]["x"]["N"], "1790000000", "its end in seconds, for the table's time to live");
        block_on(s.delete("job:4")).unwrap();
        assert_eq!(block_on(s.get("job:4")).unwrap(), None);
        assert!(block_on(s.migrate(10)).unwrap(), "nothing of an earlier layout");
    }

    #[test]
    fn records_of_the_layout_before_are_read_where_they_are_and_moved_over_a_little_at_a_time() {
        let fake = FakeDynamo::default();
        let old = |k: &str, v: &str| (("s".to_string(), k.to_string()), json!({ "p": { "S": "s" }, "k": { "S": k }, "v": { "S": v } }));
        fake.items.borrow_mut().extend([old("job:1", "one"), old("job:2", "two"), old("job:3", "three"), old("hello:1", "h")]);
        let s = store(&fake);
        // Read as they are, before anything moved; a write over one goes to its own partition.
        assert_eq!(block_on(s.get("job:2")).unwrap().as_deref(), Some("two"));
        assert_eq!(block_on(s.list("job:")).unwrap().len(), 3);
        assert!(!block_on(s.put_if_absent("job:1", "x".into())).unwrap(), "one in the old partition is there");
        assert!(block_on(s.swap("job:3", Some("three"), "tres".into())).unwrap(), "written in its own partition, the old one gone");
        assert!(fake.items.borrow().contains_key(&("job".to_string(), "job:3".to_string())) && !fake.items.borrow().contains_key(&("s".to_string(), "job:3".to_string())));
        // Moved a few at each sweep, never over what was written since; then the layout says so.
        assert!(!block_on(s.migrate(2)).unwrap());
        while !block_on(s.migrate(2)).unwrap() {}
        assert!(!fake.items.borrow().keys().any(|(p, _)| p == "s"), "nothing left in the old partition");
        assert_eq!(block_on(s.get("job:3")).unwrap().as_deref(), Some("tres"));
        assert_eq!(block_on(s.get("hello:1")).unwrap().as_deref(), Some("h"));
        assert_eq!(fake.items.borrow()[&("layout".to_string(), "layout".to_string())]["v"]["S"], "kinds");
    }

    /// What every store's caller does with the old layout and the new, as one scenario: each step's own error kept,
    /// none stopping the next (what a sweep run of it fails along the way).
    fn every_operation(fake: &FakeDynamo) -> Vec<String> {
        let old = |k: &str, v: &str| (("s".to_string(), k.to_string()), json!({ "p": { "S": "s" }, "k": { "S": k }, "v": { "S": v } }));
        fake.items.borrow_mut().extend([old("job:1", "one"), old("job:2", "two"), old("job:3", "three"), old("hello:1", "h")]);
        fake.items.borrow_mut().insert(("job".into(), "job:5".into()), json!({ "p": { "S": "job" }, "k": { "S": "job:5" }, "v": { "S": "five" } }));
        // (A store each time, as each request has: it asks again where the layout is.)
        let s = || store(fake);
        let mut errors = vec![];
        let mut keep = |r: io::Result<()>| if let Err(e) = r { errors.push(e) };
        keep(block_on(s().get("job:2")).map(|_| ()));
        keep(block_on(s().get("job:9")).map(|_| ()));
        keep(block_on(s().list("job:")).map(|_| ()));
        keep(block_on(s().list_last("job:", 2)).map(|_| ()));
        keep(block_on(s().get_many(&["job:3".into(), "job:9".into(), "job:5".into()])).map(|_| ()));
        keep(block_on(s().get_many(&(0..10).map(|i| format!("job:{i}")).collect::<Vec<_>>())).map(|_| ()));
        keep(block_on(s().put_if_absent("job:1", "x".into())).map(|_| ()));
        keep(block_on(s().put_if_absent("job:6", "six".into())).map(|_| ()));
        keep(block_on(s().swap("job:3", Some("three"), "tres".into())).map(|_| ()));
        keep(block_on(s().swap("job:5", Some("five"), "cinco".into())).map(|_| ()));
        keep(block_on(s().swap("job:7", None, "seven".into())).map(|_| ()));
        // (Not in its own partition, and the old one's value is not the one read: not written.)
        keep(block_on(s().swap("hello:1", Some("not h"), "x".into())).map(|_| ()));
        keep(block_on(s().delete("job:2")));
        keep(block_on(s().put("job:8", "eight".into())));
        keep(block_on(s().put_until("rline:t:1", "lines".into(), 1_790_000_000_000)));
        for _ in 0..4 { keep(block_on(s().migrate(2)).map(|_| ())) }
        keep(block_on(s().list_last("job:", 2)).map(|_| ()));
        errors
    }

    /// Each of DynamoDB's calls in turn answered with an error it is not asked again for: the store says it, as
    /// DynamoDB said it, and the operations after it go on.
    /// (Also with DynamoDB leaving every batch's keys for later but one. The clean runs alone unless
    /// `SUPERCI_SWEEP_FAULTS=1`, each `SUPERCI_SWEEP_SLICE` its share, as the other sweeps.)
    #[test]
    fn the_store_holds_with_each_call_and_store_operation_failing() {
        let (k0, n) = superci_testkit::sweep::slice();
        let faults = std::env::var("SUPERCI_SWEEP_FAULTS").is_ok_and(|v| v == "1");
        for unprocessed in [false, true] {
            let clean = FakeDynamo { unprocessed_always: RefCell::new(unprocessed), ..Default::default() };
            assert_eq!(every_operation(&clean), Vec::<String>::new());
            let calls = if faults { *clean.calls.borrow() } else { 0 };
            for k in (1..=calls).filter(|k| (*k as usize - 1) % n == k0 - 1) {
                let fake = FakeDynamo { unprocessed_always: RefCell::new(unprocessed), ..Default::default() };
                *fake.fail_at.borrow_mut() = Some(k);
                let errors = every_operation(&fake);
                assert!(errors.iter().all(|e| e.starts_with("ValidationException")), "call {k}: {errors:?}");
            }
        }
    }

    /// What DynamoDB may answer that is not a record (an item without its key or value), an empty page with more
    /// after it, a batch with nothing of what was asked, and throttling that does not end.
    #[test]
    fn the_store_reads_what_dynamodb_answers_amiss_and_stops_asking_again() {
        let fake = FakeDynamo::default();
        let s = store(&fake);
        // (Partition "job", no value; "s", no key: neither is a record.)
        fake.items.borrow_mut().insert(("job".into(), "job:0".into()), json!({ "p": { "S": "job" }, "k": { "S": "job:0" } }));
        fake.items.borrow_mut().insert(("s".into(), "job:x".into()), json!({ "p": { "S": "s" }, "v": { "S": "no key" } }));
        assert_eq!(block_on(s.list("job:")).unwrap(), vec![]);
        assert_eq!(block_on(s.get_many(&["job:0".into()])).unwrap(), [None]);
        assert!(!block_on(s.migrate(5)).unwrap() || fake.items.borrow().contains_key(&("layout".to_string(), "layout".to_string())));
        assert_eq!(block_on(s.list_last("job:", 0)).unwrap(), vec![]);
        // A page that says nothing, more after it.
        let fake = FakeDynamo::default();
        fake.items.borrow_mut().insert(("s".into(), "job:1".into()), json!({ "p": { "S": "s" }, "k": { "S": "job:1" }, "v": { "S": "one" } }));
        *fake.empty_page_once.borrow_mut() = true;
        assert!(!block_on(store(&fake).migrate(5)).unwrap(), "more to come");
        // A batch DynamoDB keeps leaving for later: asked eight times, what it gave kept.
        let fake = FakeDynamo::default();
        fake.items.borrow_mut().insert(("layout".into(), "layout".into()), json!({ "p": { "S": "layout" }, "k": { "S": "layout" }, "v": { "S": "kinds" } }));
        for i in 0..10 { fake.items.borrow_mut().insert(("job".into(), format!("job:{i}")), json!({ "p": { "S": "job" }, "k": { "S": format!("job:{i}") }, "v": { "S": "v" } })); }
        *fake.unprocessed_always.borrow_mut() = true;
        let got = block_on(store(&fake).get_many(&(0..10).map(|i| format!("job:{i}")).collect::<Vec<_>>())).unwrap();
        assert_eq!(got.iter().filter(|g| g.is_some()).count(), 10, "the rest read one by one");
        // Throttled every time: asked again ten times, then what DynamoDB said.
        let fake = FakeDynamo::default();
        *fake.throttled.borrow_mut() = true;
        assert!(block_on(store(&fake).get("job:1")).unwrap_err().starts_with("ThrottlingException"));
        assert_eq!(*fake.calls.borrow(), 10);
    }

    /// An address answering each request as given (its first line and headers handed back): a redirect, a header that
    /// is not text, a body cut short.
    fn answering(answer: &'static [u8]) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || for mut c in listener.incoming().flatten() {
            use std::io::{Read, Write};
            let mut head = [0u8; 4096];
            let n = c.read(&mut head).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&head[..n]).to_string());
            let _ = c.write_all(answer);
        });
        (at, rx)
    }

    /// The client as the function uses it: its headers sent as given (a host too, when nothing stands in for the
    /// address), a redirect given back when asked not to follow it, a header that is not text left out; what cannot be
    /// asked, or is answered short, said.
    #[test]
    fn the_client_sends_what_it_is_given_and_says_what_went_wrong() {
        let http = Blocking(agent(10), agent(0));
        let (at, asked) = answering(b"HTTP/1.1 302 Found\r\nlocation: https://logs.example/1\r\nx-odd: \xff\xfe\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let r = block_on(http.send(Request::new("GET", &format!("{at}/log")).with_header("host", "example.com").with_header("x-a", "1").without_following())).unwrap();
        assert_eq!((r.status, r.header("location"), r.header("x-odd")), (302, Some("https://logs.example/1"), None));
        let head = asked.recv().unwrap().to_ascii_lowercase();
        assert!(head.contains("host: example.com") && head.contains("x-a: 1"), "{head}");
        let (short, _) = answering(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\nconnection: close\r\n\r\nshort");
        assert!(block_on(http.send(Request::new("GET", &short))).is_err());
        assert!(block_on(http.send(Request::new("GET", "http://not a host/"))).is_err());
        let closed = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); format!("http://{}", l.local_addr().unwrap()) };
        assert!(block_on(http.send(Request::new("POST", &closed))).unwrap_err().starts_with("POST http://127.0.0.1:"));
    }

    /// A call to itself that cannot be signed (a region no address is made of): not sent, its note taken away.
    #[test]
    fn a_call_to_itself_that_cannot_be_signed_is_left_to_the_schedule() {
        let fake = FakeDynamo::default();
        fake.items.borrow_mut().insert(("layout".into(), "layout".into()), json!({ "p": { "S": "layout" }, "k": { "S": "layout" }, "v": { "S": "kinds" } }));
        let s = store(&fake);
        let (lambda, later) = (Lambda::default(), RefCell::new(vec![]));
        let timer = Scheduled { http: &lambda, store: &s, region: "us east-1", function: "superci-plane-t", later: &later };
        block_on(timer.wake_in(1_000)).unwrap();
        assert!(lambda.called.borrow().is_empty());
        assert_eq!(block_on(s.get(WAKING)).unwrap(), None);
    }
}
