//! SuperCI's commands: what the dashboard's pages do, from a terminal or a coding agent. A command that changes
//! something fills in the form its page has and hands it to the dashboard's own code (`Dashboard::act`), so the two
//! cannot differ; one that reads shows what the control plane says. Nothing is ever asked in the terminal: what a
//! person must do first (a sign-in, in the browser) is said, with the command for it, and the status is 3.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::dashboard::{plane_json, Background, Dashboard, Task, AWS_ENDED, NOT_SIGNED_IN};
use crate::plane::Plane;
use crate::view::PlaneView;
use crate::{store, Result};

pub const HELP: &str = "SuperCI — GitHub Actions and GitLab CI jobs on your own clouds

  superci                       Opens your dashboard (on this machine only, while this runs).
  superci login [cloud]         Signs in with a cloud in the browser (aws, cloudflare, modal), and ends.
  superci logout                Removes SuperCI's sign-ins from this machine.

Look (add --json for programs and coding agents):
  superci status                The control plane in use: where, its version, whether jobs can run.
  superci jobs                  The latest jobs: how each ended, where it ran, what it cost.
  superci runners               Runner providers in order, their limits, and the default machine.
  superci repos                 GitHub accounts and GitLab connections, and the workflows' limits.
  superci planes                Every control plane found in the clouds signed in to.
  superci gitlab projects       A GitLab connection's projects, and which send their jobs here.

Control plane:
  superci plane deploy aws --region us-east-1
  superci plane deploy cloudflare [--account ID]
  superci plane deploy modal
  superci plane update [--plane ID]
  superci plane move ID                     Moves to another control plane (settings, history, providers).
  superci plane allow [--plane ID]          Gives its AWS role what this version asks for.
  superci plane delete ID --yes             Deletes one that is not in use.
  superci leave --yes                       Stops using SuperCI: everything it made in your clouds is deleted.

