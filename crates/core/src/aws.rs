//! AWS for a control plane without an SDK or stored keys: the control plane is an OpenID Connect identity provider, the user's account
//! trusts it through a role (made by the dashboard), STS AssumeRoleWithWebIdentity gives short-lived credentials,
//! and EC2's Query API calls are signed here (Signature Version 4).
use serde::{Deserialize, Serialize};

use crate::crypto::{b64, hex, hmac_sha256, sha256_hex};
use crate::io::{Http, Request, Result};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub expires_at_ms: u64,
}

/// An ISO 8601 UTC time (`2026-09-30T16:11:43Z`, fractions allowed) as milliseconds since 1970.
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000) as u64)
}

/// RFC 3986 percent-encoding, as SigV4 requires (unreserved characters stay).
pub fn rfc3986(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// `amz_date` (YYYYMMDD'T'HHMMSS'Z') from milliseconds since the epoch.
pub fn amz_date(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Signature Version 4: the request's headers plus `authorization` (and the session token when there is one).
pub fn sigv4(method: &str, url: &str, headers: &[(String, String)], body: &[u8], creds: &Credentials, region: &str, service: &str, amz_date: &str) -> Result<Vec<(String, String)>> {
    let u = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = match u.port() { Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")), None => u.host_str().unwrap_or("").to_string() };
    let mut all: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    all.push(("host".into(), host));
    all.push(("x-amz-date".into(), amz_date.into()));
    if let Some(t) = &creds.session_token { all.push(("x-amz-security-token".into(), t.clone())); }
    all.sort();
    let signed = all.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let mut query: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (rfc3986(&k), rfc3986(&v))).collect();
    query.sort();
    let canonical = [
        method.to_string(),
        if u.path().is_empty() { "/".into() } else { u.path().to_string() },
        query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
        all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>(),
        signed.clone(),
        sha256_hex(body),
    ].join("\n");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let mut key = hmac_sha256(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes());
    for part in [region, service, "aws4_request"] { key = hmac_sha256(&key, part.as_bytes()); }
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    let mut out = all;
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", creds.access_key_id)));
    Ok(out)
}

/// Query API parameters from nested JSON: arrays become `.1`, `.2`, … and objects `.Key`.
pub fn query_params(value: &serde_json::Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => for (k, v) in map { query_params(v, &if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, out) },
        serde_json::Value::Array(items) => for (i, v) in items.iter().enumerate() { query_params(v, &format!("{prefix}.{}", i + 1), out) },
        serde_json::Value::Null => {}
        serde_json::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// The text of the first `<name>…</name>` in an XML response.
pub fn xml_tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(&xml[start..start + end])
}

fn aws_error(xml: &str, status: u16) -> String {
    format!("{}: {}", xml_tag(xml, "Code").unwrap_or(&format!("HTTP {status}")), xml_tag(xml, "Message").unwrap_or(&xml.chars().take(200).collect::<String>()))
}

/// Short-lived credentials for the connected role from a token the control plane signed (this STS call is not signed).
pub async fn assume_role_with_web_identity(http: &dyn Http, region: &str, role_arn: &str, token: &str, session: &str, now_ms: u64) -> Result<Credentials> {
    let mut url = url::Url::parse(&format!("https://sts.{region}.amazonaws.com/")).map_err(|e| e.to_string())?;
    url.query_pairs_mut().extend_pairs([("Action", "AssumeRoleWithWebIdentity"), ("Version", "2011-06-15"), ("RoleArn", role_arn), ("RoleSessionName", session), ("WebIdentityToken", token), ("DurationSeconds", "3600")]);
    let r = http.send(Request::new("GET", url.as_str())).await?;
    let xml = r.body_text();
    if r.status >= 300 { return Err(aws_error(&xml, r.status)); }
    let field = |n: &str| xml_tag(&xml, n).map(str::to_string).ok_or_else(|| format!("STS response without {n}"));
    Ok(Credentials { access_key_id: field("AccessKeyId")?, secret_access_key: field("SecretAccessKey")?, session_token: xml_tag(&xml, "SessionToken").map(str::to_string), expires_at_ms: now_ms + 3_300_000 })
}

/// One signed call to an AWS Query API (EC2, IAM, STS).
pub async fn query(http: &dyn Http, url: &str, region: &str, service: &str, version: &str, creds: &Credentials, action: &str, params: serde_json::Value, now_ms: u64) -> Result<String> {
    let mut fields = vec![("Action".to_string(), action.to_string()), ("Version".to_string(), version.to_string())];
    query_params(&params, "", &mut fields);
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(&fields).finish();
    let headers = sigv4("POST", url, &[("content-type".into(), "application/x-www-form-urlencoded; charset=utf-8".into())], body.as_bytes(), creds, region, service, &amz_date(now_ms))?;
    let mut req = Request::new("POST", url).with_body(body);
    req.headers = headers;
    let r = http.send(req).await?;
    let xml = r.body_text();
    if r.status >= 300 { return Err(aws_error(&xml, r.status)); }
    Ok(xml)
}

/// One signed call to an AWS JSON or REST API (DynamoDB, SSM, EventBridge with an `X-Amz-Target`; Lambda by path).
pub async fn json_call(http: &dyn Http, method: &str, url: &str, region: &str, service: &str, target: Option<&str>, content_type: &str, body: Option<&serde_json::Value>, creds: &Credentials, now_ms: u64) -> Result<serde_json::Value> {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let mut headers: Vec<(String, String)> = vec![("content-type".into(), content_type.into())];
    if let Some(t) = target { headers.push(("x-amz-target".into(), t.into())); }
    let signed = sigv4(method, url, &headers, bytes.as_bytes(), creds, region, service, &amz_date(now_ms))?;
    let mut req = Request::new(method, url).with_body(bytes);
    req.headers = signed;
    let r = http.send(req).await?;
    let text = r.body_text();
    let v: serde_json::Value = if text.trim().is_empty() { serde_json::Value::Null } else { serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone())) };
    if r.status >= 300 {
        // The error's code: `__type`, or the x-amzn-errortype header (Lambda's body has `Type`, which is only who is
        // at fault: "User" or "Service").
        let kind = v["__type"].as_str().or_else(|| r.header("x-amzn-errortype")).or(v["Type"].as_str()).unwrap_or("").rsplit('#').next().unwrap_or("").split(':').next().unwrap_or("").to_string();
        let message = v["message"].as_str().or(v["Message"].as_str()).map(str::to_string).unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("{}: {message}", if kind.is_empty() { format!("HTTP {}", r.status) } else { kind }));
    }
    Ok(v)
}

/// The bucket a control plane in AWS keeps jobs' files in: named after the control plane, like its table and function.
pub fn bucket(plane_id: &str) -> String { format!("superci-plane-{plane_id}") }

fn s3_host(bucket: &str, region: &str) -> String { format!("{bucket}.s3.{region}.amazonaws.com") }

/// A link that lets its holder send (`PUT`) or fetch (`GET`) one object for `secs` seconds and do nothing else (S3's
/// presigned URL): a job's machine sends a file with it, and a browser or the CLI fetches one, without AWS
/// credentials of their own and without the file passing through the control plane.
pub fn s3_link(method: &str, bucket: &str, region: &str, key: &str, creds: &Credentials, secs: u32, now_ms: u64) -> String {
    presigned(method, &s3_host(bucket, region), region, key, creds, secs, &amz_date(now_ms))
}

