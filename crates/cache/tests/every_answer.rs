//! `superci-cache` as its clients call it (GitHub's cache and artifact clients, Turbo, Nx, Bazel, GitLab's runner,
//! Azure's blob clients), with every answer the control plane, the bucket and GitHub's own service can give it: a
//! refusal, an answer amiss, a body cut short, a connection dropped, a slow answer. What it gives its client back is
//! held to what GitHub's own service would say.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// An answer the stand-in gives instead of its own: a status and body; bytes written as they are (a head promising
/// more than follows, say), then the connection closed; the connection closed with no answer; its own, after a wait.
#[derive(Clone)]
enum Answer { Json(u16, Value), Raw(Vec<u8>), Drop, Slow(u64) }

/// The control plane (`/cache/t/<call>`), the bucket (`…/o/<object>`, ranges too; `/o/gen/<n>/…` an object of `n`
/// zero bytes made as it is read), GitHub's results service (`/github/…`) and Vercel's API (`/vercel/…`), on one port.
/// What a request is answered with instead is said by `instead`: the first that matches its method and a part of its
/// path, taken `times` times (none: every time).
#[derive(Clone, Default)]
struct Stand { port: u16, objects: Arc<Mutex<HashMap<String, Vec<u8>>>>, entries: Arc<Mutex<HashMap<String, Value>>>, instead: Arc<Mutex<Vec<(String, String, Answer, Option<usize>)>>>, asked: Arc<Mutex<Vec<String>>> }

impl Stand {
    fn start() -> Stand {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stand = Stand { port: listener.local_addr().unwrap().port(), ..Default::default() };
        let s = stand.clone();
        std::thread::spawn(move || for stream in listener.incoming().flatten() { let s = s.clone(); std::thread::spawn(move || s.connection(stream)); });
        stand
    }

    fn url(&self) -> String { format!("http://127.0.0.1:{}", self.port) }

    /// `answer` for the next `times` requests (none: all) of `method` whose path has `part` in it.
    fn instead(&self, method: &str, part: &str, answer: Answer, times: Option<usize>) { self.instead.lock().unwrap().push((method.into(), part.into(), answer, times)) }
    fn calm(&self) { self.instead.lock().unwrap().clear() }

    fn connection(&self, stream: TcpStream) {
        let mut r = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 { return }
            let mut p = line.split_whitespace();
            let (method, path) = (p.next().unwrap_or_default().to_string(), p.next().unwrap_or_default().to_string());
            let (mut len, mut range) = (0usize, None::<String>);
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() { break }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap_or(0) }
                    if k.eq_ignore_ascii_case("range") { range = Some(v.trim().to_string()) }
                }
            }
            let mut body = vec![0; len];
            if r.read_exact(&mut body).is_err() { return }
            self.asked.lock().unwrap().push(format!("{method} {path}"));
            let mut s = &stream;
            let chosen = {
                let mut instead = self.instead.lock().unwrap();
                let at = instead.iter().position(|(m, part, _, _)| *m == method && path.contains(part.as_str()));
                at.map(|i| {
                    let a = instead[i].2.clone();
                    if let Some(n) = &mut instead[i].3 { *n -= 1; if *n == 0 { instead.remove(i); } }
                    a
                })
            };
            let (status, kind, out) = match chosen {
                Some(Answer::Drop) => return,
                Some(Answer::Raw(bytes)) => { let _ = s.write_all(&bytes); let _ = s.shutdown(Shutdown::Both); return }
                Some(Answer::Json(status, v)) => (status, "application/json", v.to_string().into_bytes()),
                Some(Answer::Slow(ms)) => { std::thread::sleep(Duration::from_millis(ms)); self.own(&method, &path, body, range.as_deref()) }
                None => self.own(&method, &path, body, range.as_deref()),
            };
            let partial = if status == 206 { format!("content-range: bytes {}\r\n", range.as_deref().unwrap_or_default().trim_start_matches("bytes=")) } else { String::new() };
            if s.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\ncontent-type: {kind}\r\n{partial}\r\n", out.len()).as_bytes()).and_then(|_| s.write_all(&out)).is_err() { return }
        }
    }

    /// What it answers of its own: as the control plane, the bucket and GitHub's services do.
    fn own(&self, method: &str, path: &str, body: Vec<u8>, range: Option<&str>) -> (u16, &'static str, Vec<u8>) {
        if let Some(rest) = path.split_once("/github/").map(|(_, r)| r) { return (200, "application/json", json!({ "ok": true, "github": rest }).to_string().into_bytes()) }
        if let Some(rest) = path.split_once("/vercel/").map(|(_, r)| r) { return (200, "application/json", json!({ "vercel": rest }).to_string().into_bytes()) }
        if let Some((_, name)) = path.split_once("/o/") {
            let name = name.split('?').next().unwrap().to_string();
            if method == "PUT" { self.objects.lock().unwrap().insert(name, body); return (200, "application/xml", vec![]) }
            let whole = match name.strip_prefix("gen/").and_then(|g| g.split('/').next()).and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => vec![0u8; n],
                None => match self.objects.lock().unwrap().get(&name) { Some(b) => b.clone(), None => return (404, "application/xml", b"<Error><Code>NoSuchKey</Code></Error>".to_vec()) },
            };
            return match range.and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')).and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))) {
                Some((a, b)) => (206, "application/octet-stream", whole[a.min(whole.len())..(b + 1).min(whole.len())].to_vec()),
                None => (200, "application/octet-stream", whole),
            }
        }
        let b: Value = serde_json::from_slice(&body).unwrap_or_default();
        let what = path.rsplit('/').next().unwrap().to_string();
        let k = format!("{}:{}", b["tool"].as_str().unwrap_or(if what.starts_with("artifact") { "artifact" } else { "cache" }), b["key"].as_str().or(b["hash"].as_str()).or(b["name"].as_str()).unwrap_or_default());
        let link = |n: &str| format!("{}/o/{}/{n}", self.url(), k.replace(['/', ':'], "_"));
        let mut e = self.entries.lock().unwrap();
        let pieces_of = |p: &Value| p.as_array().unwrap().iter().map(|x| json!({ "url": link(x[0].as_str().unwrap()), "size": x[1] })).collect::<Vec<_>>();
        let reply = match what.as_str() {
            "begin" | "artifact-begin" => json!({ "ok": true }),
            "tool-begin" => json!({ "ok": true, "url": link("p00000") }),
            "sign" | "tool-sign" | "artifact-sign" => json!({ "ok": true, "urls": b["names"].as_array().unwrap().iter().map(|n| link(n.as_str().unwrap())).collect::<Vec<_>>() }),
            "finish" | "tool-finish" | "artifact-finish" => { e.insert(k.clone(), b["pieces"].clone()); e.insert(format!("{k}:meta"), b["meta"].clone()); json!({ "ok": true, "id": 7 }) }
            "lookup" | "tool-get" => match e.get(&k) {
                Some(p) => json!({ "hit": true, "key": b["key"], "size": p.as_array().unwrap().iter().map(|x| x[1].as_u64().unwrap()).sum::<u64>(), "pieces": pieces_of(p), "meta": e.get(&format!("{k}:meta")).cloned().unwrap_or_default() }),
                None => json!({ "hit": false }),
            },
            "artifact-get" => match e.get(&k) { Some(p) => json!({ "pieces": pieces_of(p) }), None => return (404, "application/json", json!({ "why": "no such artifact" }).to_string().into_bytes()) },
            "artifacts" => json!({ "artifacts": e.keys().filter(|k| k.starts_with("artifact:") && !k.ends_with(":meta")).map(|k| json!({ "id": 7, "name": &k["artifact:".len()..], "size": 3, "made": 1_790_000_000_000u64 })).collect::<Vec<_>>() }),
            "artifact-delete" => json!({ "id": 7 }),
            _ => json!({}),
        };
        (200, "application/json", reply.to_string().into_bytes())
    }
}