Runners:
  superci runners add aws [--region us-east-1] | cloudflare | modal
  superci runners remove CLOUD --yes [--only-here] [--stop-jobs]
  superci runners order CLOUD...            aws, aws-on-demand, cloudflare, modal: first is tried first.
  superci runners set CLOUD [--max-jobs N|none] [--monthly-usd N|none] [--on|--off]
                            [--regions a,b,c] [--networks \"REGION subnet-… sg-… [private]\"|none]   (aws)
                            [--location enam|weur|auto|…] [--image https://…|none]                  (cloudflare)
  superci machine [--cpu N] [--ram GB] [--disk GB] [--arch x64|arm64] [--os linux|windows] [--on-demand|--spot]
                                            The machine `runs-on: superci` alone gets.

Repositories:
  superci github connect OWNER [--host https://github.example.com]
                                            Opens GitHub in the browser: a person creates the App and chooses its
                                            repositories there (GitHub offers that nowhere else).
  superci github remove OWNER --yes         A further organization; the first goes with `superci leave`.
  superci gitlab connect --url https://gitlab.com [--gitlab ID|new]
                                            The token is read from SUPERCI_GITLAB_TOKEN (scopes: api, create_runner,
                                            manage_runner).
  superci gitlab enable PROJECT_ID | --all [--gitlab ID]
  superci gitlab disable PROJECT_ID [--gitlab ID]
  superci gitlab disconnect [ID] --yes
  superci limits --max-cpu N                The largest machine a label may ask for.
  superci public add|remove OWNER/REPO      Public repositories allowed to run here.

  --json          One JSON object: what was done or read, or \"error\" with \"needs\" (the command a person runs first).
  --no-browser    (dashboard, login, github connect) Prints the link without opening it.

Status: 0 done · 1 failed · 2 not a command · 3 a person is needed first (see \"needs\").
SuperCI keeps its own sign-ins in ~/.superci (SUPERCI_HOME to put it elsewhere) and reads no other tool's. On a
machine with no browser, a sign-in can be given by name: SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_MODAL_TOKEN_ID and
SUPERCI_MODAL_TOKEN_SECRET.";

/// What was typed: words, and flags with their values (`--region us-east-1`, `--region=us-east-1`; a flag given
/// twice keeps both).
pub struct Args { pub words: Vec<String>, flags: HashMap<String, Vec<String>> }

/// Flags that stand alone; every other flag takes the word after it.
const SWITCHES: [&str; 12] = ["json", "yes", "no-browser", "all", "only-here", "stop-jobs", "on", "off", "on-demand", "spot", "help", "version"];
const VALUES: [&str; 20] = ["region", "account", "plane", "host", "url", "gitlab", "max-jobs", "monthly-usd", "regions", "networks", "location", "image", "cpu", "ram", "disk", "arch", "os", "max-cpu", "to", "token-env"];

impl Args {
    pub fn parse(raw: &[String]) -> Result<Args> {
        let (mut words, mut flags) = (vec![], HashMap::<String, Vec<String>>::new());
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                if a == "-h" { flags.entry("help".into()).or_default(); } else if a == "-V" { flags.entry("version".into()).or_default(); }
                else if a.starts_with('-') && a.len() > 1 { return Err(format!("{a} is not a flag of superci")) } else { words.push(a.clone()) }
                continue
            };
            let (name, given) = match flag.split_once('=') { Some((n, v)) => (n, Some(v.to_string())), None => (flag, None) };
            if SWITCHES.contains(&name) { flags.entry(name.into()).or_default(); }
            else if VALUES.contains(&name) {
                let value = match given { Some(v) => v, None => it.next().cloned().ok_or(format!("--{name} needs a value"))? };
                flags.entry(name.into()).or_default().push(value);
            } else { return Err(format!("--{name} is not a flag of superci")) }
        }
        Ok(Args { words, flags })
    }
    pub fn has(&self, flag: &str) -> bool { self.flags.contains_key(flag) }
    fn get(&self, flag: &str) -> Option<&str> { self.flags.get(flag).and_then(|v| v.last()).map(String::as_str) }
    fn all(&self, flag: &str) -> Vec<&str> { self.flags.get(flag).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default() }
    fn word(&self, i: usize) -> &str { self.words.get(i).map(String::as_str).unwrap_or("") }
}

/// What a command did or read: lines for a person, and the same as data.
pub struct Done { pub said: Vec<String>, pub data: Value }

impl Done {
    fn said(line: impl Into<String>) -> Done { let line = line.into(); Done { data: json!({ "ok": true, "said": line }), said: if line.is_empty() { vec!["Done.".into()] } else { vec![line] } } }
}

/// The command a person runs first, when that is what an error asks for.
pub fn needs(error: &str) -> Option<&'static str> {
    if error == NOT_SIGNED_IN { return Some("superci login") }
    let has = |words: &[&str]| words.iter().any(|w| error.contains(w));
    if error == AWS_ENDED || has(&["Sign in with AWS", "Your AWS sign-in has ended"]) { return Some("superci login aws") }
    if has(&["Sign in with Cloudflare", "Cloudflare sign-in expired"]) { return Some("superci login cloudflare") }
    if has(&["Sign in with Modal"]) { return Some("superci login modal") }
    if has(&["Sign in where this control plane runs", "sign in where it runs"]) { return Some("superci login") }
    None
}

const CLOUDS: [&str; 3] = ["aws", "cloudflare", "modal"];
const USAGE: &str = "not a command of superci (see `superci --help`)";

fn signed_in() -> Dashboard { Dashboard::signed_in(store::Store::new()) }

/// A dashboard ready for a change: signed in, its clouds looked in, the control plane in use chosen.
fn ready() -> Result<Dashboard> { let mut d = signed_in(); d.ready()?; Ok(d) }

fn in_use(d: &Dashboard) -> Result<Plane> { d.plane_in_use().ok_or_else(|| "There is no control plane yet. Set one up: `superci plane deploy aws --region us-east-1` (or cloudflare, or modal).".to_string()) }

fn confirmed(args: &Args, what: &str) -> Result<()> { if args.has("yes") { Ok(()) } else { Err(format!("This {what}. Add --yes to do it.")) } }

fn field(name: &'static str, value: impl Into<String>) -> (&'static str, String) { (name, value.into()) }

/// Runs a command that is not the dashboard itself. `steps` is told each step of a long one as it begins.
pub fn run(args: &Args, steps: &mut dyn FnMut(&str)) -> Result<Done> {
    let w = |i: usize| args.word(i);
    match (w(0), w(1)) {
        ("status", "") => { let v = signed_in().status()?; Ok(Done { said: status_lines(&v), data: v }) }
        ("jobs", "") => jobs(),
        ("runners", "") => runners(),
        ("repos", "") => repos(),
        ("planes", "") => planes(),
        ("logout", "") => { let said = signed_in().logout()?; Ok(Done { data: json!({ "ok": true, "done": said }), said }) }

        ("plane", "deploy") => {
            let cloud = w(2);
            let mut d = ready()?;
            let fields = match cloud {
                "aws" => vec![field("region", args.get("region").ok_or("Say where: --region us-east-1 (any AWS region SuperCI lists on its Control plane page).")?)],
                "cloudflare" => {
                    let accounts = d.cloudflare_accounts();
                    let account = match (args.get("account"), accounts) {
                        (Some(a), _) => a.to_string(),
                        (None, [(only, _)]) => only.clone(),
                        (None, []) => return Err("Sign in with Cloudflare first.".into()),
                        (None, several) => return Err(format!("Say which account: --account ID. Yours: {}.", several.iter().map(|(id, name)| format!("{id} ({name})")).collect::<Vec<_>>().join(", "))),
                    };
                    vec![field("account", account)]
                }
                "modal" => vec![],
                _ => return Err("Say where: `superci plane deploy aws --region us-east-1`, `superci plane deploy cloudflare` or `superci plane deploy modal`.".into()),
            };
            d.act(&format!("/plane/{cloud}"), &fields)?;
            Ok(Done::said(d.wait(Background::Deploy, steps)?))
        }
        ("plane", "update") => {
            let mut d = ready()?;
            in_use(&d)?;
            d.act("/plane/update", &[field("plane", args.get("plane").unwrap_or_default())])?;
            Ok(Done::said(d.wait(Background::Update, steps)?))
        }
        ("plane", "move") => {
            let to = Some(w(2)).filter(|t| !t.is_empty()).or(args.get("to")).ok_or("Say where to: `superci plane move ID` (ids are in `superci planes`).")?.to_string();
            let mut d = ready()?;
            d.act("/plane/move", &[field("plane", to)])?;
            Ok(Done::said(d.wait(Background::Move, steps)?))
        }
        ("plane", "allow") => {
            let mut d = ready()?;
            let plane = match args.get("plane") { Some(p) => p.to_string(), None => in_use(&d)?.plane_id().to_string() };
            d.act("/permissions/aws", &[field("plane", plane)])?;
            Ok(Done::said("Its AWS role has what this version asks for."))
        }
        ("plane", "delete") => {
            if w(2).is_empty() { return Err("Say which: `superci plane delete ID --yes` (ids are in `superci planes`).".into()) }
            confirmed(args, "deletes that control plane and what was made for it in its cloud")?;
            let mut d = ready()?;
            Ok(Done::said(d.act("/plane/delete", &[field("plane", w(2))])?))
        }
        ("leave", "") => {
            confirmed(args, "stops SuperCI: GitHub and GitLab stop sending jobs, and every control plane is deleted with what was made for it")?;
            let mut d = ready()?;
            in_use(&d)?;
            Ok(Done::said(d.act("/plane/leave", &[field("confirm", "superci")])?))
        }

        ("runners", "add") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return Err("Say which: `superci runners add aws`, `cloudflare` or `modal`.".into()) }
            let mut d = ready()?;
            let plane = in_use(&d)?;
            // As the Add runners page does: a control plane starts machines in its own cloud itself; elsewhere it is
            // connected (AWS: a role it may assume; Cloudflare, Modal: a small agent there).
            let said = match (cloud, &plane) {
                ("aws", Plane::Aws { .. }) => d.act("/runners/aws-own", &[])?,
                ("aws", _) => d.act("/aws/connect", &[field("region", args.get("region").ok_or("Say where its machines start: --region us-east-1.")?)])?,
                ("modal", Plane::Modal { .. }) => d.act("/runners/modal-own", &[])?,
                ("modal", _) => d.act("/runners/modal", &[])?,
                _ => d.act("/runners/cloudflare", &[])?,
            };
            Ok(Done::said(if said.is_empty() { format!("{cloud} runs jobs now.") } else { said }))
        }
        ("runners", "remove") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return Err("Say which: `superci runners remove aws --yes` (or cloudflare, modal).".into()) }
            confirmed(args, "removes that runner provider and deletes what SuperCI made for it there")?;
            let mut d = ready()?;
            let plane = in_use(&d)?;
            let mut fields = vec![field("plane", plane.plane_id()), field("cloud", cloud)];
            if args.has("only-here") { fields.push(field("only", "on")) }
            if args.has("stop-jobs") { fields.push(field("stop", "on")) }
            Ok(Done::said(d.act("/runners/remove", &fields)?))
        }
        ("runners", "order") => {
            let mut d = signed_in();
            let view = d.current()?.ok_or("There is no control plane yet.")?;
            // The places named, first; any other connected one after them, as it was.
            let mut order: Vec<String> = args.words[2..].to_vec();
            if order.is_empty() { return Err("Say the order: `superci runners order aws aws-on-demand cloudflare`.".into()) }
            for p in view.order() { if !order.contains(&p.cloud) { order.push(p.cloud) } }
            let names: Vec<String> = (0..order.len()).map(|i| format!("cloud_{i}")).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "order")];
            for (name, cloud) in names.iter().zip(&order) { fields.push((name, cloud.clone())) }
            d.ready()?;
            d.act("/routing", &fields)?;
            Ok(Done { said: vec![format!("Order: {}.", order.join(", "))], data: json!({ "ok": true, "order": order }) })
        }
        ("runners", "set") => {
            let cloud = w(2);
            let mut d = signed_in();
            let view = d.current()?.ok_or("There is no control plane yet.")?;
            let order = view.order();
            let pool = order.iter().find(|p| p.cloud == cloud).ok_or_else(|| format!("Say which: {}.", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(", ")))?;
            // The form's fields as they stand, with what was given in their place (the page sends them all).
            let limit = |flag: &str, now: Option<String>| match args.get(flag) { Some("none") => String::new(), Some(v) => v.to_string(), None => now.unwrap_or_default() };
            let regions: Vec<String> = args.get("regions").map(|r| r.split(',').map(|r| r.trim().to_string()).collect()).unwrap_or_default();
            let names: Vec<String> = (0..regions.len()).map(|i| format!("region_{i}")).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "pool"), field("cloud", cloud), field("current", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(",")),
                field("max", limit("max-jobs", pool.max_jobs.map(|n| n.to_string()))), field("usd", limit("monthly-usd", pool.monthly_usd.map(|n| n.to_string())))];
            if args.has("on") || (!args.has("off") && !pool.off) { fields.push(field("on", "on")) }
            for (name, region) in names.iter().zip(&regions) { fields.push((name, region.clone())) }
            if args.has("networks") { fields.push(field("networks", args.all("networks").into_iter().filter(|n| *n != "none").collect::<Vec<_>>().join("\n"))) }
            if let Some(location) = args.get("location") { fields.push(field("location", location)) }
            if let Some(image) = args.get("image") { fields.push(field("image", if image == "none" { "" } else { image })) }
            d.ready()?;
            d.act("/routing", &fields)?;
            runners_of(&mut d)
        }
        ("machine", "") => {
            let mut d = signed_in();
            let now = d.current()?.ok_or("There is no control plane yet.")?.machine();
            let given = ["cpu", "ram", "disk", "arch", "os", "on-demand", "spot"].iter().any(|f| args.has(f));
            if given {
                let number = |flag: &str, now: Option<u32>| args.get(flag).map(str::to_string).or(now.map(|n| n.to_string())).unwrap_or_default();
                let mut fields = vec![field("cpu", number("cpu", now.cpu)), field("ram", number("ram", now.ram_gb)), field("disk", number("disk", now.disk_gb)),
                    field("arch", args.get("arch").map(str::to_string).or(now.arch.clone()).unwrap_or_else(|| "x64".into())),
                    field("os", args.get("os").map(str::to_string).or(now.os.clone()).unwrap_or_else(|| "linux".into()))];
                if args.has("on-demand") || (!args.has("spot") && now.on_demand) { fields.push(field("ondemand", "on")) }
                d.ready()?;
                d.act("/machine", &fields)?;
            }
            let machine = d.current()?.map(|v| v.machine()).unwrap_or_default();
            Ok(Done { said: vec![format!("`runs-on: superci` gets: {}.", machine_words(&serde_json::to_value(&machine).unwrap_or_default()))], data: json!({ "ok": true, "machine": machine }) })
        }
        ("limits", "") => {
            let max = args.get("max-cpu").ok_or("Say the limit: `superci limits --max-cpu 32`.")?;
            let mut d = ready()?;
            in_use(&d)?;
            d.act("/routing", &[field("action", "max_cpu"), field("max_cpu", max)])?;
            Ok(Done::said(format!("A label may ask for up to {max} CPUs.")))
        }
        ("public", action @ ("add" | "remove")) => {
            if w(2).is_empty() { return Err(format!("Say which: `superci public {action} OWNER/REPO`.")) }
            let mut d = ready()?;
            in_use(&d)?;
            d.act("/routing", &[field("action", format!("public_{action}")), field("repo", w(2))])?;
            Ok(Done::said(if action == "add" { format!("{} may run here (never a fork's pull request).", w(2)) } else { format!("{} no longer runs here.", w(2)) }))
        }

        ("github", "remove") => {
            if w(2).is_empty() { return Err("Say which: `superci github remove OWNER --yes`.".into()) }
            confirmed(args, "uninstalls that organization's App: its jobs then wait for runners that never come")?;
            let mut d = ready()?;
            let view = d.current()?.ok_or("There is no control plane yet.")?;
            let apps = view.status.as_ref().and_then(|s| s["apps"].as_array().cloned()).unwrap_or_default();
            let app = apps.iter().find(|a| a["owner"].as_str().is_some_and(|o| o.eq_ignore_ascii_case(w(2)))).ok_or_else(|| format!("{} is not connected here.", w(2)))?;
            Ok(Done::said(d.act("/github/remove", &[field("app", app["id"].as_u64().unwrap_or_default().to_string())])?))
        }
        ("gitlab", "connect") => {
            let token = std::env::var(args.get("token-env").unwrap_or("SUPERCI_GITLAB_TOKEN")).ok().filter(|t| !t.trim().is_empty())
                .ok_or("Give the GitLab token by name: SUPERCI_GITLAB_TOKEN (a project, group or personal access token with the scopes api, create_runner and manage_runner). It goes to your control plane, and is not kept on this machine.")?;
            let mut d = ready()?;
            in_use(&d)?;
            let said = d.act("/gitlab/connect", &[field("url", args.get("url").unwrap_or("https://gitlab.com")), field("token", token), field("which", args.get("gitlab").unwrap_or_default())])?;
            Ok(Done::said(if said.is_empty() { "GitLab is connected. Its projects send jobs here once switched on: `superci gitlab projects`, then `superci gitlab enable PROJECT_ID`.".to_string() } else { said }))
        }
        ("gitlab", "projects") => {
            let mut d = signed_in();
            let view = d.current()?.ok_or("There is no control plane yet.")?;
            let which = args.get("gitlab").unwrap_or_default();
            let v = crate::cloudflare::plane_get(view.plane.url(), d.key(), &format!("/gitlab/projects?g={which}"))?;
            let said = v["projects"].as_array().into_iter().flatten().map(|p| format!("{:>10}  {}  {}", p["id"].as_u64().unwrap_or_default(), if p["enabled"] == true { "on " } else { "off" }, p["path"].as_str().or(p["name"].as_str()).unwrap_or_default())).collect::<Vec<_>>();
            Ok(Done { said: if said.is_empty() { vec!["No projects this token can reach.".into()] } else { said }, data: v })
        }
        ("gitlab", action @ ("enable" | "disable")) => {
            let mut fields = vec![field("gitlab", args.get("gitlab").unwrap_or_default())];
            match (action, args.has("all"), w(2)) {
                ("enable", true, _) => fields.push(field("all", "on")),
                (_, _, "") => return Err(format!("Say which: `superci gitlab {action} PROJECT_ID` (ids are in `superci gitlab projects`).")),
                (_, _, id) => { fields.push(field("id", id)); fields.push(field("enabled", if action == "enable" { "true" } else { "false" })) }
            }
            let mut d = ready()?;
            in_use(&d)?;
            d.act("/gitlab/project", &fields)?;
            Ok(Done::said(if action == "enable" { "Its jobs come here now (tags: superci)." } else { "Its jobs no longer come here." }))
        }
        ("gitlab", "disconnect") => {
            confirmed(args, "disconnects that GitLab: its projects stop sending jobs here and its token is forgotten")?;
            let mut d = ready()?;
            in_use(&d)?;
            d.act("/gitlab/disconnect", &[field("gitlab", w(2))])?;
            Ok(Done::said("GitLab is disconnected."))
        }
        _ => Err(USAGE.into()),
    }
}

