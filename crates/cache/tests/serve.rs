//! `superci-cache serve` over HTTP, as GitHub's cache client and GitLab's runner call it, against a stand-in control
//! plane and bucket: a save (one piece), its restore, and a GitLab cache upload and download, each where it belongs.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A small HTTP server: each request (method, path, body) answered by `answer` (status, body).
fn server(answer: impl Fn(&str, &str, Vec<u8>) -> (u16, Vec<u8>) + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let answer = Arc::new(answer);
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let answer = answer.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 { return }
                let mut p = line.split_whitespace();
                let (method, path) = (p.next().unwrap_or_default().to_string(), p.next().unwrap_or_default().to_string());
                let mut len = 0usize;
                loop { let mut h = String::new(); r.read_line(&mut h).unwrap(); if h.trim().is_empty() { break } if let Some((k, v)) = h.split_once(':') { if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap() } } }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let (status, out) = answer(&method, &path, body);
                let mut s = &stream;
                let _ = s.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n", out.len()).as_bytes()).and_then(|_| s.write_all(&out));
            }
        });
    });
    port
}

fn call(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) { call_with(port, method, path, &[("authorization", "Bearer job-token")], body) }

/// The same with these headers, and the answer's headers too.
fn call_with(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) { let (s, _, b) = call_full(port, method, path, headers, body); (s, b) }

fn call_full(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nhost: x\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = String::from_utf8_lossy(&out[9..12]).parse().unwrap();
    (status, String::from_utf8_lossy(&out[..at]).to_lowercase(), out[at + 4..].to_vec())
}

/// `superci-cache serve` against the stand-in control plane and bucket: its port, the program, the bucket's objects,
/// and its folder.
/// The program, ended when the test ends (a failed one included: else it keeps the test's output open).
struct Ends(std::process::Child);
impl Drop for Ends { fn drop(&mut self) { let _ = self.0.kill(); } }

fn started() -> (u16, Ends, Arc<Mutex<HashMap<String, Vec<u8>>>>, std::path::PathBuf) {
    let objects: Arc<Mutex<HashMap<String, Vec<u8>>>> = Default::default();
    let kept = objects.clone();
    let bucket = server(move |method, path, body| {
        let mut o = kept.lock().unwrap();
        let name = path.split('?').next().unwrap().to_string();
        match method { "PUT" => { o.insert(name, body); (200, vec![]) } _ => match o.get(&name) { Some(b) => (200, b.clone()), None => (404, vec![]) } }
    });
    let entries: Arc<Mutex<HashMap<String, serde_json::Value>>> = Default::default();
    let plane = server(move |_, path, body| {
        let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let what = path.rsplit('/').next().unwrap().to_string();
        let k = format!("{}:{}", b["tool"].as_str().unwrap_or("cache"), b["key"].as_str().or(b["hash"].as_str()).unwrap_or_default());
        let mut e = entries.lock().unwrap();
        let link = |n: &str| format!("http://127.0.0.1:{bucket}/o/{}/{n}", k.replace(['/', ':'], "_"));
        let reply = match what.as_str() {
            "begin" => serde_json::json!({ "ok": true }),
            "tool-begin" => serde_json::json!({ "ok": true, "url": link("p00000") }),
            "sign" | "tool-sign" => serde_json::json!({ "ok": true, "urls": b["names"].as_array().unwrap().iter().map(|n| link(n.as_str().unwrap())).collect::<Vec<_>>() }),
            "finish" | "tool-finish" => {
                let pieces = b.get("pieces").cloned().unwrap_or(serde_json::json!([["p00000", b["size"]]]));
                e.insert(k.clone(), pieces);
                // (A tool's own note of it, as the control plane keeps it: Turbo's task time.)
                e.insert(format!("{k}:meta"), b["meta"].clone());
                serde_json::json!({ "ok": true })
            }
            "lookup" | "tool-get" => match e.get(&k) {
                Some(p) => serde_json::json!({ "hit": true, "key": b["key"], "size": p.as_array().unwrap().iter().map(|x| x[1].as_u64().unwrap()).sum::<u64>(),
                    "pieces": p.as_array().unwrap().iter().map(|x| serde_json::json!({ "url": link(x[0].as_str().unwrap()), "size": x[1] })).collect::<Vec<_>>(),
                    "meta": e.get(&format!("{k}:meta")).cloned().unwrap_or_default() }),
                None => serde_json::json!({ "hit": false }),
            },
            _ => serde_json::json!({}),
        };
        (200, reply.to_string().into_bytes())
    });
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("superci-cache-serve-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap();
    let port = superci_testkit::free_port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache")).args(["serve", dir.to_str().unwrap()])
        .env("SUPERCI_CACHE_PLANE", format!("http://127.0.0.1:{plane}/cache/t")).env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string()).spawn().unwrap();
    for _ in 0..100 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(std::time::Duration::from_millis(20)) }
    (port, Ends(child), objects, dir)
}

