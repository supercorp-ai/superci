//! The cache: GitHub's Actions cache for jobs on SuperCI's machines, kept in a bucket of the control plane's own cloud
//! account, in the region the machine runs in. Each machine runs `superci-cache` (the `cache` crate), which speaks
//! GitHub's cache protocol to the job's actions and holds no keys: it asks the control plane, with the job's own
//! token, and the control plane decides here and signs links to the bucket that do one thing each.
//!
//! What a job may read and write is what GitHub says in its token (`ac`: the branches whose caches it may read, in
//! the order to look, and the one it may write): a pull request reads its own branch, then its base, then the
//! default branch, and writes only its own. A run from a computer gets a token from the control plane in the same
//! form (see `local_access`): it reads its branch's and the default branch's caches, and writes only its own scope.
//!
//! Entries are kept by repository (`ns`), scope, version (GitHub's hash of the paths and compression) and key, never
//! changed once written. What is kept (see `Settings`): each repository so much at most, the least recently used
//! going first; an entry not used for so many days goes; and one saved `MAX_AGE_DAYS` ago is taken as gone (the next
//! job saves it anew), so the bucket's own rule, which drops anything older than `BUCKET_DAYS`, never takes one still
//! in use from under the index.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::io::{get_json, Result, Store};

/// What a repository keeps at most unless set (GitHub keeps 10 GB; the bucket is your own, at about $0.023 a GB a month).
pub const SIZE_GB: u32 = 25;
/// An entry not used for this many days goes, unless set (as GitHub).
pub const UNUSED_DAYS: u32 = 7;
/// An entry saved this many days ago is taken as gone, whatever its use: the next job saves it anew.
pub const MAX_AGE_DAYS: u32 = 30;
/// The bucket's own rule: any object older than this goes, whatever the control plane's index says.
pub const BUCKET_DAYS: u32 = MAX_AGE_DAYS + 2;

/// When a saved entry's record goes by itself (eviction takes it sooner): as the bucket removes its pieces.
fn entry_end(e: &Entry) -> u64 { e.made + BUCKET_DAYS as u64 * 86_400_000 }

/// The cache as set (Settings → Cache, `superci caches update`): on unless `off`; `size_gb` a repository keeps at
/// most; `days` an entry may go unused (none: the defaults); `images_off`, Docker's images not kept (they are, on
/// Ubuntu 26.04 machines, in the bucket's layer store: see `layers`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub off: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub size_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub days: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub images_off: bool,
}

impl Settings {
    pub fn is_default(&self) -> bool { *self == Settings::default() }
    pub fn size_gb(&self) -> u32 { self.size_gb.unwrap_or(SIZE_GB) }
    pub fn days(&self) -> u32 { self.days.unwrap_or(UNUSED_DAYS).min(MAX_AGE_DAYS) }
    pub fn limit_bytes(&self) -> u64 { self.size_gb() as u64 * 1024 * 1024 * 1024 }
    pub fn unused_ms(&self) -> u64 { self.days() as u64 * 86_400_000 }
}

/// Whether an entry is too old to be used (see `MAX_AGE_DAYS`).
pub fn too_old(e: &Entry, now: u64) -> bool { now.saturating_sub(e.made) > MAX_AGE_DAYS as u64 * 86_400_000 }
/// An entry begun and not finished within this goes (its job stopped or failed while saving).
pub const UNFINISHED_MS: u64 = 3_600_000;
/// An entry begun and not finished within this may be begun again by another job (until then the key is being
/// saved, as GitHub says it: BuildKit then takes it as there, and a job left saving would keep it from everyone).
pub const TAKEOVER_MS: u64 = 600_000;
/// How long a signed link to the bucket works.
pub const LINK_SECONDS: u64 = 3600;
/// Pieces of an entry at most (a piece is one upload of GitHub's cache client: 32 MB or so; 10 GB in 128 MB pieces).
pub const MOST_PIECES: usize = 2000;

/// The bucket for a control plane's cache in a region.
pub fn bucket(plane_id: &str, region: &str) -> String { format!("superci-plane-{plane_id}-{region}") }

/// Where `superci-cache` for an architecture (`x86_64`, `aarch64`, `x86_64-windows`) of this version is in each bucket.
pub fn program_key(arch: &str) -> String { format!("superci/cache-{}-{arch}", crate::plane::VERSION) }

/// Where `superci-layers` for an architecture (`x86_64`, `aarch64`) of this version is in each bucket.
pub fn layers_key(arch: &str) -> String { format!("superci/layers-{}-{arch}", crate::plane::VERSION) }

/// The disk a machine keeps the layer store's files on (GB, MiB a second): beside its own, only when the store is on.
pub const STORE_DISK: (u32, u32) = (20, 500);

/// A file in the bucket (one of a layer's in the layer store, see `layers`): its key, size and digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredFile {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub digest: Option<String>,
}

/// One scope: a branch's caches (GitHub's `refs/heads/main`, `refs/pull/5/merge`), and whether the job may write there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scope { pub name: String, pub write: bool }

/// What a job may do in the cache: its repository's entries in these scopes, read in this order; and, for a run from
/// a computer, which run's artifacts are its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Access { pub ns: String, pub scopes: Vec<Scope>, pub run: Option<String>,
    /// The job of a run from a computer the token is for (its id), which an artifact it keeps says it was kept by.
    pub job: Option<String> }

impl Access {
    fn writes_to(&self) -> Option<&Scope> { self.scopes.iter().find(|s| s.write) }
}

