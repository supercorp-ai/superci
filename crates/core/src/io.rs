//! What a control plane needs from its runtime (a Cloudflare Worker and Durable Object, an AWS Lambda, a test): HTTP out,
//! key-value storage, the time and a wake-up timer. The control plane's logic sees only these, so it runs unchanged on any of them.
use async_trait::async_trait;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Asked of one request only (a job's log): a redirect is given back as it is, not followed, so the caller goes
    /// on to another site without this request's credentials (and a link is answered with the link, not with what
    /// is there). Every runtime gives it back as it is.
    pub no_follow: bool,
}

impl Request {
    pub fn new(method: &str, url: &str) -> Self {
        Request { method: method.into(), url: url.into(), ..Default::default() }
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
    pub fn without_following(mut self) -> Self {
        self.no_follow = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Response { status, headers: vec![("content-type".into(), content_type.into()), ("cache-control".into(), "no-store".into())], body: body.into() }
    }
    pub fn text(status: u16, body: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body)
    }
    pub fn json(value: &serde_json::Value) -> Self {
        Self::new(200, "application/json", value.to_string())
    }
    pub fn redirect(location: &str) -> Self {
        let mut r = Self::new(303, "text/plain; charset=utf-8", "");
        r.headers.push(("location".into(), location.into()));
        r
    }
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[async_trait(?Send)]
pub trait Http {
    async fn send(&self, request: Request) -> Result<Response>;
}

/// State kept by a control plane, as records by key (`<kind>:<rest>`: `job:1717`, `rline:<token>:000001`). A store
/// keeps each kind apart where it can (a DynamoDB partition of its own), so one kind's traffic never slows another's;
/// a record can be given an end (`put_until`), after which the store removes it by itself (DynamoDB) or in `expire`
/// (the others). An end is only for tidying: what reads a record still weighs its times, as it is not gone at once.
#[async_trait(?Send)]
pub trait Store {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn put(&self, key: &str, value: String) -> Result<()>;
    /// Writes only when the key is new; whether it wrote. The one step that must be atomic where requests run in
    /// parallel (a job is claimed once, however many deliveries of its event arrive).
    async fn put_if_absent(&self, key: &str, value: String) -> Result<bool>;
    /// Writes only when the key holds `was` (nothing: when it is new); whether it wrote. Atomic where requests run
    /// in parallel and the runtime can make it so (a job's record is written only over what was read of it); the
    /// two steps one after the other where it cannot.
    async fn swap(&self, key: &str, was: Option<&str>, value: String) -> Result<bool> { swap_in_two_steps(self, key, was, value).await }
    async fn delete(&self, key: &str) -> Result<()>;
    /// Every record whose key begins with `prefix`, by key. The prefix names a kind (`job:`), or part of one.
    async fn list(&self, prefix: &str) -> Result<Vec<(String, String)>>;
    /// The same, kept until `until_ms` (ms since the epoch) and then removed.
    async fn put_until(&self, key: &str, value: String, until_ms: u64) -> Result<()> { let _ = until_ms; self.put(key, value).await }
    /// `swap`, with an end (none: kept).
    async fn swap_until(&self, key: &str, was: Option<&str>, value: String, until_ms: Option<u64>) -> Result<bool> { let _ = until_ms; self.swap(key, was, value).await }
    /// The last `n` records under `prefix`, by key, the last last (keys that sort by time: the newest).
    async fn list_last(&self, prefix: &str, n: usize) -> Result<Vec<(String, String)>> {
        let mut all = self.list(prefix).await?;
        all.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(all.split_off(all.len().saturating_sub(n)))
    }
    /// Several records at once, in the order asked (absent: none).
    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys { out.push(self.get(k).await?) }
        Ok(out)
    }
    /// Removes what has passed its end, where the store does not by itself; how many.
    async fn expire(&self, now_ms: u64) -> Result<usize> { let _ = now_ms; Ok(0) }
    /// Moves records kept in an earlier layout of this store into its own, at most `budget` of them; whether none
    /// is left (most stores have only the one layout).
    async fn migrate(&self, budget: usize) -> Result<bool> { let _ = budget; Ok(true) }
}

/// `Store::swap` as two steps, read then written, for a store that has no write over what was read (one request at a
/// time: a Durable Object's, a Modal Dict's).
pub async fn swap_in_two_steps<S: Store + ?Sized>(store: &S, key: &str, was: Option<&str>, value: String) -> Result<bool> {
    if store.get(key).await?.as_deref() != was { return Ok(false) }
    store.put(key, value).await.map(|_| true)
}

/// Ends for a store that has none of its own (a Durable Object's, a Modal Dict): each record given an end has it
/// beside it (`~e:<key>`), and a mark that sorts by it (`~x:<until>:<key>`) for `expire` to find the ones due. A record
/// written again keeps only its newest end (none, when written without one), so an older mark never removes it.
pub struct Ending<S>(pub S);

fn end_key(key: &str) -> String { format!("~e:{key}") }

