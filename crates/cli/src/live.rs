//! Live checks against real clouds (paid, a few cents each; run one with `cargo test -p superci -- --ignored live_`).
//! Each starts a runner the way a control plane does, with a short time bound, and then nobody stops it, as if the
//! control plane were gone: the runner registers with GitHub, waits for a job that never comes (a label no workflow
//! uses), and must end at its bound on its own. It is cleaned up afterwards whatever happens.
//!
//! Settings (environment): SUPERCI_LIVE_APP (a GitHub App's JSON: id, slug, owner, and pem_path to its key), SUPERCI_LIVE_REPO
//! (owner/repo the App is installed on, for a repository-level runner).
#![cfg(test)]
use std::time::{Duration, Instant};

use futures::executor::block_on;
use superci_core::github::{self, App, Owner};
use superci_core::io::{Http, Request};

pub fn http() -> crate::aws::Blocking { crate::aws::Blocking::new() }

pub fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 }

pub fn app() -> App {
    let path = std::env::var("SUPERCI_LIVE_APP").expect("SUPERCI_LIVE_APP: a GitHub App's JSON");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let pem_path = v["pem_path"].as_str().map(str::to_string).unwrap_or_else(|| std::path::Path::new(&path).with_extension("pem").to_string_lossy().into());
    App { id: v["id"].as_u64().unwrap(), slug: v["slug"].as_str().unwrap_or_default().into(), pem: std::fs::read_to_string(pem_path).unwrap(),
        webhook_secret: String::new(), owner: v["owner"].as_str().unwrap_or_default().into(), owner_is_org: true, host: None }
}

/// A just-in-time runner registration nothing will ever run on, with what it takes to watch and remove it.
pub struct Idle { pub jit: String, pub id: u64, token: String, repo: String }

impl Idle {
    pub fn new(name: &str) -> Idle {
        let (http, app, repo) = (http(), app(), std::env::var("SUPERCI_LIVE_REPO").expect("SUPERCI_LIVE_REPO: owner/repo"));
        let installs = block_on(github::installations(&http, &app, now_ms())).unwrap();
        let owner = repo.split('/').next().unwrap();
        let install = installs.iter().find(|i| i.account == owner).expect("the App is installed on the repository's owner");
        let token = block_on(github::installation_token(&http, &app, install.id, now_ms())).unwrap();
        let label = format!("superci-leaktest-{}", superci_core::crypto::random_id(8));
        let (jit, id) = block_on(github::jit_config(&http, &app.api(), &token, &Owner { org: false, login: owner.into() }, &repo, name, &[&label], "/home/runner/work")).unwrap();
        Idle { jit, id, token, repo }
    }

    /// GitHub's view of the runner: "online" while it waits for a job, "offline" once its machine is gone, None when removed.
    pub fn status(&self) -> Option<String> {
        let mut r = Request::new("GET", &format!("https://api.github.com/repos/{}/actions/runners/{}", self.repo, self.id));
        r.headers.extend([("authorization".into(), format!("Bearer {}", self.token)), ("accept".into(), "application/vnd.github+json".into()), ("user-agent".into(), "superci-live".into())]);
        let res = block_on(http().send(r)).ok()?;
        if res.status == 404 { return None }
        serde_json::from_slice::<serde_json::Value>(&res.body).ok()?["status"].as_str().map(str::to_string)
    }
}

impl Drop for Idle {
    fn drop(&mut self) { let _ = block_on(github::delete_runner(&http(), &github::api_base(None), &self.token, &Owner { org: false, login: String::new() }, &self.repo, self.id)); }
}

/// Every 30 seconds (one line a minute at most is printed, plus every change) until `ended` says so or `limit` passes.
/// Returns when it ended, from `start`.
pub fn watch(what: &str, start: Instant, limit: Duration, mut look: impl FnMut() -> (String, bool)) -> Option<Duration> {
    let mut last = (String::new(), Instant::now() - Duration::from_secs(120));
    loop {
        let (state, ended) = look();
        let t = start.elapsed();
        if state != last.0 || last.1.elapsed() >= Duration::from_secs(60) {
            eprintln!("[{:>3}s] {what}: {state}", t.as_secs());
            last = (state, Instant::now());
        }
        if ended { return Some(t) }
        if t > limit { return None }
        std::thread::sleep(Duration::from_secs(30));
    }
}

mod aws_live {
    use super::*;
    use superci_core::aws::{self, Credentials, Launch};