/// The program, ended when the test ends.
struct Ends(std::process::Child);
impl Drop for Ends { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

fn folder(name: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!("superci-cache-every-{}-{name}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn command() -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache"));
    for k in ["SUPERCI_CACHE_PLANE", "SUPERCI_CACHE_LISTEN", "SUPERCI_CACHE_PORT", "SUPERCI_CACHE_GITHUB", "SUPERCI_CACHE_VERCEL", "SUPERCI_CACHE_DEBUG", "SUPERCI_STAND_IN"] { c.env_remove(k); }
    c
}

/// `superci-cache serve` against the stand-in (GitHub's service and Vercel's too), with `env` besides: its port.
fn serve(stand: &Stand, env: &[(&str, &str)]) -> (u16, Ends) {
    let dir = folder("serve");
    let port = superci_testkit::free_port();
    let mut c = command();
    c.args(["serve", dir.to_str().unwrap()]).env("SUPERCI_CACHE_PLANE", format!("{}/cache/t", stand.url())).env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string())
        .env("SUPERCI_CACHE_GITHUB", format!("{}/github", stand.url())).env("SUPERCI_CACHE_VERCEL", format!("{}/vercel", stand.url()));
    for (k, v) in env { c.env(k, v); }
    let child = c.spawn().unwrap();
    for _ in 0..200 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(Duration::from_millis(20)) }
    (port, Ends(child))
}

/// A request: status, head (lower case), body.
fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nhost: x\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
    s.write_all(body).unwrap();
    raw_answer(s)
}

/// What came back on a connection, its head and body apart (a status of 0: nothing came).
fn raw_answer(mut s: TcpStream) -> (u16, String, Vec<u8>) {
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    let Some(at) = out.windows(4).position(|w| w == b"\r\n\r\n") else { return (0, String::from_utf8_lossy(&out).into(), vec![]) };
    (String::from_utf8_lossy(&out[9..12]).parse().unwrap_or(0), String::from_utf8_lossy(&out[..at]).to_lowercase(), out[at + 4..].to_vec())
}

/// Bytes sent as they are (and the sending side closed): what came back.
fn raw(port: u16, bytes: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(bytes).unwrap();
    let _ = s.shutdown(Shutdown::Write);
    raw_answer(s)
}

/// A job's token of GitHub's (RS256, its header written with `-` and `_` as base64url has them) and a run from a
/// computer's (its control plane's, ES256).
const GITHUB_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6Ij4-Pz8_In0.e30.x";
const COMPUTER_TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.e30.x";

fn twirp(port: u16, token: &str, service: &str, op: &str, body: Value) -> (u16, Value) {
    let (s, _, b) = call(port, "POST", &format!("/twirp/github.actions.results.api.v1.{service}/{op}"), &[("authorization", &format!("Bearer {token}"))], body.to_string().as_bytes());
    (s, serde_json::from_slice(&b).unwrap_or_default())
}

fn path_of(port: u16, url: &Value) -> String { url.as_str().unwrap().split_once(&format!(":{port}")).unwrap().1.to_string() }