/// `superci login [cloud]` and `superci github connect`: the dashboard, opened for the one thing a person does in the
/// browser; it ends when that is done.
pub fn in_browser(args: &Args) -> Option<Result<()>> {
    let open = !args.has("no-browser");
    match (args.word(0), args.word(1)) {
        ("login", cloud) => {
            let cloud: Option<&'static str> = match cloud {
                "" => None,
                c => match CLOUDS.iter().find(|k| **k == c) { Some(k) => Some(*k), None => return Some(Err("Say which: `superci login aws`, `cloudflare` or `modal`.".into())) },
            };
            let d = signed_in();
            let there = match cloud { Some(c) => d.signed_in_as().is_some_and(|s| s.to_lowercase().contains(c)), None => d.signed_in_as().is_some() };
            if there { println!("SuperCI is signed in on this computer: {}. `superci logout` removes it.", d.signed_in_as().unwrap_or_default()); return Some(Ok(())) }
            Some(d.for_task(Task::Login(cloud)).serve(open))
        }
        ("github", "connect") => Some((|| {
            let owner = Some(args.word(2)).filter(|o| !o.is_empty()).ok_or("Say whose repositories: `superci github connect OWNER` (an organization, or your own GitHub name).")?;
            let d = ready()?;
            in_use(&d)?;
            d.for_task(Task::GitHub { login: owner.to_string(), host: args.get("host").unwrap_or_default().to_string(), started: false, done: false }).serve(open)
        })()),
        _ => None,
    }
}

