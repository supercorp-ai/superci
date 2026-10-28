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
        #[cfg(debug_assertions)]
        let r = to_stand_in(r);
        let mut builder = ureq::http::Request::builder().method(r.method.as_str()).uri(r.url.as_str());
        for (k, v) in &r.headers { builder = builder.header(k.as_str(), v.as_str()) }
        let request = builder.body(r.body.clone()).map_err(|e| e.to_string())?;
        let mut response = self.0.run(request).map_err(|e| format!("{} {}: {e}", r.method, r.url))?;
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
        let body = response.body_mut().read_to_vec().map_err(|e| e.to_string())?;
        #[cfg(debug_assertions)]
        record(&r, status, &headers, &body);
        Ok(Response { status, headers, body })
    }
}

/// In a debug build with `SUPERCI_RECORD=FILE`: every exchange with AWS (and Cloudflare's API) is appended to that file, a line each (the
/// address, what was asked, the status, the answer's telling headers and the whole answer), to make test fixtures of what a real account answers.
/// No header is kept (the credentials are in them) but those that say which client asked (`SENT_KEPT`), nor a sign-in's exchange unless `SUPERCI_RECORD_SIGN_INS=1` too (for
/// fixtures of sign-ins and renewals: their tokens are real). The answers name the account: they are sanitized before
/// they are kept anywhere (`scripts/sanitize-recording.py`).
/// The headers a recording keeps of what was sent: which client asked (none holds a credential).
#[cfg(debug_assertions)]
const SENT_KEPT: [&str; 3] = ["x-modal-client-type", "x-modal-client-version", "x-modal-libmodal-version"];

#[cfg(debug_assertions)]
pub(crate) fn record(r: &Request, status: u16, headers: &[(String, String)], body: &[u8]) {
    let Some(file) = std::env::var_os("SUPERCI_RECORD").filter(|f| !f.is_empty()) else { return };
    if (r.url.contains("signin.") || r.url.contains("/oauth")) && std::env::var_os("SUPERCI_RECORD_SIGN_INS").is_none() { return }
    let target = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-amz-target")).map(|(_, v)| v.clone()).unwrap_or_default();
    // (Of the answer's headers, those that say something: what kind of error, where to, which version.)
    let said: Vec<(&String, &String)> = headers.iter().filter(|(k, _)| ["x-amzn-errortype", "location", "etag", "content-type", "content-range"].contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| (k, v)).collect();
    // (Of what was sent, only what says which client asked: Modal holds a client to it, and the test kit does too.)
    let sent: serde_json::Map<String, serde_json::Value> = r.headers.iter().filter(|(k, _)| SENT_KEPT.contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| (k.to_ascii_lowercase(), v.clone().into())).collect();
    let mut line = serde_json::json!({ "method": r.method, "url": r.url, "target": target, "asked": String::from_utf8_lossy(&r.body), "status": status, "headers": said, "answer": String::from_utf8_lossy(body) });
    if !sent.is_empty() { line["sent"] = sent.into() }
    use std::io::Write;
    // (One line at a time: questions are asked side by side.)
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) { let _ = f.write_all(format!("{line}\n").as_bytes()); }
}

/// In a debug build with `SUPERCI_STAND_IN=URL`: what is asked of AWS, GitHub's API, where GitHub keeps logs, gitlab.com and Cloudflare's API, goes to a
/// stand-in for them there instead (`superci_testkit`'s), as `URL/<the host>/<path>`.
#[cfg(debug_assertions)]
fn to_stand_in(mut r: Request) -> Request {
    let to = stand_in(&r.url);
    if to != r.url { r.url = to; r.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("host")); }
    r
}

/// An outside address, or (in a debug build with `SUPERCI_STAND_IN`) the stand-in's for it.
pub fn stand_in(url: &str) -> String { superci_core::io::stand_in(url) }

