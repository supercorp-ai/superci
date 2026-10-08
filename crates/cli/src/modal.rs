//! Modal from the dashboard. Modal's API is gRPC (modal_proto/api.proto, Apache-2.0); this speaks only the few calls
//! the dashboard needs: the browser sign-in (what `modal token new` does), and deploying the runner agent, which runs
//! Modal's own `modal deploy` in a short-lived sandbox. Runner sandboxes themselves are started by the deployed agent
//! with Modal's official Python SDK. Messages are declared by hand with the protocol's field numbers.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::Result;

const API: &str = "https://api.modal.com";
/// Modal's server knows its own clients only; this one behaves like its Go SDK and says who it is.
const CLIENT_TYPE: &str = "9";

mod pb {
    #[derive(Clone, PartialEq, prost::Message)] pub struct Empty {}
    #[derive(Clone, PartialEq, prost::Message)] pub struct TokenFlowCreateRequest { #[prost(string, tag = "3")] pub utm_source: String, #[prost(int32, tag = "4")] pub localhost_port: i32, #[prost(string, tag = "5")] pub next_url: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct TokenFlowCreateResponse { #[prost(string, tag = "1")] pub token_flow_id: String, #[prost(string, tag = "2")] pub web_url: String, #[prost(string, tag = "3")] pub code: String, #[prost(string, tag = "4")] pub wait_secret: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct TokenFlowWaitRequest { #[prost(float, tag = "1")] pub timeout: f32, #[prost(string, tag = "2")] pub token_flow_id: String, #[prost(string, tag = "3")] pub wait_secret: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct TokenFlowWaitResponse { #[prost(string, tag = "1")] pub token_id: String, #[prost(string, tag = "2")] pub token_secret: String, #[prost(bool, tag = "3")] pub timeout: bool, #[prost(string, tag = "4")] pub workspace_username: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AuthTokenGetResponse { #[prost(string, tag = "1")] pub token: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct EnvironmentGetOrCreateRequest { #[prost(string, tag = "1")] pub deployment_name: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct EnvironmentGetOrCreateResponse { #[prost(string, tag = "1")] pub environment_id: String, #[prost(message, optional, tag = "2")] pub metadata: Option<EnvironmentMetadata> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct EnvironmentMetadata { #[prost(string, tag = "1")] pub name: String, #[prost(message, optional, tag = "2")] pub settings: Option<EnvironmentSettings> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct EnvironmentSettings { #[prost(string, tag = "1")] pub image_builder_version: String, #[prost(string, tag = "2")] pub webhook_suffix: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AppGetOrCreateRequest { #[prost(string, tag = "1")] pub app_name: String, #[prost(string, tag = "2")] pub environment_name: String, #[prost(int32, tag = "3")] pub object_creation_type: i32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AppGetOrCreateResponse { #[prost(string, tag = "1")] pub app_id: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct Image { #[prost(string, repeated, tag = "6")] pub dockerfile_commands: Vec<String> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct ImageGetOrCreateRequest { #[prost(message, optional, tag = "2")] pub image: Option<Image>, #[prost(string, tag = "4")] pub app_id: String, #[prost(string, tag = "9")] pub builder_version: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct GenericResult { #[prost(int32, tag = "1")] pub status: i32, #[prost(string, tag = "2")] pub exception: String, #[prost(int32, tag = "3")] pub exitcode: i32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct ImageGetOrCreateResponse { #[prost(string, tag = "1")] pub image_id: String, #[prost(message, optional, tag = "2")] pub result: Option<GenericResult> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct ImageJoinStreamingRequest { #[prost(string, tag = "1")] pub image_id: String, #[prost(float, tag = "2")] pub timeout: f32, #[prost(string, tag = "3")] pub last_entry_id: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct ImageJoinStreamingResponse { #[prost(message, optional, tag = "1")] pub result: Option<GenericResult>, #[prost(string, tag = "3")] pub entry_id: String, #[prost(bool, tag = "4")] pub eof: bool }
    #[derive(Clone, PartialEq, prost::Message)] pub struct Resources { #[prost(uint32, tag = "2")] pub memory_mb: u32, #[prost(uint32, tag = "3")] pub milli_cpu: u32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct StringMap { #[prost(map = "string, string", tag = "1")] pub contents: std::collections::HashMap<String, String> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct Sandbox {
        #[prost(string, repeated, tag = "1")] pub entrypoint_args: Vec<String>,
        #[prost(string, tag = "3")] pub image_id: String,
        #[prost(message, optional, tag = "5")] pub resources: Option<Resources>,
        #[prost(uint32, tag = "7")] pub timeout_secs: u32,
        #[prost(message, optional, tag = "40")] pub environment_variables: Option<StringMap>,
    }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxCreateRequest { #[prost(string, tag = "1")] pub app_id: String, #[prost(message, optional, tag = "2")] pub definition: Option<Sandbox> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxCreateResponse { #[prost(string, tag = "1")] pub sandbox_id: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxWaitRequest { #[prost(string, tag = "1")] pub sandbox_id: String, #[prost(float, tag = "2")] pub timeout: f32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxWaitResponse { #[prost(message, optional, tag = "1")] pub result: Option<GenericResult> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxGetLogsRequest { #[prost(string, tag = "1")] pub sandbox_id: String, #[prost(int32, tag = "2")] pub file_descriptor: i32, #[prost(float, tag = "3")] pub timeout: f32, #[prost(string, tag = "4")] pub last_entry_id: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct TaskLogs { #[prost(string, tag = "1")] pub data: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct SandboxStdinWriteRequest { #[prost(string, tag = "1")] pub sandbox_id: String, #[prost(bytes = "vec", tag = "2")] pub input: Vec<u8>, #[prost(uint32, tag = "3")] pub index: u32, #[prost(bool, tag = "4")] pub eof: bool }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AppListRequest { #[prost(string, tag = "1")] pub environment_name: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AppListItem { #[prost(string, tag = "1")] pub app_id: String, #[prost(int32, tag = "4")] pub state: i32, #[prost(string, tag = "10")] pub name: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct AppListResponse { #[prost(message, repeated, tag = "1")] pub apps: Vec<AppListItem> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct DictGetOrCreateRequest { #[prost(string, tag = "1")] pub deployment_name: String, #[prost(string, tag = "3")] pub environment_name: String, #[prost(int32, tag = "4")] pub object_creation_type: i32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct DictGetOrCreateResponse { #[prost(string, tag = "1")] pub dict_id: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct DictEntry { #[prost(bytes = "vec", tag = "1")] pub key: Vec<u8>, #[prost(bytes = "vec", tag = "2")] pub value: Vec<u8> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct DictUpdateRequest { #[prost(string, tag = "1")] pub dict_id: String, #[prost(message, repeated, tag = "2")] pub updates: Vec<DictEntry>, #[prost(bool, tag = "3")] pub if_not_exists: bool }
    #[derive(Clone, PartialEq, prost::Message)] pub struct DictUpdateResponse { #[prost(bool, tag = "1")] pub created: bool }
    #[derive(Clone, PartialEq, prost::Message)] pub struct WorkspaceNameLookupResponse { #[prost(string, tag = "2")] pub username: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct Timestamp { #[prost(int64, tag = "1")] pub seconds: i64, #[prost(int32, tag = "2")] pub nanos: i32 }
    #[derive(Clone, PartialEq, prost::Message)] pub struct WorkspaceBillingSummaryRequest { #[prost(message, optional, tag = "1")] pub start_timestamp: Option<Timestamp> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct WorkspaceBillingSummaryResponse { #[prost(string, tag = "3")] pub metered_cost: String, #[prost(string, tag = "4")] pub billed_cost: String,
        #[prost(map = "string, string", tag = "6")] pub adjustments: std::collections::HashMap<String, String> }
    #[derive(Clone, PartialEq, prost::Message)] pub struct WorkspaceBillingReportRequest { #[prost(message, optional, tag = "1")] pub start_timestamp: Option<Timestamp>,
        #[prost(message, optional, tag = "2")] pub end_timestamp: Option<Timestamp>, #[prost(string, tag = "3")] pub resolution: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct WorkspaceBillingReportItem { #[prost(string, tag = "1")] pub object_id: String, #[prost(string, tag = "2")] pub description: String,
        #[prost(string, tag = "5")] pub cost: String }
    #[derive(Clone, PartialEq, prost::Message)] pub struct TaskLogsBatch { #[prost(message, repeated, tag = "2")] pub items: Vec<TaskLogs>, #[prost(string, tag = "5")] pub entry_id: String, #[prost(bool, tag = "14")] pub eof: bool }
}