fn presigned(method: &str, host: &str, region: &str, key: &str, creds: &Credentials, secs: u32, amz_date: &str) -> String {
    let path = format!("/{}", key.split('/').map(rfc3986).collect::<Vec<_>>().join("/"));
    let scope = format!("{}/{region}/s3/aws4_request", &amz_date[..8]);
    let mut query = vec![("X-Amz-Algorithm".to_string(), "AWS4-HMAC-SHA256".to_string()), ("X-Amz-Credential".into(), format!("{}/{scope}", creds.access_key_id)),
        ("X-Amz-Date".into(), amz_date.to_string()), ("X-Amz-Expires".into(), secs.to_string()), ("X-Amz-SignedHeaders".into(), "host".into())];
    if let Some(t) = &creds.session_token { query.push(("X-Amz-Security-Token".into(), t.clone())) }
    let mut query: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", rfc3986(k), rfc3986(v))).collect();
    query.sort();
    let query = query.join("&");
    let canonical = format!("{method}\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let mut signing = hmac_sha256(format!("AWS4{}", creds.secret_access_key).as_bytes(), amz_date[..8].as_bytes());
    for part in [region, "s3", "aws4_request"] { signing = hmac_sha256(&signing, part.as_bytes()); }
    format!("https://{host}{path}?{query}&X-Amz-Signature={}", hex(&hmac_sha256(&signing, to_sign.as_bytes())))
}

/// One signed S3 call on a bucket (`path_and_query` starts with `/`): the answer's status and text.
pub async fn s3(http: &dyn Http, method: &str, bucket: &str, region: &str, path_and_query: &str, body: &[u8], creds: &Credentials, now_ms: u64) -> Result<(u16, String)> {
    let url = format!("https://{}{path_and_query}", s3_host(bucket, region));
    let mut headers = vec![("x-amz-content-sha256".to_string(), sha256_hex(body))];
    // S3 asks for a checksum of what sets a bucket's rules.
    if !body.is_empty() {
        headers.push(("x-amz-sdk-checksum-algorithm".into(), "SHA256".into()));
        headers.push(("x-amz-checksum-sha256".into(), b64(&crate::crypto::sha256(body))));
    }
    let signed = sigv4(method, &url, &headers, body, creds, region, "s3", &amz_date(now_ms))?;
    let mut req = Request::new(method, &url).with_body(body.to_vec());
    req.headers = signed;
    let r = http.send(req).await?;
    Ok((r.status, r.body_text()))
}

/// Makes the bucket (private, as every new bucket is) when it is not there, and has what is in it removed after `days`.
pub async fn make_bucket(http: &dyn Http, bucket: &str, region: &str, days: u32, creds: &Credentials, now_ms: u64) -> Result<()> {
    let place = if region == "us-east-1" { String::new() } else { format!("<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>") };
    let (status, text) = s3(http, "PUT", bucket, region, "/", place.as_bytes(), creds, now_ms).await?;
    if status >= 300 && xml_tag(&text, "Code") != Some("BucketAlreadyOwnedByYou") { return Err(aws_error(&text, status)) }
    let rule = format!("<LifecycleConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Rule><ID>superci</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>{days}</Days></Expiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>");
    let (status, text) = s3(http, "PUT", bucket, region, "/?lifecycle=", rule.as_bytes(), creds, now_ms).await?;
    if status >= 300 { return Err(aws_error(&text, status)) }
    Ok(())
}

/// Removes the bucket and everything in it. A bucket that is not there is no error.
pub async fn delete_bucket(http: &dyn Http, bucket: &str, region: &str, creds: &Credentials, now: &dyn Fn() -> u64) -> Result<()> {
    loop {
        let (status, text) = s3(http, "GET", bucket, region, "/?list-type=2&max-keys=1000", b"", creds, now()).await?;
        if status == 404 { return Ok(()) }
        if status >= 300 { return Err(aws_error(&text, status)) }
        let keys: Vec<&str> = text.split("<Key>").skip(1).filter_map(|k| k.split("</Key>").next()).collect();
        if keys.is_empty() { break }
        for key in keys {
            let key = key.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'");
            let path = format!("/{}", key.split('/').map(rfc3986).collect::<Vec<_>>().join("/"));
            let (status, text) = s3(http, "DELETE", bucket, region, &path, b"", creds, now()).await?;
            if status >= 300 && status != 404 { return Err(aws_error(&text, status)) }
        }
    }
    let (status, text) = s3(http, "DELETE", bucket, region, "/", b"", creds, now()).await?;
    if status >= 300 && status != 404 { return Err(aws_error(&text, status)) }
    Ok(())
}

/// One signed EC2 Query API call.
pub async fn ec2(http: &dyn Http, region: &str, creds: &Credentials, action: &str, params: serde_json::Value, now_ms: u64) -> Result<String> {
    query(http, &format!("https://ec2.{region}.amazonaws.com/"), region, "ec2", "2016-11-15", creds, action, params, now_ms).await
}

/// One signed IAM call (a global service, signed for us-east-1).
pub async fn iam(http: &dyn Http, creds: &Credentials, action: &str, params: serde_json::Value, now_ms: u64) -> Result<String> {
    query(http, "https://iam.amazonaws.com/", "us-east-1", "iam", "2010-05-08", creds, action, params, now_ms).await
}

/// The role a control plane assumes, trusted only for tokens this control plane signs (its host as issuer, `plane:<id>` as subject).
pub fn trust_policy(provider_arn: &str, plane_host: &str, plane_id: &str, audience: &str) -> serde_json::Value {
    serde_json::json!({ "Version": "2012-10-17", "Statement": [{
        "Effect": "Allow", "Principal": { "Federated": provider_arn }, "Action": "sts:AssumeRoleWithWebIdentity",
        "Condition": { "StringEquals": { format!("{plane_host}:aud"): audience, format!("{plane_host}:sub"): format!("plane:{plane_id}") } },
    }] })
}

/// What the role may do: launch and stop machines tagged with this control plane, and read what that needs (made from
/// `permissions::AWS_RUNNER`).
pub fn runner_policy(account_id: &str, plane_id: &str) -> serde_json::Value { crate::permissions::aws_runner_policy(account_id, plane_id) }

/// The newest image of an owner matching a name pattern (RunsOn's public GitHub-compatible Ubuntu 24.04 by default).
pub async fn latest_image(http: &dyn Http, region: &str, creds: &Credentials, owner: &str, name: &str, now_ms: u64) -> Result<String> {
    latest_image_sized(http, region, creds, owner, name, now_ms).await.map(|(id, _)| id)
}

/// The newest image of an owner matching a name pattern, and the size of its disk (a machine's may not be smaller).
pub async fn latest_image_sized(http: &dyn Http, region: &str, creds: &Credentials, owner: &str, name: &str, now_ms: u64) -> Result<(String, u32)> {
    let xml = ec2(http, region, creds, "DescribeImages", serde_json::json!({ "Owner": [owner], "Filter": [{ "Name": "name", "Value": [name] }, { "Name": "state", "Value": ["available"] }] }), now_ms).await?;
    let mut images: Vec<(String, String, u32)> = xml.split("<imageId>").skip(1).filter_map(|chunk| {
        let id = chunk.split('<').next()?.to_string();
        Some((xml_tag(chunk, "creationDate")?.to_string(), id, xml_tag(chunk, "volumeSize").and_then(|v| v.parse().ok()).unwrap_or(0)))
    }).collect();
    images.sort();
    images.pop().map(|(_, id, gb)| (id, gb)).ok_or_else(|| format!("no image {owner}/{name} in {region}"))
}

/// What AWS's spot prices call an operating system.
pub fn product(os: &str) -> &'static str { if os == "windows" { "Windows" } else { "Linux/UNIX" } }

pub struct Launch<'a> {
    pub region: &'a str,
    /// The control plane's own network in this region (see `make_network`); None: the region's default network.
    pub network: Option<&'a Network>,
    pub image: &'a str,
    pub types: &'a [String],
    pub user_data: &'a str,
    pub disk_gb: u32,
    pub tags: &'a [(String, String)],
    /// Spot (cheaper; can be taken back) or on-demand.
    pub spot: bool,
    /// "linux" or "windows" (spot prices differ).
    pub os: &'a str,
}

/// The lowest current spot price (USD per hour) for a machine type in a region, across its zones.
/// The price now in one zone (`zone`), or the lowest across the region's zones.
pub async fn spot_price(http: &dyn Http, region: &str, zone: Option<&str>, creds: &Credentials, instance_type: &str, os: &str, now_ms: u64) -> Result<f64> {
    let mut params = serde_json::json!({ "InstanceType": [instance_type], "ProductDescription": [product(os)], "StartTime": amz_iso(now_ms) });
    if let Some(z) = zone.filter(|z| !z.is_empty()) { params["AvailabilityZone"] = z.into() }
    let xml = ec2(http, region, creds, "DescribeSpotPriceHistory", params, now_ms).await?;
    xml.split("<spotPrice>").skip(1).filter_map(|s| s.split('<').next()?.parse::<f64>().ok()).fold(None, |m: Option<f64>, p| Some(m.map_or(p, |m| m.min(p)))).ok_or_else(|| "no spot price".into())
}

pub fn amz_iso(ms: u64) -> String {
    let d = amz_date(ms); // 20260930T181106Z
    format!("{}-{}-{}T{}:{}:{}Z", &d[0..4], &d[4..6], &d[6..8], &d[9..11], &d[11..13], &d[13..15])
}


/// A job machine's disk (gp3) of this size, per hour.

/// An on-demand machine's price per hour, estimated from its size (fitted to us-east-1 list prices of the c7a, m7a and
/// r7a families: about $0.042 a CPU and $0.0045 a GB of memory).
pub fn on_demand_usd_per_hour(cpu: u32, ram_gb: u32) -> f64 { cpu as f64 * 0.0423 + ram_gb as f64 * 0.0045 }

/// A one-time machine (spot or on-demand): the first instance type with capacity (or that the region has); it terminates
/// itself when it powers off.
/// A machine AWS started: its id, its type, and the zone it is in (its spot price is that zone's).
pub struct Started { pub id: String, pub kind: String, pub zone: String }

/// How many places (a machine type in a zone) one launch tries.
pub const MAX_TRIES: usize = 12;