/// Waits `ms` before asking a cloud again what it may answer otherwise in a moment (a role just made, an image being
/// prepared). In a debug build with `SUPERCI_PAUSE_MS=N`, N milliseconds instead: the tests', whose answers are in hand.
pub fn pause(ms: u64) {
    #[cfg(debug_assertions)]
    let ms = std::env::var("SUPERCI_PAUSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(ms);
    std::thread::sleep(std::time::Duration::from_millis(ms))
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

/// Whether AWS refused a call because the credentials it was made with are over. A sign-in lives as long as the
/// AWS console session it was made from (twelve hours from when that one began, which may be most of them ago), and
/// AWS can end its credentials before they say they end.
pub fn expired(e: &str) -> bool {
    ["ExpiredToken", "RequestExpired"].iter().any(|code| e.starts_with(code) || e.contains(&format!(": {code}")) || e.contains(&format!("{code}:"))) || e.contains("security token included in the request is expired")
}

impl Session {
    /// The credentials in hand are taken as over (AWS said so): the next use renews them, or finds the sign-in ended.
    pub fn expire(&mut self) { self.creds.expires_at_ms = 0 }

    /// The credentials in hand end within two minutes: the next use renews them.
    pub fn due(&self) -> bool { self.creds.expires_at_ms < now_ms() + 120_000 }

    /// Credentials good for at least two more minutes, renewing them when needed.
    pub fn credentials(&mut self) -> Result<Credentials> {
        if self.due() {
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

/// `superci-cache` for AWS machines (Linux by architecture, and Windows), put in each region's cache bucket.
const CACHE_PROGRAMS: [(&str, &[u8]); 3] = superci_programs_cache::CACHE;

/// The control plane's own network in each region (`aws::make_network`), the regions at once, and its cache bucket
/// there with this version's `superci-cache` (see `superci_core::cache`). Without the bucket (an account that may
/// not make one) jobs use GitHub's own cache, as before: that is no reason for the rest not to be made.
pub fn make_networks(creds: &Credentials, plane_id: &str, regions: &[String]) -> Result<()> {
    let made: Vec<Result<()>> = std::thread::scope(|s| {
        let each: Vec<_> = regions.iter().map(|r| s.spawn(move || {
            let http = Blocking::new();
            futures::executor::block_on(aws::make_network(&http, r, creds, plane_id, now_ms())).map_err(|e| format!("{r}: {e}"))?;
            if let Err(e) = make_cache(&http, r, creds, plane_id) { eprintln!("The cache in {r} was not set up: {e}") }
            Ok(())
        })).collect();
        each.into_iter().map(|h| h.join().unwrap_or(Err("making a network stopped".into()))).collect()
    });
    made.into_iter().collect()
}

fn make_cache(http: &Blocking, region: &str, creds: &Credentials, plane_id: &str) -> Result<()> {
    futures::executor::block_on(aws::make_bucket(http, region, creds, plane_id, now_ms()))?;
    let put = |key: String, program: &[u8]| futures::executor::block_on(aws::put_once(http, region, creds, plane_id, &key, program, now_ms()));
    CACHE_PROGRAMS.iter().try_for_each(|(arch, program)| put(superci_core::cache::program_key(arch), program))?;
    superci_programs_layers::LAYERS.iter().try_for_each(|(arch, program)| put(superci_core::cache::layers_key(arch), program))
}


/// Removes the control plane's machines, networks and cache buckets in these regions (all SuperCI offers, when it is
/// deleted); the network SuperCI's control planes share, only with the last of them. A
/// region the account's organization keeps it out of has nothing of the control plane's (nothing could be made
/// there), and is no network left behind.
pub fn delete_networks(creds: &Credentials, plane_id: &str, regions: &[&str]) -> Result<()> {
    let sleep = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
    // The network SuperCI's control planes share goes once none of the others in the account uses it (one elsewhere
    // whose machines run here has a role here too).
    let others: Vec<String> = connected_planes(creds)?.into_iter().map(|p| p.plane_id).filter(|p| p != plane_id).collect();
    let others = &others;
    let done: Vec<Result<()>> = std::thread::scope(|s| {
        let each: Vec<_> = regions.iter().map(|r| s.spawn(move || {
            let http = Blocking::new();
            match futures::executor::block_on(aws::delete_network(&http, r, creds, plane_id, others, &now_ms, &sleep)) {
                Err(e) if kept_out_by_organization(&e) => return Ok(()),
                done => done.map_err(|e| format!("{r}: {e}"))?,
            }
            // Its cache bucket there, with all it keeps.
            match futures::executor::block_on(aws::delete_bucket(&http, r, creds, plane_id, &now_ms)) {
                Err(e) if kept_out_by_organization(&e) => Ok(()),
                done => done.map_err(|e| format!("{r}: the cache bucket: {e}")),
            }
        })).collect();
        each.into_iter().map(|h| h.join().unwrap_or(Err("removing a network stopped".into()))).collect()
    });
    done.into_iter().collect()
}

/// Whether AWS refused something by a rule of the account's organization (and so for the whole account there).
pub fn kept_out_by_organization(e: &str) -> bool {
    aws::refusal(e).is_some_and(|r| matches!(r.by, aws::By::Organization | aws::By::OrganizationResources))
}

/// What AWS answered when asked whether something may be done in an account.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Allowed,
    /// Refused for want of permission: by whom, and by which policy when AWS says.
    Refused(aws::Refusal),
    /// AWS answered something else (said as it said it): not known either way.
    Unknown(String),
}

impl Answer {
    pub fn allowed(&self) -> bool { matches!(self, Answer::Allowed) }
    /// Refused by a policy of the account's organization, which nothing inside the account can lift.
    pub fn by_organization(&self) -> Option<&aws::Refusal> { match self { Answer::Refused(r) if r.by == aws::By::Organization => Some(r), _ => None } }
    fn of(result: Result<()>) -> Answer {
        match result { Ok(()) => Answer::Allowed, Err(e) => match aws::refusal(&e) { Some(r) => Answer::Refused(r), None => Answer::Unknown(e) } }
    }
}

/// What kind of account this is for SuperCI, from what it allows.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Nothing SuperCI needs is refused.
    Open,
    /// AWS said nothing of any region (it did not answer, or the sign-in was over): not known.
    Unanswered,
    /// A project of AWS's new sign-up: its organization pins it to one region. Until its owner activates AWS's
    /// "advanced features", AWS refuses spot there too; after (`activated`), spot is allowed. SuperCI works with
    /// what such an account allows as it is: it asks for no rule to be changed.
    Project { region: String, activated: bool },
    /// Its organization's policies (or the sign-in's own permissions) refuse some regions, or spot.
    Restricted,
}

/// The regions AWS's new sign-up puts projects in (by the contact address: the Americas, Europe with Africa and the
/// Middle East, Asia Pacific).
pub const PROJECT_REGIONS: [&str; 3] = ["us-east-2", "eu-north-1", "ap-southeast-2"];

/// What an AWS account allows of what SuperCI does there: asked with reads and dry runs only, so nothing is made,
/// started or changed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Look {
    /// Each region SuperCI offers: whether machines' networks can be read there (the first thing a set-up does).
    pub regions: Vec<(String, Answer)>,
    /// The region the questions below were asked in: the first one allowed.
    pub asked_in: Option<String>,
    /// Whether a spot machine and an on-demand one of the default size could be started.
    pub spot: Option<Answer>,
    pub on_demand: Option<Answer>,
    /// How many vCPUs of each the account may run at once there.
    pub spot_cpus: Option<u32>,
    pub on_demand_cpus: Option<u32>,
    /// The regions allowed, each with whether the image jobs run on is there (published, or the account's own copy;
    /// not known: AWS did not say).
    pub images: Vec<(String, Option<bool>)>,
    /// The AWS organization the account is in, when it is in one: its id and its management account.
    pub organization_id: Option<String>,
    pub management_account: Option<String>,
}

impl Look {
    pub fn allowed_regions(&self) -> Vec<&str> { self.regions.iter().filter(|(_, a)| a.allowed()).map(|(r, _)| r.as_str()).collect() }

    /// The regions jobs can run in as things are: allowed, and with the image jobs run on.
    pub fn usable_regions(&self) -> Vec<&str> { self.images.iter().filter(|(_, there)| *there == Some(true)).map(|(r, _)| r.as_str()).collect() }

    /// The regions allowed where the image jobs run on is missing: a control plane there could start no job until
    /// the image is copied in.
    pub fn regions_without_image(&self) -> Vec<&str> { self.images.iter().filter(|(_, there)| *there == Some(false)).map(|(r, _)| r.as_str()).collect() }

    /// The organization's refusal, when there is one (a region's, else spot's): it names the policy and the
    /// organization's management account.
    pub fn organization(&self) -> Option<&aws::Refusal> {
        self.regions.iter().filter_map(|(_, a)| a.by_organization()).chain(self.spot.iter().filter_map(|a| a.by_organization())).find(|r| !r.policy.is_empty())
            .or_else(|| self.regions.iter().filter_map(|(_, a)| a.by_organization()).chain(self.spot.iter().filter_map(|a| a.by_organization())).next())
    }

    /// The account's limit of vCPUs at once where it holds jobs back (fewer than 32: a new account's), with where to
    /// ask AWS for more: the limit of what its jobs run on (spot, or on-demand where spot is refused).
    pub fn low_limit(&self) -> Option<(u32, String)> {
        let region = self.asked_in.as_deref()?;
        let spot_refused = matches!(self.spot, Some(Answer::Refused(_)));
        let (cpus, code) = if spot_refused { (self.on_demand_cpus?, aws::ON_DEMAND_CPU_QUOTA) } else { (self.spot_cpus?, aws::SPOT_CPU_QUOTA) };
        (cpus < 32).then(|| (cpus, aws::quota_link(region, code)))
    }

    /// What AWS said in place of an answer, when it said of no region whether the account may work there (the
    /// sign-in was over, or AWS did not answer): nothing is known of the account then.
    pub fn unanswered(&self) -> Option<&str> {
        let said: Option<Vec<&str>> = self.regions.iter().map(|(_, a)| match a { Answer::Unknown(e) => Some(e.as_str()), _ => None }).collect();
        said?.first().copied()
    }

    pub fn kind(&self) -> Kind {
        if self.unanswered().is_some() { return Kind::Unanswered }
        let allowed = self.allowed_regions();
        let refused = |region: &str| self.regions.iter().any(|(r, a)| r == region && a.by_organization().is_some());
        let spot_refused = self.spot.as_ref().is_some_and(|a| a.by_organization().is_some());
        // The new sign-up's mark: us-east-1 refused for what is regional and one home region of its three allowed, by
        // the organization's policy. No spot either, by another: not activated. (Activating takes that one away.)
        if let [only] = allowed.as_slice() {
            if PROJECT_REGIONS.contains(only) && refused("us-east-1") && (spot_refused || self.spot == Some(Answer::Allowed)) { return Kind::Project { region: only.to_string(), activated: !spot_refused } }
        }
        // (An account that manages its own organization, or is in none, with nothing refused, is open.)
        let any_refused = self.regions.iter().any(|(_, a)| matches!(a, Answer::Refused(_))) || matches!(self.spot, Some(Answer::Refused(_))) || matches!(self.on_demand, Some(Answer::Refused(_)));
        if any_refused { Kind::Restricted } else { Kind::Open }
    }
}

/// Asks an AWS account what it allows (see `Look`): a read in each region at once, then, in the first region
/// allowed, whether a spot and an on-demand machine could be started (dry runs) and how many vCPUs it may run.
pub fn look_at(creds: &Credentials) -> Look { look_with(&Blocking::new, creds) }

/// As `look_at`, over whatever answers for AWS (`http` makes a client for each question, asked side by side).
pub fn look_with<H: Http>(http: &(dyn Fn() -> H + Sync), creds: &Credentials) -> Look {
    let regions: Vec<(String, Answer)> = std::thread::scope(|s| {
        let each: Vec<_> = superci_core::page::REGIONS.iter().map(|r| s.spawn(move || {
            let read = futures::executor::block_on(aws::ec2(&http(), r, creds, "DescribeVpcs", serde_json::json!({ "MaxResults": 5 }), now_ms())).map(|_| ());
            (r.to_string(), Answer::of(read))
        })).collect();
        each.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut look = Look { regions, asked_in: None, spot: None, on_demand: None, spot_cpus: None, on_demand_cpus: None, images: vec![], organization_id: None, management_account: None };
    // The organization it is in, if any (a member may read this much of it; never its policies).
    if let Ok(org) = futures::executor::block_on(aws::json_call(&http(), "POST", "https://organizations.us-east-1.amazonaws.com/", "us-east-1", "organizations", Some("AWSOrganizationsV20161128.DescribeOrganization"), "application/x-amz-json-1.1", Some(&serde_json::json!({})), creds, now_ms())) {
        look.organization_id = org["Organization"]["Id"].as_str().map(str::to_string);
        look.management_account = org["Organization"]["MasterAccountId"].as_str().map(str::to_string);
    }
    let config = superci_core::plane::Config::new(String::new());
    // The image jobs run on, in each region allowed.
    let allowed: Vec<String> = look.allowed_regions().into_iter().map(str::to_string).collect();
    let found: Vec<(String, Result<String>)> = std::thread::scope(|s| {
        let each: Vec<_> = allowed.iter().map(|r| { let config = &config; s.spawn(move || (r.clone(), futures::executor::block_on(aws::latest_image(&http(), r, creds, &config.image_owner, &config.image_name, now_ms())))) }).collect();
        each.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    look.images = found.iter().map(|(r, image)| (r.clone(), match image { Ok(_) => Some(true), Err(e) if e.starts_with("no image ") => Some(false), Err(_) => None })).collect();
    // The rest is asked in one region: the first that has the image, else the first allowed.
    let Some(region) = look.usable_regions().first().or(allowed.first().map(String::as_str).as_ref()).map(|r| r.to_string()) else { return look };
    let ours = found.into_iter().find(|(r, _)| *r == region).map(|(_, image)| image).unwrap_or(Err("not asked".into()));
    let kind = superci_core::spec::aws_instance_types(&superci_core::spec::Spec::default()).into_iter().next().unwrap_or("c8a.xlarge".into());
    // A dry run has to name an image that exists: where ours is not, Amazon's own Linux (its id is a public
    // parameter in every region).
    let image = match ours {
        Ok(id) => Some(id),
        Err(_) => futures::executor::block_on(aws::json_call(&http(), "POST", &format!("https://ssm.{region}.amazonaws.com/"), &region, "ssm", Some("AmazonSSM.GetParameter"), "application/x-amz-json-1.1",
            Some(&serde_json::json!({ "Name": "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64" })), creds, now_ms())).ok().and_then(|v| v["Parameter"]["Value"].as_str().map(str::to_string)),
    };
    look.asked_in = Some(region.clone());
    let Some(image) = image else { return look };
    std::thread::scope(|s| {
        let spot = s.spawn(|| Answer::of(futures::executor::block_on(aws::could_run(&http(), &region, creds, &image, &kind, true, now_ms()))));
        let on_demand = s.spawn(|| Answer::of(futures::executor::block_on(aws::could_run(&http(), &region, creds, &image, &kind, false, now_ms()))));
        let spot_cpus = s.spawn(|| futures::executor::block_on(aws::cpu_quota(&http(), &region, creds, aws::SPOT_CPU_QUOTA, now_ms())).ok());
        let on_demand_cpus = s.spawn(|| futures::executor::block_on(aws::cpu_quota(&http(), &region, creds, aws::ON_DEMAND_CPU_QUOTA, now_ms())).ok());
        (look.spot, look.on_demand) = (spot.join().ok(), on_demand.join().ok());
        (look.spot_cpus, look.on_demand_cpus) = (spot_cpus.join().ok().flatten(), on_demand_cpus.join().ok().flatten());
    });
    look
}

/// Whether a control plane with runners can be set up in a region, asked before anything is made there: the region
/// is the account's to use (the read a set-up starts with), and the image jobs run on is there.
pub fn region_ready(creds: &Credentials, region: &str) -> Result<()> {
    let http = Blocking::new();
    futures::executor::block_on(aws::ec2(&http, region, creds, "DescribeVpcs", serde_json::json!({ "MaxResults": 5 }), now_ms())).map_err(|e| format!("{region}: {e}"))?;
    let config = superci_core::plane::Config::new(String::new());
    match futures::executor::block_on(aws::latest_image(&http, region, creds, &config.image_owner, &config.image_name, now_ms())) {
        Err(e) if e.starts_with("no image ") => Err(format!("The image SuperCI's jobs run on is not in {region} yet, so no job could start there. Choose another region.")),
        // (Whatever else AWS says of the image is left to the set-up, which says it where it meets it.)
        _ => Ok(()),
    }
}

/// Where the image's maker lists each build's images, by region (public, read with no sign-in).
pub const IMAGE_LIST: &str = "https://api.github.com/repos/runs-on/runner-images-for-aws/releases?per_page=12";

/// The newest build of the kind of image jobs run on, as its maker lists it: the build's stamp, and the image's id
/// in each region it is published in. This is how a copy learns its source from an account that can ask no other
/// region.
pub fn published_image<H: Http>(http: &H) -> Result<(String, Vec<(String, String)>)> {
    let config = superci_core::plane::Config::new(String::new());
    let flavor = aws::image_flavor(&config.image_name).to_string();
    let request = Request::new("GET", IMAGE_LIST).with_header("user-agent", "superci").with_header("accept", "application/vnd.github+json");
    let answer = futures::executor::block_on(http.send(request)).map_err(|e| format!("The list of runner images could not be read from GitHub: {e}"))?;
    if answer.status != 200 { return Err(format!("The list of runner images could not be read from GitHub (it answered {}).", answer.status)) }
    let releases: serde_json::Value = serde_json::from_slice(&answer.body).map_err(|e| format!("The list of runner images from GitHub could not be read: {e}"))?;
    releases.as_array().into_iter().flatten().find_map(|r| {
        let published = aws::published_images(r["body"].as_str()?, &flavor);
        let tag = r["tag_name"].as_str()?;
        let stamp = tag.rsplit('-').next().unwrap_or(tag).to_string();
        (!published.is_empty() && stamp.len() >= 8 && stamp.bytes().all(|b| b.is_ascii_digit())).then_some((stamp, published))
    }).ok_or_else(|| format!("The maker's list of runner images has no {flavor} image now. Name one yourself: --from=REGION:ami-…"))
}

/// The name a copy of the image gets: the name its maker's images have, with the build's stamp, so that it is found
/// as theirs are.
pub fn copy_name(stamp: &str) -> String {
    let config = superci_core::plane::Config::new(String::new());
    format!("{}{stamp}", config.image_name.trim_end_matches('*'))
}

/// The account's own copies of the image in a region (see `aws::copied_images`).
pub fn copied_images(creds: &Credentials, region: &str) -> Result<Vec<aws::CopiedImage>> {
    futures::executor::block_on(aws::copied_images(&Blocking::new(), region, creds, now_ms())).map_err(|e| format!("{region}: {e}"))
}

/// Where a copy of the image into a region would come from: the build's stamp, the region and the image there.
/// From its maker's list, or (`from`) a region and an image named instead. Nothing is changed by asking.
pub fn copy_source_for(region: &str, from: Option<(&str, &str)>) -> Result<(String, String, String)> {
    match from {
        Some((r, i)) => Ok((amz_stamp(), r.to_string(), i.to_string())),
        None => {
            let (stamp, published) = published_image(&Blocking::new())?;
            let (r, i) = aws::copy_source(&published, region).ok_or("The image is published in no other region to copy it from.")?.clone();
            Ok((stamp, r, i))
        }
    }
}

/// Starts copying the image jobs run on into a region, from a source `copy_source_for` gave. The copy's id.
pub fn copy_image_into(creds: &Credentials, region: &str, (stamp, from_region, from_image): &(String, String, String)) -> Result<String> {
    futures::executor::block_on(aws::copy_image(&Blocking::new(), region, creds, from_region, from_image, &copy_name(stamp), now_ms())).map_err(|e| format!("{region}: {e}"))
}

/// Removes the account's own copies of the image in a region, with their snapshots. How many.
pub fn delete_copied_images(creds: &Credentials, region: &str) -> Result<usize> {
    let http = Blocking::new();
    let copies = futures::executor::block_on(aws::copied_images(&http, region, creds, now_ms())).map_err(|e| format!("{region}: {e}"))?;
    for copy in &copies { futures::executor::block_on(aws::delete_copied_image(&http, region, creds, copy, now_ms())).map_err(|e| format!("{region}: {e}"))?; }
    Ok(copies.len())
}

/// The build an image is of, from the end of its name (`…-x64-20261006080949`); empty when it has none.
pub fn build_of(name: &str) -> &str { name.rsplit('-').next().filter(|s| s.len() == 14 && s.bytes().all(|b| b.is_ascii_digit())).unwrap_or_default() }

/// The build the account's copies hold, when its maker has a newer one out (the image is rebuilt every week or two,
/// and a copy stays what it was): the newest build among the copies.
pub fn behind<'a>(copies: &'a [aws::CopiedImage], newest: &str) -> Option<&'a str> {
    let have = copies.iter().map(|c| build_of(&c.name)).filter(|b| !b.is_empty()).max()?;
    (have < newest).then_some(have)
}

/// A build's day, as people say it: 20261006080949 is 6 October 2026.
pub fn build_day(stamp: &str) -> String {
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    match (stamp.get(0..4), stamp.get(4..6).and_then(|m| m.parse::<usize>().ok()).filter(|m| (1..=12).contains(m)), stamp.get(6..8).and_then(|d| d.parse::<u32>().ok())) {
        (Some(year), Some(month), Some(day)) => format!("{day} {} {year}", MONTHS[month - 1]),
        _ => stamp.to_string(),
    }
}

/// The newest build last read, and when.
static NEWEST: std::sync::Mutex<Option<(u64, String)>> = std::sync::Mutex::new(None);

/// The newest build its maker lists (asked at most every six hours: the list is GitHub's, read with no sign-in).
pub fn newest_build() -> Option<String> {
    let mut known = NEWEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, stamp)) = known.as_ref().filter(|(at, _)| now_ms() < at + 6 * 3_600_000) { let _ = at; return Some(stamp.clone()) }
    let stamp = published_image(&Blocking::new()).ok()?.0;
    *known = Some((now_ms(), stamp.clone()));
    Some(stamp)
}

/// Removes the account's older copies in a region once a newer one is there (a copy that replaced them). How many.
pub fn tidy_copies(creds: &Credentials, region: &str) -> Result<usize> {
    let http = Blocking::new();
    let copies = futures::executor::block_on(aws::copied_images(&http, region, creds, now_ms())).map_err(|e| format!("{region}: {e}"))?;
    let Some(newest) = copies.iter().filter(|c| c.state == "available").map(|c| build_of(&c.name)).max().filter(|b| !b.is_empty()) else { return Ok(0) };
    let older: Vec<&aws::CopiedImage> = copies.iter().filter(|c| { let b = build_of(&c.name); !b.is_empty() && b < newest }).collect();
    for copy in &older { futures::executor::block_on(aws::delete_copied_image(&http, region, creds, copy, now_ms())).map_err(|e| format!("{region}: {e}"))?; }
    Ok(older.len())
}

/// A stamp of now, as the maker's builds have them (20261006080949).
fn amz_stamp() -> String { aws::amz_date(now_ms()).chars().filter(|c| c.is_ascii_digit()).take(14).collect() }

/// The policy in an IAM GetRolePolicy answer (URL-encoded JSON in XML).
fn policy_document(xml: &str) -> Option<serde_json::Value> {
    let encoded = aws::xml_tag(xml, "PolicyDocument")?;
    let decoded: String = url::form_urlencoded::parse(format!("d={}", encoded.replace('+', "%2B")).as_bytes()).map(|(_, v)| v).collect();
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
            // (A plane's id is letters and digits: `superci-plane-<id>-convert` is its converter's.)
            let Some(id) = aws::xml_tag(role, "RoleName").and_then(|n| n.strip_prefix("superci-plane-")).filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric())) else { continue };
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
    /// The same, its credentials good until `expires_at_ms` (renewed, say, by another SuperCI).
    pub fn for_test_until(account_id: &str, expires_at_ms: u64) -> Self { let mut s = Self::for_test(account_id); s.creds.expires_at_ms = expires_at_ms; s.refresh_token = format!("renewed-{expires_at_ms}"); s }

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
            let started = futures::executor::block_on(aws::run_instance(&http, &c, &aws::Launch { region, network: Some(&n), image: &image, types: &types, user_data, disk_gb: 8, disk_mibs: 125, disk_iops: 3000, clock: None, skip: &[], tags: &tags, spot: true, os: "linux", avoid: &[], timer: None, jitter_ms: 0, fleet: false, store_disk: None }, super::now_ms(), &mut superci_core::aws::Told::default())).unwrap();
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

/// AWS as it answered once: a recording of a real account (sanitized, see `scripts/sanitize-recording.py`) played
/// back, so that tests meet exactly what the service says. A question that was not recorded is no answer at all (a
/// status of 599), which fails whatever asked it.
#[cfg(test)]
pub mod replay {
    use super::*;

    #[derive(Clone)]
    pub struct Replay(std::sync::Arc<Vec<serde_json::Value>>, Option<std::sync::Arc<std::sync::Mutex<Vec<bool>>>>);

    impl Replay {
        /// The same, each recorded answer given once and in the order it was recorded: for a series of steps that
        /// ask the same thing before and after a change.
        pub fn in_order(recording: &str) -> Replay { let r = Replay::of(recording); let used = vec![false; r.0.len()]; Replay(r.0, Some(std::sync::Arc::new(std::sync::Mutex::new(used)))) }
        /// How many recorded answers were not asked for (in order: all of them should be).
        pub fn left(&self) -> usize { self.1.as_ref().map(|u| u.lock().unwrap().iter().filter(|u| !**u).count()).unwrap_or(0) }
        pub fn of(recording: &str) -> Replay { Replay(std::sync::Arc::new(recording.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).expect("a recording is a JSON object a line")).collect()), None) }
    }

    #[async_trait::async_trait(?Send)]
    impl Http for Replay {
        async fn send(&self, r: Request) -> io::Result<Response> {
            let target = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-amz-target")).map(|(_, v)| v.as_str()).unwrap_or_default();
            let asked = String::from_utf8_lossy(&r.body);
            let mut used = self.1.as_ref().map(|u| u.lock().unwrap());
            let at = self.0.iter().enumerate().position(|(i, x)| used.as_ref().is_none_or(|u| !u[i]) && x["method"] == r.method.as_str() && x["url"] == r.url.as_str() && x["target"] == target && x["asked"] == asked.as_ref());
            if let (Some(u), Some(i)) = (used.as_mut(), at) { u[i] = true }
            let found = at.map(|i| &self.0[i]);
            Ok(match found {
                Some(x) => Response { status: x["status"].as_u64().unwrap_or(0) as u16, headers: vec![], body: x["answer"].as_str().unwrap_or_default().as_bytes().to_vec() },
                None => Response { status: 599, headers: vec![], body: format!("<Response><Errors><Error><Code>NotRecorded</Code><Message>{} {} {target} {asked}</Message></Error></Errors></Response>", r.method, r.url).into_bytes() },
            })
        }
    }
}