/// GitHub's `ac` claim (a JSON list in a string: `[{"Scope":"refs/heads/main","Permission":3}]`, 1 read, 2 write).
fn scopes(ac: &str) -> Result<Vec<Scope>> {
    let list: Vec<Value> = serde_json::from_str(ac).map_err(|e| format!("the token's cache scopes: {e}"))?;
    let scopes: Vec<Scope> = list.iter().filter_map(|s| {
        let (name, permission) = (s["Scope"].as_str()?, s["Permission"].as_u64()?);
        (permission & 1 == 1 || permission & 2 == 2).then(|| Scope { name: name.to_string(), write: permission & 2 == 2 })
    }).collect();
    if scopes.iter().any(|s| !plain(&s.name)) { return Err("a cache scope with characters SuperCI does not keep".into()) }
    Ok(scopes)
}

/// Names kept as they are in the index: no control characters, nothing that ends a key's part.
fn plain(s: &str) -> bool { !s.is_empty() && s.len() <= 512 && !s.chars().any(|c| c.is_control() || c == ':') }

/// From a job's runtime token as GitHub signed it (its claims, checked by the caller): its repository and scopes.
pub fn github_access(claims: &Value) -> Result<Access> {
    let repo = claims["repository_id"].as_str().map(str::to_string).or_else(|| claims["repository_id"].as_u64().map(|n| n.to_string())).ok_or("a token without its repository")?;
    if repo.is_empty() || !repo.bytes().all(|b| b.is_ascii_digit()) { return Err("a token without its repository".into()) }
    Ok(Access { ns: format!("gh{repo}"), scopes: scopes(claims["ac"].as_str().unwrap_or("[]"))?, run: None, job: None })
}

/// The claims of a token the control plane gives a run from a computer, so its jobs reach the cache as GitHub's do:
/// the repository's entries (`ns`: GitHub's repository id when it is known, as GitHub's own jobs keep them), its own
/// scope to write (`superci/<who>`: the key that started it, or `computer`), and its branch's and the default
/// branch's to read.
/// It also says its run and job in GitHub's form (`scp`), where GitHub's artifact client looks for them.
pub fn local_claims(ns: &str, who: &str, reference: &str, default_branch: &str, run: &str, job: &str) -> Value {
    let ac = [reference.to_string(), format!("refs/heads/{default_branch}")].into_iter().fold(vec![serde_json::json!({ "Scope": format!("refs/superci/{who}"), "Permission": 3 })], |mut ac, r| {
        if !r.is_empty() && plain(&r) && !ac.iter().any(|s| s["Scope"] == r.as_str()) { ac.push(serde_json::json!({ "Scope": r, "Permission": 1 })) }
        ac
    });
    serde_json::json!({ "ns": ns, "ac": serde_json::Value::Array(ac).to_string(), "scp": format!("Actions.Results:{run}:{job}"), "run": run })
}

/// From such a token's claims (checked by the caller).
pub fn local_access(claims: &Value) -> Result<Access> {
    let ns = claims["ns"].as_str().unwrap_or_default();
    if !(ns.starts_with("gh") || ns.starts_with("local-")) || !ns.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') { return Err("a token without its repository".into()) }
    let run = claims["run"].as_str().filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())).map(str::to_string);
    let job = claims["scp"].as_str().and_then(|s| s.rsplit(':').next()).filter(|j| !j.is_empty() && j.bytes().all(|b| b.is_ascii_digit())).map(str::to_string);
    Ok(Access { ns: ns.to_string(), scopes: scopes(claims["ac"].as_str().unwrap_or("[]"))?, run, job })
}

/// A repository's entries for a run from a computer whose repository GitHub does not know (no App there).
pub fn local_ns(repository: &str) -> String { format!("local-{}", &crate::crypto::sha256_hex(repository.as_bytes())[..20]) }

/// One entry: its pieces in the bucket of `region` (named in order, with their sizes), when it was made and last
/// used, and whether it was finished (until then it is being saved and is no hit).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub region: String,
    #[serde(default)] pub pieces: Vec<(String, u64)>,
    #[serde(default)] pub size: u64,
    pub made: u64,
    #[serde(default)] pub used: u64,
    #[serde(default)] pub done: bool,
    /// What a tool kept beside its artifact (Turbo: its signature and how long the task took), given back with it.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub meta: Option<String>,
    /// An artifact's: the job that keeps it (its id, and its name in the run).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub by: Option<(String, String)>,
}

impl Entry {
    /// Where its pieces are in the bucket: the repository's, then the entry's own.
    pub fn prefix(&self, ns: &str) -> String { format!("c/{ns}/{}", self.id) }
}

fn index(ns: &str, scope: &str, version: &str, key: &str) -> String { format!("cache:{ns}:{scope}:{version}:{key}") }

/// A key as GitHub takes it: at most 512 characters, no comma (a colon is fine: BuildKit's have them; the key is the
/// index's last part).
fn key_ok(key: &str) -> bool { !key.is_empty() && key.len() <= 512 && !key.chars().any(|c| c.is_control() || c == ',') }

/// A key and version as GitHub's client sends them (a version is a SHA-256 in hex).
fn check(key: &str, version: &str) -> Result<()> {
    if !key_ok(key) { return Err("a cache key GitHub would not take (empty, too long, or with a comma)".into()) }
    if version.is_empty() || version.len() > 128 || !version.bytes().all(|b| b.is_ascii_alphanumeric()) { return Err("a cache version SuperCI does not keep".into()) }
    Ok(())
}

/// A hit: the key that matched, its entry, and where it is in the index.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit { pub key: String, pub entry: Entry, pub index: String }

