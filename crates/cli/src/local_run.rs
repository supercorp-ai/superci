//! `superci runs create`: a workflow's job run from this computer, as GitHub would run it, with the working tree as it
//! is and nothing pushed. The tree goes to the control plane in parts, the control plane starts a machine as for any
//! job, and the machine's runner takes the job from the control plane (see `superci_core::runner_service`). This
//! waits for the job and prints its lines.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use futures::executor::block_on;
use serde_json::{json, Value};
use superci_core::io::{Http, Request};

use crate::commands::Done;

type Result<T> = std::result::Result<T, String>;

/// A part of the tree is at most this much (the control plane takes a request of a few MB).
const PART: usize = 3_000_000;
/// The tree, packed, is at most this much when it goes through the control plane's store, and when it goes to its
/// bucket.
const MOST: usize = 150_000_000;
const MOST_IN_BUCKET: usize = 1_000_000_000;
/// The first control plane that runs a workflow from a computer and keeps its artifacts.
pub const SINCE: &str = "0.18.0";

/// Whether the control plane at `plane` knows runs from a computer: one of an older version answers "not here".
pub fn new_enough(version: Option<&str>) -> Result<()> {
    match version {
        Some(v) if crate::view::older(Some(v), SINCE) => Err(format!("The control plane is at {v}; runs from a computer need {SINCE} or newer. Update it: `superci planes update`.")),
        _ => Ok(()),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() { return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())) }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The working tree's files: what git tracks and the new files it does not ignore (an ignored `.env` stays here).
fn tree_files(root: &Path) -> Result<Vec<Vec<u8>>> {
    let listed = Command::new("git").current_dir(root).args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).output().map_err(|e| format!("git: {e}"))?;
    Ok(listed.stdout.split(|b| *b == 0).filter(|f| !f.is_empty()).filter(|f| root.join(String::from_utf8_lossy(f).as_ref()).symlink_metadata().is_ok_and(|m| !m.is_dir())).map(<[u8]>::to_vec).collect())
}

/// Each file's content hash (a link's: where it points), by its path: what a base kept before is told apart by.
fn hashes(root: &Path, files: &[Vec<u8>]) -> std::collections::BTreeMap<String, (String, u64)> {
    use sha2::Digest;
    files.iter().map(|f| {
        let path = String::from_utf8_lossy(f).into_owned();
        let at = root.join(&path);
        let (bytes, size) = match std::fs::read_link(&at) { Ok(to) => (format!("link:{}", to.display()).into_bytes(), 0), Err(_) => { let b = std::fs::read(&at).unwrap_or_default(); let n = b.len() as u64; (b, n) } };
        #[cfg(unix)]
        let mode = at.symlink_metadata().map(|m| { use std::os::unix::fs::PermissionsExt; m.permissions().mode() & 0o111 }).unwrap_or(0);
        #[cfg(not(unix))]
        let mode = 0u32;
        let mut h = sha2::Sha256::new();
        h.update(&bytes);
        h.update(mode.to_le_bytes());
        (path, (format!("{:x}", h.finalize()), size))
    }).collect()
}

/// The files given of the working tree as a gzipped tar, as they are on disk now, with `removed` (paths
/// gone since a base) as `.superci-removed`. By the system's own tar.
fn pack_files(root: &Path, files: &[Vec<u8>], removed: &[String]) -> Result<Vec<u8>> {
    if removed.is_empty() { return tar_of(root, files, true) }
    // What was removed since the base, as an archive of its own after this one, the two compressed as one (the
    // machine reads both in turn, past the end of the first).
    let mut raw = tar_of(root, files, false)?;
    let extra = std::env::temp_dir().join(format!("superci-removed-{}", superci_core::crypto::random_token(8)));
    std::fs::create_dir_all(&extra).and_then(|_| std::fs::write(extra.join(".superci-removed"), removed.join("\n") + "\n")).map_err(|e| e.to_string())?;
    let list = tar_of(&extra, &[b".superci-removed".to_vec()], false);
    let _ = std::fs::remove_dir_all(&extra);
    raw.extend(list?);
    let out = piped(Command::new("gzip").arg("-c"), raw).map_err(|e| format!("gzip: {e}"))?;
    if !out.status.success() { return Err("gzip failed".into()) }
    Ok(out.stdout)
}

/// What a program gives for `input` on its standard input, written from a thread of its own (it may fill its output
/// before it has read all of it).
fn piped(cmd: &mut Command, input: Vec<u8>) -> std::io::Result<std::process::Output> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || stdin.as_mut().map(|s| s.write_all(&input)));
    let out = child.wait_with_output();
    let _ = writer.join();
    out
}

fn tar_of(root: &Path, files: &[Vec<u8>], compress: bool) -> Result<Vec<u8>> {
    let files: Vec<&[u8]> = files.iter().map(Vec::as_slice).collect();
    let args = [if compress { "-czf" } else { "-cf" }, "-", "--no-xattrs", "--null", "-T", "-"];
    let list: Vec<u8> = files.iter().flat_map(|f| f.iter().copied().chain([0])).collect();
    let out = piped(Command::new("tar").current_dir(root).args(args).env("COPYFILE_DISABLE", "1").stderr(Stdio::piped()), list).map_err(|e| format!("tar: {e}"))?;
    if !out.status.success() { return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim())) }
    Ok(out.stdout)
}

/// What this computer last kept as a base for a repository's runs on a control plane: its name and each file's hash.
/// Each run's changes are kept as a layer on what came before: at most this many layers, then all of it again.
const MOST_LAYERS: usize = 8;

/// How long a run is followed with no answer from its control plane before giving up (it goes on there).
fn lost_s() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_LOST_S").ok().and_then(|v| v.parse().ok()) { return s }
    600
}

/// How often a run is asked how it stands (milliseconds): every second and a half, or in a test what `SUPERCI_POLL_MS`
/// says (a debug build), where the waits would only slow the tests.
fn poll_ms() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("SUPERCI_POLL_MS").ok().and_then(|v| v.parse().ok()) { return ms }
    1500
}

/// How long a run that is over is waited for, at most, for each step's own log (seconds): a minute, or in a test what
/// `SUPERCI_LOGS_WAIT_S` says (a debug build).
fn logs_wait_s() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_LOGS_WAIT_S").ok().and_then(|v| v.parse().ok()) { return s }
    60
}

/// What a run from a computer sends through the control plane at most, packed (bytes): `MOST`, or in a test what
/// `SUPERCI_MOST_KB` says (a debug build), where a working tree of 150 MB would only slow the tests.
fn most_through_plane() -> usize {
    #[cfg(debug_assertions)]
    if let Some(kb) = std::env::var("SUPERCI_MOST_KB").ok().and_then(|v| v.parse::<usize>().ok()) { return kb * 1000 }
    MOST
}

/// How long a job's lines are waited for once it ended, or the run is over (seconds), before its end is said with
/// what came: 15, or in a test what `SUPERCI_GIVE_UP_S` says (a debug build).
fn give_up_s() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_GIVE_UP_S").ok().and_then(|v| v.parse().ok()) { return s }
    15
}

/// How long a run is followed at most (seconds): six hours, or in a test what `SUPERCI_FOLLOW_S` says (a debug build).
fn follow_s() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("SUPERCI_FOLLOW_S").ok().and_then(|v| v.parse().ok()) { return s }
    6 * 3600
}

/// What this computer kept of a repository's last run on a control plane: the whole tree it sent once (`name`) and
/// what each run since sent on top of it (`layers`), each file's hash as of that last run, and when it was.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Base {
    name: String,
    #[serde(default)] layers: Vec<String>,
    files: std::collections::BTreeMap<String, (String, u64)>,
    #[serde(default)] at_ms: u64,
}

/// How long ago, for a person: "2 minutes ago", "3 hours ago", "yesterday or before".
fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let m = now.saturating_sub(ms) / 60_000;
    match m { 0 => "just now".into(), 1 => "a minute ago".into(), 2..=59 => format!("{m} minutes ago"), 60..=119 => "an hour ago".into(), 120..=1439 => format!("{} hours ago", m / 60), _ => "a day or more ago".into() }
}

fn base_path(plane: &str, root: &Path) -> Option<std::path::PathBuf> {
    let key = superci_core::crypto::sha256_hex(format!("{plane}\n{}", root.display()).as_bytes());
    Some(crate::store::root()?.join("trees").join(format!("{}.json", &key[..32])))
}

/// `owner/name` of the repository this checkout's `origin` is on GitHub (else the folder's name under `local/`), or
/// the one given (`--repo`).
fn repository(root: &Path, given: Option<&str>) -> String {
    if let Some(r) = given { return r.to_string() }
    let url = git(root, &["remote", "get-url", "origin"]).unwrap_or_default();
    let path = url.trim_end_matches(".git").rsplit_once("github.com").map(|(_, p)| p.trim_start_matches([':', '/']).to_string()).unwrap_or_default();
    if path.split('/').count() == 2 { path } else { format!("local/{}", root.file_name().and_then(|n| n.to_str()).unwrap_or("repository")) }
}

/// How a run is followed: `quiet`, its jobs' lines are not printed (each step's result is, and the end of a failed
/// one's log); `detach`, not at all (its id is said, for `superci runs wait`).
#[derive(Clone, Copy, Default)]
pub struct Follow { pub quiet: bool, pub detach: bool, pub dry_run: bool }