pub async fn run_instance(http: &dyn Http, creds: &Credentials, l: &Launch<'_>, now_ms: u64) -> Result<Started> {
    let mut last = String::from("no instance type given");
    // Each zone in turn, cheapest spot price first (for the first type), the next when one has no room: in its own
    // network, by its subnet there; in the default network, by naming the zone. On-demand: wherever AWS puts it.
    let prices = match (l.spot, l.types.first()) { (true, Some(first)) => zone_prices(http, l.region, creds, first, l.os, now_ms).await.unwrap_or_default(), _ => Default::default() };
    let price = |z: &str| prices.get(z).copied().unwrap_or(f64::MAX);
    let mut places: Vec<(Option<&(String, String)>, Option<String>)> = match l.network {
        Some(n) if !n.subnets.is_empty() => n.subnets.iter().map(|s| (Some(s), None)).collect(),
        _ if !prices.is_empty() => prices.keys().map(|z| (None, Some(z.clone()))).collect(),
        _ => vec![(None, None)],
    };
    places.sort_by(|a, b| { let z = |p: &(Option<&(String, String)>, Option<String>)| p.0.map(|(z, _)| z.clone()).or(p.1.clone()).unwrap_or_default(); price(&z(a)).total_cmp(&price(&z(b))).then(z(a).cmp(&z(b))) });
    // So many tries at most (types by zones can be many, and a runtime's time for one request is short): the rest
    // is left for the next try of the job.
    let mut tries = 0;
    for t in l.types { for (subnet, zone) in &places {
        tries += 1;
        if tries > MAX_TRIES { return Err(last) }
        let mut params = serde_json::json!({
            "ImageId": l.image, "InstanceType": t, "MinCount": 1, "MaxCount": 1, "UserData": b64(l.user_data.as_bytes()),
            "InstanceInitiatedShutdownBehavior": "terminate",
            "MetadataOptions": { "HttpTokens": "required" },
            "BlockDeviceMapping": [{ "DeviceName": "/dev/sda1", "Ebs": { "VolumeSize": l.disk_gb, "VolumeType": "gp3", "DeleteOnTermination": "true" } }],
            "TagSpecification": [{ "ResourceType": "instance", "Tag": l.tags.iter().map(|(k, v)| serde_json::json!({ "Key": k, "Value": v })).collect::<Vec<_>>() }],
        });
        if l.spot { params["InstanceMarketOptions"] = serde_json::json!({ "MarketType": "spot", "SpotOptions": { "SpotInstanceType": "one-time", "InstanceInterruptionBehavior": "terminate" } }); }
        if let Some(z) = zone { params["Placement"] = serde_json::json!({ "AvailabilityZone": z }); }
        if let (Some((_, id)), Some(n)) = (subnet, l.network) {
            let groups: Vec<&String> = std::iter::once(&n.security_group).chain(&n.more_groups).collect();
            params["NetworkInterface"] = serde_json::json!([{ "DeviceIndex": 0, "SubnetId": id, "SecurityGroupId": groups, "AssociatePublicIpAddress": if n.private { "false" } else { "true" }, "DeleteOnTermination": "true" }]);
        }
        match ec2(http, l.region, creds, "RunInstances", params, now_ms).await {
            Ok(xml) => {
                let id = xml_tag(&xml, "instanceId").ok_or("RunInstances without an instanceId")?.to_string();
                return Ok(Started { id, kind: t.clone(), zone: xml_tag(&xml, "availabilityZone").unwrap_or_default().to_string() });
            }
            // No room for this type now, or a type this region does not have: the next one.
            Err(e) if ["InsufficientInstanceCapacity", "Unsupported", "SpotMaxPriceTooLow", "InvalidParameterValue"].iter().any(|c| e.starts_with(c)) => last = e,
            Err(e) => return Err(e),
        }
    } }
    Err(last)
}

/// A machine type's spot price now in each of a region's zones.
async fn zone_prices(http: &dyn Http, region: &str, creds: &Credentials, instance_type: &str, os: &str, now_ms: u64) -> Result<std::collections::HashMap<String, f64>> {
    let xml = ec2(http, region, creds, "DescribeSpotPriceHistory", serde_json::json!({ "InstanceType": [instance_type], "ProductDescription": [product(os)], "StartTime": amz_iso(now_ms) }), now_ms).await?;
    let mut prices = std::collections::HashMap::new();
    for item in xml.split("<item>").skip(1) {
        if let (Some(z), Some(p)) = (xml_tag(item, "availabilityZone"), xml_tag(item, "spotPrice").and_then(|p| p.parse::<f64>().ok())) { prices.entry(z.to_string()).or_insert(p); }
    }
    Ok(prices)
}

/// A control plane's own network in a region: a VPC with a subnet in each zone (public addresses, a route to the
/// internet) and a security group that lets nothing in. Jobs' machines cannot reach each other, nothing can reach
/// them, and they cannot reach anything else in the account's private networks. Tagged `superci-plane`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Network { pub vpc: String, pub security_group: String, /// (zone, subnet id)
    pub subnets: Vec<(String, String)>,
    /// A network of the account's own (see `given_network`): its further security groups, and whether its machines
    /// get no public address (private subnets, out through the account's own NAT).
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub more_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub private: bool }

/// A network of the account's own for a region's machines, as the dashboard sets it: subnets (one or more, in one
/// VPC) and security groups there. Machines then reach what that network reaches (a database, an internal registry)
/// and leave through it (a fixed address, with its NAT).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GivenNetwork {
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    /// No public addresses (the subnets are private: they need a NAT to reach GitHub).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub private: bool,
}

/// An id as AWS writes one ("subnet-0abc…", "sg-0abc…").
pub fn aws_id(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|rest| (8..=17).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A given network, looked up: its subnets' zones and VPC, each subnet and security group there and in that one VPC
/// (else why not).
pub async fn given_network(http: &dyn Http, region: &str, creds: &Credentials, g: &GivenNetwork, now_ms: u64) -> Result<Network> {
    if g.subnets.is_empty() || g.security_groups.is_empty() { return Err(format!("{region}: a network of your own needs a subnet and a security group")) }
    if let Some(bad) = g.subnets.iter().find(|s| !aws_id(s, "subnet-")).or(g.security_groups.iter().find(|s| !aws_id(s, "sg-"))) { return Err(format!("{region}: {bad} is not a subnet or security group id")) }
    let xml = ec2(http, region, creds, "DescribeSubnets", serde_json::json!({ "Filter": [{ "Name": "subnet-id", "Value": g.subnets }] }), now_ms).await?;
    // Only the ones named, whatever else AWS lists.
    let found: Vec<(&str, &str, &str)> = xml.split("<subnetId>").skip(1).filter_map(|c| Some((c.split('<').next()?, xml_tag(c, "availabilityZone")?, xml_tag(c, "vpcId")?)))
        .filter(|(id, ..)| g.subnets.iter().any(|s| s == id)).collect();
    if let Some(missing) = g.subnets.iter().find(|s| !found.iter().any(|(id, ..)| id == s)) { return Err(format!("{region}: no subnet {missing} there")) }
    let vpc = found[0].2;
    if found.iter().any(|(_, _, v)| *v != vpc) { return Err(format!("{region}: its subnets are in more than one VPC")) }
    let groups = ec2(http, region, creds, "DescribeSecurityGroups", serde_json::json!({ "Filter": [{ "Name": "group-id", "Value": g.security_groups }, { "Name": "vpc-id", "Value": [vpc] }] }), now_ms).await?;
    if let Some(missing) = g.security_groups.iter().find(|s| !groups.contains(&format!("<groupId>{s}</groupId>"))) { return Err(format!("{region}: no security group {missing} in {vpc}")) }
    let mut subnets: Vec<(String, String)> = found.iter().map(|(id, zone, _)| (zone.to_string(), id.to_string())).collect();
    subnets.sort();
    subnets.dedup();
    Ok(Network { vpc: vpc.to_string(), security_group: g.security_groups[0].clone(), subnets, more_groups: g.security_groups[1..].to_vec(), private: g.private })
}

/// Its security group's name, in its VPC.
const NETWORK_GROUP: &str = "superci-machines";

fn tagged(kind: &str, plane_id: &str) -> serde_json::Value {
    serde_json::json!([{ "ResourceType": kind, "Tag": [{ "Key": "superci-plane", "Value": plane_id }, { "Key": "Name", "Value": format!("SuperCI {plane_id}") }] }])
}

fn filters(f: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Array(f.iter().map(|(n, v)| serde_json::json!({ "Name": n, "Value": [v] })).collect())
}

/// Each `<tag>` value after `<first>` in a list answer, with the first `<then>` after it (e.g. a subnet and its zone).
fn pairs<'a>(xml: &'a str, first: &str, then: &str) -> Vec<(&'a str, &'a str)> {
    xml.split(&format!("<{first}>")).skip(1).filter_map(|chunk| Some((chunk.split('<').next()?, xml_tag(chunk, then).unwrap_or("")))).collect()
}