/// Clouds answering as a test has them, on this computer, for code that reaches them over HTTP itself (Cloudflare's
/// API, registries, a control plane's address): a server answering each request with what `answer` makes of it, and
/// every outside address sent there (`SUPERCI_STAND_IN`) while `with` runs. Where they go is the process's, so one
/// such test at a time.
#[cfg(test)]
pub mod clouds {
    use std::io::{Read, Write};

    /// A whole HTTP answer: its status and body (JSON, by its first character; else text).
    pub fn answer(status: u16, body: &str) -> Vec<u8> {
        let kind = if body.starts_with('{') || body.starts_with('[') { "application/json" } else { "text/plain" };
        format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }

    /// An answer cut off midway: it says more is coming than comes before its connection closes.
    pub fn cut(status: u16) -> Vec<u8> { format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{{\"part").into_bytes() }

    /// Cloudflare's envelope around a result.
    pub fn cf(result: serde_json::Value) -> Vec<u8> { answer(200, &serde_json::json!({ "success": true, "errors": [], "result": result }).to_string()) }

    /// A server answering each request (its method, address and body) with what `answer` makes of it. Its address.
    pub fn serve(answer: impl Fn(&str, &str, &[u8]) -> Vec<u8> + Send + Sync + 'static) -> String { listen(move |method, path, _, body| answer(method, path, body)) }

    /// AWS as a test has it: each call answered with what `answer` makes of what it asks (its target, its action, or
    /// its method and address for an API by address, Lambda's) and its body.
    pub fn serve_aws(answer: impl Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static) -> String {
        listen(move |method, path, head, body| {
            let target = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("x-amz-target")).map(|(_, t)| t.trim().to_string()));
            let action = String::from_utf8_lossy(body).split('&').find_map(|kv| kv.strip_prefix("Action=").map(str::to_string));
            answer(&target.or(action).unwrap_or_else(|| format!("{method} {path}")), body)
        })
    }

