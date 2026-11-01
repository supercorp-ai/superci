//! superci-layers as a machine runs it, from its command line: what it is told amiss, its own service told it is
//! ready (systemd's way), an index read again that cannot be; and `keep` after the job against a control plane that
//! answers each of its requests in every way it can (refusing, not there, a layer it has, one it will not take, an
//! upload refused), containerd's part played by the program's own content store, as the service tests do.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use superci_layers::containerd::services::content::v1 as capi;

fn digest(b: &[u8]) -> String { format!("sha256:{}", Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()) }

fn program() -> Command { Command::new(env!("CARGO_BIN_EXE_superci-layers")) }

fn folder(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sl-cmd-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Service(Child, std::path::PathBuf);
impl Drop for Service {
    fn drop(&mut self) { unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM); } let _ = self.0.wait(); let _ = std::fs::remove_dir_all(&self.1); }
}
impl Service {
    fn log(&self) -> String { std::fs::read_to_string(self.1.join("log")).unwrap_or_default() }
    fn socket(&self) -> std::path::PathBuf { self.1.join("s.sock") }
}

/// The service on `dir`'s folders, with these arguments and environment besides.
fn serve(dir: &std::path::Path, args: &[String], env: &[(&str, &str)]) -> Service {
    let mut c = program();
    c.args([format!("--root={}", dir.join("root").display()), format!("--content={}", dir.join("content").display()), format!("--socket={}", dir.join("s.sock").display()),
        format!("--index={}", dir.join("index.json").display()), format!("--containerd={}", dir.join("none.sock").display())]).args(args)
        .stderr(std::fs::File::create(dir.join("log")).unwrap());
    for (k, v) in env { c.env(k, v); }
    let s = Service(c.spawn().unwrap(), dir.to_path_buf());
    let until = Instant::now() + Duration::from_secs(20);
    while !s.socket().exists() { assert!(Instant::now() < until, "{}", s.log()); std::thread::sleep(Duration::from_millis(20)) }
    s
}

