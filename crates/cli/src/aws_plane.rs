//! A control plane in AWS, set up from the dashboard with your AWS sign-in: a role for the function (it may use its
//! own table and parameters, write its logs, and start and stop its own tagged machines), a DynamoDB table, the Lambda
//! function (embedded in this binary) with a public URL for GitHub's webhooks, and a schedule for the sweep. Secrets
//! are SecureString parameters under /superci/<id>/ in SSM Parameter Store.
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::executor::block_on;
use serde_json::{json, Value};

use superci_core::aws::{self, Credentials};

use crate::aws::Blocking;
use crate::Result;

const BOOTSTRAP_ZIP: &[u8] = include_bytes!(env!("SUPERCI_PLANE_AWS"));

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

pub fn name(plane_id: &str) -> String { format!("superci-plane-{plane_id}") }

struct Api<'a> { http: Blocking, creds: &'a Credentials, region: String }

impl Api<'_> {
    fn target(&self, service: &str, host: &str, target: &str, version: &str, body: Value) -> Result<Value> {
        block_on(aws::json_call(&self.http, "POST", &format!("https://{host}/"), &self.region, service, Some(target), &format!("application/x-amz-json-{version}"), Some(&body), self.creds, now_ms()))
    }
    fn lambda(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        block_on(aws::json_call(&self.http, method, &format!("https://lambda.{}.amazonaws.com{path}", self.region), &self.region, "lambda", None, "application/json", body.as_ref(), self.creds, now_ms()))
    }
    fn iam(&self, action: &str, params: Value) -> Result<String> { block_on(aws::iam(&self.http, self.creds, action, params, now_ms())) }
    fn ssm(&self, op: &str, body: Value) -> Result<Value> { self.target("ssm", &format!("ssm.{}.amazonaws.com", self.region), &format!("AmazonSSM.{op}"), "1.1", body) }
}

fn api<'a>(creds: &'a Credentials, region: &str) -> Api<'a> { Api { http: Blocking::new(), creds, region: region.to_string() } }

fn ok_if(r: Result<impl Sized>, benign: &[&str]) -> Result<()> {
    match r { Ok(_) => Ok(()), Err(e) if benign.iter().any(|b| e.starts_with(b)) => Ok(()), Err(e) => Err(e) }
}

pub struct Deployed { pub url: String }

/// The function's memory, and so its share of a CPU (Lambda gives a whole one at 1769 MB): at 1024 MB it starts and
/// answers several times faster than at 256 MB, and is paid by the millisecond, so it costs about the same.
const MEMORY_MB: u32 = 1024;

/// What a new control plane's deploy does, in order, as the dashboard shows it (the last step is the dashboard's own wait).
pub const DEPLOY_STEPS: [&str; 6] = ["Creating its permissions (an IAM role)", "Creating its storage (a DynamoDB table)", "Uploading it (a Lambda function)", "Giving it its address", "Scheduling its check for waiting jobs and leftover machines, every 2 minutes", "Waiting for it to answer"];

