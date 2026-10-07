//! AWS from the dashboard with nothing installed: you sign in with AWS in the browser (AWS Sign-In's OAuth flow for
//! local developer tools, the one `aws login` uses: PKCE, a redirect back to 127.0.0.1, DPoP-bound tokens), and the
//! dashboard gets short-lived credentials (15 minutes, renewed for up to 12 hours) with your console permissions. With
//! them it creates what the control plane needs in your account through AWS's own APIs.
use sha2::{Digest, Sha256};

use superci_core::aws::{self, Credentials};
use superci_core::crypto::{b64url, random_token, SigningKeyStore};
use superci_core::io::{self, Http, Request, Response};

use crate::Result;

/// AWS's public client for developer tools on the same machine as the browser.
const CLIENT_ID: &str = "arn:aws:signin:::devtools/same-device";

fn signin_base(region: &str) -> String { format!("https://{region}.signin.aws.amazon.com") }

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// The blocking HTTP client behind core's async calls.
pub struct Blocking(ureq::Agent);

impl Blocking {
    pub fn new() -> Self { Blocking(ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(60))).build().into()) }
}

#[async_trait::async_trait(?Send)]
impl Http for Blocking {
    async fn send(&self, r: Request) -> io::Result<Response> {
        let mut builder = ureq::http::Request::builder().method(r.method.as_str()).uri(r.url.as_str());
        for (k, v) in &r.headers { builder = builder.header(k.as_str(), v.as_str()) }
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = self.0.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        let body = response.body_mut().read_to_vec().map_err(|e| e.to_string())?;
        Ok(Response { status, headers, body })
    }
}

/// How an error starts when AWS says a sign-in is over (see `Session::credentials`).
pub const ENDED: &str = "Your AWS sign-in has ended";

/// A sign-in on its way: sent to AWS's page, waiting for its redirect back.
pub struct Pending { verifier: String, pub state: String, key: SigningKeyStore, region: String, redirect: String }

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub region: String,
    pub account_id: String,
    /// Who signed in (an ARN), shown on the dashboard.
    pub who: String,
    creds: Credentials,
    refresh_token: String,
    dpop: SigningKeyStore,
}

/// AWS's sign-in page for this dashboard; it redirects back to `redirect` (http://127.0.0.1:<port>/oauth/callback).
pub fn authorize(region: &str, redirect: &str) -> (String, Pending) {
    // As the AWS CLI does: a 64-character verifier from the unreserved set, and a UUID for state.
    let verifier = random_token(48);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = uuid_v4();
    let q = url::form_urlencoded::Serializer::new(String::new()).extend_pairs([
        ("response_type", "code"), ("client_id", CLIENT_ID), ("state", &state), ("code_challenge_method", "SHA-256"), ("scope", "openid"),
        ("redirect_uri", redirect), ("code_challenge", &challenge),
    ]).finish();
    (format!("{}/v1/authorize?{q}", signin_base(region)), Pending { verifier, state: state.clone(), key: SigningKeyStore::generate(), region: region.into(), redirect: redirect.into() })
}

/// The same sign-in, by way of AWS's sign-out: AWS's sign-in answers "400 Bad Request" when the browser still holds a
/// console session that has ended on AWS's side (aws/aws-cli#10186), and signing out clears it. AWS's sign-out goes on
/// to an address of AWS's own, here the sign-in.
pub fn signed_out_first(link: &str) -> String {
    format!("https://signin.aws.amazon.com/oauth?Action=logout&redirect_uri={}", url::form_urlencoded::byte_serialize(link.as_bytes()).collect::<String>())
}

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The token endpoint, with a DPoP proof from the key this sign-in is bound to.
fn token_call(region: &str, key: &SigningKeyStore, body: serde_json::Value) -> Result<serde_json::Value> {
    let url = format!("{}/v1/token", signin_base(region));
    let proof = key.dpop(&url, now_ms() / 1000)?;
    let req = Request::new("POST", &url).with_header("content-type", "application/json").with_header("dpop", &proof).with_body(body.to_string());
    let r = futures::executor::block_on(Blocking::new().send(req))?;
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
    if r.status >= 300 {
        let kind = v["error"].as_str().unwrap_or_default();
        if kind == "INSUFFICIENT_PERMISSIONS" {
            return Err("AWS says this sign-in may not be used by developer tools: your user or role needs the SignInLocalDevelopmentAccess policy (an administrator can add it in IAM).".into());
        }
        return Err(format!("AWS sign-in: {} {}", r.status, if v.is_null() { r.body_text() } else { v.to_string() }));
    }
    Ok(v)
}

