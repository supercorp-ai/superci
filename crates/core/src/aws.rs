//! AWS for a control plane without an SDK or stored keys: the control plane is an OpenID Connect identity provider, the user's account
//! trusts it through a role (made by the dashboard), STS AssumeRoleWithWebIdentity gives short-lived credentials,
//! and EC2's Query API calls are signed here (Signature Version 4).
use serde::{Deserialize, Serialize};

use crate::crypto::{b64, hex, hmac_sha256, sha256_hex};
use crate::io::{Clock, Http, Request, Result};

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
        // (Never empty: an http(s) address's path is `/` at least.)
        u.path().to_string(),
        query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"),
        all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>(),
        signed.clone(),
        sha256_hex(body),
    ].join("\n");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let signature = hex(&hmac_sha256(&signing_key(&creds.secret_access_key, date, region, service), to_sign.as_bytes()));
    let mut out = all;
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", creds.access_key_id)));
    Ok(out)
}

/// SigV4's signing key: the secret, then the day, the region, the service and `aws4_request`, each signed with the last.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    [region, service, "aws4_request"].iter().fold(hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes()), |key, part| hmac_sha256(&key, part.as_bytes()))
}

/// A link that does one S3 request (`method` on `url`, an object's address) for `expires_s` seconds, signed in its
/// query (Signature Version 4, the body unsigned): whoever holds it may do that and nothing else. `url`'s path must be
/// plain (letters, digits, `/-_.`), as SuperCI's object names are.
pub fn presign(method: &str, url: &str, creds: &Credentials, region: &str, expires_s: u64, now_ms: u64) -> Result<String> { presign_with(method, url, creds, region, expires_s, now_ms, &[]) }

/// The same, with headers the request must carry as they are (an upload's `x-amz-checksum-sha256`: S3 then takes
/// only a body of that SHA-256).
pub fn presign_with(method: &str, url: &str, creds: &Credentials, region: &str, expires_s: u64, now_ms: u64, headers: &[(&str, &str)]) -> Result<String> {
    let u = url::Url::parse(url).map_err(|e| e.to_string())?;
    if !u.path().bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)) || u.query().is_some() { return Err("an object name SuperCI does not make".into()) }
    let host = u.host_str().unwrap_or("").to_string();
    let date = amz_date(now_ms);
    let scope = format!("{}/{region}/s3/aws4_request", &date[..8]);
    let mut signed: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
    signed.push(("host".into(), host.clone()));
    signed.sort();
    let names = signed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let mut query = vec![("X-Amz-Algorithm".to_string(), "AWS4-HMAC-SHA256".to_string()), ("X-Amz-Credential".into(), format!("{}/{scope}", creds.access_key_id)),
        ("X-Amz-Date".into(), date.clone()), ("X-Amz-Expires".into(), expires_s.to_string()), ("X-Amz-SignedHeaders".into(), names.clone())];
    if let Some(t) = &creds.session_token { query.push(("X-Amz-Security-Token".into(), t.clone())) }
    let mut encoded: Vec<(String, String)> = query.iter().map(|(k, v)| (rfc3986(k), rfc3986(v))).collect();
    encoded.sort();
    let canonical_query = encoded.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let canonical = format!("{method}\n{}\n{canonical_query}\n{}\n{names}\nUNSIGNED-PAYLOAD", u.path(), signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>());
    let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let key = signing_key(&creds.secret_access_key, &date[..8], region, "s3");
    Ok(format!("{}://{host}{}?{canonical_query}&X-Amz-Signature={}", u.scheme(), u.path(), hex(&hmac_sha256(&key, to_sign.as_bytes()))))
}

/// A function called without waiting for it (Lambda's `Event` call): whether Lambda took it.
pub async fn invoke_later(http: &dyn Http, region: &str, function: &str, body: &serde_json::Value, creds: &Credentials, now_ms: u64) -> Result<()> {
    let url = format!("https://lambda.{region}.amazonaws.com/2015-03-31/functions/{function}/invocations");
    let body = body.to_string();
    let headers = sigv4("POST", &url, &[("content-type".into(), "application/json".into()), ("x-amz-invocation-type".into(), "Event".into())], body.as_bytes(), creds, region, "lambda", &amz_date(now_ms))?;
    let mut req = Request::new("POST", &url).with_body(body);
    req.headers = headers;
    let r = http.send(req).await?;
    if r.status >= 300 { return Err(format!("{function}: {}", serde_json::from_slice::<serde_json::Value>(&r.body).ok().and_then(|v| v["Message"].as_str().or(v["message"].as_str()).map(str::to_string)).unwrap_or_else(|| format!("HTTP {}", r.status)))) }
    Ok(())
}

/// One signed S3 request (S3 also wants the body's hash as a header); its answer, whatever its status.
pub async fn s3(http: &dyn Http, method: &str, url: &str, region: &str, creds: &Credentials, body: Vec<u8>, headers: &[(&str, &str)], now_ms: u64) -> Result<crate::io::Response> {
    let mut all: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    all.push(("x-amz-content-sha256".into(), sha256_hex(&body)));
    let signed = sigv4(method, url, &all, &body, creds, region, "s3", &amz_date(now_ms))?;
    let mut req = Request::new(method, url).with_body(body);
    req.headers = signed;
    http.send(req).await
}

/// The address of an object in a bucket (virtual-hosted, in the bucket's region: what a VPC's S3 endpoint serves).
pub fn s3_url(bucket: &str, region: &str, key: &str) -> String { format!("https://{bucket}.s3.{region}.amazonaws.com/{key}") }

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

/// What comes before the next tag (all of it when none does).
fn upto_tag(s: &str) -> &str { s.split_once('<').map_or(s, |(before, _)| before) }

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
    image_for(http, region, creds, owner, name, None, now_ms).await.map(|(id, ..)| id)
}

/// The build of each kind of image this version of SuperCI boots: the stamp its maker's image names end with
/// (`runs-on-v2.2-ubuntu26-full-x64-20261102081426`). A build is never taken as it comes out: the maker's Ubuntu
/// 26.04 build of 2 November 2026 came with a kernel under which every signal inside a container was denied, on every
/// job at once. A release moves these to builds its own jobs ran on. The newest of a kind:
/// `aws ec2 describe-images --region us-east-1 --owners 135269210855 --filters Name=name,Values=runs-on-v2.2-KIND-*`.
pub const IMAGE_BUILDS: [(&str, &str); 7] = [
    ("ubuntu24-full-x64", "20261102081424"), ("ubuntu24-full-arm64", "20261102081430"),
    ("ubuntu26-full-x64", "20261102081426"), ("ubuntu26-full-arm64", "20261102081425"),
    ("ubuntu24-gpu-x64", "20261022130417"), ("ubuntu26-gpu-x64", "20260821141648"), ("windows25-full-x64", "20261022175853"),
];

/// The maker's release of the pinned Ubuntu 24.04 build (the stamp of its tag, `linux-20261102081230`, a little before
/// its images' own): where a copy into a region the maker does not publish in comes from.
pub const IMAGE_RELEASE: &str = "20261102081230";

/// The build this version boots of a kind of image (`ubuntu26-full-x64`; see `image_flavor`), when it pins one.
pub fn pinned_build(kind: &str) -> Option<&'static str> { IMAGE_BUILDS.iter().find(|(k, _)| *k == kind).map(|(_, b)| *b) }

/// The build an image is of, from the end of its name (`…-x64-20261006080949`); empty when it has none.
pub fn build_of(name: &str) -> &str { name.rsplit('-').next().filter(|s| s.len() == 14 && s.bytes().all(|b| b.is_ascii_digit())).unwrap_or_default() }

/// An image as DescribeImages lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed { pub created: String, pub id: String, pub gb: u32, pub name: String }

/// Of a kind's images in a region, the one jobs boot: the pinned build. Once its maker has deleted it (it keeps a
/// kind's last three builds, a month or so), the oldest of the newer ones left: out the longest, so a fault in it has
/// been met by others first; that build is said with it. With none newer (no pin; a copy of an older build in a region
/// the maker does not publish in), the newest.
pub fn pick_image<'a>(images: &'a [Listed], pinned: Option<&str>) -> Option<(&'a Listed, Option<&'a str>)> {
    let newest = || images.iter().max_by(|a, b| (&a.created, &a.id).cmp(&(&b.created, &b.id))).map(|i| (i, None));
    let Some(pin) = pinned else { return newest() };
    if let Some(i) = images.iter().find(|i| build_of(&i.name) == pin) { return Some((i, None)) }
    match images.iter().filter(|i| build_of(&i.name) > pin).min_by_key(|i| build_of(&i.name)) {
        Some(i) => Some((i, Some(build_of(&i.name)))),
        None => newest(),
    }
}

/// The image of an owner (or the account's own) matching a name pattern that jobs boot (see `pick_image`), the size
/// of its disk (a machine's may not be smaller), and the build taken in the pinned one's place, when it was.
pub async fn image_for(http: &dyn Http, region: &str, creds: &Credentials, owner: &str, name: &str, pinned: Option<&str>, now_ms: u64) -> Result<(String, u32, Option<String>)> {
    // (Its owner's, or the account's own copy of it under the same name: a region the image is not published in
    // has it once it is copied there.)
    let xml = ec2(http, region, creds, "DescribeImages", serde_json::json!({ "Owner": [owner, "self"], "Filter": [{ "Name": "name", "Value": [name] }, { "Name": "state", "Value": ["available"] }] }), now_ms).await?;
    let images: Vec<Listed> = xml.split("<imageId>").skip(1).filter_map(|chunk| Some(Listed {
        created: xml_tag(chunk, "creationDate")?.to_string(), id: upto_tag(chunk).to_string(),
        gb: xml_tag(chunk, "volumeSize").and_then(|v| v.parse().ok()).unwrap_or(0), name: xml_tag(chunk, "name").unwrap_or_default().to_string(),
    })).collect();
    pick_image(&images, pinned).map(|(i, instead)| (i.id.clone(), i.gb, instead.map(str::to_string))).ok_or_else(|| format!("no image {owner}/{name} in {region}"))
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
    /// For how long each request takes (see `Told::asked`); none: not kept.
    pub clock: Option<&'a dyn Clock>,
    /// Pools (a machine type in a zone) not to ask at all: a spot pool that had no machine a moment ago (asked by
    /// this job or another: the answer would be the same), a zone AWS does not offer the type in.
    pub skip: &'a [(String, String)],
    /// The disk's speed: MiB a second and IOPS (see `spec::disk_speed`).
    pub disk_mibs: u32,
    pub disk_iops: u32,
    pub tags: &'a [(String, String)],
    /// Spot (cheaper; can be taken back) or on-demand.
    pub spot: bool,
    /// "linux" or "windows" (spot prices differ).
    pub os: &'a str,
    /// Spot pools (a type in a zone) to ask after the others: a moment ago they had no machine, or took one back.
    pub avoid: &'a [(String, String)],
    /// What waits in place when AWS refuses a request for its rate, and a little of this launch's own to add to each
    /// wait (so that launches asked at once do not ask again at once).
    pub timer: Option<&'a dyn crate::io::Timer>,
    pub jitter_ms: u64,
    /// Whether the spot pools left after the first has no machine may be asked in one request (see `fleet`). Not
    /// where the role lately had no permission for it: they are asked one by one, as before.
    pub fleet: bool,
    /// A second disk of SuperCI's own beside the job's (GB, MiB a second): the layer store's working space, so it uses
    /// neither the job's disk nor its memory. None: no second disk.
    pub store_disk: Option<(u32, u32)>,
}

/// The machine's disks: the job's, and the layer store's own when asked for (`Launch::store_disk`).
fn disks(l: &Launch<'_>) -> serde_json::Value {
    let mut d = vec![serde_json::json!({ "DeviceName": "/dev/sda1", "Ebs": { "VolumeSize": l.disk_gb, "VolumeType": "gp3", "Throughput": l.disk_mibs, "Iops": l.disk_iops, "DeleteOnTermination": "true" } })];
    if let Some((gb, mibs)) = l.store_disk { d.push(serde_json::json!({ "DeviceName": "/dev/sdf", "Ebs": { "VolumeSize": gb, "VolumeType": "gp3", "Throughput": mibs, "Iops": 3000, "DeleteOnTermination": "true" } })) }
    serde_json::Value::Array(d)
}

/// What a launch was told on the way to its machine (or to none): each pool AWS had no room in (type, zone, AWS's
/// word for it), and how often AWS refused for the rate of requests.
#[derive(Default, Debug)]
pub struct Told { pub refused: Vec<(String, String, String)>, pub throttled: u32,
    /// Every request for a machine, in order: the type, the zone, what AWS said ("started", or its word for no), how
    /// long the answer took (ms; with the waits, when AWS refused for the rate of requests), and how many pools the
    /// request asked at once (0: the one it names; more: see `fleet`, the type and zone being the pool AWS chose,
    /// or none). With `Launch::clock`.
    pub asked: Vec<(String, String, String, u64, u32, f64)>,
    /// It stopped at `MAX_TRIES` with pools left to ask: the job goes on with those, not on down the order.
    pub more: bool,
    /// Why many pools could not be asked in one request (the role may not, or AWS refused the request itself): they
    /// were asked one by one.
    pub fleet_refused: Option<String>,
    /// The launch template made for a request for many pools (see `fleet`), its one use over: to be removed by
    /// whoever asked (`delete_template`), once that is in nobody's way.
    pub template_left: Option<String>,
    /// Machine types not asked for on spot: priced over what the cheapest of them costs on-demand.
    pub priced_out: Vec<String> }