#[test]
fn a_save_its_restore_and_gitlabs_cache_each_go_where_they_belong() {
    let (port, _ends, objects, dir) = started();
    let twirp = |op: &str, body: serde_json::Value| -> serde_json::Value { serde_json::from_slice(&call(port, "POST", &format!("/twirp/github.actions.results.api.v1.CacheService/{op}"), body.to_string().as_bytes()).1).unwrap() };

    // A save as GitHub's client makes one (one PUT of the whole archive to its upload address), then its restore.
    let made = twirp("CreateCacheEntry", serde_json::json!({ "key": "deps", "version": "v1" }));
    let upload = made["signed_upload_url"].as_str().unwrap().split_once(&format!(":{port}")).unwrap().1.to_string();
    assert_eq!(call(port, "PUT", &upload, b"the archive").0, 201);
    assert_eq!(twirp("FinalizeCacheEntryUpload", serde_json::json!({ "key": "deps", "version": "v1", "size_bytes": "11" }))["ok"], true);
    let found = twirp("GetCacheEntryDownloadURL", serde_json::json!({ "key": "deps", "version": "v1" }));
    let download = found["signed_download_url"].as_str().unwrap().split_once(&format!(":{port}")).unwrap().1.to_string();
    assert_eq!(call(port, "GET", &download, b""), (200, b"the archive".to_vec()));

    // GitLab's runner's cache, as S3 (its own key, under its cache path), kept and given back.
    let key = "/superci/gl/project/380/deps-protected/cache.zip?X-Amz-Signature=x";
    assert_eq!(call(port, "GET", key, b"").0, 404);
    assert_eq!(call(port, "PUT", key, b"zip bytes").0, 200);
    assert_eq!(call(port, "GET", key, b""), (200, b"zip bytes".to_vec()));
    assert!(objects.lock().unwrap().keys().any(|k| k.contains("gitlab_gl_project_380")), "{:?}", objects.lock().unwrap().keys().collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turbo_nx_and_bazel_keep_their_outputs_and_a_save_in_blocks_comes_back_whole_or_in_part() {
    let (port, _ends, _, dir) = started();
    let tool = [("authorization", "Bearer superci")];
    // Turbo: a task's outputs under its hash, with how long the task took; asked which are kept, and fetched.
    assert_eq!(call_with(port, "GET", "/v8/artifacts/status", &tool, b""), (200, br#"{"status":"enabled"}"#.to_vec()));
    assert_eq!(call_with(port, "GET", "/v8/artifacts/abc123", &tool, b"").0, 404);
    assert_eq!(call_with(port, "PUT", "/v8/artifacts/abc123", &[tool[0], ("x-artifact-duration", "4200")], b"turbo outputs").0, 202);
    let (status, head, got) = call_full(port, "GET", "/v8/artifacts/abc123", &tool, b"");
    assert_eq!((status, got.as_slice()), (200, &b"turbo outputs"[..]));
    assert!(head.contains("x-artifact-duration: 4200"), "{head}");
    let kept: serde_json::Value = serde_json::from_slice(&call_with(port, "POST", "/v8/artifacts", &tool, br#"{"hashes":["abc123","nope"]}"#).1).unwrap();
    assert_eq!((kept["abc123"]["size"].as_u64(), kept["abc123"]["taskDurationMs"].as_u64(), kept["nope"].is_null()), (Some(13), Some(4200), true), "{kept}");
    // Nx: kept once; a second save of the same hash is refused, as Nx expects.
    assert_eq!(call(port, "PUT", "/v1/cache/nx1", b"nx tar").0, 200);
    assert_eq!(call(port, "GET", "/v1/cache/nx1", b""), (200, b"nx tar".to_vec()));
    assert_eq!(call(port, "GET", "/v1/cache/nx2", b"").0, 404);
    // Bazel: an action's result and content by its hash (after an instance name).
    assert_eq!(call(port, "PUT", "/bazel/main/cas/0f1e", b"content").0, 200);
    assert_eq!(call(port, "GET", "/bazel/main/cas/0f1e", b""), (200, b"content".to_vec()));
    assert_eq!(call(port, "GET", "/bazel/main/ac/0f1e", b"").0, 404);
    assert_eq!(call(port, "GET", "/bazel/main/other/0f1e", b"").0, 404);

    // GitHub's client saving a large entry: blocks, then their order; restored whole, and in part (as its client
    // downloads in ranges).
    let twirp = |op: &str, body: serde_json::Value| -> serde_json::Value { serde_json::from_slice(&call(port, "POST", &format!("/twirp/github.actions.results.api.v1.CacheService/{op}"), body.to_string().as_bytes()).1).unwrap() };
    let made = twirp("CreateCacheEntry", serde_json::json!({ "key": "big", "version": "v1" }));
    let upload = made["signed_upload_url"].as_str().unwrap().split_once(&format!(":{port}")).unwrap().1.to_string();
    let join = if upload.contains('?') { '&' } else { '?' };
    for (id, part) in [("YmxvY2sx", &b"first-"[..]), ("YmxvY2sy", &b"second"[..])] { assert_eq!(call(port, "PUT", &format!("{upload}{join}comp=block&blockid={id}"), part).0, 201) }
    let list = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>YmxvY2sx</Latest><Latest>YmxvY2sy</Latest></BlockList>";
    assert_eq!(call(port, "PUT", &format!("{upload}{join}comp=blocklist"), list).0, 201);
    assert_eq!(twirp("FinalizeCacheEntryUpload", serde_json::json!({ "key": "big", "version": "v1", "size_bytes": "12" }))["ok"], true);
    let found = twirp("GetCacheEntryDownloadURL", serde_json::json!({ "key": "big", "version": "v1" }));
    let download = found["signed_download_url"].as_str().unwrap().split_once(&format!(":{port}")).unwrap().1.to_string();
    assert_eq!(call(port, "GET", &download, b""), (200, b"first-second".to_vec()));
    let (status, head, got) = call_full(port, "GET", &download, &[("x-ms-range", "bytes=6-11")], b"");
    assert_eq!((status, got.as_slice()), (206, &b"second"[..]));
    assert!(head.contains("content-range: bytes 6-11/12"), "{head}");
    assert_eq!(call_with(port, "GET", &download, &[("range", "bytes=20-30")], b"").0, 416);
    // A key nobody saved is not found.
    assert_eq!(twirp("GetCacheEntryDownloadURL", serde_json::json!({ "key": "none", "version": "v1" }))["ok"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the control plane refuses (or cannot be reached for) is said to GitHub's artifact client as twirp says it:
/// an artifact there already, none of that name, anything else; and none of it as kept.
#[test]
fn artifacts_the_control_plane_refuses_are_said_as_twirp_errors() {
    let (status_of, why) = (|what: &str| match what { "artifact-begin" => 409, "artifact-get" | "artifact-delete" => 404, _ => 400 }, "the store refused");
    let plane = server(move |_, path, _| { let what = path.rsplit('/').next().unwrap(); (status_of(what), serde_json::json!({ "why": why }).to_string().into_bytes()) });
    let gone = superci_testkit::free_port();
    for (plane_port, reached) in [(plane, true), (gone, false)] {
        let dir = std::env::temp_dir().join(format!("superci-cache-refused-{}-{plane_port}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let port = superci_testkit::free_port();
        let _ends = Ends(std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache")).args(["serve", dir.to_str().unwrap()])
            .env("SUPERCI_CACHE_PLANE", format!("http://127.0.0.1:{plane_port}/cache/t")).env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string()).spawn().unwrap());
        for _ in 0..100 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(std::time::Duration::from_millis(20)) }
        let twirp = |op: &str| { let (s, b) = call(port, "POST", &format!("/twirp/github.actions.results.api.v1.ArtifactService/{op}"), br#"{"name":"coverage"}"#); (s, serde_json::from_slice::<serde_json::Value>(&b).unwrap_or_default()) };
        let expect = |op: &str, status: u16, code: &str| {
            let (s, b) = twirp(op);
            assert!(s == status && b["code"] == code, "{op} ({}): {s} {b}", if reached { "refused" } else { "not reached" });
        };
        if reached {
            expect("CreateArtifact", 409, "already_exists");
            expect("ListArtifacts", 400, "internal");
            expect("GetSignedArtifactURL", 404, "not_found");
            expect("DeleteArtifact", 404, "not_found");
        } else {
            expect("CreateArtifact", 503, "failed_precondition");
            expect("ListArtifacts", 503, "internal");
            expect("GetSignedArtifactURL", 503, "internal");
            expect("DeleteArtifact", 503, "internal");
        }
        // (Finished before anything was uploaded: said as that.)
        expect("FinalizeArtifact", 400, "failed_precondition");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