/// GitHub's cache service with each answer of the control plane: found (its pieces said amiss too), not found, refused,
/// not reached (and reached the second time), for a job of GitHub's (which then uses GitHub's own cache) and a run
/// from a computer's; a save begun, refused, there already, not taken; one finished with the wrong size, finished
/// before its upload, refused at the end; and what is not the cache, on to GitHub.
#[test]
fn githubs_cache_service_says_each_answer_of_the_control_plane_as_github_would() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[]);
    let cache = |token: &str, op: &str, body: Value| twirp(port, token, "CacheService", op, body);
    let key = json!({ "key": "deps", "version": "v1" });
    // A hit whose pieces are said amiss (one with no size): what can be read is given.
    stand.instead("POST", "/lookup", Answer::Json(200, json!({ "hit": true, "key": "deps", "size": 3, "pieces": [{ "url": format!("{}/o/gen/3/x", stand.url()), "size": 3 }, { "url": "no size" }, { "size": 4 }] })), Some(1));
    let (_, found) = cache(COMPUTER_TOKEN, "GetCacheEntryDownloadURL", key.clone());
    assert_eq!(call(port, "GET", &path_of(port, &found["signed_download_url"]), &[], b"").2, vec![0u8; 3]);
    // Not found: GitHub's own cache asked for a job of GitHub's.
    assert_eq!(cache(GITHUB_TOKEN, "GetCacheEntryDownloadURL", key.clone()).1["github"], "twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL");
    // Refused, or not reached: GitHub's own for a job of GitHub's; nothing for a run's.
    for answer in [Answer::Json(403, json!({ "why": "a branch SuperCI does not keep" })), Answer::Json(503, json!({}))] {
        stand.instead("POST", "/lookup", answer, None);
        assert_eq!(cache(GITHUB_TOKEN, "GetCacheEntryDownloadURL", key.clone()).1["ok"], true);
        assert_eq!(cache(COMPUTER_TOKEN, "GetCacheEntryDownloadURL", key.clone()).1, json!({ "ok": false }));
        stand.calm();
    }
    // Not reached once: asked again, and answered.
    stand.instead("POST", "/lookup", Answer::Drop, Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "GetCacheEntryDownloadURL", key.clone()).1, json!({ "ok": false }));
    // A save: refused as it begins (a run's: said; one of GitHub's: saved in GitHub's own cache, and finished there).
    stand.instead("POST", "/begin", Answer::Json(200, json!({ "ok": false, "why": "too large" })), Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "CreateCacheEntry", key.clone()).1["message"], "SuperCI's cache: too large");
    stand.instead("POST", "/begin", Answer::Json(400, json!({ "why": "a key SuperCI does not take" })), Some(1));
    assert_eq!(cache(GITHUB_TOKEN, "CreateCacheEntry", json!({ "key": "odd", "version": "v1" })).1["ok"], true);
    assert_eq!(cache(GITHUB_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "odd", "version": "v1" })).1["github"], "twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload");
    stand.instead("POST", "/begin", Answer::Json(400, json!({ "why": "a key SuperCI does not take" })), Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "CreateCacheEntry", json!({ "key": "odd", "version": "v1" })).1["ok"], false, "a run's: not saved");
    stand.instead("POST", "/begin", Answer::Json(409, json!({})), Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "CreateCacheEntry", key.clone()), (409, json!({ "code": "already_exists", "msg": "cache entry with the same key, version, and scope already exists" })));
    stand.instead("POST", "/begin", Answer::Json(503, json!({})), None);
    assert_eq!(cache(GITHUB_TOKEN, "CreateCacheEntry", json!({ "key": "down", "version": "v1" })).1["ok"], true, "GitHub's own");
    assert_eq!(cache(COMPUTER_TOKEN, "CreateCacheEntry", key.clone()).1, json!({ "ok": false }));
    stand.calm();
    // (An artifact being uploaded beside it, of the same name, is not it.)
    let (_, art) = twirp(port, COMPUTER_TOKEN, "ArtifactService", "CreateArtifact", json!({ "name": "deps" }));
    assert_eq!(call(port, "PUT", &path_of(port, &art["signed_upload_url"]), &[], b"zip").0, 201);
    // Saved: one of another version is not the one finished; the size said is not the size uploaded; the end refused.
    let (_, made) = cache(COMPUTER_TOKEN, "CreateCacheEntry", key.clone());
    assert_eq!(call(port, "PUT", &path_of(port, &made["signed_upload_url"]), &[], b"archive").0, 201);
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v2" })).1, json!({ "ok": false }));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v1", "size_bytes": 99 })).1, json!({ "ok": false }));
    stand.instead("POST", "/finish", Answer::Json(400, json!({ "why": "its pieces are not there" })), Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v1", "size_bytes": "7" })).1, json!({ "ok": false }));
    stand.instead("POST", "/finish", Answer::Json(200, json!({ "ok": false })), Some(1));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v1" })).1, json!({ "ok": false }));
    // (Begun, nothing uploaded; and a key nobody began: neither finished.)
    cache(COMPUTER_TOKEN, "CreateCacheEntry", json!({ "key": "nothing", "version": "v1" }));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "nothing", "version": "v1" })).1, json!({ "ok": false }));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "nobody", "version": "v1" })).1, json!({ "ok": false }));
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v1" })).1["ok"], true);
    // Finished: not finished again.
    assert_eq!(cache(COMPUTER_TOKEN, "FinalizeCacheEntryUpload", json!({ "key": "deps", "version": "v1" })).1, json!({ "ok": false }));
    // Finished: its upload takes nothing more.
    assert_eq!(call(port, "PUT", &path_of(port, &made["signed_upload_url"]), &[], b"more").0, 404);
    // What is not the cache: GitHub's own service for a job of GitHub's (artifacts too); not on SuperCI for a run's.
    assert_eq!(cache(GITHUB_TOKEN, "Other", json!({})).1["ok"], true);
    assert_eq!(twirp(port, GITHUB_TOKEN, "ArtifactService", "CreateArtifact", json!({ "name": "x" })).1["github"], "twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact");
    assert_eq!(cache(COMPUTER_TOKEN, "Other", json!({})).0, 501);
}