/// The lowest current spot price (USD per hour) for a machine type in a region, across its zones.
/// The price now in one zone (`zone`), or the lowest across the region's zones.
pub async fn spot_price(http: &dyn Http, region: &str, zone: Option<&str>, creds: &Credentials, instance_type: &str, os: &str, now_ms: u64) -> Result<f64> {
    let mut params = serde_json::json!({ "InstanceType": [instance_type], "ProductDescription": [product(os)], "StartTime": amz_iso(now_ms) });
    if let Some(z) = zone.filter(|z| !z.is_empty()) { params["AvailabilityZone"] = z.into() }
    let xml = ec2(http, region, creds, "DescribeSpotPriceHistory", params, now_ms).await?;
    xml.split("<spotPrice>").skip(1).filter_map(|s| upto_tag(s).parse::<f64>().ok()).fold(None, |m: Option<f64>, p| Some(m.map_or(p, |m| m.min(p)))).ok_or_else(|| "no spot price".into())
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
/// How often one launch waits and asks again when AWS refuses for the rate of requests (about ten seconds in all).
pub const MAX_WAITS: u32 = 7;

pub async fn run_instance(http: &dyn Http, creds: &Credentials, l: &Launch<'_>, now_ms: u64, told: &mut Told) -> Result<Started> {
    let mut last = String::from("no instance type given");
    // Every type's spot price now in every zone: a spot machine is asked for where it costs least for the speed it
    // gives (see `speed`), the next pool when one has no room. In its own network a zone is its subnet there; in the
    // default network it is named. On-demand: the types in their order, wherever AWS puts them.
    let prices = if l.spot { spot_prices(http, l.region, creds, l.types, l.os, now_ms).await.unwrap_or_default() } else { Default::default() };
    let mut zones: Vec<String> = prices.keys().map(|(_, z)| z.clone()).collect();
    zones.sort(); zones.dedup();
    let mut places: Vec<(Option<&(String, String)>, Option<String>)> = match l.network {
        // (A network found has a subnet at least: see `find_network`, `given_network`.)
        Some(n) => n.subnets.iter().map(|s| (Some(s), None)).collect(),
        _ if !zones.is_empty() => zones.iter().map(|z| (None, Some(z.clone()))).collect(),
        _ => vec![(None, None)],
    };
    places.sort_by_key(|p| p.0.map(|(z, _)| z.clone()).or(p.1.clone()).unwrap_or_default());
    // So many tries at most (types by zones can be many, and a runtime's time for one request is short): the rest
    // is left for the next try of the job.
    // Every type in every place, in that order; but a spot pool that had no machine a moment ago, or took one back,
    // comes after the rest (asked first by every job of a burst, it costs each of them a request and gives none a
    // machine, or gives one that is soon taken back).
    let zone_of = |p: &(Option<&(String, String)>, Option<String>)| p.0.map(|(z, _)| z.clone()).or(p.1.clone()).unwrap_or_default();
    let mut pools: Vec<(&String, &(Option<&(String, String)>, Option<String>))> = l.types.iter().flat_map(|t| places.iter().map(move |p| (t, p))).collect();
    if l.spot {
        // A zone where a type has no spot price, when it has one elsewhere, is a zone AWS does not offer it in: not asked.
        let priced = |t: &String| prices.keys().any(|(pt, _)| pt == t);
        pools.retain(|(t, p)| !priced(t) || prices.contains_key(&((*t).clone(), zone_of(p))));
        // The least price for the speed first (a type with no price known: after those with one, in the types' order).
        let worth = |t: &String, z: &str| prices.get(&(t.clone(), z.to_string())).map(|p| p / speed(t)).unwrap_or(f64::MAX);
        let order = |t: &String| l.types.iter().position(|x| x == t).unwrap_or(usize::MAX);
        pools.sort_by(|(at, ap), (bt, bp)| worth(at, &zone_of(ap)).total_cmp(&worth(bt, &zone_of(bp))).then(order(at).cmp(&order(bt))).then(zone_of(ap).cmp(&zone_of(bp))));
        pools.sort_by_key(|(t, p)| { let z = zone_of(p); l.avoid.iter().any(|(at, az)| at == *t && *az == z) });
        // Never more than the cheapest of these types costs on-demand: spot is for paying less, and a job is better
        // off on an on-demand machine than on a spot one priced over it.
        let cap = l.types.iter().filter_map(|t| on_demand_estimate(t)).fold(f64::MAX, f64::min);
        let over = |t: &String, p: &(Option<&(String, String)>, Option<String>)| prices.get(&(t.clone(), zone_of(p))).is_some_and(|usd| *usd > cap);
        for (t, p) in &pools { if over(t, p) && !told.priced_out.contains(*t) { told.priced_out.push((*t).clone()) } }
        let before = pools.len();
        pools.retain(|(t, p)| !over(t, p));
        if pools.is_empty() && before > 0 { return Err(format!("InsufficientInstanceCapacity: every spot pool cost more than an on-demand machine (${cap:.3} an hour)")) }
    }
    // Not the pools that are not to be asked (see `Launch::skip`); with none left, that is no room, said as AWS says it.
    let all = pools.len();
    pools.retain(|(t, p)| { let z = zone_of(p); !l.skip.iter().any(|(st, sz)| st == *t && *sz == z) });
    if pools.is_empty() && all > 0 { return Err("InsufficientInstanceCapacity: AWS had no machine of these types a moment ago".into()) }
    let mut tries = 0;
    for (at, (t, place)) in pools.iter().enumerate() {
        let (t, place) = (*t, *place);
        let (subnet, zone) = place;
        // The best pool had no spot machine: the others are asked in one request, not one by one (each no costs
        // most of a second, and a large machine may have a dozen pools to say it). AWS takes the cheapest pool it has
        // room in. With few left, one by one is as quick.
        if at == 1 && l.spot && l.fleet && pools.len() - at >= FLEET_FROM {
            let rest: Vec<(String, Option<String>, String)> = pools[at..].iter().take(FLEET_MOST).map(|(t, p)| ((*t).clone(), p.0.map(|(_, id)| id.clone()), zone_of(*p))).collect();
            // AWS weighs a pool's room before its price, so it is asked first only among the pools within `BAND` of
            // what a job costs on the best one (price for the speed; a larger size costs more, so it is out of the
            // band unless it is a bargain); then the others priced. (Asked for all at once, AWS took r8a at three
            // times what m7a would have cost.) Those with no price known come last, by what they are: as fast as the
            // first choice and of its size first, then the slower ones of that size, then larger sizes.
            let worth = |kind: &str, zone: &str| prices.get(&(kind.to_string(), zone.to_string())).map(|p| p / speed(kind));
            let best = worth(pools[0].0, &zone_of(pools[0].1)).unwrap_or(f64::MAX);
            let (first, size) = (speed(pools[0].0), cpus(pools[0].0));
            let tier = |kind: &str, zone: &str| match worth(kind, zone) {
                Some(w) if w <= best * BAND => 0u8,
                Some(_) => 1,
                None => 2 + (cpus(kind) > size) as u8 * 2 + (speed(kind) < first - 0.001) as u8,
            };
            let groups: Vec<Vec<(String, Option<String>, String)>> = (0..6u8).map(|t| rest.iter().filter(|(kind, _, zone)| tier(kind, zone) == t).cloned().collect::<Vec<_>>()).filter(|g| !g.is_empty()).collect();
            match fleet(http, creds, l, &groups, now_ms, told).await {
                Ok(answer) => {
                    // (The pool AWS chose, with its price.)
                    if let (Ok(m), Some(last)) = (&answer, told.asked.last_mut()) { last.5 = prices.get(&(m.kind.clone(), m.zone.clone())).copied().unwrap_or(0.0) }
                    return answer;
                }
                // Not to be had in one request here: one by one, as before.
                Err(e) => told.fleet_refused = Some(e),
            }
        }
        tries += 1;
        if tries > MAX_TRIES { told.more = true; return Err(last) }
        let mut params = serde_json::json!({
            "ImageId": l.image, "InstanceType": t, "MinCount": 1, "MaxCount": 1, "UserData": b64(compact(&l.user_data).as_bytes()),
            "InstanceInitiatedShutdownBehavior": "terminate",
            "MetadataOptions": { "HttpTokens": "required" },
            "BlockDeviceMapping": disks(l),
            "TagSpecification": [{ "ResourceType": "instance", "Tag": l.tags.iter().map(|(k, v)| serde_json::json!({ "Key": k, "Value": v })).collect::<Vec<_>>() }],
        });
        if l.spot { params["InstanceMarketOptions"] = serde_json::json!({ "MarketType": "spot", "SpotOptions": { "SpotInstanceType": "one-time", "InstanceInterruptionBehavior": "terminate" } }); }
        if let Some(z) = zone { params["Placement"] = serde_json::json!({ "AvailabilityZone": z }); }
        if let (Some((_, id)), Some(n)) = (subnet, l.network) {
            let groups: Vec<&String> = std::iter::once(&n.security_group).chain(&n.more_groups).collect();
            params["NetworkInterface"] = serde_json::json!([{ "DeviceIndex": 0, "SubnetId": id, "SecurityGroupId": groups, "AssociatePublicIpAddress": if n.private { "false" } else { "true" }, "DeleteOnTermination": "true" }]);
        }
        let began = l.clock.map(|c| c.now_ms());
        let note = |told: &mut Told, said: &str| if let (Some(clock), Some(began)) = (l.clock, began) { told.asked.push((t.clone(), zone_of(place), said.split(':').next().unwrap_or_default().to_string(), clock.now_ms().saturating_sub(began), 0, prices.get(&(t.clone(), zone_of(place))).copied().unwrap_or(0.0))) };
        loop {
            match ec2(http, l.region, creds, "RunInstances", params.clone(), now_ms).await {
                Ok(xml) => {
                    note(told, "started");
                    let id = xml_tag(&xml, "instanceId").ok_or("RunInstances without an instanceId")?.to_string();
                    return Ok(Started { id, kind: t.clone(), zone: xml_tag(&xml, "availabilityZone").unwrap_or_default().to_string() });
                }
                // AWS takes only so many requests in a moment from an account (few, from a new one), and refuses the
                // rest: "RequestLimitExceeded". Asked again for the same pool after a wait, longer each time; what
                // was already learnt of the other pools is not asked again.
                Err(e) if e.starts_with("RequestLimitExceeded") && told.throttled < MAX_WAITS && l.timer.is_some() => {
                    told.throttled += 1;
                    l.timer.expect("asked again only with a timer").pause(300 + l.jitter_ms + told.throttled as u64 * 250).await
                }
                // No room for this type now, or a type this region does not have: the next one.
                Err(e) if ["InsufficientInstanceCapacity", "Unsupported", "SpotMaxPriceTooLow", "InvalidParameterValue"].iter().any(|c| e.starts_with(c)) => {
                    told.refused.push((t.clone(), zone_of(place), e.split(':').next().unwrap_or_default().to_string()));
                    note(told, &e);
                    last = e;
                    break
                }
                Err(e) => { note(told, &e); return Err(e) }
            }
        }
    }
    Err(last)
}

/// How much more a job may cost on a spot pool than on the best one for it to be asked with the first request for
/// many pools (see `run_instance`): a quarter.
pub const BAND: f64 = 1.25;

/// What a machine type costs on-demand, about (by its CPUs and its family's memory for each; see
/// `on_demand_usd_per_hour`). None: a family not known here.
pub fn on_demand_estimate(kind: &str) -> Option<f64> {
    let per_cpu = crate::spec::FAMILIES.iter().find(|(f, _)| *f == kind.split('.').next().unwrap_or_default())?.1;
    let cpu = cpus(kind);
    (cpu > 0).then(|| on_demand_usd_per_hour(cpu, cpu * per_cpu))
}

/// With so many spot pools left after the first has no machine, they are asked in one request (see `fleet`).
pub const FLEET_FROM: usize = 4;
/// How many pools one such request asks at most.
pub const FLEET_MOST: usize = 100;

/// One request for one spot machine from any of a group of pools (type, subnet in the control plane's own network,
/// zone), best first, and the next group when none of a group has one: an "instant fleet", which answers in about three seconds whatever the number of pools, where asking
/// them one by one takes most of a second each. AWS chooses the pool with the most spare machines (so the one least
/// likely to be taken back), and among those alike the earliest here. A fleet takes what a machine is from a launch
/// template and not from the request, so one is made for this one use (the start-up script is the job's own), to be
/// removed once AWS has answered.
///
/// Its machine; or AWS's word for none (as `RunInstances` says it), with what was said of each pool in `told.refused`;
/// or, outside those, why the pools could not be asked this way at all.
async fn fleet(http: &dyn Http, creds: &Credentials, l: &Launch<'_>, groups: &[Vec<(String, Option<String>, String)>], now_ms: u64, told: &mut Told) -> Result<Result<Started>> {
    let tag = |k: &str| l.tags.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or_default();
    let name = format!("superci-{}-{}-{}", tag("superci-plane"), tag("superci-job"), l.clock.map(|c| c.now_ms()).unwrap_or(now_ms));
    let tags = |t: &[(String, String)]| t.iter().map(|(k, v)| serde_json::json!({ "Key": k, "Value": v })).collect::<Vec<_>>();
    let mut data = serde_json::json!({
        "ImageId": l.image, "UserData": b64(compact(&l.user_data).as_bytes()), "InstanceInitiatedShutdownBehavior": "terminate", "MetadataOptions": { "HttpTokens": "required" },
        "BlockDeviceMapping": disks(l),
        "TagSpecification": [{ "ResourceType": "instance", "Tag": tags(l.tags) }],
    });
    if let Some(n) = l.network.filter(|n| !n.subnets.is_empty()) {
        let groups: Vec<&String> = std::iter::once(&n.security_group).chain(&n.more_groups).collect();
        data["NetworkInterface"] = serde_json::json!([{ "DeviceIndex": 0, "SecurityGroupId": groups, "AssociatePublicIpAddress": if n.private { "false" } else { "true" }, "DeleteOnTermination": "true" }]);
    }
    let own: Vec<(String, String)> = l.tags.iter().filter(|(k, _)| k == "superci-plane").cloned().collect();
    let began = l.clock.map(|c| c.now_ms());
    // (Its own tag says whose it is, for a sweep to find. Every launch's tags have it: AWS would refuse a tag list with
    // nothing in it.)
    let asked = serde_json::json!({ "LaunchTemplateName": name, "LaunchTemplateData": data, "TagSpecification": [{ "ResourceType": "launch-template", "Tag": tags(&own) }] });
    ec2(http, l.region, creds, "CreateLaunchTemplate", asked, now_ms).await?;
    // Each group in turn, until one has a machine. (The time of making the template is counted with the first.)
    let mut answer: Result<Result<Started>> = Err("no pools to ask".into());
    let (mut began, mut groups) = (began, groups.into_iter());
    loop {
        let Some(pools) = groups.next() else { break };
        let overrides: Vec<serde_json::Value> = pools.iter().enumerate().map(|(i, (kind, subnet, zone))| {
            let mut o = serde_json::json!({ "InstanceType": kind, "Priority": i + 1 });
            match subnet { Some(id) => o["SubnetId"] = id.clone().into(), None if !zone.is_empty() => o["AvailabilityZone"] = zone.clone().into(), None => {} }
            o
        }).collect();
        let request = serde_json::json!({
            "Type": "instant",
            "TargetCapacitySpecification": { "TotalTargetCapacity": 1, "DefaultTargetCapacityType": "spot" },
            // (Of the pools AWS has room in, the cheapest: each group's pools are of one size and speed, so a pricier
            // one, with more memory than asked, is no better. AWS's choice by room alone took r8a at nearly three times
            // c8a's price.)
            "SpotOptions": { "AllocationStrategy": "price-capacity-optimized", "InstanceInterruptionBehavior": "terminate" },
            "LaunchTemplateConfigs": [{ "LaunchTemplateSpecification": { "LaunchTemplateName": name, "Version": "$Latest" }, "Overrides": overrides }],
        });
        let said = loop {
            match ec2(http, l.region, creds, "CreateFleet", request.clone(), now_ms).await {
                Err(e) if e.starts_with("RequestLimitExceeded") && told.throttled < MAX_WAITS && l.timer.is_some() => {
                    told.throttled += 1;
                    l.timer.expect("asked again only with a timer").pause(300 + l.jitter_ms + told.throttled as u64 * 250).await
                }
                other => break other,
            }
        };
        answer = match said { Ok(xml) => fleet_said(&xml, pools, told), Err(e) => Err(e) };
        let took = match (l.clock, began) { (Some(c), Some(b)) => c.now_ms().saturating_sub(b), _ => 0 };
        began = l.clock.map(|c| c.now_ms());
        match &answer {
            Ok(Ok(m)) => if l.clock.is_some() { told.asked.push((m.kind.clone(), m.zone.clone(), "started".into(), took, pools.len() as u32, 0.0)) },
            Ok(Err(e)) => if l.clock.is_some() { told.asked.push((String::new(), String::new(), e.split(':').next().unwrap_or_default().to_string(), took, pools.len() as u32, 0.0)) },
            Err(_) => {}
        }
        // A machine, or the account's limit (no other pool would answer otherwise), or no answer at all: no more is asked.
        if !matches!(&answer, Ok(Err(e)) if e.starts_with("InsufficientInstanceCapacity")) { break }
    }
    // (The template has done its one use: whoever asked removes it, when that is in nobody's way. See `Told::template_left`.)
    told.template_left = Some(name);
    answer
}

/// Removes a launch template made for one request (see `fleet`). One that is no longer there is removed.
pub async fn delete_template(http: &dyn Http, region: &str, creds: &Credentials, name: &str, now_ms: u64) -> Result<()> {
    match ec2(http, region, creds, "DeleteLaunchTemplate", serde_json::json!({ "LaunchTemplateName": name }), now_ms).await { Ok(_) => Ok(()), Err(e) if e.contains("NotFound") => Ok(()), Err(e) => Err(e) }
}

/// What an instant fleet answered: its machine, or what it said of each pool (kept in `told.refused` in
/// `RunInstances`' words) and one word for all of them. An answer that is neither (the role may not start a machine
/// this way) is an error of its own.
fn fleet_said(xml: &str, pools: &[(String, Option<String>, String)], told: &mut Told) -> Result<Result<Started>> {
    let section = |name: &str| xml.split_once(&format!("<{name}>")).and_then(|(_, rest)| rest.split_once(&format!("</{name}>"))).map(|(inside, _)| inside).unwrap_or_default();
    let got = section("fleetInstanceSet");
    if let Some(id) = xml_tag(got, "instanceIds").and_then(|ids| xml_tag(ids, "item")) {
        let subnet = xml_tag(got, "subnetId").unwrap_or_default();
        let zone = xml_tag(got, "availabilityZone").map(str::to_string).or_else(|| pools.iter().find(|(_, s, _)| s.as_deref() == Some(subnet)).map(|(_, _, z)| z.clone())).unwrap_or_default();
        return Ok(Ok(Started { id: id.trim().to_string(), kind: xml_tag(got, "instanceType").unwrap_or_default().to_string(), zone }))
    }
    let (mut limit, mut other) = (false, None::<String>);
    for item in section("errorSet").split("<errorCode>").skip(1) {
        let code = item.split('<').next().unwrap_or_default();
        let message = xml_tag(item, "errorMessage").unwrap_or_default();
        let over = xml_tag(item, "overrides").unwrap_or_default();
        let (kind, subnet) = (xml_tag(over, "instanceType").unwrap_or_default(), xml_tag(over, "subnetId"));
        let zone = xml_tag(over, "availabilityZone").map(str::to_string).or_else(|| pools.iter().find(|(_, s, _)| s.is_some() && s.as_deref() == subnet).map(|(_, _, z)| z.clone())).unwrap_or_default();
        let word = match code {
            // (The zone does not offer the type: "Your requested instance type (…) is not supported in your requested Availability Zone".)
            "InvalidFleetConfiguration" if message.contains("not supported in your requested Availability Zone") => "Unsupported",
            // (No machine there now. "UnfulfillableCapacity" is also what a fleet says of the pools it did not try.)
            "InsufficientInstanceCapacity" | "UnfulfillableCapacity" => "InsufficientInstanceCapacity",
            "MaxSpotInstanceCountExceeded" => { limit = true; "MaxSpotInstanceCountExceeded" }
            _ => { other.get_or_insert_with(|| format!("{code}: {message}")); continue }
        };
        if !kind.is_empty() { told.refused.push((kind.to_string(), zone, word.to_string())) }
    }
    // Refused for something else than room (the role may not launch from a template, say): not an answer about the pools.
    if let Some(e) = other { return Err(e) }
    if xml_tag(xml, "fleetId").is_none() { return Err("CreateFleet without a fleet".into()) }
    Ok(Err(if limit { "MaxSpotInstanceCountExceeded: Max spot instance count exceeded".to_string() } else { format!("InsufficientInstanceCapacity: AWS had no spot machine in any of {} pools", pools.len()) }))
}

/// Which of these machine types a region has at all, in their order.
pub async fn offered_types(http: &dyn Http, region: &str, creds: &Credentials, types: &[String], now_ms: u64) -> Result<Vec<String>> {
    let xml = ec2(http, region, creds, "DescribeInstanceTypeOfferings", serde_json::json!({ "LocationType": "region", "Filter": [{ "Name": "instance-type", "Value": types }] }), now_ms).await?;
    let have: Vec<&str> = xml.split("<item>").skip(1).filter_map(|item| xml_tag(item, "instanceType")).collect();
    Ok(types.iter().filter(|t| have.contains(&t.as_str())).cloned().collect())
}

/// How many CPUs a machine type of the usual sizes has, by its name (`m8a.large` 2, `c8a.4xlarge` 16); none known:
/// counted as the same size as any other.
pub fn cpus(kind: &str) -> u32 {
    match kind.rsplit('.').next().unwrap_or_default() {
        "nano" | "micro" | "small" | "medium" => 1, "large" => 2, "xlarge" => 4,
        size => size.strip_suffix("xlarge").and_then(|n| n.parse::<u32>().ok()).map(|n| n * 4).unwrap_or(0),
    }
}

/// How fast a machine type is beside one of the generation before, for choosing among spot pools by what a job
/// costs, not what an hour costs: a job on a faster machine is over sooner. Each generation of a family is taken
/// as a fifth faster than the last (AWS says "up to 30%" of M8a over M7a, and of Graviton4 over Graviton3; a job is
/// rarely all CPU). By the number in the family's name: `m8a` 8, `c7g` 7, `m6id` 6. Only among the families a
/// job's CPUs come from (c, m, r): a GPU family's number says which GPU it has, not how new it is. An Intel family
/// (`c7i`, `m8i`, `c6id`) counts a quarter slower: each of its CPUs is half a core (hyperthreading), where AMD's
/// (from the 7th generation) and Graviton's are a whole one.
pub fn speed(kind: &str) -> f64 {
    if !kind.starts_with(['c', 'm', 'r']) { return 1.0 }
    let generation = kind.chars().find(|c| c.is_ascii_digit()).and_then(|c| c.to_digit(10)).unwrap_or(7) as i32;
    let family = kind.split('.').next().unwrap_or_default();
    let intel = family.trim_start_matches(|c: char| c.is_ascii_alphabetic()).trim_start_matches(|c: char| c.is_ascii_digit()).starts_with('i');
    1.2f64.powi(generation - 7) * if intel { 0.75 } else { 1.0 }
}

/// The spot price now of each of these machine types in each of a region's zones, by (type, zone). One request.
pub async fn spot_prices(http: &dyn Http, region: &str, creds: &Credentials, types: &[String], os: &str, now_ms: u64) -> Result<std::collections::HashMap<(String, String), f64>> {
    if types.is_empty() { return Ok(Default::default()) }
    let xml = ec2(http, region, creds, "DescribeSpotPriceHistory", serde_json::json!({ "InstanceType": types, "ProductDescription": [product(os)], "StartTime": amz_iso(now_ms) }), now_ms).await?;
    let mut prices = std::collections::HashMap::new();
    for item in xml.split("<item>").skip(1) {
        let (Some(z), Some(p)) = (xml_tag(item, "availabilityZone"), xml_tag(item, "spotPrice").and_then(|p| p.parse::<f64>().ok())) else { continue };
        // (The newest price of a pool comes first. An answer that names no type is of every type asked for.)
        match xml_tag(item, "instanceType") {
            Some(t) => { prices.entry((t.to_string(), z.to_string())).or_insert(p); }
            None => types.iter().for_each(|t| { prices.entry((t.clone(), z.to_string())).or_insert(p); }),
        }
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
    let found: Vec<(&str, &str, &str)> = xml.split("<subnetId>").skip(1).filter_map(|c| Some((upto_tag(c), xml_tag(c, "availabilityZone")?, xml_tag(c, "vpcId")?)))
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

/// Whose a network is: one control plane's own (made before 0.19.13, tagged with its id), or the one SuperCI's
/// control planes in an account share in a region (each region holds 5 VPCs unless AWS is asked for more).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Whose<'a> { Own(&'a str), Shared }

impl Whose<'_> {
    /// The tag that says so: as a filter, and on what is made.
    fn tag(&self) -> (&'static str, &str) { match self { Whose::Own(id) => ("superci-plane", id), Whose::Shared => ("superci-network", "shared") } }
    fn filter(&self) -> (String, String) { let (k, v) = self.tag(); (format!("tag:{k}"), v.to_string()) }
    fn tagged(&self, kind: &str) -> serde_json::Value {
        let ((k, v), name) = (self.tag(), match self { Whose::Own(id) => format!("SuperCI {id}"), Whose::Shared => "SuperCI".to_string() });
        serde_json::json!([{ "ResourceType": kind, "Tag": [{ "Key": k, "Value": v }, { "Key": "Name", "Value": name }] }])
    }
}

/// The VPC a control plane's machines use in a region, and whose it is: its own from before, else the shared one.
async fn network_vpc<'a>(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &'a str, now_ms: u64) -> Result<Option<(String, Whose<'a>)>> {
    let find = async |whose: Whose<'a>| -> Result<Option<(String, Whose<'a>)>> {
        let (k, v) = whose.filter();
        let vpcs = ec2(http, region, creds, "DescribeVpcs", serde_json::json!({ "Filter": filters(&[(&k, &v)]) }), now_ms).await?;
        // (Two made at once: the one with the lowest id, as `make_network` keeps.)
        Ok(pairs(&vpcs, "vpcId", "vpcId").into_iter().map(|(v, _)| v).min().map(|vpc| (vpc.to_string(), whose)))
    };
    match find(Whose::Own(plane_id)).await? { Some(own) => Ok(Some(own)), None => find(Whose::Shared).await }
}

fn filters(f: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Array(f.iter().map(|(n, v)| serde_json::json!({ "Name": n, "Value": [v] })).collect())
}

/// Each `<tag>` value after `<first>` in a list answer, with the first `<then>` after it (e.g. a subnet and its zone).
fn pairs<'a>(xml: &'a str, first: &str, then: &str) -> Vec<(&'a str, &'a str)> {
    xml.split(&format!("<{first}>")).skip(1).map(|chunk| (upto_tag(chunk), xml_tag(chunk, then).unwrap_or(""))).collect()
}