/// GitHub's order: in each scope in turn, the key itself, then each restore key as the start of a key (the newest
/// such entry), before the next scope. Only finished entries of the same version count.
pub async fn lookup(store: &dyn Store, access: &Access, key: &str, restore: &[String], version: &str, now: u64) -> Result<Option<Hit>> {
    check(key, version)?;
    for scope in &access.scopes {
        let at = index(&access.ns, &scope.name, version, key);
        if let Some(e) = get_json::<Entry>(store, &at).await?.filter(|e| e.done && !too_old(e, now)) { return Ok(Some(Hit { key: key.to_string(), entry: e, index: at })) }
        for prefix in restore.iter().filter(|r| key_ok(r)) {
            let start = index(&access.ns, &scope.name, version, prefix);
            let newest = store.list(&start).await?.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_str::<Entry>(&v).ok()?))).filter(|(_, e)| e.done && !too_old(e, now)).max_by_key(|(_, e)| e.made);
            if let Some((at, e)) = newest {
                // (The first such mark: scopes and the repository have no colon; a key may.)
                let found = at.split_once(&format!(":{version}:")).map(|(_, k)| k.to_string()).unwrap_or_default();
                return Ok(Some(Hit { key: found, entry: e, index: at }))
            }
        }
    }
    Ok(None)
}

/// Marks a hit used (at most once an hour: a busy entry is not written at every restore).
pub async fn used(store: &dyn Store, hit: &Hit, now: u64) -> Result<()> {
    if now.saturating_sub(hit.entry.used) < 3_600_000 { return Ok(()) }
    let was = crate::io::json(&hit.entry);
    let mut e = hit.entry.clone();
    e.used = now;
    // Another restore at the same moment wrote it already: as good.
    store.swap_until(&hit.index, Some(&was), crate::io::json(&e), Some(entry_end(&e))).await.map(|_| ())
}

/// Why an entry could not be begun.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused { Exists, NoWrite, Bad(String) }

/// Begins an entry in the job's own scope, in the bucket of `region`; what GitHub's client then uploads. An entry of
/// that key and version that exists, or is being saved, is not begun again (GitHub's entries never change); one begun
/// `TAKEOVER_MS` ago and never finished is.
pub async fn begin(store: &dyn Store, access: &Access, key: &str, version: &str, region: &str, now: u64) -> std::result::Result<(String, Entry), Refused> {
    check(key, version).map_err(Refused::Bad)?;
    let scope = access.writes_to().ok_or(Refused::NoWrite)?;
    let at = index(&access.ns, &scope.name, version, key);
    let entry = Entry { id: crate::crypto::random_id(20).to_ascii_lowercase(), region: region.to_string(), made: now, used: now, ..Default::default() };
    let text = crate::io::json(&entry);
    if store.put_if_absent(&at, text.clone()).await.map_err(Refused::Bad)? { return Ok((at, entry)) }
    let was = store.get(&at).await.map_err(Refused::Bad)?;
    let stale = was.as_deref().and_then(|v| serde_json::from_str::<Entry>(v).ok()).is_some_and(|e| if e.done { too_old(&e, now) } else { now.saturating_sub(e.made) > TAKEOVER_MS });
    if stale && store.swap(&at, was.as_deref(), text).await.map_err(Refused::Bad)? { return Ok((at, entry)) }
    Err(Refused::Exists)
}

/// The entry being saved for this key and version in the job's own scope.
pub async fn begun(store: &dyn Store, access: &Access, key: &str, version: &str) -> Result<Option<(String, Entry)>> {
    check(key, version)?;
    let Some(scope) = access.writes_to() else { return Ok(None) };
    let at = index(&access.ns, &scope.name, version, key);
    Ok(get_json::<Entry>(store, &at).await?.filter(|e| !e.done).map(|e| (at, e)))
}

/// A piece's name as `superci-cache` gives it (from the block id GitHub's client gave it): plain, short.
pub fn piece_name(name: &str) -> bool { !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') }

/// Finishes an entry being saved: its pieces, in order. From now on it is a hit.
pub async fn finish(store: &dyn Store, at: &str, entry: &Entry, pieces: Vec<(String, u64)>) -> Result<Entry> { finish_with(store, at, entry, pieces, None).await }

/// The same, with what a tool keeps beside it.
pub async fn finish_with(store: &dyn Store, at: &str, entry: &Entry, pieces: Vec<(String, u64)>, meta: Option<String>) -> Result<Entry> {
    if pieces.is_empty() || pieces.len() > MOST_PIECES || !pieces.iter().all(|(n, _)| piece_name(n)) { return Err("pieces SuperCI does not keep".into()) }
    let was = crate::io::json(entry);
    let mut e = entry.clone();
    e.size = pieces.iter().map(|(_, s)| s).sum();
    e.pieces = pieces;
    e.done = true;
    e.meta = meta.filter(|m| m.len() <= 2048);
    if !store.swap_until(at, Some(&was), crate::io::json(&e), Some(entry_end(&e))).await? { return Err("the entry changed while it was saved".into()) }
    Ok(e)
}

/// What goes from the bucket: entries over their repository's limit (least recently used first), not used for
/// as long as set, saved `MAX_AGE_DAYS` ago, or begun and never finished. They are taken out of the index here; their
/// pieces are for the caller to delete (region, object names).
pub async fn evict(store: &dyn Store, now: u64, settings: &Settings) -> Result<Vec<(String, Vec<String>)>> {
    evict_within(store, now, settings.limit_bytes(), settings.unused_ms()).await
}

async fn evict_within(store: &dyn Store, now: u64, limit: u64, unused: u64) -> Result<Vec<(String, Vec<String>)>> {
    let all: Vec<(String, Entry)> = store.list("cache:").await?.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_str::<Entry>(&v).ok()?))).collect();
    let mut by_ns: std::collections::BTreeMap<String, Vec<(String, Entry)>> = Default::default();
    for (k, e) in all { by_ns.entry(k.splitn(3, ':').nth(1).unwrap_or_default().to_string()).or_default().push((k, e)) }
    let mut gone = vec![];
    for (ns, mut entries) in by_ns {
        entries.sort_by_key(|(_, e)| std::cmp::Reverse(e.used.max(e.made)));
        let (mut kept, mut entries) = (0u64, entries.into_iter());
        loop {
            let Some((k, e)) = entries.next() else { break };
            let old = if e.done { now.saturating_sub(e.used.max(e.made)) > unused || too_old(&e, now) } else { crate::io::before(UNFINISHED_MS, now.saturating_sub(e.made)) };
            if !old && (!e.done || kept + e.size <= limit) { kept += e.size; continue }
            store.delete(&k).await?;
            gone.push((e.region.clone(), e.pieces.iter().map(|(p, _)| format!("{}/{p}", e.prefix(&ns))).collect()));
        }
    }
    Ok(gone)
}