/// The event a run from a computer is started as: `push`, `pull_request` (with its labels) or `workflow_dispatch`
/// (with its inputs), and a file of JSON put over the payload made for it.
#[derive(Clone, Default)]
pub struct Event { pub name: String, pub labels: Vec<String>, pub inputs: Vec<(String, String)>, pub payload: Option<String>,
    /// The GitHub repository the run is of (`--repo=OWNER/NAME`), for a checkout whose `origin` does not say it.
    pub repo: Option<String> }

/// The event's payload as GitHub would send it (`github.event`), from the working tree and what was given, and a
/// manual start's inputs, typed (`inputs`). What does not go together is refused, before anything is sent.
fn event_payload(root: &Path, text: &str, e: &Event, reference: &str, sha: &str, actor: &str) -> Result<(Value, Value)> {
    if !["push", "pull_request", "workflow_dispatch"].contains(&e.name.as_str()) { return Err(format!("--event is push, pull_request or workflow_dispatch, not “{}”.", e.name)) }
    if !e.labels.is_empty() && e.name != "pull_request" { return Err("--label is a pull request's label: --event=pull_request --label=NAME.".into()) }
    if !e.inputs.is_empty() && e.name != "workflow_dispatch" { return Err("--input is a manual start's input: --event=workflow_dispatch --input=NAME=VALUE.".into()) }
    let repo = repository(root, e.repo.as_deref());
    let (owner, name) = repo.split_once('/').unwrap_or(("local", repo.as_str()));
    // (The default branch: origin's, else main.)
    let default_branch = git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).ok().and_then(|b| b.strip_prefix("origin/").map(str::to_string)).unwrap_or_else(|| "main".into());
    let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string();
    let subject = git(root, &["log", "-1", "--format=%s"]).unwrap_or_default();
    let mut payload = json!({ "repository": { "full_name": repo, "name": name, "owner": { "login": owner }, "default_branch": default_branch }, "sender": { "login": actor } });
    let mut inputs = json!({});
    match e.name.as_str() {
        "push" => { payload["ref"] = reference.into(); payload["before"] = "0".repeat(40).into(); payload["after"] = sha.into(); payload["head_commit"] = json!({ "id": sha, "message": git(root, &["log", "-1", "--format=%B"]).unwrap_or_default() }); payload["pusher"] = json!({ "name": actor }) }
        "pull_request" => {
            // (Labelled: as GitHub sends a label's adding, the last one the label added.)
            payload["action"] = if e.labels.is_empty() { "synchronize" } else { "labeled" }.into();
            payload["number"] = 1.into();
            payload["pull_request"] = json!({ "number": 1, "title": subject, "state": "open", "draft": false, "user": { "login": actor }, "head": { "ref": branch, "sha": sha }, "base": { "ref": default_branch },
                "labels": e.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>() });
            if let Some(l) = e.labels.last() { payload["label"] = json!({ "name": l }) }
        }
        _ => {
            let wf = superci_core::workflow::parse(text).map_err(|err| err.to_string())?;
            let (typed, as_text) = superci_core::workflow::dispatch_inputs(&wf, &e.inputs).map_err(|err| format!("{}{}.", &err[..1].to_uppercase(), &err[1..]))?;
            payload["ref"] = reference.into();
            payload["inputs"] = as_text;
            inputs = typed;
        }
    }
    // A file's JSON over it: objects merged, anything else replaced.
    if let Some(file) = &e.payload {
        let given: Value = serde_json::from_slice(&std::fs::read(file).map_err(|err| format!("{file}: {err}"))?).map_err(|err| format!("{file} is not JSON: {err}"))?;
        if !given.is_object() { return Err(format!("{file} is a JSON object: the payload's fields, as GitHub's event has them.")) }
        fn over(a: &mut Value, b: Value) { match (a, b) { (Value::Object(a), Value::Object(b)) => for (k, v) in b { over(a.entry(k).or_insert(Value::Null), v) }, (a, b) => *a = b } }
        over(&mut payload, given);
    }
    let bytes = payload.to_string().len() + inputs.to_string().len();
    if bytes > superci_core::plane::RUN_EVENT_BYTES { return Err(format!("The event's payload is {} KB; a run takes {} KB at most.", bytes.div_ceil(1024), superci_core::plane::RUN_EVENT_BYTES / 1024)) }
    Ok((payload, inputs))
}