/// The network a control plane's machines use in a region (its own from before, else the account's shared one), if it
/// is there (whole: a subnet and its security group).
pub async fn find_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now_ms: u64) -> Result<Option<Network>> {
    let Some((vpc, _)) = network_vpc(http, region, creds, plane_id, now_ms).await? else { return Ok(None) };
    let subnets = ec2(http, region, creds, "DescribeSubnets", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc)]) }), now_ms).await?;
    let subnets: Vec<(String, String)> = pairs(&subnets, "subnetId", "availabilityZone").into_iter().map(|(id, zone)| (zone.to_string(), id.to_string())).collect();
    let groups = ec2(http, region, creds, "DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) }), now_ms).await?;
    let Some(security_group) = xml_tag(&groups, "groupId").map(str::to_string) else { return Ok(None) };
    if subnets.is_empty() { return Ok(None) }
    Ok(Some(Network { vpc, security_group, subnets, ..Default::default() }))
}

/// Makes (or completes) the network a control plane's machines use in a region; safe to repeat: its own from before
/// when it has one, else the network SuperCI's control planes in the account share there (made by the first). With
/// the account's own sign-in (the control plane's role cannot make networks).
pub async fn make_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now_ms: u64) -> Result<Network> {
    let call = |action: &'static str, params: serde_json::Value| ec2(http, region, creds, action, params, now_ms);
    let ok_if = |r: Result<String>, codes: &[&str]| match r { Err(e) if codes.iter().any(|c| e.starts_with(c)) => Ok(String::new()), r => r };
    let found = network_vpc(http, region, creds, plane_id, now_ms).await?;
    let whose = found.as_ref().map(|(_, w)| *w).unwrap_or(Whose::Shared);
    let (tk, tv) = whose.filter();
    let mine = filters(&[(&tk, &tv)]);
    let vpc = match found {
        Some((v, _)) => v,
        None => {
            let made = xml_tag(&call("CreateVpc", serde_json::json!({ "CidrBlock": "10.212.0.0/16", "TagSpecification": whose.tagged("vpc") })).await?, "vpcId").ok_or("CreateVpc gave no id")?.to_string();
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
        None => xml_tag(&call("CreateInternetGateway", serde_json::json!({ "TagSpecification": whose.tagged("internet-gateway") })).await?, "internetGatewayId").ok_or("CreateInternetGateway gave no id")?.to_string(),
    };
    if !gateways.contains(&format!("<vpcId>{vpc}</vpcId>")) {
        ok_if(call("AttachInternetGateway", serde_json::json!({ "InternetGatewayId": gateway, "VpcId": vpc })).await, &["Resource.AlreadyAssociated"])?;
    }
    let route_table = match xml_tag(&call("DescribeRouteTables", serde_json::json!({ "Filter": filters(&[(&tk, &tv), ("vpc-id", &vpc)]) })).await?, "routeTableId") {
        Some(t) => t.to_string(),
        None => xml_tag(&call("CreateRouteTable", serde_json::json!({ "VpcId": vpc, "TagSpecification": whose.tagged("route-table") })).await?, "routeTableId").ok_or("CreateRouteTable gave no id")?.to_string(),
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
        let made = call("CreateSubnet", serde_json::json!({ "VpcId": vpc, "AvailabilityZone": zone, "CidrBlock": cidr, "TagSpecification": whose.tagged("subnet") })).await?;
        subnets.push((zone.clone(), xml_tag(&made, "subnetId").ok_or("CreateSubnet gave no id")?.to_string()));
    }
    for (_, subnet) in &subnets {
        ok_if(call("AssociateRouteTable", serde_json::json!({ "RouteTableId": route_table, "SubnetId": subnet })).await, &["Resource.AlreadyAssociated"])?;
    }
    // S3 reached from inside the network (the cache's bucket in this region): a gateway endpoint, which AWS does not
    // charge for, and the traffic stays in AWS's network. Without it machines reach S3 over the internet as before.
    let s3 = format!("com.amazonaws.{region}.s3");
    let endpoints = call("DescribeVpcEndpoints", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("service-name", &s3)]) })).await;
    if endpoints.as_ref().is_ok_and(|e| !e.contains("<vpcEndpointId>")) {
        let _ = call("CreateVpcEndpoint", serde_json::json!({ "VpcId": vpc, "ServiceName": s3, "VpcEndpointType": "Gateway", "RouteTableId": [route_table], "TagSpecification": whose.tagged("vpc-endpoint") })).await;
    }
    // Nothing comes in (no inbound rules, not even from its own machines); everything may go out (AWS's default).
    let groups = call("DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) })).await?;
    let security_group = match xml_tag(&groups, "groupId") {
        Some(g) => g.to_string(),
        None => xml_tag(&call("CreateSecurityGroup", serde_json::json!({ "GroupName": NETWORK_GROUP, "GroupDescription": "SuperCI job machines: nothing comes in", "VpcId": vpc,
            "TagSpecification": whose.tagged("security-group") })).await?, "groupId").ok_or("CreateSecurityGroup gave no id")?.to_string(),
    };
    subnets.sort();
    Ok(Network { vpc, security_group, subnets, ..Default::default() })
}

