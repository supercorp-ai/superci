//! A control plane in AWS, set up from the dashboard with your AWS sign-in: a role for the function (it may use its
//! own table and parameters, write its logs, and start and stop its own tagged machines), a DynamoDB table, the Lambda
//! function (embedded in this binary) with a public URL for GitHub's webhooks, and a schedule for the sweep. Secrets
//! are SecureString parameters under /superci/<id>/ in SSM Parameter Store.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::executor::block_on;
use serde_json::{json, Value};

use superci_core::aws::{self, Credentials};

use crate::aws::Blocking;
use crate::Result;

const BOOTSTRAP_ZIP: &[u8] = superci_programs_planes::PLANE_AWS;

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

pub fn name(plane_id: &str) -> String { format!("superci-plane-{plane_id}") }

/// The layer store's converter beside it: its function and its role (which may only read and write the layer store's
/// part of the buckets: it reads registries' and machines' blobs, and runs mkfs.erofs on them).
pub fn converter_name(plane_id: &str) -> String { format!("superci-plane-{plane_id}-convert") }

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

/// The converter's: the most every account may give (new accounts may give no more), its disk the most there is
/// (a blob and its EROFS file at once), and the longest a function may run.
const CONVERTER_MEMORY_MB: u32 = 3008;
const CONVERTER_DISK_MB: u32 = 10240;
const CONVERTER_SECONDS: u32 = 900;