    /// AWS's error for a JSON API (`kind`), or for a query API (IAM's) when `kind` is said with `query:`.
    pub fn aws_error(kind: &str) -> Vec<u8> {
        match kind.strip_prefix("query:") {
            Some(code) => answer(400, &format!("<ErrorResponse><Error><Code>{code}</Code><Message>Refused.</Message></Error></ErrorResponse>")),
            None => answer(400, &serde_json::json!({ "__type": kind, "message": "Refused." }).to_string()),
        }
    }

    fn listen(answer: impl Fn(&str, &str, &str, &[u8]) -> Vec<u8> + Send + Sync + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let answer = std::sync::Arc::new(answer);
        std::thread::spawn(move || for mut s in listener.incoming().flatten() {
            let answer = answer.clone();
            std::thread::spawn(move || {
                let mut got = vec![];
                let mut buf = [0u8; 65536];
                let head = loop {
                    let Ok(n) = s.read(&mut buf) else { return };
                    if n == 0 { return }
                    got.extend_from_slice(&buf[..n]);
                    if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") { break at + 4 }
                };
                let text = String::from_utf8_lossy(&got[..head]).to_string();
                let length: usize = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                while got.len() < head + length { let Ok(n) = s.read(&mut buf) else { return }; if n == 0 { break } got.extend_from_slice(&buf[..n]); }
                let mut first = text.split_whitespace();
                let (method, path) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
                let _ = s.write_all(&answer(&method, &path, &text, &got[head..]));
            });
        });
        url
    }