/// Removes a control plane's machines in a region (when it is deleted, or its AWS runners removed), and its own network
/// there (from before 0.19.13); and the network SuperCI's control planes share there when none of the `others` (the
/// account's other control planes, as the caller found them) uses it (each has a network of its own there) and no
/// machine at all is in it. Safe to repeat. The machines are terminated first, and a network goes once they are.
pub async fn delete_network(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, others: &[String], now: &dyn Fn() -> u64, sleep: &dyn Fn(u64)) -> Result<()> {
    let call = |action: &'static str, params: serde_json::Value| ec2(http, region, creds, action, params, now());
    let alive = ["pending", "running", "stopping", "stopped", "shutting-down"];
    let up = || call("DescribeInstances", serde_json::json!({ "Filter": [{ "Name": "tag:superci-plane", "Value": [plane_id] }, { "Name": "instance-state-name", "Value": alive }] }));
    let machines: Vec<String> = pairs(&up().await?, "instanceId", "instanceId").into_iter().map(|(i, _)| i.to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if !machines.is_empty() { call("TerminateInstances", serde_json::json!({ "InstanceId": machines })).await?; }
    // (Another control plane with no network of its own here uses the shared one: asked only where there is one.)
    let (sk, sv) = Whose::Shared.filter();
    let mut shared_used = !call("DescribeVpcs", serde_json::json!({ "Filter": filters(&[(&sk, &sv)]) })).await?.contains("<vpcId>");
    for o in others.iter().filter(|_| !shared_used) {
        let (k, v) = Whose::Own(o).filter();
        if !call("DescribeVpcs", serde_json::json!({ "Filter": filters(&[(&k, &v)]) })).await?.contains("<vpcId>") { shared_used = true; break }
    }
    let mut doomed: Vec<(String, Whose)> = vec![];
    // Its own, and the shared one when no other control plane uses it.
    let mut whose_ones = [Whose::Own(plane_id), Whose::Shared].into_iter().filter(|w| !shared_used || matches!(w, Whose::Own(_)));
    loop {
        let Some(whose) = whose_ones.next() else { break };
        let (k, v) = whose.filter();
        for (vpc, _) in pairs(&call("DescribeVpcs", serde_json::json!({ "Filter": filters(&[(&k, &v)]) })).await?, "vpcId", "vpcId") {
            if !doomed.iter().any(|(d, _)| d == vpc) { doomed.push((vpc.to_string(), whose)) }
        }
    }
    if doomed.is_empty() { return Ok(()) }
    // (Its machines ended first: a network with one in it cannot go. Ten minutes at most.)
    let mut waited = 0;
    while waited < 40 && up().await?.contains("<instanceId>") { sleep(5_000); waited += 1 }
    let mut doomed = doomed.into_iter();
    loop {
        let Some((vpc, whose)) = doomed.next() else { break };
        // (The shared one keeps any machine in it: another control plane's, one the caller did not know of.)
        if whose == Whose::Shared && call("DescribeInstances", serde_json::json!({ "Filter": [{ "Name": "vpc-id", "Value": [&vpc] }, { "Name": "instance-state-name", "Value": alive }] })).await?.contains("<instanceId>") { continue }
        let (tk, tv) = whose.filter();
        let in_vpc = filters(&[("vpc-id", &vpc)]);
        let gone = |r: Result<String>| match r { Err(e) if e.contains("NotFound") => Ok(()), r => r.map(|_| ()) };
        for (g, _) in pairs(&call("DescribeSecurityGroups", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc), ("group-name", NETWORK_GROUP)]) })).await?, "groupId", "groupId") {
            gone(call("DeleteSecurityGroup", serde_json::json!({ "GroupId": g })).await)?;
        }
        for (s, _) in pairs(&call("DescribeSubnets", serde_json::json!({ "Filter": in_vpc })).await?, "subnetId", "subnetId") {
            gone(call("DeleteSubnet", serde_json::json!({ "SubnetId": s })).await)?;
        }
        let endpoints: Vec<String> = pairs(&call("DescribeVpcEndpoints", serde_json::json!({ "Filter": filters(&[("vpc-id", &vpc)]) })).await.unwrap_or_default(), "vpcEndpointId", "vpcEndpointId").into_iter().map(|(e, _)| e.to_string()).collect();
        if !endpoints.is_empty() { gone(call("DeleteVpcEndpoints", serde_json::json!({ "VpcEndpointId": endpoints })).await)?; }
        for (t, _) in pairs(&call("DescribeRouteTables", serde_json::json!({ "Filter": filters(&[(&tk, &tv), ("vpc-id", &vpc)]) })).await?, "routeTableId", "routeTableId") {
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

/// The control plane's cache bucket in a region (see `cache`): made when missing, private (as AWS makes every new
/// bucket: no public access, no ACLs, encrypted), and with a rule of its own that removes any object older than
/// `cache::BUCKET_DAYS` (what the control plane's index no longer knows of goes that way too), and blobs machines
/// put in the layer store's staging after a day. Safe to repeat.
pub async fn make_bucket(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now_ms: u64) -> Result<()> {
    let bucket = crate::cache::bucket(plane_id, region);
    let url = format!("https://{bucket}.s3.{region}.amazonaws.com/");
    let said = |r: crate::io::Response| -> Result<crate::io::Response> { if r.status < 300 { Ok(r) } else { Err(aws_error(&r.body_text(), r.status)) } };
    let head = s3(http, "HEAD", &url, region, creds, vec![], &[], now_ms).await?;
    match head.status {
        200 => {}
        404 => {
            let body = if region == "us-east-1" { String::new() } else { format!(r#"<CreateBucketConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"#) };
            let made = s3(http, "PUT", &url, region, creds, body.into_bytes(), &[], now_ms).await?;
            if made.status >= 300 && !made.body_text().contains("BucketAlreadyOwnedByYou") { return Err(aws_error(&made.body_text(), made.status)) }
        }
        403 => return Err(format!("AccessDenied: the bucket {bucket} is not reachable with this sign-in (or is someone else's)")),
        s => return Err(format!("HTTP {s}: the bucket {bucket}")),
    }
    let rule = format!(r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ID>superci-cache</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status><Expiration><Days>{}</Days></Expiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule><Rule><ID>superci-layers-staging</ID><Filter><Prefix>{}</Prefix></Filter><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>"#, crate::cache::BUCKET_DAYS, crate::layers::STAGING);
    let sum = b64(&crate::crypto::sha256(rule.as_bytes()));
    said(s3(http, "PUT", &format!("{url}?lifecycle"), region, creds, rule.into_bytes(), &[("x-amz-sdk-checksum-algorithm", "SHA256"), ("x-amz-checksum-sha256", &sum)], now_ms).await?)?;
    Ok(())
}

/// Puts an object in the cache bucket of a region unless it is there as it is (`superci-cache` for a version; its
/// SHA-256 is kept with it).
pub async fn put_once(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, key: &str, body: &[u8], now_ms: u64) -> Result<()> {
    let url = s3_url(&crate::cache::bucket(plane_id, region), region, key);
    let sum = sha256_hex(body);
    let there = s3(http, "HEAD", &url, region, creds, vec![], &[], now_ms).await?;
    if there.status == 200 && there.header("x-amz-meta-sha256") == Some(sum.as_str()) { return Ok(()) }
    let r = s3(http, "PUT", &url, region, creds, body.to_vec(), &[("x-amz-meta-sha256", &sum)], now_ms).await?;
    if r.status >= 300 { return Err(aws_error(&r.body_text(), r.status)) }
    Ok(())
}

/// Removes a region's cache bucket and everything in it (when its control plane is deleted); safe to repeat.
pub async fn delete_bucket(http: &dyn Http, region: &str, creds: &Credentials, plane_id: &str, now: &dyn Fn() -> u64) -> Result<()> {
    let bucket = crate::cache::bucket(plane_id, region);
    let url = format!("https://{bucket}.s3.{region}.amazonaws.com/");
    loop {
        let listed = s3(http, "GET", &format!("{url}?list-type=2&max-keys=1000"), region, creds, vec![], &[], now()).await?;
        if listed.status == 404 { return Ok(()) }
        if listed.status >= 300 { return Err(aws_error(&listed.body_text(), listed.status)) }
        let xml = listed.body_text();
        let keys: Vec<String> = xml.split("<Key>").skip(1).filter_map(|k| k.split("</Key>").next()).map(|k| k.replace("&amp;", "&")).collect();
        if keys.is_empty() { break }
        let mut keys = keys.into_iter();
        loop {
            let Some(k) = keys.next() else { break };
            let r = s3(http, "DELETE", &s3_url(&bucket, region, &k), region, creds, vec![], &[], now()).await?;
            if r.status >= 300 && r.status != 404 { return Err(aws_error(&r.body_text(), r.status)) }
        }
    }
    let r = s3(http, "DELETE", &url, region, creds, vec![], &[], now()).await?;
    if r.status >= 300 && r.status != 404 { return Err(aws_error(&r.body_text(), r.status)) }
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
    // (A minute at least: an hour begun at the start, always.)
    let mut hour = from_ms;
    loop {
        let end = (hour + 3_600_000).min(to_ms);
        cost += (end - hour) as f64 / 3_600_000.0 * in_effect(hour).ok_or("no spot price for that time")?;
        hour = end;
        if hour >= to_ms { break }
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
    Ok(xml.split("<Sum>").skip(1).filter_map(|s| upto_tag(s).parse::<f64>().ok()).sum())
}

/// Data sent from AWS to the internet, per GB (the first 10 TB a month, after the account's free 100 GB).
pub const DATA_OUT_USD_PER_GB: f64 = 0.09;

/// A public IPv4 address, per hour (every machine in a default VPC gets one).
pub const PUBLIC_IPV4_USD_PER_HOUR: f64 = 0.005;

/// What a machine pays per hour besides itself: its disk (gp3, `gb_month` per GB-month), the disk's speed above
/// what AWS includes, and its public address (none in a private subnet). AWS prices a MiB a second for a month at
/// half a GB's price and an IOPS at a sixteenth, in every region ($0.040 and $0.005 where a GB is $0.08).
pub fn extras_usd_per_hour(gb_month: f64, disk_gb: u32, disk_mibs: u32, disk_iops: u32, public_address: bool) -> f64 {
    let (mibs, iops) = (disk_mibs.saturating_sub(crate::spec::DISK_MIBS_INCLUDED) as f64, disk_iops.saturating_sub(crate::spec::DISK_IOPS_INCLUDED) as f64);
    (disk_gb as f64 + mibs / 2.0 + iops / 16.0) * gb_month / 730.0 + if public_address { PUBLIC_IPV4_USD_PER_HOUR } else { 0.0 }
}

/// Whether a launch failed for want of room in the region (spot capacity, or the account's quota there), so the next
/// region may have it.
pub fn out_of_room(e: &str) -> bool {
    ["InsufficientInstanceCapacity", "MaxSpotInstanceCountExceeded", "VcpuLimitExceeded", "SpotMaxPriceTooLow", "Unsupported", "InvalidParameterValue", "InstanceLimitExceeded"].iter().any(|c| e.starts_with(c))
}

/// Service Quotas' code for "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests": the vCPUs of spot
/// machines an account may run at once in a region.
pub const SPOT_CPU_QUOTA: &str = "L-34B43A08";

/// How many spot vCPUs this account may run at once in a region (its applied quota, else AWS's default).
pub async fn spot_cpu_quota(http: &dyn Http, region: &str, creds: &Credentials, now_ms: u64) -> Result<u32> { cpu_quota(http, region, creds, SPOT_CPU_QUOTA, now_ms).await }

/// Whose rule refused a call. An organization's policies bind every sign-in in the account, its administrators too:
/// nothing done inside the account lifts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    /// A service control policy of the AWS organization the account is in.
    Organization,
    /// A resource control policy of that organization.
    OrganizationResources,
    /// A permissions boundary on the role or user signed in as.
    Boundary,
    /// The sign-in's own permissions (its role or user is not allowed this).
    Identity,
    /// The policy of this one session.
    Session,
    /// The policy on the resource asked for.
    Resource,
    /// AWS did not say.
    Unsaid,
}

/// AWS refusing a call for want of permission, as it says it: what was refused, whose rule it was, and the policy
/// when AWS names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Refusal { pub action: String, pub by: By, pub policy: String }

impl Refusal {
    /// The organization's management account, from the policy's name (`arn:aws:organizations::ACCOUNT:policy/…`).
    pub fn management_account(&self) -> Option<&str> {
        self.policy.strip_prefix("arn:aws:organizations::")?.split(':').next().filter(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()))
    }
    /// The policy's id (`p-…`), as AWS Organizations lists it.
    pub fn policy_id(&self) -> Option<&str> { self.policy.rsplit('/').next().filter(|p| p.starts_with("p-")) }
}

/// Reads a refusal out of an AWS error (None: the error is something else). AWS words them alike in every service:
/// "… is not authorized to perform: ACTION … with an explicit deny in a service control policy: ARN", or "… because
/// no identity-based policy allows the ACTION action".
pub fn refusal(e: &str) -> Option<Refusal> {
    let denied = ["UnauthorizedOperation", "AccessDenied", "AccessDeniedException", "AuthorizationError", "Client.UnauthorizedOperation"].iter().any(|c| e.starts_with(c))
        || e.contains("not authorized to perform") || e.contains("explicit deny");
    if !denied { return None }
    let word = |after: &str| e.split(after).nth(1).map(|rest| rest.split(|c: char| c.is_whitespace()).next().unwrap_or_default().trim_end_matches(['.', ',', ';']).to_string());
    let by = [("service control policy", By::Organization), ("resource control policy", By::OrganizationResources), ("permissions boundary", By::Boundary),
        ("session policy", By::Session), ("identity-based policy", By::Identity), ("resource-based policy", By::Resource), ("VPC endpoint policy", By::Resource)]
        .iter().find(|(words, _)| e.contains(words)).map(|(_, by)| *by).unwrap_or(By::Unsaid);
    Some(Refusal { action: word("not authorized to perform: ").unwrap_or_default(), by, policy: word("policy: ").filter(|p| p.starts_with("arn:")).unwrap_or_default() })
}

/// What is made in an AWS account for its budget (see `plane::AwsBudget`), named by the control plane it is for: the
/// budget, the rule that refuses starting machines, and the role AWS Budgets puts that rule on with.
pub fn budget_name(plane_id: &str) -> String { format!("superci-{plane_id}") }
pub fn stop_policy_name(plane_id: &str) -> String { format!("superci-stop-{plane_id}") }
pub fn budget_role_name(plane_id: &str) -> String { format!("superci-budget-{plane_id}") }

/// The one kind of machine the budget's rule lets through: the small one that takes a refused job at GitHub and
/// fails it with the reason (see `plane::ControlPlane::fail_fast`), so that nobody waits for a runner that will not
/// come. No job's own machine is ever one of these.
pub const SAYS_WHY_TYPES: [&str; 2] = ["t3a.small", "t3.small"];

/// Service Quotas' code for "Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances": the vCPUs of
/// on-demand machines an account may run at once in a region.
pub const ON_DEMAND_CPU_QUOTA: &str = "L-1216C47A";

/// How many vCPUs of a kind (a quota's code) this account may run at once in a region.
pub async fn cpu_quota(http: &dyn Http, region: &str, creds: &Credentials, code: &str, now_ms: u64) -> Result<u32> {
    let url = format!("https://servicequotas.{region}.amazonaws.com/");
    let body = serde_json::json!({ "ServiceCode": "ec2", "QuotaCode": code });
    let call = |target: &'static str| json_call(http, "POST", &url, region, "servicequotas", Some(target), "application/x-amz-json-1.1", Some(&body), creds, now_ms);
    let v = match call("ServiceQuotasV20190624.GetServiceQuota").await {
        Ok(v) => v,
        Err(e) if e.starts_with("NoSuchResourceException") => call("ServiceQuotasV20190624.GetAWSDefaultServiceQuota").await?,
        Err(e) => return Err(e),
    };
    v["Quota"]["Value"].as_f64().map(|n| n as u32).ok_or_else(|| "Service Quotas answered without a value".into())
}

/// Asks AWS whether a machine of this kind could be started here, without starting one (a dry run): `Ok(())` when it
/// could. A refusal comes back as AWS words it (see `refusal`).
pub async fn could_run(http: &dyn Http, region: &str, creds: &Credentials, image: &str, instance_type: &str, spot: bool, now_ms: u64) -> Result<()> {
    let mut params = serde_json::json!({ "DryRun": "true", "ImageId": image, "InstanceType": instance_type, "MinCount": 1, "MaxCount": 1 });
    if spot { params["InstanceMarketOptions"] = serde_json::json!({ "MarketType": "spot", "SpotOptions": { "SpotInstanceType": "one-time", "InstanceInterruptionBehavior": "terminate" } }); }
    match ec2(http, region, creds, "RunInstances", params, now_ms).await {
        // (A dry run that would have worked is answered as an error of this name.)
        Err(e) if e.starts_with("DryRunOperation") => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err("AWS started a machine although asked only whether it could".into()),
    }
}

/// Where to ask AWS for more vCPUs of a kind (a quota's code) in a region.
pub fn quota_link(region: &str, code: &str) -> String {
    format!("https://{region}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/{code}")
}

/// The kind of image a name pattern asks for, as its maker lists it: `ubuntu24-full-x64` of
/// `runs-on-v2.2-ubuntu24-full-x64-*`.
pub fn image_flavor(pattern: &str) -> &str {
    let rest = pattern.trim_end_matches('*').trim_end_matches('-');
    match rest.find("-ubuntu").or_else(|| rest.find("-windows")) { Some(at) => &rest[at + 1..], None => rest }
}

/// An image's id in each region its maker publishes it in, read from the maker's own list of a build (the notes of
/// a release of `runs-on/runner-images-for-aws`: a heading for each kind of image, then a table of regions and ids).
/// This is how a copy learns its source where no other region can be asked.
pub fn published_images(release_notes: &str, flavor: &str) -> Vec<(String, String)> {
    let heading = format!("### `{flavor}`");
    let Some(at) = release_notes.find(&heading) else { return vec![] };
    release_notes[at + heading.len()..].lines().skip_while(|l| !l.trim_start().starts_with('|')).take_while(|l| l.trim_start().starts_with('|')).filter_map(|row| {
        let mut cells = row.split('|').map(|c| c.trim().trim_matches('`')).filter(|c| !c.is_empty());
        let (region, id) = (cells.next()?, cells.next()?);
        (aws_id(id, "ami-") && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')).then(|| (region.to_string(), id.to_string()))
    }).collect()
}

/// The region to copy an image from, of those it is published in: one on the same continent as where it goes (by
/// the region's first letters), else the first listed.
pub fn copy_source<'a>(published: &'a [(String, String)], to: &str) -> Option<&'a (String, String)> {
    let continent = to.split('-').next().unwrap_or_default();
    published.iter().find(|(r, _)| r != to && r.split('-').next() == Some(continent)).or_else(|| published.iter().find(|(r, _)| r != to))
}

/// What marks an image as a copy SuperCI made (its tag), so that it is found and removed again.
pub const IMAGE_TAG: (&str, &str) = ("superci", "runner-image");

/// Starts copying an image into a region from another (AWS does the copying, in about ten minutes; the copy is the
/// account's own, kept in its storage there). The copy's id.
pub async fn copy_image(http: &dyn Http, region: &str, creds: &Credentials, from_region: &str, from_image: &str, name: &str, now_ms: u64) -> Result<String> {
    let xml = ec2(http, region, creds, "CopyImage", serde_json::json!({ "SourceRegion": from_region, "SourceImageId": from_image, "Name": name,
        "Description": format!("The image SuperCI's jobs run on, copied from {from_region} ({from_image})"),
        "TagSpecification": [{ "ResourceType": "image", "Tag": [{ "Key": IMAGE_TAG.0, "Value": IMAGE_TAG.1 }] }] }), now_ms).await?;
    xml_tag(&xml, "imageId").map(str::to_string).ok_or_else(|| "CopyImage without an imageId".into())
}

/// The account's own copies of an image in a region (see `copy_image`): each one's id, name, state (`pending` while
/// AWS copies, then `available`) and the snapshots that hold it.
pub async fn copied_images(http: &dyn Http, region: &str, creds: &Credentials, now_ms: u64) -> Result<Vec<CopiedImage>> {
    let xml = ec2(http, region, creds, "DescribeImages", serde_json::json!({ "Owner": ["self"], "Filter": [{ "Name": format!("tag:{}", IMAGE_TAG.0), "Value": [IMAGE_TAG.1] }] }), now_ms).await?;
    Ok(xml.split("<imageId>").skip(1).map(|chunk| CopiedImage {
        id: chunk.split('<').next().unwrap_or_default().to_string(),
        name: xml_tag(chunk, "name").unwrap_or_default().to_string(),
        state: xml_tag(chunk, "imageState").unwrap_or_default().to_string(),
        snapshots: chunk.split("<snapshotId>").skip(1).filter_map(|s| s.split('<').next()).filter(|s| aws_id(s, "snap-")).map(str::to_string).collect(),
    }).filter(|i| aws_id(&i.id, "ami-")).collect())
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CopiedImage { pub id: String, pub name: String, pub state: String, pub snapshots: Vec<String> }

/// Removes a copy of an image with the snapshots that hold it.
pub async fn delete_copied_image(http: &dyn Http, region: &str, creds: &Credentials, image: &CopiedImage, now_ms: u64) -> Result<()> {
    ec2(http, region, creds, "DeregisterImage", serde_json::json!({ "ImageId": image.id }), now_ms).await?;
    for snapshot in &image.snapshots { ec2(http, region, creds, "DeleteSnapshot", serde_json::json!({ "SnapshotId": snapshot }), now_ms).await?; }
    Ok(())
}

/// Where to ask AWS for a higher spot quota in a region.
pub fn spot_quota_link(region: &str) -> String { quota_link(region, SPOT_CPU_QUOTA) }

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
/// tells the control plane (`NOTICE_URL`, a link only this machine has), and lets the job go on for what is left of
/// the two minutes: one that ends by itself in that time passes, and is not run again. Twenty seconds before the
/// machine goes the runner is stopped, so a job still going fails at once, not when GitHub gives up on a runner it
/// no longer hears from.
const SPOT_WATCH: &str = r#"# AWS takes spot machines back with two minutes' notice: said to the control plane; the job has until twenty seconds
# before the machine goes, and its runner is stopped then if it is still going.
# (A file /tmp/superci-interrupted counts as the notice, to rehearse it: with the seconds left in it, or none.)
( while sleep 5; do
    END=$(date +%s)
    if [ -e /tmp/superci-interrupted ]; then
      LEFT=$(tr -cd 0-9 < /tmp/superci-interrupted | head -c 4)
      [ -n "$LEFT" ] && END=$(( END + LEFT - 20 ))
    else
      T=$(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60') || continue
      [ "$(curl -s -m 2 -o /dev/null -w '%{http_code}' -H "X-aws-ec2-metadata-token: $T" http://169.254.169.254/latest/meta-data/spot/instance-action)" = 200 ] || continue
      # When AWS says the machine goes; ninety-five seconds from now if it does not say.
      WHEN=$(curl -s -m 2 -H "X-aws-ec2-metadata-token: $T" http://169.254.169.254/latest/meta-data/spot/instance-action | sed -n 's/.*"time" *: *"\([^"]*\)".*/\1/p')
      AT=$(date -d "$WHEN" +%s 2>/dev/null) || AT=
      if [ -n "$WHEN" ] && [ -n "$AT" ]; then END=$(( AT - 20 )); else END=$(( END + 95 )); fi
    fi
    echo "superci: AWS is taking this spot machine back; its job has $(( END - $(date +%s) )) seconds to end by itself"
    curl -s -f -m 5 --retry 4 --retry-all-errors -X POST 'NOTICE_URL' 2>/dev/null|bash &>/dev/null
    SEEN=
    while [ "$(date +%s)" -lt "$END" ]; do
      if pgrep -f 'RUNNER_PROCESS' > /dev/null; then SEEN=1; elif [ -n "$SEEN" ]; then break; fi
      sleep 2
    done
    STOP_RUNNER
    break
  done ) &
"#;

/// What a spot machine runs when the control plane has its notice (`SPOT_WATCH`): the reason written as a GitHub
/// error into the output of the job's step running now (the runner's worker reads it from there, as it reads a step's
/// own `::error::`), so the job's log and its annotations say it beside the runner's "shutdown signal".
pub fn reclaim_said(next: &str) -> String {
    let line = format!("::error title=SuperCI::AWS is taking this spot machine back (two minutes’ notice). {next}");
    format!("w=$(pgrep -f Runner.Worker | head -1)\n[ -n \"$w\" ] || exit 0\nfor p in $(pgrep -P \"$w\"); do printf '\\n%s\\n' '{}' > /proc/$p/fd/1 2>/dev/null && break; done\n", line.replace('\'', ""))
}

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
    # The job has until forty seconds before the machine goes to end by itself (as on Linux; ending it here takes longer).
    $end = (Get-Date).ToUniversalTime()
    if (-not (Test-Path 'C:\superci-interrupted')) {
      $end = $end.AddSeconds(75)
      try { $when = (Invoke-RestMethod -TimeoutSec 2 -Headers @{ 'X-aws-ec2-metadata-token' = $t } http://169.254.169.254/latest/meta-data/spot/instance-action).time; if ($when) { $end = ([datetime]$when).ToUniversalTime().AddSeconds(-40) } } catch {}
    }
    while ((Get-Date).ToUniversalTime() -lt $end -and (Get-Process -Name Runner.Worker -ErrorAction SilentlyContinue)) { Start-Sleep 2 }
    if (Get-Process -Name Runner.Worker -ErrorAction SilentlyContinue) { Stop-Process -Name Runner.Worker -Force -ErrorAction SilentlyContinue; Start-Sleep 20 }
    Stop-Process -Name Runner.Listener -Force -ErrorAction SilentlyContinue
    break
  }
} | Out-Null
"#;

/// A machine's start-up script for an operating system ("linux" or "windows"; see `runner_user_data`). `notice`: for
/// a spot machine running a GitHub job, where it says that AWS is taking it back.
pub fn runner_user_data_for(work: &crate::io::Work, max_minutes: u32, os: &str, notice: Option<&str>) -> Result<String> {
    if notice.is_some_and(|url| !(url.starts_with("https://") || crate::io::local_test_address(url)) || !url.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-?=&".contains(&b))) { return Err("invalid notice link".into()) }
    if os != "windows" {
        let script = runner_user_data(work, max_minutes)?;
        // Before the runner starts. GitHub's runner stops its job on an interrupt; GitLab's on being ended (the job
        // fails at once either way, not when the machine is gone).
        let (at, stop, process) = match work {
            crate::io::Work::GitHub { .. } => ("cd /home/runner && ", "pkill -INT -f Runner.Listener", "Runner.Listener"),
            crate::io::Work::GitLab { .. } => ("export GL_URL=", "pkill -TERM -f /tmp/gitlab-runner", "/tmp/gitlab-runner"),
            crate::io::Work::Fail { .. } => return Ok(script),
        };
        let Some(url) = notice else { return Ok(script) };
        // (Before where its runner starts: the script always has that, see `runner_user_data`.)
        return Ok(script.replacen(at, &format!("{}{at}", SPOT_WATCH.replace("NOTICE_URL", url).replace("STOP_RUNNER", stop).replace("RUNNER_PROCESS", process)), 1));
    }
    let crate::io::Work::GitHub { jit } = work else { return Err("Windows machines run GitHub jobs only".into()) };
    if !jit.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)) { return Err("invalid runner config".into()); }
    let watch = notice.map(|url| SPOT_WATCH_WINDOWS.replace("NOTICE_URL", url)).unwrap_or_default();
    // PowerShell, run by EC2Launch at first boot. Bounded like Linux's: Windows powers off after the time bound
    // whatever happens (and the machine terminates when it does); then the runner, for its one job.
    // GitHub's addresses for the runner, tried first, as on Linux (`reach_github`): a TCP connection each second, 30 at
    // most; the runner waits 30 s after a refusal.
    let hosts = runner_hosts(jit);
    let reach = if hosts.is_empty() { "$w = 0\n".to_string() } else {
        format!("$w = 0; foreach ($h in @({})) {{ while ($w -lt 30) {{ try {{ (New-Object Net.Sockets.TcpClient($h, 443)).Close(); break }} catch {{ Start-Sleep 1; $w++ }} }} }}\n", hosts.iter().map(|h| format!("'{h}'")).collect::<Vec<_>>().join(","))
    };
    Ok(format!(r#"<powershell>
Start-Transcript -Path C:\superci.log -Append
$t0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
shutdown.exe /s /f /t {secs} /c "superci: time bound"
$ProgressPreference = 'SilentlyContinue'
# The C: drive as large as the disk the machine was given (the image's is 100 GB).
try {{ $max = (Get-PartitionSupportedSize -DriveLetter C).SizeMax; if ((Get-Partition -DriveLetter C).Size -lt ($max - 1GB)) {{ Resize-Partition -DriveLetter C -Size $max }} }} catch {{ }}
$dir = 'C:\actions-runner'
if (-not (Test-Path "$dir\run.cmd")) {{
  $v = (Invoke-RestMethod -UseBasicParsing -Headers @{{ 'User-Agent' = 'superci' }} https://api.github.com/repos/actions/runner/releases/latest).tag_name.TrimStart('v')
  New-Item -ItemType Directory -Force $dir | Out-Null
  Invoke-WebRequest -UseBasicParsing "https://github.com/actions/runner/releases/download/v$v/actions-runner-win-x64-$v.zip" -OutFile "$env:TEMP\runner.zip"
  Expand-Archive "$env:TEMP\runner.zip" $dir -Force
}}
Set-Location $dir
{reach}{watch}& .\run.cmd --jitconfig {jit}
shutdown.exe /a
shutdown.exe /s /f /t 0
</powershell>
"#, secs = max_minutes * 60, jit = jit, watch = watch, reach = reach))
}

/// What a Windows machine adds before its runner starts (as `with_hello` on Linux): how its start went, said to the
/// control plane (when its script began, how long after the machine booted, how long GitHub's addresses refused it,
/// when the runner was started), and, while the runner runs, what it writes of each step, every two seconds.
pub fn with_hello_windows(script: String, url: &str) -> Result<String> {
    // (Its callers give it an https link only.)
    if !url.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-?=&".contains(&b)) { return Err("invalid hello link".into()) }
    const RUNNER: &str = "& .\\run.cmd --jitconfig";
    let said = HELLO_WINDOWS.replace("HELLO_URL", url);
    let (before, after) = script.split_once(RUNNER).ok_or("no runner in the start-up script")?;
    Ok(format!("{before}{said}{RUNNER}{after}"))
}

const HELLO_WINDOWS: &str = r#"$up = ((Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime).TotalSeconds
$t2 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
try { Invoke-RestMethod -Method Post -Uri ("HELLO_URL&boot={0:F2}&script=$t0&ready=$t2&gh=$w" -f $up) | Out-Null } catch { }
Start-Job -ArgumentList $dir -ScriptBlock { param($d)
  $o = @{}; for ($i = 0; $i -lt 90; $i++) { if (Get-Process Runner.Listener -ErrorAction SilentlyContinue) { break }; Start-Sleep 1 }
  while ($true) { $r = Get-Process Runner.Listener -ErrorAction SilentlyContinue
    foreach ($f in Get-ChildItem "$d\_diag\pages\*.log" -ErrorAction SilentlyContinue) { $p = [int64]$o[$f.Name]
      if ($f.Length -gt $p) { try { $s = [IO.File]::Open($f.FullName, 'Open', 'Read', 'ReadWrite'); [void]$s.Seek($p, 'Begin'); $b = New-Object byte[] ($f.Length - $p); $n = $s.Read($b, 0, $b.Length); $s.Close()
        if ($n -lt $b.Length) { $b = $b[0..($n - 1)] }; Invoke-RestMethod -Method Post -Uri "HELLO_URL&live=$($f.Name)&at=$p" -Body ([byte[]]$b) -ContentType 'application/octet-stream' | Out-Null; $o[$f.Name] = $p + $n } catch { } } }
    if (-not $r) { break }; Start-Sleep 2 } } | Out-Null
"#;

/// The root filesystem as large as the disk the machine was given. The volume has the size asked for; the image's
/// partition and filesystem keep the image's own (29 GB of Ubuntu's, 18 of them used) unless they are grown, which
/// the image does not do by itself. Under two seconds on a mounted root; nothing here stops the job (`growpart`
/// ends with an error when there is nothing to grow).
/// The kernel's settings on a Linux machine, before Docker or a job starts. Kept in /etc/sysctl.d too, so a job that
/// installs or updates a package which loads the system's settings again (procps, apparmor) does not undo them.
/// - As GitHub's Ubuntu runners: unprivileged user namespaces (Chromium's sandbox).
/// - A container's processes may signal each other (Postgres's start, a build killing its workers). With Ubuntu's
///   `apparmor_restrict_unprivileged_unconfined=1` and kernels from 7.0.0-1016-aws (Ubuntu 26.04 images from
///   2026-11-02), runc's change to Docker's profile is stacked as `docker-default//&unconfined`, and the profile lets a
///   process signal only `docker-default`: every signal inside a container is denied (moby/moby#54099). A machine
///   runs one job and is thrown away, so this hardening of a shared host buys nothing here.
/// - The ports a job's own servers listen on in 50000-59999 (Supabase's 54321-54329, say) never handed out to the
///   connections the machine opens (Linux hands those out from 32768-60999): one found its port taken now and then.
/// (`-e`: a key a kernel does not have is passed over, the others still set.)
const KERNEL_SETTINGS: &str = "printf '%s\\n' kernel.apparmor_restrict_unprivileged_userns=0 kernel.apparmor_restrict_unprivileged_unconfined=0 net.ipv4.ip_local_reserved_ports=50000-59999>/etc/sysctl.d/99-superci.conf\nsysctl -eqp /etc/sysctl.d/99-superci.conf||:\n";

const GROW_ROOT: &str =r#"root="$(findmnt -n -o SOURCE /)"; disk="$(lsblk -n -o PKNAME "$root" 2>/dev/null | head -n 1)"; part="$(echo "$root" | grep -o '[0-9]*$')"
if [ -n "$disk" ] && [ -n "$part" ]; then growpart "/dev/$disk" "$part" || true; resize2fs "$root" 2>/dev/null || xfs_growfs / 2>/dev/null || true; fi
df -h / | tail -n 1
"#;

/// A fast place for what a job writes most: Docker's files, its work folder and /tmp, bound from one place that
/// `MOUNT_SCRATCH` mounts (memory, or the machine's own NVMe disk). Before Docker starts. With nothing mounted
/// (a machine with no such disk), everything stays on the root disk as it was.
const SCRATCH: &str = r#"scratch=/mnt/superci; mkdir -p "$scratch"
MOUNT_SCRATCH
if mountpoint -q "$scratch"; then
  systemctl stop docker.socket docker containerd > /dev/null 2>&1 || true
  for d in /var/lib/docker /var/lib/containerd /var/lib/superci-layers /home/runner/work /tmp; do mkdir -p "$d" "$scratch$d"; mount --bind "$scratch$d" "$d"; done
  chmod 1777 /tmp; chown runner: /home/runner/work 2>/dev/null || true
  df -h "$scratch" | tail -n 1
fi
"#;

/// The machine's own NVMe disks (instance storage; several as one): formatted and mounted. Nothing kept there
/// outlives the machine, so its inode tables are not written out behind the job's back (`noinit_itable`: on a real
/// machine that work took the disk from the job's first half minute).
const MOUNT_NVME: &str = r#"set -- $(lsblk -d -n -o NAME,MODEL | awk '/Instance Storage/{print "/dev/"$1}'); dev="$1"
if [ $# -gt 1 ] && command -v mdadm > /dev/null && mdadm --create /dev/md0 --run --level=0 --raid-devices=$# "$@" > /dev/null 2>&1; then dev=/dev/md0; fi
if [ -n "$dev" ] && mkfs.ext4 -q -F -E nodiscard,lazy_itable_init=1,lazy_journal_init=1 "$dev"; then mount -o noatime,noinit_itable "$dev" "$scratch"; fi"#;

/// Memory as that place: up to three quarters of the machine's, used only as files are written (and counted as
/// memory: what a job keeps there must fit beside what it runs). Full, it says "No space left on device"; the last
/// quarter stays for what the job runs.
const MOUNT_RAM: &str = r#"mount -t tmpfs -o size=75% tmpfs "$scratch""#;

/// A Linux machine's start-up script with a fast place for a job's files (see `SCRATCH`): memory when the job
/// asked for it (`ramdisk`), else the machine's own NVMe disk when its type has one (nothing is mounted on a
/// machine without, and the files stay on its disk as they were). One or the other. Set up after the root
/// filesystem is grown and before Docker and the runner start. (Not for the runner that only says why a job failed.)
pub fn with_scratch(script: String, ramdisk: bool) -> String {
    if !script.contains(GROW_ROOT) { return script }
    script.replacen(GROW_ROOT, &format!("{GROW_ROOT}{}", SCRATCH.replace("MOUNT_SCRATCH", if ramdisk { MOUNT_RAM } else { MOUNT_NVME })), 1)
}

/// What `with_hello` adds before the runner starts. While GitHub's runner runs, what it writes of each step (pages of
/// its log under `_diag/pages`, in pieces as it writes them) is sent on every two seconds (`live`, the page's name;
/// `at`, where in it): GitHub gives a job's log only once it has ended. A runner that took more than ten seconds to listen for a job
/// also says what it logged meanwhile (GitHub's runner keeps a log of its own under `_diag`): when the machine
/// started it, each wait of two seconds or more with the line before and the line after, and its warnings and
/// errors. So a slow one says where it waited: for its own start, for a name, for GitHub's answer. Once its job begins,
/// the start of the runner's own log (`worker`), which has the job message it was handed: its steps' shape (which run
/// in the background, in a parallel group, wait for which), for the job's page.
const HELLO: &str = r#"# How the start went, said to the control plane (see where a slow one lost its time).
( superci_t2=$(date +%s%3N); curl -s -m 5 -X POST "HELLO_URL&boot=$superci_up&script=$superci_t0&read=${superci_t1:-0}&ready=$superci_t2&gh=$(cat /tmp/superci-gh 2>/dev/null)" > /dev/null 2>&1
  for i in $(seq 360); do grep -qs "Listening for Jobs" /var/log/superci.log && break; sleep 0.5; done
  superci_t3=$(date +%s%3N)
  if grep -qs "Listening for Jobs" /var/log/superci.log; then
    curl -s -m 5 -X POST "HELLO_URL&listening=$superci_t3" > /dev/null 2>&1
    if [ $((superci_t3 - superci_t2)) -gt 10000 ]; then
      { echo "[$(date -u -d "@$((superci_t2 / 1000))" '+%Y-%m-%d %H:%M:%SZ') the machine's script started the runner]"; cat /home/runner/_diag/Runner_*.log 2>/dev/null; } | awk 'function at(s) { return substr(s, 13, 2) * 3600 + substr(s, 16, 2) * 60 + substr(s, 19, 2) }
        /^\[20[0-9][0-9]-/ { t = at($0); if (n && t - p >= 2) { if (last != shown) print last; print "... " (t - p) " s ..."; print; shown = $0 } else if (!n || $0 ~ /^\[[^]]* (WARN|ERR) /) { print; shown = $0 }; p = t; last = $0; n++ }
        END { if (last != shown) print last }' | cut -c1-400 | head -c 20000 | curl -s -m 10 -X POST -H "content-type: text/plain" --data-binary @- "HELLO_URL&log=runner" > /dev/null 2>&1
    fi
  fi ) &
(for i in $(seq 90);do pgrep -f Runner.Listener>/dev/null&&break;sleep 1;done;declare -A o;while :;do r=$(pgrep -f Runner.Listener);for f in /home/runner/_diag/pages/*.log;do [ -f "$f" ]||continue;n=${f##*/};s=$(stat -c %s "$f");p=${o[$n]:-0};[ "$s" -gt "$p" ]&&tail -c +$((p+1)) "$f"|head -c $((s-p))|curl -sf -m 10 --data-binary @- "HELLO_URL&live=$n&at=$p">/dev/null&&o[$n]=$s;done;if [ "$w" != 1 ];then g=$(ls /home/runner/_diag/Worker_*.log 2>/dev/null|head -1);if [ -n "$w" ];then head -c 2000000 "$g"|curl -sf -m 20 --data-binary @- "HELLO_URL&worker=1">/dev/null&&w=1;elif grep -qs '"steps"' "$g";then w=0;fi;fi;[ -z "$r" ]&&break;sleep 2;done) &
"#;

/// A Linux machine's start-up script that also tells the control plane how its start went (`url`: a link only this
/// machine has): how long after its kernel started the script began, when the script began, when it began reading
/// the runner's files, when it was ready to start the runner, and when the runner listened for a job. So a slow
/// start can be seen for what it was: AWS, the boot, the disk, or the runner reaching GitHub. Sent in the
/// background: nothing here waits for it or stops for it.
/// What `with_cache` adds before the runner starts: the tools' remote caches pointed at `superci-cache` on this
/// machine's private address (Turbo and Nx in the runner's own environment, Bazel in its system-wide settings: a
/// workflow's own settings come first), then `superci-cache` fetched (the machine's link sends it on to the bucket, by a
/// link that does only that), put in front of the runner's own Node (which runs the job's actions), and started.
/// Beside the runner, not before it: the first action runs seconds after the runner starts, and until the program is
/// up the actions use GitHub's cache and the tools their own.
const CACHE: &str = r#"# GitHub's cache (and Turbo's, Nx's, Bazel's) in this control plane's own bucket, through superci-cache (it holds no keys).
c='PLANE_LINK';i=$(hostname -I|cut -d' ' -f1);u=http://$i:8257
[ -n "$i" ]&&printf 'TURBO_API=%s\nTURBO_TOKEN=superci\nTURBO_TEAM=superci\nNX_SELF_HOSTED_REMOTE_CACHE_SERVER=%s\nNX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN=superci\n' $u $u>>/home/runner/.env&&echo "build --remote_cache=$u/bazel">>/etc/bazel.bazelrc
(f=/usr/local/bin/superci-cache;curl -fsSL --retry 2 -m 30 -o $f "$c/program"&&chmod 755 $f&&$f wrap /home/runner/externals&&SUPERCI_CACHE_PLANE="$c" SUPERCI_CACHE_LISTEN="$i" setsid $f serve /home/runner/externals>/var/log/superci-cache.log 2>&1</dev/null&)>/dev/null 2>&1
"#;

/// A Linux machine's start-up script with `superci-cache` (see `cache`): `plane` the machine's own link to the
/// control plane's cache, said once (`$c`, before Docker is started again: `with_layers` uses it there too), and the
/// cache's part fetched from it (`cache_script`) before the runner. (AWS takes 16 KB of start-up script.)
pub fn with_cache(script: String, plane: &str) -> Result<String> {
    // (Its caller gives it an https link only.)
    if !plane.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b)) { return Err("invalid cache link".into()) }
    const RUNNER: &str = "cd /home/runner && ";
    let (before, after) = script.split_once(RUNNER).ok_or("no runner in the start-up script")?;
    let fetched = format!("{before}curl -fsSm30 --retry 2 $c/cache|bash &>/dev/null\n{RUNNER}{after}");
    let link = format!("c='{plane}'\n");
    Ok(match fetched.contains(DOCKER_RESTART) { true => fetched.replacen(DOCKER_RESTART, &format!("{link}{DOCKER_RESTART}"), 1), false => fetched.replacen("curl -fsSm30 --retry 2 $c/cache", &format!("{link}curl -fsSm30 --retry 2 $c/cache"), 1) })
}

/// What a Linux machine's link answers at `cache` (see `with_cache`): the cache's part of its start, `CACHE`.
pub fn cache_script(plane: &str) -> String { compact_lines(&CACHE.replace("PLANE_LINK", plane)) }

fn compact_lines(s: &str) -> String { s.lines().filter(|l| !l.trim_start().starts_with("# ")).map(|l| format!("{}\n", l.trim_start())).collect() }

/// What `with_layers` puts in place of the start script's restart of Docker (beside the runner): the control
/// plane's script for `superci-layers` (`layers_script`), which switches Docker to it and starts containerd and Docker
/// again itself, beside the runner as before; the restart as it was when that script cannot be had.
const LAYERS: &str = "curl -fsSm60 --retry 2 $c/layers|bash||(systemctl restart docker &>/dev/null &)\n";
const DOCKER_RESTART: &str = "(systemctl restart docker &>/dev/null &)\n";

/// A Linux machine's start-up script, with the cache's link already (`with_cache`: `$c`), with `superci-layers`.
/// (Given only what `with_keep` made: a script with the link, and its restart of Docker as `runner_user_data` writes it.)
pub fn with_layers(script: String) -> Result<String> {
    Ok(script.replacen(DOCKER_RESTART, LAYERS, 1))
}

/// What `with_keep` puts before a machine's end (after its runner): `superci-layers keep`, the program fetched when the
/// start did not set it up (see `layers::plan`), three minutes at most; nothing it does stops the machine going off.
/// (What fails before it can say so itself is said to the control plane: the job's record says why nothing was kept.)
const KEEP: &str = "f=/usr/local/bin/superci-layers;[ -x $f ]||curl -fsSL --retry 2 -m 60 -o $f \"$c/layers-program\"||curl -s -m 10 -d '{\"error\":\"its program could not be fetched\"}' \"$c/layers-kept\";chmod 755 $f;timeout 200 $f keep --link=\"$c\" --most=180 >>/var/log/superci-layers.log 2>&1||curl -s -m 10 -d \"{\\\"error\\\":\\\"it ended with status $?\\\"}\" \"$c/layers-kept\"\n";
/// What the machine's image has in containerd at its start (images it comes with), noted before the runner: none of
/// it is offered after the job.
const BOOT_BLOBS: &str = "ls /var/lib/containerd/io.containerd.content.v1.content/blobs/sha256 >/run/superci-boot-blobs 2>/dev/null\n";

/// A Linux machine's start-up script, with the cache's link already (`with_cache`: `$c`), that keeps the Docker images
/// its job pulled after the job (`KEEP`, before its last `poweroff`).
pub fn with_keep(script: String) -> Result<String> {
    if !script.contains("c='https://") { return Err("no cache link in the start-up script".into()) }
    let at = script.trim_end().rfind("\npoweroff").ok_or("no end in the start-up script")?;
    let kept = format!("{}\n{}{}", &script[..at], KEEP, &script[at + 1..]);
    // (A GitHub job's Linux script, the only one given here: its runner is there.)
    const RUNNER: &str = "cd /home/runner && ";
    Ok(kept.replacen(RUNNER, &format!("{BOOT_BLOBS}{RUNNER}"), 1))
}

/// What a machine's link answers at `layers` (see `LAYERS`): `superci-layers` fetched from its `layers-program` and
/// its `setup` run (it starts containerd and Docker again, beside the runner, with the index at `layers.json`); what
/// it says in its own log. Without the program, Docker restarted as the start script restarts it.
pub fn layers_script(link: &str) -> String {
    format!("exec >>/var/log/superci-layers.log 2>&1\nf=/usr/local/bin/superci-layers\ncurl -fsSL --retry 2 -m 60 -o $f '{link}/layers-program' && chmod 755 $f && $f setup --index-url='{link}/layers.json' || (systemctl restart docker &>/dev/null &)\n")
}

/// The same for a GitLab job's machine: GitLab's runner keeps its cache in "S3" at `superci-cache` here (path-style,
/// keys that are no secret: the program answers only what the control plane lets the job on this machine do).
const CACHE_GITLAB: &str = r#"# GitLab's cache in this control plane's own bucket, through superci-cache (it holds no keys).
c='PLANE_LINK';i=$(hostname -I|cut -d' ' -f1)
[ -n "$i" ]&&export CACHE_TYPE=s3 CACHE_SHARED=true CACHE_PATH=gl CACHE_S3_SERVER_ADDRESS=$i:8257 CACHE_S3_ACCESS_KEY=superci CACHE_S3_SECRET_KEY=superci CACHE_S3_BUCKET_NAME=superci CACHE_S3_BUCKET_LOCATION=us-east-1 CACHE_S3_INSECURE=true CACHE_S3_AUTHENTICATION_TYPE=access-key
(f=/usr/local/bin/superci-cache;curl -fsSL --retry 2 -m 30 -o $f "$c/program"&&chmod 755 $f&&SUPERCI_CACHE_PLANE="$c" SUPERCI_CACHE_LISTEN="$i" setsid $f serve /tmp>/var/log/superci-cache.log 2>&1</dev/null&)>/dev/null 2>&1
"#;

/// A GitLab job's machine's start-up script with `superci-cache`, before its runner.
pub fn with_cache_gitlab(script: String, plane: &str) -> Result<String> {
    // (Its caller gives it an https link only.)
    if !plane.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b)) { return Err("invalid cache link".into()) }
    const RUNNER: &str = "export GL_URL=";
    let (before, after) = script.split_once(RUNNER).ok_or("no runner in the start-up script")?;
    Ok(format!("{before}{}{RUNNER}{after}", CACHE_GITLAB.replace("PLANE_LINK", plane)))
}

/// The same for a Windows machine (PowerShell): the tools' settings, then `superci-cache` fetched, put in front of
/// the runner's Node and started, before the runner (Windows boots in minutes; this is a second of it).
const CACHE_WINDOWS: &str = r#"# GitHub's cache (and Turbo's, Nx's, Bazel's) in this control plane's own bucket, through superci-cache (it holds no keys).
try {
  $c = 'PLANE_LINK'; $f = 'C:\superci-cache.exe'
  $ip = (Get-NetIPConfiguration | Where-Object { $_.IPv4DefaultGateway } | Select-Object -First 1).IPv4Address.IPAddress
  $u = "http://${ip}:8257"
  Add-Content "$dir\.env" "TURBO_API=$u`nTURBO_TOKEN=superci`nTURBO_TEAM=superci`nNX_SELF_HOSTED_REMOTE_CACHE_SERVER=$u`nNX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN=superci"
  Add-Content "$env:ProgramData\bazel.bazelrc" "build --remote_cache=$u/bazel"
  Invoke-WebRequest -UseBasicParsing "$c/program" -OutFile $f
  & $f wrap "$dir\externals"
  $env:SUPERCI_CACHE_PLANE = $c; $env:SUPERCI_CACHE_LISTEN = $ip
  Start-Process -FilePath $f -ArgumentList 'serve', "$dir\externals" -WindowStyle Hidden -RedirectStandardError C:\superci-cache.log -RedirectStandardOutput C:\superci-cache.out
} catch { Write-Output "superci-cache: $_" }
"#;

/// A Windows machine's start-up script with `superci-cache` (see `cache`).
pub fn with_cache_windows(script: String, plane: &str) -> Result<String> {
    // (Its caller gives it an https link only.)
    if !plane.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b)) { return Err("invalid cache link".into()) }
    const RUNNER: &str = "Set-Location $dir\n";
    let (before, after) = script.split_once(RUNNER).ok_or("no runner in the start-up script")?;
    Ok(format!("{before}{}{RUNNER}{after}", CACHE_WINDOWS.replace("PLANE_LINK", plane)))
}

pub fn with_hello(script: String, url: &str) -> Result<String> {
    // (Its callers give it an https link only.)
    if !url.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-?=&".contains(&b)) { return Err("invalid hello link".into()) }
    const LOG: &str = "exec > >(tee /var/log/superci.log > /dev/console) 2>&1\n";
    const READ: &str = "# First reads from a new volume are slow";
    // (Its caller gives it a GitHub or GitLab job's script only, never a failing runner's: see `runner_user_data`.)
    let runner = if script.contains("cd /home/runner && ") { "cd /home/runner && " } else { "export GL_URL=" };
    let said = HELLO.replace("HELLO_URL", url);
    let script = script.replacen(LOG, &format!("{LOG}superci_t0=$(date +%s%3N); superci_up=$(cut -d' ' -f1 /proc/uptime)\n"), 1).replacen(READ, &format!("superci_t1=$(date +%s%3N)\n{READ}"), 1);
    let (before, after) = script.split_once(runner).ok_or("no runner in the start-up script")?;
    Ok(format!("{before}{said}{runner}{after}"))
}

/// A Linux start-up script as a machine is given it: without its lines that only explain and its indentation (they
/// are said here, and AWS takes 16,384 bytes at most). A Windows one as it is.
pub fn compact(script: &str) -> String {
    if !script.starts_with("#!/bin/bash") { return script.to_string() }
    // (Its indentation too: nothing in these scripts is a here-document, where it would count.)
    script.lines().filter(|l| !l.trim_start().starts_with("# ")).map(|l| format!("{}\n", l.trim_start())).collect()
}

/// The addresses at GitHub a runner with this just-in-time configuration reaches first (its service, and the
/// broker when it uses one): from the configuration's `.runner`.
pub fn runner_hosts(jit_config: &str) -> Vec<String> {
    use base64::Engine;
    let decode = |s: &str| base64::engine::general_purpose::STANDARD.decode(s.trim()).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let Some(runner) = decode(jit_config).and_then(|all| all[".runner"].as_str().and_then(decode)) else { return vec![] };
    ["ServerUrlV2", "serverUrlV2", "ServerUrl", "serverUrl"].iter()
        .filter_map(|k| runner[*k].as_str().and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string)))
        .filter(|host| host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)))
        .fold(vec![], |mut hosts, host| { if !hosts.contains(&host) { hosts.push(host) } hosts })
}

