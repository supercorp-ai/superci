//! SuperCI's commands: what the dashboard's pages do, from a terminal or a coding agent. A command that changes
//! something fills in the form its page has and hands it to the dashboard's own code (`Dashboard::act`), so the two
//! cannot differ; one that reads shows what the control plane says. Nothing is ever asked in the terminal: what a
//! person must do first (a sign-in, in the browser) is said, with the command for it, and the status is 3.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::dashboard::{day_of, job_words, plane_json, read_keys, Background, Dashboard, Task, AWS_ENDED, NOT_SIGNED_IN};
use crate::plane::Plane;
use crate::view::{PlaneView, DASHBOARD_VERSION};
use crate::store;

/// What help is made of: a resource by its name, its commands (a resource, an operation, then what it takes), and
/// what more `superci help <resource>` says. The shape is the Stripe CLI's: `superci <resource> <operation> [id]
/// [--param=value]`, the operations `list`, `retrieve`, `create`, `update` and `delete` wherever they fit.
const HELP: &[(&str, &str, &str)] = &[
    ("dashboard", "  superci dashboard                 Opens your dashboard in the browser (on this machine only, while it runs).",
        "Set up or find SuperCI in your cloud, connect repositories and runner providers, and see your jobs. It opens signed in\nonce you have signed in before. Ctrl-C stops it; your runners keep working without it.\n\n  --no-browser    Prints its link without opening it."),
    ("login", "  superci login [CLOUD]             Signs in with a cloud in the browser (aws, cloudflare, modal), and ends.",
        "A person signs in once; SuperCI keeps that sign-in in its own folder (~/.superci) and commands use it from then on.\nWithout a cloud, the first screen asks which. AWS ends a sign-in after twelve hours at most: looking still works\nafter that, and a change in AWS asks for `superci login aws` again.\n\n  --no-browser    Prints the link without opening it."),
    ("logout", "  superci logout                    Removes SuperCI's sign-ins from this machine.",
        "It also asks Cloudflare to end its sign-in, and the control plane to forget this machine's key. Your control plane\nand runners keep working."),
    ("status", "  superci status                    The control plane in use: where, its version, whether jobs can run.",
        "With --json: the control plane, and under \"status\" everything it says of itself (repositories, runner providers,\nlimits, the latest jobs)."),
    ("jobs", "  superci jobs list\n  superci jobs retrieve ID [--bytes=N]",
        "list      The latest jobs: each one's id, how it ended, where it ran, how long, its cost, and why it failed.\nretrieve  One job, with the end of its log: GitHub's (read with your App's token) or GitLab's, as far back as --bytes\n          says (200000 unless given, a million at most). GitHub keeps a job's log once the job has ended."),
    ("planes", "  superci planes list\n  superci planes create aws --region=us-east-1 | cloudflare [--account=ID] | modal\n  superci planes update [ID]\n  superci planes move ID\n  superci planes allow [ID]\n  superci planes delete ID --confirm",
        "list      Every control plane found in the clouds signed in to. One is in use at a time.\ncreate    Puts a control plane in a cloud you are signed in to. Nothing else is needed first.\nupdate    Brings the control plane in use (or the one named) to this program's version.\nmove      Moves to another control plane you created: settings, history and runner providers go along, then\n          GitHub and GitLab are switched over. No job is lost.\nallow     Gives its AWS role what this version asks for (the dashboard's Control plane → Permissions → Allow).\ndelete    Deletes one that is not in use, with what was made for it in its cloud."),
    ("runners", "  superci runners list\n  superci runners create aws [--region=us-east-1] | cloudflare | modal\n  superci runners update CLOUD [--max-jobs=N|none] [--monthly-usd=N|none] [--enabled=true|false]\n                               [--regions=a,b,c] [--networks=\"REGION subnet-… sg-… [private]\"|none]\n                               [--location=enam|weur|auto|…] [--image=https://…|none]\n  superci runners order CLOUD...\n  superci runners delete CLOUD --confirm [--only-here] [--stop-jobs]",
        "list      Runner providers in order, with their limits.\ncreate    Lets a cloud you are signed in to run jobs. --region: where AWS machines start, for a control plane that\n          is not in AWS itself.\nupdate    A provider's limits: jobs at once, dollars a month (`none` lifts one). --enabled: aws-on-demand only.\n          --regions and --networks are AWS's (a network of your own per region; give --networks once per region),\n          --location and --image Cloudflare's.\norder     aws (its spot machines), aws-on-demand, cloudflare, modal. A job goes to the first that can run it and is\n          within its limits; when one cannot start it, to the next.\ndelete    Takes it out and deletes what SuperCI made for it there. --only-here: forgets it without deleting its\n          part (for when you cannot sign in there). --stop-jobs: also when jobs are running on it."),
    ("labels", "  superci labels list\n  superci labels create NAME\n  superci labels delete NAME --confirm",
        "The labels workflows name in `runs-on`: a list, `superci` alone unless you add others. A workflow may name any\nof them (`runs-on: soroci`, `soroci-8cpu`).\n\ncreate    Adds NAME to the list: 2 to 24 lowercase letters and digits. It becomes the one shown in examples.\ndelete    Stops answering to NAME: jobs that still name it wait for a runner that never comes. The last one stays."),
    ("name", "  superci name retrieve\n  superci name update NAME",
        "What your CI is called where people read it: the dashboard's header, the GitHub App's name for an organization\nadded from then on, and what a job's page says when it could not be run. SuperCI unless you name it.\n\nupdate    Calls it NAME (\"SoroCI\", \"Acme CI\"): 2 to 24 letters, digits, spaces and dashes. `SuperCI` puts it back.\n          The command stays `superci`; what workflows name is `superci labels`."),
    ("machine", "  superci machine retrieve\n  superci machine update [--cpu=N] [--ram=GB] [--disk=GB] [--arch=x64|arm64] [--os=linux|windows] [--on-demand=true|false]",
        "The machine `runs-on: superci` alone gets. A job's label can ask for another (`superci-8cpu-arm64`)."),
    ("limits", "  superci limits retrieve\n  superci limits update [--max-cpu=N] [--max-hours=N]",
        "--max-cpu     The largest machine a label may ask for (32 unless set). A label asking for more is refused.\n--max-hours   The longest a job may run: its machine is ended after this (6 unless set, as on GitHub's own runners;\n              120 at most). Whatever is set, a machine on Cloudflare lives 6 hours at most and one on Modal 24. A\n              workflow's timeout-minutes ends a job sooner."),
    ("public_repos", "  superci public_repos list\n  superci public_repos create OWNER/REPO\n  superci public_repos delete OWNER/REPO",
        "Public repositories allowed to run here. Jobs from public repositories are refused unless the repository is allowed.\nEven then, a fork's pull request and `pull_request_target` runs are refused."),
    ("github", "  superci github list\n  superci github create OWNER [--host=https://github.example.com]\n  superci github delete OWNER --confirm",
        "list      The GitHub accounts connected, and where each one's App is installed.\ncreate    Opens GitHub in the browser, where a person creates the App for OWNER (an organization, or your own name)\n          and chooses its repositories. GitHub offers that nowhere else. It ends when the App is installed.\n          --host: a GitHub Enterprise Server, or GitHub Enterprise Cloud with data residency.\ndelete    A further organization: its App is uninstalled and forgotten. The first one goes with `superci leave`."),
    ("github_deliveries", "  superci github_deliveries list\n  superci github_deliveries retrieve ID",
        "What GitHub says of the events it sent your control plane lately: for a job that never got a machine.\n\nlist      The newest hundred: when, what (a job queued, begun, ended), and how it was answered. A job that is in no\n          line was never sent by GitHub; the control plane finds such jobs by asking, within a few minutes.\nretrieve  One of them, with the job it was about, the address it was sent to and what was answered."),
    ("gitlab", "  superci gitlab list\n  superci gitlab create --url=https://gitlab.com [--gitlab=ID|new]\n  superci gitlab delete [ID] --confirm",
        "create    The token is read from SUPERCI_GITLAB_TOKEN (--token-env=NAME for another variable): a project, group or\n          personal access token with the scopes api, create_runner and manage_runner. It goes to your control plane\n          and is not kept on this machine. --gitlab=new: a further connection beside the first.\ndelete    Its projects stop sending jobs here, and its token is forgotten."),
    ("gitlab_projects", "  superci gitlab_projects list [--gitlab=ID]\n  superci gitlab_projects update PROJECT_ID --enabled=true|false [--gitlab=ID]\n  superci gitlab_projects update --all --enabled=true [--gitlab=ID]",
        "list      The projects a GitLab connection's token reaches, with their ids and whether their jobs come here.\nupdate    --enabled=true: the project's jobs with `tags: [superci]` run here from then on."),
    ("keys", "  superci keys list\n  superci keys create NAME [--days=30]\n  superci keys delete NAME",
        "Keys that only read, for a coding agent or a script.\n\ncreate    Makes a key and shows it once. With it and the control plane's address, anything that looks works with no\n          sign-in on that machine, and nothing can be changed:\n            SUPERCI_PLANE=https://… SUPERCI_KEY=superci_read_… superci jobs list\n          Only the key's SHA-256 is kept, in your control plane. It ends by itself (30 days unless --days says\n          otherwise, 366 at most).\ndelete    Ends one at once."),
    ("leave", "  superci leave --confirm           Stops using SuperCI: everything it made in your clouds is deleted.",
        "GitHub and GitLab stop sending jobs, then every control plane is deleted with what was made for it. The GitHub App\nitself is deleted on GitHub; change `runs-on: superci` back in your workflows first."),
];