fn session_from(region: &str, key: SigningKeyStore, v: &serde_json::Value, previous: Option<&Session>) -> Result<Session> {
    let t = &v["accessToken"];
    let s = |x: &serde_json::Value, k: &str| x[k].as_str().map(str::to_string).ok_or_else(|| format!("AWS sign-in answer without {k}"));
    let creds = Credentials { access_key_id: s(t, "accessKeyId")?, secret_access_key: s(t, "secretAccessKey")?, session_token: Some(s(t, "sessionToken")?),
        expires_at_ms: now_ms() + v["expiresIn"].as_u64().unwrap_or(900) * 1000 };
    let who = match (v["idToken"].as_str(), previous) {
        (Some(id), _) => {
            let payload = id.split('.').nth(1).ok_or("malformed id token")?;
            use base64::Engine;
            let claims: serde_json::Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            claims["sub"].as_str().unwrap_or_default().to_string()
        }
        (None, Some(p)) => p.who.clone(),
        (None, None) => String::new(),
    };
    let account_id = who.split(':').nth(4).map(str::to_string).filter(|a| a.len() == 12).or_else(|| previous.map(|p| p.account_id.clone())).unwrap_or_default();
    Ok(Session { region: region.into(), account_id, who, creds, refresh_token: s(v, "refreshToken").or_else(|_| previous.map(|p| p.refresh_token.clone()).ok_or("no refresh token"))?, dpop: key })
}

/// The redirect's code becomes a session.
pub fn exchange(p: Pending, code: &str) -> Result<Session> {
    let v = token_call(&p.region, &p.key, serde_json::json!({ "clientId": CLIENT_ID, "grantType": "authorization_code", "code": code, "codeVerifier": p.verifier, "redirectUri": p.redirect }))?;
    session_from(&p.region, p.key, &v, None)
}

impl Session {
    /// Credentials good for at least two more minutes, renewing them when needed.
    pub fn credentials(&mut self) -> Result<Credentials> {
        if self.creds.expires_at_ms < now_ms() + 120_000 {
            let v = token_call(&self.region, &self.dpop, serde_json::json!({ "clientId": CLIENT_ID, "grantType": "refresh_token", "refreshToken": self.refresh_token }))
                // AWS answering that the sign-in is over (its refresh token lasts as long as the console session, twelve
                // hours at most) is said as that; AWS not answering leaves the sign-in as it is.
                .map_err(|e| if e.starts_with("AWS sign-in: 4") || e.starts_with("AWS says") { format!("{ENDED} ({e})") } else { e })?;
            *self = session_from(&self.region.clone(), self.dpop.clone(), &v, Some(self))?;
        }
        Ok(self.creds.clone())
    }
}

/// What a Cloudflare-hosted control plane needs in AWS: its identity provider, the role that trusts only it, the role's
/// launch-and-stop policy, and Spot's service role. Safe to repeat. Returns the role's ARN.
pub fn connect_runners(creds: &Credentials, account_id: &str, plane_url: &str, plane_id: &str, audience: &str) -> Result<String> {
    let http = Blocking::new();
    let host = url::Url::parse(plane_url).ok().and_then(|u| u.host_str().map(str::to_string)).ok_or("bad control plane URL")?;
    let iam = |action: &str, params: serde_json::Value| futures::executor::block_on(aws::iam(&http, creds, action, params, now_ms()));
    let exists = |r: Result<String>| match r { Err(e) if e.starts_with("EntityAlreadyExists") => Ok(()), Err(e) => Err(e), Ok(_) => Ok(()) };
    let provider_arn = format!("arn:aws:iam::{account_id}:oidc-provider/{host}");
    exists(iam("CreateOpenIDConnectProvider", serde_json::json!({ "Url": plane_url, "ClientIDList": { "member": [audience] },
        "Tags": { "member": [{ "Key": "superci-plane", "Value": plane_id }] } })))?;
    let role = format!("superci-plane-{plane_id}");
    let trust = aws::trust_policy(&provider_arn, &host, plane_id, audience).to_string();
    match iam("CreateRole", serde_json::json!({ "RoleName": role, "AssumeRolePolicyDocument": trust, "MaxSessionDuration": 3600,
        "Description": format!("Assumed by the SuperCI control plane {plane_url} (OpenID Connect) to run GitHub Actions jobs."),
        "Tags": { "member": [{ "Key": "superci-plane", "Value": plane_id }] } })) {
        Err(e) if e.starts_with("EntityAlreadyExists") => { iam("UpdateAssumeRolePolicy", serde_json::json!({ "RoleName": role, "PolicyDocument": trust }))?; }
        r => { r?; }
    }
    iam("PutRolePolicy", serde_json::json!({ "RoleName": role, "PolicyName": "runner-machines", "PolicyDocument": aws::runner_policy(account_id, plane_id).to_string() }))?;
    match iam("CreateServiceLinkedRole", serde_json::json!({ "AWSServiceName": "spot.amazonaws.com" })) {
        Err(e) if e.starts_with("InvalidInput") || e.starts_with("EntityAlreadyExists") => {} // it exists already
        r => { r?; }
    }
    Ok(format!("arn:aws:iam::{account_id}:role/{role}"))
}