/// What a new control plane's deploy does, in order, as the dashboard shows it (the last step is the dashboard's own wait).
pub const DEPLOY_STEPS: [&str; 6] = ["Creating its permissions (an IAM role)", "Creating its storage (a DynamoDB table)", "Uploading it (a Lambda function)", "Giving it its address", "Scheduling its check for waiting jobs and leftover machines, every minute", "Waiting for it to answer"];

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
        "Tags": aws::plane_tags_iam(plane_id) })), &["EntityAlreadyExists"])?;
    let own = json!({ "Version": "2012-10-17", "Statement": [
        { "Sid": "State", "Effect": "Allow", "Action": ["dynamodb:GetItem", "dynamodb:BatchGetItem", "dynamodb:PutItem", "dynamodb:DeleteItem", "dynamodb:Query"], "Resource": table_arn },
        { "Sid": "Secrets", "Effect": "Allow", "Action": "ssm:GetParametersByPath", "Resource": [params_arn, format!("{params_arn}/*")] },
        { "Sid": "Logs", "Effect": "Allow", "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"], "Resource": format!("arn:aws:logs:{region}:{account_id}:log-group:/aws/lambda/{name}:*") },
        // Itself, and nothing else: a sweep sooner than the schedule's minute (a start tried again, jobs waiting for room).
        { "Sid": "WakeItself", "Effect": "Allow", "Action": "lambda:InvokeFunction", "Resource": format!("arn:aws:lambda:{region}:{account_id}:function:{name}") },
        // The layer store's converter, given a layer at a time.
        { "Sid": "Convert", "Effect": "Allow", "Action": "lambda:InvokeFunction", "Resource": format!("arn:aws:lambda:{region}:{account_id}:function:{}", converter_name(plane_id)) },
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
        "Tags": aws::plane_tags_list(plane_id) })), &["ResourceInUseException"])?;
    // Records with an end (`x`, seconds) are removed by DynamoDB itself. Only for tidying, so a refusal stops nothing.
    // (Half a minute at most: then on, as if it were.)
    let mut asked = 0;
    while !ddb("DescribeTable", json!({ "TableName": name })).is_ok_and(|t| t["Table"]["TableStatus"] == "ACTIVE") {
        crate::aws::pause(1000);
        asked += 1;
        if asked == 30 { break }
    }
    let ttl = ddb("DescribeTimeToLive", json!({ "TableName": name })).ok();
    if !ttl.as_ref().is_some_and(|t| ["ENABLED", "ENABLING"].contains(&t["TimeToLiveDescription"]["TimeToLiveStatus"].as_str().unwrap_or(""))) {
        let _ = ddb("UpdateTimeToLive", json!({ "TableName": name, "TimeToLiveSpecification": { "Enabled": true, "AttributeName": "x" } }));
    }

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
        a.lambda("PUT", &format!("/2015-03-31/functions/{name}/configuration"), Some(json!({ "MemorySize": MEMORY_MB, "Timeout": 60 })))?;
    } else {
        create_function(&a, json!({ "FunctionName": name, "Runtime": "provided.al2023", "Architectures": ["arm64"], "Handler": "bootstrap",
            "Role": role_arn, "Code": { "ZipFile": zip }, "Timeout": 60, "MemorySize": MEMORY_MB, "Environment": env, "Description": "SuperCI control plane",
            "Tags": aws::plane_tags_map(plane_id) }))?;
    }
    wait_ready(&a, &name)?;
    deploy_converter(&a, account_id, region, plane_id, &zip)?;

    // Its URL, open to GitHub (the control plane checks every webhook's signature itself).
    step(3);
    let url = match a.lambda("GET", &format!("/2021-10-31/functions/{name}/url"), None) {
        Ok(v) => v["FunctionUrl"].as_str().unwrap_or_default().to_string(),
        Err(_) => a.lambda("POST", &format!("/2021-10-31/functions/{name}/url"), Some(json!({ "AuthType": "NONE" })))?["FunctionUrl"].as_str().unwrap_or_default().to_string(),
    };
    let url = url.trim_end_matches('/').to_string();
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "url", "Action": "lambda:InvokeFunctionUrl", "Principal": "*", "FunctionUrlAuthType": "NONE" }))), &["ResourceConflictException"])?;
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "url-invoke", "Action": "lambda:InvokeFunction", "Principal": "*", "InvokedViaFunctionUrl": true }))), &["ResourceConflictException", "InvalidParameterValueException", "ValidationException"])?;

    // The sweep, every minute (the shortest AWS schedules): jobs waiting for room, machines left idle.
    step(4);
    let events = |op: &str, body: Value| a.target("events", &format!("events.{region}.amazonaws.com"), &format!("AWSEvents.{op}"), "1.1", body);
    let rule_arn = events("PutRule", json!({ "Name": name, "ScheduleExpression": "rate(1 minute)", "State": "ENABLED", "Description": "SuperCI control plane sweep", "Tags": aws::plane_tags_list(plane_id) }))?["RuleArn"].as_str().unwrap_or_default().to_string();
    events("PutTargets", json!({ "Rule": name, "Targets": [{ "Id": "plane", "Arn": format!("arn:aws:lambda:{region}:{account_id}:function:{name}"), "Input": json!({ "superci": "sweep" }).to_string() }] }))?;
    ok_if(a.lambda("POST", &format!("/2015-03-31/functions/{name}/policy"), Some(json!({ "StatementId": "sweep", "Action": "lambda:InvokeFunction", "Principal": "events.amazonaws.com", "SourceArn": rule_arn }))), &["ResourceConflictException"])?;

    // The role's description is how the dashboard finds this control plane again (IAM is global; Lambda is regional).
    a.iam("UpdateRole", json!({ "RoleName": name, "Description": format!("Runs the SuperCI control plane {url} (Lambda, {region}, runs-on {label}).") }))?;
    Ok(Deployed { url })
}

