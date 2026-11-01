//! `superci-cache serve` against the control plane itself (`superci_testkit`, its bucket the test kit's S3): GitHub's
//! cache client saving and restoring with a GitHub job's own token, what the control plane does not take going on to
//! GitHub's own cache, and Turbo's outputs kept for the machine's job (a workflow's own Turbo token's on to Vercel).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use superci_testkit::{serve, GitHub};

/// The program, ended when the test ends.
struct Ends(std::process::Child);
impl Drop for Ends { fn drop(&mut self) { let _ = self.0.kill(); } }

/// A request to the program: status, headers (lower case), body.
fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nhost: x\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&out[9..12]).parse().unwrap(), String::from_utf8_lossy(&out[..at]).to_lowercase(), out[at + 4..].to_vec())
}

/// GitHub's own results service, standing in: what was sent on to it.
fn githubs_results() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Arc<Mutex<Vec<String>>> = Default::default();
    let kept = seen.clone();
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let mut r = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if r.read_line(&mut line).is_err() { continue }
        let mut len = 0usize;
        loop { let mut h = String::new(); if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() { break } if let Some((k, v)) = h.split_once(':') { if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap_or(0) } } }
        let mut body = vec![0; len];
        let _ = r.read_exact(&mut body);
        kept.lock().unwrap().push(line.split_whitespace().nth(1).unwrap_or_default().to_string());
        let answer = br#"{"ok":true,"github":true}"#;
        let mut s = &stream;
        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", answer.len()).as_bytes()).and_then(|_| s.write_all(answer));
    });
    (url, seen)
}