fn status_lines(v: &Value) -> Vec<String> {
    let mut out = vec![format!("Signed in     {}", v["signed_in"].as_str().unwrap_or_default())];
    let p = &v["control_plane"];
    if p.is_null() { out.push("Control plane none found in the clouds signed in to. Set one up: `superci plane deploy aws --region us-east-1` (or cloudflare, or modal).".into()); return out }
    let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
    out.push(format!("Control plane {} · {}{}", s(&p["where"]), p["version"].as_str().unwrap_or("not answering"), p["update_to"].as_str().map(|to| format!(" (update to {to}: `superci plane update`)")).unwrap_or_default()));
    out.push(format!("Address       {}", s(&p["url"])));
    out.push(format!("Jobs can run  {}", if p["ready"] == true { "yes" } else { "not yet: it needs repositories (`superci github connect OWNER`) and a runner provider (`superci runners add aws`)" }));
    let st = &v["status"];
    if st.is_null() { out.push("Its jobs could not be read just now (it did not answer with this machine's key yet). Try again in a moment.".into()); return out }
    let jobs = st["jobs"].as_array().cloned().unwrap_or_default();
    let count = |states: &[&str]| jobs.iter().filter(|j| states.contains(&j["state"].as_str().unwrap_or_default())).count();
    out.push(format!("Jobs          {} listed · {} running · {} starting · {} waiting", jobs.len(), count(&["running"]), count(&["launching", "launched"]), count(&["waiting"])));
    out
}