/// This month at Modal, as Modal bills it: what the workspace was metered and billed (after its plan's credits and
/// allowances), and how much of the metered cost was SuperCI's own apps (control planes and runner agents).
#[derive(Clone, Debug, PartialEq)]
pub struct Month { pub metered: f64, pub billed: f64, pub superci: f64 }

pub fn month(session: &Session) -> Result<Month> {
    let mut c = Client::new(Some(session.clone()))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs() as i64;
    let start = superci_core::plane::month_start_ms(now as u64 * 1000) as i64 / 1000;
    let ts = |seconds: i64| Some(pb::Timestamp { seconds, nanos: 0 });
    let summary: pb::WorkspaceBillingSummaryResponse = c.unary("WorkspaceBillingSummary", pb::WorkspaceBillingSummaryRequest { start_timestamp: ts(start) })?;
    let items: Vec<pb::WorkspaceBillingReportItem> = c.stream("WorkspaceBillingReport", pb::WorkspaceBillingReportRequest { start_timestamp: ts(start), end_timestamp: ts(now + 86_400), resolution: "d".into() }, 100_000)?;
    let n = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    let superci = items.iter().filter(|i| i.description.starts_with("superci-")).map(|i| n(&i.cost)).sum();
    Ok(Month { metered: n(&summary.metered_cost), billed: n(&summary.billed_cost), superci })
}

/// A signed-in Modal workspace (SuperCI's own token there, kept with its sign-ins: store.rs).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Session { pub token_id: String, pub token_secret: String, pub workspace: String }