pub fn create(plane: &str, key: &str, workflow: &str, job: Option<&str>, event: &Event, secrets: &[(String, String)], vars: &[(String, String)], how: Follow, say: &mut dyn FnMut(&str)) -> Result<Done> {
    let here = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = std::path::PathBuf::from(git(&here, &["rev-parse", "--show-toplevel"]).map_err(|_| "Run it in a git checkout: the run takes its working tree.".to_string())?);
    let file = here.join(workflow);
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{workflow}: {e}"))?;
    let path = file.canonicalize().ok().and_then(|f| f.strip_prefix(root.canonicalize().ok()?).ok().map(|p| p.to_string_lossy().replace('\\', "/"))).unwrap_or_else(|| workflow.to_string());
    let reference = git(&root, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_else(|_| "refs/heads/main".into());
    let (sha, actor) = (git(&root, &["rev-parse", "HEAD"]).unwrap_or_default(), git(&root, &["config", "user.name"]).unwrap_or_else(|_| "superci".into()));
    let (payload, inputs) = event_payload(&root, &text, event, &reference, &sha, &actor)?;
    new_enough(crate::cloudflare::health(plane).as_ref().and_then(|h| h["version"].as_str()))?;
    if how.dry_run { return dry_run(&root, &path, &text, job) }
    let http = crate::aws::Blocking::new();
    let call = |method: &str, at: &str, body: Vec<u8>| -> Result<Value> {
        let r = Request::new(method, &format!("{plane}{at}")).with_header("authorization", &format!("Bearer {key}")).with_body(body);
        let res = block_on(http.send(r))?;
        if res.status >= 300 { return Err(format!("the control plane said {}: {}", res.status, res.body_text().trim())) }
        serde_json::from_slice(&res.body).map_err(|e| format!("the control plane's answer: {e}"))
    };
    let tree = format!("tree-{}", superci_core::crypto::random_token(18));
    let put = |link: &str, bytes: Vec<u8>| -> Result<()> {
        let res = block_on(http.send(Request::new("PUT", link).with_body(bytes)))?;
        if res.status >= 300 { return Err(format!("the bucket said {} to the working tree: {}", res.status, res.body_text().trim())) }
        Ok(())
    };
    // Only what changed since the base this computer kept for the repository, when the control plane still has it
    // and that is less than half of the tree: the machine puts the base in place, then the changes.
    let files = tree_files(&root)?;
    let now = hashes(&root, &files);
    let kept_at = base_path(plane, &root);
    let kept: Option<Base> = kept_at.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    let mut sent: Option<(u32, bool)> = None;
    // Into the bucket: at once, or while the run's machine starts when the control plane can have the machine wait
    // for it (`later`: it says so); then the base is kept, when it was one.
    let mut pending: Option<(String, Vec<u8>, bool)> = None;
    // What changed since this folder's last run, as a layer on what that run had (kept for the next run's), while
    // that is less than half of the tree and the layers are few.
    let mut layer: Option<String> = None;
    if let Some(base) = kept.as_ref().filter(|b| b.layers.len() < MOST_LAYERS) {
        let changed: Vec<Vec<u8>> = files.iter().filter(|f| { let p = String::from_utf8_lossy(f); base.files.get(p.as_ref()) != now.get(p.as_ref()) }).cloned().collect();
        let removed: Vec<String> = base.files.keys().filter(|p| !now.contains_key(*p)).cloned().collect();
        let (some, all) = (changed.iter().map(|f| now.get(String::from_utf8_lossy(f).as_ref()).map(|h| h.1).unwrap_or(0)).sum::<u64>(), now.values().map(|h| h.1).sum::<u64>());
        if some * 2 < all.max(1) {
            let chain: Vec<String> = std::iter::once(base.name.clone()).chain(base.layers.iter().cloned()).collect();
            let name = format!("l{}", &superci_core::crypto::sha256_hex(format!("{}\n{}", chain.join(","), serde_json::to_string(&now).unwrap_or_default()).as_bytes())[..40]);
            let asked = call("POST", &format!("/runs/tree?id={tree}&part=0&link=1&later=1&base={}&layer={name}", chain.join(",")), vec![])?;
            if let (Some(link), true) = (asked["url"].as_str(), asked["base"] == true) {
                let packed = pack_files(&root, &changed, &removed)?;
                let since = if base.at_ms > 0 { format!(", {}", ago(base.at_ms)) } else { String::new() };
                say(&format!("Sending what changed since this folder's last run{since}: {} file{}, {} removed ({} KB packed)", changed.len(), if changed.len() == 1 { "" } else { "s" }, removed.len(), packed.len().div_ceil(1000)));
                if asked["later"] == true { pending = Some((link.to_string(), packed, true)) } else { put(link, packed)? }
                sent = Some((asked["parts"].as_u64().unwrap_or(2) as u32, true));
                layer = Some(name);
            }
        }
    }
    // Else all of it, into the control plane's bucket in one when it has one (as a build service takes its source),
    // kept there as the base for the next runs'; else through the control plane, in parts (it takes a request of a
    // few MB).
    let packed = if sent.is_some() { vec![] } else { pack_files(&root, &files, &[])? };
    let name = format!("b{}", &superci_core::crypto::sha256_hex(serde_json::to_string(&now).unwrap_or_default().as_bytes())[..40]);
    let asked = if sent.is_some() { Value::Null } else { call("POST", &format!("/runs/tree?id={tree}&part=0&link=1&later=1&keep={name}"), vec![])? };
    let link = asked["url"].as_str().map(str::to_string);
    let most = if link.is_some() || sent.is_some() { MOST_IN_BUCKET } else { most_through_plane() };
    if packed.len() > most { return Err(format!("The working tree is {} MB packed; a run from a computer takes {} MB at most.", packed.len() / 1_000_000, most / 1_000_000)) }
    // Kept once what was sent is there: the whole tree as a new base, or the chain with this run's layer on it.
    let keep_base = |name: &str, now: &std::collections::BTreeMap<String, (String, u64)>| if let Some(at) = &kept_at {
        let at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let next = match (&layer, &kept) {
            (Some(l), Some(b)) => Base { name: b.name.clone(), layers: b.layers.iter().cloned().chain([l.clone()]).collect(), files: now.clone(), at_ms },
            _ => Base { name: name.to_string(), layers: vec![], files: now.clone(), at_ms },
        };
        let _ = std::fs::create_dir_all(at.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(at, serde_json::to_vec(&next).unwrap_or_default());
    };
    let (parts, layered) = match (&link, sent) {
        (_, Some(sent)) => sent,
        (Some(link), None) => {
            say(&format!("Sending the working tree ({} KB packed)", packed.len().div_ceil(1000)));
            if asked["later"] == true { pending = Some((link.clone(), packed, true)) } else { put(link, packed)?; keep_base(&name, &now) }
            (1, false)
        }
        (None, None) => {
            let parts = packed.chunks(PART).count() as u32;
            say(&format!("Sending the working tree ({} KB packed, {parts} part{})", packed.len().div_ceil(1000), if parts == 1 { "" } else { "s" }));
            packed.chunks(PART).enumerate().try_for_each(|(i, part)| call("POST", &format!("/runs/tree?id={tree}&part={i}"), part.to_vec()).map(drop))?;
            (parts, false)
        }
    };
    // (A layer sent at once: kept now.)
    if layer.is_some() && pending.is_none() { keep_base(&name, &now) }
    // The workflows its jobs call (`uses: ./.github/workflows/x.yml`), from the working tree as it is.
    let mut called = serde_json::Map::new();
    if let Ok(wf) = superci_core::workflow::parse(&text) {
        for (_, j) in wf.get("jobs").map(|j| j.pairs().to_vec()).unwrap_or_default() {
            let Some(p) = j.get("uses").and_then(|u| u.text()).and_then(|u| u.strip_prefix("./")).filter(|p| !p.contains('@')) else { continue };
            if called.contains_key(p) { continue }
            let file = root.join(p);
            let t = std::fs::read_to_string(&file).map_err(|e| format!("{p}, which a job calls: {e}"))?;
            called.insert(p.to_string(), t.into());
        }
    }
    let body = json!({ "workflow": text, "path": path, "job": job, "repository": repository(&root, event.repo.as_deref()), "ref": reference, "sha": sha,
        "actor": actor, "event_name": event.name, "event": payload, "inputs": inputs, "secrets": secrets, "vars": vars, "tree": tree, "parts": parts, "layered": layered, "called": called });
    let made = call("POST", "/runs", body.to_string().into_bytes())?;
    let run = made["run_id"].as_str().unwrap_or_default().to_string();
    // Secrets its workflows name that this CI does not keep for it and that were not given: empty, as on GitHub.
    let missing: Vec<&str> = made["missing_secrets"].as_array().into_iter().flatten().filter_map(|n| n.as_str()).collect();
    let missing_said = (!missing.is_empty()).then(|| format!("Not set in this CI, empty as on GitHub: {} (`superci secrets create NAME`, or --secret=NAME=VALUE).", missing.join(", ")));
    // Its cache kept apart from what GitHub's jobs saved: why, and what to do.
    let repo = repository(&root, event.repo.as_deref());
    let cache_said = (made["cache_own"] == true).then(|| if repo.starts_with("local/") { "Its cache is its own: this checkout has no GitHub remote, so it does not read what GitHub's jobs saved. --repo=OWNER/NAME names its repository.".to_string() }
        else { format!("Its cache is its own: GitHub's App is not installed on {repo}, so it does not read what GitHub's jobs saved.") });
    let notes = match (missing_said, cache_said) { (Some(m), Some(c)) => Some(format!("{m}\n{c}")), (m, c) => m.or(c) };
    // The tree still to send goes while the machine starts; the control plane is told once it is all there. Not
    // sent: the run is cancelled (its machine would wait for it).
    let sending = pending.map(|(link, bytes, base)| {
        let (plane, key, run, tree) = (plane.to_string(), key.to_string(), run.clone(), tree.clone());
        std::thread::spawn(move || -> Result<bool> {
            let http = crate::aws::Blocking::new();
            let ask = |method: &str, at: &str, body: Vec<u8>| block_on(http.send(Request::new(method, at).with_header("authorization", &format!("Bearer {key}")).with_body(body)));
            let sent = block_on(http.send(Request::new("PUT", &link).with_body(bytes))).and_then(|r| if r.status < 300 { Ok(()) } else { Err(format!("the bucket said {} to the working tree: {}", r.status, r.body_text().trim())) })
                .and_then(|_| ask("POST", &format!("{plane}/runs/tree?id={tree}&part=0&sent=1"), vec![]).and_then(|r| if r.status < 300 { Ok(()) } else { Err(format!("the control plane said {} to the working tree's end: {}", r.status, r.body_text().trim())) }));
            if sent.is_err() { let _ = ask("POST", &format!("{plane}/runs/{run}/cancel"), b"{}".to_vec()); }
            sent.map(|_| base)
        })
    });
    let sent = |sending: Option<std::thread::JoinHandle<Result<bool>>>| -> Result<()> {
        match sending.map(|t| t.join().unwrap_or(Err("sending the working tree stopped".into()))) {
            Some(Ok(true)) => { keep_base(&name, &now); Ok(()) }
            Some(Err(e)) => Err(format!("The working tree was not sent ({e}); run {run} was cancelled.")),
            _ => Ok(()),
        }
    };
    if how.detach {
        sent(sending)?;
        return Ok(Done { said: notes.into_iter().chain([format!("Run {run} of {path} started. Follow it: superci runs wait {run}")]).collect(), data: json!({ "run_id": run, "jobs": made["jobs"], "missing_secrets": missing, "cache_own": made["cache_own"] == true }) })
    }
    if let Some(m) = &notes { say(m) }
    say(&format!("Run {run} of {path}, on machines of this control plane's"));
    let followed = follow(plane, key, &run, how, std::time::Instant::now(), true, say);
    sent(sending)?;
    followed
}

/// `superci runs create --dry-run`: what would run, after what can be checked here was (the workflow, its job, the
/// control plane, the working tree), with nothing sent and no machine started.
fn dry_run(root: &Path, path: &str, text: &str, job: Option<&str>) -> Result<Done> {
    let wf = superci_core::workflow::parse(text).map_err(|e| format!("{path}: {e}"))?;
    let jobs: Vec<String> = wf.get("jobs").map(|j| j.pairs().iter().filter_map(|(k, _)| k.text().map(str::to_string)).collect()).unwrap_or_default();
    if jobs.is_empty() { return Err(format!("{path} has no jobs.")) }
    if let Some(j) = job.filter(|j| !jobs.iter().any(|k| k == j)) { return Err(format!("{path} has no job {j} (its jobs: {}).", jobs.join(", "))) }
    let files = tree_files(root)?;
    let bytes: u64 = files.iter().filter_map(|f| root.join(String::from_utf8_lossy(f).as_ref()).symlink_metadata().ok()).map(|m| m.len()).sum();
    let which = match job { Some(j) => format!("its job {j} (and the jobs it needs)"), None => format!("its job{} {}", if jobs.len() == 1 { "" } else { "s" }, jobs.join(", ")) };
    let would = format!("send the working tree ({} files, {} MB before packing) and run {path}: {which}, on this control plane's machines", files.len(), bytes.div_ceil(1_000_000));
    Ok(Done { said: vec![format!("Would {would}. Nothing was sent and no machine was started.")], data: json!({ "dry_run": true, "would": would, "jobs": jobs, "files": files.len(), "bytes": bytes }) })
}

/// `superci runs wait ID`: a run from a computer followed to its end, as `runs create` follows the one it starts.
pub fn wait(plane: &str, key: &str, run: &str, how: Follow, say: &mut dyn FnMut(&str)) -> Result<Done> {
    if run.is_empty() || !run.bytes().all(|b| b.is_ascii_digit()) { return Err("Say which run: `superci runs wait ID` (ids are in `superci runs list`).".into()) }
    follow(plane, key, run, how, std::time::Instant::now(), false, say)
}

fn follow(plane: &str, key: &str, run: &str, how: Follow, began: std::time::Instant, sent_here: bool, say: &mut dyn FnMut(&str)) -> Result<Done> {
    let http = crate::aws::Blocking::new();
    // (An error that may pass: no answer at all, as when this computer's network or DNS drops, or the control plane
    // busy or restarting. The run goes on meanwhile.)
    let call = |method: &str, at: &str| -> std::result::Result<Value, (bool, String)> {
        let r = Request::new(method, &format!("{plane}{at}")).with_header("authorization", &format!("Bearer {key}"));
        let res = block_on(http.send(r)).map_err(|e| (true, e.to_string()))?;
        if res.status == 404 { return Err((false, format!("No run {run} among this control plane's runs from a computer (they are kept a day)."))) }
        if res.status >= 300 { return Err((res.status >= 500 || res.status == 429, format!("the control plane said {}: {}", res.status, res.body_text().trim()))) }
        serde_json::from_slice(&res.body).map_err(|e| (false, format!("the control plane's answer: {e}")))
    };
    let mut lost: Option<std::time::Instant> = None;
    let (mut after, mut last): (Vec<u64>, Vec<String>) = (vec![], vec![]);
    let mut logs: Vec<Lines> = vec![];
    let mut ended_at: Vec<Option<std::time::Instant>> = vec![];
    // (Jobs whose end was said without all their steps' lines: the summary does not wait for those again.)
    let mut said_without: Vec<bool> = vec![];
    let mut over_at: Option<std::time::Instant> = None;
    // (Said once: the run is over, and its summary waits for what its runner still sends.)
    let mut told_waiting = false;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(poll_ms()));
        let cursor = after.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
        // Out of touch with the control plane: asked again, a few seconds apart, for ten minutes at most.
        let st = match call("GET", &format!("/runs/{run}?after={cursor}")) {
            Ok(st) => { if let Some(t) = lost.take() { say(&format!("In touch with the control plane again after {}.", span(t.elapsed().as_secs()))) } st }
            Err((true, e)) => {
                let since = *lost.get_or_insert_with(|| { say(&format!("Out of touch with the control plane ({e}); asking again. The run goes on meanwhile.")); std::time::Instant::now() });
                if since.elapsed().as_secs() >= lost_s() { return Err(format!("Out of touch with the control plane for {} ({e}). The run goes on there: `superci runs wait {run}` follows it again.", span(since.elapsed().as_secs()))) }
                std::thread::sleep(std::time::Duration::from_millis(poll_ms() * 7 / 3));
                continue
            }
            Err((false, e)) => return Err(e),
        };
        let jobs = st["jobs"].as_array().cloned().unwrap_or_default();
        after.resize(jobs.len(), 0);
        last.resize(jobs.len(), String::new());
        logs.resize_with(jobs.len(), Lines::default);
        ended_at.resize(jobs.len(), None);
        said_without.resize(jobs.len(), false);
        let several = jobs.len() > 1;
        for (i, j) in jobs.iter().enumerate() {
            let name = j["name"].as_str().unwrap_or_default();
            let mark = if several { format!("[{name}] ") } else { String::new() };
            // How each job stands, when that changes.
            let state = match (j["state"].as_str().unwrap_or_default(), j["result"].as_str()) {
                (_, Some("skipped")) => "skipped".to_string(),
                ("pending", _) => String::new(),
                ("launching", _) => "asking for its machine".to_string(),
                ("launched", _) => format!("its machine is starting ({})", j["machine"].as_str().unwrap_or("")),
                ("running", _) => "running".to_string(),
                // (With the control plane's word for why, when the job ended without its runner: no machine began it.)
                (_, Some(r)) => match j["error"].as_str().filter(|e| !e.is_empty()) { Some(e) => format!("ended: {r}: {e}"), None => format!("ended: {r}") },
                (other, _) => other.to_string(),
            };
            // (Its end is said after its lines: once each step's are out, or 15 seconds after it ended at most.)
            let ended = j["result"].is_string() && state != "skipped";
            if ended && ended_at[i].is_none() { ended_at[i] = Some(std::time::Instant::now()) }
            let give_up = ended_at[i].is_some_and(|t| t.elapsed().as_secs() > give_up_s()) || (st["over"] == true && over_at.is_some_and(|t| t.elapsed().as_secs() > give_up_s()));
            let said_state = |say: &mut dyn FnMut(&str), state: String, last: &mut String| if !state.is_empty() && state != *last {
                // Running: how its start went, in a line (where the time from sending it to its first step went).
                let start = if state == "running" { start_line(&j["start"]) } else { String::new() };
                say(&format!("{mark}{}{}{start}", &state[..1].to_uppercase(), &state[1..]));
                // (A machine other than the first choice, and why: AWS had none of that one, at that price.)
                if state == "running" { if let Some(choice) = crate::dashboard::choice_said(&j["start"]) { say(&format!("{mark}{choice}; it got {}{}.", j["start"]["machine_type"].as_str().unwrap_or("another"), j["start"]["usd_per_hour"].as_f64().map(|u| format!(" (${u:.3} an hour)")).unwrap_or_default())) } }
                *last = state
            };
            if !ended { said_state(say, state.clone(), &mut last[i]) }
            // Its lines as GitHub shows them (a group's heading is a line, its end is nothing), each step's whole: what
            // the runner sent as it went while nothing is missing, then the rest from the step's own log once it ends
            // (the runner drops lines it cannot send as fast as they come).
            let fetch = |n: u64| -> Option<String> {
                let r = Request::new("GET", &format!("{plane}/runs/{run}/log?job={i}&log={n}")).with_header("authorization", &format!("Bearer {key}"));
                block_on(http.send(r)).ok().filter(|r| r.status < 300).map(|r| r.body_text())
            };
            let pages = |step: &str| -> Option<String> {
                let r = Request::new("GET", &format!("{plane}/runs/{run}/log?job={i}&step={step}")).with_header("authorization", &format!("Bearer {key}"));
                block_on(http.send(r)).ok().filter(|r| r.status < 300).map(|r| r.body_text())
            };
            for line in logs[i].take(j, give_up, &fetch, &pages) {
                if line.starts_with("##[endgroup]") || how.quiet { continue }
                say(&format!("{}{mark}{}", crate::commands::JOB_LINE, line.strip_prefix("##[group]").unwrap_or(&line)))
            }
            if ended && (logs[i].complete() || give_up) {
                if !logs[i].complete() { said_without[i] = true }
                said_state(say, state, &mut last[i])
            }
            after[i] = j["next"].as_u64().unwrap_or(after[i]);
        }
        // Over: once every step's lines are out (its log may come a while after: the runner sends steps' logs as it
        // can, a large one after the job has ended), or a minute later at most.
        if st["over"] == true && over_at.is_none() { over_at = Some(std::time::Instant::now()) }
        // (And for each step's result, which its runner may send after the job has ended too.)
        // (Only a job that did not pass: a step's result that never comes is judged by its log then. A passed job's
        // summary does not wait for it.)
        let results_missing = jobs.iter().any(|j| j["result"].is_string() && !["skipped", "cancelled", "success", "succeeded"].contains(&j["result"].as_str().unwrap_or_default()) && j["steps"].as_array().is_some_and(|ss| ss.iter().any(|s| s["result"].is_null() && s["start"].is_string())));
        // (And for what a machine that keeps Docker's images had from the cache, which it says as its job ends: 10
        // seconds at most.)
        let images_missing = jobs.iter().any(|j| j["result"].is_string() && j["start"]["keeps"] == true && j["start"]["images"].is_null());
        let waited = over_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let lines_missing = logs.iter().zip(&said_without).any(|(l, without)| !l.complete() && !without);
        if st["over"] == true && (((lines_missing || results_missing) && waited <= logs_wait_s()) || (images_missing && waited <= logs_wait_s().min(10))) {
            if !told_waiting && over_at.is_some_and(|t| t.elapsed().as_secs() >= 2) { say("The run is over; its summary comes once its runner has sent the steps' logs (a minute at most)."); told_waiting = true }
            continue
        }
        if st["over"] == true {
            let (result, took) = (st["result"].as_str().unwrap_or_default().to_string(), began.elapsed().as_secs());
            // Each job's steps, how each went and how long it took; a failed step's last lines when they were not
            // printed as they came.
            let usd: f64 = jobs.iter().filter_map(|j| j["usd"].as_f64()).sum::<f64>() + 0.0;
            for (i, j) in jobs.iter().enumerate() {
                say(&format!("{}{}{} ({}{}{}): {}", crate::commands::JOB_LINE, j["name"].as_str().unwrap_or("job"), j["job_id"].as_str().map(|id| format!(" · job {id}")).unwrap_or_default(), j["machine"].as_str().unwrap_or("no machine"),
                    j["took_ms"].as_u64().map(|ms| format!(", {}", span(ms / 1000))).unwrap_or_default(), j["usd"].as_f64().map(|u| format!(", ${u:.3}")).unwrap_or_default(),
                    match (j["result"].as_str().unwrap_or("?"), j["error"].as_str().filter(|e| !e.is_empty())) { (r, Some(e)) => format!("{r} ({e})"), (r, None) => r.to_string() }));
                // Started again: said, with its first attempt (its artifacts are kept anew by this one).
                // What it had from the cache's Docker images and pulled (what was kept, once its machine has: `jobs retrieve`).
                match crate::dashboard::images_said(&j["start"]) {
                    Some(i) => say(&format!("{}  Docker images: {i}.", crate::commands::JOB_LINE)),
                    None if j["start"]["keeps"] == true && j["start"]["images"].is_null() => if let Some(id) = j["job_id"].as_str() { say(&format!("{}  Docker images: not said yet by its machine (`superci jobs retrieve {id}` says them once it has).", crate::commands::JOB_LINE)) },
                    None => {}
                }
                if let Some(first) = j["restarted"]["job_id"].as_str() { say(&format!("{}  Started again {}: {} (its first attempt was job {first}).", crate::commands::JOB_LINE, if j["restarted"]["why"].as_str().is_some_and(|w| w.starts_with("Modal")) { "further down the order" } else { "on an on-demand machine" }, j["restarted"]["why"].as_str().unwrap_or("its machine ended"))) }
                let mut steps: Vec<&Value> = j["steps"].as_array().into_iter().flatten().collect();
                steps.sort_by_key(|s| s["order"].as_u64().unwrap_or(0));
                // Each step's whole log is read for its tests (its own log, else what its machine sent of it).
                let step_text = |s: &Value| -> String {
                    let at = match s["log"].as_u64() { Some(n) => format!("log={n}"), None => format!("step={}&lines=1", s["id"].as_str().unwrap_or_default()) };
                    let r = Request::new("GET", &format!("{plane}/runs/{run}/log?job={i}&{at}")).with_header("authorization", &format!("Bearer {key}"));
                    block_on(http.send(r)).ok().filter(|r| r.status < 300).map(|r| r.body_text()).unwrap_or_default()
                };
                let mut all = String::new();
                let mut failed_unsaid: Vec<String> = vec![];
                for s in steps {
                    // (A step whose result never came is judged by its own log: tests that failed there, failed.)
                    let unsaid = s["result"].is_null();
                    let text = if ["skipped", "cancelled"].contains(&s["result"].as_str().unwrap_or_default()) { String::new() } else { step_text(s) };
                    let failed_here = s["result"] == "failed" || (unsaid && crate::test_results::read(&text).failed > 0);
                    if failed_here && unsaid { failed_unsaid.push(s["name"].as_str().unwrap_or("step").to_string()) }
                    let mark = match s["result"].as_str() { Some("succeeded") => "✓", Some("failed") => "✗", Some("skipped") | Some("cancelled") => "–", _ if failed_here => "✗", _ => "·" };
                    let time = match (s["start"].as_str().and_then(crate::log_view::seconds), s["finish"].as_str().and_then(crate::log_view::seconds)) { (Some(a), Some(b)) => format!(" {}", span(b.saturating_sub(a))), _ => String::new() };
                    say(&format!("{}  {mark} {}{time}", crate::commands::JOB_LINE, s["name"].as_str().unwrap_or("step")));
                    if ["skipped", "cancelled"].contains(&s["result"].as_str().unwrap_or_default()) { continue }
                    if how.quiet && failed_here {
                        // The tests it failed, each with its whole error (its last lines may be another's, or a
                        // teardown's), then those lines.
                        for f in &crate::test_results::read(&text).failures {
                            say(&format!("{}    ✗ {}{}", crate::commands::JOB_LINE, f.name, f.file.as_deref().map(|p| format!(" ({p})")).unwrap_or_default()));
                            for l in f.error.lines() { say(&format!("{}        {l}", crate::commands::JOB_LINE)) }
                        }
                        for l in last_lines(&text) { say(&format!("{}    {l}", crate::commands::JOB_LINE)) }
                    }
                    all.push_str(&text);
                    all.push('\n');
                }
                // A runner slow to reach the control plane: what it logged meanwhile (its machine sends it once it listens).
                if let Some(log) = j["start"]["machine"]["log"].as_str().filter(|l| !l.trim().is_empty()) {
                    crate::dashboard::runner_log_lines(log, true).iter().for_each(|l| say(&format!("{}  {l}", crate::commands::JOB_LINE)));
                }
                // A failed job: which steps failed, said in words (all its tests may have passed: a type-check failed, say).
                if j["result"] == "failure" {
                    let failed: Vec<&str> = j["steps"].as_array().into_iter().flatten().filter(|s| s["result"] == "failed").filter_map(|s| s["name"].as_str()).chain(failed_unsaid.iter().map(String::as_str)).collect();
                    if !failed.is_empty() { say(&format!("{}  Failed: {}", crate::commands::JOB_LINE, failed.join(", "))) }
                }
                if let (Some(id), true) = (j["job_id"].as_str(), j["result"] == "failure") { say(&format!("{}  Its whole log: superci jobs retrieve {id} --out=job.log", crate::commands::JOB_LINE)) }
                // What its tests came to, passed or not, and what a runner said of them as a whole. A step whose own
                // log has not come is read from the lines its runner sent as it went, some dropped: its tests are
                // not all counted, and that is said.
                let tests = crate::test_results::read(&all);
                let unlogged = j["steps"].as_array().into_iter().flatten().filter(|s| ["succeeded", "failed"].contains(&s["result"].as_str().unwrap_or_default()) && s["log"].is_null()).count();
                if !tests.runners.is_empty() {
                    let t = serde_json::to_value(&tests).unwrap_or_default();
                    say(&format!("{}  {}", crate::commands::JOB_LINE, crate::commands::totals_line(&t)));
                    for l in crate::commands::process_lines(&t, "  ").into_iter().chain(crate::commands::ran_lines(&t, "  ")).chain(crate::commands::expected_lines(&t, "  ")) { say(&format!("{}{l}", crate::commands::JOB_LINE)) }
                    for n in &tests.notes { say(&format!("{}  {n}", crate::commands::JOB_LINE)) }
                    if unlogged > 0 { say(&format!("{}  (Counted from what came: {unlogged} step{} log{} had not come. `superci runs retrieve {run} --tests` counts {} once {} here.)", crate::commands::JOB_LINE, if unlogged == 1 { "'s" } else { "s'" }, if unlogged == 1 { "" } else { "s" }, if unlogged == 1 { "it" } else { "them" }, if unlogged == 1 { "it is" } else { "they are" })) }
                }
            }
            let said = format!("The run ended: {result}, {} after it was {}, ${usd:.3} of machines.", span(took), if sent_here { "sent" } else { "followed from here" });
            // (Not a success: the same data, as a failure's, with its job ids for a script.)
            let ok = result == "success";
            return Ok(Done { said: vec![said.clone()], data: json!({ "ok": ok, "error": if ok { Value::Null } else { said.into() }, "run_id": run, "result": result, "seconds": took, "usd": usd, "jobs": jobs.iter().map(|j| json!({ "name": j["name"], "job_id": j["job_id"], "result": j["result"], "machine": j["machine"], "usd": j["usd"], "took_ms": j["took_ms"],
                "steps": j["steps"].as_array().into_iter().flatten().map(|s| json!({ "name": s["name"], "result": s["result"], "start": s["start"], "finish": s["finish"] })).collect::<Vec<_>>() })).collect::<Vec<_>>() }) })
        }
        if began.elapsed().as_secs() > follow_s() { return Err(format!("Still not over after six hours: `superci runs retrieve {run}`.")) }
    }
}