    /// Credentials of a named profile in ~/.aws/credentials (SUPERCI_LIVE_AWS_PROFILE).
    fn creds() -> Credentials {
        let profile = std::env::var("SUPERCI_LIVE_AWS_PROFILE").expect("SUPERCI_LIVE_AWS_PROFILE");
        let file = std::fs::read_to_string(format!("{}/.aws/credentials", std::env::var("HOME").unwrap())).unwrap();
        let mut section = String::new();
        let (mut id, mut secret, mut token) = (None, None, None);
        for line in file.lines().map(str::trim) {
            if line.starts_with('[') { section = line.trim_matches(|c| c == '[' || c == ']').to_string(); continue }
            if section != profile { continue }
            if let Some((k, v)) = line.split_once('=') {
                let v = Some(v.trim().to_string());
                match k.trim() { "aws_access_key_id" => id = v, "aws_secret_access_key" => secret = v, "aws_session_token" => token = v, _ => {} }
            }
        }
        Credentials { access_key_id: id.expect("key id"), secret_access_key: secret.expect("secret"), session_token: token, expires_at_ms: u64::MAX }
    }

    fn state(region: &str, c: &Credentials, id: &str) -> String {
        let xml = block_on(aws::ec2(&http(), region, c, "DescribeInstances", serde_json::json!({ "InstanceId": [id] }), now_ms())).unwrap_or_default();
        xml.split("<instanceState>").nth(1).and_then(|s| aws::xml_tag(s, "name")).unwrap_or("unknown").to_string()
    }

    /// AWS's prices as the control plane reads them: a zone's spot prices over a past day, the Price List's on-demand
    /// and gp3 prices (needs pricing:GetProducts), against what AWS publishes.
    #[test]
    #[ignore]
    fn live_aws_prices() {
        let (region, c, now) = ("us-east-1", creds(), now_ms());
        let day = block_on(aws::spot_cost(&http(), region, "us-east-1a", &c, "c7a.large", "linux", now - 26 * 3_600_000, now - 2 * 3_600_000, now)).unwrap();
        let minute = block_on(aws::spot_cost(&http(), region, "us-east-1a", &c, "c7a.large", "linux", now - 3_600_000, now - 3_590_000, now)).unwrap();
        eprintln!("c7a.large spot in us-east-1a: a day ${day:.4} (${:.4}/h on average), ten seconds billed as a minute ${minute:.6}", day / 24.0);
        assert!(day > 0.0 && (minute * 60.0 - day / 24.0).abs() < day / 24.0, "a minute at about the hour's rate");
        match block_on(aws::on_demand_price(&http(), &c, region, "c7a.large", "linux", now)) { Ok(p) => eprintln!("c7a.large on-demand ${p}/h (AWS lists $0.10264)"), Err(e) => eprintln!("on-demand: {e}") }
        // What a machine sent (CloudWatch), e.g. the one live_aws_machine_ends_at_its_time_bound started (SUPERCI_LIVE_AWS_INSTANCE).
        if let Ok(id) = std::env::var("SUPERCI_LIVE_AWS_INSTANCE") {
            let bytes = block_on(aws::bytes_sent(&http(), region, &c, &id, now - 12 * 3_600_000, now, now)).unwrap();
            eprintln!("{id} sent {:.1} MB", bytes / 1e6);
        }
        match block_on(aws::disk_price(&http(), &c, region, now)) { Ok(p) => eprintln!("gp3 ${p}/GB-month (AWS lists $0.08)"), Err(e) => eprintln!("gp3: {e}") }
    }

    /// A spot machine with a control plane's user data and a 3-minute bound terminates itself, nobody asking.
    #[test]
    #[ignore]
    fn live_aws_machine_ends_at_its_time_bound() {
        let (region, c) = ("us-east-1", creds());
        let idle = Idle::new(&format!("superci-leaktest-aws-{}", superci_core::crypto::random_id(6)));
        let image = block_on(aws::latest_image(&http(), region, &c, "135269210855", "runs-on-v2.2-ubuntu24-full-x64-*", now_ms())).unwrap();
        let user_data = aws::runner_user_data(&superci_core::io::Work::GitHub { jit: idle.jit.clone() }, 3).unwrap();
        let types = ["c7a.large".to_string(), "m7a.large".into(), "c7i.large".into()];
        let tags = [("superci".to_string(), "leaktest".to_string())];
        let start = Instant::now();
        let aws::Started { id, kind, .. } = block_on(aws::run_instance(&http(), &c, &Launch { region, network: None, image: &image, types: &types, user_data: &user_data, disk_gb: 30, tags: &tags, spot: true, os: "linux" }, now_ms())).unwrap();
        eprintln!("launched {id} ({kind}) in {region}, bound 3 min");
        let mut online = false;
        let ended = watch("machine", start, Duration::from_secs(10 * 60), || {
            let s = state(region, &c, &id);
            let runner = idle.status().unwrap_or_else(|| "removed".into());
            online |= runner == "online";
            (format!("{s}, runner {runner}"), s == "shutting-down" || s == "terminated")
        });
        if ended.is_none() { let _ = block_on(aws::terminate_instance(&http(), region, &c, &id, now_ms())); }
        let ended = ended.expect("the machine was still up after 10 minutes (terminated now)");
        assert!(online, "its runner never came online: the bound was not tested on a waiting runner");
        assert!(ended >= Duration::from_secs(170) && ended <= Duration::from_secs(7 * 60), "ended after {ended:?}");
    }
}