impl Session {
    /// For a machine with no browser: a token given to SuperCI by name (SUPERCI_MODAL_TOKEN_ID,
    /// SUPERCI_MODAL_TOKEN_SECRET) instead of signing in. Modal's own variables are not read.
    pub fn from_env() -> Option<Session> {
        let var = |n: &str| std::env::var(n).ok().filter(|v| !v.trim().is_empty());
        let (token_id, token_secret) = (var("SUPERCI_MODAL_TOKEN_ID")?, var("SUPERCI_MODAL_TOKEN_SECRET")?);
        let mut c = Client::new(Some(Session { token_id: token_id.clone(), token_secret: token_secret.clone(), workspace: String::new() })).ok()?;
        let w: pb::WorkspaceNameLookupResponse = c.unary("WorkspaceNameLookup", pb::Empty {}).ok()?;
        Some(Session { token_id, token_secret, workspace: w.username })
    }
}

struct Client { rt: tokio::runtime::Runtime, channel: Channel, session: Option<Session>, auth_token: Option<String> }

impl Client {
    fn new(session: Option<Session>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let channel = rt.block_on(async {
            Endpoint::from_static(API).tls_config(ClientTlsConfig::new().with_webpki_roots()).map_err(|e| e.to_string())?
                .timeout(Duration::from_secs(90)).connect().await.map_err(|e| format!("Modal: {e}"))
        })?;
        Ok(Client { rt, channel, session, auth_token: None })
    }

    fn request<T>(&self, message: T, with_auth: bool) -> tonic::Request<T> {
        let mut r = tonic::Request::new(message);
        let m = r.metadata_mut();
        let set = |m: &mut tonic::metadata::MetadataMap, k: &'static str, v: &str| { if let Ok(v) = MetadataValue::try_from(v) { m.insert(k, v); } };
        set(m, "x-modal-client-type", CLIENT_TYPE);
        set(m, "x-modal-client-version", "1.0.0");
        set(m, "x-modal-libmodal-version", concat!("superci/", env!("CARGO_PKG_VERSION")));
        set(m, "x-modal-host", "api.modal.com");
        if let Some(s) = &self.session {
            set(m, "x-modal-token-id", &s.token_id);
            set(m, "x-modal-token-secret", &s.token_secret);
        }
        if with_auth { if let Some(t) = &self.auth_token { set(m, "x-modal-auth-token", t); } }
        r
    }

    fn unary<Q: prost::Message + Send + Sync + 'static, R: prost::Message + Default + Send + Sync + 'static>(&mut self, method: &'static str, message: Q) -> Result<R> {
        let authed = self.session.is_some() && method != "AuthTokenGet";
        if authed && self.auth_token.is_none() {
            let t: pb::AuthTokenGetResponse = self.unary("AuthTokenGet", pb::Empty {})?;
            self.auth_token = Some(t.token);
        }
        let req = self.request(message, authed);
        let path: tonic::codegen::http::uri::PathAndQuery = format!("/modal.client.ModalClient/{method}").parse().map_err(|_| "bad method".to_string())?;
        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        self.rt.block_on(async {
            grpc.ready().await.map_err(|e| format!("Modal: {e}"))?;
            grpc.unary(req, path, tonic_prost::ProstCodec::default()).await.map(|r| r.into_inner()).map_err(|s| format!("Modal {method}: {}", s.message()))
        })
    }

    /// A server stream, read to its end (or `limit` messages).
    fn stream<Q: prost::Message + Send + Sync + 'static, R: prost::Message + Default + Send + Sync + 'static>(&mut self, method: &'static str, message: Q, limit: usize) -> Result<Vec<R>> {
        let req = self.request(message, true);
        let path: tonic::codegen::http::uri::PathAndQuery = format!("/modal.client.ModalClient/{method}").parse().map_err(|_| "bad method".to_string())?;
        let mut grpc = tonic::client::Grpc::new(self.channel.clone());
        self.rt.block_on(async {
            grpc.ready().await.map_err(|e| format!("Modal: {e}"))?;
            let mut s = grpc.server_streaming(req, path, tonic_prost::ProstCodec::default()).await.map_err(|s| format!("Modal {method}: {}", s.message()))?.into_inner();
            let mut out = vec![];
            while let Some(m) = s.message().await.map_err(|s| format!("Modal {method}: {}", s.message()))? { out.push(m); if out.len() >= limit { break } }
            Ok(out)
        })
    }
}

/// The browser sign-in: Modal's page, then the token for the workspace you choose there. Modal's page checks a tiny
/// server on this machine that answers with the flow's id (as `modal token new` does).
#[derive(Clone)]
pub struct Pending { pub web_url: String, flow_id: String, wait_secret: String }

#[cfg(test)]
impl Pending {
    /// A sign-in waiting for approval that never reaches Modal, for rendering pages in tests.
    pub fn for_test() -> Pending { Pending { web_url: "https://modal.com/token-flow/tf-test".into(), flow_id: "tf-test".into(), wait_secret: String::new() } }
}