/// Sets up (or updates) the control plane `plane_id` in `region`. Safe to repeat.
/// The same, telling `step` which of [`DEPLOY_STEPS`] it has started (0 to 4).
/// `runners`: whether it starts machines in this account (its role gets the machines' policy and its network); off
/// once AWS runners were removed.
pub fn deploy_with(creds: &Credentials, account_id: &str, region: &str, plane_id: &str, label: &str, runners: bool, step: &dyn Fn(usize)) -> Result<Deployed> {
    step(0);
    let a = api(creds, region);
    let name = name(plane_id);
    let table_arn = format!("arn:aws:dynamodb:{region}:{account_id}:table/{name}");
    let params_arn = format!("arn:aws:ssm:{region}:{account_id}:parameter/superci/{plane_id}");

    // The function's role: its own table and parameters, its logs, and its machines (the same policy a role for
    // another cloud's control plane gets).
    let trust = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Principal": { "Service": "lambda.amazonaws.com" }, "Action": "sts:AssumeRole" }] });
    ok_if(a.iam("CreateRole", json!({ "RoleName": name, "AssumeRolePolicyDocument": trust.to_string(), "Description": format!("Runs a SuperCI control plane (Lambda, {region})."),
        "Tags": { "member": [{ "Key": "superci-plane", "Value": plane_id }] } })), &["EntityAlreadyExists"])?;
    let own = json!({ "Version": "2012-10-17", "Statement": [
        { "Sid": "State", "Effect": "Allow", "Action": ["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:DeleteItem", "dynamodb:Query"], "Resource": table_arn },
        { "Sid": "Secrets", "Effect": "Allow", "Action": "ssm:GetParametersByPath", "Resource": [params_arn, format!("{params_arn}/*")] },
        { "Sid": "Logs", "Effect": "Allow", "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"], "Resource": format!("arn:aws:logs:{region}:{account_id}:log-group:/aws/lambda/{name}:*") },
    ] });
    a.iam("PutRolePolicy", json!({ "RoleName": name, "PolicyName": "control-plane", "PolicyDocument": own.to_string() }))?;
    if runners {
        a.iam("PutRolePolicy", json!({ "RoleName": name, "PolicyName": "runner-machines", "PolicyDocument": aws::runner_policy(account_id, plane_id).to_string() }))?;
        ok_if(a.iam("CreateServiceLinkedRole", json!({ "AWSServiceName": "spot.amazonaws.com" })), &["InvalidInput", "EntityAlreadyExists"])?;
        // Its machines' own network here (other regions it uses get theirs when they are added, or with Review).
        crate::aws::make_networks(creds, plane_id, &[region.to_string()])?;
    }

    // State.
    step(1);
    let ddb = |op: &str, body: Value| a.target("dynamodb", &format!("dynamodb.{region}.amazonaws.com"), &format!("DynamoDB_20120810.{op}"), "1.0", body);
    ok_if(ddb("CreateTable", json!({ "TableName": name, "BillingMode": "PAY_PER_REQUEST",
        "AttributeDefinitions": [{ "AttributeName": "p", "AttributeType": "S" }, { "AttributeName": "k", "AttributeType": "S" }],
        "KeySchema": [{ "AttributeName": "p", "KeyType": "HASH" }, { "AttributeName": "k", "KeyType": "RANGE" }],
        "Tags": [{ "Key": "superci-plane", "Value": plane_id }] })), &["ResourceInUseException"])?;

    // The function: created, or its code replaced.
    step(2);
    let role_arn = format!("arn:aws:iam::{account_id}:role/{name}");
    let env = json!({ "Variables": { "PLANE_ID": plane_id, "LABEL": label, "TABLE": name, "RUNNER_REGION": region } });
    let zip = STANDARD.encode(BOOTSTRAP_ZIP);
    let exists = a.lambda("GET", &format!("/2015-03-31/functions/{name}"), None).is_ok();
    if exists {
        a.lambda("PUT", &format!("/2015-03-31/functions/{name}/code"), Some(json!({ "ZipFile": zip, "Architectures": ["arm64"] })))?;
        // Its size as set now (one set up before had a smaller one: slower to start and to answer).
        wait_ready(&a, &name)?;
        a.lambda("PUT", &format!("/2015-03-31/functions/{name}/configuration"), Some(json!({ "MemorySize": MEMORY_MB, "Timeout": 30 })))?;
    } else {
        // A new role takes a few seconds before Lambda may use it.
        let mut last = String::new();
        for attempt in 0..12 {
            match a.lambda("POST", "/2015-03-31/functions", Some(json!({ "FunctionName": name, "Runtime": "provided.al2023", "Architectures": ["arm64"], "Handler": "bootstrap",
                "Role": role_arn, "Code": { "ZipFile": zip }, "Timeout": 30, "MemorySize": MEMORY_MB, "Environment": env, "Description": "SuperCI control plane",
                "Tags": { "superci-plane": plane_id } }))) {
                Ok(_) => { last.clear(); break }
                Err(e) if e.contains("cannot be assumed") || e.contains("role") => { last = e; std::thread::sleep(Duration::from_secs(3 + attempt)) }
                Err(e) => return Err(e),
            }
        }
        if !last.is_empty() { return Err(last) }
    }
    wait_ready(&a, &name)?;

    // Its URL, open to GitHub (the control plane checks every webhook's signature itself).
    step(3);
    let url = match a.lambda("GET", &format!("/2021-10-31/functions/{name}/url"), None) {
        Ok(v) => v["FunctionUrl"].as_str().unwrap_or_default().to_string(),
        Err(_) => a.lambda("POST", &format!("/2021-10-31/functions/{name}/url"), Some(json!({ "AuthType": "NONE" })))?["FunctionUrl"].as_str().unwrap_or_default().to_string(),
    };
    let url = url.trim_end_matches('/').to_string();
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "url", "Action": "lambda:InvokeFunctionUrl", "Principal": "*", "FunctionUrlAuthType": "NONE" }))), &["ResourceConflictException"])?;
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "url-invoke", "Action": "lambda:InvokeFunction", "Principal": "*", "InvokedViaFunctionUrl": true }))), &["ResourceConflictException", "InvalidParameterValueException", "ValidationException"])?;

    // The sweep, every two minutes.
    step(4);
    let events = |op: &str, body: Value| a.target("events", &format!("events.{region}.amazonaws.com"), &format!("AWSEvents.{op}"), "1.1", body);
    let rule_arn = events("PutRule", json!({ "Name": name, "ScheduleExpression": "rate(2 minutes)", "State": "ENABLED", "Description": "SuperCI control plane sweep" }))?["RuleArn"].as_str().unwrap_or_default().to_string();
    events("PutTargets", json!({ "Rule": name, "Targets": [{ "Id": "plane", "Arn": format!("arn:aws:lambda:{region}:{account_id}:function:{name}"), "Input": json!({ "superci": "sweep" }).to_string() }] }))?;
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "sweep", "Action": "lambda:InvokeFunction", "Principal": "events.amazonaws.com", "SourceArn": rule_arn }))), &["ResourceConflictException"])?;

    // The role's description is how the dashboard finds this control plane again (IAM is global; Lambda is regional).
    a.iam("UpdateRole", json!({ "RoleName": name, "Description": format!("Runs the SuperCI control plane {url} (Lambda, {region}, runs-on {label}).") }))?;
    Ok(Deployed { url })
}