const FLAGS: &str = "  --param=value   What an operation takes (also `--param value`). An id or a name comes before the flags.
  --json          One JSON object: what was done or read, or \"error\" with \"needs\" (the command a person runs first).
  --dry-run       Checks a change and says what it would do, without doing it.
  --confirm       Needed by what deletes something. Nothing is ever asked in the terminal.

Status: 0 done · 1 failed · 2 not a command, or not a whole one · 3 a person is needed first (see \"needs\").";

/// `superci help`, `superci help RESOURCE`, `superci RESOURCE --help`, and a resource alone.
pub fn help(topic: &str) -> String {
    match HELP.iter().find(|(word, ..)| *word == topic) {
        Some((_, usage, more)) => format!("{usage}\n{}\n{FLAGS}", if more.is_empty() { String::new() } else { format!("\n{more}\n") }),
        None => {
            let group = |words: &[&str]| HELP.iter().filter(|(w, ..)| words.contains(w)).map(|(_, usage, _)| *usage).collect::<Vec<_>>().join("\n");
            format!("SuperCI — GitHub Actions and GitLab CI jobs on your own clouds

  superci <resource> <operation> [id] [--param=value]

{}

Jobs:
{}

Control plane:
{}

Runners:
{}

Repositories:
{}

Coding agents and scripts:
{}

{}

  superci help RESOURCE             More about one (also: superci RESOURCE --help).

{FLAGS}
SuperCI keeps its own sign-ins in ~/.superci (SUPERCI_HOME to put it elsewhere) and reads no other tool's. On a
machine with no browser, a sign-in can be given by name: SUPERCI_CLOUDFLARE_TOKEN, SUPERCI_MODAL_TOKEN_ID and
SUPERCI_MODAL_TOKEN_SECRET.", group(&["dashboard", "login", "logout", "status"]), group(&["jobs"]), group(&["planes"]), group(&["runners", "labels", "name", "machine", "limits"]),
                group(&["github", "github_deliveries", "gitlab", "gitlab_projects", "public_repos"]), group(&["keys"]), group(&["leave"]))
        }
    }
}

/// How a command did not do its thing: it was not a whole command (its resource's help follows), it would delete
/// something and was not told to, or it failed.
#[derive(Debug, PartialEq)]
pub enum Fail { Usage(String), Unconfirmed(String), Failed(String) }

impl From<String> for Fail { fn from(e: String) -> Self { Fail::Failed(e) } }
impl From<&str> for Fail { fn from(e: &str) -> Self { Fail::Failed(e.to_string()) } }

impl Fail {
    pub fn said(&self) -> &str { match self { Fail::Usage(e) | Fail::Unconfirmed(e) | Fail::Failed(e) => e } }
}

type Result<T> = std::result::Result<T, Fail>;

fn usage<T>(say: impl Into<String>) -> Result<T> { Err(Fail::Usage(say.into())) }

/// What was typed: words, and flags with their values (`--region us-east-1`, `--region=us-east-1`; a flag given
/// twice keeps both).
pub struct Args { pub words: Vec<String>, flags: HashMap<String, Vec<String>> }

/// Flags that stand alone; every other flag takes the word after it.
const SWITCHES: [&str; 9] = ["json", "confirm", "dry-run", "no-browser", "all", "only-here", "stop-jobs", "help", "version"];
const VALUES: [&str; 24] = ["enabled", "on-demand", "max-hours", "days", "bytes", "region", "account", "plane", "host", "url", "gitlab", "max-jobs", "monthly-usd", "regions", "networks", "location", "image", "cpu", "ram", "disk", "arch", "os", "max-cpu", "token-env"];

impl Args {
    pub fn parse(raw: &[String]) -> Result<Args> {
        let (mut words, mut flags) = (vec![], HashMap::<String, Vec<String>>::new());
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                if a == "-h" { flags.entry("help".into()).or_default(); } else if a == "-V" { flags.entry("version".into()).or_default(); }
                else if a.starts_with('-') && a.len() > 1 { return usage(format!("{a} is not a flag of superci")) } else { words.push(a.clone()) }
                continue
            };
            let (name, given) = match flag.split_once('=') { Some((n, v)) => (n, Some(v.to_string())), None => (flag, None) };
            if SWITCHES.contains(&name) { flags.entry(name.into()).or_default(); }
            else if VALUES.contains(&name) {
                let value = match given { Some(v) => v, None => match it.next() { Some(v) => v.clone(), None => return usage(format!("--{name} needs a value")) } };
                flags.entry(name.into()).or_default().push(value);
            } else { return usage(format!("--{name} is not a flag of superci")) }
        }
        Ok(Args { words, flags })
    }
    pub fn has(&self, flag: &str) -> bool { self.flags.contains_key(flag) }
    fn get(&self, flag: &str) -> Option<&str> { self.flags.get(flag).and_then(|v| v.last()).map(String::as_str) }
    fn all(&self, flag: &str) -> Vec<&str> { self.flags.get(flag).map(|v| v.iter().map(String::as_str).collect()).unwrap_or_default() }
    pub fn word(&self, i: usize) -> &str { self.words.get(i).map(String::as_str).unwrap_or("") }
}

/// What a command did or read: lines for a person, and the same as data.
pub struct Done { pub said: Vec<String>, pub data: Value }

impl Done {
    fn said(line: impl Into<String>) -> Done { let line = line.into(); let line = if line.is_empty() { "Done.".to_string() } else { line }; Done { data: json!({ "ok": true, "said": line }), said: vec![line] } }
}

/// The command a person runs first, when that is what an error asks for.
pub fn needs(error: &str) -> Option<&'static str> {
    if error == NOT_SIGNED_IN { return Some("superci login") }
    let has = |words: &[&str]| words.iter().any(|w| error.contains(w));
    if error == AWS_ENDED || has(&["Sign in with AWS", "Your AWS sign-in has ended"]) { return Some("superci login aws") }
    if has(&["Sign in with Cloudflare", "Cloudflare sign-in expired"]) { return Some("superci login cloudflare") }
    if has(&["Sign in with Modal"]) { return Some("superci login modal") }
    if has(&["Sign in where this control plane runs", "sign in where it runs", "needs SuperCI's own sign-in"]) { return Some("superci login") }
    None
}