/// A run from a computer's artifacts with each answer of the control plane: kept and listed (by name, by id), read,
/// deleted; refused as each step is asked; one finished where only a cache entry of its name was uploaded.
#[test]
fn a_runs_artifacts_say_each_answer_of_the_control_plane_as_twirp_does() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[]);
    let artifacts = |op: &str, body: Value| twirp(port, COMPUTER_TOKEN, "ArtifactService", op, body);
    let (_, made) = artifacts("CreateArtifact", json!({ "name": { "value": "report" } }));
    assert_eq!(call(port, "PUT", &path_of(port, &made["signed_upload_url"]), &[], b"zip").0, 201);
    assert_eq!(artifacts("FinalizeArtifact", json!({ "name": "report" })).1, json!({ "ok": true, "artifact_id": "7" }));
    let (_, list) = artifacts("ListArtifacts", json!({ "name_filter": { "value": "report" }, "id_filter": "7" }));
    assert_eq!(list["artifacts"][0]["name"], "report", "{list}");
    assert_eq!(artifacts("ListArtifacts", json!({ "name_filter": "other" })).1, json!({ "artifacts": [] }));
    assert_eq!(artifacts("ListArtifacts", json!({ "id_filter": "8" })).1, json!({ "artifacts": [] }));
    let (_, signed) = artifacts("GetSignedArtifactURL", json!({ "name": "report" }));
    assert_eq!(call(port, "GET", &path_of(port, &signed["signed_url"]), &[], b"").2, b"zip");
    assert_eq!(artifacts("DeleteArtifact", json!({ "name": "report" })).1, json!({ "ok": true, "artifact_id": "7" }));
    // Pieces said amiss (one with no size): left out.
    stand.instead("POST", "/artifact-get", Answer::Json(200, json!({ "pieces": [{ "url": "x" }, { "size": 1 }] })), Some(1));
    assert!(artifacts("GetSignedArtifactURL", json!({ "name": "report" })).1["signed_url"].is_string());
    // Refused at each step (400: said as a failed precondition, or internal).
    for (call_name, op) in [("artifact-begin", "CreateArtifact"), ("artifacts", "ListArtifacts"), ("artifact-get", "GetSignedArtifactURL"), ("artifact-delete", "DeleteArtifact")] {
        stand.instead("POST", call_name, Answer::Json(400, json!({ "why": "refused" })), Some(1));
        let (status, said) = artifacts(op, json!({ "name": "report" }));
        assert_eq!((status, said["msg"].as_str()), (400, Some("refused")), "{op}: {said}");
    }
    // Uploaded, its end refused; a cache entry of its name is not it.
    let (_, made) = artifacts("CreateArtifact", json!({ "name": "logs" }));
    assert_eq!(call(port, "PUT", &path_of(port, &made["signed_upload_url"]), &[], b"zip").0, 201);
    stand.instead("POST", "/artifact-finish", Answer::Json(400, json!({ "why": "too large" })), Some(1));
    assert_eq!(artifacts("FinalizeArtifact", json!({ "name": "logs" })).0, 400);
    let (_, made) = twirp(port, COMPUTER_TOKEN, "CacheService", "CreateCacheEntry", json!({ "key": "cached", "version": "v1" }));
    assert_eq!(call(port, "PUT", &path_of(port, &made["signed_upload_url"]), &[], b"tar").0, 201);
    assert_eq!(artifacts("FinalizeArtifact", json!({ "name": "cached" })).1["msg"], "finished before its upload");
    // Begun and finished with nothing uploaded.
    artifacts("CreateArtifact", json!({ "name": "empty" }));
    assert_eq!(artifacts("FinalizeArtifact", json!({ "name": "empty" })).1["msg"], "finished before its upload");
    assert_eq!(artifacts("Migrate", json!({})).0, 501);
}