/// The control plane's network in a region, if it is there (whole: a subnet and its security group).
pub async fn find_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now_ms: u64) -> Result<Option<Network>> {
    let vpcs = ec2(http, region, creds, "DescribeVpcs", serde_json::json!({ "Filter": filters(&[("tag:superci-plane", plane_id)]) }), now_ms).await?;
    let Some(vpc) = xml_tag(&vpcs, "vpcId").map(str::to_string) else { return Ok(None) };
    let subnets = ec2(http, region, creds, "DescribeSubnets", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc)]) }), now_ms).await?;
    let subnets: Vec<(String, String)> = pairs(&subnets, "subnetId", "availabilityZone").into_iter().map(|(id, zone)| (zone.to_string(), id.to_string())).collect();
    let groups = ec2(http, region, creds, "DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) }), now_ms).await?;
    let Some(security_group) = xml_tag(&groups, "groupId").map(str::to_string) else { return Ok(None) };
    if subnets.is_empty() { return Ok(None) }
    Ok(Some(Network { vpc, security_group, subnets, ..Default::default() }))
}

/// Makes (or completes) the control plane's network in a region; safe to repeat. With the account's own sign-in (the
/// control plane's role cannot make networks).
pub async fn make_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now_ms: u64) -> Result<Network> {
    let call = |action: &'static str, params: serde_json::Value| ec2(http, region, creds, action, params, now_ms);
    let ok_if = |r: Result<String>, codes: &[&str]| match r { Err(e) if codes.iter().any(|c| e.starts_with(c)) => Ok(String::new()), r => r };
    let mine = filters(&[("tag:superci-plane", plane_id)]);
    let vpc = match xml_tag(&call("DescribeVpcs", serde_json::json!({ "Filter": mine })).await?, "vpcId") {
        Some(v) => v.to_string(),
        None => {
            let made = xml_tag(&call("CreateVpc", serde_json::json!({ "CidrBlock": "10.212.0.0/16", "TagSpecification": tagged("vpc", plane_id) })).await?, "vpcId").ok_or("CreateVpc gave no id")?.to_string();
            // Made twice at once (two dashboards, a double click): the one with the lowest id stays, for both.
            let all = call("DescribeVpcs", serde_json::json!({ "Filter": mine })).await?;
            match pairs(&all, "vpcId", "vpcId").into_iter().map(|(v, _)| v).min() {
                Some(first) if first != made => { let _ = call("DeleteVpc", serde_json::json!({ "VpcId": made })).await; first.to_string() }
                _ => made,
            }
        }
    };
    // A way out to the internet (GitHub, packages): an internet gateway and a route table that uses it.
    let gateways = call("DescribeInternetGateways", serde_json::json!({ "Filter": mine })).await?;
    let gateway = match xml_tag(&gateways, "internetGatewayId") {
        Some(g) => g.to_string(),
        None => xml_tag(&call("CreateInternetGateway", serde_json::json!({ "TagSpecification": tagged("internet-gateway", plane_id) })).await?, "internetGatewayId").ok_or("CreateInternetGateway gave no id")?.to_string(),
    };
    if !gateways.contains(&format!("<vpcId>{vpc}</vpcId>")) {
        ok_if(call("AttachInternetGateway", serde_json::json!({ "InternetGatewayId": gateway, "VpcId": vpc })).await, &["Resource.AlreadyAssociated"])?;
    }
    let route_table = match xml_tag(&call("DescribeRouteTables", serde_json::json!({ "Filter": filters(&[("tag:superci-plane", plane_id), ("vpc-id", &vpc)]) })).await?, "routeTableId") {
        Some(t) => t.to_string(),
        None => xml_tag(&call("CreateRouteTable", serde_json::json!({ "VpcId": vpc, "TagSpecification": tagged("route-table", plane_id) })).await?, "routeTableId").ok_or("CreateRouteTable gave no id")?.to_string(),
    };
    ok_if(call("CreateRoute", serde_json::json!({ "RouteTableId": route_table, "DestinationCidrBlock": "0.0.0.0/0", "GatewayId": gateway })).await, &["RouteAlreadyExists"])?;
    // A subnet in each zone (spot capacity differs by zone), each a /20 of the VPC.
    let zones = call("DescribeAvailabilityZones", serde_json::json!({ "Filter": filters(&[("zone-type", "availability-zone"), ("state", "available"), ("opt-in-status", "opt-in-not-required")]) })).await?;
    let zones: Vec<String> = pairs(&zones, "zoneName", "zoneName").into_iter().map(|(z, _)| z.to_string()).collect();
    let have = call("DescribeSubnets", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc)]) })).await?;
    let used: Vec<&str> = pairs(&have, "cidrBlock", "cidrBlock").into_iter().map(|(c, _)| c).collect();
    let mut subnets: Vec<(String, String)> = pairs(&have, "subnetId", "availabilityZone").into_iter().map(|(id, zone)| (zone.to_string(), id.to_string())).collect();
    let mut free = (0..16).map(|i| format!("10.212.{}.0/20", i * 16)).filter(|c| !used.contains(&c.as_str()));
    let missing: Vec<String> = zones.into_iter().filter(|z| !subnets.iter().any(|(have, _)| have == z)).collect();
    for zone in &missing {
        let Some(cidr) = free.next() else { break };
        let made = call("CreateSubnet", serde_json::json!({ "VpcId": vpc, "AvailabilityZone": zone, "CidrBlock": cidr, "TagSpecification": tagged("subnet", plane_id) })).await?;
        subnets.push((zone.clone(), xml_tag(&made, "subnetId").ok_or("CreateSubnet gave no id")?.to_string()));
    }
    for (_, subnet) in &subnets {
        ok_if(call("AssociateRouteTable", serde_json::json!({ "RouteTableId": route_table, "SubnetId": subnet })).await, &["Resource.AlreadyAssociated"])?;
    }
    // Nothing comes in (no inbound rules, not even from its own machines); everything may go out (AWS's default).
    let groups = call("DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) })).await?;
    let security_group = match xml_tag(&groups, "groupId") {
        Some(g) => g.to_string(),
        None => xml_tag(&call("CreateSecurityGroup", serde_json::json!({ "GroupName": NETWORK_GROUP, "GroupDescription": "SuperCI job machines: nothing comes in", "VpcId": vpc,
            "TagSpecification": tagged("security-group", plane_id) })).await?, "groupId").ok_or("CreateSecurityGroup gave no id")?.to_string(),
    };
    subnets.sort();
    Ok(Network { vpc, security_group, subnets, ..Default::default() })
}