const CLOUDS: [&str; 3] = ["aws", "cloudflare", "modal"];
const NO_PLANE: &str = "There is no control plane yet. Set one up: `superci planes create aws --region=us-east-1` (or cloudflare, or modal).";

const ONLY_READS: &str = "SUPERCI_KEY only reads. A change needs SuperCI's own sign-in on this machine: `superci login`.";

fn signed_in() -> Dashboard { Dashboard::signed_in(store::Store::new()) }

/// A dashboard ready for a change: signed in, its clouds looked in, the control plane in use chosen.
fn ready() -> Result<Dashboard> {
    let mut d = signed_in();
    if d.signed_in_as().is_none() && given_key().is_some() { return Err(ONLY_READS.into()) }
    d.ready()?;
    Ok(d)
}

fn in_use(d: &Dashboard) -> Result<Plane> { Ok(d.plane_in_use().ok_or(NO_PLANE)?) }

/// A dashboard ready to change a setting of the control plane in use (its limits, its order, its keys): as `ready`,
/// without looking through the clouds again when that control plane is known.
fn ready_to_set() -> Result<Dashboard> { let mut d = signed_in(); to_set(&mut d)?; Ok(d) }

/// The same, of a dashboard that has read already. A key that only reads is no sign-in, and is said to be none.
fn to_set(d: &mut Dashboard) -> Result<()> {
    if d.signed_in_as().is_none() && given_key().is_some() { return Err(ONLY_READS.into()) }
    Ok(d.ready_for_settings()?)
}

/// A key that only reads, given by name with its control plane's address (SUPERCI_PLANE, SUPERCI_KEY): what a coding
/// agent or a script is handed in place of the sign-ins.
fn given_key() -> Option<(String, String)> {
    let var = |n: &str| std::env::var(n).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Some((var("SUPERCI_PLANE")?.trim_end_matches('/').to_string(), var("SUPERCI_KEY")?))
}

/// What a command that looks reads: the control plane, and the key it is read with. With a key given by name: that
/// control plane, with no sign-in. Else the one in use, with this machine's key.
fn look(d: &mut Dashboard) -> Result<(PlaneView, String)> {
    let Some((url, key)) = given_key() else { let v = d.current()?.ok_or(NO_PLANE)?; return Ok((v, d.key().to_string())) };
    if !url.starts_with("https://") { return usage("SUPERCI_PLANE is the control plane's address, starting with https:// (`superci status` says it).") }
    let health = crate::cloudflare::health(&url).ok_or("The control plane at SUPERCI_PLANE did not answer.")?;
    let v = crate::view::plane_view(&Plane::Seen { url, plane_id: health["plane"].as_str().unwrap_or_default().to_string() }, Some(&key));
    if v.status.is_none() { return Err("The control plane did not take SUPERCI_KEY: it has ended, was revoked, or is another control plane's (keys that only read need control plane 0.11.0 or newer).".into()) }
    Ok((v, key))
}

fn view(d: &mut Dashboard) -> Result<PlaneView> { Ok(look(d)?.0) }

/// What deletes something needs --confirm (said with what it would do); --dry-run needs none.
fn confirmed(args: &Args, what: &str) -> Result<()> { if args.has("confirm") || args.has("dry-run") { Ok(()) } else { Err(Fail::Unconfirmed(format!("This would {what}. Add --confirm to do it, or --dry-run to check it first."))) } }

/// A flag that is true or false (`--enabled=true`). None: not given.
fn truth(args: &Args, flag: &str) -> Result<Option<bool>> {
    match args.get(flag) { None => Ok(None), Some("true") => Ok(Some(true)), Some("false") => Ok(Some(false)), Some(other) => usage(format!("--{flag} is true or false, not {other}.")) }
}

fn field(name: &'static str, value: impl Into<String>) -> (&'static str, String) { (name, value.into()) }

/// With --dry-run: what the command would do, after everything that can be checked without changing anything was
/// (the flags, the sign-in, the control plane). The form it would send is in the data, without any token.
fn dry(args: &Args, would: &str, path: &str, fields: &[(&str, String)]) -> Option<Done> {
    if !args.has("dry-run") { return None }
    let form: serde_json::Map<String, Value> = fields.iter().map(|(k, v)| (k.to_string(), if *k == "token" { "(given)".into() } else { v.clone().into() })).collect();
    Some(Done { said: vec![format!("Would {would}. Nothing was changed.")], data: json!({ "ok": true, "dry_run": true, "would": would, "form": { "to": path, "fields": form } }) })
}

/// Does what the page's form does, or with --dry-run returns from the command saying what it would do.
macro_rules! apply {
    ($d:expr, $args:expr, $would:expr, $path:expr, $fields:expr) => {{
        let (would, path, fields): (String, &str, &[(&str, String)]) = ($would.into(), $path, $fields);
        if let Some(done) = dry($args, &would, path, fields) { return Ok(done) }
        $d.act(path, fields)?
    }};
}