/// What a control plane's runner role (`superci-plane-<id>`, made by `connect_runners` or with an AWS control plane)
/// lacks of what this version asks for (`permissions::AWS_RUNNER`), read from IAM; and its own network, if a region
/// its machines use has none.
pub fn runner_role_missing(creds: &Credentials, account_id: &str, plane_id: &str, regions: &[String]) -> Result<Vec<&'static superci_core::permissions::Need>> {
    let http = Blocking::new();
    let have = futures::executor::block_on(aws::iam(&http, creds, "GetRolePolicy", serde_json::json!({ "RoleName": format!("superci-plane-{plane_id}"), "PolicyName": "runner-machines" }), now_ms()));
    let have = match have { Ok(xml) => policy_document(&xml), Err(e) if e.starts_with("NoSuchEntity") => None, Err(e) => return Err(e) };
    let mut missing = superci_core::permissions::aws_runner_missing(have.as_ref(), account_id, plane_id);
    if regions.iter().any(|r| matches!(futures::executor::block_on(aws::find_network(&http, r, creds, plane_id, now_ms())), Ok(None))) {
        missing.push(&superci_core::permissions::AWS_NETWORK);
    }
    Ok(missing)
}

/// Gives a control plane's runner role what this version asks for, and its own network in each region.
pub fn give_runner_role(creds: &Credentials, account_id: &str, plane_id: &str, regions: &[String]) -> Result<()> {
    let http = Blocking::new();
    futures::executor::block_on(aws::iam(&http, creds, "PutRolePolicy", serde_json::json!({ "RoleName": format!("superci-plane-{plane_id}"), "PolicyName": "runner-machines",
        "PolicyDocument": aws::runner_policy(account_id, plane_id).to_string() }), now_ms()))?;
    make_networks(creds, plane_id, regions)
}

/// The control plane's own network in each region (`aws::make_network`), the regions at once.
pub fn make_networks(creds: &Credentials, plane_id: &str, regions: &[String]) -> Result<()> {
    let made: Vec<Result<()>> = std::thread::scope(|s| {
        let each: Vec<_> = regions.iter().map(|r| s.spawn(move || futures::executor::block_on(aws::make_network(&Blocking::new(), r, creds, plane_id, now_ms()))
            .map(|_| ()).map_err(|e| format!("{r}: {e}")))).collect();
        each.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("making a network stopped".into()))).collect()
    });
    made.into_iter().collect()
}

/// Removes the control plane's machines and networks in these regions (all SuperCI offers, when it is deleted).
pub fn delete_networks(creds: &Credentials, plane_id: &str, regions: &[&str]) -> Result<()> {
    let sleep = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
    let done: Vec<Result<()>> = std::thread::scope(|s| {
        let each: Vec<_> = regions.iter().map(|r| s.spawn(move || {
            futures::executor::block_on(aws::delete_network(&Blocking::new(), r, creds, plane_id, &now_ms, &sleep)).map_err(|e| format!("{r}: {e}"))
        })).collect();
        each.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("removing a network stopped".into()))).collect()
    });
    done.into_iter().collect()
}

/// The policy in an IAM GetRolePolicy answer (URL-encoded JSON in XML).
fn policy_document(xml: &str) -> Option<serde_json::Value> {
    let encoded = aws::xml_tag(xml, "PolicyDocument")?;
    let decoded = url::form_urlencoded::parse(format!("d={}", encoded.replace('+', "%2B")).as_bytes()).next()?.1.to_string();
    serde_json::from_str(&decoded).ok()
}