fn jobs() -> Result<Done> {
    let mut d = signed_in();
    let view = d.current()?.ok_or("There is no control plane yet.")?;
    let jobs = view.jobs();
    let word = |j: &Value| match j["state"].as_str().unwrap_or_default() { "done" => "done", "failed" | "orphan" | "swept" => "failed", "cancelled" => "cancelled", "running" => "running", "waiting" => "waiting", _ => "starting" };
    let said = jobs.iter().take(30).map(|j| {
        let s = |k: &str| j[k].as_str().unwrap_or_default();
        let why = if ["failed", "waiting"].contains(&word(j)) { Some(s("error")).filter(|e| !e.is_empty()).map(|e| format!("  ({})", e.chars().take(120).collect::<String>())).unwrap_or_default() } else { String::new() };
        format!("{:<9} {:<14} {}  {}{why}", word(j), s("cloud"), s("repo"), Some(s("name")).filter(|n| !n.is_empty()).unwrap_or(s("workflow")))
    }).collect::<Vec<_>>();
    Ok(Done { said: if said.is_empty() { vec!["No jobs yet.".into()] } else { said }, data: json!({ "jobs": jobs }) })
}

fn machine_words(m: &Value) -> String {
    let n = |k: &str, unit: &str| m[k].as_u64().map(|v| format!("{v} {unit}"));
    let parts: Vec<String> = [n("cpu", "CPUs"), n("ram_gb", "GB memory"), n("disk_gb", "GB disk"), m["arch"].as_str().map(str::to_string), m["os"].as_str().map(str::to_string), (m["on_demand"] == true).then(|| "on-demand".to_string())].into_iter().flatten().collect();
    if parts.is_empty() { "each provider's standard machine (Linux, x64)".into() } else { parts.join(", ") }
}