/// Turbo, Nx, Bazel and GitLab's runner with each answer of the control plane and the bucket: not reached, refused,
/// there already; a large output in pieces (more links asked for, refused, too few); a piece the bucket will not take;
/// the end refused. What each tool is told is what it expects.
#[test]
fn the_tools_caches_say_each_answer_of_the_control_plane_and_the_bucket() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[]);
    let tool = [("authorization", "Bearer superci")];
    let nx = |method: &str, hash: &str, body: &[u8]| call(port, method, &format!("/v1/cache/{hash}"), &[], body).0;
    // Looking up: not reached, refused, its pieces not said.
    for answer in [Answer::Json(503, json!({})), Answer::Json(403, json!({})), Answer::Json(200, json!({ "hit": true, "pieces": "none" }))] {
        stand.instead("POST", "/tool-get", answer, None);
        assert_eq!(nx("GET", "a1", b""), 404);
        stand.calm();
    }
    // Its pieces said amiss (one with no size, one with no link): what is said is given, here nothing.
    stand.instead("POST", "/tool-get", Answer::Json(200, json!({ "hit": true, "size": 0, "pieces": [{ "url": "x" }, { "size": 1 }] })), Some(1));
    assert_eq!(call(port, "GET", "/v1/cache/a1", &[], b"").2.len(), 0);
    // Keeping: not reached, there already, no link given, refused at the end (or not reached), the bucket refusing.
    stand.instead("POST", "/tool-begin", Answer::Json(503, json!({})), None);
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.calm();
    stand.instead("POST", "/tool-begin", Answer::Json(409, json!({})), Some(1));
    assert_eq!(nx("PUT", "a1", b"out"), 409);
    stand.instead("POST", "/tool-begin", Answer::Json(200, json!({})), Some(1));
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.instead("POST", "/tool-begin", Answer::Json(400, json!({ "url": "a link, refused" })), Some(1));
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.instead("POST", "/tool-finish", Answer::Json(400, json!({ "why": "no" })), Some(1));
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.instead("POST", "/tool-finish", Answer::Json(503, json!({})), None);
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.calm();
    stand.instead("PUT", "/o/", Answer::Json(500, json!({})), None);
    assert_eq!(nx("PUT", "a1", b"out"), 500);
    stand.calm();
    assert_eq!(nx("DELETE", "a1", b""), 405);
    // Larger than a piece: more links asked for (refused, too few, not reached), then kept whole and given back.
    let big = vec![7u8; (64 << 20) + 5];
    for answer in [Answer::Json(400, json!({})), Answer::Json(200, json!({ "urls": ["only one"] })), Answer::Json(503, json!({}))] {
        stand.instead("POST", "/tool-sign", answer, None);
        assert_eq!(nx("PUT", "big", &big), 500);
        stand.calm();
    }
    assert_eq!(nx("PUT", "big", &big), 200);
    let (status, _, got) = call(port, "GET", "/v1/cache/big", &[], b"");
    assert!(status == 200 && got == big);
    // Turbo: its token's events, which of none are kept, an artifact without its duration, one with no hash, one the
    // control plane does not keep; a request with another token on to Vercel, its query kept.
    assert_eq!(call(port, "POST", "/v8/artifacts/events", &tool, b"[]").0, 200);
    assert_eq!(call(port, "POST", "/v8/artifacts", &tool, b"{}").2, b"{}");
    assert_eq!(call(port, "PUT", "/v8/artifacts/t1", &tool, b"turbo").0, 202);
    let (status, head, _) = call(port, "GET", "/v8/artifacts/t1", &tool, b"");
    assert!(status == 200 && !head.contains("x-artifact-duration"), "{head}");
    assert_eq!(call(port, "PUT", "/v8/artifacts/t2", &[tool[0], ("x-artifact-tag", "abc")], b"turbo").0, 202);
    assert!(call(port, "HEAD", "/v8/artifacts/t2", &tool, b"").1.contains("x-artifact-tag: abc"));
    assert_eq!(call(port, "GET", "/v8/artifacts/", &tool, b"").0, 404);
    assert_eq!(call(port, "PUT", "/v8/artifacts/", &tool, b"x").0, 404);
    stand.instead("POST", "/tool-begin", Answer::Json(400, json!({})), Some(1));
    assert_eq!(call(port, "PUT", "/v8/artifacts/t3", &tool, b"turbo").0, 500);
    let (_, _, said) = call(port, "GET", "/v8/artifacts/t9?slug=acme", &[("authorization", "Bearer vercel-token")], b"");
    assert_eq!(serde_json::from_slice::<Value>(&said).unwrap()["vercel"], "v8/artifacts/t9?slug=acme");
    // Bazel: a path with no kind, another method; GitLab: another method, keys with what is not percent-encoding.
    assert_eq!(call(port, "GET", "/bazel/0f1e", &[], b"").0, 404);
    assert_eq!(call(port, "DELETE", "/bazel/cas/0f1e", &[], b"").0, 405);
    assert_eq!(call(port, "POST", "/superci/gl/project/1/c.zip", &[], b"").0, 405);
    assert_eq!(call(port, "PUT", "/superci/gl/project/1/%zz%4", &[], b"zip").0, 200);
    assert_eq!(call(port, "GET", "/superci/gl/project/1/%zz%4", &[], b"").2, b"zip");
    stand.instead("POST", "/tool-begin", Answer::Json(400, json!({})), Some(1));
    assert_eq!(call(port, "PUT", "/superci/gl/project/1/d.zip", &[], b"zip").0, 500);
    // Sent in one request larger than the program holds (GitLab's runner's, Bazel's), cut off midway: not kept.
    for path in ["/superci/gl/project/1/huge.zip", "/bazel/cas/huge"] {
        let (status, _, _) = raw(port, format!("PUT {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\nsome of it", 300u64 << 20).as_bytes());
        assert_eq!(status, 500, "{path}");
    }
}