/// How long what a job's tests left is kept, in days.
pub const KEPT_DAYS: u32 = 30;

/// Lets the control plane keep jobs' files: its bucket (private; what is in it goes after [`KEPT_DAYS`]), and its
/// role's leave to put files there and read them. Safe to repeat.
pub fn keep_files(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    let (a, bucket) = (api(creds, region), aws::bucket(plane_id));
    block_on(aws::make_bucket(&a.http, &bucket, region, KEPT_DAYS, creds, now_ms()))?;
    let policy = json!({ "Version": "2012-10-17", "Statement": [{ "Sid": "KeptFiles", "Effect": "Allow", "Action": ["s3:PutObject", "s3:GetObject"], "Resource": format!("arn:aws:s3:::{bucket}/*") }] });
    a.iam("PutRolePolicy", json!({ "RoleName": name(plane_id), "PolicyName": "kept-files", "PolicyDocument": policy.to_string() }))?;
    Ok(())
}

/// Deletes the control plane `plane_id` from `region`: its schedule, function, table, parameters, logs and role. Safe
/// to repeat (what is gone already is skipped). Machines it started end by themselves.
pub fn delete(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    let a = api(creds, region);
    let name = name(plane_id);
    let gone = ["ResourceNotFoundException", "NoSuchEntity", "ParameterNotFound"];
    let events = |op: &str, body: Value| a.target("events", &format!("events.{region}.amazonaws.com"), &format!("AWSEvents.{op}"), "1.1", body);
    ok_if(events("RemoveTargets", json!({ "Rule": name, "Ids": ["plane"] })), &gone)?;
    ok_if(events("DeleteRule", json!({ "Name": name })), &gone)?;
    ok_if(a.lambda("DELETE", &format!("/2015-03-31/functions/{name}"), None), &gone)?;
    ok_if(a.target("dynamodb", &format!("dynamodb.{region}.amazonaws.com"), "DynamoDB_20120810.DeleteTable", "1.0", json!({ "TableName": name })), &gone)?;
    for _ in 0..20 {
        let names: Vec<String> = secret_names(creds, region, plane_id)?.into_iter().map(|n| format!("/superci/{plane_id}/{n}")).collect();
        if names.is_empty() { break }
        a.ssm("DeleteParameters", json!({ "Names": names }))?;
    }
    ok_if(a.target("logs", &format!("logs.{region}.amazonaws.com"), "Logs_20140328.DeleteLogGroup", "1.1", json!({ "logGroupName": format!("/aws/lambda/{name}") })), &gone)?;
    // The files it kept, with their bucket (none, when keeping was never switched on).
    block_on(aws::delete_bucket(&a.http, &aws::bucket(plane_id), region, creds, &now_ms))?;
    for policy in ["control-plane", "runner-machines", "kept-files"] { ok_if(a.iam("DeleteRolePolicy", json!({ "RoleName": name, "PolicyName": policy })), &gone)?; }
    ok_if(a.iam("DeleteRole", json!({ "RoleName": name })), &gone)
}