fn runners_of(d: &mut Dashboard) -> Result<Done> {
    let view = d.current()?.ok_or("There is no control plane yet.")?;
    let st = view.status.clone().unwrap_or_default();
    let order: Vec<Value> = view.order().into_iter().map(|p| json!({ "cloud": p.cloud, "off": p.off, "max_jobs": p.max_jobs, "monthly_usd": p.monthly_usd,
        "running": st["active"][&p.cloud].as_u64().unwrap_or(0), "spent_usd": st["spend"][&p.cloud].as_f64().unwrap_or(0.0) })).collect();
    let mut said: Vec<String> = order.iter().enumerate().map(|(i, p)| format!("{}. {:<14} {}{}{}", i + 1, p["cloud"].as_str().unwrap_or_default(), if p["off"] == true { "off  " } else { "" },
        p["max_jobs"].as_u64().map(|n| format!("up to {n} jobs at once  ")).unwrap_or_default(), p["monthly_usd"].as_f64().map(|n| format!("up to ${n} a month")).unwrap_or_default()).trim_end().to_string()).collect();
    if said.is_empty() { said.push("No runner provider yet: `superci runners add aws` (or cloudflare, or modal).".into()) }
    let machine = serde_json::to_value(view.machine()).unwrap_or_default();
    said.push(format!("`runs-on: superci` gets: {}.", machine_words(&machine)));
    Ok(Done { said, data: json!({ "order": order, "machine": machine, "aws_regions": view.aws_regions(), "aws_networks": st["aws_networks"], "cloudflare_location": st["cloudflare_location"], "cloudflare_image": st["cloudflare_image"] }) })
}