/// The layer store's converter: its role (S3, only the layer store's part of this control plane's buckets, and its
/// logs), and its function, made from the same package (created, or its code and size set again). Async calls are
/// not tried again: the control plane asks again for what did not come.
fn deploy_converter(a: &Api, account_id: &str, region: &str, plane_id: &str, zip: &str) -> Result<()> {
    let name = converter_name(plane_id);
    let trust = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Principal": { "Service": "lambda.amazonaws.com" }, "Action": "sts:AssumeRole" }] });
    ok_if(a.iam("CreateRole", json!({ "RoleName": name, "AssumeRolePolicyDocument": trust.to_string(), "Description": format!("Converts Docker layers for the SuperCI control plane {plane_id} (Lambda, {region})."),
        "Tags": aws::plane_tags_iam(plane_id) })), &["EntityAlreadyExists"])?;
    let own = json!({ "Version": "2012-10-17", "Statement": [
        { "Sid": "LayerStore", "Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:AbortMultipartUpload"], "Resource": format!("arn:aws:s3:::superci-plane-{plane_id}-*/layers/*") },
        { "Sid": "Logs", "Effect": "Allow", "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"], "Resource": format!("arn:aws:logs:{region}:{account_id}:log-group:/aws/lambda/{name}:*") },
    ] });
    a.iam("PutRolePolicy", json!({ "RoleName": name, "PolicyName": "converter", "PolicyDocument": own.to_string() }))?;
    let config = json!({ "MemorySize": CONVERTER_MEMORY_MB, "Timeout": CONVERTER_SECONDS, "EphemeralStorage": { "Size": CONVERTER_DISK_MB }, "Environment": { "Variables": { "SUPERCI_CONVERTER": "1" } } });
    if a.lambda("GET", &format!("/2015-03-31/functions/{name}"), None).is_ok() {
        a.lambda("PUT", &format!("/2015-03-31/functions/{name}/code"), Some(json!({ "ZipFile": zip, "Architectures": ["arm64"] })))?;
        wait_ready(a, &name)?;
        a.lambda("PUT", &format!("/2015-03-31/functions/{name}/configuration"), Some(config))?;
    } else {
        let mut create = json!({ "FunctionName": name, "Runtime": "provided.al2023", "Architectures": ["arm64"], "Handler": "bootstrap", "Role": format!("arn:aws:iam::{account_id}:role/{name}"),
            "Code": { "ZipFile": zip }, "Description": "SuperCI layer store converter", "Tags": aws::plane_tags_map(plane_id) });
        create.as_object_mut().unwrap().extend(config.as_object().unwrap().clone());
        create_function(a, create)?;
    }
    wait_ready(a, &name)?;
    a.lambda("PUT", &format!("/2019-09-25/functions/{name}/event-invoke-config"), Some(json!({ "MaximumRetryAttempts": 0 }))).map(|_| ())
}