/// Removes the control plane's machines and network in a region (when the control plane is deleted); safe to repeat.
/// Its machines are terminated first, and the network goes once they are (a few minutes at most).
pub async fn delete_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now: &dyn Fn() -> u64, sleep: &dyn Fn(u64)) -> Result<()> {
    let call = |action: &'static str, params: serde_json::Value| ec2(http, region, creds, action, params, now());
    let mine = filters(&[("tag:superci-plane", plane_id)]);
    let up = || call("DescribeInstances", serde_json::json!({ "Filter": [{ "Name": "tag:superci-plane", "Value": [plane_id] },
        { "Name": "instance-state-name", "Value": ["pending", "running", "stopping", "stopped", "shutting-down"] }] }));
    let machines: Vec<String> = pairs(&up().await?, "instanceId", "instanceId").into_iter().map(|(i, _)| i.to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if !machines.is_empty() { call("TerminateInstances", serde_json::json!({ "InstanceId": machines })).await?; }
    let vpcs = call("DescribeVpcs", serde_json::json!({ "Filter": mine })).await?;
    let vpcs: Vec<String> = pairs(&vpcs, "vpcId", "vpcId").into_iter().map(|(v, _)| v.to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if vpcs.is_empty() { return Ok(()) }
    for _ in 0..40 { if !up().await?.contains("<instanceId>") { break } sleep(5_000) }
    for vpc in vpcs {
        let in_vpc = filters(&[("vpc-id", &vpc)]);
        let gone = |r: Result<String>| match r { Err(e) if e.contains("NotFound") => Ok(()), r => r.map(|_| ()) };
        for (g, _) in pairs(&call("DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) })).await?, "groupId", "groupId") {
            gone(call("DeleteSecurityGroup", serde_json::json!({ "GroupId": g })).await)?;
        }
        for (s, _) in pairs(&call("DescribeSubnets", serde_json::json!({ "Filter": in_vpc })).await?, "subnetId", "subnetId") {
            gone(call("DeleteSubnet", serde_json::json!({ "SubnetId": s })).await)?;
        }
        for (t, _) in pairs(&call("DescribeRouteTables", serde_json::json!({ "Filter": filters(&[("tag:superci-plane", plane_id), ("vpc-id", &vpc)]) })).await?, "routeTableId", "routeTableId") {
            gone(call("DeleteRouteTable", serde_json::json!({ "RouteTableId": t })).await)?;
        }
        for (g, _) in pairs(&call("DescribeInternetGateways", serde_json::json!({ "Filter": filters(&[("attachment.vpc-id", &vpc)]) })).await?, "internetGatewayId", "internetGatewayId") {
            gone(call("DetachInternetGateway", serde_json::json!({ "InternetGatewayId": g, "VpcId": vpc })).await)?;
            gone(call("DeleteInternetGateway", serde_json::json!({ "InternetGatewayId": g })).await)?;
        }
        gone(call("DeleteVpc", serde_json::json!({ "VpcId": vpc })).await)?;
    }
    // A gateway left unattached by an earlier try.
    for (g, _) in pairs(&call("DescribeInternetGateways", serde_json::json!({ "Filter": filters(&[("tag:superci-plane", plane_id)]) })).await?, "internetGatewayId", "internetGatewayId") {
        let _ = call("DeleteInternetGateway", serde_json::json!({ "InternetGatewayId": g })).await;
    }
    Ok(())
}

/// What a spot machine of this type cost in this zone from `from_ms` to `to_ms`: each instance-hour at the price in
/// effect when it began, billed by the second, a minute at least (AWS's spot billing).
pub async fn spot_cost(http: &dyn Http, region: &str, zone: &str, creds: &Credentials, instance_type: &str, os: &str, from_ms: u64, to_ms: u64, now_ms: u64) -> Result<f64> {
    let to_ms = to_ms.max(from_ms + 60_000);
    let xml = ec2(http, region, creds, "DescribeSpotPriceHistory", serde_json::json!({ "InstanceType": [instance_type], "ProductDescription": [product(os)],
        "AvailabilityZone": zone, "StartTime": amz_iso(from_ms), "EndTime": amz_iso(to_ms) }), now_ms).await?;
    // (when, price), oldest first; AWS includes the one in effect at the start.
    let mut prices: Vec<(u64, f64)> = xml.split("<item>").skip(1).filter_map(|item| {
        let at = parse_iso_ms(xml_tag(item, "timestamp")?)?;
        Some((at, xml_tag(item, "spotPrice")?.parse().ok()?))
    }).collect();
    prices.sort_by_key(|p| p.0);
    let in_effect = |t: u64| prices.iter().take_while(|(at, _)| *at <= t).last().or(prices.first()).map(|p| p.1);
    let mut cost = 0.0;
    let mut hour = from_ms;
    while hour < to_ms {
        let end = (hour + 3_600_000).min(to_ms);
        cost += (end - hour) as f64 / 3_600_000.0 * in_effect(hour).ok_or("no spot price for that time")?;
        hour = end;
    }
    Ok(cost)
}

/// An on-demand Linux machine's hourly price in a region, from AWS's Price List (pricing:GetProducts).
pub async fn on_demand_price(http: &dyn Http, creds: &Credentials, region: &str, instance_type: &str, os: &str, now_ms: u64) -> Result<f64> {
    // Windows: its licence included (operation RunInstances:0002).
    let (system, operation) = if os == "windows" { ("Windows", "RunInstances:0002") } else { ("Linux", "RunInstances") };
    let filters = [("instanceType", instance_type), ("regionCode", region), ("operatingSystem", system), ("tenancy", "Shared"), ("preInstalledSw", "NA"), ("capacitystatus", "Used"), ("operation", operation)];
    let body = serde_json::json!({ "ServiceCode": "AmazonEC2", "MaxResults": 10,
        "Filters": filters.iter().map(|(f, v)| serde_json::json!({ "Type": "TERM_MATCH", "Field": f, "Value": v })).collect::<Vec<_>>() });
    first_price(&price_list(http, creds, &body, now_ms).await?).ok_or_else(|| format!("no on-demand price for {instance_type} in {region}"))
}

/// A gp3 volume's price per GB-month in a region, from AWS's Price List.
pub async fn disk_price(http: &dyn Http, creds: &Credentials, region: &str, now_ms: u64) -> Result<f64> {
    let body = serde_json::json!({ "ServiceCode": "AmazonEC2", "MaxResults": 10, "Filters": [
        { "Type": "TERM_MATCH", "Field": "regionCode", "Value": region }, { "Type": "TERM_MATCH", "Field": "productFamily", "Value": "Storage" },
        { "Type": "TERM_MATCH", "Field": "volumeApiName", "Value": "gp3" }] });
    first_price(&price_list(http, creds, &body, now_ms).await?).ok_or_else(|| format!("no gp3 price in {region}"))
}

async fn price_list(http: &dyn Http, creds: &Credentials, body: &serde_json::Value, now_ms: u64) -> Result<Vec<serde_json::Value>> {
    let v = json_call(http, "POST", "https://api.pricing.us-east-1.amazonaws.com/", "us-east-1", "pricing", Some("AWSPriceListService.GetProducts"), "application/x-amz-json-1.1", Some(body), creds, now_ms).await?;
    // Each product comes as a JSON document in a string.
    Ok(v["PriceList"].as_array().into_iter().flatten().filter_map(|p| p.as_str().and_then(|s| serde_json::from_str(s).ok()).or_else(|| Some(p.clone()))).collect())
}

/// The first non-zero on-demand USD price among products.
fn first_price(products: &[serde_json::Value]) -> Option<f64> {
    products.iter().flat_map(|p| p["terms"]["OnDemand"].as_object().into_iter().flat_map(|t| t.values()))
        .flat_map(|term| term["priceDimensions"].as_object().into_iter().flat_map(|d| d.values()))
        .filter_map(|d| d["pricePerUnit"]["USD"].as_str()?.parse::<f64>().ok()).find(|p| *p > 0.0)
}

/// A gp3 volume's price per GB-month by region, as published (used when the Price List cannot be read).
pub fn disk_gb_month(region: &str) -> f64 {
    match region {
        "us-east-1" | "us-east-2" | "us-west-2" => 0.08, "eu-west-1" => 0.088, "eu-central-1" => 0.0952, "us-west-1" | "ap-northeast-1" | "ap-southeast-1" | "ap-southeast-2" => 0.096,
        "ca-central-1" => 0.088, "eu-west-2" => 0.0928, "eu-north-1" => 0.0836, "ap-south-1" => 0.0912, _ => 0.096,
    }
}

/// Bytes a machine sent between two times (CloudWatch's NetworkOut, summed over five-minute periods): at most what
/// AWS bills as data sent out (it also counts traffic within the region, which is free).
pub async fn bytes_sent(http: &dyn Http, region: &str, creds: &Credentials, instance_id: &str, from_ms: u64, to_ms: u64, now_ms: u64) -> Result<f64> {
    let xml = query(http, &format!("https://monitoring.{region}.amazonaws.com/"), region, "monitoring", "2010-08-01", creds, "GetMetricStatistics", serde_json::json!({
        "Namespace": "AWS/EC2", "MetricName": "NetworkOut", "Period": 300, "Statistics": { "member": ["Sum"] },
        "Dimensions": { "member": [{ "Name": "InstanceId", "Value": instance_id }] },
        "StartTime": amz_iso(from_ms.saturating_sub(300_000)), "EndTime": amz_iso(to_ms + 300_000) }), now_ms).await?;
    Ok(xml.split("<Sum>").skip(1).filter_map(|s| s.split('<').next()?.parse::<f64>().ok()).sum())
}

/// Data sent from AWS to the internet, per GB (the first 10 TB a month, after the account's free 100 GB).
pub const DATA_OUT_USD_PER_GB: f64 = 0.09;

/// A public IPv4 address, per hour (every machine in a default VPC gets one).
pub const PUBLIC_IPV4_USD_PER_HOUR: f64 = 0.005;

/// What a machine pays per hour besides itself: its disk (gp3, `gb_month` per GB-month) and its public address
/// (none in a private subnet).
pub fn extras_usd_per_hour(gb_month: f64, disk_gb: u32, public_address: bool) -> f64 { disk_gb as f64 * gb_month / 730.0 + if public_address { PUBLIC_IPV4_USD_PER_HOUR } else { 0.0 } }

/// Whether a launch failed for want of room in the region (spot capacity, or the account's quota there), so the next
/// region may have it.
pub fn out_of_room(e: &str) -> bool {
    ["InsufficientInstanceCapacity", "MaxSpotInstanceCountExceeded", "VcpuLimitExceeded", "SpotMaxPriceTooLow", "Unsupported", "InvalidParameterValue", "InstanceLimitExceeded"].iter().any(|c| e.starts_with(c))
}

/// Service Quotas' code for "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests": the vCPUs of spot
/// machines an account may run at once in a region.
pub const SPOT_CPU_QUOTA: &str = "L-34B43A08";

/// How many spot vCPUs this account may run at once in a region (its applied quota, else AWS's default).
pub async fn spot_cpu_quota(http: &dyn Http, region: &str, creds: &Credentials, now_ms: u64) -> Result<u32> {
    let url = format!("https://servicequotas.{region}.amazonaws.com/");
    let body = serde_json::json!({ "ServiceCode": "ec2", "QuotaCode": SPOT_CPU_QUOTA });
    let call = |target: &'static str| json_call(http, "POST", &url, region, "servicequotas", Some(target), "application/x-amz-json-1.1", Some(&body), creds, now_ms);
    let v = match call("ServiceQuotasV20190624.GetServiceQuota").await {
        Ok(v) => v,
        Err(e) if e.starts_with("NoSuchResourceException") => call("ServiceQuotasV20190624.GetAWSDefaultServiceQuota").await?,
        Err(e) => return Err(e),
    };
    v["Quota"]["Value"].as_f64().map(|n| n as u32).ok_or_else(|| "Service Quotas answered without a value".into())
}

/// Where to ask AWS for a higher spot quota in a region.
pub fn spot_quota_link(region: &str) -> String {
    format!("https://{region}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/{SPOT_CPU_QUOTA}")
}

pub async fn terminate_instance(http: &dyn Http, region: &str, creds: &Credentials, instance_id: &str, now_ms: u64) -> Result<()> {
    ec2(http, region, creds, "TerminateInstances", serde_json::json!({ "InstanceId": [instance_id] }), now_ms).await.map(|_| ())
}

/// The runner machine's user data: GitHub's runner (already in the image, /home/runner) for exactly one job, or
/// GitLab's runner (downloaded at boot) for the next job with its tags, in Docker (privileged, as GitLab's own runners).
/// The machine's own time bound, whatever happens to the control plane: it powers off (and so terminates) after the
/// job's longest time. Twice: a timer a job might end (`pkill sleep`), and the system's scheduled shutdown a minute
/// later, which only `shutdown -c` cancels.
macro_rules! bound { () => { "( sleep {secs}; poweroff ) > /dev/null 2>&1 &\nshutdown -P +{minutes} > /dev/null 2>&1 || true" } }

/// What a spot machine does when AWS gives its two minutes' notice (instance metadata's `spot/instance-action`): it
/// tells the control plane (`NOTICE_URL`, a link only this machine has) and stops the runner, so its job fails at
/// once, not when GitHub gives up on a runner it no longer hears from.
const SPOT_WATCH: &str = r#"# AWS takes spot machines back with two minutes' notice: said to the control plane, and the runner stopped.
# (A file /tmp/superci-interrupted counts as the notice, to rehearse it.)
( while sleep 5; do
    if [ ! -e /tmp/superci-interrupted ]; then
      T=$(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60') || continue
      [ "$(curl -s -m 2 -o /dev/null -w '%{http_code}' -H "X-aws-ec2-metadata-token: $T" http://169.254.169.254/latest/meta-data/spot/instance-action)" = 200 ] || continue
    fi
    echo "superci: AWS is taking this spot machine back; stopping the runner"
    curl -s -f -m 5 --retry 4 --retry-all-errors -X POST 'NOTICE_URL' > /dev/null 2>&1
    STOP_RUNNER
    break
  done ) &
"#;

/// The same on Windows: a background job of the start-up script's PowerShell. Windows has no signal that stops the
/// runner gently: the job's own process is ended (the runner then reports the job failed), then the runner.
const SPOT_WATCH_WINDOWS: &str = r#"Start-Job -ScriptBlock {
  while ($true) {
    Start-Sleep 5
    if (-not (Test-Path 'C:\superci-interrupted')) {
      try {
        $t = Invoke-RestMethod -Method Put -TimeoutSec 2 -Headers @{ 'X-aws-ec2-metadata-token-ttl-seconds' = '60' } http://169.254.169.254/latest/api/token
        Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 -Headers @{ 'X-aws-ec2-metadata-token' = $t } http://169.254.169.254/latest/meta-data/spot/instance-action | Out-Null
      } catch { continue }
    }
    foreach ($i in 1..4) { try { Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 -Method Post 'NOTICE_URL' | Out-Null; break } catch { Start-Sleep 2 } }
    if (Get-Process -Name Runner.Worker -ErrorAction SilentlyContinue) { Stop-Process -Name Runner.Worker -Force -ErrorAction SilentlyContinue; Start-Sleep 20 }
    Stop-Process -Name Runner.Listener -Force -ErrorAction SilentlyContinue
    break
  }
} | Out-Null
"#;

/// What a machine does once its job's last step has ended, when files are kept (see `plane::Plugins::output`): it
/// finds what the job's tests left by their own names (Playwright's and Cypress's folders, pytest's list of what
/// failed, coverage in the usual places, JUnit XML written during the job), tells the control plane, and sends each
/// file where it is told. GitHub's runner runs it as a job-completed hook, so it is bounded in time and always says
/// it went well: nothing here can fail a job. `KEEP_LINK` is the machine's own link to the control plane.
const KEEP: &str = r#"link='KEEP_LINK'
cd "${GITHUB_WORKSPACE:-/nonexistent}" 2>/dev/null || exit 0
t=$(mktemp -d) || exit 0
skip='( -name node_modules -o -name .git -o -name vendor -o -name .venv -o -name venv )'
{
  find . -maxdepth 6 $skip -prune -o -type d \( -name playwright-report -o -name blob-report -o -name test-results -o -path '*/cypress/videos' -o -path '*/cypress/screenshots' \) -print 2>/dev/null | while IFS= read -r d; do
    case "$d" in */test-results) [ -e "$d/.last-run.json" ] || continue ;; esac
    find "$d" -type f 2>/dev/null
  done
  find . -maxdepth 6 $skip -prune -o -type f \( -path '*/.pytest_cache/v/cache/lastfailed' -o -path '*/coverage/lcov.info' -o -name cobertura-coverage.xml -o -name coverage.xml \) -print 2>/dev/null
  find . -maxdepth 6 $skip -prune -o -type f -name '*.xml' -size -30M -newer /opt/superci/started -print 2>/dev/null | while IFS= read -r f; do head -c 4096 "$f" | grep -q '<testsuite' && printf '%s\n' "$f"; done
} | sed 's|^\./||' | awk '!seen[$0]++' | head -n 5000 > "$t/files"
[ -s "$t/files" ] || exit 0
while IFS= read -r f; do printf '%s\t%s\n' "$(stat -c %s "$f" 2>/dev/null || echo 0)" "$f"; done < "$t/files" > "$t/said"
curl -sS -f -m 30 --retry 2 -X POST --data-binary "@$t/said" "$link" -o "$t/links" 2>/dev/null || exit 0
n=0; sent=0
while IFS= read -r to <&3 && IFS= read -r f <&4; do
  [ -n "$to" ] || continue
  curl -sS -f -m 200 -T "$f" "$to" > /dev/null 2>&1 &
  sent=$((sent + 1)); n=$((n + 1)); [ $((n % 8)) -eq 0 ] && wait
done 3< "$t/links" 4< "$t/files"
wait
curl -sS -f -m 20 -X POST "$link&done=1" > /dev/null 2>&1
echo "SuperCI kept $sent files this job's tests left."
"#;

/// The start-up script's part that puts `KEEP` on the machine, with the hook GitHub's runner is given.
fn keep_files(link: &str) -> String {
    format!("mkdir -p /opt/superci && cat > /opt/superci/keep.sh <<'SUPERCI_KEEP'\n{}SUPERCI_KEEP\nprintf '#!/bin/bash\\ntimeout 240 bash /opt/superci/keep.sh\\nexit 0\\n' > /opt/superci/kept.sh\nchmod 755 /opt/superci/kept.sh && touch /opt/superci/started\n", KEEP.replace("KEEP_LINK", link))
}

/// A machine's start-up script for an operating system ("linux" or "windows"; see `runner_user_data`). `notice`: for
/// a spot machine running a GitHub job, where it says that AWS is taking it back.
pub fn runner_user_data_for(work: &crate::io::Work, max_minutes: u32, os: &str, notice: Option<&str>, keep: Option<&str>) -> Result<String> {
    let link = |url: &str| url.starts_with("https://") && url.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-?=&".contains(&b));
    if notice.is_some_and(|url| !link(url)) { return Err("invalid notice link".into()) }
    if keep.is_some_and(|url| !link(url)) { return Err("invalid link for kept files".into()) }
    if os != "windows" {
        let mut script = runner_user_data(work, max_minutes)?;
        // What the job's tests leave is kept: GitHub's runner runs this once the job's last step has ended.
        if let (Some(url), crate::io::Work::GitHub { .. }) = (keep, work) {
            let (before, after) = script.split_once("cd /home/runner && sudo -u runner -H ./run.sh").ok_or("no runner in the start-up script")?;
            script = format!("{before}{}cd /home/runner && sudo -u runner -H env ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/opt/superci/kept.sh ./run.sh{after}", keep_files(url));
        }
        // Before the runner starts. GitHub's runner stops its job on an interrupt; GitLab's on being ended (the job
        // fails at once either way, not when the machine is gone).
        let (at, stop) = match work {
            crate::io::Work::GitHub { .. } => ("cd /home/runner && ", "pkill -INT -f Runner.Listener"),
            crate::io::Work::GitLab { .. } => ("export GL_URL=", "pkill -TERM -f /tmp/gitlab-runner"),
            crate::io::Work::Fail { .. } => return Ok(script),
        };
        let Some(url) = notice else { return Ok(script) };
        let (before, after) = script.split_once(at).ok_or("no runner in the start-up script")?;
        return Ok(format!("{before}{}{at}{after}", SPOT_WATCH.replace("NOTICE_URL", url).replace("STOP_RUNNER", stop)));
    }
    let crate::io::Work::GitHub { jit } = work else { return Err("Windows machines run GitHub jobs only".into()) };
    if !jit.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)) { return Err("invalid runner config".into()); }
    let watch = notice.map(|url| SPOT_WATCH_WINDOWS.replace("NOTICE_URL", url)).unwrap_or_default();
    // PowerShell, run by EC2Launch at first boot. Bounded like Linux's: Windows powers off after the time bound
    // whatever happens (and the machine terminates when it does); then the runner, for its one job.
    Ok(format!(r#"<powershell>
Start-Transcript -Path C:\superci.log -Append
shutdown.exe /s /f /t {secs} /c "superci: time bound"
$ProgressPreference = 'SilentlyContinue'
$dir = 'C:\actions-runner'
if (-not (Test-Path "$dir\run.cmd")) {{
  $v = (Invoke-RestMethod -UseBasicParsing -Headers @{{ 'User-Agent' = 'superci' }} https://api.github.com/repos/actions/runner/releases/latest).tag_name.TrimStart('v')
  New-Item -ItemType Directory -Force $dir | Out-Null
  Invoke-WebRequest -UseBasicParsing "https://github.com/actions/runner/releases/download/v$v/actions-runner-win-x64-$v.zip" -OutFile "$env:TEMP\runner.zip"
  Expand-Archive "$env:TEMP\runner.zip" $dir -Force
}}
Set-Location $dir
{watch}& .\run.cmd --jitconfig {jit}
shutdown.exe /a
shutdown.exe /s /f /t 0
</powershell>
"#, secs = max_minutes * 60, jit = jit, watch = watch))
}

pub fn runner_user_data(work: &crate::io::Work, max_minutes: u32) -> Result<String> {
    let (jit_config, fail) = match work {
        crate::io::Work::GitHub { jit } => (jit.as_str(), None),
        crate::io::Work::Fail { jit, why } => (jit.as_str(), Some(why.as_str())),
        crate::io::Work::GitLab { url, token } => {
            let safe = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b));
            if !safe(url) || !safe(token) { return Err("invalid GitLab runner settings".into()) }
            return Ok(format!(concat!("#!/bin/bash\nexec > >(tee /var/log/superci.log > /dev/console) 2>&1\n", bound!(), "\nexport GL_URL={url} GL_TOKEN={token} GL_RUNNER_FLAGS=--docker-privileged\nbash -c '{script}' || true\npoweroff\n"),
                url = url, token = token, secs = max_minutes * 60, minutes = max_minutes + 1, script = crate::gitlab::RUNNER_SCRIPT.replace('\'', "'\\''")))
        }
    };
    let base64 = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    if !base64(jit_config) || fail.is_some_and(|f| !base64(f)) { return Err("invalid runner config".into()); }
    // A failing runner: its job-started hook says why and fails the job before any step.
    let (hook, run) = match fail {
        Some(why) => (format!("printf '#!/bin/bash\\necho {why} | base64 -d\\nexit 1\\n' > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh\n"), "env ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh ./run.sh"),
        None => (String::new(), "./run.sh"),
    };
    Ok(format!(concat!(r#"#!/bin/bash
exec > >(tee /var/log/superci.log > /dev/console) 2>&1
"#, bound!(), r#"
# As GitHub's Ubuntu runners: unprivileged user namespaces (Chromium's sandbox).
sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 || true
# Docker as on GitHub's runners: the runner's user may use it (the image leaves it to root), and it starts now,
# beside the runner, not at a job's first use of it (services, container jobs, docker steps).
usermod -aG docker runner 2>/dev/null || true
(systemctl start docker > /dev/null 2>&1 &)
# First reads from a new volume are slow one at a time: the runner's own files are read 64 at a time first.
find /home/runner/bin -type f -print0 2>/dev/null | xargs -0 -r -P 64 -n 16 cat > /dev/null 2>&1
{hook}cd /home/runner && sudo -u runner -H {run} --jitconfig {jit_config}
poweroff
"#), jit_config = jit_config, hook = hook, run = run, secs = max_minutes * 60, minutes = max_minutes + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_keeps_its_jobs_files_only_when_told_to() {
        let gh = crate::io::Work::GitHub { jit: "abc".into() };
        let link = "https://plane.example/kept?runner=superci-p1-7&t=abcdefghijklmnopqrstuvwx";
        let plain = runner_user_data_for(&gh, 70, "linux", None, None).unwrap();
        assert!(!plain.contains("ACTIONS_RUNNER_HOOK_JOB_COMPLETED") && !plain.contains("/opt/superci"));
        let keeps = runner_user_data_for(&gh, 70, "linux", None, Some(link)).unwrap();
        // The script is on the machine before the runner starts, which is given it as its job-completed hook; the
        // hook is bounded and always says it went well.
        assert!(keeps.find("cat > /opt/superci/keep.sh <<'SUPERCI_KEEP'").unwrap() < keeps.find("sudo -u runner -H env ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/opt/superci/kept.sh ./run.sh --jitconfig abc").unwrap());
        assert!(keeps.contains(&format!("link='{link}'")) && keeps.contains(r"timeout 240 bash /opt/superci/keep.sh\nexit 0\n") && !keeps.contains("KEEP_LINK"));
        assert!(keeps.len() < 16_000, "EC2 takes 16 KB of start-up script: {}", keeps.len());
        // Not a failing runner, not GitLab's, not Windows (so far); and no link that is more than a link.
        assert!(!runner_user_data_for(&crate::io::Work::Fail { jit: "abc".into(), why: "abc".into() }, 10, "linux", None, Some(link)).unwrap().contains("/opt/superci"));
        assert!(!runner_user_data_for(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "glrt-a".into() }, 70, "linux", None, Some(link)).unwrap().contains("/opt/superci"));
        assert!(!runner_user_data_for(&gh, 70, "windows", None, Some(link)).unwrap().contains("superci\\keep"));
        assert!(runner_user_data_for(&gh, 70, "linux", None, Some("https://x/'; reboot; '")).is_err());
        if let Ok(dir) = std::env::var("SUPERCI_PREVIEW_DIR") { std::fs::write(format!("{dir}/keep.sh"), KEEP).unwrap(); }
    }

    #[test]
    fn a_presigned_link_is_signed_as_aws_documents() {
        // AWS's own example (Signature Version 4, query string authentication).
        let creds = Credentials { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), session_token: None, expires_at_ms: 0 };
        let link = presigned("GET", "examplebucket.s3.amazonaws.com", "us-east-1", "test.txt", &creds, 86400, "20130524T000000Z");
        assert_eq!(link, "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404");
        // A key's parts are escaped one by one; a session's token is part of what is signed.
        let session = Credentials { session_token: Some("tok/en+".into()), ..creds };
        let link = s3_link("PUT", &bucket("abc123"), "eu-west-1", "jobs/7/a b/c.xml", &session, 900, 1_700_000_000_000);
        assert!(link.starts_with("https://superci-plane-abc123.s3.eu-west-1.amazonaws.com/jobs/7/a%20b/c.xml?") && link.contains("X-Amz-Security-Token=tok%2Fen%2B") && link.contains("X-Amz-Expires=900"));
    }

    /// EC2 holding one account's networks, answering the calls `make_network` and `find_network` make.
    #[derive(Default)]
    struct Ec2 { calls: std::cell::RefCell<Vec<String>>, made: std::cell::RefCell<Vec<(String, String)>> }
    #[async_trait::async_trait(?Send)]
    impl Http for Ec2 {
        async fn send(&self, r: Request) -> Result<crate::io::Response> {
            let f: Vec<(String, String)> = url::form_urlencoded::parse(&r.body).into_owned().collect();
            let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let action = get("Action");
            self.calls.borrow_mut().push(action.clone());
            let made = |kind: &str| self.made.borrow().iter().filter(|(k, _)| k == kind).map(|(_, id)| id.clone()).collect::<Vec<_>>();
            let make = |kind: &str, id: String| { self.made.borrow_mut().push((kind.into(), id.clone())); id };
            let body = match action.as_str() {
                "DescribeVpcs" => made("vpc").iter().map(|v| format!("<item><vpcId>{v}</vpcId></item>")).collect(),
                "CreateVpc" => format!("<vpc><vpcId>{}</vpcId></vpc>", make("vpc", "vpc-1".into())),
                "DescribeInternetGateways" => made("igw").iter().map(|g| format!("<item><internetGatewayId>{g}</internetGatewayId>{}</item>", if made("attached").is_empty() { String::new() } else { "<attachmentSet><item><vpcId>vpc-1</vpcId></item></attachmentSet>".into() })).collect(),
                "CreateInternetGateway" => format!("<internetGatewayId>{}</internetGatewayId>", make("igw", "igw-1".into())),
                "AttachInternetGateway" => { make("attached", "igw-1".into()); "<return>true</return>".into() }
                "DescribeRouteTables" => made("rtb").iter().map(|t| format!("<item><routeTableId>{t}</routeTableId></item>")).collect(),
                "CreateRouteTable" => format!("<routeTableId>{}</routeTableId>", make("rtb", "rtb-1".into())),
                "CreateRoute" if !made("route").is_empty() => return Ok(crate::io::Response::new(400, "text/xml", "<Response><Errors><Error><Code>RouteAlreadyExists</Code></Error></Errors></Response>")),
                "CreateRoute" => { make("route", "r".into()); "<return>true</return>".into() }
                "DescribeAvailabilityZones" => "<item><zoneName>us-east-1a</zoneName></item><item><zoneName>us-east-1b</zoneName></item>".into(),
                "DescribeSubnets" => made("subnet").iter().map(|s| { let (id, zone, cidr) = s.split_once('|').map(|(a, b)| { let (z, c) = b.split_once('|').unwrap(); (a, z, c) }).unwrap(); format!("<item><subnetId>{id}</subnetId><cidrBlock>{cidr}</cidrBlock><availabilityZone>{zone}</availabilityZone></item>") }).collect(),
                "CreateSubnet" => { let id = format!("subnet-{}", get("AvailabilityZone")); make("subnet", format!("{id}|{}|{}", get("AvailabilityZone"), get("CidrBlock"))); format!("<subnet><subnetId>{id}</subnetId></subnet>") }
                "AssociateRouteTable" => "<associationId>a</associationId>".into(),
                "DescribeSecurityGroups" => made("sg").iter().map(|g| format!("<item><groupId>{g}</groupId></item>")).collect(),
                "CreateSecurityGroup" => { assert_eq!((get("GroupName").as_str(), get("VpcId").as_str()), (NETWORK_GROUP, "vpc-1")); format!("<groupId>{}</groupId>", make("sg", "sg-1".into())) }
                other => panic!("unexpected {other}"),
            };
            Ok(crate::io::Response::new(200, "text/xml", body))
        }
    }

    #[test]
    fn a_network_is_made_once_whole_and_found_after() {
        let (ec2, creds) = (Ec2::default(), Credentials { access_key_id: "A".into(), secret_access_key: "s".into(), session_token: None, expires_at_ms: u64::MAX });
        assert_eq!(futures::executor::block_on(find_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap(), None);
        let n = futures::executor::block_on(make_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap();
        assert_eq!(n, Network { vpc: "vpc-1".into(), security_group: "sg-1".into(), subnets: vec![("us-east-1a".into(), "subnet-us-east-1a".into()), ("us-east-1b".into(), "subnet-us-east-1b".into())], ..Default::default() });
        let made: Vec<String> = ec2.calls.borrow().iter().filter(|c| c.starts_with("Create") || c.starts_with("Attach")).cloned().collect();
        assert_eq!(made, ["CreateVpc", "CreateInternetGateway", "AttachInternetGateway", "CreateRouteTable", "CreateRoute", "CreateSubnet", "CreateSubnet", "CreateSecurityGroup"]);
        assert!(ec2.made.borrow().iter().any(|(k, v)| k == "subnet" && v.ends_with("|10.212.16.0/20")), "a /20 each");
        // Again: nothing new; and it is found.
        ec2.calls.borrow_mut().clear();
        assert_eq!(futures::executor::block_on(make_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap(), n);
        assert!(!ec2.calls.borrow().iter().any(|c| c.starts_with("Create") && c != "CreateRoute" || c.starts_with("Attach")));
        assert_eq!(futures::executor::block_on(find_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap(), Some(n));
    }

    #[test]
    fn iso_times() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-09-30T16:11:43Z"), Some(1_790_784_703_000));
        assert_eq!(parse_iso_ms("2024-02-29T12:00:00.123Z"), Some(1_709_208_000_000));
    }

    #[test]
    fn sigv4_get_vanilla() {
        // AWS's Signature Version 4 test suite, "get-vanilla".
        let creds = Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: None, expires_at_ms: 0 };
        let h = sigv4("GET", "https://example.amazonaws.com/", &[], b"", &creds, "us-east-1", "service", "20150830T123600Z").unwrap();
        let auth = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert_eq!(auth, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_date(1_440_938_160_000), "20150830T123600Z");
        assert_eq!(amz_date(951_782_400_000), "20000229T000000Z");
        assert_eq!(rfc3986("a b/c~d*"), "a%20b%2Fc~d%2A");
    }

    #[test]
    fn query_params_flatten() {
        let mut out = vec![];
        query_params(&serde_json::json!({ "InstanceId": ["i-1", "i-2"], "Tag": [{ "Key": "a", "Value": "b" }], "MinCount": 1, "M": { "HttpTokens": "required" } }), "", &mut out);
        out.sort();
        assert_eq!(out, vec![
            ("InstanceId.1".into(), "i-1".into()), ("InstanceId.2".into(), "i-2".into()), ("M.HttpTokens".into(), "required".into()),
            ("MinCount".into(), "1".into()), ("Tag.1.Key".into(), "a".into()), ("Tag.1.Value".into(), "b".into()),
        ]);
    }

    #[test]
    fn xml_and_user_data() {
        assert_eq!(xml_tag("<a><instanceId>i-9</instanceId></a>", "instanceId"), Some("i-9"));
        assert_eq!(xml_tag("<a/>", "instanceId"), None);
        let gh = |jit: &str| crate::io::Work::GitHub { jit: jit.into() };
        let script = runner_user_data(&gh("abc+/="), 70).unwrap();
        assert!(script.contains("--jitconfig abc+/=") && script.contains("( sleep 4200; poweroff )") && script.contains("shutdown -P +71"));
        assert!(runner_user_data(&gh("abc; rm -rf /"), 70).is_err());
        let gl = runner_user_data(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "glrt-abc_123".into() }, 70).unwrap();
        assert!(gl.contains("GL_URL=https://gitlab.com GL_TOKEN=glrt-abc_123") && gl.contains("run-single"));
        assert!(runner_user_data(&crate::io::Work::GitLab { url: "https://x".into(), token: "t; rm -rf /".into() }, 70).is_err());
    }

    #[test]
    fn a_spot_machine_of_each_kind_watches_for_its_notice_before_its_runner_starts() {
        let gh = crate::io::Work::GitHub { jit: "abc".into() };
        let link = "https://plane.example/interrupted?runner=r-1&t=tok";
        // Windows: a background job of the start-up script; the job's process is ended, then the runner; and the
        // machine powers off whatever shutdown was scheduled before.
        let win = runner_user_data_for(&gh, 70, "windows", Some(link), None).unwrap();
        assert!(win.contains("Start-Job") && win.contains(&format!("-Method Post '{link}'")) && win.contains("Stop-Process -Name Runner.Worker -Force"));
        assert!(win.find("spot/instance-action").unwrap() < win.find(".\\run.cmd --jitconfig abc").unwrap());
        assert!(win.ends_with("shutdown.exe /a\nshutdown.exe /s /f /t 0\n</powershell>\n"));
        assert!(!runner_user_data_for(&gh, 70, "windows", None, None).unwrap().contains("Start-Job"), "on-demand: nothing to watch");
        // Docker is the job's to use on a Linux machine, as on GitHub's runners: its user is let in before the runner starts.
        let linux = runner_user_data_for(&gh, 70, "linux", None, None).unwrap();
        assert!(linux.find("usermod -aG docker runner").unwrap() < linux.find("sudo -u runner").unwrap() && linux.contains("systemctl start docker"));
        // Linux: GitHub's runner is interrupted, GitLab's ended; a failing runner watches nothing.
        let linux = runner_user_data_for(&gh, 70, "linux", Some(link), None).unwrap();
        assert!(linux.contains(&format!("-X POST '{link}'")) && linux.contains("--retry 4") && linux.contains("pkill -INT -f Runner.Listener"));
        let gl = runner_user_data_for(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "glrt-a".into() }, 70, "linux", Some(link), None).unwrap();
        assert!(gl.contains("pkill -TERM -f /tmp/gitlab-runner") && !gl.contains("STOP_RUNNER") && !gl.contains("NOTICE_URL"));
        assert!(!runner_user_data_for(&crate::io::Work::Fail { jit: "abc".into(), why: "abc".into() }, 10, "linux", Some(link), None).unwrap().contains("instance-action"));
        // A link is only ever what a shell reads as one word.
        assert!(runner_user_data_for(&gh, 70, "linux", Some("https://x/'; reboot; '"), None).is_err() && runner_user_data_for(&gh, 70, "windows", Some("http://x/y"), None).is_err());
    }

    #[test]
    fn a_given_network_names_real_ids() {
        assert!(aws_id("subnet-0abc1234", "subnet-") && aws_id("sg-0123456789abcdef0", "sg-"));
        for bad in ["subnet-", "subnet-xyz12345", "sg-0abc1234", "subnet-0abc1234; reboot", "subnet-0abc12345678901234567"] { assert!(!aws_id(bad, "subnet-"), "{bad}") }
        let creds = Credentials { access_key_id: "A".into(), secret_access_key: "s".into(), session_token: None, expires_at_ms: u64::MAX };
        let none = GivenNetwork::default();
        assert!(futures::executor::block_on(given_network(&Ec2::default(), "us-east-1", &creds, &none, 0)).unwrap_err().contains("needs a subnet and a security group"));
        let odd = GivenNetwork { subnets: vec!["my-subnet".into()], security_groups: vec!["sg-0abc1234".into()], private: false };
        assert!(futures::executor::block_on(given_network(&Ec2::default(), "us-east-1", &creds, &odd, 0)).unwrap_err().contains("my-subnet is not a subnet or security group id"));
    }
}