fn runners() -> Result<Done> { runners_of(&mut signed_in()) }

fn repos() -> Result<Done> {
    let mut d = signed_in();
    let view = d.current()?.ok_or("There is no control plane yet.")?;
    let st = view.status.clone().unwrap_or_default();
    let mut said = vec![];
    for a in st["apps"].as_array().into_iter().flatten() {
        let installed = st["installations"].as_array().into_iter().flatten().filter(|i| i["app"] == a["id"]).map(|i| format!("{} ({})", i["account"].as_str().unwrap_or_default(), i["repositories"].as_str().unwrap_or("selected"))).collect::<Vec<_>>();
        said.push(format!("GitHub  {}  {}", a["owner"].as_str().unwrap_or_default(), if installed.is_empty() { "its App is not installed yet".to_string() } else { format!("installed on {}", installed.join(", ")) }));
    }
    for g in view.gitlabs() { said.push(format!("GitLab  {}{}", g.1, if g.0.is_empty() { String::new() } else { format!("  ({})", g.0) })) }
    if said.is_empty() { said.push("No repositories yet: `superci github connect OWNER`, or `superci gitlab connect`.".into()) }
    let routing = view.routing();
    Ok(Done { said, data: json!({ "github": st["apps"], "installations": st["installations"], "gitlab": st["gitlabs"], "public_repos": routing.public_repos, "max_cpu": routing.max_cpu }) })
}