/// Runs a command that is not the dashboard itself. `steps` is told each step of a long one as it begins.
pub fn run(args: &Args, steps: &mut dyn FnMut(&str)) -> Result<Done> {
    let w = |i: usize| args.word(i);
    match (w(0), w(1)) {
        ("status", "") => {
            let mut d = signed_in();
            let (signed_in, view) = match given_key() {
                Some(_) => ("with a key that only reads (SUPERCI_KEY)".to_string(), Some(look(&mut d)?.0)),
                None => { let v = d.current()?; (d.signed_in_as().unwrap_or_else(|| "nowhere now (AWS's sign-in has ended; this is read with this machine's key)".into()), v) }
            };
            let v = match view { Some(v) => json!({ "signed_in": signed_in, "control_plane": plane_json(&v), "status": v.status }), None => json!({ "signed_in": signed_in, "control_plane": null }) };
            Ok(Done { said: status_lines(&v), data: v })
        }
        ("jobs", "list") => jobs(),
        ("jobs", "retrieve") => {
            let id = w(2);
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return usage("Say which: `superci jobs retrieve ID` (ids are in `superci jobs list`).") }
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            let j = v.jobs().into_iter().find(|j| j["job_id"].as_u64().is_some_and(|n| n.to_string() == id)).ok_or("That job is not among the latest this control plane keeps (ids are in `superci jobs list`).")?;
            let words = job_words(&j);
            let mut path = format!("/job/log?id={id}&bytes={}", args.get("bytes").unwrap_or("200000"));
            if j["provider"] == "gitlab" { path += &format!("&gl={}", j["gitlab"].as_str().unwrap_or_default()) }
            let got = crate::cloudflare::plane_get(v.plane.url(), &key, &path)
                .map_err(|e| if e.contains("404") { "Reading a job's log needs control plane 0.11.0 or newer: `superci planes update`.".to_string() } else { e })?;
            let mut said = vec![job_line(&words), words["link"].as_str().unwrap_or_default().to_string()];
            match got["log"].as_str() {
                Some(log) => { said.push(if got["truncated"] == true { format!("The end of its log ({} bytes in all):", got["bytes"]) } else { "Its log:".to_string() }); said.push(log.trim_end().to_string()) }
                None => said.push(format!("No log: {}.", got["error"].as_str().unwrap_or("the control plane gave none").trim_end_matches('.'))),
            }
            Ok(Done { said, data: json!({ "job": words, "log": got["log"], "truncated": got["truncated"], "log_error": got["error"] }) })
        }
        ("keys", "list") => {
            let mut d = signed_in();
            let v = d.current()?.ok_or(NO_PLANE)?;
            let keys: Vec<Value> = read_keys(&v.status.clone().unwrap_or_default()).into_iter().map(|(name, until)| json!({ "name": name.to_lowercase(), "until": day_of(until / 1000) })).collect();
            let said = if keys.is_empty() { vec!["No keys that only read. Make one for a coding agent or a script: `superci keys create agent`.".to_string()] }
                else { keys.iter().map(|k| format!("{:<24} reads until {}", k["name"].as_str().unwrap_or_default(), k["until"].as_str().unwrap_or_default())).collect() };
            Ok(Done { said, data: json!({ "keys": keys }) })
        }
        ("keys", "create") => {
            if w(2).is_empty() { return usage("Give it a name: `superci keys create agent`.") }
            let Ok(days) = args.get("days").unwrap_or("30").parse::<u64>() else { return usage("--days is a number of days (30 unless given).") };
            let mut d = ready_to_set()?;
            let plane = in_use(&d)?;
            if let Some(done) = dry(args, &format!("make a key named {} that only reads, good for {days} days", w(2)), "keys", &[]) { return Ok(done) }
            let (key, until) = d.create_key(w(2), days)?;
            Ok(Done { said: vec![format!("A key that only reads, until {}. It is shown this once:", day_of(until)), String::new(), format!("  SUPERCI_PLANE={} SUPERCI_KEY={key}", plane.url()), String::new(),
                "With these two set, whatever looks works with no sign-in (`superci status`, `superci jobs list`, `superci runners list`…), and nothing can be changed.".to_string()],
                data: json!({ "ok": true, "name": w(2).to_lowercase(), "until": day_of(until), "env": { "SUPERCI_PLANE": plane.url(), "SUPERCI_KEY": key } }) })
        }
        ("keys", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci keys delete NAME` (names are in `superci keys list`).") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            if let Some(done) = dry(args, &format!("end the key named {}", w(2)), "keys", &[]) { return Ok(done) }
            d.revoke_key(w(2))?;
            Ok(Done::said(format!("The key {} no longer reads.", w(2).to_lowercase())))
        }
        ("runners", "list") => runners_of(&mut signed_in()),
        ("github", "list") => github(),
        ("github_deliveries", operation @ ("list" | "retrieve")) => {
            let id = w(2);
            if operation == "retrieve" && (id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit())) { return usage("Say which: `superci github_deliveries retrieve ID` (ids are in `superci github_deliveries list`).") }
            let mut d = signed_in();
            let (v, key) = look(&mut d)?;
            let got = crate::cloudflare::plane_get(v.plane.url(), &key, &if operation == "retrieve" { format!("/github/deliveries?id={id}") } else { "/github/deliveries".to_string() })
                .map_err(|e| if e.contains("404") { "This needs control plane 0.11.2 or newer: `superci planes update`.".to_string() } else { e })?;
            let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
            let mut said = vec![];
            for app in got.as_array().into_iter().flatten() {
                let list = &app["deliveries"];
                if let Some(e) = list["error"].as_str() { said.push(format!("{}: GitHub would not say ({e})", s(&app["app"]))); continue }
                if operation == "retrieve" {
                    said.push(format!("{} {} {} · answered {} {} · job {} {} {}", s(&list["delivered_at"]), s(&list["event"]), s(&list["action"]), list["status_code"], s(&list["status"]), list["job"], s(&list["job_name"]), list["labels"]));
                    said.push(format!("sent to {}", s(&list["url"])));
                    if let Some(a) = list["answer"].as_str().filter(|a| !a.is_empty()) { said.push(format!("answer: {a}")) }
                } else {
                    for x in list.as_array().into_iter().flatten() {
                        said.push(format!("{}  {:<13} {:<12} {} {}{}  {}", s(&x["delivered_at"]).replace('T', " ").chars().take(19).collect::<String>(), s(&x["event"]), s(&x["action"]), x["status_code"], s(&x["status"]).chars().take(60).collect::<String>(), if x["redelivery"] == true { "  (sent again)" } else { "" }, x["id"]));
                    }
                }
            }
            if said.is_empty() { said.push("GitHub is not connected, or it has sent nothing lately.".into()) }
            Ok(Done { said, data: got })
        }
        ("gitlab", "list") => gitlab(),
        ("planes", "list") => planes(),
        ("logout", "") => { let said = signed_in().logout()?; Ok(Done { data: json!({ "ok": true, "done": said }), said }) }

        ("planes", "create") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return usage("Say where: `superci planes create aws --region=us-east-1`, `superci planes create cloudflare` or `superci planes create modal`.") }
            let region = match (cloud, args.get("region")) { ("aws", None) => return usage("Say where: --region=us-east-1 (any AWS region the dashboard's Control plane page lists)."), (_, r) => r.unwrap_or_default().to_string() };
            let mut d = ready()?;
            let (fields, place) = match cloud {
                "aws" => (vec![field("region", region.clone())], format!("AWS, in {region}")),
                "cloudflare" => {
                    let accounts = d.cloudflare_accounts();
                    let account = match (args.get("account"), accounts) {
                        (Some(a), _) => a.to_string(),
                        (None, [(only, _)]) => only.clone(),
                        (None, []) => return Err("Sign in with Cloudflare first.".into()),
                        (None, several) => return usage(format!("Say which account: --account=ID. Yours: {}.", several.iter().map(|(id, name)| format!("{id} ({name})")).collect::<Vec<_>>().join(", "))),
                    };
                    (vec![field("account", account.clone())], format!("Cloudflare, in account {account}"))
                }
                _ => (vec![], "Modal".to_string()),
            };
            apply!(d, args, format!("deploy a control plane to {place}"), &format!("/plane/{cloud}"), &fields);
            Ok(Done::said(d.wait(Background::Deploy, steps)?))
        }
        ("planes", "update") => {
            let mut d = ready()?;
            let plane = in_use(&d)?;
            apply!(d, args, format!("update the control plane {} to {DASHBOARD_VERSION}", if w(2).is_empty() { format!("in use ({})", plane.place()) } else { w(2).to_string() }), "/plane/update", &[field("plane", w(2))]);
            Ok(Done::said(d.wait(Background::Update, steps)?))
        }
        ("planes", "move") => {
            if w(2).is_empty() { return usage("Say where to: `superci planes move ID` (ids are in `superci planes list`).") }
            let mut d = ready()?;
            apply!(d, args, format!("move to the control plane {}: settings, history and runner providers go along, then GitHub and GitLab are switched over", w(2)), "/plane/move", &[field("plane", w(2))]);
            Ok(Done::said(d.wait(Background::Move, steps)?))
        }
        ("planes", "allow") => {
            let mut d = ready()?;
            let plane = if w(2).is_empty() { in_use(&d)?.plane_id().to_string() } else { w(2).to_string() };
            apply!(d, args, format!("give the AWS role of the control plane {plane} what this version asks for"), "/permissions/aws", &[field("plane", plane.clone())]);
            Ok(Done::said("Its AWS role has what this version asks for."))
        }
        ("planes", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci planes delete ID --confirm` (ids are in `superci planes list`).") }
            let would = format!("delete the control plane {} and what was made for it in its cloud", w(2));
            confirmed(args, &would)?;
            let mut d = ready()?;
            Ok(Done::said(apply!(d, args, would, "/plane/delete", &[field("plane", w(2))])))
        }
        ("leave", "") => {
            let would = "stop SuperCI: GitHub and GitLab stop sending jobs, and every control plane is deleted with what was made for it";
            confirmed(args, would)?;
            let mut d = ready()?;
            in_use(&d)?;
            Ok(Done::said(apply!(d, args, would, "/plane/leave", &[field("confirm", "superci")])))
        }

        ("runners", "create") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return usage("Say which: `superci runners create aws`, `cloudflare` or `modal`.") }
            let mut d = ready()?;
            let plane = in_use(&d)?;
            // As the Add runners page does: a control plane starts machines in its own cloud itself; elsewhere it is
            // connected (AWS: a role it may assume; Cloudflare, Modal: a small agent there).
            let (path, fields) = match (cloud, &plane) {
                ("aws", Plane::Aws { .. }) => ("/runners/aws-own", vec![]),
                ("aws", _) => match args.get("region") { Some(r) => ("/aws/connect", vec![field("region", r)]), None => return usage("Say where its machines start: `superci runners create aws --region=us-east-1`.") },
                ("modal", Plane::Modal { .. }) => ("/runners/modal-own", vec![]),
                ("modal", _) => ("/runners/modal", vec![]),
                _ => ("/runners/cloudflare", vec![]),
            };
            let said = apply!(d, args, format!("let {cloud} run jobs for the control plane in use ({})", plane.place()), path, &fields);
            Ok(Done::said(if said.is_empty() { format!("{cloud} runs jobs now.") } else { said }))
        }
        ("runners", "delete") => {
            let cloud = w(2);
            if !CLOUDS.contains(&cloud) { return usage("Say which: `superci runners delete aws --confirm` (or cloudflare, modal).") }
            let would = format!("remove {cloud} as a runner provider{}", if args.has("only-here") { ", leaving what SuperCI made for it there" } else { " and delete what SuperCI made for it there" });
            confirmed(args, &would)?;
            let mut d = ready()?;
            let plane = in_use(&d)?;
            let mut fields = vec![field("plane", plane.plane_id()), field("cloud", cloud)];
            if args.has("only-here") { fields.push(field("only", "on")) }
            if args.has("stop-jobs") { fields.push(field("stop", "on")) }
            Ok(Done::said(apply!(d, args, would, "/runners/remove", &fields)))
        }
        ("runners", "order") => {
            if args.words.len() < 3 { return usage("Say the order: `superci runners order aws aws-on-demand cloudflare`.") }
            let mut d = signed_in();
            let now = view(&mut d)?.order();
            // The places named, first; any other connected one after them, as it was.
            let mut order: Vec<String> = args.words[2..].to_vec();
            if let Some(unknown) = order.iter().find(|c| !now.iter().any(|p| p.cloud == **c)) { return usage(format!("{unknown} is not one of your runner providers ({}).", now.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(", "))) }
            for p in now { if !order.contains(&p.cloud) { order.push(p.cloud) } }
            let names: Vec<String> = (0..order.len()).map(|i| format!("cloud_{i}")).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "order")];
            for (name, cloud) in names.iter().zip(&order) { fields.push((name, cloud.clone())) }
            to_set(&mut d)?;
            apply!(d, args, format!("set the order to {}", order.join(", ")), "/routing", &fields);
            Ok(Done { said: vec![format!("Order: {}.", order.join(", "))], data: json!({ "ok": true, "order": order }) })
        }
        ("runners", "update") => {
            let cloud = w(2);
            let mut d = signed_in();
            let order = view(&mut d)?.order();
            let Some(pool) = order.iter().find(|p| p.cloud == cloud) else { return usage(format!("Say which: `superci runners update CLOUD …`, one of {}.", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(", "))) };
            // The form's fields as they stand, with what was given in their place (the page sends them all).
            let limit = |flag: &str, now: Option<String>| match args.get(flag) { Some("none") => String::new(), Some(v) => v.to_string(), None => now.unwrap_or_default() };
            let regions: Vec<String> = args.get("regions").map(|r| r.split(',').map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect()).unwrap_or_default();
            let names: Vec<String> = (0..regions.len()).map(|i| format!("region_{i}")).collect();
            let mut fields: Vec<(&str, String)> = vec![field("action", "pool"), field("cloud", cloud), field("current", order.iter().map(|p| p.cloud.clone()).collect::<Vec<_>>().join(",")),
                field("max", limit("max-jobs", pool.max_jobs.map(|n| n.to_string()))), field("usd", limit("monthly-usd", pool.monthly_usd.map(|n| n.to_string())))];
            if truth(args, "enabled")?.unwrap_or(!pool.off) { fields.push(field("on", "on")) }
            for (name, region) in names.iter().zip(&regions) { fields.push((name, region.clone())) }
            if args.has("networks") { fields.push(field("networks", args.all("networks").into_iter().filter(|n| *n != "none").collect::<Vec<_>>().join("\n"))) }
            if let Some(location) = args.get("location") { fields.push(field("location", location)) }
            if let Some(image) = args.get("image") { fields.push(field("image", if image == "none" { "" } else { image })) }
            let given: Vec<String> = ["max-jobs", "monthly-usd", "enabled", "regions", "networks", "location", "image"].iter().filter(|f| args.has(f)).map(|f| format!("--{f}={}", args.get(f).unwrap_or_default())).collect();
            if given.is_empty() { return usage(format!("Say what to change: `superci runners update {cloud} --max-jobs=20` (see `superci help runners`).")) }
            to_set(&mut d)?;
            apply!(d, args, format!("update {cloud}: {}", given.join(" ")), "/routing", &fields);
            runners_of(&mut d)
        }
        ("labels", "list") => {
            let labels = view(&mut signed_in())?.labels();
            Ok(Done { said: labels.iter().enumerate().map(|(i, l)| if i == 0 && labels.len() > 1 { format!("{l}  (shown in examples)") } else { l.clone() }).collect(), data: json!({ "labels": labels }) })
        }
        ("labels", operation @ ("create" | "delete")) => {
            if w(2).is_empty() { return usage(format!("Say which: `superci labels {operation} NAME`.")) }
            let would = if operation == "create" { format!("add {} to the labels workflows may name; the ones before keep working", w(2)) } else { format!("stop answering to the label {}: jobs that still name it would wait for a runner that never comes", w(2)) };
            if operation == "delete" { confirmed(args, &would)? }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, would, "/labels", &[field("action", if operation == "create" { "add" } else { "remove" }), field("label", w(2))]);
            let labels = view(&mut d)?.labels();
            Ok(Done { said: vec![format!("It answers to: {}.", labels.join(", ")), format!("In a workflow: `runs-on: {}`.", labels.first().cloned().unwrap_or_default())], data: json!({ "ok": true, "labels": labels }) })
        }
        ("name", "retrieve") => {
            let name = view(&mut signed_in())?.name();
            Ok(Done { said: vec![name.clone()], data: json!({ "name": name }) })
        }
        ("name", "update") => {
            if w(2).is_empty() { return usage("Say what to call it: `superci name update SoroCI`.") }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, format!("call it {}", w(2)), "/name", &[field("name", w(2))]);
            let name = view(&mut d)?.name();
            Ok(Done { said: vec![format!("It is called {name}.")], data: json!({ "ok": true, "name": name }) })
        }
        ("machine", operation @ ("retrieve" | "update")) => {
            let mut d = signed_in();
            let now = view(&mut d)?.machine();
            if operation == "update" {
                if !["cpu", "ram", "disk", "arch", "os", "on-demand"].iter().any(|f| args.has(f)) { return usage("Say what to change: `superci machine update --cpu=4 --ram=16` (see `superci help machine`).") }
                let number = |flag: &str, now: Option<u32>| args.get(flag).map(str::to_string).or(now.map(|n| n.to_string())).unwrap_or_default();
                let mut fields = vec![field("cpu", number("cpu", now.cpu)), field("ram", number("ram", now.ram_gb)), field("disk", number("disk", now.disk_gb)),
                    field("arch", args.get("arch").map(str::to_string).or(now.arch.clone()).unwrap_or_else(|| "x64".into())),
                    field("os", args.get("os").map(str::to_string).or(now.os.clone()).unwrap_or_else(|| "linux".into()))];
                if truth(args, "on-demand")?.unwrap_or(now.on_demand) { fields.push(field("ondemand", "on")) }
                to_set(&mut d)?;
                let would = fields.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
                apply!(d, args, format!("give `runs-on: superci` this machine: {would}"), "/machine", &fields);
            }
            let machine = view(&mut d)?.machine();
            Ok(Done { said: vec![format!("`runs-on: superci` gets: {}.", machine_words(&serde_json::to_value(&machine).unwrap_or_default()))], data: json!({ "ok": true, "machine": machine }) })
        }
        ("limits", operation @ ("retrieve" | "update")) => {
            let mut d = signed_in();
            if operation == "update" {
                if !args.has("max-cpu") && !args.has("max-hours") { return usage("Say what to change: `superci limits update --max-hours=12` (see `superci help limits`).") }
                to_set(&mut d)?;
                in_use(&d)?;
                if let Some(max) = args.get("max-cpu") { apply!(d, args, format!("let a label ask for up to {max} CPUs"), "/routing", &[field("action", "max_cpu"), field("max_cpu", max)]); }
                if let Some(hours) = args.get("max-hours") { apply!(d, args, format!("let a job run for up to {hours} hours"), "/routing", &[field("action", "max_hours"), field("max_hours", hours)]); }
            }
            let routing = view(&mut d)?.routing();
            let (cpu, hours) = (routing.max_cpu.unwrap_or(superci_core::plane::MAX_CPU), routing.max_minutes.unwrap_or(superci_core::plane::MAX_JOB_MINUTES) / 60);
            Ok(Done { said: vec![format!("A label may ask for up to {cpu} CPUs."), format!("A job may run for up to {hours} hours (6 at most on Cloudflare, 24 on Modal).")], data: json!({ "ok": true, "max_cpu": cpu, "max_hours": hours }) })
        }
        ("public_repos", "list") => {
            let allowed = view(&mut signed_in())?.routing().public_repos;
            Ok(Done { said: if allowed.is_empty() { vec!["No public repository is allowed to run here. Allow one: `superci public_repos create OWNER/REPO`.".into()] } else { allowed.clone() }, data: json!({ "public_repos": allowed }) })
        }
        ("public_repos", operation @ ("create" | "delete")) => {
            let action = if operation == "create" { "add" } else { "remove" };
            if w(2).is_empty() { return usage(format!("Say which: `superci public_repos {operation} OWNER/REPO`.")) }
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, if action == "add" { format!("let the public repository {} run here", w(2)) } else { format!("stop the public repository {} from running here", w(2)) }, "/routing", &[field("action", format!("public_{action}")), field("repo", w(2))]);
            Ok(Done::said(if action == "add" { format!("{} may run here (never a fork's pull request).", w(2)) } else { format!("{} no longer runs here.", w(2)) }))
        }

        ("github", "delete") => {
            if w(2).is_empty() { return usage("Say which: `superci github delete OWNER --confirm`.") }
            let would = format!("uninstall the GitHub App of {}: its jobs then wait for runners that never come", w(2));
            confirmed(args, &would)?;
            let mut d = ready()?;
            let apps = view(&mut d)?.status.as_ref().and_then(|s| s["apps"].as_array().cloned()).unwrap_or_default();
            let app = apps.iter().find(|a| a["owner"].as_str().is_some_and(|o| o.eq_ignore_ascii_case(w(2)))).ok_or_else(|| format!("{} is not connected here.", w(2)))?;
            Ok(Done::said(apply!(d, args, would, "/github/remove", &[field("app", app["id"].as_u64().unwrap_or_default().to_string())])))
        }
        ("gitlab", "create") => {
            let name = args.get("token-env").unwrap_or("SUPERCI_GITLAB_TOKEN");
            let Some(token) = std::env::var(name).ok().filter(|t| !t.trim().is_empty()) else {
                return usage(format!("Give the GitLab token by name: {name} (a project, group or personal access token with the scopes api, create_runner and manage_runner). It goes to your control plane, and is not kept on this machine."))
            };
            let url = args.get("url").unwrap_or("https://gitlab.com");
            let mut d = ready()?;
            in_use(&d)?;
            let said = apply!(d, args, format!("connect {url} with the token in {name}"), "/gitlab/connect", &[field("url", url), field("token", token), field("which", args.get("gitlab").unwrap_or_default())]);
            Ok(Done::said(if said.is_empty() { "GitLab is connected. Its projects send jobs here once switched on: `superci gitlab_projects list`, then `superci gitlab_projects update PROJECT_ID --enabled=true`.".to_string() } else { said }))
        }
        ("gitlab_projects", "list") => {
            let mut d = signed_in();
            let (view, key) = look(&mut d)?;
            let v = crate::cloudflare::plane_get(view.plane.url(), &key, &format!("/gitlab/projects?g={}", args.get("gitlab").unwrap_or_default()))?;
            let said = v["projects"].as_array().into_iter().flatten().map(|p| format!("{:>10}  {}  {}", p["id"].as_u64().unwrap_or_default(), if p["enabled"] == true { "on " } else { "off" }, p["path"].as_str().unwrap_or_default())).collect::<Vec<_>>();
            Ok(Done { said: if said.is_empty() { vec!["No projects this token can reach.".into()] } else { said }, data: v })
        }
        ("gitlab_projects", "update") => {
            let Some(enabled) = truth(args, "enabled")? else { return usage("Say which way: `superci gitlab_projects update PROJECT_ID --enabled=true` (or false).") };
            let mut fields = vec![field("gitlab", args.get("gitlab").unwrap_or_default())];
            let would = match (enabled, args.has("all"), w(2)) {
                (true, true, _) => { fields.push(field("all", "on")); "send every project's jobs here".to_string() }
                (false, true, _) => return usage("Projects are switched off one at a time: `superci gitlab_projects update PROJECT_ID --enabled=false`."),
                (_, _, "") => return usage("Say which: `superci gitlab_projects update PROJECT_ID --enabled=true` (ids are in `superci gitlab_projects list`)."),
                (_, _, id) => { fields.push(field("id", id)); fields.push(field("enabled", if enabled { "true" } else { "false" })); format!("{} project {id}'s jobs here", if enabled { "send" } else { "stop sending" }) }
            };
            let mut d = ready_to_set()?;
            in_use(&d)?;
            apply!(d, args, would, "/gitlab/project", &fields);
            Ok(Done::said(if enabled { "Its jobs come here now (tags: superci)." } else { "Its jobs no longer come here." }))
        }
        ("gitlab", "delete") => {
            let would = "disconnect that GitLab: its projects stop sending jobs here and its token is forgotten";
            confirmed(args, would)?;
            let mut d = ready()?;
            in_use(&d)?;
            apply!(d, args, would, "/gitlab/disconnect", &[field("gitlab", w(2))]);
            Ok(Done::said("GitLab is disconnected."))
        }
        (word, operation) => usage(match HELP.iter().find(|(w, ..)| *w == word) {
            Some(_) if operation.is_empty() => format!("`superci {word}` takes an operation."),
            Some(_) => format!("`{operation}` is not an operation of `superci {word}`."),
            None if word.is_empty() => String::new(),
            None => format!("`{word}` is not a command of superci."),
        }),
    }
}