/// `_back`: where the dashboard would have Modal return. Modal's page says it will and never does (it stays on
/// "API token created"), so it is not asked to: SuperCI asks Modal whether the sign-in was approved (`wait`).
pub fn authorize(_back: &str) -> Result<Pending> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let mut client = Client::new(None)?;
    let r: pb::TokenFlowCreateResponse = client.unary("TokenFlowCreate", pb::TokenFlowCreateRequest { utm_source: "superci".into(), localhost_port: port as i32, next_url: String::new() })?;
    let flow_id = r.token_flow_id.clone();
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(false);
        for stream in listener.incoming().flatten().take(20) {
            let mut stream = stream;
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", flow_id.len(), flow_id);
        }
    });
    Ok(Pending { web_url: r.web_url, flow_id: r.token_flow_id, wait_secret: r.wait_secret })
}

/// Whether the sign-in was approved in the browser (waiting up to `seconds`).
pub fn wait(p: &Pending, seconds: f32) -> Result<Option<Session>> {
    let mut client = Client::new(None)?;
    let r: pb::TokenFlowWaitResponse = client.unary("TokenFlowWait", pb::TokenFlowWaitRequest { timeout: seconds, token_flow_id: p.flow_id.clone(), wait_secret: p.wait_secret.clone() })?;
    Ok((!r.timeout).then(|| Session { token_id: r.token_id, token_secret: r.token_secret, workspace: r.workspace_username }))
}

/// Deploys the runner agent for one control plane (Modal's `modal deploy`, run in a sandbox in your workspace) and
/// returns its URL. The agent trusts only that control plane (its keys pinned here).
pub fn deploy_runners(session: &Session, plane_url: &str, plane_id: &str, keys: &serde_json::Value) -> Result<String> {
    let name = format!("superci-runners-{plane_id}");
    let trust = serde_json::json!({ "issuer": plane_url, "subject": format!("plane:{plane_id}"), "keys": keys });
    use base64::Engine;
    let agent = include_str!("modal_agent.py").replace("__TRUST__", &base64::engine::general_purpose::STANDARD.encode(trust.to_string())).replace("__NAME__", &name);
    let suffix = modal_deploy(session, &agent, None)?;
    Ok(format!("https://{}--{name}.{suffix}", session.workspace))
}

/// The control plane program a Modal control plane runs (Linux x86_64), handed to `modal deploy` with plane.py.
const PLANE_BINARY: &[u8] = include_bytes!("../../plane-modal/build/superci-plane");

/// What deploying a control plane to Modal does, in order, as the dashboard shows it.
pub const DEPLOY_STEPS: [&str; 3] = ["Building its images in your workspace", "Deploying it, with its check for waiting jobs and leftover machines every minute", "Waiting for it to answer"];

/// Deploys (or updates) a control plane in this workspace: app `superci-plane-<id>`, its state and settings Dicts,
/// its schedule. Returns its URL.
pub fn deploy_plane(session: &Session, plane_id: &str, label: &str, step: &dyn Fn(usize)) -> Result<String> {
    step(0);
    let name = plane_app(plane_id);
    let script = include_str!("modal_plane.py").replace("__NAME__", &name).replace("__PLANE_ID__", plane_id).replace("__LABEL__", label);
    let suffix = modal_deploy(session, &script, Some(PLANE_BINARY))?;
    step(1);
    Ok(format!("https://{}--{name}.{suffix}", session.workspace))
}

/// A Modal control plane's app name.
pub fn plane_app(plane_id: &str) -> String { format!("superci-plane-{plane_id}") }

/// The control planes in this workspace: deployed apps named `superci-plane-<id>`, as (plane id, URL).
pub fn find_planes(session: &Session) -> Result<Vec<(String, String)>> {
    let mut c = Client::new(Some(session.clone()))?;
    let env: pb::EnvironmentGetOrCreateResponse = c.unary("EnvironmentGetOrCreate", pb::EnvironmentGetOrCreateRequest { deployment_name: String::new() })?;
    let suffix = env.metadata.and_then(|m| m.settings).map(|s| s.webhook_suffix).filter(|s| !s.is_empty()).unwrap_or_else(|| "modal.run".into());
    let apps: pb::AppListResponse = c.unary("AppList", pb::AppListRequest { environment_name: String::new() })?;
    Ok(apps.apps.into_iter().filter(|a| a.state == 3).filter_map(|a| {
        let id = a.name.strip_prefix("superci-plane-")?.to_string();
        Some((id, format!("https://{}--{}.{suffix}", session.workspace, a.name)))
    }).collect())
}

/// Writes one of a Modal control plane's settings (its settings Dict; the control plane reads it within 10 seconds).
pub fn put_setting(session: &Session, plane_id: &str, name: &str, value: &str) -> Result<()> {
    let mut c = Client::new(Some(session.clone()))?;
    let d: pb::DictGetOrCreateResponse = c.unary("DictGetOrCreate", pb::DictGetOrCreateRequest { deployment_name: format!("{}-settings", plane_app(plane_id)), environment_name: String::new(), object_creation_type: 1 })?;
    let _: pb::DictUpdateResponse = c.unary("DictUpdate", pb::DictUpdateRequest { dict_id: d.dict_id, updates: vec![pb::DictEntry { key: pickle_str(name)?, value: pickle_str(value)? }], if_not_exists: false })?;
    Ok(())
}