/// What a repository keeps now: entries and bytes.
pub async fn kept(store: &dyn Store, ns: &str) -> Result<(usize, u64)> {
    let entries: Vec<Entry> = store.list(&format!("cache:{ns}:")).await?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).filter(|e: &Entry| e.done).collect();
    Ok((entries.len(), entries.iter().map(|e| e.size).sum()))
}

/// One finished entry as it is listed: its repository, scope, key, version, and the entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed { pub ns: String, pub scope: String, pub key: String, pub version: String, pub entry: Entry }

/// Every finished entry (of one repository, or all), the most recently used first.
pub async fn list(store: &dyn Store, ns: Option<&str>) -> Result<Vec<Listed>> {
    let start = match ns { Some(n) => format!("cache:{n}:"), None => "cache:".to_string() };
    let mut all: Vec<Listed> = store.list(&start).await?.into_iter().filter_map(|(k, v)| {
        let entry: Entry = serde_json::from_str(&v).ok().filter(|e: &Entry| e.done)?;
        // (`cache:<repository>:<scope>:<version>:<key>`; one without all of them is passed over.)
        let mut p = k["cache:".len()..].splitn(4, ':');
        let ns = p.next().unwrap_or_default().to_string();
        let (scope, version, key) = (p.next()?.to_string(), p.next()?.to_string(), p.next()?.to_string());
        Some(Listed { ns, scope, key, version, entry })
    }).collect();
    all.sort_by_key(|l| std::cmp::Reverse(l.entry.used.max(l.entry.made)));
    Ok(all)
}

/// Takes entries of a repository out of the index (those with `key`, or all of them; in `scope` only, when given);
/// their pieces, by region, are for the caller to delete.
pub async fn remove(store: &dyn Store, ns: &str, key: Option<&str>, scope: Option<&str>) -> Result<(usize, Vec<(String, Vec<String>)>)> {
    let mut gone = vec![];
    let mut n = 0;
    for (k, v) in store.list(&format!("cache:{ns}:")).await? {
        let mut p = k.splitn(5, ':').skip(2);
        let (s, _, kk) = (p.next().unwrap_or_default(), p.next(), p.next().unwrap_or_default());
        if key.is_some_and(|want| want != kk) || scope.is_some_and(|want| want != s) { continue }
        store.delete(&k).await?;
        n += 1;
        if let Ok(e) = serde_json::from_str::<Entry>(&v) { gone.push((e.region.clone(), e.pieces.iter().map(|(p, _)| format!("{}/{p}", e.prefix(ns))).collect())) }
    }
    Ok((n, gone))
}

/// Artifacts of a run from a computer (it has no GitHub to keep them), kept in the bucket as cache entries are, by
/// run and name, for as long as the run is kept (a day). A name is used once in a run, as on GitHub.
fn artifact_index(run: &str, name: &str) -> String { format!("artifact:{run}:{name}") }

/// An artifact's name as GitHub allows it (none of `"<>|*?:\/`, no control characters).
pub fn artifact_name(name: &str) -> bool { plain(name) && name.len() <= 256 && !name.chars().any(|c| "\"<>|*?\\/".contains(c)) }

/// Where an artifact's pieces are: the run's, then the entry's own.
pub fn artifact_ns(run: &str) -> String { format!("run-{run}") }

/// A run's artifact begun by one of its jobs (`by`: its id and name). One of that name exists already: refused, as
/// GitHub refuses it in a run, unless an earlier run of the same job kept it (a job started again, after AWS took
/// back its spot machine, is a new attempt of it, which GitHub lets keep its artifacts anew): then this one takes
/// its place.
pub async fn begin_artifact(store: &dyn Store, run: &str, name: &str, region: &str, by: Option<(String, String)>, now: u64) -> std::result::Result<(String, Entry), Refused> {
    if !artifact_name(name) { return Err(Refused::Bad("an artifact's name GitHub would not take".into())) }
    let at = artifact_index(run, name);
    let entry = Entry { id: crate::crypto::random_id(20).to_ascii_lowercase(), region: region.to_string(), made: now, used: now, by: by.clone(), ..Default::default() };
    let text = crate::io::json(&entry);
    if store.put_if_absent(&at, text.clone()).await.map_err(Refused::Bad)? { return Ok((at, entry)) }
    let Some(was) = store.get(&at).await.map_err(Refused::Bad)? else { return Err(Refused::Exists) };
    let earlier = serde_json::from_str::<Entry>(&was).ok().and_then(|e| e.by);
    match (earlier, by) {
        (Some((id0, n0)), Some((id1, n1))) if n0 == n1 && id0 != id1 && store.swap(&at, Some(&was), text).await.map_err(Refused::Bad)? => Ok((at, entry)),
        _ => Err(Refused::Exists),
    }
}

/// A run's artifact by its name (`done`: only a finished one).
pub async fn artifact(store: &dyn Store, run: &str, name: &str, done: bool) -> Result<Option<(String, Entry)>> {
    let at = artifact_index(run, name);
    Ok(get_json::<Entry>(store, &at).await?.filter(|e| e.done == done).map(|e| (at, e)))
}