/// `superci dashboard`, `superci login [cloud]` and `superci github create`: the dashboard, or the dashboard opened
/// for the one thing a person does in the browser (it ends when that is done). None: not one of these.
pub fn in_browser(args: &Args) -> Option<Result<()>> {
    let open = !args.has("no-browser");
    match (args.word(0), args.word(1)) {
        ("dashboard", "") => Some(signed_in().serve(open).map_err(Fail::from)),
        ("login", cloud) => {
            let cloud: Option<&'static str> = match cloud {
                "" => None,
                c => match CLOUDS.iter().find(|k| **k == c) { Some(k) => Some(*k), None => return Some(usage("Say which: `superci login aws`, `cloudflare` or `modal`.")) },
            };
            let d = signed_in();
            let there = match cloud { Some(c) => d.signed_in_as().is_some_and(|s| s.to_lowercase().contains(c)), None => d.signed_in_as().is_some() };
            if there { println!("SuperCI is signed in on this computer: {}. `superci logout` removes it.", d.signed_in_as().unwrap_or_default()); return Some(Ok(())) }
            Some(d.for_task(Task::Login(cloud)).serve(open).map_err(Fail::from))
        }
        ("github", "create") => Some((|| {
            let owner = args.word(2);
            if owner.is_empty() { return usage("Say whose repositories: `superci github create OWNER` (an organization, or your own GitHub name).") }
            let d = ready()?;
            in_use(&d)?;
            if args.has("dry-run") { println!("Would open GitHub in the browser, for a person to create the App for {owner} and choose its repositories. Nothing was changed."); return Ok(()) }
            Ok(d.for_task(Task::GitHub { login: owner.to_string(), host: args.get("host").unwrap_or_default().to_string(), started: false, done: false }).serve(open)?)
        })()),
        _ => None,
    }
}