/// Its AWS runners removed: the machines' policy comes off its role (the control plane's own policy stays).
pub fn delete_runner_policy(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    ok_if(api(creds, region).iam("DeleteRolePolicy", json!({ "RoleName": name(plane_id), "PolicyName": "runner-machines" })), &["NoSuchEntity"]).map(|_| ())
}

fn wait_ready(a: &Api, name: &str) -> Result<()> {
    for _ in 0..30 {
        let v = a.lambda("GET", &format!("/2015-03-31/functions/{name}/configuration"), None)?;
        let (state, update) = (v["State"].as_str().unwrap_or(""), v["LastUpdateStatus"].as_str().unwrap_or(""));
        if state == "Active" && update != "InProgress" { return Ok(()) }
        if state == "Failed" || update == "Failed" { return Err(format!("the function failed to start: {}", v["StateReason"].as_str().or(v["LastUpdateStatusReason"].as_str()).unwrap_or(""))) }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("the function did not become ready".into())
}

/// A secret for the control plane: a SecureString parameter it reads (encrypted with the account's key for SSM).
pub fn put_secret(creds: &Credentials, region: &str, plane_id: &str, secret: &str, value: &str) -> Result<()> {
    api(creds, region).ssm("PutParameter", json!({ "Name": format!("/superci/{plane_id}/{secret}"), "Value": value, "Type": "SecureString", "Overwrite": true })).map(|_| ())
}

pub fn delete_secret(creds: &Credentials, region: &str, plane_id: &str, secret: &str) -> Result<()> {
    ok_if(api(creds, region).ssm("DeleteParameter", json!({ "Name": format!("/superci/{plane_id}/{secret}") })), &["ParameterNotFound"])
}

pub fn secret_names(creds: &Credentials, region: &str, plane_id: &str) -> Result<Vec<String>> {
    let v = api(creds, region).ssm("GetParametersByPath", json!({ "Path": format!("/superci/{plane_id}/"), "WithDecryption": false, "MaxResults": 10 }))?;
    Ok(v["Parameters"].as_array().into_iter().flatten().filter_map(|p| p["Name"].as_str()?.rsplit('/').next().map(str::to_string)).collect())
}
