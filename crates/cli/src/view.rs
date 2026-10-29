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
    /// Every label it answers to, the one workflows are shown first (a control plane from before it could have several: its one).
    pub fn labels(&self) -> Vec<String> {
        let said: Vec<String> = self.status.as_ref().and_then(|s| s["labels"].as_array().cloned()).unwrap_or_default().iter().filter_map(|l| l.as_str().map(str::to_string)).collect();
        if said.is_empty() { vec![self.plane.label().to_string()] } else { said }
    }
    /// The label shown in examples: the first of them (there is always one).
    pub fn label(&self) -> String { self.labels().swap_remove(0) }
    /// What it is called (SuperCI, unless named; one from before it could be named: SuperCI).
    pub fn name(&self) -> String { self.status.as_ref().and_then(|s| s["name"].as_str()).filter(|n| !n.is_empty()).unwrap_or("SuperCI").to_string() }
    /// The latest jobs, each a machine's row. A machine that took another job's work than the one it was started
    /// for (GitHub gives a waiting job to any runner with its label, so jobs queued together run on each other's
    /// machines) is listed as the job it ran: that job's name, its page and how it ended, as GitHub shows them.
    pub fn jobs(&self) -> Vec<Value> {
        let mut jobs = self.status.as_ref().and_then(|s| s["jobs"].as_array().cloned()).unwrap_or_default();
        let own: std::collections::HashMap<u64, Value> = jobs.iter().filter_map(|j| Some((j["job_id"].as_u64()?, j.clone()))).collect();
        for j in jobs.iter_mut() {
            let Some(ran) = j["ran"].as_u64().and_then(|id| own.get(&id)) else { continue };
            j["started_for"] = j["job_id"].clone();
            ["job_id", "name", "workflow", "run_id", "attempt", "repo", "conclusion"].into_iter().for_each(|field| j[field] = ran[field].clone());
        }
        jobs
    }
    pub fn app_slug(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["slug"].as_str().map(str::to_string)) }
    /// Where the first App's GitHub is, when not github.com.
    pub fn app_host(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["host"].as_str().map(str::to_string)) }
    pub fn app_owner(&self) -> Option<String> { self.status.as_ref().and_then(|s| s["app"]["owner"].as_str().map(str::to_string)) }
    pub fn launchers(&self) -> Vec<superci_core::plane::Launcher> {
        self.status.as_ref().and_then(|s| serde_json::from_value(s["launchers"].clone()).ok()).unwrap_or_default()
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
    /// This month's estimated machine spend in all clouds together, and in one cloud account (its providers together).
    pub fn spend_total(&self) -> f64 { self.status.as_ref().and_then(|s| s["spend"].as_object().map(|m| m.values().filter_map(|v| v.as_f64()).sum::<f64>() + 0.0)).unwrap_or(0.0) }
    pub fn spend_account(&self, account: &str) -> f64 {
        self.status.as_ref().and_then(|s| s["spend"].as_object().map(|m| m.iter().filter(|(c, _)| superci_core::plane::account_of(c) == account).filter_map(|(_, v)| v.as_f64()).sum::<f64>() + 0.0)).unwrap_or(0.0)
    }
    /// Today's (UTC), likewise.
    pub fn spend_today_total(&self) -> f64 { self.status.as_ref().and_then(|s| s["spend_today"].as_object().map(|m| m.values().filter_map(|v| v.as_f64()).sum::<f64>() + 0.0)).unwrap_or(0.0) }
    pub fn spend_today_account(&self, account: &str) -> f64 {
        self.status.as_ref().and_then(|s| s["spend_today"].as_object().map(|m| m.iter().filter(|(c, _)| superci_core::plane::account_of(c) == account).filter_map(|(_, v)| v.as_f64()).sum::<f64>() + 0.0)).unwrap_or(0.0)
    }
    /// Whether this control plane keeps limits for a day and what the AWS account's own budget does.
    pub fn daily_limits(&self) -> bool { self.status.as_ref().is_some_and(|s| s["daily_limits"] == true) }
    /// When AWS last refused a machine by its budget's rule, if it has.
    pub fn budget_stopped_ms(&self) -> Option<u64> { self.status.as_ref().and_then(|s| s["aws_budget_stopped_ms"].as_u64()) }
    /// Whether this control plane keeps a limit for the whole CI and for a cloud account (one from before does not).
    pub fn account_limits(&self) -> bool { self.status.as_ref().is_some_and(|s| s["account_limits"] == true) }
    /// Whether this control plane starts Cloudflare containers itself.
    pub fn own_containers(&self) -> bool { self.status.as_ref().is_some_and(|s| s["containers"] == true) }
    /// The clouds connected for jobs, AWS first.
    pub fn clouds(&self) -> Vec<String> {
        let own = self.status.as_ref().and_then(|s| s["own_cloud"].as_str().map(str::to_string)).unwrap_or_else(|| "cloudflare".into());
        let mut c: Vec<String> = self.aws.then(|| "aws".to_string()).into_iter().chain(self.own_containers().then_some(own)).chain(self.launchers().into_iter().map(|a| a.cloud)).collect();
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
            let new = Pool { regions: vec![], off: false, cloud: c.clone(), max_jobs: None, monthly_usd: None, function: false };
            match order.iter().position(|p| p.cloud == "aws") { Some(at) if c == AWS_ON_DEMAND => order.insert(at + 1, new), _ => order.push(new) }
        }
        order
    }
    /// The regions a place of AWS's starts its machines in, in order: its own, of those the control plane is set up
    /// in; else all of those.
    pub fn pool_regions(&self, pool: &superci_core::plane::Pool) -> Vec<String> {
        let all = self.aws_regions();
        let own: Vec<String> = pool.regions.iter().filter(|r| all.contains(r)).cloned().collect();
        if own.is_empty() { all } else { own }
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

impl PlaneView {
    /// A CI of its own: GitHub or GitLab sends jobs to it, it has not moved away, and it is not standing by for a
    /// move. Another such control plane beside the one in use is another team's: never a place to move to, never
    /// deleted from beside it.
    pub fn serves_its_own(&self) -> bool { (self.github || self.gitlab) && self.moved_to.is_none() && !self.standby }

    /// Whether the control plane at `from` may move here: only onto one that serves nothing yet, or back onto the
    /// one it moved from (a move replaces the target's GitHub App and settings).
    pub fn takes_a_move_from(&self, from: &str) -> bool { !(self.github || self.gitlab) || self.standby || self.moved_to.as_deref() == Some(from) }

    /// Whether stopping the control plane `used` deletes this other one too (true), leaves it without a word
    /// (false), or leaves it and says why. One that `used` moved from always goes with it. With another CI still
    /// running here (`another_ci`), nothing else goes: the ones that serve nothing may be that CI's places to move to.
    /// With none, SuperCI is stopped altogether and the ones that serve nothing go too. One that does not answer
    /// cannot say what it serves: left, with the command that deletes it.
    pub fn goes_with(&self, used: &Plane, another_ci: bool) -> std::result::Result<bool, String> {
        let (place, id) = (self.plane.place(), self.plane.plane_id());
        if self.moved_to.as_deref() == Some(used.url()) { return Ok(true) }
        if self.serves_its_own() { return Err(format!("{place} serves a code host of its own and goes on as before (`superci leave --plane={id} --confirm` stops that one)")) }
        if !self.online { return Err(format!("{place} did not answer and was left as it is (`superci planes delete {id} --confirm` deletes it)")) }
        Ok(!another_ci)
    }
}

/// Whether version `a` (none: from before versions) is older than `b`.
/// The first version that can move to another control plane.
pub const MOVE_VERSION: &str = "0.9.3";

thread_local! {
    /// The control plane this request or command names (its id, or its address): a page opened for one, `--plane=ID`,
    /// `SUPERCI_PLANE`. Each request and each command runs on a thread of its own.
    static NAMED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Names the control plane for what this thread does next (none: the only one, see `in_use` and `several`).
pub fn name(named: Option<&str>) { NAMED.with(|n| *n.borrow_mut() = named.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)) }

pub fn named() -> Option<String> { NAMED.with(|n| n.borrow().clone()) }

/// Whether a control plane is the one a name says: by its id, or by its address.
pub fn is(plane: &Plane, named: &str) -> bool { plane.plane_id() == named || plane.url().trim_end_matches('/') == named.trim_end_matches('/') }

/// The control planes that serve a code host of their own, when there are several and none is named: nothing picks
/// among them (no "first that answers", no last used); whoever asked has to say which.
pub fn several(views: &[PlaneView]) -> Option<Vec<&PlaneView>> {
    if named().is_some_and(|n| views.iter().any(|v| is(&v.plane, &n))) { return None }
    let live: Vec<&PlaneView> = views.iter().filter(|v| v.serves_its_own()).collect();
    (live.len() > 1).then_some(live)
}

/// What is said to a command when several control planes are live and it named none.
pub fn name_one(live: &[&PlaneView]) -> String {
    let rows: Vec<String> = live.iter().map(|v| format!("  {}  {}", v.plane.plane_id(), v.plane.place())).collect();
    format!("{} control planes are live here. Name one: --plane=ID (or SUPERCI_PLANE=ID). Nothing was done.\n{}", live.len(), rows.join("\n"))
}

/// The control plane in use: of those that have not moved away, the one GitHub or GitLab sends jobs to (else the
/// first). Only one is in use at a time; the others are places to move to.
pub fn in_use(views: &[PlaneView]) -> usize {
    // The one named for this request or command (see `name`), when it is among them.
    if let Some(named) = named() {
        if let Some(i) = views.iter().position(|v| is(&v.plane, &named)) { return i }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: Option<Value>) -> PlaneView {
        PlaneView { plane: Plane::Seen { url: "https://plane.example".into(), plane_id: "aaaaaaaaaaaa".into() }, online: true, github: false, installed: false, aws: false, runners: false, gitlab: false, moved_to: None, standby: false, version: None, status }
    }

    /// One from before there could be several GitLabs says its one, with its token's scopes; one without GitLab, none.
    #[test]
    fn the_gitlab_of_a_control_plane_from_before_several_is_its_one() {
        let v = seen(Some(serde_json::json!({ "gitlab": { "url": "https://gitlab.com" }, "permissions": { "gitlab_scopes": ["api"] } })));
        assert_eq!(v.gitlabs(), [(String::new(), "https://gitlab.com".to_string(), Some(vec!["api".to_string()]))]);
        assert!(seen(Some(serde_json::json!({}))).gitlabs().is_empty() && seen(None).gitlabs().is_empty());
        // A connection said without where it is: not one.
        assert!(seen(Some(serde_json::json!({ "gitlabs": [{ "id": "x" }] }))).gitlabs().is_empty());
    }

    /// A machine that ran another job's work is listed as that job; a row with no job of its own is listed as it is
    /// and no other job's row is taken for it. An AWS connection said without its region is none, and its label is
    /// the control plane's own when it says none.
    #[test]
    fn a_job_is_listed_as_the_work_it_ran_and_what_is_said_only_in_part_is_not_taken() {
        let v = seen(Some(serde_json::json!({ "jobs": [
            { "job_id": 1, "name": "lint", "ran": 2 },
            { "job_id": 2, "name": "tests", "repo": "o/r", "conclusion": "success" },
            { "name": "no id", "ran": 9 },
        ], "aws": { "account_id": "123456789012" } })));
        let jobs = v.jobs();
        assert_eq!((jobs[0]["name"].as_str(), jobs[0]["job_id"].as_u64(), jobs[0]["started_for"].as_u64(), jobs[0]["conclusion"].as_str()), (Some("tests"), Some(2), Some(1), Some("success")));
        assert_eq!(jobs[2], serde_json::json!({ "name": "no id", "ran": 9 }));
        assert_eq!((v.aws_account(), v.aws_regions()), (None, Vec::<String>::new()));
        assert_eq!(v.label(), "superci");
    }
}