fn status_lines(v: &Value) -> Vec<String> {
    let mut out = vec![format!("Signed in     {}", v["signed_in"].as_str().unwrap_or_default())];
    let p = &v["control_plane"];
    if p.is_null() { out.push(format!("Control plane none found in the clouds signed in to. {}", NO_PLANE.trim_start_matches("There is no control plane yet. "))); return out }
    let s = |x: &Value| x.as_str().unwrap_or_default().to_string();
    out.push(format!("Control plane {} · {}{}", s(&p["where"]), p["version"].as_str().unwrap_or("not answering"), p["update_to"].as_str().map(|to| format!(" (update to {to}: `superci planes update`)")).unwrap_or_default()));
    out.push(format!("Address       {}", s(&p["url"])));
    out.push(format!("Jobs can run  {}", if p["ready"] == true { "yes" } else { "not yet: it needs repositories (`superci github create OWNER`) and a runner provider (`superci runners create aws`)" }));
    let st = &v["status"];
    if st.is_null() { out.push("Its jobs could not be read just now (it did not answer with this machine's key yet). Try again in a moment.".into()); return out }
    let jobs = st["jobs"].as_array().cloned().unwrap_or_default();
    let count = |states: &[&str]| jobs.iter().filter(|j| states.contains(&j["state"].as_str().unwrap_or_default())).count();
    out.push(format!("Jobs          {} listed · {} running · {} starting · {} waiting", jobs.len(), count(&["running"]), count(&["launching", "launched"]), count(&["waiting"])));
    out
}