#[test]
fn what_the_program_is_told_amiss_is_said_and_its_service_says_it_is_ready() {
    let said = |args: &[&str]| { let o = program().args(args).output().unwrap(); (o.status.code(), String::from_utf8_lossy(&o.stderr).to_string()) };
    assert_eq!(said(&["--help"]).0, Some(2));
    assert!(said(&["--help"]).1.contains("containerd's snapshotter and content store"));
    for args in [&["--bogus"][..], &["--parallel=x"], &["--layers=0"], &["keep", "--bogus"], &["keep"], &["setup", "--bogus"], &["setup", "--disk=none"], &["setup"], &["setup", "--root=/nowhere"]] {
        let (code, words) = said(args);
        assert_eq!(code, Some(2), "{args:?}: {words}");
    }
    // With nothing said, its own folders (where a machine has them): as another user than root, it cannot make them.
    if unsafe { libc::geteuid() } != 0 { assert_eq!(program().output().unwrap().status.code(), Some(1)); }
    // A socket where none can be: said, ended.
    let dir = folder("bad");
    let o = program().args([format!("--root={}", dir.join("root").display()), format!("--content={}", dir.join("content").display()), "--socket=/".to_string()]).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stderr));
    // In a user namespace (as containerd's overlay, it runs as the machine's own root only): said, ended. (Where a
    // user may make one, or root through sudo where only root may; a coverage tool's settings on the command line.)
    // (Through sudo, the users mapped as they are, up to 65536: the files of this user, whose folders root in the
    // namespace may then read, as outside.)
    let tries = [vec!["unshare", "-Ur"], vec!["sudo", "-n", "unshare", "-U", "--map-users=0:0:65536", "--map-groups=0:0:65536"]];
    let unshare = tries.into_iter().find(|u| Command::new(u[0]).args(&u[1..]).arg("true").status().is_ok_and(|s| s.success()));
    if let Some(unshare) = unshare.filter(|_| cfg!(target_os = "linux")) {
        let dir = folder("userns");
        let o = Command::new(unshare[0]).args(&unshare[1..]).arg("env").args(std::env::vars().filter(|(k, _)| k.starts_with("SUPERCOV_")).map(|(k, v)| format!("{k}={v}"))).arg(env!("CARGO_BIN_EXE_superci-layers")).args([format!("--root={}", dir.join("root").display()), format!("--content={}", dir.join("content").display()), format!("--socket={}", dir.join("s.sock").display())]).output().unwrap();
        assert!(o.status.code() == Some(1) && String::from_utf8_lossy(&o.stderr).contains("not supported in a user namespace"), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // Its own files folder, layers at once, removing at cleanup: up, and (on Linux) systemd told it is ready, at a path
    // and at an abstract address.
    let dir = folder("ready");
    let notify = std::os::unix::net::UnixDatagram::bind(dir.join("notify")).unwrap();
    notify.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let s = serve(&dir, &[format!("--files={}", dir.join("files").display()), "--layers=2".into(), "--parallel=2".into()], &[("NOTIFY_SOCKET", dir.join("notify").to_str().unwrap())]);
    let mut got = [0u8; 16];
    if cfg!(target_os = "linux") {
        let n = notify.recv(&mut got).unwrap();
        assert_eq!(&got[..n], b"READY=1");
    }
    // The index written amiss, then SIGHUP: said, the one it had kept.
    std::fs::write(dir.join("index.json"), "not json").unwrap();
    unsafe { libc::kill(s.0.id() as i32, libc::SIGHUP); }
    let until = Instant::now() + Duration::from_secs(10);
    while !s.log().contains("the index was not read again") { assert!(Instant::now() < until, "{}", s.log()); std::thread::sleep(Duration::from_millis(50)) }
    drop(s);
    // systemd's socket at a path too long for one: not told, up all the same; stopped as a terminal stops it.
    let dir = folder("longnotify");
    let s = serve(&dir, &[], &[("NOTIFY_SOCKET", &format!("/{}", "n".repeat(200)))]);
    unsafe { libc::kill(s.0.id() as i32, libc::SIGINT); }
    let mut s = s;
    let until = Instant::now() + Duration::from_secs(10);
    while !matches!(s.0.try_wait(), Ok(Some(_))) { assert!(Instant::now() < until, "SIGINT stops it"); std::thread::sleep(Duration::from_millis(50)) }
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("superci-layers-test-{}", std::process::id());
        let abs = std::os::unix::net::UnixDatagram::bind_addr(&std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
        abs.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let dir = folder("abstract");
        let _s = serve(&dir, &[], &[("NOTIFY_SOCKET", &format!("@{name}"))]);
        let n = abs.recv(&mut got).unwrap();
        assert_eq!(&got[..n], b"READY=1");
    }
}

/// How the control plane answers each of keep's requests: (status, body), or the connection closed.
#[derive(Clone)]
enum Answer { Json(u16, serde_json::Value), Drop }

/// The control plane as keep asks it: an offer answered as `propose` says (each layer: upload, have, refuse, or an
/// upload with no link), uploads (as `put` says), their word, and what was kept (`kept` says how that is answered).
struct Plane { propose: Option<Answer>, put: u16, kept: Answer, asked: Vec<String>, reports: Vec<serde_json::Value>, on_propose: Option<Box<dyn Fn() + Send>> }

fn plane(p: Arc<Mutex<Plane>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://{}", listener.local_addr().unwrap());
    let base = at.clone();
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let (p, base) = (p.clone(), base.clone());
        std::thread::spawn(move || {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() { return }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
            let mut len = 0;
            loop { let mut h = String::new(); if r.read_line(&mut h).is_err() || h.trim().is_empty() { break } if let Some((k, v)) = h.split_once(':') { if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().unwrap_or(0) } } }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let mut out = stream;
            let mut send = |status: u16, b: &[u8]| { let _ = out.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", b.len()).as_bytes()); let _ = out.write_all(b); };
            let mut pl = p.lock().unwrap();
            pl.asked.push(format!("{method} {path}"));
            match (method.as_str(), path.as_str()) {
                ("POST", "/layers-propose") => {
                    if let Some(f) = &pl.on_propose { f() }
                    match pl.propose.clone() {
                        Some(Answer::Drop) => {}
                        Some(Answer::Json(s, b)) => send(s, b.to_string().as_bytes()),
                        None => {
                            // (In turn: upload, have, refuse, an upload with no link and headers that are not text.)
                            let answers: serde_json::Map<String, serde_json::Value> = json["layers"].as_array().into_iter().flatten().enumerate().filter_map(|(i, l)| Some((l["digest"].as_str()?.to_string(), match i % 4 {
                                0 => serde_json::json!({ "do": "upload", "url": format!("{base}/up/{i}"), "id": i, "headers": { "x-a": "1", "x-n": 2 } }),
                                1 => serde_json::json!({ "do": "have" }),
                                2 => serde_json::json!({ "do": "no" }),
                                _ => serde_json::json!({ "do": "upload", "id": i }),
                            }))).collect();
                            send(200, serde_json::json!({ "layers": answers, "uploaded": format!("{base}/uploaded") }).to_string().as_bytes())
                        }
                    }
                }
                ("PUT", _) => { let s = pl.put; send(s, b"") }
                ("POST", "/uploaded") => send(200, br#"{"do": "converting"}"#),
                ("POST", "/layers-kept") => { pl.reports.push(json); match pl.kept.clone() { Answer::Drop => {}, Answer::Json(s, b) => send(s, b.to_string().as_bytes()) } }
                _ => send(404, b""),
            }
        });
    });
    at
}