/// A run's finished artifacts, by name, the oldest first.
pub async fn artifacts(store: &dyn Store, run: &str) -> Result<Vec<(String, Entry)>> {
    let start = artifact_index(run, "");
    let mut all: Vec<(String, Entry)> = store.list(&start).await?.into_iter().filter_map(|(k, v)| Some((k[start.len()..].to_string(), serde_json::from_str::<Entry>(&v).ok()?))).filter(|(_, e)| e.done).collect();
    all.sort_by_key(|(_, e)| e.made);
    Ok(all)
}

/// An artifact's number, as GitHub numbers them (from its entry: the same each time it is asked).
pub fn artifact_id(e: &Entry) -> u64 { u64::from_str_radix(&crate::crypto::sha256_hex(e.id.as_bytes())[..12], 16).unwrap_or(1) }

/// A machine's link to the cache: the region it runs in, its runner's name (which job it is on, as the control plane
/// knows it), and until when the link works (its longest job).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine { pub region: String, pub arch: String, pub until: u64, #[serde(default)] pub runner: String,
    /// The record of the job it was started for (`job:…`): what it says after the job goes there (the runner's own
    /// record goes when the job ends).
    #[serde(default, skip_serializing_if = "String::is_empty")] pub job: String }

/// What a job may do in the cache when it asks without a token of GitHub's (the tools on its machine: Turbo, Nx,
/// Bazel), from what GitHub said of the job: its repository's entries, its branch's to write and read, then the
/// default branch's to read. A push to the default branch writes the default branch's; a pull request's branch only
/// its own (as GitHub's own token has it).
pub fn branch_access(repo_id: u64, branch: &str, default_branch: &str) -> Option<Access> {
    if branch.is_empty() { return None }
    ref_access(&format!("gh{repo_id}"), &format!("refs/heads/{branch}"), default_branch)
}

/// The same for any repository's entries: `git_ref` (a full ref) to write and read, then the default branch's to read.
pub fn ref_access(ns: &str, git_ref: &str, default_branch: &str) -> Option<Access> {
    if !plain(git_ref) || git_ref.ends_with('/') { return None }
    let mut scopes = vec![Scope { name: git_ref.to_string(), write: true }];
    let default = format!("refs/heads/{default_branch}");
    if !default_branch.is_empty() && default != git_ref && plain(&default) { scopes.push(Scope { name: default, write: false }) }
    Some(Access { ns: ns.to_string(), scopes, run: None, job: None })
}

/// A GitLab project's entries: by its connection (a GitLab of its own) and the project's id there.
pub fn gitlab_ns(connection: &str, project: u64) -> String { format!("gl{}x{project}", &crate::crypto::sha256_hex(connection.as_bytes())[..8]) }

pub async fn machine(store: &dyn Store, token: &str, now: u64) -> Result<Option<Machine>> {
    if token.len() < 16 || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { return Ok(None) }
    Ok(get_json::<Machine>(store, &format!("cachem:{token}")).await?.filter(|m| crate::io::before(now, m.until)))
}