fn get(plane: &str, key: &str, at: &str) -> Result<Value> {
    let r = Request::new("GET", &format!("{plane}{at}")).with_header("authorization", &format!("Bearer {key}"));
    let res = block_on(crate::aws::Blocking::new().send(r))?;
    // Not found: the run (an answer of the plane's own, with no words), or an artifact of it (said in words).
    if res.status == 404 {
        let t = res.body_text().trim().to_string();
        return Err(if t.is_empty() || t.starts_with(['{', '<']) || t.contains("nothing here") { "No such run among this control plane's runs from a computer (their ids are in `superci runs list`).".into() } else { format!("{}{}.", &t[..1].to_uppercase(), &t[1..]) })
    }
    if res.status >= 300 { return Err(format!("the control plane said {}: {}", res.status, res.body_text().trim())) }
    serde_json::from_slice(&res.body).map_err(|e| format!("the control plane's answer: {e}"))
}

/// `superci artifacts list --run=ID`: what a run from a computer's jobs kept with `upload-artifact`. (The run's id and
/// the artifact's name are checked by the command, before it asks.)
pub fn artifacts(plane: &str, key: &str, run: &str) -> Result<Done> {
    let got = get(plane, key, &format!("/runs/{run}/artifacts"))?;
    let all = got["artifacts"].as_array().cloned().unwrap_or_default();
    let said = if all.is_empty() { vec!["No artifacts: its jobs kept none, or the run is over a day old.".into()] }
        else { all.iter().map(|a| format!("{}  {}", a["name"].as_str().unwrap_or_default(), crate::dashboard::size_words(a["size"].as_u64().unwrap_or(0)))).collect() };
    Ok(Done { said, data: json!({ "run_id": run, "artifacts": all }) })
}