/// Blobs pulled as Docker pulls them (written to the content store, as a pull writes them, with where they came
/// from), named by an image's manifest as containerd's records name its layers.
async fn pulled(socket: &std::path::Path, blobs: &[Vec<u8>], extra: &[(&str, &str)]) {
    let mut c = capi::content_client::ContentClient::new(superci_layers::client::channel(socket).await.unwrap());
    let write = |data: Vec<u8>, labels: HashMap<String, String>| { let mut c = c.clone(); async move {
        let d = digest(&data);
        let m = capi::WriteContentRequest { action: capi::WriteAction::Commit as i32, r#ref: d.clone(), total: data.len() as i64, expected: d, offset: 0, data, labels };
        let mut a = c.write(tokio_stream::iter(vec![m])).await.unwrap().into_inner();
        while a.message().await.unwrap().is_some() {}
    } };
    for (i, b) in blobs.iter().enumerate() {
        // (Where they came from: a registry and repositories, one with none, one with an empty name between.)
        let labels = match i % 3 { 0 => [("containerd.io/distribution.source.ghcr.io".to_string(), "acme/app,,acme/other".to_string())].into_iter().collect(), 1 => [("containerd.io/distribution.source.docker.io".to_string(), String::new())].into_iter().collect(), _ => HashMap::new() };
        write(b.clone(), labels).await;
    }
    // (A manifest of its own each time.)
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let manifest = format!("{{\"layers\": [], \"n\": {}}}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)).into_bytes();
    let mut labels: HashMap<String, String> = blobs.iter().enumerate().map(|(i, b)| (format!("containerd.io/gc.ref.content.l.{i}"), digest(b))).collect();
    labels.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    write(manifest, labels).await;
    let _ = c.info(capi::InfoRequest { digest: String::new() }).await;
}

fn keep(at: &str, dir: &std::path::Path, extra: &[String]) -> std::process::Output {
    program().arg("keep").args([format!("--link={at}"), format!("--socket={}", dir.join("s.sock").display()), format!("--content={}", dir.join("content").display()),
        format!("--index={}", dir.join("index.json").display()), format!("--state={}", dir.join("state").display()), format!("--boot={}", dir.join("boot").display())]).args(extra).output().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_says_what_was_kept_whatever_the_control_plane_answers() {
    let dir = folder("keep");
    // The store has one layer (switched in: from the store, not offered); the machine's image had one (not offered).
    let stored = b"a layer the store has".to_vec();
    let booted = b"a layer of the machine's own image".to_vec();
    std::fs::write(dir.join("index.json"), serde_json::json!({ "layers": { digest(&stored): { "size": stored.len(), "erofs": { "size": 1 }, "blob": {} } } }).to_string()).unwrap();
    std::fs::write(dir.join("boot"), format!("{}\n", &digest(&booted)[7..])).unwrap();
    std::fs::write(dir.join("state"), "on: Docker answered on it").unwrap();
    let s = serve(&dir, &[], &[]);
    let blobs: Vec<Vec<u8>> = (0..5).map(|i| format!("a pulled layer {i}").repeat(20).into_bytes()).collect();
    let mut all = blobs.clone();
    all.extend([stored.clone(), booted.clone()]);
    pulled(&s.socket(), &all, &[]).await;
    let p = Arc::new(Mutex::new(Plane { propose: None, put: 200, kept: Answer::Json(200, serde_json::json!({})), asked: vec![], reports: vec![], on_propose: None }));
    let at = plane(p.clone());
    // As the control plane answers each layer: uploaded, had, refused, a link missing; the rest said.
    let o = keep(&at, &dir, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let last = p.lock().unwrap().reports.last().cloned().unwrap();
    assert_eq!((last["served"].as_u64(), last["pulled"].as_u64()), (Some(1), Some(5)), "{last}");
    assert!(last["uploaded"].as_u64().unwrap() >= 1 && last["had"].as_u64().unwrap() >= 1 && last["refused"].as_u64().unwrap() >= 1 && last["failed"].as_u64().unwrap() >= 1, "{last}");
    // A layer kept by another digest (no file of it here, as sha256: not offered), and an answer naming as a layer
    // what is a path, not a digest: never read.
    let big = b"a layer kept by its sha512".to_vec();
    let d512 = format!("sha512:{}", sha2::Sha512::digest(&big).iter().map(|x| format!("{x:02x}")).collect::<String>());
    {
        let mut c = capi::content_client::ContentClient::new(superci_layers::client::channel(&s.socket()).await.unwrap());
        let m = capi::WriteContentRequest { action: capi::WriteAction::Commit as i32, r#ref: "big".into(), total: big.len() as i64, expected: d512.clone(), offset: 0, data: big.clone(), labels: HashMap::new() };
        let mut a = c.write(tokio_stream::iter(vec![m])).await.unwrap().into_inner();
        while a.message().await.unwrap().is_some() {}
    }
    pulled(&s.socket(), &[], &[("containerd.io/gc.ref.content.l.7", &d512)]).await;
    let traversal = format!("sha256:{}", "../".repeat(21) + "x");
    p.lock().unwrap().propose = Some(Answer::Json(200, serde_json::json!({ "layers": { &traversal: { "do": "upload", "url": format!("{at}/up/x"), "id": 1 }, format!("sha256:{}", "ab"): { "do": "upload", "url": format!("{at}/up/y"), "id": 2 } }, "uploaded": format!("{at}/uploaded") })));
    assert!(keep(&at, &dir, &[]).status.success());
    let last = p.lock().unwrap().reports.last().cloned().unwrap();
    assert!(last["missing"].as_u64().unwrap() >= 1 && last["note"] == "not a digest", "{last}");
    p.lock().unwrap().propose = None;
    // The uploads refused by the bucket; out of time before any; the blob gone before its upload.
    for (put, most, gone) in [(500, None, false), (200, Some("0"), false), (200, None, true)] {
        p.lock().unwrap().put = put;
        if gone {
            let content = dir.join("content/blobs/sha256");
            p.lock().unwrap().on_propose = Some(Box::new(move || { for e in std::fs::read_dir(&content).unwrap().flatten() { let _ = std::fs::remove_file(e.path()); } }));
        }
        let extra: Vec<String> = most.map(|m| vec![format!("--most={m}")]).unwrap_or_default();
        assert!(keep(&at, &dir, &extra).status.success());
        let last = p.lock().unwrap().reports.last().cloned().unwrap();
        assert!(last["failed"].as_u64().unwrap() >= 1 && !last["note"].as_str().unwrap_or_default().is_empty(), "{put} {most:?} {gone}: {last}");
    }
    // The offer refused, or never answered; what was kept not taken (said in its log, never failing the machine's end).
    let s2 = serve(&folder("keep2"), &[], &[]);
    pulled(&s2.socket(), &blobs, &[]).await;
    for propose in [Answer::Json(500, serde_json::json!({ "error": "busy" })), Answer::Drop] {
        let mut pl = p.lock().unwrap();
        (pl.propose, pl.put, pl.on_propose) = (Some(propose), 200, None);
        drop(pl);
        assert!(keep(&at, &s2.1, &[]).status.success());
        assert!(p.lock().unwrap().reports.last().unwrap()["note"].as_str().unwrap().starts_with("offering them"));
    }
    p.lock().unwrap().propose = None;
    for kept in [Answer::Json(500, serde_json::json!({})), Answer::Drop] {
        p.lock().unwrap().kept = kept;
        let o = keep(&at, &s2.1, &[]);
        assert!(o.status.success() && String::from_utf8_lossy(&o.stderr).contains("superci-layers keep: saying what"), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // A content store with nothing pulled (switched in): nothing offered, said once.
    let s3 = serve(&folder("keep3"), &[], &[]);
    std::fs::write(s3.1.join("state"), "on: Docker answered on it").unwrap();
    p.lock().unwrap().kept = Answer::Json(200, serde_json::json!({}));
    let before = p.lock().unwrap().reports.len();
    assert!(keep(&at, &s3.1, &[]).status.success());
    assert_eq!(p.lock().unwrap().reports.len(), before + 1);
    // Not switched in, a layer the store has pulled as Docker pulls (had, not offered); the offer answered with no
    // layers, or with what is not JSON; an upload with no headers to a link out of reach.
    let dir4 = folder("keep4");
    std::fs::write(dir4.join("index.json"), serde_json::json!({ "layers": { digest(&blobs[0]): { "size": blobs[0].len(), "erofs": { "size": 1 }, "blob": {} } } }).to_string()).unwrap();
    std::fs::write(dir4.join("state"), "off: the kernel does not mount an EROFS file").unwrap();
    let s4 = serve(&dir4, &[], &[]);
    pulled(&s4.socket(), &blobs, &[]).await;
    let gone = 1u16;
    let answers: serde_json::Map<String, serde_json::Value> = blobs[1..].iter().map(|b| (digest(b), serde_json::json!({ "do": "upload", "url": format!("http://127.0.0.1:{gone}/up"), "id": 1 }))).collect();
    for propose in [serde_json::json!({ "layers": {}, "uploaded": format!("{at}/uploaded") }), serde_json::json!({ "layers": answers, "uploaded": format!("{at}/uploaded") })] {
        p.lock().unwrap().propose = Some(Answer::Json(200, propose));
        assert!(keep(&at, &dir4, &[]).status.success());
        let last = p.lock().unwrap().reports.last().cloned().unwrap();
        assert_eq!(last["had"].as_u64(), Some(1), "{last}");
    }
    assert!(p.lock().unwrap().reports.last().unwrap()["failed"].as_u64().unwrap() >= 1);
    // containerd's socket answering nothing it can read: said, as containerd out of reach is.
    let mute = dir4.join("mute.sock");
    let l = std::os::unix::net::UnixListener::bind(&mute).unwrap();
    std::thread::spawn(move || for c in l.incoming().flatten() { drop(c) });
    let o = program().args(["keep".to_string(), format!("--link={at}"), format!("--socket={}", mute.display())]).output().unwrap();
    assert!(o.status.success());
    assert!(p.lock().unwrap().reports.last().unwrap()["error"].as_str().is_some_and(|e| e.starts_with("containerd: ")));
    // (An offer answered 200 with what is not JSON.)
    let raw_plane = { let l = TcpListener::bind("127.0.0.1:0").unwrap(); let at = format!("http://{}", l.local_addr().unwrap()); std::thread::spawn(move || for mut c in l.incoming().flatten() { let mut b = [0u8; 4096]; let _ = c.read(&mut b); let _ = c.write_all(b"HTTP/1.1 200 X\r\ncontent-length: 8\r\nconnection: close\r\n\r\nnot json"); }); at };
    assert!(keep(&raw_plane, &dir4, &[]).status.success());
}