/// Removes what `connect_runners` made for a control plane elsewhere: its role (with its policy) and the identity
/// provider that trusts it. Safe to repeat.
pub fn disconnect_runners(creds: &Credentials, account_id: &str, plane_url: &str, plane_id: &str) -> Result<()> {
    let http = Blocking::new();
    let host = url::Url::parse(plane_url).ok().and_then(|u| u.host_str().map(str::to_string)).ok_or("bad control plane URL")?;
    let iam = |action: &str, params: serde_json::Value| futures::executor::block_on(aws::iam(&http, creds, action, params, now_ms()));
    let gone = |r: Result<String>| match r { Err(e) if e.starts_with("NoSuchEntity") => Ok(()), Err(e) => Err(e), Ok(_) => Ok(()) };
    let role = format!("superci-plane-{plane_id}");
    gone(iam("DeleteRolePolicy", serde_json::json!({ "RoleName": role, "PolicyName": "runner-machines" })))?;
    gone(iam("DeleteRole", serde_json::json!({ "RoleName": role })))?;
    gone(iam("DeleteOpenIDConnectProvider", serde_json::json!({ "OpenIDConnectProviderArn": format!("arn:aws:iam::{account_id}:oidc-provider/{host}") })))
}

/// A control plane found through its role in this account: one running here (a Lambda function, with its region and
/// runs-on label), or one elsewhere whose machines run here (a role it assumes by OpenID Connect).
pub struct FoundPlane { pub plane_id: String, pub url: String, pub lambda: Option<(String, String)> }

/// The control planes this account runs or starts machines for: roles named `superci-plane-<id>`, described with
/// the control plane's URL.
pub fn connected_planes(creds: &Credentials) -> Result<Vec<FoundPlane>> {
    let http = Blocking::new();
    let (mut planes, mut marker) = (Vec::new(), None::<String>);
    loop {
        let xml = futures::executor::block_on(aws::iam(&http, creds, "ListRoles", serde_json::json!({ "MaxItems": 1000, "Marker": marker }), now_ms()))?;
        for role in xml.split("<member>").skip(1) {
            let Some(id) = aws::xml_tag(role, "RoleName").and_then(|n| n.strip_prefix("superci-plane-")) else { continue };
            let description = aws::xml_tag(role, "Description").unwrap_or_default();
            let url = description.split("SuperCI control plane ").nth(1).and_then(|r| r.split(' ').next()).unwrap_or_default().to_string();
            // "Runs the SuperCI control plane <url> (Lambda, <region>, runs-on <label>)."
            let lambda = description.starts_with("Runs the").then(|| {
                let inner = description.split("(Lambda, ").nth(1).unwrap_or_default().trim_end_matches(").");
                let mut parts = inner.split(", runs-on ");
                (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or("superci").to_string())
            });
            planes.push(FoundPlane { plane_id: id.to_string(), url, lambda });
        }
        marker = (aws::xml_tag(&xml, "IsTruncated") == Some("true")).then(|| aws::xml_tag(&xml, "Marker").map(str::to_string)).flatten();
        if marker.is_none() { return Ok(planes) }
    }
}

#[cfg(test)]
impl Session {
    /// A sign-in that never reaches AWS, for rendering pages in tests.
    pub fn for_test(account_id: &str) -> Self {
        Session { region: "us-east-1".into(), account_id: account_id.into(), who: String::new(), refresh_token: String::new(), dpop: SigningKeyStore::generate(),
            creds: Credentials { access_key_id: String::new(), secret_access_key: String::new(), session_token: None, expires_at_ms: 0 } }
    }
}

#[cfg(test)]
mod policy_tests {
    #[test]
    fn reads_the_policy_iam_gives_back() {
        let want = superci_core::aws::runner_policy("123456789012", "abc");
        let encoded: String = url::form_urlencoded::byte_serialize(want.to_string().as_bytes()).collect::<String>().replace('+', "%20");
        let xml = format!("<GetRolePolicyResponse><GetRolePolicyResult><PolicyName>runner-machines</PolicyName><PolicyDocument>{encoded}</PolicyDocument></GetRolePolicyResult></GetRolePolicyResponse>");
        assert_eq!(super::policy_document(&xml), Some(want));
    }
}

#[cfg(test)]
mod live_network {
    use superci_core::aws;