/// A string as Modal's client serializes it (Python's pickle, protocol 4), so Python reads it back as that string.
fn pickle_str(s: &str) -> Result<Vec<u8>> {
    let data = s.as_bytes();
    if data.len() >= 60_000 { return Err("a setting longer than 60 kB".into()) }
    let mut payload = vec![];
    if data.len() < 256 { payload.push(0x8c); payload.push(data.len() as u8) } else { payload.push(b'X'); payload.extend_from_slice(&(data.len() as u32).to_le_bytes()) }
    payload.extend_from_slice(data);
    payload.extend_from_slice(&[0x94, b'.']);
    let mut out = vec![0x80, 0x04, 0x95];
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Runs Modal's own `modal deploy` on `script` in a short-lived sandbox in the workspace (with `file`, if any, at
/// /tmp/superci-plane); returns the workspace's web address suffix.
/// Deletes a Modal control plane (its app and its two Dicts) and the runner agent made for a control plane, with
/// Modal's own client in a sandbox. Safe to repeat.
pub fn delete_plane(session: &Session, plane_id: &str) -> Result<()> {
    let app = plane_app(plane_id);
    modal_run(session, &format!("modal app stop --yes {app} || true; modal dict delete --yes {app}-state || true; modal dict delete --yes {app}-settings || true; modal app stop --yes superci-runners-{plane_id} || true"), "", None).map(|_| ())
}

/// Deletes the runner agent made for a control plane elsewhere.
pub fn delete_runners(session: &Session, plane_id: &str) -> Result<()> {
    modal_run(session, &format!("modal app stop --yes superci-runners-{plane_id} || true"), "", None).map(|_| ())
}

fn modal_deploy(session: &Session, script: &str, file: Option<&[u8]>) -> Result<String> {
    modal_run(session, "modal deploy app.py", script, file)
}

/// Runs a command of Modal's client (`modal …`) in a sandbox in the workspace, with `script` as /tmp/app.py and
/// `file` as /tmp/superci-plane.
fn modal_run(session: &Session, command: &str, script: &str, file: Option<&[u8]>) -> Result<String> {
    let mut c = Client::new(Some(session.clone()))?;
    let env: pb::EnvironmentGetOrCreateResponse = c.unary("EnvironmentGetOrCreate", pb::EnvironmentGetOrCreateRequest { deployment_name: String::new() })?;
    let settings = env.metadata.and_then(|m| m.settings).unwrap_or_default();
    let app: pb::AppGetOrCreateResponse = c.unary("AppGetOrCreate", pb::AppGetOrCreateRequest { app_name: "superci-setup".into(), environment_name: String::new(), object_creation_type: 1 })?;
    // An image with Modal's own client, to run `modal deploy` from.
    let image: pb::ImageGetOrCreateResponse = c.unary("ImageGetOrCreate", pb::ImageGetOrCreateRequest {
        image: Some(pb::Image { dockerfile_commands: vec!["FROM python:3.12-slim".into(), "RUN pip install --no-cache-dir modal==1.6.0".into()] }),
        app_id: app.app_id.clone(), builder_version: settings.image_builder_version.clone(),
    })?;
    let mut result = image.result.filter(|r| r.status != 0);
    let mut last = String::new();
    for _ in 0..30 {
        if result.is_some() { break }
        for m in c.stream::<_, pb::ImageJoinStreamingResponse>("ImageJoinStreaming", pb::ImageJoinStreamingRequest { image_id: image.image_id.clone(), timeout: 55.0, last_entry_id: last.clone() }, 10_000)? {
            if !m.entry_id.is_empty() { last = m.entry_id.clone() }
            if let Some(r) = m.result.filter(|r| r.status != 0) { result = Some(r); break }
        }
    }
    match result { Some(r) if r.status == 1 => {}, Some(r) => return Err(format!("Modal could not build the setup image: {}", r.exception)), None => return Err("Modal's setup image did not finish building".into()) }

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut vars = std::collections::HashMap::new();
    vars.insert("MODAL_TOKEN_ID".to_string(), session.token_id.clone());
    vars.insert("MODAL_TOKEN_SECRET".to_string(), session.token_secret.clone());
    vars.insert("SCRIPT".to_string(), b64.encode(script));
    // The file comes on the sandbox's standard input (too big for a variable).
    let receive = if file.is_some() { "base64 -d > /tmp/superci-plane && " } else { "" };
    let sandbox: pb::SandboxCreateResponse = c.unary("SandboxCreate", pb::SandboxCreateRequest { app_id: app.app_id, definition: Some(pb::Sandbox {
        entrypoint_args: vec!["sh".into(), "-c".into(), format!("{receive}echo \"$SCRIPT\" | base64 -d > /tmp/app.py && cd /tmp && {command}")],
        image_id: image.image_id, resources: Some(pb::Resources { memory_mb: 1024, milli_cpu: 1000 }), timeout_secs: 900,
        environment_variables: Some(pb::StringMap { contents: vars }),
    }) })?;
    if let Some(bytes) = file {
        let encoded = b64.encode(bytes).into_bytes();
        let chunks: Vec<&[u8]> = encoded.chunks(512 * 1024).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let _: pb::Empty = c.unary("SandboxStdinWrite", pb::SandboxStdinWriteRequest { sandbox_id: sandbox.sandbox_id.clone(), input: chunk.to_vec(), index: i as u32 + 1, eof: i + 1 == chunks.len() })?;
        }
    }
    let mut done = None;
    for _ in 0..18 {
        let w: pb::SandboxWaitResponse = c.unary("SandboxWait", pb::SandboxWaitRequest { sandbox_id: sandbox.sandbox_id.clone(), timeout: 50.0 })?;
        if let Some(r) = w.result.filter(|r| r.status != 0) { done = Some(r); break }
    }
    let ok = done.as_ref().is_some_and(|r| r.status == 1 && r.exitcode == 0);
    if !ok {
        let logs = c.stream::<_, pb::TaskLogsBatch>("SandboxGetLogs", pb::SandboxGetLogsRequest { sandbox_id: sandbox.sandbox_id.clone(), file_descriptor: 2, timeout: 5.0, last_entry_id: String::new() }, 50)
            .unwrap_or_default().into_iter().flat_map(|b| b.items).map(|i| i.data).collect::<String>();
        let tail: String = logs.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
        return Err(format!("`{command}` did not succeed ({}): {tail}", done.map(|r| format!("status {}, exit {}", r.status, r.exitcode)).unwrap_or_else(|| "still running".into())));
    }
    Ok(if settings.webhook_suffix.is_empty() { "modal.run".to_string() } else { settings.webhook_suffix })
}