fn jobs() -> Result<Done> {
    let mut d = signed_in();
    let jobs: Vec<Value> = view(&mut d)?.jobs().iter().map(job_words).collect();
    let said = jobs.iter().take(40).map(job_line).collect::<Vec<_>>();
    Ok(Done { said: if said.is_empty() { vec!["No jobs yet.".into()] } else { said }, data: json!({ "jobs": jobs }) })
}

/// A job in a line: its id, how it stands, what it is, and (its runner, how long, its cost, why it failed or waits).
fn job_line(j: &Value) -> String {
    let s = |k: &str| j[k].as_str().unwrap_or_default().to_string();
    let tail: Vec<String> = [s("runner"), s("took"), j["usd"].as_f64().map(|c| format!("${c:.3}")).unwrap_or_default(), s("why")].into_iter().filter(|t| !t.is_empty()).collect();
    format!("{:<12} {:<9} {} · {}{}", j["id"].as_u64().unwrap_or_default(), s("state"), s("job"), s("in"), if tail.is_empty() { String::new() } else { format!("  ({})", tail.join(" · ")) })
}

fn machine_words(m: &Value) -> String {
    let n = |k: &str, unit: &str| m[k].as_u64().map(|v| format!("{v} {unit}"));
    let parts: Vec<String> = [n("cpu", "CPUs"), n("ram_gb", "GB memory"), n("disk_gb", "GB disk"), m["arch"].as_str().map(str::to_string), m["os"].as_str().map(str::to_string), (m["on_demand"] == true).then(|| "on-demand".to_string())].into_iter().flatten().collect();
    if parts.is_empty() { "each provider's standard machine (Linux, x64)".into() } else { parts.join(", ") }
}

fn runners_of(d: &mut Dashboard) -> Result<Done> {
    let view = view(d)?;
    let st = view.status.clone().unwrap_or_default();
    let order: Vec<Value> = view.order().into_iter().map(|p| json!({ "cloud": p.cloud, "off": p.off, "max_jobs": p.max_jobs, "monthly_usd": p.monthly_usd,
        "running": st["active"][&p.cloud].as_u64().unwrap_or(0), "spent_usd": st["spend"][&p.cloud].as_f64().unwrap_or(0.0) })).collect();
    let mut said: Vec<String> = order.iter().enumerate().map(|(i, p)| format!("{}. {:<14} {}{}{}", i + 1, p["cloud"].as_str().unwrap_or_default(), if p["off"] == true { "off  " } else { "" },
        p["max_jobs"].as_u64().map(|n| format!("up to {n} jobs at once  ")).unwrap_or_default(), p["monthly_usd"].as_f64().map(|n| format!("up to ${n} a month")).unwrap_or_default()).trim_end().to_string()).collect();
    if said.is_empty() { said.push("No runner provider yet: `superci runners create aws` (or cloudflare, or modal).".into()) }
    let machine = serde_json::to_value(view.machine()).unwrap_or_default();
    said.push(format!("`runs-on: superci` gets: {}.", machine_words(&machine)));
    Ok(Done { said, data: json!({ "ok": true, "order": order, "machine": machine, "aws_regions": view.aws_regions(), "aws_networks": st["aws_networks"], "cloudflare_location": st["cloudflare_location"], "cloudflare_image": st["cloudflare_image"] }) })
}

fn github() -> Result<Done> {
    let st = view(&mut signed_in())?.status.unwrap_or_default();
    let mut said = vec![];
    for a in st["apps"].as_array().into_iter().flatten() {
        let installed = st["installations"].as_array().into_iter().flatten().filter(|i| i["app"] == a["id"]).map(|i| format!("{} ({} repositories)", i["account"].as_str().unwrap_or_default(), i["repositories"].as_str().unwrap_or("selected"))).collect::<Vec<_>>();
        said.push(format!("{}  {}", a["owner"].as_str().unwrap_or_default(), if installed.is_empty() { "its App is not installed yet".to_string() } else { format!("installed on {}", installed.join(", ")) }));
    }
    if said.is_empty() { said.push("GitHub is not connected: `superci github create OWNER`.".into()) }
    Ok(Done { said, data: json!({ "github": st["apps"], "installations": st["installations"] }) })
}

fn gitlab() -> Result<Done> {
    let view = view(&mut signed_in())?;
    let mut said: Vec<String> = view.gitlabs().into_iter().map(|g| format!("{}{}", g.1, if g.0.is_empty() { String::new() } else { format!("  ({})", g.0) })).collect();
    if said.is_empty() { said.push("GitLab is not connected: `superci gitlab create --url=https://gitlab.com` (its token in SUPERCI_GITLAB_TOKEN).".into()) }
    Ok(Done { said, data: json!({ "gitlab": view.status.unwrap_or_default()["gitlabs"] }) })
}