fn planes() -> Result<Done> {
    let (views, used): (Vec<PlaneView>, usize) = signed_in().all()?;
    let list: Vec<Value> = views.iter().enumerate().map(|(i, v)| { let mut p = plane_json(v); p["in_use"] = (i == used).into(); p }).collect();
    let mut said: Vec<String> = list.iter().map(|p| format!("{}  {:<44} {}{}", p["id"].as_str().unwrap_or_default(), p["where"].as_str().unwrap_or_default(), p["version"].as_str().unwrap_or("not answering"), if p["in_use"] == true { "  in use" } else { "" })).collect();
    if said.is_empty() { said.push("No control plane in the clouds signed in to. Set one up: `superci plane deploy aws --region us-east-1` (or cloudflare, or modal).".into()) }
    Ok(Done { said, data: json!({ "control_planes": list }) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Args { Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).unwrap() }

    #[test]
    fn words_and_flags() {
        let a = args("runners set aws --max-jobs 5 --monthly-usd=200 --networks a --networks b --yes");
        assert_eq!(a.words, ["runners", "set", "aws"]);
        assert!(a.has("yes") && !a.has("json") && a.get("max-jobs") == Some("5") && a.get("monthly-usd") == Some("200") && a.all("networks") == ["a", "b"]);
        let parse = |line: &str| Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).err();
        assert_eq!(parse("status --jsno").unwrap(), "--jsno is not a flag of superci");
        assert_eq!(parse("plane deploy aws --region").unwrap(), "--region needs a value");
        assert!(args("-h").has("help") && args("-V").has("version"));
    }

    #[test]
    fn what_a_person_is_needed_for() {
        assert_eq!(needs(NOT_SIGNED_IN), Some("superci login"));
        assert_eq!(needs(AWS_ENDED), Some("superci login aws"));
        assert_eq!(needs("Sign in with AWS first."), Some("superci login aws"));
        assert_eq!(needs("Sign in with Cloudflare first"), Some("superci login cloudflare"));
        assert_eq!(needs("the Cloudflare sign-in expired: sign in again"), Some("superci login cloudflare"));
        assert_eq!(needs("Sign in with Modal first: Its runners come along with the move."), Some("superci login modal"));
        assert_eq!(needs("3 jobs are running on AWS: Remove it when they finish, or stop them as you remove it."), None);
    }

    /// With SuperCI signed in nowhere, every command says so and asks for the sign-in; a destructive one asks for
    /// --yes before anything else; what is not a command is said to be none.
    #[test]
    fn nothing_runs_unasked_or_signed_out() {
        let dir = std::env::temp_dir().join(format!("superci-commands-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        std::env::set_var("SUPERCI_HOME", &dir);
        let run = |line: &str| run(&args(line), &mut |_| {}).map(|d| d.said).unwrap_err();
        for line in ["status", "jobs", "runners", "repos", "planes", "machine", "plane update", "plane deploy modal", "runners add aws", "runners order aws", "runners set aws --max-jobs 3",
            "limits --max-cpu 8", "public add acme/site", "gitlab projects", "gitlab enable 7", "plane allow", "plane move abcdef123456"] {
            assert_eq!(run(line), NOT_SIGNED_IN, "{line}");
        }
        for line in ["plane delete abcdef123456", "leave", "runners remove aws", "github remove acme", "gitlab disconnect"] {
            assert!(run(line).ends_with("Add --yes to do it."), "{line}");
            assert_eq!(run(&format!("{line} --yes")), NOT_SIGNED_IN, "{line}");
        }
        assert_eq!(run("runners add gcp"), "Say which: `superci runners add aws`, `cloudflare` or `modal`.");
        assert_eq!(run("frobnicate"), USAGE);
        assert_eq!(run("plane"), USAGE);
        assert!(run("gitlab connect").starts_with("Give the GitLab token by name: SUPERCI_GITLAB_TOKEN"));
        assert_eq!(super::run(&args("logout"), &mut |_| {}).unwrap().said, ["Nothing was kept on this machine."]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