/// `superci artifacts retrieve NAME --run=ID [--out=DIR]`: fetches one and unpacks it into DIR (`./NAME` unless
/// given), as `download-artifact` would. One that is not a zip is kept as the file it is.
pub fn artifact(plane: &str, key: &str, run: &str, name: &str, out: Option<&str>) -> Result<Done> {
    let at = format!("/runs/{run}/artifacts?name={}", url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>());
    let got = get(plane, key, &at)?;
    let dir = std::path::PathBuf::from(out.unwrap_or(name));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Its pieces, in order, into one file beside where it goes.
    let zip = dir.join(format!(".{}.superci-download", name.replace(['/', '\\'], "_")));
    let mut file = std::fs::File::create(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    let http = crate::aws::Blocking::new();
    for p in got["pieces"].as_array().into_iter().flatten() {
        let res = block_on(http.send(Request::new("GET", p["url"].as_str().unwrap_or_default())))?;
        if res.status >= 300 { let _ = std::fs::remove_file(&zip); return Err(format!("the bucket said {} for a piece of {name}", res.status)) }
        file.write_all(&res.body).map_err(|e| e.to_string())?;
    }
    drop(file);
    let files = match unzip(&zip, &dir) {
        Ok(n) => { let _ = std::fs::remove_file(&zip); n }
        Err(Unpack::NotZip) => { let kept = dir.join(name); std::fs::rename(&zip, &kept).map_err(|e| e.to_string())?; 1 }
        Err(Unpack::Bad(e)) => { let _ = std::fs::remove_file(&zip); return Err(format!("{name} could not be unpacked: {e}")) }
    };
    let size = got["size"].as_u64().unwrap_or(0);
    Ok(Done { said: vec![format!("{name} ({}, {files} file{}) is in {}", crate::dashboard::size_words(size), if files == 1 { "" } else { "s" }, dir.display())],
        data: json!({ "run_id": run, "name": name, "size": size, "files": files, "out": dir.to_string_lossy() }) })
}

enum Unpack { NotZip, Bad(String) }

/// Unpacks a zip (as `upload-artifact` makes them: stored or deflated, its names relative) into `dir`; how many files.
fn unzip(path: &Path, dir: &Path) -> std::result::Result<usize, Unpack> {
    use std::io::{Read, Seek, SeekFrom};
    let bad = |e: &str| Unpack::Bad(e.to_string());
    let mut f = std::fs::File::open(path).map_err(|e| Unpack::Bad(e.to_string()))?;
    let mut head = [0u8; 4];
    if f.read_exact(&mut head).is_err() || head != [0x50, 0x4b, 0x03, 0x04] { return Err(Unpack::NotZip) }
    // Its end record: in the last 64 KB (and 22 bytes), the last that says so. (A file just opened and read from is
    // measured and read within its length; were that ever refused, no end record is found.)
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let tail_len = len.min(65_557);
    let mut tail = vec![0u8; tail_len as usize];
    let tail = f.seek(SeekFrom::Start(len - tail_len)).and_then(|_| f.read_exact(&mut tail)).map(|_| tail).unwrap_or_default();
    let end = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]).ok_or(bad("no end record"))?;
    let u16at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as u64;
    let u32at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as u64;
    let (count, cd_size, cd_at) = (u16at(&tail, end + 10), u32at(&tail, end + 12), u32at(&tail, end + 16));
    if count == 0xffff || cd_at == 0xffff_ffff { return Err(bad("it is over 4 GB (zip64), which this does not unpack yet")) }
    let mut cd = vec![0u8; cd_size as usize];
    f.seek(SeekFrom::Start(cd_at)).and_then(|_| f.read_exact(&mut cd)).map_err(|e| Unpack::Bad(e.to_string()))?;
    let (mut at, mut files) = (0usize, 0usize);
    for _ in 0..count {
        if cd.len() < at + 46 || cd[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] { return Err(bad("a damaged directory")) }
        let (method, packed, size) = (u16at(&cd, at + 10), u32at(&cd, at + 20), u32at(&cd, at + 24));
        let (n, e, c, local) = (u16at(&cd, at + 28) as usize, u16at(&cd, at + 30) as usize, u16at(&cd, at + 32) as usize, u32at(&cd, at + 42));
        let name = String::from_utf8_lossy(&cd[at + 46..at + 46 + n]).replace('\\', "/");
        at += 46 + n + e + c;
        // Only names inside the folder.
        if name.starts_with('/') || name.split('/').any(|p| p == "..") || name.contains(':') { return Err(Unpack::Bad(format!("a name outside its folder: {name}"))) }
        let to = dir.join(&name);
        if name.ends_with('/') { std::fs::create_dir_all(&to).map_err(|e| Unpack::Bad(e.to_string()))?; continue }
        if packed == 0xffff_ffff || size == 0xffff_ffff { return Err(bad("a file over 4 GB (zip64), which this does not unpack yet")) }
        let mut lh = [0u8; 30];
        // (Past its header, its name and extra field: a seek forward, which a file takes past its end too.)
        f.seek(SeekFrom::Start(local)).and_then(|_| f.read_exact(&mut lh)).and_then(|_| f.seek(SeekFrom::Current((u16at(&lh, 26) + u16at(&lh, 28)) as i64))).map_err(|e| Unpack::Bad(e.to_string()))?;
        let mut data = vec![0u8; packed as usize];
        f.read_exact(&mut data).map_err(|e| Unpack::Bad(e.to_string()))?;
        let data = match method {
            0 => data,
            8 => miniz_oxide::inflate::decompress_to_vec(&data).map_err(|e| Unpack::Bad(format!("{name}: {e:?}")))?,
            m => return Err(Unpack::Bad(format!("{name} is packed in a way this does not know ({m})"))),
        };
        // (A name joined to the folder: in a folder, that one or one inside it.)
        std::fs::create_dir_all(to.parent().unwrap_or(dir)).map_err(|e| Unpack::Bad(e.to_string()))?;
        std::fs::write(&to, data).map_err(|e| Unpack::Bad(format!("{}: {e}", to.display())))?;
        files += 1;
    }
    Ok(files)
}

