//! What the dashboard shows, gathered fresh on each page load from your signed-in clouds and your control planes.
use serde_json::Value;

use crate::cloudflare::{self, health};
use crate::plane::Plane;

#[derive(Clone)]
pub struct PlaneView {
    pub plane: Plane,
    pub online: bool,
    pub github: bool,
    pub installed: bool,
    pub aws: bool,
    /// At least one cloud is connected to run jobs.
    pub runners: bool,
    /// GitLab connected (jobs from its projects' webhooks).
    pub gitlab: bool,
    /// Where it moved to, once another control plane took over from it; and whether it is taking over (a move to it
    /// under way: not in use yet).
    pub moved_to: Option<String>,
    pub standby: bool,
    /// Its SuperCI version (one from before versions began, at 0.2.0, is 0.1.0).
    pub version: Option<String>,
    /// The control plane's own view (App, installations, AWS connection, jobs), when this session's key reached it.
    pub status: Option<Value>,
}

impl PlaneView {
    /// Online, but older than this dashboard: an update brings it to this dashboard's version.
    pub fn outdated(&self) -> bool { self.online && older(self.version.as_deref(), DASHBOARD_VERSION) }
    pub fn version_name(&self) -> String { self.version.clone().unwrap_or_default() }
    /// Jobs can come (GitHub's App installed, or GitLab connected) and run (a runner provider).
    pub fn ready(&self) -> bool { self.online && ((self.github && self.installed) || self.gitlab) && self.runners }
    /// Where GitLab is, when connected.
    pub fn gitlab_url(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["gitlab"]["url"].as_str().map(str::to_string)) }
    /// Every GitLab connection, the first first: its name (nothing for the first), where it is, its token's scopes
    /// (not read yet: none). A control plane from before there could be several: its one.
    pub fn gitlabs(&self) -> Vec<(String, String, Option<Vec<String>>)> {
        let Some(st) = &self.status else { return vec![] };
        let scopes = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>());
        match st["gitlabs"].as_array() {
            Some(list) => list.iter().filter_map(|g| Some((g["id"].as_str().unwrap_or_default().to_string(), g["url"].as_str()?.to_string(), scopes(&g["scopes"])))).collect(),
            None => self.gitlab_url().map(|u| vec![(String::new(), u, scopes(&st["permissions"]["gitlab_scopes"]))]).unwrap_or_default(),
        }
    }
    pub fn jobs(&self) -> Vec<Value> { self.status.as_ref().and_then(|s| s["jobs"].as_array().cloned()).unwrap_or_default() }
    pub fn app_slug(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["slug"].as_str().map(str::to_string)) }
    /// Where the first App's GitHub is, when not github.com.
    pub fn app_host(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["host"].as_str().map(str::to_string)) }
    pub fn app_owner(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["owner"].as_str().map(str::to_string)) }
    pub fn agents(&self) -> Vec<superci_core::plane::Agent> {
        self.status.as_ref().and_then(|s| serde_json::from_value(s["agents"].clone()).ok()).unwrap_or_default()
    }
    pub fn routing(&self) -> superci_core::plane::Routing {
        self.status.as_ref().and_then(|s| serde_json::from_value(s["routing"].clone()).ok()).unwrap_or_default()
    }
    /// The default machine (for `runs-on: superci` alone), as set in the dashboard.
    pub fn machine(&self) -> superci_core::spec::Spec {
        self.status.as_ref().and_then(|s| serde_json::from_value(s["machine"].clone()).ok()).unwrap_or_default()
    }
    /// This month's estimated machine spend by cloud.
    pub fn spend(&self, cloud: &str) -> f64 { self.status.as_ref().and_then(|s| s["spend"][cloud].as_f64()).unwrap_or(0.0) }
    /// Whether this control plane starts Cloudflare containers itself.
    pub fn own_containers(&self) -> bool { self.status.as_ref().is_some_and(|s| s["containers"] == true) }
    /// The clouds connected for jobs, AWS first.
    pub fn clouds(&self) -> Vec<String> {
        let own = self.status.as_ref().and_then(|s| s["own_cloud"].as_str().map(str::to_string)).unwrap_or_else(|| "cloudflare".into());
        let mut c: Vec<String> = self.aws.then(|| "aws".to_string()).into_iter().chain(self.own_containers().then_some(own)).chain(self.agents().into_iter().map(|a| a.cloud)).collect();
        c.dedup();
        c
    }
    /// The places jobs go, in order: as set, then any connected since. AWS is two places: its spot machines (`aws`)
    /// and its on-demand ones (right after, unless the order has them elsewhere).
    pub fn order(&self) -> Vec<superci_core::plane::Pool> {
        use superci_core::plane::{Pool, AWS_ON_DEMAND};
        let routing = self.routing();
        let mut places = self.clouds();
        if let Some(at) = places.iter().position(|c| c == "aws") { places.insert(at + 1, AWS_ON_DEMAND.to_string()) }
        let mut order: Vec<_> = routing.order.into_iter().filter(|p| places.contains(&p.cloud)).collect();
        for c in places {
            if order.iter().any(|p| p.cloud == c) { continue }
            let new = Pool { off: false, cloud: c.clone(), max_jobs: None, monthly_usd: None };
            match order.iter().position(|p| p.cloud == "aws") { Some(at) if c == AWS_ON_DEMAND => order.insert(at + 1, new), _ => order.push(new) }
        }
        order
    }
    /// The regions AWS machines go to, in order (older control planes: the connection's one).
    pub fn aws_regions(&self) -> Vec<String> {
        let listed: Vec<String> = self.status.as_ref().and_then(|s| s["aws_regions"].as_array().cloned()).unwrap_or_default().iter().filter_map(|r| r.as_str().map(str::to_string)).collect();
        if listed.is_empty() { self.aws_account().map(|(_, r)| vec![r]).unwrap_or_default() } else { listed }
    }
    pub fn aws_account(&self) -> Option<(String, String)> {
        let a = &self.status.as_ref()?["aws"];
        Some((a["account_id"].as_str()?.to_string(), a["region"].as_str()?.to_string()))
    }
}