fn planes() -> Result<Done> {
    let (views, used): (Vec<PlaneView>, usize) = signed_in().all()?;
    let list: Vec<Value> = views.iter().enumerate().map(|(i, v)| { let mut p = plane_json(v); p["in_use"] = (i == used).into(); p }).collect();
    let mut said: Vec<String> = list.iter().map(|p| format!("{}  {:<44} {}{}", p["id"].as_str().unwrap_or_default(), p["where"].as_str().unwrap_or_default(), p["version"].as_str().unwrap_or("not answering"), if p["in_use"] == true { "  in use" } else { "" })).collect();
    if said.is_empty() { said.push(format!("No control plane in the clouds signed in to. {}", NO_PLANE.trim_start_matches("There is no control plane yet. "))) }
    Ok(Done { said, data: json!({ "control_planes": list }) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Args { Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).unwrap() }

    #[test]
    fn words_and_flags() {
        let a = args("runners update aws --max-jobs 5 --monthly-usd=200 --networks a --networks b --confirm");
        assert_eq!(a.words, ["runners", "update", "aws"]);
        assert!(a.has("confirm") && !a.has("json") && a.get("max-jobs") == Some("5") && a.get("monthly-usd") == Some("200") && a.all("networks") == ["a", "b"]);
        let parse = |line: &str| Args::parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>()).err();
        assert_eq!(parse("status --jsno").unwrap(), Fail::Usage("--jsno is not a flag of superci".into()));
        assert_eq!(parse("planes create aws --region").unwrap(), Fail::Usage("--region needs a value".into()));
        assert!(args("-h").has("help") && args("-V").has("version"));
        assert_eq!(truth(&args("runners update aws --enabled=false"), "enabled"), Ok(Some(false)));
        assert_eq!(truth(&args("runners update aws"), "enabled"), Ok(None));
        assert!(matches!(truth(&args("runners update aws --enabled off"), "enabled"), Err(Fail::Usage(e)) if e == "--enabled is true or false, not off."));
    }

    #[test]
    fn help_for_all_and_for_each() {
        let all = help("");
        // Every resource's commands are in the list of all, and each has help of its own.
        for (word, usage, _) in HELP {
            assert!(all.contains(usage), "{word} is in the list");
            let one = help(word);
            assert!(one.starts_with(usage) && one.contains("--json") && !one.contains("Runners:"), "{word} has its own help");
        }
        assert!(all.starts_with("SuperCI —") && all.contains("superci <resource> <operation> [id] [--param=value]") && all.contains("superci help RESOURCE") && all.contains("SUPERCI_HOME"));
        assert!(help("runners").contains("aws-on-demand") && help("gitlab").contains("SUPERCI_GITLAB_TOKEN") && help("github").contains("GitHub offers that nowhere else"));
        // Every command `run` and `in_browser` know is in some resource's lines.
        for command in ["superci dashboard", "superci login", "superci logout", "superci status", "superci jobs list", "superci jobs retrieve ID", "superci keys list", "superci keys create NAME", "superci keys delete NAME",
            "superci runners list", "superci runners create", "superci runners update CLOUD", "superci runners order", "superci runners delete CLOUD", "superci labels list", "superci labels create NAME", "superci labels delete NAME", "superci machine retrieve", "superci machine update", "superci limits retrieve", "superci limits update",
            "superci planes list", "superci planes create", "superci planes update", "superci planes move ID", "superci planes allow", "superci planes delete ID", "superci leave --confirm", "superci github list", "superci github create OWNER", "superci github delete OWNER",
            "superci github_deliveries list", "superci github_deliveries retrieve ID", "superci gitlab list", "superci gitlab create", "superci gitlab delete", "superci gitlab_projects list", "superci gitlab_projects update", "superci public_repos list", "superci public_repos create", "superci public_repos delete"] {
            assert!(all.contains(command), "{command}");
        }
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

    /// With SuperCI signed in nowhere, every command says so and asks for the sign-in; one that deletes asks for
    /// --confirm before anything else; half a command is said to be half, and what is none, none.
    #[test]
    fn nothing_runs_unasked_or_signed_out() {
        let dir = std::env::temp_dir().join(format!("superci-commands-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        std::env::set_var("SUPERCI_HOME", &dir);
        for name in ["SUPERCI_GITLAB_TOKEN", "SUPERCI_KEY", "SUPERCI_PLANE"] { std::env::remove_var(name) }
        let run = |line: &str| run(&args(line), &mut |_| {}).map(|d| d.said).unwrap_err();
        let signed_out = Fail::Failed(NOT_SIGNED_IN.into());
        for line in ["status", "jobs list", "jobs retrieve 7", "github_deliveries list", "github_deliveries retrieve 7", "keys list", "keys create agent", "keys delete agent", "runners list", "labels list", "labels create soroci", "name retrieve", "name update SoroCI", "github list", "gitlab list", "planes list", "public_repos list", "machine retrieve", "machine update --cpu=8",
            "planes update", "planes create modal", "planes create aws --region=us-east-1", "runners create aws", "runners order aws", "runners update aws --max-jobs=3", "limits retrieve", "limits update --max-cpu=8", "limits update --max-hours 12",
            "public_repos create acme/site", "public_repos delete acme/site", "gitlab_projects list", "gitlab_projects update 7 --enabled=true", "gitlab_projects update --all --enabled=true", "planes allow", "planes move abcdef123456",
            "planes update --dry-run", "runners create cloudflare --dry-run"] {
            assert_eq!(run(line), signed_out, "{line}");
        }
        for line in ["planes delete abcdef123456", "leave", "runners delete aws", "labels delete superci", "github delete acme", "gitlab delete"] {
            assert!(matches!(run(line), Fail::Unconfirmed(e) if e.starts_with("This would ") && e.ends_with("Add --confirm to do it, or --dry-run to check it first.")), "{line}");
            assert_eq!(run(&format!("{line} --confirm")), signed_out, "{line}");
            assert_eq!(run(&format!("{line} --dry-run")), signed_out, "{line}: a dry run checks the sign-in too");
        }
        for (line, says) in [("runners create gcp", "Say which: `superci runners create aws`"), ("planes create", "Say where: `superci planes create aws"), ("planes create aws", "Say where: --region"), ("planes move", "Say where to:"), ("planes delete", "Say which:"),
            ("runners order", "Say the order:"), ("public_repos create", "Say which:"), ("gitlab create", "Give the GitLab token by name: SUPERCI_GITLAB_TOKEN"), ("gitlab create --token-env MY_TOKEN", "Give the GitLab token by name: MY_TOKEN"),
            ("jobs retrieve", "Say which: `superci jobs retrieve ID`"), ("jobs retrieve seven", "Say which: `superci jobs retrieve ID`"), ("keys create", "Give it a name:"), ("keys delete", "Say which:"), ("name update", "Say what to call it:"), ("keys create agent --days soon", "--days is a number"),
            ("gitlab_projects update 7", "Say which way:"), ("gitlab_projects update --all --enabled=false", "Projects are switched off one at a time"),
            ("planes", "`superci planes` takes an operation."), ("limits", "`superci limits` takes an operation."), ("runners frob", "`frob` is not an operation of `superci runners`."), ("limits set", "`set` is not an operation of `superci limits`."),
            ("frobnicate", "`frobnicate` is not a command of superci.")] {
            assert!(matches!(run(line), Fail::Usage(e) if e.starts_with(says)), "{line}");
        }
        assert_eq!(super::run(&args("logout"), &mut |_| {}).unwrap().said, ["Nothing was kept on this machine."]);
        // What opens the browser asks for the same first.
        assert!(matches!(in_browser(&args("login gcp")), Some(Err(Fail::Usage(_)))) && matches!(in_browser(&args("github create")), Some(Err(Fail::Usage(_)))));
        assert_eq!(in_browser(&args("github create acme")).unwrap().unwrap_err(), signed_out);
        assert!(in_browser(&args("status")).is_none() && in_browser(&args("github list")).is_none());
        // A key that only reads, given by name, is no sign-in: a change says so, and names what is needed.
        std::env::set_var("SUPERCI_PLANE", "https://plane.example");
        std::env::set_var("SUPERCI_KEY", "superci_read_x");
        assert_eq!(run("runners create aws"), Fail::Failed(ONLY_READS.into()));
        assert_eq!(needs(ONLY_READS), Some("superci login"));
        std::env::set_var("SUPERCI_PLANE", "plane.example");
        assert!(matches!(run("status"), Fail::Usage(e) if e.starts_with("SUPERCI_PLANE is the control plane's address")));
        for name in ["SUPERCI_KEY", "SUPERCI_PLANE"] { std::env::remove_var(name) }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_says_what_it_would_do() {
        let a = args("gitlab create --dry-run");
        let fields = [field("url", "https://gitlab.com"), field("token", "glpat-secret")];
        let done = dry(&a, "connect https://gitlab.com with the token in SUPERCI_GITLAB_TOKEN", "/gitlab/connect", &fields).unwrap();
        assert_eq!(done.said, ["Would connect https://gitlab.com with the token in SUPERCI_GITLAB_TOKEN. Nothing was changed."]);
        assert!(done.data["dry_run"] == true && done.data["form"]["to"] == "/gitlab/connect" && done.data["form"]["fields"]["token"] == "(given)" && !done.data.to_string().contains("glpat-secret"));
        assert!(dry(&args("gitlab create"), "x", "/gitlab/connect", &fields).is_none());
    }
}