/// A job's lines as they are printed: each step in turn, its lines in order, none twice and none missing.
#[derive(Default)]
struct Lines {
    /// What came and is not printed yet: each post's step, where it begins in that step's log, and its lines.
    held: Vec<(Option<String>, Option<u64>, Vec<String>)>,
    /// The last line printed of each step, and the steps whose lines are all out.
    shown: std::collections::HashMap<String, u64>,
    /// Where each step's printed lines stand in its log: the first line of the post they follow on from (a post says
    /// its first line's number) and how many were printed since. The runner leaves empty lines out of what it sends
    /// but counts them, so only the step's log says which line the last one printed was (`end_in`).
    anchor: std::collections::HashMap<String, (u64, u64)>,
    /// Lines of a step perhaps not had when later ones came (where the lines before stood, as `anchor`, and the
    /// last line before the post that came next), printed once its whole log comes, if they were not empty.
    missing: std::collections::HashMap<String, Vec<(u64, u64, u64)>>,
    done: std::collections::HashSet<String>,
    /// Steps known (from the run's status): no steps, no order to keep (an older control plane).
    steps: usize,
}

impl Lines {
    /// What to print now from the job's latest status (`j`: its new posts and its steps). `give_up`: the run is over
    /// and a step's log has not come: what is held is printed as it is. `fetch`: a step's whole log by its number.
    fn take(&mut self, j: &Value, give_up: bool, fetch: &dyn Fn(u64) -> Option<String>, pages: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        for p in j["posts"].as_array().into_iter().flatten() {
            let lines = p["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
            self.held.push((p["step"].as_str().map(str::to_string), p["start"].as_u64(), lines));
        }
        // An older control plane: its lines as they come.
        if j["posts"].is_null() { return j["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect() }
        let mut steps: Vec<&Value> = j["steps"].as_array().into_iter().flatten().collect();
        steps.sort_by_key(|s| s["order"].as_u64().unwrap_or(0));
        self.steps = steps.len();
        let mut out = vec![];
        // (A step is over once one after it has begun, whatever its own record says.)
        let begun: Vec<bool> = steps.iter().map(|s| s["state"].as_str().is_some_and(|st| st != "pending")).collect();
        for (at, s) in steps.iter().enumerate() {
            let Some(id) = s["id"].as_str() else { continue };
            let s = *s;
            if self.done.contains(id) { continue }
            let shown = self.shown.entry(id.to_string()).or_insert(0);
            let anchor = self.anchor.entry(id.to_string()).or_insert((1, 0));
            // What was sent of it as it went, while it follows on from what is out; past what may be a gap (the
            // runner drops lines that come faster than it sends them, and empty lines always), what its machine sent
            // of the step's pages fills it, and on.
            let mut rest = vec![];
            let mut filled = false;
            for (step, start, lines) in self.held.drain(..) {
                if step.as_deref() != Some(id) { rest.push((step, start, lines)); continue }
                if let (Some(st), false) = (start, filled) {
                    if st > *shown + 1 {
                        filled = true;
                        if let Some(text) = pages(id) {
                            let all: Vec<&str> = text.lines().collect();
                            let from = end_in(&all, *anchor) as usize;
                            for l in all.iter().skip(from) { out.push(without_time(l).to_string()) }
                            if all.len() > from { *shown = all.len() as u64; *anchor = (*shown + 1, 0) }
                        }
                    }
                }
                match start {
                    Some(start) if start <= *shown + 1 => for (k, line) in lines.into_iter().enumerate() { if start + k as u64 > *shown { out.push(line); *shown = start + k as u64; anchor.1 += 1 } },
                    // Perhaps a gap (the runner keeps the last of a page until it is full or the step ends), perhaps
                    // only empty lines left out: on with what comes; what was missing, if anything, follows once the
                    // step's whole log is here.
                    Some(start) => {
                        self.missing.entry(id.to_string()).or_default().push((anchor.0, anchor.1, start - 1));
                        *shown = start + lines.len() as u64 - 1;
                        *anchor = (start, lines.len() as u64);
                        out.extend(lines);
                    }
                    None => { *shown += lines.len() as u64; anchor.1 += lines.len() as u64; out.extend(lines) }
                }
            }
            self.held = rest;
            // Over: the rest from its whole log, then the next step. (The run over and its log long in coming: what
            // came of it as it went, as it is.)
            let ended = s["state"] == "completed" || begun[at + 1..].iter().any(|b| *b) || give_up;
            if !ended { return out }
            match s["log"].as_u64().and_then(fetch) {
                Some(text) => {
                    let all: Vec<&str> = text.lines().collect();
                    for (start, printed, b) in self.missing.remove(id).unwrap_or_default() {
                        // From the line after the last one printed, without the empty lines at either end.
                        let mut a = end_in(&all, (start, printed)) + 1;
                        let mut b = b.min(all.len() as u64);
                        while a <= b && blank(all[a as usize - 1]) { a += 1 }
                        while b >= a && blank(all[b as usize - 1]) { b -= 1 }
                        if a > b { continue }
                        out.push(format!("··· {} of this step, which came late:", which(a, b)));
                        out.extend(all[a as usize - 1..b as usize].iter().map(|l| without_time(l).to_string()));
                    }
                    let anchor = self.anchor.get(id).copied().unwrap_or((1, 0));
                    out.extend(all.iter().skip(end_in(&all, anchor) as usize).map(|l| without_time(l).to_string()));
                }
                // (What came of it as it went is out already: taken above.)
                None if give_up => {}
                None => return out,
            }
            self.done.insert(id.to_string());
        }
        // (No steps known yet: what came, as it came.)
        if steps.is_empty() { out.extend(self.held.drain(..).flat_map(|(_, _, lines)| lines)) }
        out
    }

    /// Whether every step's lines are out.
    fn complete(&self) -> bool { self.steps == 0 || self.done.len() >= self.steps }
}

/// A failed step's last lines: its last 40, or where several processes wrote at once (`[slot 2] …`), each one's own
/// last lines (the last 40 would be the last one to end's), in the order each began.
fn last_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(without_time).collect();
    let mut marks: Vec<&str> = vec![];
    for l in &lines { let m = crate::test_results::marked(l).0; if !m.is_empty() && !marks.contains(&m) { marks.push(m) } }
    if marks.len() < 2 { return lines[lines.len().saturating_sub(40)..].iter().map(|l| l.to_string()).collect() }
    let each = (40 / marks.len()).max(8);
    marks.iter().flat_map(|m| {
        let own: Vec<&&str> = lines.iter().filter(|l| crate::test_results::marked(l).0 == *m).collect();
        own[own.len().saturating_sub(each)..].iter().map(|l| l.to_string()).collect::<Vec<_>>()
    }).collect()
}

/// A time in seconds as a person reads it: "42 s", "3m 5s".
/// A job's start in a line, from its record (see `dashboard::job_start`): " (its machine in 19.4 s: AWS gave a
/// machine 6.1 s, …)"; nothing when the control plane did not say.
fn start_line(record: &Value) -> String {
    if record.is_null() { return String::new() }
    let start = crate::dashboard::job_start(record);
    let steps: Vec<String> = start["steps"].as_array().into_iter().flatten().map(|s| format!("{} {:.1} s", s["what"].as_str().unwrap_or_default(), s["ms"].as_u64().unwrap_or(0) as f64 / 1000.0)).collect();
    match start["ms"].as_u64() { Some(ms) if !steps.is_empty() => format!(" (started in {:.1} s: {})", ms as f64 / 1000.0, steps.join(", ")), _ => String::new() }
}

fn span(s: u64) -> String { if s < 60 { format!("{s} s") } else { format!("{}m {}s", s / 60, s % 60) } }

/// The number of the last line printed of a step, in its log (`all`): `printed` lines on from line `start`, the
/// empty ones not counted (the runner does not send them). As many lines on when the log has fewer.
fn end_in(all: &[&str], (start, printed): (u64, u64)) -> u64 {
    let mut seen = 0;
    if printed == 0 { return start - 1 }
    for (k, l) in all.iter().enumerate().skip(start as usize - 1) {
        if !blank(l) { seen += 1; if seen == printed { return k as u64 + 1 } }
    }
    start + printed - 1
}

/// A log's line with nothing after its time.
fn blank(l: &str) -> bool { without_time(&format!("{} ", l.trim_end())).trim().is_empty() }

/// Lines of a step by their numbers: "line 4", "lines 7–8".
fn which(a: u64, b: u64) -> String { if a == b { format!("line {a}") } else { format!("lines {a}–{b}") } }

/// A log's line without the time the runner put before it ("2026-10-14T21:09:53.1234567Z ").
fn without_time(line: &str) -> &str {
    // (A log's first line begins with a byte order mark.)
    let line = line.trim_start_matches('\u{feff}');
    let b = line.as_bytes();
    if b.len() > 20 && b[4] == b'-' && b[10] == b'T' && b[..4].iter().all(u8::is_ascii_digit) {
        if let Some(at) = line.find("Z ") { if at < 40 { return &line[at + 2..] } }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_steps_last_lines_are_each_slots_own() {
        let mut text = String::new();
        for i in 0..60 { text.push_str(&format!("2026-10-17T10:00:00.0000000Z [slot {}] line {i}\n", if i < 10 { 1 } else { 2 })) }
        let got = last_lines(&text);
        assert!(got.contains(&"[slot 1] line 9".to_string()) && got.contains(&"[slot 2] line 59".to_string()), "{got:?}");
        assert_eq!(got.iter().filter(|l| l.starts_with("[slot 1]")).count(), 10, "slot 1's all ten, though slot 2 wrote after it");
        let plain: String = (0..50).map(|i| format!("line {i}\n")).collect();
        assert_eq!(last_lines(&plain).len(), 40);
    }


    #[test]
    fn a_zip_as_upload_artifact_makes_it_is_unpacked_inside_its_folder() {
        let dir = std::env::temp_dir().join(format!("superci-unzip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/run")).unwrap();
        std::fs::write(dir.join("src/run/gaps.json"), "{\"gaps\":[1,2,3]}".repeat(50)).unwrap();
        std::fs::write(dir.join("src/lcov.info"), "TN:\nSF:a.rs\nend_of_record\n").unwrap();
        // The system's zip, deflated (as upload-artifact makes them).
        let ok = Command::new("zip").current_dir(dir.join("src")).args(["-q", "-r", "../a.zip", "."]).status();
        if !ok.is_ok_and(|s| s.success()) { eprintln!("no zip here"); return }
        assert_eq!(unzip(&dir.join("a.zip"), &dir.join("out")).ok(), Some(2));
        assert_eq!(std::fs::read_to_string(dir.join("out/run/gaps.json")).unwrap(), "{\"gaps\":[1,2,3]}".repeat(50));
        assert_eq!(std::fs::read_to_string(dir.join("out/lcov.info")).unwrap(), "TN:\nSF:a.rs\nend_of_record\n");
        std::fs::write(dir.join("plain.txt"), "not a zip").unwrap();
        assert!(matches!(unzip(&dir.join("plain.txt"), &dir.join("out2")), Err(Unpack::NotZip)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_steps_lines_are_printed_whole_and_in_order_what_the_runner_dropped_from_its_log() {
        let mut l = Lines::default();
        let step = |id: &str, order: u64, state: &str, log: Option<u64>| json!({ "id": id, "order": order, "state": state, "log": log });
        let post = |s: &str, start: u64, lines: &[&str]| json!({ "step": s, "start": start, "lines": lines });
        let logs = |n: u64| Some(match n { 1 => "2026-10-14T21:09:53.1234567Z a1\n2026-10-14T21:09:53.1234567Z a2\n2026-10-14T21:09:53.1234567Z a3\n2026-10-14T21:09:54.0000000Z a4".to_string(), _ => "b1\nb2".to_string() });
        let none = |_: u64| None;
        // Step a sends line 1, then 4 (2 and 3 were dropped); b's first line comes before a's log does.
        let j = json!({ "posts": [post("a", 1, &["a1"]), post("a", 4, &["a4"]), post("b", 1, &["b1"])], "steps": [step("a", 1, "inProgress", None), step("b", 2, "pending", None)] });
        assert_eq!(l.take(&j, false, &none, &|_| None), ["a1", "a4"], "past a gap with no pages, on; b waits for a");
        let j = json!({ "posts": [], "steps": [step("a", 1, "completed", Some(1)), step("b", 2, "inProgress", None)] });
        assert_eq!(l.take(&j, false, &logs, &|_| None), ["··· lines 2–3 of this step, which came late:", "a2", "a3", "b1"], "what a missed, from its log, its time taken off; then b");
        let j = json!({ "posts": [post("b", 2, &["b2"])], "steps": [step("a", 1, "completed", Some(1)), step("b", 2, "completed", Some(2))] });
        assert_eq!(l.take(&j, false, &logs, &|_| None), ["b2"]);
        assert!(l.complete());
        // An older control plane: its lines as they come.
        assert_eq!(Lines::default().take(&json!({ "lines": ["x", "y"] }), false, &none, &|_| None), ["x", "y"]);
        // Past a gap, its pages so far fill it, and the step's lines go on as they come.
        let mut l = Lines::default();
        let pages = |s: &str| (s == "a").then(|| "2026-10-15T11:00:49.1Z a1\n2026-10-15T11:00:49.2Z a2\n2026-10-15T11:00:49.3Z a3\n".to_string());
        let j = json!({ "posts": [post("a", 1, &["a1"]), post("a", 5, &["a5"])], "steps": [step("a", 1, "inProgress", None)] });
        assert_eq!(l.take(&j, false, &none, &pages), ["a1", "a2", "a3", "a5"], "a gap filled from the pages as far as they go");
        let j = json!({ "posts": [post("a", 4, &["a4", "a5", "a6"])], "steps": [step("a", 1, "inProgress", None)] });
        assert_eq!(l.take(&j, false, &none, &pages), ["a6"], "and on, none twice");
        // A gap the pages do not reach yet: said, and on; its lines once the step's whole log is here.
        let j = json!({ "posts": [post("a", 9, &["a9"])], "steps": [step("a", 1, "inProgress", None)] });
        assert_eq!(l.take(&j, false, &none, &pages), ["a9"]);
        let whole = |_: u64| Some((1..=10).map(|n| format!("2026-10-15T11:00:49.1Z a{n}")).collect::<Vec<_>>().join("\n"));
        let j = json!({ "posts": [], "steps": [step("a", 1, "completed", Some(1))] });
        assert_eq!(l.take(&j, false, &whole, &pages), ["··· line 4 of this step, which came late:", "a4", "··· lines 7–8 of this step, which came late:", "a7", "a8", "a10"]);
        // The runner leaves empty lines out of a post but counts them: no line twice, none said missing, inside a
        // post or between two (lines 3 and 5 empty), and the rest from the log after the last one printed.
        let mut l = Lines::default();
        let log = |_: u64| Some(["a1", "a2", "", "a4", "", "a6", "a7", "", "a9"].iter().map(|t| format!("2026-10-16T19:55:31.2107257Z {t}")).collect::<Vec<_>>().join("\n"));
        let j = json!({ "posts": [post("a", 1, &["a1", "a2", "a4"]), post("a", 6, &["a6"]), post("a", 7, &["a7"])], "steps": [step("a", 1, "completed", Some(1))] });
        assert_eq!(l.take(&j, false, &log, &|_| None), ["a1", "a2", "a4", "a6", "a7", "", "a9"]);
    }

    #[test]
    fn how_long_ago_is_said_in_minutes_hours_or_a_day_or_more() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let said: Vec<String> = [0, 1, 5, 61, 125, 1440].iter().map(|m| ago(now - m * 60_000 - 1_000)).collect();
        assert_eq!(said, ["just now", "a minute ago", "5 minutes ago", "an hour ago", "2 hours ago", "a day or more ago"]);
    }

    #[test]
    fn lines_with_no_steps_known_come_as_they_came_and_a_post_without_its_place_or_lines_follows_on() {
        let none = |_: u64| None;
        let mut l = Lines::default();
        let j = json!({ "posts": [{ "step": "a", "start": 1, "lines": ["x"] }, { "lines": ["y"] }], "steps": [] });
        assert_eq!(l.take(&j, false, &none, &|_| None), ["x", "y"]);
        // A post with no line in it, and one that does not say where it begins: on from what is out.
        let mut l = Lines::default();
        let j = json!({ "posts": [{ "step": "a", "start": 1, "lines": [] }, { "step": "a", "lines": ["a1", "a2"] }], "steps": [{ "id": "a", "order": 1, "state": "inProgress" }] });
        assert_eq!(l.take(&j, false, &none, &|_| None), ["a1", "a2"]);
        let j = json!({ "posts": [{ "step": "a", "start": 3, "lines": ["a3"] }], "steps": [{ "id": "a", "order": 1, "state": "inProgress" }] });
        assert_eq!(l.take(&j, false, &none, &|_| None), ["a3"], "none twice, none said missing");
    }

    #[test]
    fn an_empty_failed_steps_last_lines_are_none() {
        assert!(last_lines("").is_empty());
    }

    /// A zip as `upload-artifact` makes one: each entry's name, how it is packed (0 stored, 8 deflated) and its bytes
    /// as they are in the zip.
    fn zip(entries: &[(&str, u16, &[u8])]) -> (Vec<u8>, Vec<usize>) {
        let (mut out, mut dir, mut at) = (vec![], vec![], vec![]);
        for (name, method, data) in entries {
            at.push(out.len());
            let (n, size) = (name.len() as u16, data.len() as u32);
            out.extend([0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0]); out.extend(method.to_le_bytes()); out.extend([0u8; 8]);
            out.extend(size.to_le_bytes()); out.extend(size.to_le_bytes()); out.extend(n.to_le_bytes()); out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes()); out.extend(*data);
            dir.extend([0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0]); dir.extend(method.to_le_bytes()); dir.extend([0u8; 8]);
            dir.extend(size.to_le_bytes()); dir.extend(size.to_le_bytes()); dir.extend(n.to_le_bytes());
            dir.extend([0u8; 12]); dir.extend((*at.last().unwrap() as u32).to_le_bytes()); dir.extend(name.as_bytes());
        }
        let (cd_at, cd_len, count) = (out.len() as u32, dir.len() as u32, entries.len() as u16);
        out.extend(dir);
        out.extend([0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        out.extend(count.to_le_bytes()); out.extend(count.to_le_bytes()); out.extend(cd_len.to_le_bytes()); out.extend(cd_at.to_le_bytes()); out.extend([0, 0]);
        (out, at)
    }

    #[test]
    fn a_damaged_zip_is_refused_saying_what_is_wrong_and_an_empty_one_unpacks_to_nothing() {
        let dir = std::env::temp_dir().join(format!("superci-unzip-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let unpacked = |bytes: &[u8], name: &str| -> std::result::Result<usize, String> {
            let (file, out) = (dir.join(format!("{name}.zip")), dir.join(name));
            std::fs::write(&file, bytes).unwrap();
            unzip(&file, &out).map_err(|e| match e { Unpack::Bad(e) => e, Unpack::NotZip => "not a zip".into() })
        };
        // Nothing in it: only a header's first bytes, then its end record.
        let mut empty = vec![0x50, 0x4b, 0x03, 0x04];
        empty.extend(zip(&[]).0);
        assert_eq!(unpacked(&empty, "empty"), Ok(0));
        // One gone before it is opened: said as the system says it.
        assert!(matches!(unzip(&dir.join("gone.zip"), &dir.join("gone")), Err(Unpack::Bad(e)) if !e.is_empty()));
        assert_eq!(unpacked(b"PK\x03\x04 and nothing that ends it", "endless"), Err("no end record".into()));
        // Its directory said to be past its end.
        let (mut z, _) = zip(&[("a.txt", 0, b"a")]);
        let n = z.len();
        z[n - 6..n - 2].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(unpacked(&z, "far-directory").is_err_and(|e| !e.is_empty()));
        // An entry whose own header is past the end, and one longer than the file.
        let (mut z, _) = zip(&[("a.txt", 0, b"a")]);
        let cd = u32::from_le_bytes(z[z.len() - 6..z.len() - 2].try_into().unwrap()) as usize;
        z[cd + 42..cd + 46].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(unpacked(&z, "far-entry").is_err_and(|e| !e.is_empty()));
        let (mut z, _) = zip(&[("a.txt", 0, b"a")]);
        z[cd + 20..cd + 24].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(unpacked(&z, "long-entry").is_err_and(|e| !e.is_empty()));
        // Deflated bytes that do not inflate.
        assert!(unpacked(&zip(&[("a.txt", 8, &[0xff; 16])]).0, "not-deflated").is_err_and(|e| e.starts_with("a.txt: ")));
        // A file where a folder goes, and a folder where a file goes.
        assert!(unpacked(&zip(&[("a", 0, b"a"), ("a/b", 0, b"b")]).0, "file-then-folder").is_err());
        assert!(unpacked(&zip(&[("a", 0, b"a"), ("a/", 0, b"")]).0, "file-then-dir").is_err());
        assert!(unpacked(&zip(&[("d/", 0, b""), ("d", 0, b"d")]).0, "dir-then-file").is_err_and(|e| e.contains("/d: ")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_control_plane_before_runs_from_a_computer_is_said_to_need_updating() {
        assert!(new_enough(Some("0.16.9")).unwrap_err().contains("superci planes update"));
        assert!(new_enough(Some("0.18.0")).is_ok() && new_enough(Some("0.19.2")).is_ok() && new_enough(None).is_ok());
    }
}