    /// An address nobody answers at.
    pub fn closed() -> String {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("http://127.0.0.1:{port}")
    }

    /// `f` run with every outside address sent to `at` (and no pause between tries), one such test at a time.
    pub fn with<T>(at: &str, f: impl FnOnce() -> T) -> T {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SUPERCI_STAND_IN", at);
        std::env::set_var("SUPERCI_PAUSE_MS", "0");
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::env::remove_var("SUPERCI_STAND_IN");
        std::env::remove_var("SUPERCI_PAUSE_MS");
        out.unwrap_or_else(|p| std::panic::resume_unwind(p))
    }
}

#[cfg(test)]
mod answers {
    use super::clouds::{self, answer, aws_error, cut, with};
    use super::replay::Replay;
    use super::*;

    fn creds() -> Credentials { Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, expires_at_ms: 0 } }

    /// The maker's list of builds, with one image published only in `region`.
    fn releases(region: &str) -> String {
        serde_json::json!([{ "tag_name": "v2.2-20261006080949" }, { "body": "no tag" },
            { "tag_name": "v2.2-20261006080949", "body": format!("### `ubuntu24-full-x64`\n| Region | AMI |\n| --- | --- |\n| {region} | `ami-0123456789abcdef0` |\n") }]).to_string()
    }

    #[test]
    fn a_request_that_cannot_be_made_or_whose_answer_is_cut_off_is_an_error_and_an_answers_telling_headers_are_recorded() {
        assert!(futures::executor::block_on(Blocking::new().send(Request::new("GET", "http://no where/"))).is_err());
        assert!(futures::executor::block_on(Blocking::new().send(Request::new("GET", &clouds::serve(|_, _, _| cut(200))))).is_err());
        // (A header's value that is not text is left out; one that tells what kind of error, kept.)
        let at = clouds::serve(|_, _, _| {
            let mut a = b"HTTP/1.1 400 X\r\nx-amzn-errortype: ThrottlingException\r\nx-odd: ".to_vec();
            a.extend_from_slice(b"\xe9\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
            a
        });
        let file = std::env::temp_dir().join(format!("superci-record-{}", superci_core::crypto::random_id(6)));
        let r = with(&at, || {
            std::env::set_var("SUPERCI_RECORD", &file);
            let r = futures::executor::block_on(Blocking::new().send(Request::new("POST", &format!("{at}/")).with_header("x-amz-target", "AmazonSSM.GetParameter").with_body("{}")));
            std::env::remove_var("SUPERCI_RECORD");
            r
        });
        assert_eq!(r.unwrap().header("x-odd"), None);
        let line: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!((&line["headers"], &line["target"]), (&serde_json::json!([["x-amzn-errortype", "ThrottlingException"]]), &serde_json::json!("AmazonSSM.GetParameter")));
        let _ = std::fs::remove_file(&file);
    }

    /// AWS's sign-in answering without the credentials, with an id token that is not one, or without what renews
    /// it: refused, but for what renews it, which a renewal may leave as it was.
    #[test]
    fn a_sign_in_answer_without_its_credentials_or_with_a_broken_id_token_is_refused() {
        use base64::Engine;
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let full = || serde_json::json!({ "accessToken": { "accessKeyId": "ASIAX", "secretAccessKey": "s", "sessionToken": "t" }, "refreshToken": "r" });
        let from = |v: &serde_json::Value, previous: Option<&Session>| session_from("us-east-1", SigningKeyStore::generate(), v, previous);
        for missing in ["accessKeyId", "secretAccessKey", "sessionToken"] {
            let mut v = full();
            v["accessToken"].as_object_mut().unwrap().remove(missing);
            assert_eq!(from(&v, None).err().unwrap(), format!("AWS sign-in answer without {missing}"));
        }
        for id in ["no-dots".to_string(), "a.!!!.c".to_string(), format!("a.{}.c", b64("not json"))] {
            let mut v = full();
            v["idToken"] = id.clone().into();
            assert!(from(&v, None).is_err(), "{id}");
        }
        // Nobody named, nothing before: no account; nothing to renew with is refused unless one was there before.
        let first = from(&full(), None).unwrap();
        assert_eq!((first.who.as_str(), first.account_id.as_str()), ("", ""));
        let mut v = full();
        v.as_object_mut().unwrap().remove("refreshToken");
        assert_eq!(from(&v, None).err().unwrap(), "no refresh token");
        assert_eq!(from(&v, Some(&first)).unwrap().refresh_token, "r");
    }

    /// A renewal AWS does not answer, or answers without credentials: an error, and the sign-in not taken as ended.
    #[test]
    fn a_renewal_aws_does_not_answer_or_answers_without_credentials_is_an_error() {
        let session = || Session { region: "us-east-1".into(), account_id: "111111111111".into(), who: String::new(), creds: creds(), refresh_token: "r".into(), dpop: SigningKeyStore::generate() };
        with(&clouds::closed(), || assert!(!session().credentials().unwrap_err().starts_with(ENDED)));
        with(&clouds::serve(|_, _, _| answer(200, "{}")), || assert_eq!(session().credentials().unwrap_err(), "AWS sign-in answer without accessKeyId"));
    }

    #[test]
    fn a_control_plane_address_that_is_none_connects_and_disconnects_nothing() {
        assert_eq!(connect_runners(&creds(), "111111111111", "no address", "p1", "aud").unwrap_err(), "bad control plane URL");
        assert_eq!(disconnect_runners(&creds(), "111111111111", "no address", "p1").unwrap_err(), "bad control plane URL");
    }

    #[test]
    fn a_runner_role_there_already_whose_trust_cannot_be_updated_is_an_error() {
        let at = clouds::serve_aws(|what, _| match what {
            "CreateRole" => aws_error("query:EntityAlreadyExists"),
            "UpdateAssumeRolePolicy" => aws_error("query:ServiceFailure"),
            _ => answer(200, "<Ok/>"),
        });
        with(&at, || assert!(connect_runners(&creds(), "111111111111", "https://plane.example", "p1", "aud").unwrap_err().starts_with("ServiceFailure")));
    }

    #[test]
    fn a_low_limit_is_not_known_without_the_on_demand_limit_where_spot_is_refused_and_no_region_is_no_answer() {
        let refused = aws::Refusal { action: "ec2:RunInstances".into(), by: aws::By::Identity, policy: String::new() };
        let look = Look { regions: vec![], asked_in: Some("us-east-1".into()), spot: Some(Answer::Refused(refused)), on_demand: None, spot_cpus: Some(5), on_demand_cpus: None, images: vec![], organization_id: None, management_account: None };
        assert_eq!((look.low_limit(), look.unanswered()), (None, None));
    }

    /// The maker's list of builds: not JSON, or with no build of the image, is said as that; a build without its
    /// notes or its tag is passed over.
    #[test]
    fn the_makers_list_is_read_for_the_newest_build_of_the_image() {
        let listed = |status: u16, body: &str| Replay::of(&serde_json::json!({ "method": "GET", "url": IMAGE_LIST, "target": "", "asked": "", "status": status, "answer": body }).to_string());
        assert!(published_image(&listed(200, "not json")).unwrap_err().starts_with("The list of runner images from GitHub could not be read"));
        assert!(published_image(&listed(200, "[]")).unwrap_err().starts_with("The maker's list of runner images has no ubuntu24-full-x64 image now"));
        assert_eq!(published_image(&listed(200, &releases("us-east-1"))).unwrap(), ("20261006080949".to_string(), vec![("us-east-1".to_string(), "ami-0123456789abcdef0".to_string())]));
        // Published only where the copy would go: nowhere to copy it from.
        with(&clouds::serve(|_, _, _| answer(200, &releases("us-east-1"))), || assert_eq!(copy_source_for("us-east-1", None).unwrap_err(), "The image is published in no other region to copy it from."));
    }

    /// The newest build: not known while GitHub does not answer; once read, kept for six hours.
    #[test]
    fn the_newest_build_is_kept_once_read() {
        *NEWEST.lock().unwrap() = None;
        with(&clouds::closed(), || assert_eq!(newest_build(), None));
        with(&clouds::serve(|_, _, _| answer(200, &releases("eu-west-1"))), || assert_eq!(newest_build().as_deref(), Some("20261006080949")));
        with(&clouds::closed(), || assert_eq!(newest_build().as_deref(), Some("20261006080949")));
    }

    #[test]
    fn copies_aws_does_not_list_or_remove_are_an_error() {
        let listed = "<DescribeImagesResponse><imagesSet><item><imageId>ami-0123456789abcdef0</imageId><name>runs-on-v2.2-ubuntu24-full-x64-20261006080949</name><imageState>available</imageState></item></imagesSet></DescribeImagesResponse>";
        for failing in ["DescribeImages", "DeregisterImage"] {
            let at = clouds::serve_aws(move |what, _| if what == failing { aws_error("query:InternalError") } else { answer(200, listed) });
            with(&at, || assert!(delete_copied_images(&creds(), "eu-west-1").unwrap_err().starts_with("eu-west-1: InternalError"), "{failing}"));
        }
        let at = clouds::serve_aws(|_, _| aws_error("query:InternalError"));
        with(&at, || assert!(tidy_copies(&creds(), "eu-west-1").unwrap_err().starts_with("eu-west-1: InternalError")));
    }

    #[test]
    fn a_role_policy_answer_without_its_document_has_none() { assert_eq!(policy_document("<GetRolePolicyResponse/>"), None) }
}