/// What the start-up script adds for a runner of GitHub's: its addresses at GitHub tried before it starts, every
/// second, 30 at most, beside the reads of its files. Its runner, refused once (GitHub's address for it not answering
/// for a moment), waits 30 s before it tries again; this waits only as long as the refusal lasts, and says how long.
fn reach_github(hosts: &[String]) -> String {
    if hosts.is_empty() { return String::new() }
    format!("(w=0;for h in {};do until curl -so/dev/null --connect-timeout 2 -m4 https://$h/||[ $w -ge 30 ];do sleep 1;w=$((w+1));done;done;echo $w>/tmp/superci-gh)&g=$!\n", hosts.join(" "))
}

pub fn runner_user_data(work: &crate::io::Work, max_minutes: u32) -> Result<String> {
    let (jit_config, fail) = match work {
        crate::io::Work::GitHub { jit } => (jit.as_str(), None),
        crate::io::Work::Fail { jit, why } => (jit.as_str(), Some(why.as_str())),
        crate::io::Work::GitLab { url, token } => {
            let safe = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b":/._-".contains(&b));
            if !safe(url) || !safe(token) { return Err("invalid GitLab runner settings".into()) }
            return Ok(format!(concat!("#!/bin/bash\nexec > >(tee /var/log/superci.log > /dev/console) 2>&1\n", bound!(), "\n{grow}{settings}export GL_URL={url} GL_TOKEN={token} GL_RUNNER_FLAGS=--docker-privileged\nbash -c '{script}' || true\npoweroff\n"),
                grow = GROW_ROOT, settings = KERNEL_SETTINGS, url = url, token = token, secs = max_minutes * 60, minutes = max_minutes + 1, script = crate::gitlab::RUNNER_SCRIPT.replace('\'', "'\\''")))
        }
    };
    let base64 = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    // (A failing runner's words are base64 as the control plane wrote them.)
    if !base64(jit_config) { return Err("invalid runner config".into()); }
    // A failing runner: its job-started hook says why and fails the job before any step.
    let (hook, run) = match fail {
        Some(why) => (format!("printf '#!/bin/bash\\necho {why} | base64 -d\\nexit 1\\n' > /tmp/superci-fail.sh && chmod 755 /tmp/superci-fail.sh\n"), "env ACTIONS_RUNNER_HOOK_JOB_STARTED=/tmp/superci-fail.sh ./run.sh"),
        None => (String::new(), "./run.sh"),
    };
    Ok(format!(concat!(r#"#!/bin/bash
exec > >(tee /var/log/superci.log > /dev/console) 2>&1
"#, bound!(), r#"
{grow}{settings}# Docker as on GitHub's runners: the runner's user may use it (the image leaves it to root), and it starts now,
# beside the runner, not at a job's first use of it (services, container jobs, docker steps).
usermod -aG docker runner 2>/dev/null||:
# Docker pulls sixteen of an image's layers at once (three unless told): a stack of many images (a local Supabase's
# eight, 99 layers) is pulled sooner, its own settings kept. (Restarted for it when the image started it already;
# nothing runs in it yet.)
f=/etc/docker/daemon.json;[ -e $f ]||echo {{}}>$f;jq '.["max-concurrent-downloads"]=16' $f>$f.n&&mv $f.n $f
(systemctl restart docker &>/dev/null &)
{reach}# First reads from a new volume are slow one at a time: the runner's own files are read 64 at a time first.
find /home/runner/bin -type f -print0 2>/dev/null|xargs -0 -r -P 64 -n 16 cat &>/dev/null
{wait}{hook}cd /home/runner && sudo -u runner -H {run} --jitconfig {jit_config}
poweroff
"#), jit_config = jit_config, hook = hook, run = run, secs = max_minutes * 60, minutes = max_minutes + 1, settings = KERNEL_SETTINGS,
        reach = if fail.is_some() { String::new() } else { reach_github(&runner_hosts(jit_config)) },
        wait = if fail.is_some() || runner_hosts(jit_config).is_empty() { "" } else { "wait $g\n" },
        // (Not for the runner that only says why a job failed: it uses no disk.)
        grow = if fail.is_some() { "" } else { GROW_ROOT }))
}