/// GitHub's client uploading as to Azure's blob storage, with each answer of the control plane and the bucket: links
/// refused or not reached, a piece or a block the bucket does not take, blocks faster than the bucket takes them,
/// blocks listed amiss; and the program's own lines for finding a fault.
#[test]
fn an_upload_says_each_answer_of_the_control_plane_and_the_bucket() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[("SUPERCI_CACHE_DEBUG", "1")]);
    let begin = |key: &str| path_of(port, &twirp(port, COMPUTER_TOKEN, "CacheService", "CreateCacheEntry", json!({ "key": key, "version": "v1" })).1["signed_upload_url"]);
    let put = |path: &str, body: &[u8]| call(port, "PUT", path, &[], body).0;
    let up = begin("a");
    assert_eq!(call(port, "POST", &up, &[], b"").0, 405);
    assert_eq!(put("/superci/u/none", b"x"), 404);
    assert_eq!(put(&format!("{up}?comp=block&blockid="), b"x"), 400);
    // (A query with what is not percent-encoding, and a name with no value.)
    assert_eq!(put(&format!("{up}?comp=blocklist&x=%zz&y=%4&z="), b"<BlockList><Latest>none</Latest></BlockList>"), 400);
    // Links refused, too few, not reached.
    for answer in [Answer::Json(400, json!({ "why": "no" })), Answer::Json(200, json!({ "urls": [] })), Answer::Json(503, json!({}))] {
        stand.instead("POST", "/sign", answer, None);
        assert_eq!(put(&begin("b"), b"x"), 500);
        stand.calm();
    }
    // The bucket refusing a piece (asked four times), or out of reach; a block it refuses, said when they are listed.
    stand.instead("PUT", "/o/", Answer::Json(503, json!({})), None);
    assert_eq!(put(&begin("c"), b"x"), 500);
    let blocks = begin("d");
    assert_eq!(put(&format!("{blocks}?comp=block&blockid=YQ%3D%3D"), b"x"), 201);
    assert_eq!(put(&format!("{blocks}?comp=blocklist"), b"<BlockList><Latest>YQ==</Latest></BlockList>"), 500);
    stand.calm();
    stand.instead("PUT", "/o/", Answer::Drop, Some(1));
    assert_eq!(put(&begin("e"), b"x"), 201, "asked again");
    // Blocks faster than the bucket takes them (more than the program stores at once): waited for, all kept.
    stand.instead("PUT", "/o/", Answer::Slow(150), None);
    let many = begin("f");
    let ids: Vec<String> = (0..40).map(|i| format!("{i:04}")).collect();
    std::thread::scope(|s| for id in &ids { let many = many.clone(); s.spawn(move || assert_eq!(put(&format!("{many}?comp=block&blockid={id}"), id.as_bytes()), 201)); });
    let list: String = ids.iter().map(|i| format!("<Latest>{i}</Latest>")).collect();
    assert_eq!(put(&format!("{many}?comp=blocklist"), format!("<BlockList>{list}</BlockList>").as_bytes()), 201);
    stand.calm();
    // Listed with none of its blocks: refused.
    assert_eq!(put(&format!("{}?comp=blocklist", begin("g")), b"<BlockList></BlockList>"), 400);
}

/// A download with each answer of the bucket: a range not satisfiable or not said right, a piece answered whole where
/// a part was asked, cut short, broken off, out of reach, refused once; an entry of no pieces; one only asked about;
/// a large one broken off by its client; a link to the bucket the stand-in stands in for.
#[test]
fn a_download_says_each_answer_of_the_bucket() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[("SUPERCI_STAND_IN", &stand.url())]);
    let hit = |pieces: Value, size: u64| -> String {
        stand.instead("POST", "/lookup", Answer::Json(200, json!({ "hit": true, "key": "k", "size": size, "pieces": pieces })), Some(1));
        path_of(port, &twirp(port, COMPUTER_TOKEN, "CacheService", "GetCacheEntryDownloadURL", json!({ "key": "k", "version": "v1" })).1["signed_download_url"])
    };
    let gen = |n: u64| json!([{ "url": format!("{}/o/gen/{n}/x", stand.url()), "size": n }]);
    assert_eq!(call(port, "GET", "/superci/d/none", &[], b"").0, 404);
    let ten = hit(gen(10), 10);
    let (status, head, body) = call(port, "HEAD", &ten, &[], b"");
    assert!(status == 200 && head.contains("content-length: 10") && body.is_empty(), "{head}");
    for range in ["bytes=x-1", "bytes", "bytes=5", "bytes=5-2", "bytes=3-x"] { assert_eq!(call(port, "GET", &ten, &[("range", range)], b"").0, 416, "{range}") }
    let (status, _, got) = call(port, "GET", &ten, &[("range", "bytes=2-4")], b"");
    assert_eq!((status, got.len()), (206, 3));
    // No pieces: nothing to give; a range of it not satisfiable.
    let none = hit(json!([]), 0);
    assert_eq!(call(port, "GET", &none, &[], b"").2.len(), 0);
    assert_eq!(call(port, "GET", &none, &[("range", "bytes=0-")], b"").0, 416);
    assert_eq!(call(port, "GET", &none, &[("range", "bytes=0-3")], b"").0, 416);
    // Through the stand-in for the bucket's own address.
    let amazon = hit(json!([{ "url": "https://superci-plane-p.s3.us-east-1.amazonaws.com/o/gen/4/x", "size": 4 }]), 4);
    assert_eq!(call(port, "GET", &amazon, &[], b"").2, vec![0u8; 4]);
    // The bucket answering amiss: whole where a part was asked, cut short, a head promising more, nothing, out of reach.
    let broken = |answer: Answer, range: Option<&str>| -> usize {
        stand.instead("GET", "/o/gen/", answer, None);
        let p = hit(gen(10), 10);
        let headers: Vec<(&str, &str)> = range.map(|r| vec![("range", r)]).unwrap_or_default();
        let got = call(port, "GET", &p, &headers, b"").2.len();
        stand.calm();
        got
    };
    assert!(broken(Answer::Json(200, json!("whole")), Some("bytes=2-4")) < 3);
    assert!(broken(Answer::Raw(b"HTTP/1.1 206 X\r\ncontent-length: 2\r\n\r\nab".to_vec()), None) < 10);
    assert!(broken(Answer::Raw(b"HTTP/1.1 206 X\r\ncontent-length: 10\r\n\r\nab".to_vec()), None) < 10);
    assert!(broken(Answer::Drop, None) < 10);
    let gone = superci_testkit::free_port();
    let away = hit(json!([{ "url": format!("http://127.0.0.1:{gone}/o/x"), "size": 10 }]), 10);
    assert!(call(port, "GET", &away, &[], b"").2.len() < 10);
    // Refused once: asked again, and given.
    stand.instead("GET", "/o/gen/", Answer::Json(503, json!({})), Some(1));
    assert_eq!(call(port, "GET", &hit(gen(10), 10), &[], b"").2.len(), 10);
    // A tool's output said larger than its pieces: the download breaks off where they end.
    stand.instead("POST", "/tool-get", Answer::Json(200, json!({ "hit": true, "size": 20, "pieces": gen(10) })), Some(1));
    assert!(call(port, "GET", "/v1/cache/short", &[], b"").2.len() < 20);
    // A large one, read from a little way in and broken off by its client: said as broken off.
    let large = hit(gen(70 << 20), 70 << 20);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("GET {large} HTTP/1.1\r\nhost: x\r\nrange: bytes=1-\r\n\r\n").as_bytes()).unwrap();
    let mut first = [0u8; 4096];
    let _ = s.read(&mut first);
    drop(s);
    std::thread::sleep(Duration::from_millis(300));
}