#[cfg(test)]
mod look_tests {
    use super::replay::Replay;
    use super::*;

    fn creds() -> Credentials { Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: Some("token".into()), expires_at_ms: 0 } }

    #[test]
    fn a_look_aws_did_not_answer_is_not_taken_for_an_account_with_nothing_refused() {
        // As AWS answered every region once the sign-in's credentials were over (13 October 2026).
        let said = "RequestExpired: Request has expired.";
        let look = Look { regions: superci_core::page::REGIONS.iter().map(|r| (r.to_string(), Answer::Unknown(said.into()))).collect(), asked_in: None, spot: None, on_demand: None, spot_cpus: None, on_demand_cpus: None, images: vec![], organization_id: None, management_account: None };
        assert_eq!((look.kind(), look.unanswered()), (Kind::Unanswered, Some(said)));
        assert!(look.allowed_regions().is_empty());
        // That answer, and the one other services give, mean the credentials are over; a refusal does not.
        for over in [said, "ExpiredToken: The security token included in the request is expired", "us-east-1: RequestExpired: Request has expired."] { assert!(expired(over), "{over}") }
        for other in ["UnauthorizedOperation: You are not authorized to perform this operation.", "AWS did not answer in time", ""] { assert!(!expired(other), "{other}") }
        let lines = crate::commands::aws_look_lines("111111111111", "", &look);
        assert_eq!(lines[1], "Kind          not known: AWS did not answer (RequestExpired: Request has expired.)");
    }

    #[test]
    fn a_copy_of_an_older_build_is_known_as_behind() {
        let copy = |name: &str, state: &str| aws::CopiedImage { id: "ami-00000000000000001".into(), name: name.into(), state: state.into(), snapshots: vec![] };
        assert_eq!((build_of("runs-on-v2.2-ubuntu24-full-x64-20261006080949"), build_of("runs-on-v2.2-ubuntu24-full-x64"), build_of("")), ("20261006080949", "", ""));
        let copies = [copy("runs-on-v2.2-ubuntu24-full-x64-20261006080949", "available")];
        // A newer build is out: the copy is behind, by the build it holds. The same build, or none known: it is not.
        assert_eq!((behind(&copies, "20261013070000"), behind(&copies, "20261006080949"), behind(&copies, ""), behind(&[], "20261013070000")), (Some("20261006080949"), None, None, None));
        // With the newer one already being copied in, the account holds that build.
        assert_eq!(behind(&[copies[0].clone(), copy("runs-on-v2.2-ubuntu24-full-x64-20261013070000", "pending")], "20261013070000"), None);
        assert_eq!((build_day("20261006080949"), build_day("20261113"), build_day("soon")), ("6 October 2026".to_string(), "13 November 2026".to_string(), "soon".to_string()));
    }

    fn look(recording: &'static str) -> Look { let aws = Replay::of(recording); look_with(&|| aws.clone(), &creds()) }

    /// The same project minutes after its owner activated AWS's advanced features, with no policy edited yet: the
    /// spot rule is gone, the region rule is another policy than before, and from inside the organization still
    /// cannot be read.
    #[test]
    fn an_activated_project_allows_spot_and_is_still_kept_to_its_region() {
        let l = look(include_str!("../fixtures/aws/project-activated/look.jsonl"));
        assert_eq!(l.allowed_regions(), ["eu-north-1"]);
        assert_eq!(l.kind(), Kind::Project { region: "eu-north-1".into(), activated: true });
        let refused: Vec<&aws::Refusal> = l.regions.iter().filter_map(|(_, a)| a.by_organization()).collect();
        assert_eq!(refused.len(), 11);
        assert!(refused.iter().all(|r| r.action == "ec2:DescribeVpcs" && r.policy_id() == Some("p-example1") && r.management_account() == Some("222222222222")), "{refused:?}");
        assert_eq!((&l.spot, &l.on_demand), (&Some(Answer::Allowed), &Some(Answer::Allowed)));
        assert_eq!((l.asked_in.as_deref(), l.spot_cpus, l.on_demand_cpus), (Some("eu-north-1"), Some(32), Some(16)));
        // (The account's own copy of the runner image is there by now.)
        assert_eq!(l.images, [("eu-north-1".to_string(), Some(true))]);
        assert_eq!(l.usable_regions(), ["eu-north-1"]);
        assert_eq!((l.organization_id.as_deref(), l.management_account.as_deref()), (Some("o-example001"), Some("222222222222")));
        let lines = crate::commands::aws_look_lines("111111111111", "", &l);
        // It is said as it is: where it works and what is allowed there. Nothing is asked to be changed.
        assert_eq!(lines[1], "Kind          an AWS project (AWS's new sign-up): it works in eu-north-1 only, with spot machines");
        assert_eq!(lines[4], "Spot          allowed (asked in eu-north-1)");
    }

    /// A project of AWS's new sign-up, on its paid plan, before its advanced features are activated (recorded on
    /// 12 October 2026 in a project AWS had put in Stockholm).
    #[test]
    fn a_project_of_the_new_sign_up_is_known_by_what_it_allows() {
        let l = look(include_str!("../fixtures/aws/project-paid/look.jsonl"));
        assert_eq!(l.allowed_regions(), ["eu-north-1"]);
        assert_eq!(l.kind(), Kind::Project { region: "eu-north-1".into(), activated: false });
        // Every other region is refused by one policy of the organization, spot by another; on-demand is allowed.
        let refused: Vec<&aws::Refusal> = l.regions.iter().filter_map(|(_, a)| a.by_organization()).collect();
        assert_eq!(refused.len(), 11);
        assert!(refused.iter().all(|r| r.action == "ec2:DescribeVpcs" && r.policy_id() == Some("p-example1") && r.management_account() == Some("222222222222")), "{refused:?}");
        let spot = l.spot.as_ref().and_then(|a| a.by_organization()).expect("spot is refused by the organization");
        assert_eq!((spot.action.as_str(), spot.policy_id()), ("ec2:RunInstances", Some("p-example2")));
        assert_eq!(l.on_demand, Some(Answer::Allowed));
        assert_eq!((l.asked_in.as_deref(), l.spot_cpus, l.on_demand_cpus), (Some("eu-north-1"), Some(5), Some(5)));
        // The image jobs run on is not published in Stockholm; the account is a member of an organization it does
        // not manage.
        assert_eq!(l.images, [("eu-north-1".to_string(), Some(false))]);
        assert!(l.usable_regions().is_empty() && l.regions_without_image() == ["eu-north-1"]);
        assert_eq!((l.organization_id.as_deref(), l.management_account.as_deref()), (Some("o-example001"), Some("222222222222")));
        assert_eq!(crate::commands::aws_look_lines("111111111111", "arn:aws:sts::111111111111:assumed-role/AccountFullAccessRole/someone", &l), [
            "AWS account   111111111111 · signed in as arn:aws:sts::111111111111:assumed-role/AccountFullAccessRole/someone",
            "Kind          an AWS project (AWS's new sign-up): it works in eu-north-1 only, and spot machines are refused",
            "Regions       allowed: eu-north-1",
            "              refused by your AWS organization's policy p-example1 (management account 222222222222): us-east-1, us-east-2, us-west-2, ca-central-1, eu-west-1, eu-west-2, eu-central-1, ap-south-1, ap-northeast-1, ap-southeast-1, ap-southeast-2",
            "Spot          refused by your AWS organization's policy p-example2 (management account 222222222222) (asked in eu-north-1)",
            "On-demand     allowed (asked in eu-north-1)",
            "At once       spot 5 vCPUs · on-demand 5 vCPUs",
            "              5 vCPUs is one job of the default size at a time. Ask AWS for more: https://eu-north-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-1216C47A",
            "Runner image  not in eu-north-1: jobs cannot start there until it is copied in",
            "Organization  o-example001, managed from account 222222222222: its policies are not this account's to read or change",
        ]);
    }

    /// An account from AWS's standard sign-up, in no organization (recorded the same day).
    #[test]
    fn an_account_of_its_own_allows_everything() {
        let l = look(include_str!("../fixtures/aws/classic/look.jsonl"));
        assert_eq!(l.allowed_regions().len(), 12);
        assert_eq!(l.kind(), Kind::Open);
        assert_eq!((l.spot.clone(), l.on_demand.clone()), (Some(Answer::Allowed), Some(Answer::Allowed)));
        assert_eq!((l.asked_in.as_deref(), l.spot_cpus, l.on_demand_cpus), (Some("us-east-1"), Some(96), Some(64)));
        // The image jobs run on is published in ten of the twelve regions SuperCI offers.
        assert_eq!(l.usable_regions(), ["us-east-1", "us-east-2", "us-west-2", "eu-west-1", "eu-west-2", "eu-central-1", "ap-south-1", "ap-northeast-1", "ap-southeast-1", "ap-southeast-2"]);
        assert_eq!(l.regions_without_image(), ["ca-central-1", "eu-north-1"]);
        assert!(l.organization_id.is_none() && l.organization().is_none());
        assert_eq!(crate::commands::aws_look_lines("111111111111", "arn:aws:iam::111111111111:root", &l), [
            "AWS account   111111111111 · signed in as arn:aws:iam::111111111111:root",
            "Kind          open: nothing SuperCI does is refused",
            "Regions       allowed: us-east-1, us-east-2, us-west-2, ca-central-1, eu-west-1, eu-west-2, eu-central-1, eu-north-1, ap-south-1, ap-northeast-1, ap-southeast-1, ap-southeast-2",
            "Spot          allowed (asked in us-east-1)",
            "On-demand     allowed (asked in us-east-1)",
            "At once       spot 96 vCPUs · on-demand 64 vCPUs",
            "Runner image  not in ca-central-1, eu-north-1: jobs cannot start there until it is copied in",
        ]);
    }

    /// The image maker's public list as GitHub served it on 12 October 2026: the newest build of the image jobs run
    /// on, with its id in each of the eleven regions it is published in.
    #[test]
    fn the_image_to_copy_is_read_from_its_makers_public_list() {
        let github = Replay::of(include_str!("../fixtures/aws/public/image-list.jsonl"));
        let (stamp, published) = published_image(&github).unwrap();
        assert_eq!(stamp, "20261006080844");
        assert_eq!(published.len(), 11);
        assert!(published.contains(&("eu-west-1".to_string(), "ami-0b8d70b1c8eb9af4a".to_string())) && !published.iter().any(|(r, _)| r == "eu-north-1" || r == "ca-central-1"));
        // Stockholm's copy comes from Ireland, Canada's from N. Virginia; the copy is named as the maker's are.
        assert_eq!(aws::copy_source(&published, "eu-north-1"), Some(&("eu-west-1".to_string(), "ami-0b8d70b1c8eb9af4a".to_string())));
        assert_eq!(aws::copy_source(&published, "ca-central-1").map(|(r, _)| r.as_str()), Some("us-east-1"));
        assert_eq!(copy_name(&stamp), "runs-on-v2.2-ubuntu24-full-x64-20261006080844");
        // GitHub not answering, or answering something else, is said; nothing is guessed.
        assert!(published_image(&Replay::of("")).unwrap_err().starts_with("The list of runner images could not be read from GitHub (it answered 599)"));
    }

    /// What the project answered to further questions, each as AWS worded it.
    #[test]
    fn a_region_the_organization_keeps_the_account_out_of_holds_nothing_to_remove() {
        // As the project answered a read in a region its organization refuses (the recording): no leftover there.
        let recording = include_str!("../fixtures/aws/project-paid/look.jsonl");
        let refused = recording.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).find(|x| x["status"] == 403 && x["url"].as_str().is_some_and(|u| u.contains("us-east-1"))).expect("a refused region in the recording");
        let said = format!("UnauthorizedOperation: {}", aws::xml_tag(refused["answer"].as_str().unwrap(), "Message").unwrap());
        assert!(kept_out_by_organization(&said), "{said}");
        // What a role's own permissions refuse, or anything else AWS says, is still an error to report.
        assert!(!kept_out_by_organization("UnauthorizedOperation: User: arn:aws:sts::111111111111:assumed-role/dev/me is not authorized to perform: ec2:DeleteVpc because no identity-based policy allows the ec2:DeleteVpc action"));
        assert!(!kept_out_by_organization("DependencyViolation: The vpc has dependencies and cannot be deleted."));
    }

    #[test]
    fn what_a_project_refuses_is_read_as_aws_says_it() {
        let aws = Replay::of(include_str!("../fixtures/aws/project-paid/more.jsonl"));
        let asked = |region: &str, action: &str, params: serde_json::Value| futures::executor::block_on(aws::ec2(&aws, region, &creds(), action, params, 0));
        let image = "ami-04c267a6ca05944f8";
        // On-demand machines of any size pass the policies (the account's limit of five vCPUs is met only at a real start).
        for kind in ["t3.micro", "c8a.xlarge", "c8a.8xlarge", "g4dn.xlarge"] {
            let e = asked("eu-north-1", "RunInstances", serde_json::json!({ "DryRun": "true", "ImageId": image, "InstanceType": kind, "MinCount": "1", "MaxCount": "1" })).unwrap_err();
            assert!(e.starts_with("DryRunOperation") && aws::refusal(&e).is_none(), "{kind}: {e}");
        }
        // A spot request of the older kind is refused as a spot launch is.
        let e = asked("eu-north-1", "RequestSpotInstances", serde_json::json!({ "DryRun": "true", "InstanceCount": "1", "LaunchSpecification.ImageId": image, "LaunchSpecification.InstanceType": "c8a.xlarge" })).unwrap_err();
        let r = aws::refusal(&e).unwrap();
        assert_eq!((r.action.as_str(), r.by, r.policy_id()), ("ec2:RequestSpotInstances", aws::By::Organization, Some("p-example2")));
        // The runner image could be copied into the project's region from one that has it.
        let e = asked("eu-north-1", "CopyImage", serde_json::json!({ "DryRun": "true", "SourceRegion": "eu-west-1", "SourceImageId": "ami-0ea113836c24cdf49", "Name": "superci-probe" })).unwrap_err();
        assert!(e.starts_with("DryRunOperation"), "{e}");
        // A member account cannot read its organization's policies: what they say is known only by what they refuse.
        let e = futures::executor::block_on(aws::json_call(&aws, "POST", "https://organizations.us-east-1.amazonaws.com/", "us-east-1", "organizations", Some("AWSOrganizationsV20161128.DescribePolicy"),
            "application/x-amz-json-1.1", Some(&serde_json::json!({ "PolicyId": "p-example1" })), &creds(), 0)).unwrap_err();
        assert!(e.starts_with("AccessDeniedException"), "{e}");
        assert_eq!(aws::refusal(&e).map(|r| r.by), Some(aws::By::Unsaid));
        // Stockholm has the older machine families and not the newest.
        let offered = asked("eu-north-1", "DescribeInstanceTypeOfferings", serde_json::json!({ "LocationType": "region", "Filter.1.Name": "instance-type", "Filter.1.Value.1": "c8a.xlarge", "Filter.1.Value.2": "c7a.xlarge",
            "Filter.1.Value.3": "m7a.xlarge", "Filter.1.Value.4": "c7i.xlarge", "Filter.1.Value.5": "c8g.xlarge", "Filter.1.Value.6": "c7g.xlarge", "Filter.1.Value.7": "m8a.xlarge" })).unwrap();
        assert!(offered.contains("<instanceType>c7a.xlarge</instanceType>") && !offered.contains("<instanceType>c8a.xlarge</instanceType>"));
        // A question nobody recorded has no answer: a test cannot pass on a guess.
        assert!(asked("eu-north-1", "DescribeSubnets", serde_json::json!({})).unwrap_err().starts_with("NotRecorded"));
    }
}