#[cfg(test)]
#[path = "aws_faults.rs"]
mod faults;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machines_whole_script_fits_what_aws_takes() {
        // AWS takes 16,384 bytes of start-up script. GitHub's runner configuration is the most of it (about 5,000
        // characters lately); everything SuperCI adds, at once, leaves it twice that.
        use base64::Engine;
        let b64 = |v: serde_json::Value| base64::engine::general_purpose::STANDARD.encode(v.to_string());
        let runner = b64(serde_json::json!({ "ServerUrl": "https://pipelinesghubeus5.actions.githubusercontent.com/AbC123/", "ServerUrlV2": "https://broker.actions.githubusercontent.com/" }));
        let jit = b64(serde_json::json!({ ".runner": runner, ".credentials": "A".repeat(7_280) }));
        assert!((9_900..10_100).contains(&jit.len()), "{}", jit.len());
        let script = runner_user_data_for(&crate::io::Work::GitHub { jit }, 360, "linux", Some("https://abcdefghijklmnopqrstuvwxyz012345.lambda-url.ap-southeast-2.on.aws/interrupted?runner=acmeci-abc123def456-123456789012&t=tokentokentokentokentoken")).unwrap();
        let script = with_hello(with_scratch(script, false), "https://abcdefghijklmnopqrstuvwxyz012345.lambda-url.ap-southeast-2.on.aws/hello?runner=acmeci-abc123def456-123456789012&t=tokentokentokentokentoken").unwrap();
        let script = with_cache(script, "https://abcdefghijklmnopqrstuvwxyz012345.lambda-url.ap-southeast-2.on.aws/cache/tokentokentokentokentoken").unwrap();
        let script = with_layers(script).unwrap();
        let script = compact(&script);
        assert!(script.contains("wait $g") && script.contains("$c/layers|bash||(systemctl restart docker") && script.contains("$c/cache|bash") && script.len() < 16_384, "{} bytes", script.len());
        assert!(script.find("c='https://").unwrap() < script.find("$c/layers|bash").unwrap() && script.find("$c/layers|bash").unwrap() < script.find("$c/cache|bash").unwrap() && script.find("$c/cache|bash").unwrap() < script.find("cd /home/runner && ").unwrap());
        assert_eq!((script.matches("&live=").count(), script.matches("&log=runner").count()), (1, 1), "each part of what the machine says, once");
        println!("{} bytes with a 10,000-character configuration", script.len());
    }

    /// The image maker's list of a build, as it publishes it (the shape of its release notes on 6 October 2026).
    #[test]
    fn a_copy_learns_its_source_from_the_makers_list() {
        let notes = "## Available images\n\n### `ubuntu22-full-arm64`\n\n| Region | AMI ID |\n| --- | --- |\n| us-east-1 | `ami-0c440955e5d86ed34` |\n| eu-west-1 | `ami-02e5d5a4bf1be6d80` |\n\n### `ubuntu24-full-x64`\n\n| Region | AMI ID |\n| --- | --- |\n| us-east-1 | `ami-04e964d6e3d2f3f4a` |\n| us-east-2 | `ami-0aaaaaaaaaaaaaaaa` |\n| eu-west-1 | `ami-0b8d70b1c8eb9af4a` |\n| ap-southeast-2 | `ami-0cccccccccccccccc` |\n\n### `ubuntu26-full-x64`\n\n| Region | AMI ID |\n| --- | --- |\n| us-east-1 | `ami-0dddddddddddddddd` |\n";
        assert_eq!(image_flavor("runs-on-v2.2-ubuntu24-full-x64-*"), "ubuntu24-full-x64");
        assert_eq!((image_flavor("runs-on-v2.2-windows25-full-x64-*"), image_flavor("runs-on-v2.2-ubuntu24-gpu-x64-*")), ("windows25-full-x64", "ubuntu24-gpu-x64"));
        let published = published_images(notes, "ubuntu24-full-x64");
        assert_eq!(published.len(), 4);
        assert_eq!(published[2], ("eu-west-1".to_string(), "ami-0b8d70b1c8eb9af4a".to_string()));
        // From the same continent when it is published there, else from the first region listed.
        assert_eq!(copy_source(&published, "eu-north-1").map(|(r, _)| r.as_str()), Some("eu-west-1"));
        assert_eq!(copy_source(&published, "ca-central-1").map(|(r, _)| r.as_str()), Some("us-east-1"));
        assert_eq!(copy_source(&published, "us-east-1").map(|(r, _)| r.as_str()), Some("us-east-2"), "never from the region itself");
        // A kind that is not in the list, and a list that is something else, give nothing: nothing is guessed.
        assert!(published_images(notes, "ubuntu24-full-arm64").is_empty() && published_images("No images this time.", "ubuntu24-full-x64").is_empty());
        assert!(published_images("### `ubuntu24-full-x64`\n\n| Region | AMI ID |\n| --- | --- |\n| us-east-1 | not-an-id |\n", "ubuntu24-full-x64").is_empty());
        assert_eq!(quota_link("eu-north-1", ON_DEMAND_CPU_QUOTA), "https://eu-north-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-1216C47A");
    }

    /// AWS's refusals as it words them (the first two as a project of its new sign-up answered on 12 October 2026,
    /// with stand-in ids; the others as its documentation gives them).
    #[test]
    fn a_refusal_says_whose_rule_it_is() {
        let scp = "UnauthorizedOperation: You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/AccountFullAccessRole/someone is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy: arn:aws:organizations::222222222222:policy/o-example001/service_control_policy/p-example1";
        let r = refusal(scp).unwrap();
        assert_eq!((r.action.as_str(), r.by, r.policy_id(), r.management_account()), ("ec2:DescribeVpcs", By::Organization, Some("p-example1"), Some("222222222222")));
        let on_resource = "UnauthorizedOperation: You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/AccountFullAccessRole/someone is not authorized to perform: ec2:RunInstances on resource: arn:aws:ec2:eu-north-1:111111111111:instance/* with an explicit deny in a service control policy: arn:aws:organizations::222222222222:policy/o-example001/service_control_policy/p-example2. Encoded authorization failure message: abc";
        let r = refusal(on_resource).unwrap();
        assert_eq!((r.action.as_str(), r.by, r.policy_id()), ("ec2:RunInstances", By::Organization, Some("p-example2")));
        for (said, by) in [
            ("AccessDenied: User: arn:aws:iam::111111111111:user/ci is not authorized to perform: lambda:CreateFunction on resource: arn:aws:lambda:us-east-1:111111111111:function:x because no identity-based policy allows the lambda:CreateFunction action", By::Identity),
            ("AccessDenied: User: arn:aws:iam::111111111111:role/ci is not authorized to perform: iam:CreateRole on resource: arn:aws:iam::111111111111:role/x with an explicit deny in a permissions boundary", By::Boundary),
            ("AccessDenied: User: arn:aws:sts::111111111111:assumed-role/ci/s is not authorized to perform: s3:GetObject on resource: arn:aws:s3:::b/k with an explicit deny in a resource control policy", By::OrganizationResources),
            ("AccessDenied: User: arn:aws:sts::111111111111:assumed-role/ci/s is not authorized to perform: ssm:PutParameter on resource: x because no session policy allows the ssm:PutParameter action", By::Session),
            ("AccessDenied: User: arn:aws:sts::111111111111:assumed-role/ci/s is not authorized to perform: dynamodb:CreateTable on resource: x because no service control policy allows the dynamodb:CreateTable action", By::Organization),
            ("AccessDeniedException: You don't have permissions to access this resource.", By::Unsaid),
            ("UnauthorizedOperation: You are not authorized to perform this operation. Encoded authorization failure message: abc", By::Unsaid),
        ] { assert_eq!(refusal(said).map(|r| r.by), Some(by), "{said}") }
        // What is no refusal is none: a machine that would have started, a limit, a missing image.
        for said in ["DryRunOperation: Request would have succeeded, but DryRun flag is set.", "VcpuLimitExceeded: You have requested more vCPU capacity than your current vCPU limit of 5 allows", "InvalidAMIID.NotFound: The image id '[ami-0]' does not exist", "no image 135269210855/x in eu-north-1"] {
            assert_eq!(refusal(said), None, "{said}");
        }
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
                // (Each VPC with the tag it was made with, `vpc-1|key=value`; a tag filter honoured.)
                "DescribeVpcs" => made("vpc").iter().filter(|v| get("Filter.1.Name").strip_prefix("tag:").is_none_or(|k| v.ends_with(&format!("|{k}={}", get("Filter.1.Value.1"))))).map(|v| format!("<item><vpcId>{}</vpcId></item>", v.split('|').next().unwrap())).collect(),
                "CreateVpc" => { make("vpc", format!("vpc-1|{}={}", get("TagSpecification.1.Tag.1.Key"), get("TagSpecification.1.Tag.1.Value"))); "<vpc><vpcId>vpc-1</vpcId></vpc>".into() }
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
                "DescribeVpcEndpoints" => made("vpce").iter().map(|e| format!("<item><vpcEndpointId>{e}</vpcEndpointId></item>")).collect(),
                "CreateVpcEndpoint" => { assert_eq!((get("ServiceName").as_str(), get("VpcEndpointType").as_str(), get("RouteTableId.1").as_str()), ("com.amazonaws.us-east-1.s3", "Gateway", "rtb-1")); format!("<vpcEndpointId>{}</vpcEndpointId>", make("vpce", "vpce-1".into())) }
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
        assert_eq!(made, ["CreateVpc", "CreateInternetGateway", "AttachInternetGateway", "CreateRouteTable", "CreateRoute", "CreateSubnet", "CreateSubnet", "CreateVpcEndpoint", "CreateSecurityGroup"]);
        assert!(ec2.made.borrow().iter().any(|(k, v)| k == "subnet" && v.ends_with("|10.212.16.0/20")), "a /20 each");
        // Again: nothing new; and it is found.
        ec2.calls.borrow_mut().clear();
        assert_eq!(futures::executor::block_on(make_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap(), n);
        assert!(!ec2.calls.borrow().iter().any(|c| c.starts_with("Create") && c != "CreateRoute" || c.starts_with("Attach")));
        assert_eq!(futures::executor::block_on(find_network(&ec2, "us-east-1", &creds, "p1", 0)).unwrap(), Some(n.clone()));
        // It is the account's shared one: another control plane finds and uses it, making nothing.
        assert!(ec2.made.borrow().iter().any(|(k, v)| k == "vpc" && v == "vpc-1|superci-network=shared"), "{:?}", ec2.made.borrow());
        ec2.calls.borrow_mut().clear();
        assert_eq!(futures::executor::block_on(make_network(&ec2, "us-east-1", &creds, "p2", 0)).unwrap(), n);
        assert!(!ec2.calls.borrow().iter().any(|c| c == "CreateVpc"));
        assert_eq!(futures::executor::block_on(find_network(&ec2, "us-east-1", &creds, "p2", 0)).unwrap(), Some(n));
    }

    #[test]
    fn a_control_plane_with_a_network_of_its_own_from_before_keeps_using_it() {
        let (ec2, creds) = (Ec2::default(), Credentials { access_key_id: "A".into(), secret_access_key: "s".into(), session_token: None, expires_at_ms: u64::MAX });
        ec2.made.borrow_mut().push(("vpc".into(), "vpc-old|superci-plane=old".into()));
        ec2.made.borrow_mut().push(("vpc".into(), "vpc-1|superci-network=shared".into()));
        assert_eq!(block_on_vpc(&ec2, &creds, "old"), Some(("vpc-old".to_string(), Whose::Own("old"))));
        assert_eq!(block_on_vpc(&ec2, &creds, "new"), Some(("vpc-1".to_string(), Whose::Shared)));
    }

    fn block_on_vpc<'a>(ec2: &Ec2, creds: &Credentials, plane: &'a str) -> Option<(String, Whose<'a>)> { futures::executor::block_on(network_vpc(ec2, "us-east-1", creds, plane, 0)).unwrap() }

    #[test]
    fn iso_times() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-09-30T16:11:43Z"), Some(1_790_784_703_000));
        assert_eq!(parse_iso_ms("2024-02-29T12:00:00.123Z"), Some(1_709_208_000_000));
    }

    #[test]
    fn a_windows_machine_starts_the_cache_before_its_runner() {
        let script = runner_user_data_for(&crate::io::Work::GitHub { jit: "A".repeat(10_000) }, 360, "windows", Some("https://abcdefghijklmnopqrstuvwxyz012345.lambda-url.ap-southeast-2.on.aws/interrupted?runner=acmeci-abc123def456-123456789012&t=tokentokentokentokentoken")).unwrap();
        let script = with_cache_windows(script, "https://abcdefghijklmnopqrstuvwxyz012345.lambda-url.ap-southeast-2.on.aws/cache/tokentokentokentokentoken").unwrap();
        let at = |s: &str| script.find(s).unwrap_or_else(|| panic!("{s}"));
        assert!(at("Invoke-WebRequest -UseBasicParsing \"$c/program\"") < at("& $f wrap \"$dir\\externals\"") && at("& $f wrap") < at("Set-Location $dir") && at("Set-Location $dir") < at("& .\\run.cmd"));
        assert!(script.contains("TURBO_API=$u") && script.contains("$env:ProgramData\\bazel.bazelrc") && script.contains("-ArgumentList 'serve'"));
        assert!(script.len() < 16_384, "{} bytes", script.len());
        assert!(with_cache_windows("no runner here".into(), "https://p.example/cache/t").is_err());
    }

    #[test]
    fn presigned_link_as_aws_documents_it() {
        // AWS's own example of a presigned GET (S3 docs, "Authenticating Requests: Using Query Parameters").
        let creds = Credentials { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), session_token: None, expires_at_ms: 0 };
        let link = presign("GET", "https://examplebucket.s3.amazonaws.com/test.txt", &creds, "us-east-1", 86400, 1_369_353_600_000).unwrap();
        assert!(link.ends_with("&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"), "{link}");
        assert!(link.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host"), "{link}");
        assert!(presign("GET", "https://b.s3.amazonaws.com/a b", &creds, "us-east-1", 60, 0).is_err());
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
    fn a_runner_waits_for_its_github_address_to_answer_before_it_starts() {
        use base64::Engine;
        let b64 = |v: serde_json::Value| base64::engine::general_purpose::STANDARD.encode(v.to_string());
        let jit = b64(serde_json::json!({ ".runner": b64(serde_json::json!({ "AgentId": 5, "ServerUrl": "https://pipelinesghubeus5.actions.githubusercontent.com/AbC123/", "ServerUrlV2": "https://broker.actions.githubusercontent.com/", "UseV2Flow": true })), ".credentials": "x" }));
        assert_eq!(runner_hosts(&jit), ["broker.actions.githubusercontent.com", "pipelinesghubeus5.actions.githubusercontent.com"]);
        let script = runner_user_data(&crate::io::Work::GitHub { jit: jit.clone() }, 60).unwrap();
        assert!(script.contains("for h in broker.actions.githubusercontent.com pipelinesghubeus5.actions.githubusercontent.com;do until curl") && script.contains("[ $w -ge 30 ]"), "{script}");
        assert!(script.find("&g=$!").unwrap() < script.find("xargs -0").unwrap() && script.find("xargs -0").unwrap() < script.find("wait $g").unwrap() && script.find("wait $g").unwrap() < script.find("./run.sh").unwrap());
        assert!(runner_hosts("not-base64!").is_empty() && !runner_user_data(&crate::io::Work::GitHub { jit: "abc".into() }, 60).unwrap().contains("wait $g"));
        // An address with characters a script should not run, and one said twice: each passed over.
        let odd = b64(serde_json::json!({ ".runner": b64(serde_json::json!({ "ServerUrl": "https://a_b.example/", "serverUrl": "https://c.example/", "ServerUrlV2": "https://c.example/" })) }));
        assert_eq!(runner_hosts(&odd), ["c.example"]);
    }

    #[test]
    fn a_windows_machine_says_how_its_start_went_and_sends_its_runners_lines() {
        use base64::Engine;
        let b64 = |v: serde_json::Value| base64::engine::general_purpose::STANDARD.encode(v.to_string());
        let jit = b64(serde_json::json!({ ".runner": b64(serde_json::json!({ "ServerUrl": "https://pipelinesghubeus5.actions.githubusercontent.com/AbC123/", "ServerUrlV2": "https://broker.actions.githubusercontent.com/" })), ".credentials": "A".repeat(7_280) }));
        let script = runner_user_data_for(&crate::io::Work::GitHub { jit }, 360, "windows", Some("https://plane.example/interrupted?runner=acmeci-abc-1&t=tokentokentokentoken")).unwrap();
        assert!(script.contains("foreach ($h in @('broker.actions.githubusercontent.com','pipelinesghubeus5.actions.githubusercontent.com'))") && script.contains("TcpClient($h, 443)"), "{script}");
        let said = with_hello_windows(script, "https://plane.example/hello?runner=acmeci-abc-1&t=tokentokentokentoken").unwrap();
        let runner = said.find("& .\\run.cmd --jitconfig").unwrap();
        assert!(said.find("&gh=$w").unwrap() < runner && said.find("&live=$($f.Name)&at=$p").unwrap() < runner && said.find("$t0 = ").unwrap() < said.find("&script=$t0").unwrap());
        assert!(said.len() < 16_384, "{} bytes", said.len());
        assert!(with_hello_windows(said.clone(), "https://x/'; reboot; '").is_err());
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
        // The root filesystem is grown to the disk's size before the runner starts (the image's stays 29 GB by
        // itself, whatever disk was asked for), on GitHub's and GitLab's machines alike; and C: on Windows.
        let grow = r#"growpart "/dev/$disk" "$part" || true; resize2fs "$root" 2>/dev/null || xfs_growfs / 2>/dev/null || true"#;
        assert!(script.contains(grow) && script.find("growpart").unwrap() < script.find("./run.sh").unwrap(), "{script}");
        assert!(gl.contains(grow) && gl.find("growpart").unwrap() < gl.find("export GL_URL=").unwrap(), "{gl}");
        // The kernel's settings, before Docker restarts and before the runner (GitHub's) or GitLab's runner starts,
        // kept in /etc/sysctl.d: among them, a container's processes may signal each other (moby/moby#54099).
        let settings = "kernel.apparmor_restrict_unprivileged_unconfined=0";
        assert!(script.contains(settings) && script.find(settings).unwrap() < script.find("systemctl restart docker").unwrap() && script.find(settings).unwrap() < script.find("./run.sh").unwrap(), "{script}");
        assert!(gl.contains(settings) && gl.find(settings).unwrap() < gl.find("export GL_URL=").unwrap(), "{gl}");
        assert!(compact(&script).contains("sysctl -eqp /etc/sysctl.d/99-superci.conf||:\n") && script.contains(">/etc/sysctl.d/99-superci.conf\n"));
        let win = runner_user_data_for(&gh("abc"), 70, "windows", None).unwrap();
        assert!(win.contains("Resize-Partition -DriveLetter C -Size $max") && win.find("Resize-Partition").unwrap() < win.find("run.cmd --jitconfig").unwrap(), "{win}");
        // (The runner that only says why a job failed uses no disk.)
        assert!(!runner_user_data(&crate::io::Work::Fail { jit: "abc".into(), why: "abc".into() }, 10).unwrap().contains("growpart"));
        // A fast place for a job's files, when its label asks: memory, or the machine's own NVMe disk, mounted before
        // Docker and the runner start, with Docker's files, the work folder and /tmp bound from it.
        let ram = with_scratch(script.clone(), true);
        assert!(ram.contains(r#"mount -t tmpfs -o size=75% tmpfs "$scratch""#) && !ram.contains("Instance Storage") && ram.contains("for d in /var/lib/docker /var/lib/containerd /var/lib/superci-layers /home/runner/work /tmp;"), "{ram}");
        assert!(ram.find("growpart").unwrap() < ram.find("tmpfs").unwrap() && ram.find("mount --bind").unwrap() < ram.find("systemctl restart docker").unwrap() && ram.find("mount --bind").unwrap() < ram.find("./run.sh").unwrap());
        // Without it, the machine's own NVMe disk when it has one: found, formatted and mounted; nothing on a machine without.
        let local = with_scratch(script.clone(), false);
        assert!(local.contains("/Instance Storage/") && local.contains("mkfs.ext4 -q -F") && !local.contains("tmpfs") && !local.contains("MOUNT_SCRATCH") && local.contains(r#"if mountpoint -q "$scratch"; then"#), "{local}");
        assert!(with_scratch(gl.clone(), true).contains("tmpfs") && with_scratch(gl.clone(), false).contains("/Instance Storage/"));
        let failing = runner_user_data(&crate::io::Work::Fail { jit: "abc".into(), why: "abc".into() }, 10).unwrap();
        assert_eq!(with_scratch(failing.clone(), true), failing);
        // A machine says how its start went, with a link of its own, in the background and before its runner starts.
        let hello = with_hello(script.clone(), "https://plane.example/hello?runner=superci-abc-7&t=tokentokentokentoken").unwrap();
        assert!(hello.contains("superci_t0=$(date +%s%3N); superci_up=$(cut -d' ' -f1 /proc/uptime)") && hello.contains("superci_t1=$(date +%s%3N)\n# First reads") && hello.contains(r#"curl -s -m 5 -X POST "https://plane.example/hello?runner=superci-abc-7&t=tokentokentokentoken&boot=$superci_up&script=$superci_t0&read=${superci_t1:-0}&ready=$superci_t2&gh=$(cat /tmp/superci-gh 2>/dev/null)""#), "{hello}");
        // (And what its runner logged, when the runner was slow to listen: after it listens, and only then.)
        assert!(hello.find("&listening=$superci_t3").unwrap() < hello.find("&log=runner").unwrap() && hello.contains("-gt 10000") && hello.contains("/home/runner/_diag/Runner_*.log") && !hello.contains("HELLO_URL"), "{hello}");
        assert!(hello.find("superci_t0=").unwrap() < hello.find("growpart").unwrap() && hello.find("&listening=").unwrap() < hello.find("./run.sh").unwrap());
        assert!(with_hello(script.clone(), "https://x/'; reboot; '").is_err());
        assert!(with_hello(gl.clone(), "https://plane.example/hello?gl=g-1&t=tokentokentokentoken").unwrap().contains("&ready=$superci_t2"));
        // The disk's speed above what AWS includes is in the price of the hour: 1,000 MiB a second and 4,000 IOPS on
        // 60 GB, where a GB is $0.08 a month, add $0.055.
        let (plain, fast) = (extras_usd_per_hour(0.08, 60, 125, 3000, false), extras_usd_per_hour(0.08, 60, 1000, 4000, false));
        assert!((plain - 60.0 * 0.08 / 730.0).abs() < 1e-9 && (fast - plain - 0.0548).abs() < 0.0002, "{plain} {fast}");
    }

    #[test]
    fn a_spot_machine_of_each_kind_watches_for_its_notice_before_its_runner_starts() {
        let gh = crate::io::Work::GitHub { jit: "abc".into() };
        let link = "https://plane.example/interrupted?runner=r-1&t=tok";
        // Windows: a background job of the start-up script; the job's process is ended, then the runner; and the
        // machine powers off whatever shutdown was scheduled before.
        let win = runner_user_data_for(&gh, 70, "windows", Some(link)).unwrap();
        assert!(win.contains("Start-Job") && win.contains(&format!("-Method Post '{link}'")) && win.contains("Stop-Process -Name Runner.Worker -Force"));
        assert!(win.find("spot/instance-action").unwrap() < win.find(".\\run.cmd --jitconfig abc").unwrap());
        assert!(win.ends_with("shutdown.exe /a\nshutdown.exe /s /f /t 0\n</powershell>\n"));
        assert!(!runner_user_data_for(&gh, 70, "windows", None).unwrap().contains("Start-Job"), "on-demand: nothing to watch");
        // Docker is the job's to use on a Linux machine, as on GitHub's runners: its user is let in before the runner starts.
        let linux = runner_user_data_for(&gh, 70, "linux", None).unwrap();
        assert!(linux.find("usermod -aG docker runner").unwrap() < linux.find("sudo -u runner").unwrap() && linux.contains("systemctl restart docker"));
        // (Many layers at once, set before Docker starts.)
        assert!(linux.find(r#"jq '.["max-concurrent-downloads"]=16'"#).unwrap() < linux.find("systemctl restart docker").unwrap());
        // (Its job's servers' ports kept from the machine's own connections, before Docker or the runner opens any.)
        assert!(linux.find("net.ipv4.ip_local_reserved_ports=50000-59999").unwrap() < linux.find("systemctl restart docker").unwrap());
        // Linux: GitHub's runner is interrupted, GitLab's ended; a failing runner watches nothing.
        let linux = runner_user_data_for(&gh, 70, "linux", Some(link)).unwrap();
        assert!(linux.contains(&format!("-X POST '{link}'")) && linux.contains("--retry 4") && linux.contains("pkill -INT -f Runner.Listener"));
        // The job has what is left of AWS's two minutes, less twenty seconds, to end by itself: the runner is watched
        // until then (by when AWS says the machine goes), and stopped only if it is still going.
        let (told, waits, stops) = (linux.find("-X POST 'https://plane").unwrap(), linux.find("while [ \"$(date +%s)\" -lt \"$END\" ]").unwrap(), linux.find("pkill -INT -f Runner.Listener").unwrap());
        assert!(told < waits && waits < stops && linux.contains("END=$(( AT - 20 ))") && linux.contains("pgrep -f 'Runner.Listener'") && !linux.contains("RUNNER_PROCESS"), "{linux}");
        assert!(win.contains("AddSeconds(-40)") && win.find("AddSeconds(-40)").unwrap() < win.find("Stop-Process -Name Runner.Worker -Force").unwrap());
        let gl = runner_user_data_for(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "glrt-a".into() }, 70, "linux", Some(link)).unwrap();
        assert!(gl.contains("pkill -TERM -f /tmp/gitlab-runner") && gl.contains("pgrep -f '/tmp/gitlab-runner'") && !gl.contains("STOP_RUNNER") && !gl.contains("NOTICE_URL") && !gl.contains("RUNNER_PROCESS"));
        assert!(!runner_user_data_for(&crate::io::Work::Fail { jit: "abc".into(), why: "abc".into() }, 10, "linux", Some(link)).unwrap().contains("instance-action"));
        // A link is only ever what a shell reads as one word.
        assert!(runner_user_data_for(&gh, 70, "linux", Some("https://x/'; reboot; '")).is_err() && runner_user_data_for(&gh, 70, "windows", Some("http://x/y")).is_err());
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