/// A store lent: the same store.
#[async_trait(?Send)]
impl<T: Store + ?Sized> Store for &T {
    async fn get(&self, key: &str) -> Result<Option<String>> { (**self).get(key).await }
    async fn put(&self, key: &str, value: String) -> Result<()> { (**self).put(key, value).await }
    async fn put_if_absent(&self, key: &str, value: String) -> Result<bool> { (**self).put_if_absent(key, value).await }
    async fn swap(&self, key: &str, was: Option<&str>, value: String) -> Result<bool> { (**self).swap(key, was, value).await }
    async fn delete(&self, key: &str) -> Result<()> { (**self).delete(key).await }
    async fn list(&self, prefix: &str) -> Result<Vec<(String, String)>> { (**self).list(prefix).await }
    async fn put_until(&self, key: &str, value: String, until_ms: u64) -> Result<()> { (**self).put_until(key, value, until_ms).await }
    async fn swap_until(&self, key: &str, was: Option<&str>, value: String, until_ms: Option<u64>) -> Result<bool> { (**self).swap_until(key, was, value, until_ms).await }
    async fn list_last(&self, prefix: &str, n: usize) -> Result<Vec<(String, String)>> { (**self).list_last(prefix, n).await }
    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>> { (**self).get_many(keys).await }
    async fn expire(&self, now_ms: u64) -> Result<usize> { (**self).expire(now_ms).await }
    async fn migrate(&self, budget: usize) -> Result<bool> { (**self).migrate(budget).await }
}

#[async_trait(?Send)]
impl<S: Store> Store for Ending<S> {
    async fn get(&self, key: &str) -> Result<Option<String>> { self.0.get(key).await }
    async fn put(&self, key: &str, value: String) -> Result<()> { self.0.put(key, value).await?; self.0.delete(&end_key(key)).await }
    async fn put_if_absent(&self, key: &str, value: String) -> Result<bool> {
        let made = self.0.put_if_absent(key, value).await?;
        if made { self.0.delete(&end_key(key)).await? }
        Ok(made)
    }
    async fn swap(&self, key: &str, was: Option<&str>, value: String) -> Result<bool> { self.swap_until(key, was, value, None).await }
    async fn delete(&self, key: &str) -> Result<()> { self.0.delete(key).await?; self.0.delete(&end_key(key)).await }
    async fn list(&self, prefix: &str) -> Result<Vec<(String, String)>> { self.0.list(prefix).await }
    async fn put_until(&self, key: &str, value: String, until_ms: u64) -> Result<()> {
        self.0.put(key, value).await?;
        self.0.put(&end_key(key), until_ms.to_string()).await?;
        self.0.put(&format!("~x:{until_ms:013}:{key}"), String::new()).await
    }
    async fn swap_until(&self, key: &str, was: Option<&str>, value: String, until_ms: Option<u64>) -> Result<bool> {
        if !self.0.swap(key, was, value).await? { return Ok(false) }
        match until_ms {
            Some(until) => { self.0.put(&end_key(key), until.to_string()).await?; self.0.put(&format!("~x:{until:013}:{key}"), String::new()).await? }
            None => self.0.delete(&end_key(key)).await?,
        }
        Ok(true)
    }
    async fn list_last(&self, prefix: &str, n: usize) -> Result<Vec<(String, String)>> { self.0.list_last(prefix, n).await }
    async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>> { self.0.get_many(keys).await }
    async fn expire(&self, now_ms: u64) -> Result<usize> {
        let mut gone = 0;
        for (mark, _) in self.0.list("~x:").await? {
            let Some((until, key)) = mark.strip_prefix("~x:").and_then(|r| r.split_once(':')) else { continue };
            let Ok(until) = until.parse::<u64>() else { continue };
            if until > now_ms { continue }
            // Only while this is still its end: written again since, it keeps its newer one.
            if self.0.get(&end_key(key)).await?.as_deref() == Some(until.to_string().as_str()) {
                self.0.delete(key).await?;
                self.0.delete(&end_key(key)).await?;
                gone += 1;
            }
            self.0.delete(&mark).await?;
        }
        Ok(gone)
    }
    async fn migrate(&self, budget: usize) -> Result<bool> { self.0.migrate(budget).await }
}

/// What a machine runs: GitHub's runner with a just-in-time configuration (exactly one job), or GitLab's runner with a
/// project runner's token (the next job with its tags), see `gitlab::RUNNER_SCRIPT`.
#[derive(Clone, Debug, PartialEq)]
pub enum Work {
    GitHub { jit: String },
    GitLab { url: String, token: String },
    /// A runner that takes a job and fails it at once, before any of its steps: `why` is the line it prints (base64,
    /// so any text is safe on a command line), a GitHub `::error::` saying why SuperCI could not run it.
    Fail { jit: String, why: String },
}