/// The recordings kept as fixtures name no real account: whatever is the account's own was replaced by a stand-in
/// when the fixture was made, and this holds every fixture to it.
#[cfg(test)]
mod fixture_tests {
    const FIXTURES: [(&str, &str); 6] = [
        ("budget", crate::aws_budget::tests::RECORDED),
        ("project-activated/look", include_str!("../fixtures/aws/project-activated/look.jsonl")),
        ("public/image-list", include_str!("../fixtures/aws/public/image-list.jsonl")),
        ("project-paid/look", include_str!("../fixtures/aws/project-paid/look.jsonl")),
        ("project-paid/more", include_str!("../fixtures/aws/project-paid/more.jsonl")),
        ("classic/look", include_str!("../fixtures/aws/classic/look.jsonl")),
    ];

    /// Every run of `len` or more characters that `is` holds for, with what stands right before it.
    fn runs<'a>(text: &'a str, len: usize, is: impl Fn(u8) -> bool) -> Vec<(&'a str, &'a str)> {
        let (bytes, mut out, mut i) = (text.as_bytes(), vec![], 0);
        while i < bytes.len() {
            if is(bytes[i]) {
                let start = i;
                while i < bytes.len() && is(bytes[i]) { i += 1 }
                if i - start >= len { out.push((&text[start.saturating_sub(12)..start], &text[start..i])) }
            } else { i += 1 }
        }
        out
    }

    #[test]
    fn fixtures_name_no_real_account() {
        for (name, text) in FIXTURES {
            // Account numbers: twelve digits. Only stand-ins (one digit twelve times), the public owner of the
            // runner image, and the counters that stand in for request ids.
            for (before, digits) in runs(text, 12, |b| b.is_ascii_digit()) {
                let stand_in = digits.len() == 12 && (digits.bytes().all(|b| b == digits.as_bytes()[0]) || digits == "135269210855" || digits.starts_with("0000000"));
                // (A number's digits after its point are no account.)
                assert!(stand_in || digits.len() != 12 || before.ends_with('.'), "{name}: {before}{digits}");
            }
            // Organizations, policies, request and session ids, addresses, and AWS's sealed message.
            for id in text.split("o-").skip(1).filter(|rest| rest.len() > 10 && rest.as_bytes()[..10].iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())) { assert!(id.starts_with("example"), "{name}: o-{}", &id[..10]) }
            for id in text.split("service_control_policy/").skip(1) { assert!(id.starts_with("p-example"), "{name}: {}", &id[..12.min(id.len())]) }
            for at in text.split('@').skip(1) { assert!(at.starts_with("example.com"), "{name}: @{}", &at[..16.min(at.len())]) }
            for sealed in text.split("Encoded authorization failure message: ").skip(1) { assert!(sealed.starts_with("(removed)"), "{name}: a sealed message was kept") }
            for (_, uuid) in runs(text, 36, |b| b.is_ascii_hexdigit() || b == b'-').into_iter().filter(|(_, u)| u.len() == 36 && u.as_bytes()[8] == b'-') { assert!(uuid.starts_with("00000000-0000-4000-8000-"), "{name}: {uuid}") }
            // No long opaque token is left (a key, a sealed message under another name).
            for (_, token) in runs(text, 80, |b| b.is_ascii_alphanumeric() || b"+/_=-".contains(&b)) {
                // (Words of a form's text, joined by its pluses, are no token.)
                assert!(token.contains("ami-amazon-linux") || token.contains("AccountFullAccessRole") || token.matches('+').count() >= 5, "{name}: {}…", &token[..30])
            }
        }
    }
}