/// Requests as a client may send them amiss: an empty line first, a line that is not text, a header with no colon,
/// a head or body cut short, chunks said amiss or larger than taken, HTTP/1.0 (closed after), a body larger than
/// held sent to what does not read it, no headers at all (on to Vercel as it came), a client gone before its answer.
#[test]
fn requests_sent_amiss_are_answered_or_let_go() {
    let stand = Stand::start();
    let (port, _p) = serve(&stand, &[]);
    let status = b"GET /v8/artifacts/status HTTP/1.1\r\nauthorization: Bearer superci\r\nconnection: close\r\n\r\n";
    assert_eq!(raw(port, &[b"\r\n".as_slice(), status].concat()).0, 200, "an empty line first");
    assert_eq!(raw(port, b"\xff\xfe /x HTTP/1.1\r\n\r\n").0, 0);
    assert_eq!(raw(port, b"GET /v8/artifacts/status HTTP/1.1\r\nno colon here\r\nauthorization: Bearer superci\r\nconnection: close\r\n\r\n").0, 200);
    assert_eq!(raw(port, b"GET /x HTTP/1.1\r\nhost: x\r\n").0, 0, "its head cut short");
    assert_eq!(raw(port, b"GET /x HTTP/1.1\r\nx: \xff\r\n\r\n").0, 0);
    assert_eq!(raw(port, b"PUT /v1/cache/c HTTP/1.1\r\ncontent-length: 10\r\n\r\nabc").0, 0, "its body cut short");
    for chunks in [&b"zz\r\n"[..], b"", b"\xff\r\n", b"5\r\nab", b"2\r\nab", b"40000001\r\n", b"2\r\nab\r\n0\r\n\xff\r\n"] {
        assert_eq!(raw(port, &[&b"PUT /v1/cache/c HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"[..], chunks].concat()).0, 0, "{}", String::from_utf8_lossy(chunks));
    }
    // (Its last chunk with the connection ended where trailers would be: the body is whole.)
    assert_eq!(raw(port, b"PUT /v1/cache/c HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n2\r\nab\r\n0\r\n").0, 200);
    assert_eq!(raw(port, b"PUT /v1/cache/c2 HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n2\r\nab\r\n0\r\nx-trailer: a\r\n\r\n").0, 200, "a trailer");
    // A token with no header to read, and one with base64's own `+` and `/`.
    assert_eq!(call(port, "POST", "/twirp/github.actions.results.api.v1.CacheService/Other", &[], b"{}").0, 501);
    assert_eq!(call(port, "POST", "/twirp/github.actions.results.api.v1.CacheService/Other", &[("authorization", "Bearer ab+/cd.e30.x")], b"{}").0, 501);
    let (s, head, _) = raw(port, b"GET /v8/artifacts/status HTTP/1.0\r\nauthorization: Bearer superci\r\n\r\n");
    assert!(s == 200 && head.contains("connection: close"), "{head}");
    let (s, head, _) = raw(port, format!("GET /superci/d/none HTTP/1.1\r\ncontent-length: {}\r\n\r\n", 300u64 << 20).as_bytes());
    assert!(s == 404 && head.contains("connection: close"), "{head}");
    let (s, _, said) = raw(port, b"GET /v8/artifacts/x HTTP/1.0\r\n\r\n");
    assert_eq!((s, serde_json::from_slice::<Value>(&said).unwrap()["vercel"].as_str()), (200, Some("v8/artifacts/x")));
    // GitHub's service answering what cannot be sent on (a header no request can carry is refused before), forbidden,
    // cut short, larger than taken, or out of reach.
    let token = format!("Bearer {GITHUB_TOKEN}");
    let forward = |answer: Option<Answer>, extra: &[(&str, &str)]| {
        if let Some(a) = answer { stand.instead("POST", "/github/", a, Some(1)) }
        let mut h = vec![("authorization", token.as_str())];
        h.extend_from_slice(extra);
        call(port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts", &h, b"{}").0
    };
    assert_eq!(forward(None, &[("x-odd", "a\u{1}b")]), 500);
    assert_eq!(forward(Some(Answer::Json(403, json!({}))), &[]), 403);
    assert_eq!(forward(Some(Answer::Raw(b"HTTP/1.1 200 X\r\ncontent-length: 10\r\n\r\nab".to_vec())), &[]), 500);
    let huge = format!("HTTP/1.1 200 X\r\ncontent-length: {}\r\n\r\n", 65u64 << 20).into_bytes();
    assert_eq!(forward(Some(Answer::Raw([huge, vec![b'a'; 65 << 20]].concat())), &[]), 500);
    // A client gone before a large answer is given.
    stand.instead("POST", "/github/", Answer::Json(200, json!("a".repeat(4 << 20))), Some(1));
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("POST /twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts HTTP/1.1\r\nauthorization: {token}\r\ncontent-length: 2\r\n\r\n{{}}").as_bytes()).unwrap();
    drop(s);
    std::thread::sleep(Duration::from_millis(500));
    let (gone_port, _g) = { let stand = Stand::start(); let gone = superci_testkit::free_port(); serve(&stand, &[("SUPERCI_CACHE_GITHUB", &format!("http://127.0.0.1:{gone}"))]) };
    assert_eq!(call(gone_port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts", &[("authorization", &token)], b"{}").0, 500);
}

/// How the program is run: with nothing to do, or without what it needs to serve (the control plane's address, an
/// address to listen on, its folder), it says so; on an IPv6 address, or on the machine's own private address.
#[test]
fn the_program_says_what_it_needs_to_serve() {
    let fails = |c: &mut std::process::Command| { let out = c.output().unwrap(); assert!(!out.status.success()); String::from_utf8_lossy(&out.stderr).to_string() };
    assert!(fails(&mut command()).contains("usage: superci-cache wrap|serve"));
    let dir = folder("needs");
    assert!(fails(command().args(["serve", dir.to_str().unwrap()])).contains("SUPERCI_CACHE_PLANE is not set"));
    let plane = [("SUPERCI_CACHE_PLANE", "http://127.0.0.1:9/cache/t")];
    assert!(fails(command().args(["serve", dir.to_str().unwrap()]).envs(plane).env("SUPERCI_CACHE_LISTEN", "not an address")).contains("SUPERCI_CACHE_LISTEN"));
    assert!(!fails(command().args(["serve", dir.to_str().unwrap()]).envs(plane).env("SUPERCI_CACHE_LISTEN", "192.0.2.1")).is_empty());
    assert!(fails(command().args(["serve", dir.join("missing").to_str().unwrap()]).envs(plane).env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", "0")).contains("missing"));
    // In a network with no way out (a namespace of its own, as root): its private address cannot be found, said.
    // (Linux, as root or through sudo; a coverage tool's settings given on the command line, which sudo keeps.)
    let root = cfg!(target_os = "linux") && std::process::Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.starts_with(b"0\n"));
    if cfg!(target_os = "linux") && (root || std::process::Command::new("sudo").args(["-n", "true"]).status().is_ok_and(|s| s.success())) {
        let mut c = std::process::Command::new(if root { "unshare" } else { "sudo" });
        if !root { c.args(["-n", "unshare"]); }
        c.args(["-n", "env"]).args(std::env::vars().filter(|(k, _)| k.starts_with("SUPERCOV_")).map(|(k, v)| format!("{k}={v}")))
            .args(["SUPERCI_CACHE_PLANE=http://127.0.0.1:9/cache/t", env!("CARGO_BIN_EXE_superci-cache"), "serve", dir.to_str().unwrap()]);
        assert!(fails(&mut c).contains("unreachable"));
    }
    // On IPv6's loopback, and on the machine's private address (what it wrote beside the externals).
    for listen in [Some("::1"), None] {
        let dir = folder("listen");
        let mut c = command();
        c.args(["serve", dir.to_str().unwrap()]).envs(plane).env("SUPERCI_CACHE_PORT", "0");
        if let Some(l) = listen { c.env("SUPERCI_CACHE_LISTEN", l); }
        let _p = Ends(c.spawn().unwrap());
        let file = dir.join("superci-cache.address");
        for _ in 0..200 { if file.exists() { break } std::thread::sleep(Duration::from_millis(20)) }
        let at = std::fs::read_to_string(&file).unwrap();
        let host = at.trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(host.replace(['[', ']'], "").rsplit_once(':').map(|(h, p)| (h.to_string(), p.parse::<u16>().unwrap())).unwrap()).unwrap();
        s.write_all(b"GET /v8/artifacts/status HTTP/1.1\r\nauthorization: Bearer superci\r\nconnection: close\r\n\r\n").unwrap();
        assert_eq!(raw_answer(s).0, 200, "{at}");
    }
}

/// `wrap` where there is nothing to wrap or it may not: no folder, no Node in it, a folder it may not write; and the
/// wrapped Node without the real one beside it.
#[test]
fn wrap_says_what_it_could_not_do_and_node_without_the_real_one_says_so() {
    let wrap = |dir: &std::path::Path| command().args(["wrap", dir.to_str().unwrap()]).output().unwrap();
    let dir = folder("wrap");
    assert!(!wrap(&dir.join("missing")).status.success());
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::create_dir_all(dir.join("node18/bin")).unwrap();
    assert!(String::from_utf8_lossy(&wrap(&dir).stderr).contains("no Node in"));
    let empty = folder("empty");
    assert!(String::from_utf8_lossy(&wrap(&empty).stderr).contains("no Node in"));
    // A Node in a folder it may not write: it cannot be moved aside; once moved, this program cannot be put there.
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("node20/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("node"), "#!/bin/sh\necho real\n").unwrap();
    std::fs::set_permissions(bin.join("node"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(!wrap(&dir).status.success());
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(bin.join("node"), bin.join("node.real")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(!wrap(&dir).status.success());
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Wrapped, then the real Node taken away: the wrapped one says so.
    assert!(wrap(&dir).status.success());
    std::fs::remove_file(bin.join("node.real")).unwrap();
    let out = std::process::Command::new(bin.join("node")).arg("-v").output().unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("node.real"));
    let _ = std::fs::remove_dir_all(&dir);
}
