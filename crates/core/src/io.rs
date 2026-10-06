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

#[async_trait(?Send)]
pub trait Store {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn put(&self, key: &str, value: String) -> Result<()>;
    /// Writes only when the key is new; whether it wrote. The one step that must be atomic where requests run in
    /// parallel (a job is claimed once, however many deliveries of its event arrive).
    async fn put_if_absent(&self, key: &str, value: String) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<(String, String)>>;
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
    async fn stop(&self, id: &str) -> Result<()>;
}

pub trait Clock {
    fn now_ms(&self) -> u64;
}

#[async_trait(?Send)]
pub trait Timer {
    /// Calls the control plane's `alarm` at the latest after `ms` (an earlier pending wake-up stays).
    async fn wake_in(&self, ms: u64) -> Result<()>;
}

/// Typed JSON values in a `Store`.
pub async fn get_json<T: serde::de::DeserializeOwned>(store: &dyn Store, key: &str) -> Result<Option<T>> {
    match store.get(key).await? {
        Some(v) => serde_json::from_str(&v).map(Some).map_err(|e| format!("{key}: {e}")),
        None => Ok(None),
    }
}

pub async fn put_json<T: serde::Serialize>(store: &dyn Store, key: &str, value: &T) -> Result<()> {
    store.put(key, serde_json::to_string(value).map_err(|e| e.to_string())?).await
}

/// application/x-www-form-urlencoded fields.
pub fn form(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

pub fn field<'a>(fields: &'a [(String, String)], name: &str) -> &'a str {
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
}