pub async fn add_machine(store: &dyn Store, token: &str, m: &Machine) -> Result<()> {
    // (Kept a day past its longest job, then gone by itself.)
    store.put_until(&format!("cachem:{token}"), crate::io::json(m), m.until + 86_400_000).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// The store, in memory; `1`: the operation it does not answer (`get`, `swap`, …), and `2`: one it answers as
    /// though another request had removed the key just before (`get`), or written it (`swap`, lost).
    #[derive(Default)]
    struct Mem(RefCell<BTreeMap<String, String>>, RefCell<&'static str>, RefCell<&'static str>);
    impl Mem {
        fn failing(&self, op: &str) -> Result<()> { if *self.1.borrow() == op { Err(format!("the store did not answer ({op})")) } else { Ok(()) } }
    }
    #[async_trait::async_trait(?Send)]
    impl Store for Mem {
        async fn get(&self, k: &str) -> Result<Option<String>> { self.failing("get")?; if *self.2.borrow() == "get" { return Ok(None) } Ok(self.0.borrow().get(k).cloned()) }
        async fn put(&self, k: &str, v: String) -> Result<()> { self.0.borrow_mut().insert(k.into(), v); Ok(()) }
        async fn put_if_absent(&self, k: &str, v: String) -> Result<bool> { if self.0.borrow().contains_key(k) { return Ok(false) } self.0.borrow_mut().insert(k.into(), v); Ok(true) }
        async fn swap(&self, k: &str, was: Option<&str>, v: String) -> Result<bool> { self.failing("swap")?; if *self.2.borrow() == "swap" { return Ok(false) } crate::io::swap_in_two_steps(self, k, was, v).await }
        async fn delete(&self, k: &str) -> Result<()> { self.0.borrow_mut().remove(k); Ok(()) }
        async fn list(&self, p: &str) -> Result<Vec<(String, String)>> { self.failing("list")?; Ok(self.0.borrow().iter().filter(|(k, _)| k.starts_with(p)).map(|(k, v)| (k.clone(), v.clone())).collect()) }
    }
    fn run<T>(f: impl std::future::Future<Output = T>) -> T { futures::executor::block_on(f) }
    const V: &str = "0123abcd";

    #[test]
    fn entries_listed_and_removed_by_repository_key_and_scope() {
        let store = Mem::default();
        save(&store, &main_branch(), "deps-1", 1);
        save(&store, &main_branch(), "buildkit-blob-1-sha256:aa", 2);
        save(&store, &pr(), "deps-1", 3);
        let all = run(list(&store, Some("gh77"))).unwrap();
        assert_eq!(all.iter().map(|l| (l.scope.as_str(), l.key.as_str())).collect::<Vec<_>>(), [("refs/pull/5/merge", "deps-1"), ("refs/heads/main", "buildkit-blob-1-sha256:aa"), ("refs/heads/main", "deps-1")]);
        assert_eq!(run(list(&store, None)).unwrap().len(), 3);
        let (n, gone) = run(remove(&store, "gh77", Some("deps-1"), Some("refs/heads/main"))).unwrap();
        assert_eq!((n, gone[0].1.len()), (1, 2));
        assert_eq!(run(remove(&store, "gh77", None, None)).unwrap().0, 2);
        assert!(run(list(&store, None)).unwrap().is_empty());
        assert_eq!((Settings::default().size_gb(), Settings::default().days(), Settings { days: Some(90), ..Default::default() }.days()), (25, 7, MAX_AGE_DAYS));
    }

    #[test]
    fn a_runs_artifacts_by_name_once_each() {
        let store = Mem::default();
        let (at, e) = run(begin_artifact(&store, "41", "report", "us-east-1", None, 5)).unwrap();
        assert!(run(artifact(&store, "41", "report", true)).unwrap().is_none(), "not finished yet");
        assert_eq!(run(begin_artifact(&store, "41", "report", "us-east-1", None, 6)).unwrap_err(), Refused::Exists);
        run(finish(&store, &at, &e, vec![("p1".into(), 9)])).unwrap();
        assert_eq!(run(artifacts(&store, "41")).unwrap().iter().map(|(n, e)| (n.as_str(), e.size)).collect::<Vec<_>>(), [("report", 9)]);
        assert!(run(artifacts(&store, "42")).unwrap().is_empty(), "another run's");
        assert!(matches!(run(begin_artifact(&store, "41", "a/b", "us-east-1", None, 7)), Err(Refused::Bad(_))));
        // A job started again (a new attempt of it, after AWS took back its spot machine) keeps its artifact anew; another
        // job of the run, or the same attempt again, is refused as GitHub refuses it.
        let by = |id: &str, name: &str| Some((id.to_string(), name.to_string()));
        let (_, first) = run(begin_artifact(&store, "41", "playwright-artifacts", "us-east-1", by("100", "tests"), 8)).unwrap();
        assert_eq!(run(begin_artifact(&store, "41", "playwright-artifacts", "us-east-1", by("100", "tests"), 9)).unwrap_err(), Refused::Exists);
        assert_eq!(run(begin_artifact(&store, "41", "playwright-artifacts", "us-east-1", by("101", "lint"), 9)).unwrap_err(), Refused::Exists);
        let (_, again) = run(begin_artifact(&store, "41", "playwright-artifacts", "us-east-1", by("102", "tests"), 10)).unwrap();
        assert_ne!(first.id, again.id);
        assert_eq!(run(begin_artifact(&store, "41", "playwright-artifacts", "us-east-1", None, 11)).unwrap_err(), Refused::Exists);
        let a = local_access(&local_claims("gh1", "computer", "refs/heads/main", "main", "41", "9")).unwrap();
        assert_eq!(a.run.as_deref(), Some("41"));
        assert_eq!(local_claims("gh1", "computer", "refs/heads/main", "main", "41", "9")["scp"], "Actions.Results:41:9");
    }

    fn pr() -> Access { github_access(&serde_json::json!({ "repository_id": "77", "ac": r#"[{"Scope":"refs/pull/5/merge","Permission":3},{"Scope":"refs/heads/main","Permission":1}]"# })).unwrap() }
    fn main_branch() -> Access { github_access(&serde_json::json!({ "repository_id": "77", "ac": r#"[{"Scope":"refs/heads/main","Permission":3}]"# })).unwrap() }

    fn save(store: &Mem, access: &Access, key: &str, now: u64) -> Entry {
        let (at, e) = run(begin(store, access, key, V, "us-east-1", now)).unwrap();
        run(finish(store, &at, &e, vec![("p0".into(), 10), ("p1".into(), 5)])).unwrap()
    }

    #[test]
    fn scopes_as_githubs_token_says() {
        assert_eq!(pr().scopes, vec![Scope { name: "refs/pull/5/merge".into(), write: true }, Scope { name: "refs/heads/main".into(), write: false }]);
        assert_eq!(pr().ns, "gh77");
        // The token GitHub gave a job (recorded): its one scope, read and write.
        let recorded = serde_json::json!({ "ac": "[{\"Scope\":\"refs/heads/main\",\"Permission\":3}]", "acsl": "10", "repository_id": "1000001", "run_id": "1" });
        assert_eq!(github_access(&recorded).unwrap(), Access { ns: "gh1000001".into(), scopes: vec![Scope { name: "refs/heads/main".into(), write: true }], run: None, job: None });
        assert!(github_access(&serde_json::json!({ "ac": "[]" })).is_err());
        assert!(github_access(&serde_json::json!({ "repository_id": "1", "ac": r#"[{"Scope":"a:b","Permission":3}]"# })).is_err());
    }

    #[test]
    fn a_pull_request_reads_main_and_writes_only_its_own() {
        let store = Mem::default();
        save(&store, &main_branch(), "deps-aaa", 1);
        // Found in main (the PR's own scope has nothing).
        let hit = run(lookup(&store, &pr(), "deps-aaa", &[], V, 0)).unwrap().unwrap();
        assert_eq!((hit.key.as_str(), hit.entry.size), ("deps-aaa", 15));
        // Its own save goes to its own scope, and main does not see it.
        save(&store, &pr(), "deps-bbb", 2);
        assert!(run(lookup(&store, &main_branch(), "deps-bbb", &[], V, 0)).unwrap().is_none());
        assert_eq!(run(lookup(&store, &pr(), "deps-bbb", &[], V, 0)).unwrap().unwrap().index, "cache:gh77:refs/pull/5/merge:0123abcd:deps-bbb");
        // Read only: no save.
        let reader = Access { ns: "gh77".into(), scopes: vec![Scope { name: "refs/heads/main".into(), write: false }], run: None, job: None };
        assert_eq!(run(begin(&store, &reader, "x", V, "us-east-1", 3)).unwrap_err(), Refused::NoWrite);
    }

    #[test]
    fn restore_keys_find_the_newest_of_their_start_in_each_scope_in_turn() {
        let store = Mem::default();
        save(&store, &main_branch(), "deps-1", 10);
        save(&store, &main_branch(), "deps-2", 20);
        save(&store, &main_branch(), "other-9", 30);
        let hit = run(lookup(&store, &pr(), "deps-3", &["deps-".into()], V, 0)).unwrap().unwrap();
        assert_eq!(hit.key, "deps-2");
        // The PR's own scope comes first, even with an older entry.
        save(&store, &pr(), "deps-0", 5);
        assert_eq!(run(lookup(&store, &pr(), "deps-3", &["deps-".into()], V, 0)).unwrap().unwrap().key, "deps-0");
        // Another version is no hit.
        assert!(run(lookup(&store, &pr(), "deps-2", &[], "ffff", 0)).unwrap().is_none());
        // BuildKit's keys, with colons.
        save(&store, &main_branch(), "buildkit-blob-1-sha256:62a5bc3a", 40);
        assert_eq!(run(lookup(&store, &pr(), "buildkit-blob-1-sha256:62a5bc3a", &[], V, 0)).unwrap().unwrap().key, "buildkit-blob-1-sha256:62a5bc3a");
        assert_eq!(run(lookup(&store, &pr(), "nothing", &["buildkit-blob-1-sha256:".into()], V, 0)).unwrap().unwrap().key, "buildkit-blob-1-sha256:62a5bc3a");
        assert!(run(lookup(&store, &pr(), "a,b", &[], V, 0)).is_err());
    }

    #[test]
    fn an_entry_is_written_once_and_an_unfinished_one_is_no_hit() {
        let store = Mem::default();
        let (at, e) = run(begin(&store, &main_branch(), "k", V, "us-east-1", 100)).unwrap();
        assert!(run(lookup(&store, &main_branch(), "k", &[], V, 0)).unwrap().is_none());
        assert_eq!(run(begin(&store, &main_branch(), "k", V, "us-east-1", 200)).unwrap_err(), Refused::Exists);
        assert_eq!(run(begun(&store, &main_branch(), "k", V)).unwrap().unwrap().1.id, e.id);
        run(finish(&store, &at, &e, vec![("a".into(), 1)])).unwrap();
        assert_eq!(run(begin(&store, &main_branch(), "k", V, "us-east-1", 300 + UNFINISHED_MS * 2)).unwrap_err(), Refused::Exists);
        // One begun and left (its job ended while saving) is begun again ten minutes later.
        run(begin(&store, &main_branch(), "left", V, "us-east-1", 100)).unwrap();
        assert_eq!(run(begin(&store, &main_branch(), "left", V, "us-east-1", 100 + TAKEOVER_MS / 2)).unwrap_err(), Refused::Exists);
        assert!(run(begin(&store, &main_branch(), "left", V, "us-east-1", 200 + TAKEOVER_MS)).is_ok());
        assert!(run(finish(&store, &at, &e, vec![("bad name".into(), 1)])).is_err());
    }

    #[test]
    fn eviction_keeps_the_most_recently_used_within_the_limit() {
        let store = Mem::default();
        save(&store, &main_branch(), "a", 1);
        save(&store, &main_branch(), "b", 2);
        save(&store, &main_branch(), "c", 3);
        let hit = run(lookup(&store, &main_branch(), "a", &[], V, 0)).unwrap().unwrap();
        run(used(&store, &hit, 3_700_000)).unwrap();
        // 15 bytes each, room for two: b (least recently used) goes.
        let gone = run(evict_within(&store, 3_800_000, 30, 7 * 86_400_000)).unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].1.len(), 2);
        assert!(gone[0].1[0].starts_with("c/gh77/") && gone[0].1[0].ends_with("/p0"));
        assert!(run(lookup(&store, &main_branch(), "b", &[], V, 0)).unwrap().is_none());
        assert_eq!(run(kept(&store, "gh77")).unwrap(), (2, 30));
        // A week unused (as set by default): gone.
        assert_eq!(run(evict(&store, 3 + Settings::default().unused_ms() + 1, &Settings::default())).unwrap().len(), 1);
        assert_eq!(run(kept(&store, "gh77")).unwrap(), (1, 15));
        // Used every hour, but saved MAX_AGE_DAYS ago: no hit, saved anew, and gone at the next sweep.
        let old = MAX_AGE_DAYS as u64 * 86_400_000 + 10;
        assert!(run(lookup(&store, &main_branch(), "a", &[], V, old)).unwrap().is_none());
        assert!(run(begin(&store, &main_branch(), "a", V, "us-east-1", old)).is_ok(), "begun anew over the old one");
        save(&store, &main_branch(), "z", 5);
        assert_eq!(run(evict(&store, old, &Settings::default())).unwrap().len(), 1, "z is too old");
    }

    #[test]
    fn a_run_from_a_computer_reads_its_branch_and_main_and_writes_its_own() {
        let a = local_access(&local_claims("gh77", "agent-1", "refs/heads/feature", "main", "1", "1")).unwrap();
        assert_eq!(a.scopes, vec![Scope { name: "refs/superci/agent-1".into(), write: true }, Scope { name: "refs/heads/feature".into(), write: false }, Scope { name: "refs/heads/main".into(), write: false }]);
        let store = Mem::default();
        save(&store, &main_branch(), "deps", 1);
        assert_eq!(run(lookup(&store, &a, "deps", &[], V, 0)).unwrap().unwrap().key, "deps");
        save(&store, &a, "deps-local", 2);
        assert!(run(lookup(&store, &main_branch(), "deps-local", &[], V, 0)).unwrap().is_none());
        assert_eq!(local_access(&local_claims("gh77", "computer", "refs/heads/main", "main", "1", "1")).unwrap().scopes.len(), 2);
        assert!(local_access(&serde_json::json!({ "ns": "x/y", "ac": "[]" })).is_err());
        assert!(local_ns("acme/app").starts_with("local-"));
    }

    #[test]
    fn a_token_whose_scopes_cannot_be_read_gives_no_access_and_what_is_kept_damaged_is_passed_over() {
        // Scopes that are no list, one without its name or permission (passed over), a repository given as a number.
        assert!(github_access(&serde_json::json!({ "repository_id": "77", "ac": "not a list" })).unwrap_err().starts_with("the token's cache scopes"));
        assert!(local_access(&serde_json::json!({ "ns": "gh77", "ac": "not a list" })).is_err());
        let some = github_access(&serde_json::json!({ "repository_id": 77, "ac": r#"[{"Permission":3},{"Scope":"refs/heads/a"},{"Scope":"refs/heads/main","Permission":1}]"# })).unwrap();
        assert_eq!((some.ns.as_str(), some.scopes.len()), ("gh77", 1));
        // A key that is none, and a job that may only read: nothing being saved.
        let store = Mem::default();
        assert!(run(begun(&store, &main_branch(), "a,b", V)).is_err());
        assert!(run(begun(&store, &some, "deps", V)).unwrap().is_none());
        // Entries kept damaged, and an index key without its parts: passed over where they are read.
        save(&store, &main_branch(), "deps-1", 1);
        store.0.borrow_mut().insert("cache:gh77:refs/heads/main:0123abcd:deps-2".into(), "damaged".into());
        store.0.borrow_mut().insert("cache:gh77:short".into(), crate::io::json(&Entry { id: "x".into(), region: "us-east-1".into(), done: true, ..Default::default() }));
        assert_eq!(run(lookup(&store, &main_branch(), "deps-9", &["deps-".into()], V, 2)).unwrap().unwrap().key, "deps-1");
        // (Of a restore key's entries, one unfinished and one too old are no hit either.)
        let (_, _) = run(begin(&store, &main_branch(), "deps-3", V, "us-east-1", 5)).unwrap();
        save(&store, &main_branch(), "deps-0", 0);
        assert_eq!(run(lookup(&store, &main_branch(), "deps-9", &["deps-".into()], V, MAX_AGE_DAYS as u64 * 86_400_000 + 1)).unwrap().unwrap().key, "deps-1");
        for k in ["cache:gh77:s", "cache:gh77:s:v"] { store.0.borrow_mut().insert(k.into(), crate::io::json(&Entry { id: "x".into(), region: "us-east-1".into(), done: true, ..Default::default() })); }
        store.0.borrow_mut().insert("cache:gh77".into(), crate::io::json(&Entry { id: "x".into(), region: "us-east-1".into(), done: true, ..Default::default() }));
        assert_eq!(run(list(&store, None)).unwrap().len(), 2, "deps-1 and deps-0, the rest without all their parts");
        store.0.borrow_mut().insert("artifact:41:damaged".into(), "damaged".into());
        assert!(run(artifacts(&store, "41")).unwrap().is_empty());
        assert_eq!(run(evict_within(&store, 3, 100, 7 * 86_400_000)).unwrap().len(), 0);
    }

    #[test]
    fn a_store_that_does_not_answer_midway_is_said_and_nothing_is_begun_twice() {
        let store = Mem::default();
        let (at, e) = run(begin(&store, &main_branch(), "k", V, "us-east-1", 100)).unwrap();
        // Begun again over one left unfinished: the store not answering as it is read, or as it is written over.
        let later = 100 + TAKEOVER_MS + 1;
        for op in ["get", "swap"] {
            *store.1.borrow_mut() = op;
            assert!(matches!(run(begin(&store, &main_branch(), "k", V, "us-east-1", later)), Err(Refused::Bad(_))), "{op}");
        }
        *store.1.borrow_mut() = "list";
        assert!(run(kept(&store, "gh77")).is_err());
        *store.1.borrow_mut() = "";
        run(finish(&store, &at, &e, vec![("p0".into(), 1)])).unwrap();
        // An artifact of a name taken: gone as it is read (another request removed it), or the store not answering.
        run(begin_artifact(&store, "41", "report", "us-east-1", Some(("1".into(), "tests".into())), 5)).unwrap();
        *store.2.borrow_mut() = "get";
        assert_eq!(run(begin_artifact(&store, "41", "report", "us-east-1", None, 6)).unwrap_err(), Refused::Exists);
        *store.2.borrow_mut() = "swap";
        assert_eq!(run(begin_artifact(&store, "41", "report", "us-east-1", Some(("2".into(), "tests".into())), 7)).unwrap_err(), Refused::Exists, "another attempt took its place first");
        *store.2.borrow_mut() = "";
        for op in ["get", "swap"] {
            *store.1.borrow_mut() = op;
            assert!(matches!(run(begin_artifact(&store, "41", "report", "us-east-1", Some(("2".into(), "tests".into())), 7)), Err(Refused::Bad(_))), "{op}");
        }
        // An entry left unfinished, begun anew as another request begins it too: that one's.
        *store.1.borrow_mut() = "";
        run(begin(&store, &main_branch(), "left", V, "us-east-1", 100)).unwrap();
        *store.2.borrow_mut() = "swap";
        assert_eq!(run(begin(&store, &main_branch(), "left", V, "us-east-1", 200 + TAKEOVER_MS)).unwrap_err(), Refused::Exists);
    }
}