#[test]
fn githubs_cache_client_and_turbo_save_and_restore_through_the_control_plane_and_what_it_does_not_take_goes_to_github() {
    // A control plane with its own AWS account (its bucket ready), GitHub connected, and a job of acme/app on a
    // machine whose link to the cache is this one.
    let served = serve(|c| {
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
        c.routing.order = vec![superci_core::plane::Pool::of("cloudflare")];
    });
    served.github.put_object(&format!("{}/{}", superci_core::cache::bucket(superci_testkit::PLANE, "us-east-1"), superci_core::cache::program_key("x86_64")), b"program");
    served.connect_github();
    let (id, queued) = served.github.queue("test", "superci", served.now_ms());
    assert_eq!(served.webhook(queued), 202);
    let runner = served.store.under(&format!("job:{id}"))[0].1["runner"].as_str().unwrap().to_string();
    let link = "machine-cache-link-000000000003";
    served.store.0.lock().unwrap().insert(format!("cachem:{link}"), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": runner }).to_string());
    let (github, forwarded) = githubs_results();
    let dir = std::env::temp_dir().join(format!("superci-cache-plane-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let port = superci_testkit::free_port();
    let _ends = Ends(std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache")).args(["serve", dir.to_str().unwrap()])
        .env("SUPERCI_CACHE_PLANE", format!("{}/cache/{link}", served.url)).env("SUPERCI_STAND_IN", &served.aws.url).env("SUPERCI_CACHE_GITHUB", &github).env("SUPERCI_CACHE_VERCEL", &github)
        .env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string()).stderr(std::process::Stdio::null()).spawn().unwrap());
    for _ in 0..100 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(std::time::Duration::from_millis(20)) }

    // GitHub's tokens: a push to main, and a job that only reads.
    let token = |ac: serde_json::Value| GitHub::job_token(&serde_json::json!({ "repository_id": "197814629", "ac": ac.to_string(), "exp": 4_102_444_800u64 }));
    let (main, reads) = (token(serde_json::json!([{ "Scope": "refs/heads/main", "Permission": 3 }])), token(serde_json::json!([{ "Scope": "refs/heads/main", "Permission": 1 }])));
    let twirp = |t: &str, op: &str, body: serde_json::Value| -> (u16, serde_json::Value) {
        let (status, _, b) = call(port, "POST", &format!("/twirp/github.actions.results.api.v1.CacheService/{op}"), &[("authorization", &format!("Bearer {t}")), ("content-type", "application/json")], body.to_string().as_bytes());
        (status, serde_json::from_slice(&b).unwrap_or_default())
    };
    let path = |url: &str| url.split_once(&format!(":{port}")).unwrap().1.to_string();
    let version = "d".repeat(64);
    // Saved: begun, uploaded, finished, in the control plane's bucket.
    let (_, made) = twirp(&main, "CreateCacheEntry", serde_json::json!({ "key": "deps-linux-1", "version": version }));
    assert_eq!(call(port, "PUT", &path(made["signed_upload_url"].as_str().unwrap()), &[], b"node_modules, packed").0, 201);
    assert_eq!(twirp(&main, "FinalizeCacheEntryUpload", serde_json::json!({ "key": "deps-linux-1", "version": version, "size_bytes": "20" })).1["ok"], true);
    assert!(served.github.objects().iter().any(|(k, n)| k.contains("/c/") || *n == 20), "{:?}", served.github.objects());
    // Restored by its key's beginning, whole and in part.
    let (_, found) = twirp(&main, "GetCacheEntryDownloadURL", serde_json::json!({ "key": "deps-linux-2", "restore_keys": ["deps-linux-"], "version": version }));
    assert_eq!(found["matched_key"], "deps-linux-1", "{found}");
    let download = path(found["signed_download_url"].as_str().unwrap());
    assert_eq!(call(port, "GET", &download, &[], b"").2, b"node_modules, packed");
    let (status, head, part) = call(port, "GET", &download, &[("range", "bytes=14-19")], b"");
    assert!(status == 206 && part == b"packed" && head.contains("content-range: bytes 14-19/20"), "{status} {head}");
    // There already: as GitHub's own service says it.
    assert_eq!(twirp(&main, "CreateCacheEntry", serde_json::json!({ "key": "deps-linux-1", "version": version })).0, 409);
    // A job that only reads may not save here: said, nothing sent on.
    let (_, refused) = twirp(&reads, "CreateCacheEntry", serde_json::json!({ "key": "deps-new", "version": version }));
    assert!(refused["ok"] == false && refused["message"].as_str().unwrap_or_default().contains("may only read"), "{refused}");
    // What the control plane does not take (a key with a comma) is saved in GitHub's own cache, and finished there;
    // a key not found here is asked of GitHub.
    assert_eq!(twirp(&main, "CreateCacheEntry", serde_json::json!({ "key": "a,b", "version": version })).1["github"], true);
    assert_eq!(twirp(&main, "FinalizeCacheEntryUpload", serde_json::json!({ "key": "a,b", "version": version, "size_bytes": "3" })).1["github"], true);
    assert_eq!(twirp(&main, "GetCacheEntryDownloadURL", serde_json::json!({ "key": "nowhere", "version": version })).1["github"], true);
    let sent = forwarded.lock().unwrap().clone();
    assert!(sent.iter().filter(|p| p.ends_with("CreateCacheEntry")).count() == 1 && sent.iter().any(|p| p.ends_with("FinalizeCacheEntryUpload")) && sent.iter().any(|p| p.ends_with("GetCacheEntryDownloadURL")), "{sent:?}");

    // Turbo, with no token of GitHub's: kept for the machine's job (its repository's, its branch's).
    let tool = [("authorization", "Bearer superci")];
    let hash = "e".repeat(64);
    assert_eq!(call(port, "GET", &format!("/v8/artifacts/{hash}"), &tool, b"").0, 404);
    assert_eq!(call(port, "PUT", &format!("/v8/artifacts/{hash}"), &[tool[0], ("x-artifact-duration", "4200"), ("x-artifact-tag", "sig")], b"turbo outputs").0, 202);
    let (status, head, got) = call(port, "GET", &format!("/v8/artifacts/{hash}"), &tool, b"");
    assert!(status == 200 && got == b"turbo outputs" && head.contains("x-artifact-duration: 4200") && head.contains("x-artifact-tag: sig"), "{status} {head}");
    assert_eq!(call(port, "HEAD", &format!("/v8/artifacts/{hash}"), &tool, b"").0, 200);
    // (A workflow's own Turbo token, Vercel's: sent on to Vercel, standing in here.)
    assert_eq!(call(port, "GET", &format!("/v8/artifacts/{hash}"), &[("authorization", "Bearer a-vercel-token")], b"").0, 200);
    assert!(forwarded.lock().unwrap().iter().any(|p| p == &format!("/v8/artifacts/{hash}")), "{:?}", forwarded.lock().unwrap());
    let (_, _, kept) = call(port, "POST", "/v8/artifacts", &tool, format!(r#"{{"hashes":["{hash}","nope"]}}"#).as_bytes());
    let kept: serde_json::Value = serde_json::from_slice(&kept).unwrap();
    assert_eq!((kept[&hash]["size"].as_u64(), kept[&hash]["taskDurationMs"].as_u64(), kept["nope"].is_null()), (Some(13), Some(4200), true), "{kept}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_from_a_computers_artifacts_and_nx_and_bazels_outputs_go_through_the_control_plane() {
    let served = serve(|c| {
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
        c.routing.order = vec![superci_core::plane::Pool::of("cloudflare")];
    });
    // A run from a computer (as `superci runs create` asks for one), its job on a machine whose link is this one.
    let workflow = "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make\n";
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let made: serde_json::Value = serde_json::from_str(&agent.post(&format!("{}/runs", served.url)).header("authorization", &format!("Bearer {}", served.run_key))
        .send(serde_json::json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "local/app", "ref": "refs/heads/main" }).to_string()).unwrap().body_mut().read_to_string().unwrap()).unwrap();
    let run = made["run_id"].as_str().unwrap().to_string();
    let record = served.store.under(&format!("run:{run}"))[0].1.clone();
    let job_token = record["jobs"][0]["token"].as_str().unwrap().to_string();
    let token = served.store.under(&format!("rjob:{job_token}"))[0].1["message"]["resources"]["endpoints"][0]["authorization"]["parameters"]["AccessToken"].as_str().unwrap().to_string();
    let runner = served.store.under("job:").into_iter().find(|(_, j)| j["run_id"].to_string() == run).unwrap().1["runner"].as_str().unwrap().to_string();
    let link = "machine-cache-link-000000000004";
    served.store.0.lock().unwrap().insert(format!("cachem:{link}"), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": runner }).to_string());
    let dir = std::env::temp_dir().join(format!("superci-cache-plane-local-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let port = superci_testkit::free_port();
    let _ends = Ends(std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache")).args(["serve", dir.to_str().unwrap()])
        .env("SUPERCI_CACHE_PLANE", format!("{}/cache/{link}", served.url)).env("SUPERCI_STAND_IN", &served.aws.url)
        .env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string()).stderr(std::process::Stdio::null()).spawn().unwrap());
    for _ in 0..100 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(std::time::Duration::from_millis(20)) }
    let path = |url: &str| url.split_once(&format!(":{port}")).unwrap().1.to_string();
    let artifacts = |op: &str, body: serde_json::Value| -> (u16, serde_json::Value) {
        let (status, _, b) = call(port, "POST", &format!("/twirp/github.actions.results.api.v1.ArtifactService/{op}"), &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")], body.to_string().as_bytes());
        (status, serde_json::from_slice(&b).unwrap_or_default())
    };
    // `upload-artifact`: made, its blocks uploaded (as Azure's client puts them), finished; listed, fetched, deleted.
    let (_, made) = artifacts("CreateArtifact", serde_json::json!({ "name": { "value": "report" }, "version": 4 }));
    let upload = path(made["signed_upload_url"].as_str().unwrap_or_else(|| panic!("{made}")));
    let join = if upload.contains('?') { '&' } else { '?' };
    for (id, part) in [("YmxvY2sx", &b"first-"[..]), ("YmxvY2sy", &b"second"[..])] { assert_eq!(call(port, "PUT", &format!("{upload}{join}comp=block&blockid={id}"), &[], part).0, 201) }
    let list = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>YmxvY2sx</Latest><Latest>YmxvY2sy</Latest></BlockList>";
    assert_eq!(call(port, "PUT", &format!("{upload}{join}comp=blocklist"), &[], list).0, 201);
    let (_, done) = artifacts("FinalizeArtifact", serde_json::json!({ "name": "report", "size": "12" }));
    assert!(done["ok"] == true && done["artifact_id"].as_str().is_some(), "{done}");
    assert_eq!(artifacts("CreateArtifact", serde_json::json!({ "name": "report" })).0, 409);
    assert_eq!(artifacts("FinalizeArtifact", serde_json::json!({ "name": "never" })).0, 400);
    let (_, listed) = artifacts("ListArtifacts", serde_json::json!({ "name_filter": "report" }));
    assert_eq!((listed["artifacts"][0]["name"].as_str(), listed["artifacts"][0]["size"].as_str()), (Some("report"), Some("12")), "{listed}");
    assert_eq!(artifacts("ListArtifacts", serde_json::json!({ "id_filter": "1" })).1["artifacts"], serde_json::json!([]));
    let (_, signed) = artifacts("GetSignedArtifactURL", serde_json::json!({ "name": "report" }));
    let (_, head, body) = call(port, "GET", &path(signed["signed_url"].as_str().unwrap()), &[], b"");
    // (Said to be a zip, as GitHub's storage says it: `download-artifact` unpacks only that.)
    assert!(body == b"first-second" && head.contains("content-type: application/zip"), "{head}");
    assert_eq!(artifacts("GetSignedArtifactURL", serde_json::json!({ "name": "none" })).0, 404);
    assert_eq!(artifacts("DeleteArtifact", serde_json::json!({ "name": "report" })).1["ok"], true);
    assert_eq!(artifacts("DeleteArtifact", serde_json::json!({ "name": "report" })).0, 404);
    assert_eq!(artifacts("MigrateArtifact", serde_json::json!({})).0, 501);
    // A cache call GitHub's client makes that SuperCI does not stand in for: said, nothing sent on (not GitHub's job).
    let (status, _, said) = call(port, "POST", "/twirp/github.actions.results.api.v1.CacheService/DeleteCacheEntry", &[("authorization", &format!("Bearer {token}"))], b"{}");
    assert!(status == 501 && String::from_utf8_lossy(&said).contains("unimplemented"), "{status}");
    // Nx and Bazel, for the machine's job (a run from a computer's own scope).
    assert_eq!(call(port, "PUT", "/v1/cache/nx1", &[("authorization", "Bearer superci")], b"nx tar").0, 200);
    assert_eq!(call(port, "PUT", "/v1/cache/nx1", &[("authorization", "Bearer superci")], b"nx tar").0, 409);
    assert_eq!(call(port, "GET", "/v1/cache/nx1", &[("authorization", "Bearer superci")], b"").2, b"nx tar");
    assert_eq!(call(port, "PUT", "/bazel/main/cas/0f1e", &[], b"content").0, 200);
    assert_eq!(call(port, "GET", "/bazel/main/cas/0f1e", &[], b"").2, b"content");
    assert_eq!(call(port, "GET", "/bazel/main/ac/0f1e", &[], b"").0, 404);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gitlabs_runner_cache_is_kept_for_the_job_its_runner_runs_and_read_by_its_branch() {
    let served = serve(|c| {
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
        c.routing.order = vec![superci_core::plane::Pool::of("cloudflare")];
    });
    served.connect_gitlab();
    // A GitLab job of acme/app, its runner made for it, running (as GitLab told).
    let (id, pending) = served.gitlab.pending("test", &["superci"], served.now_ms());
    assert!(served.gitlab_webhook(pending) < 300);
    let port = superci_testkit::free_port();
    let dir = std::env::temp_dir().join(format!("superci-cache-plane-gl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let start = |link: &str| Ends(std::process::Command::new(env!("CARGO_BIN_EXE_superci-cache")).args(["serve", dir.to_str().unwrap()])
        .env("SUPERCI_CACHE_PLANE", format!("{}/cache/{link}", served.url)).env("SUPERCI_STAND_IN", &served.aws.url)
        .env("SUPERCI_CACHE_LISTEN", "127.0.0.1").env("SUPERCI_CACHE_PORT", port.to_string()).stderr(std::process::Stdio::null()).spawn().unwrap());
    let runner = served.store.under("glrunner:").into_iter().next().map(|(k, _)| k.trim_start_matches("glrunner:").to_string()).expect("its runner");
    let link = "machine-cache-link-000000000005";
    served.store.0.lock().unwrap().insert(format!("cachem:{link}"), serde_json::json!({ "region": "us-east-1", "arch": "x86_64", "until": u64::MAX / 4, "runner": format!("gl:{runner}") }).to_string());
    let _ends = start(link);
    for _ in 0..100 { if TcpStream::connect(("127.0.0.1", port)).is_ok() { break } std::thread::sleep(std::time::Duration::from_millis(20)) }
    let key = "/superci/gl/project/380/deps-protected/cache.zip?X-Amz-Signature=x";
    // Before its job has begun on the runner: GitLab is asked which job it runs (none yet): nothing kept.
    assert_eq!(call(port, "PUT", key, &[], b"zip bytes").0, 500);
    // Its job running: kept for its branch, and read back (whole, and its head).
    assert!(served.gitlab_webhook(served.gitlab.running(id, served.now_ms())) < 300);
    assert_eq!(call(port, "GET", key, &[], b"").0, 404);
    assert_eq!(call(port, "PUT", key, &[], b"zip bytes").0, 200);
    assert_eq!(call(port, "GET", key, &[], b"").2, b"zip bytes");
    let (status, head, _) = call(port, "HEAD", key, &[], b"");
    assert!(status == 200 && head.contains("etag"), "{status} {head}");
    assert_eq!(call(port, "DELETE", key, &[], b"").0, 405);
    // The cache's list: the project by its path, as GitLab has it.
    let kept: Vec<String> = served.store.under("cachename:").into_iter().map(|(_, v)| v.to_string()).collect();
    assert!(kept.iter().any(|n| n.contains("acme/app")), "{kept:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