impl Work {
    /// As machines are told it: `{"jit": …}` or `{"gitlab": {"url": …, "token": …}}`.
    pub fn json(&self) -> serde_json::Value {
        match self {
            Work::GitHub { jit } => serde_json::json!({ "jit": jit }),
            Work::GitLab { url, token } => serde_json::json!({ "gitlab": { "url": url, "token": token } }),
            Work::Fail { jit, why } => serde_json::json!({ "jit": jit, "fail": why }),
        }
    }
}

/// Containers the control plane's own runtime can start (a Cloudflare Worker with a container class): one per job.
#[async_trait(?Send)]
pub trait Containers {
    /// Starts a runner for `work` on a machine of `size`; its id.
    async fn start(&self, name: &str, work: &Work, max_minutes: u32, size: crate::spec::Size) -> Result<String>;
    /// The same, from an image of the control plane's own (`plane::RunnerImage`: its address, and the Modal Secret
    /// for a private registry). Only Modal's sandboxes start one (placement sends no such job elsewhere).
    async fn start_from(&self, _name: &str, _work: &Work, _max_minutes: u32, _size: crate::spec::Size, _image: &str, _secret: Option<&str>) -> Result<String> {
        Err("these containers start from no image of your own".into())
    }
    async fn stop(&self, id: &str) -> Result<()>;
}

pub trait Clock {
    fn now_ms(&self) -> u64;
}

#[async_trait(?Send)]
pub trait Timer {
    /// Calls the control plane's `alarm` at the latest after `ms` (an earlier pending wake-up stays).
    async fn wake_in(&self, ms: u64) -> Result<()>;
    /// Waits `ms` in place, the request being answered waiting with it, where the runtime can; at once where not.
    async fn pause(&self, _ms: u64) {}
    /// Holds an answer that says to wait (`x-superci-hold`: a runner asking for a message when there is none) `ms`
    /// before it goes: in place, unless the runtime holds it itself once the answer has left the control plane (one
    /// that answers one request at a time, so that the others are not held behind it).
    async fn hold(&self, ms: u64) { self.pause(ms).await }
    /// Leaves `what` to be done once the request at hand is answered: the runtime then gives it to the control
    /// plane's `later`. Whether it will; a runtime that cannot says no, and it is done before the answer.
    fn later(&self, _what: &str) -> bool { false }
}

/// Typed JSON values in a `Store`.
pub async fn get_json<T: serde::de::DeserializeOwned>(store: &dyn Store, key: &str) -> Result<Option<T>> {
    match store.get(key).await? {
        Some(v) => serde_json::from_str(&v).map(Some).map_err(|e| format!("{key}: {e}")),
        None => Ok(None),
    }
}

pub async fn put_json<T: serde::Serialize>(store: &dyn Store, key: &str, value: &T) -> Result<()> {
    store.put(key, json(value)).await
}

/// One of SuperCI's own records as JSON text: its fields are strings, numbers, lists and maps keyed by strings, which
/// serde_json always writes.
pub fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("SuperCI's records are always JSON")
}

/// A control plane on this computer, as tests serve one: an address taken as an https one in a debug build only.
#[cfg(debug_assertions)]
pub fn local_test_address(url: &str) -> bool { url.starts_with("http://127.0.0.1:") }
#[cfg(not(debug_assertions))]
pub fn local_test_address(_: &str) -> bool { false }

/// An outside address, or, in a debug build with `SUPERCI_STAND_IN=URL`, the stand-in's for it (`superci_testkit`'s,
/// as `URL/<the host>/<path>`): AWS, GitHub's API (and GitHub Enterprise Cloud's, `*.ghe.com`), where GitHub keeps logs, gitlab.com, Cloudflare's API and Modal's launchers (Modal's own API,
/// gRPC, is reached at the stand-in itself).
/// What tests run (the CLI, the dashboard, the AWS control plane's program) reaches no cloud.
pub fn stand_in(url: &str) -> String {
    #[cfg(debug_assertions)]
    if let Some(at) = std::env::var("SUPERCI_STAND_IN").ok().filter(|a| !a.is_empty()) { return stand_in_at(url, &at) }
    url.to_string()
}

/// An outside address as the stand-in at `at` has it, or as it is when the stand-in has nothing for its host (the
/// same in any build: what the CLI's live checks use offline, the stand-in given them rather than in the environment).
pub fn stand_in_at(url: &str, at: &str) -> String {
    match url.strip_prefix("https://").filter(|u| u.split('/').next().is_some_and(|h| h.ends_with(".amazonaws.com") || h.ends_with(".signin.aws.amazon.com") || h.ends_with(".blob.core.windows.net") || h == "api.github.com" || h.ends_with(".ghe.com") || h == "gitlab.com" || h == "api.cloudflare.com" || h == "dash.cloudflare.com" || h == "registry.cloudflare.com" || h == "ghcr.io" || h.ends_with(".workers.dev") || h.ends_with(".modal.run"))) {
        Some(rest) => format!("{}/{rest}", at.trim_end_matches('/')),
        None => url.to_string(),
    }
}