    fn creds() -> super::Credentials {
        let profile = std::env::var("SUPERCI_LIVE_AWS_PROFILE").unwrap();
        let out = std::process::Command::new("aws").args(["configure", "export-credentials", "--profile", &profile, "--format", "process"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        super::Credentials { access_key_id: v["AccessKeyId"].as_str().unwrap().into(), secret_access_key: v["SecretAccessKey"].as_str().unwrap().into(), session_token: v["SessionToken"].as_str().map(str::to_string), expires_at_ms: u64::MAX }
    }

    /// A control plane's own network made twice (the second changes nothing), a spot machine started in it that reaches
    /// the internet (its console says so), then the machine and the network removed (SUPERCI_LIVE_AWS_PROFILE; a cent or so).
    #[test]
    #[ignore]
    fn live_aws_network() {
        let (c, region, http) = (creds(), "us-east-1", super::Blocking::new());
        let plane = format!("livetest{}", superci_core::crypto::random_id(6).to_lowercase());
        let t = std::time::Instant::now();
        super::make_networks(&c, &plane, &[region.to_string()]).unwrap();
        eprintln!("made in {:?}", t.elapsed());
        let n = futures::executor::block_on(aws::find_network(&http, region, &c, &plane, super::now_ms())).unwrap().expect("found");
        eprintln!("{n:?}");
        assert!(n.subnets.len() >= 3, "a subnet per zone");
        super::make_networks(&c, &plane, &[region.to_string()]).unwrap();
        assert_eq!(futures::executor::block_on(aws::find_network(&http, region, &c, &plane, super::now_ms())).unwrap().as_ref(), Some(&n), "made once");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let image = futures::executor::block_on(aws::latest_image(&http, region, &c, "099720109477", "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*", super::now_ms())).unwrap();
            let user_data = "#!/bin/bash\necho \"superci-internet: $(curl -s -o /dev/null -w '%{http_code}' https://api.github.com)\" > /dev/console\nshutdown -h +5\n";
            let types = ["c7a.large".to_string(), "m7a.large".into(), "c7i.large".into()];
            let tags = [("superci-plane".to_string(), plane.clone()), ("Name".to_string(), "SuperCI live network test".to_string())];
            let started = futures::executor::block_on(aws::run_instance(&http, &c, &aws::Launch { region, network: Some(&n), image: &image, types: &types, user_data, disk_gb: 8, tags: &tags, spot: true, os: "linux" }, super::now_ms())).unwrap();
            eprintln!("started {} ({}) in {}", started.id, started.kind, started.zone);
            assert!(n.subnets.iter().any(|(z, _)| *z == started.zone));
            let described = futures::executor::block_on(aws::ec2(&http, region, &c, "DescribeInstances", serde_json::json!({ "InstanceId": [started.id] }), super::now_ms())).unwrap();
            assert!(described.contains(&format!("<vpcId>{}</vpcId>", n.vpc)) && described.contains(&format!("<groupId>{}</groupId>", n.security_group)));
            // Its console, once it has written it (a few minutes).
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_secs(20));
                let out = futures::executor::block_on(aws::ec2(&http, region, &c, "GetConsoleOutput", serde_json::json!({ "InstanceId": started.id, "Latest": "true" }), super::now_ms())).unwrap_or_default();
                let text = aws::xml_tag(&out, "output").and_then(|o| { use base64::Engine; base64::engine::general_purpose::STANDARD.decode(o.trim()).ok() }).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
                if let Some(line) = text.lines().find(|l| l.contains("superci-internet:")) { eprintln!("{line}"); assert!(line.contains("200")); return }
            }
            panic!("no console line in 10 minutes");
        }));
        let t = std::time::Instant::now();
        super::delete_networks(&c, &plane, &[region]).unwrap();
        eprintln!("removed in {:?}", t.elapsed());
        assert_eq!(futures::executor::block_on(aws::find_network(&http, region, &c, &plane, super::now_ms())).unwrap(), None);
        result.unwrap();
    }
}

mod live_role {
    /// Reads a control plane's runner role as the dashboard does (SUPERCI_LIVE_AWS_PROFILE; a role that is not there reads as
    /// lacking everything).
    #[test]
    #[ignore]
    fn live_runner_role_missing() {
        let profile = std::env::var("SUPERCI_LIVE_AWS_PROFILE").unwrap();
        let out = std::process::Command::new("aws").args(["configure", "export-credentials", "--profile", &profile, "--format", "process"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let creds = super::Credentials { access_key_id: v["AccessKeyId"].as_str().unwrap().into(), secret_access_key: v["SecretAccessKey"].as_str().unwrap().into(), session_token: v["SessionToken"].as_str().map(str::to_string), expires_at_ms: u64::MAX };
        let t = std::time::Instant::now();
        let missing = super::runner_role_missing(&creds, &std::env::var("SUPERCI_LIVE_AWS_ACCOUNT").unwrap(), "doesnotexist", &["us-east-1".to_string()]);
        eprintln!("{:?} in {:?}", missing.as_ref().map(|m| m.len()), t.elapsed());
        assert!(t.elapsed() < std::time::Duration::from_secs(20));
    }
}