/// A function made, asked again while its new role is not yet one Lambda may use (a few seconds, a minute at most).
fn create_function(a: &Api, create: Value) -> Result<()> {
    let mut attempt = 0;
    loop {
        match a.lambda("POST", "/2015-03-31/functions", Some(create.clone())) {
            Ok(_) => return Ok(()),
            Err(e) if e.contains("cannot be assumed") || e.contains("role") => {
                crate::aws::pause((3 + attempt) * 1000);
                attempt += 1;
                if attempt == 12 { return Err(e) }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Deletes the control plane `plane_id` from `region`: its schedule, function, table, parameters, logs and role. Safe
/// to repeat (what is gone already is skipped). Machines it started end by themselves.
pub fn delete(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    delete_in_region(creds, region, plane_id)?;
    delete_role(creds, region, plane_id)?;
    // The function's last lines can reach its log after the log was deleted, which makes the log anew (seen: a
    // log group left in an otherwise empty account). Deleted once more, a moment later.
    crate::aws::pause(3000);
    ok_if(api(creds, region).target("logs", &format!("logs.{region}.amazonaws.com"), "Logs_20140328.DeleteLogGroup", "1.1", json!({ "logGroupName": format!("/aws/lambda/{}", name(plane_id)) })), &["ResourceNotFoundException"]).map(|_| ())
}

/// What a deploy that stopped partway made, removed when its place is given up for another: its parts in the
/// region, its machines' network there, and its role (made first, and in no region: what a deploy refused by its
/// region left behind before the region was asked first). A region the account may not use refuses the asking too:
/// nothing was made there. Returns what could not be removed, in words.
pub fn abandon(creds: &Credentials, region: &str, plane_id: &str) -> Vec<String> {
    let mut left = vec![];
    let refused = |e: &str| aws::refusal(e).is_some();
    if let Err(e) = delete_in_region(creds, region, plane_id) { if !refused(&e) { left.push(format!("what the deploy made in {region} ({e})")) } }
    if let Err(e) = crate::aws::delete_networks(creds, plane_id, &[region]) { if !refused(&e) { left.push(format!("its machines' network in {region} ({e})")) } }
    if let Err(e) = delete_role(creds, region, plane_id) { left.push(format!("the role {} ({e})", name(plane_id))) }
    left
}

fn delete_in_region(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    let a = api(creds, region);
    let name = name(plane_id);
    let gone = ["ResourceNotFoundException", "NoSuchEntity", "ParameterNotFound"];
    let events = |op: &str, body: Value| a.target("events", &format!("events.{region}.amazonaws.com"), &format!("AWSEvents.{op}"), "1.1", body);
    ok_if(events("RemoveTargets", json!({ "Rule": name, "Ids": ["plane"] })), &gone)?;
    ok_if(events("DeleteRule", json!({ "Name": name })), &gone)?;
    ok_if(a.lambda("DELETE", &format!("/2015-03-31/functions/{name}"), None), &gone)?;
    ok_if(a.lambda("DELETE", &format!("/2015-03-31/functions/{}", converter_name(plane_id)), None), &gone)?;
    ok_if(a.target("logs", &format!("logs.{region}.amazonaws.com"), "Logs_20140328.DeleteLogGroup", "1.1", json!({ "logGroupName": format!("/aws/lambda/{}", converter_name(plane_id)) })), &gone)?;
    ok_if(a.target("dynamodb", &format!("dynamodb.{region}.amazonaws.com"), "DynamoDB_20120810.DeleteTable", "1.0", json!({ "TableName": name })), &gone)?;
    // (Ten at a time; twenty rounds at most.)
    let mut rounds = 0;
    loop {
        let names: Vec<String> = secret_names(creds, region, plane_id)?.into_iter().map(|n| format!("/superci/{plane_id}/{n}")).collect();
        if names.is_empty() { break }
        a.ssm("DeleteParameters", json!({ "Names": names }))?;
        rounds += 1;
        if rounds == 20 { break }
    }
    ok_if(a.target("logs", &format!("logs.{region}.amazonaws.com"), "Logs_20140328.DeleteLogGroup", "1.1", json!({ "logGroupName": format!("/aws/lambda/{name}") })), &gone).map(|_| ())
}

fn delete_role(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    let a = api(creds, region);
    let name = name(plane_id);
    let gone = ["ResourceNotFoundException", "NoSuchEntity", "ParameterNotFound"];
    ["control-plane", "runner-machines"].iter().try_for_each(|policy| ok_if(a.iam("DeleteRolePolicy", json!({ "RoleName": name, "PolicyName": policy })), &gone))?;
    ok_if(a.iam("DeleteRolePolicy", json!({ "RoleName": converter_name(plane_id), "PolicyName": "converter" })), &gone)?;
    ok_if(a.iam("DeleteRole", json!({ "RoleName": converter_name(plane_id) })), &gone)?;
    ok_if(a.iam("DeleteRole", json!({ "RoleName": name })), &gone)
}

/// Its AWS runners removed: the machines' policy comes off its role (the control plane's own policy stays).
pub fn delete_runner_policy(creds: &Credentials, region: &str, plane_id: &str) -> Result<()> {
    ok_if(api(creds, region).iam("DeleteRolePolicy", json!({ "RoleName": name(plane_id), "PolicyName": "runner-machines" })), &["NoSuchEntity"]).map(|_| ())
}

fn wait_ready(a: &Api, name: &str) -> Result<()> {
    let mut asked = 0;
    loop {
        let v = a.lambda("GET", &format!("/2015-03-31/functions/{name}/configuration"), None)?;
        let (state, update) = (v["State"].as_str().unwrap_or(""), v["LastUpdateStatus"].as_str().unwrap_or(""));
        if state == "Active" && update != "InProgress" { return Ok(()) }
        if state == "Failed" || update == "Failed" { return Err(format!("the function failed to start: {}", v["StateReason"].as_str().or(v["LastUpdateStatusReason"].as_str()).unwrap_or(""))) }
        crate::aws::pause(2000);
        asked += 1;
        if asked == 30 { return Err("the function did not become ready".into()) }
    }
}

/// A secret for the control plane: a SecureString parameter it reads (encrypted with the account's key for SSM).
pub fn put_secret(creds: &Credentials, region: &str, plane_id: &str, secret: &str, value: &str) -> Result<()> {
    let mut body = json!({ "Name": format!("/superci/{plane_id}/{secret}"), "Value": value, "Type": "SecureString", "Overwrite": true });
    // (A secret kept for runs from a computer may be larger than SSM's standard 4 KB: AWS keeps it as an advanced
    // parameter then, at $0.05 a month.)
    if secret.starts_with("SECRET_") { body["Tier"] = json!("Intelligent-Tiering") }
    api(creds, region).ssm("PutParameter", body).map(|_| ())
}

pub fn delete_secret(creds: &Credentials, region: &str, plane_id: &str, secret: &str) -> Result<()> {
    ok_if(api(creds, region).ssm("DeleteParameter", json!({ "Name": format!("/superci/{plane_id}/{secret}") })), &["ParameterNotFound"])
}

/// The names of all the control plane's secrets (SSM gives them ten at a time).
pub fn secret_names(creds: &Credentials, region: &str, plane_id: &str) -> Result<Vec<String>> {
    let (a, mut names, mut next) = (api(creds, region), vec![], None::<String>);
    loop {
        let mut body = json!({ "Path": format!("/superci/{plane_id}/"), "WithDecryption": false, "MaxResults": 10 });
        if let Some(n) = &next { body["NextToken"] = json!(n) }
        let v = a.ssm("GetParametersByPath", body)?;
        names.extend(v["Parameters"].as_array().into_iter().flatten().filter_map(|p| p["Name"].as_str()?.rsplit('/').next().map(str::to_string)));
        next = v["NextToken"].as_str().map(str::to_string);
        if next.is_none() { return Ok(names) }
    }
}

/// What a control plane says of itself to whoever may read its account (before any key of this computer's is in it):
/// its name and whose jobs it runs (`NAME`, `OWNERS`), none for what is not set.
pub fn about(creds: &Credentials, region: &str, plane_id: &str) -> Result<(Option<String>, Option<String>)> {
    let names: Vec<String> = ["NAME", "OWNERS"].iter().map(|n| format!("/superci/{plane_id}/{n}")).collect();
    let v = api(creds, region).ssm("GetParameters", json!({ "Names": names, "WithDecryption": true }))?;
    let get = |n: &str| v["Parameters"].as_array().into_iter().flatten().find(|p| p["Name"].as_str().is_some_and(|x| x.ends_with(&format!("/{n}")))).and_then(|p| p["Value"].as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    Ok((get("NAME"), get("OWNERS")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::clouds::{self, answer, aws_error, with};

    const LAMBDA: &str = "/lambda.us-east-1.amazonaws.com/2015-03-31/functions";

    fn creds() -> Credentials { Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, expires_at_ms: 0 } }

    /// An account in us-east-1 as a test has it: the control plane `p1`'s functions there already (`exists`) or not,
    /// every call going through, but for what a call's target, action or address starts with in `given`: answered as
    /// given there.
    fn account(exists: bool, given: Vec<(String, Vec<u8>)>) -> String {
        clouds::serve_aws(move |what, _| {
            if let Some((_, a)) = given.iter().find(|(p, _)| what.starts_with(p.as_str())) { return a.clone() }
            match what {
                w if w.starts_with("GET ") && w.ends_with("/configuration") => answer(200, r#"{"State":"Active","LastUpdateStatus":"Successful"}"#),
                w if w.starts_with(&format!("GET {LAMBDA}/")) && !exists => aws_error("ResourceNotFoundException"),
                w if w.contains("/url") => answer(200, r#"{"FunctionUrl":"https://p1.lambda-url.us-east-1.on.aws/"}"#),
                "DynamoDB_20120810.DescribeTable" => answer(200, r#"{"Table":{"TableStatus":"ACTIVE"}}"#),
                "DynamoDB_20120810.DescribeTimeToLive" => answer(200, r#"{"TimeToLiveDescription":{"TimeToLiveStatus":"ENABLED"}}"#),
                "AWSEvents.PutRule" => answer(200, r#"{"RuleArn":"arn:aws:events:us-east-1:111111111111:rule/superci-plane-p1"}"#),
                "AmazonSSM.GetParametersByPath" => answer(200, r#"{"Parameters":[]}"#),
                _ => answer(200, "{}"),
            }
        })
    }

    fn deploy(at: &str) -> Result<Deployed> { with(at, || deploy_with(&creds(), "111111111111", "us-east-1", "p1", "superci", false, &|_| {})) }

    fn given(what: &str, a: Vec<u8>) -> Vec<(String, Vec<u8>)> { vec![(what.to_string(), a)] }

    /// A control plane updated: its function and its converter's each given their code, waited for and sized; each
    /// step refused is an error.
    #[test]
    fn an_update_fails_at_whichever_step_lambda_refuses() {
        assert_eq!(deploy(&account(true, vec![])).ok().map(|d| d.url).as_deref(), Some("https://p1.lambda-url.us-east-1.on.aws"));
        for step in [format!("PUT {LAMBDA}/superci-plane-p1/code"), format!("GET {LAMBDA}/superci-plane-p1/configuration"), format!("PUT {LAMBDA}/superci-plane-p1/configuration"),
            format!("PUT {LAMBDA}/superci-plane-p1-convert/code"), format!("GET {LAMBDA}/superci-plane-p1-convert/configuration"), format!("PUT {LAMBDA}/superci-plane-p1-convert/configuration")] {
            assert!(deploy(&account(true, given(&step, aws_error("ServiceException")))).is_err(), "{step}");
        }
    }

    /// A new control plane whose table never says it is active goes on all the same; its function, refused for its
    /// new role every time, is not made: the refusal is said after a minute of asking again. A function that never
    /// becomes ready is said as that.
    #[test]
    fn a_new_function_whose_role_lambda_never_takes_or_that_never_becomes_ready_is_an_error() {
        let mut slow = given("DynamoDB_20120810.DescribeTable", answer(200, r#"{"Table":{"TableStatus":"CREATING"}}"#));
        slow.push((format!("POST {LAMBDA}"), answer(400, r#"{"Type":"User","message":"The role defined for the function cannot be assumed by Lambda."}"#)));
        assert!(deploy(&account(false, slow)).err().unwrap().contains("cannot be assumed"));
        assert_eq!(deploy(&account(true, given("GET ", answer(200, r#"{"State":"Pending"}"#)))).err().unwrap(), "the function did not become ready");
        // Refused for another reason: said at once. A new function that cannot be read once made: an error.
        assert!(deploy(&account(false, given(&format!("POST {LAMBDA}"), aws_error("CodeStorageExceededException")))).is_err());
        assert!(deploy(&account(false, given(&format!("GET {LAMBDA}/superci-plane-p1/configuration"), aws_error("ServiceException")))).is_err());
    }

    /// Deleting: parameters that keep coming back are deleted twenty rounds, then the rest goes on; a parameter
    /// without its name is none.
    #[test]
    fn parameters_that_keep_coming_back_are_deleted_twenty_times_then_left() {
        let at = account(true, given("AmazonSSM.GetParametersByPath", answer(200, r#"{"Parameters":[{"Name":"/superci/p1/KEY"},{"Type":"SecureString"}]}"#)));
        with(&at, || {
            assert_eq!(secret_names(&creds(), "us-east-1", "p1").unwrap(), ["KEY"]);
            delete_in_region(&creds(), "us-east-1", "p1").unwrap();
        });
        // A role whose policies cannot be taken off stays.
        with(&account(true, given("DeleteRolePolicy", aws_error("query:ServiceFailure"))), || assert!(delete_role(&creds(), "us-east-1", "p1").unwrap_err().starts_with("ServiceFailure")));
    }
}