/// This dashboard's version: what an update brings a control plane to.
pub const DASHBOARD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether version `a` (none: from before versions) is older than `b`.
/// The first version that can move to another control plane.
pub const MOVE_VERSION: &str = "0.9.3";

/// The control plane in use: of those that have not moved away, the one GitHub or GitLab sends jobs to (else the
/// first). Only one is in use at a time; the others are places to move to.
pub fn in_use(views: &[PlaneView]) -> usize {
    let staying: Vec<usize> = (0..views.len()).filter(|i| views[*i].moved_to.is_none()).collect();
    let active: Vec<usize> = staying.iter().copied().filter(|i| !views[*i].standby).collect();
    active.iter().copied().find(|i| views[*i].online && (views[*i].github || views[*i].gitlab))
        .or_else(|| active.first().copied()).or_else(|| staying.first().copied()).unwrap_or(0)
}

pub fn older(a: Option<&str>, b: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    a.is_none_or(|a| parse(a) < parse(b))
}

pub fn plane_view(plane: &Plane, key: Option<&str>) -> PlaneView {
    // Its public yes/no and (with a key) what it says of itself, asked at once.
    let t = std::time::Instant::now();
    let (h, health_ms, said) = std::thread::scope(|scope| {
        let said = key.map(|k| scope.spawn(move || { let t = std::time::Instant::now(); (cloudflare::status(plane.url(), k), t.elapsed().as_millis()) }));
        let h = health(plane.url());
        let health_ms = t.elapsed().as_millis();
        (h, health_ms, said.and_then(|s| s.join().ok()))
    });
    let flag = |k: &str| h.as_ref().is_some_and(|h| h[k] == true);
    PlaneView { plane: plane.clone(), online: h.is_some(), github: flag("github"), installed: flag("installed"), aws: flag("aws"), runners: flag("runners"), gitlab: flag("gitlab"),
        moved_to: h.as_ref().and_then(|h| h["moved_to"].as_str().map(str::to_string)), standby: flag("standby"),
        version: h.as_ref().map(|h| h["version"].as_str().unwrap_or("0.1.0").to_string()),
        status: said.and_then(|(s, status_ms)| {
            if std::env::var_os("SUPERCI_TIMING").is_some() { eprintln!("  {:>6} ms  {}: health · {:>6} ms status{}", health_ms, plane.place(), status_ms, if s.is_some() { "" } else { " (not read)" }) }
            s
        }) }
}