#[cfg(test)]
mod tests {
    /// The bytes Python's pickle (protocol 4, as Modal's client uses) gives a string, checked against Python if present.
    #[test]
    fn strings_pickle_as_modal_writes_them() {
        assert_eq!(super::pickle_str("config:GITHUB_APP").unwrap(), b"\x80\x04\x95\x15\x00\x00\x00\x00\x00\x00\x00\x8c\x11config:GITHUB_APP\x94.".to_vec());
        for s in ["", "é ünïcode", &"x".repeat(300), &"{\"pem\":\"-----BEGIN\"}".repeat(200)] {
            let Ok(out) = std::process::Command::new("python3").args(["-c", "import pickle,sys; sys.stdout.buffer.write(pickle.dumps(sys.argv[1], protocol=4))", s]).output() else { return };
            assert_eq!(super::pickle_str(s).unwrap(), out.stdout, "{}", s.len());
        }
    }

    /// A runner sandbox with a 3-minute bound, as the control plane starts one, ends by itself; and a job's sandbox,
    /// holding no token, cannot read this workspace's stored settings (SUPERCI_MODAL_TOKEN_ID/SECRET, plus live.rs's settings).
    #[test]
    #[ignore]
    fn live_modal_sandbox_ends_at_its_time_bound_and_cannot_read_settings() {
        use super::{pb, Client, Session};
        let session = Session::from_env().expect("SUPERCI_MODAL_TOKEN_ID and SUPERCI_MODAL_TOKEN_SECRET");
        let idle = crate::live::Idle::new(&format!("superci-leaktest-modal-{}", superci_core::crypto::random_id(6)));
        let script = r#"
import modal, os, sys, time
def say(*a): print(*a, file=sys.stderr, flush=True)
app = modal.App.lookup("superci-leaktest", create_if_missing=True)
d = modal.Dict.from_name("superci-leaktest-probe", create_if_missing=True)
d["secret"] = "visible"
probe = modal.Sandbox.create("python", "-c", "import modal\ntry:\n    print('PROBE read:', modal.Dict.from_name('superci-leaktest-probe').get('secret'))\nexcept Exception as e:\n    print('PROBE blocked:', type(e).__name__, str(e)[:160])", app=app, image=modal.Image.debian_slim().pip_install("modal==1.6.0"), timeout=300)
probe.wait(raise_on_termination=False)
say(probe.stdout.read().strip())
try:
    modal.Dict.objects.delete("superci-leaktest-probe")
except Exception as e:
    say("probe dict left:", e)
image = modal.Image.from_registry("ghcr.io/actions/actions-runner:latest").env({"RUNNER_ALLOW_RUNASROOT": "1"})
t0 = time.time()
# As the control plane starts one for a 1-CPU job: half a physical core, held to what it asks for.
sb = modal.Sandbox.create("/home/runner/run.sh", "--jitconfig", os.environ["JIT"], app=app, image=image, workdir="/home/runner", cpu=(0.5, 0.5), memory=(2048, 2048), timeout=180)
say("BOUND started", sb.object_id)
try:
    sb.wait(raise_on_termination=False)
except modal.exception.SandboxTimeoutError:
    say("BOUND timed out (Modal ended it)")
say("BOUND ended after", round(time.time() - t0), "s, return code", sb.returncode)
# Modal's own billing report (Team and Enterprise plans), for the sandbox just ended.
try:
    import datetime
    end = datetime.datetime.now(datetime.timezone.utc).replace(minute=0, second=0, microsecond=0) + datetime.timedelta(hours=1)
    for item in modal.Workspace.billing.report(start=end - datetime.timedelta(hours=2), end=end, resolution="h"):
        say("BILL", item)
except Exception as e:
    say("BILL unavailable:", type(e).__name__, str(e)[:160])
"#;
        // The runner's state at GitHub, every 30 seconds, while Modal runs the script.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, start) = (done.clone(), std::time::Instant::now());
        let watcher = std::thread::scope(|scope| {
            let w = scope.spawn(|| {
                let mut seen = vec![];
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let s = idle.status().unwrap_or_else(|| "removed".into());
                    if seen.last().map(|(x, _): &(String, u64)| x != &s).unwrap_or(true) { eprintln!("[{:>3}s] runner: {s}", start.elapsed().as_secs()); seen.push((s, start.elapsed().as_secs())) }
                    std::thread::sleep(std::time::Duration::from_secs(15));
                }
                seen
            });
            let mut c = Client::new(Some(session.clone())).unwrap();
            let env: pb::EnvironmentGetOrCreateResponse = c.unary("EnvironmentGetOrCreate", pb::EnvironmentGetOrCreateRequest { deployment_name: String::new() }).unwrap();
            let settings = env.metadata.and_then(|m| m.settings).unwrap_or_default();
            let app: pb::AppGetOrCreateResponse = c.unary("AppGetOrCreate", pb::AppGetOrCreateRequest { app_name: "superci-setup".into(), environment_name: String::new(), object_creation_type: 1 }).unwrap();
            let image: pb::ImageGetOrCreateResponse = c.unary("ImageGetOrCreate", pb::ImageGetOrCreateRequest {
                image: Some(pb::Image { dockerfile_commands: vec!["FROM python:3.12-slim".into(), "RUN pip install --no-cache-dir modal==1.6.0".into()] }),
                app_id: app.app_id.clone(), builder_version: settings.image_builder_version.clone(),
            }).unwrap();
            use base64::Engine;
            let mut vars = std::collections::HashMap::new();
            vars.insert("MODAL_TOKEN_ID".to_string(), session.token_id.clone());
            vars.insert("MODAL_TOKEN_SECRET".to_string(), session.token_secret.clone());
            vars.insert("SCRIPT".to_string(), base64::engine::general_purpose::STANDARD.encode(script));
            vars.insert("JIT".to_string(), idle.jit.clone());
            let sandbox: pb::SandboxCreateResponse = c.unary("SandboxCreate", pb::SandboxCreateRequest { app_id: app.app_id, definition: Some(pb::Sandbox {
                entrypoint_args: vec!["sh".into(), "-c".into(), "echo \"$SCRIPT\" | base64 -d > /tmp/t.py && python /tmp/t.py".into()],
                image_id: image.image_id, resources: Some(pb::Resources { memory_mb: 1024, milli_cpu: 1000 }), timeout_secs: 900,
                environment_variables: Some(pb::StringMap { contents: vars }),
            }) }).unwrap();
            for _ in 0..18 {
                let w: pb::SandboxWaitResponse = c.unary("SandboxWait", pb::SandboxWaitRequest { sandbox_id: sandbox.sandbox_id.clone(), timeout: 50.0 }).unwrap();
                if w.result.is_some_and(|r| r.status != 0) { break }
            }
            let mut logs = |fd: i32| c.stream::<_, pb::TaskLogsBatch>("SandboxGetLogs", pb::SandboxGetLogsRequest { sandbox_id: sandbox.sandbox_id.clone(), file_descriptor: fd, timeout: 5.0, last_entry_id: String::new() }, 200)
                .unwrap_or_default().into_iter().flat_map(|b| b.items).map(|i| i.data).collect::<String>();
            let out = logs(1);
            let err = logs(2);
            // GitHub notices a runner gone within a minute or two.
            for _ in 0..12 { if idle.status().as_deref() != Some("online") { break } std::thread::sleep(std::time::Duration::from_secs(10)) }
            std::thread::sleep(std::time::Duration::from_secs(16));
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            (w.join().unwrap(), out, err)
        });
        let (seen, out, err) = watcher;
        let out = format!("{out}{err}");
        eprintln!("{}", out.lines().filter(|l| l.contains("PROBE") || l.contains("BOUND") || l.contains("BILL") || l.contains("Error") || l.contains("left")).collect::<Vec<_>>().join("\n"));
        if !out.contains("BOUND ended") { eprintln!("{}", out.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")) }
        assert!(out.contains("PROBE blocked"), "a sandbox without a token read the workspace's settings: {out}");
        let secs: u64 = out.split("BOUND ended after ").nth(1).and_then(|s| s.split(' ').next()).and_then(|n| n.parse().ok()).expect("the bound sandbox's end");
        assert!(seen.iter().any(|(s, _)| s == "online"), "its runner never came online");
        assert!(seen.last().is_some_and(|(s, _)| s != "online"), "its runner is still online");
        assert!((170..=240).contains(&secs), "ended after {secs} s");
    }

    /// Runs a Python script in Modal (with Modal's client and this session's token), returning what it printed.
    fn run_script(session: &super::Session, script: &str, vars: &[(&str, &str)]) -> String {
        use super::{pb, Client};
        use base64::Engine;
        let mut c = Client::new(Some(session.clone())).unwrap();
        let env: pb::EnvironmentGetOrCreateResponse = c.unary("EnvironmentGetOrCreate", pb::EnvironmentGetOrCreateRequest { deployment_name: String::new() }).unwrap();
        let settings = env.metadata.and_then(|m| m.settings).unwrap_or_default();
        let app: pb::AppGetOrCreateResponse = c.unary("AppGetOrCreate", pb::AppGetOrCreateRequest { app_name: "superci-setup".into(), environment_name: String::new(), object_creation_type: 1 }).unwrap();
        let image: pb::ImageGetOrCreateResponse = c.unary("ImageGetOrCreate", pb::ImageGetOrCreateRequest {
            image: Some(pb::Image { dockerfile_commands: vec!["FROM python:3.12-slim".into(), "RUN pip install --no-cache-dir modal==1.6.0".into()] }),
            app_id: app.app_id.clone(), builder_version: settings.image_builder_version.clone(),
        }).unwrap();
        let mut env_vars: std::collections::HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        env_vars.insert("MODAL_TOKEN_ID".into(), session.token_id.clone());
        env_vars.insert("MODAL_TOKEN_SECRET".into(), session.token_secret.clone());
        env_vars.insert("SCRIPT".into(), base64::engine::general_purpose::STANDARD.encode(script));
        let sandbox: pb::SandboxCreateResponse = c.unary("SandboxCreate", pb::SandboxCreateRequest { app_id: app.app_id, definition: Some(pb::Sandbox {
            entrypoint_args: vec!["sh".into(), "-c".into(), "echo \"$SCRIPT\" | base64 -d > /tmp/t.py && python /tmp/t.py".into()],
            image_id: image.image_id, resources: Some(pb::Resources { memory_mb: 1024, milli_cpu: 1000 }), timeout_secs: 600,
            environment_variables: Some(pb::StringMap { contents: env_vars }),
        }) }).unwrap();
        for _ in 0..12 {
            let w: pb::SandboxWaitResponse = c.unary("SandboxWait", pb::SandboxWaitRequest { sandbox_id: sandbox.sandbox_id.clone(), timeout: 50.0 }).unwrap();
            if w.result.is_some_and(|r| r.status != 0) { break }
        }
        [1, 2].iter().map(|fd| c.stream::<_, pb::TaskLogsBatch>("SandboxGetLogs", pb::SandboxGetLogsRequest { sandbox_id: sandbox.sandbox_id.clone(), file_descriptor: *fd, timeout: 5.0, last_entry_id: String::new() }, 200)
            .unwrap_or_default().into_iter().flat_map(|b| b.items).map(|i| i.data).collect::<String>()).collect()
    }

    /// What Modal says about this workspace's costs: its rates, this month's summary (metered, billed, credits), and
    /// the last hours by object (SUPERCI_MODAL_TOKEN_ID/SECRET).
    #[test]
    #[ignore]
    fn live_modal_billing() {
        let session = super::Session::from_env().expect("SUPERCI_MODAL_TOKEN_ID and SUPERCI_MODAL_TOKEN_SECRET");
        let out = run_script(&session, r#"
import modal, datetime
w = modal.Workspace.from_context()
for name, call in [("rates", lambda: w.billing.rates()), ("summary", lambda: w.billing.summary()),
                   ("report", lambda: w.billing.report(start=datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=12), resolution="h", tag_names=["*"]))]:
    try:
        r = call(); print("BILLING", name, repr(r) if name != "rates" else "\n".join("BILLING   " + l for l in str(r).splitlines()), flush=True)
    except Exception as e:
        print("BILLING", name, "unavailable:", type(e).__name__, str(e)[:200], flush=True)
"#, &[]);
        eprintln!("{}", out.lines().filter(|l| l.contains("BILLING") || l.contains("Error")).collect::<Vec<_>>().join("\n"));
        assert!(out.contains("BILLING rates"));
    }

    /// This month as Modal bills it, read directly (SUPERCI_MODAL_TOKEN_ID/SECRET).
    #[test]
    #[ignore]
    fn live_modal_month() {
        let m = super::month(&super::Session::from_env().expect("SUPERCI_MODAL_TOKEN_ID and SUPERCI_MODAL_TOKEN_SECRET")).unwrap();
        eprintln!("{m:?}");
        assert!(m.metered >= m.superci && m.metered >= m.billed);
    }

    /// Reaches Modal: starting a sign-in needs no account (run with --ignored).
    #[test]
    #[ignore]
    fn starts_a_sign_in() {
        let p = super::authorize("").expect("Modal answers");
        assert!(p.web_url.starts_with("https://"), "{}", p.web_url);
        println!("sign-in page: {}", p.web_url.split('?').next().unwrap_or_default());
    }
}
