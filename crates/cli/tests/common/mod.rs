//! What the CLI's end-to-end tests share: a workspace on this computer that keeps a control plane served here
//! (`superci_testkit`), a run played to its end on it, the CLI and the dashboard as they run.
#![allow(dead_code)]
use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use superci_testkit::{Runner, Served};

/// A folder of its own for one test.
pub fn temp(what: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("superci-{what}-{}-{}-{nanos}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A workspace on this computer that keeps the served control plane and a key it takes, as one signed in before
/// would (AWS's sign-in since ended).
pub fn home(served: &Served) -> std::path::PathBuf {
    let home = temp("home");
    let ws = home.join("workspaces/default");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("sign-ins.json"), serde_json::json!({
        "key": { "name": "DEVICE_KEY_4102444800_TEST", "value": served.key, "planes": ["testplane0001"] },
        "plane": { "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": served.url, "plane_id": "testplane0001", "label": "superci" } },
    }).to_string()).unwrap();
    home
}

/// The same, signed in to the control plane's AWS account too (so it may change it), its AWS being the test kit's
/// stand-in.
pub fn signed_in(served: &Served) -> std::path::PathBuf {
    let home = home(served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["aws"] = serde_json::json!({ "region": "us-east-1", "account_id": "111111111111", "who": "arn:aws:sts::111111111111:assumed-role/Admin/someone",
        "creds": { "access_key_id": "ASIAEXAMPLE", "secret_access_key": "example", "session_token": "example", "expires_at_ms": 4102444800000u64 },
        "refresh_token": "", "dpop": { "private_b64": "", "kid": "" } });
    std::fs::write(&file, kept.to_string()).unwrap();
    std::fs::write(home.join("aws-at"), &served.aws.url).unwrap();
    home
}

/// The CLI or the dashboard, in `home`'s workspace (with its stand-in for AWS, if it has one).
pub fn command(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_superci"));
    c.env("SUPERCI_HOME", home).env_remove("SUPERCI_PLANE").env_remove("SUPERCI_KEY").env("SUPERCI_STAND_IN", std::fs::read_to_string(home.join("aws-at")).unwrap_or_default());
    c
}

/// The CLI run in `home`'s workspace.
pub fn superci(home: &std::path::Path, args: &[&str]) -> Output { command(home).args(args).output().unwrap() }

/// The CLI's answer with `--json`, and how it ended.
pub fn json(home: &std::path::Path, args: &[&str]) -> (i32, serde_json::Value) {
    let o = superci(home, &[args, &["--json"]].concat());
    let text = String::from_utf8_lossy(&o.stdout);
    (o.status.code().unwrap_or(-1), serde_json::from_str(&text).unwrap_or_else(|_| panic!("{args:?}: {text}{}", String::from_utf8_lossy(&o.stderr))))
}

/// A run of one job played to its end in a checkout under `dir`: its step, its 12 lines (in colour) and log, passed.
/// A run from a computer of `workflow`'s job, its runner played: each step (its name, and how it ended) begun and
/// ended in turn, a line each.
pub fn played_steps(served: &Served, dir: &std::path::Path, workflow: &str, steps: &[(&str, &str)]) {
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), workflow).unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(Command::new("git").current_dir(dir).args(args).status().unwrap().success()) }
    let cli = Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(dir).args(["runs", "create", "--workflow=.github/workflows/ci.yml", "--quiet"])
        .env("SUPERCI_HOME", dir.join(".cli")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &served.run_key).env("SUPERCI_STAND_IN", &served.aws.url).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let run = loop { if let Some((k, _)) = served.store.under("run:").into_iter().next() { break k.trim_start_matches("run:").to_string() } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let runner = loop { if let Some(r) = Runner::of_run(served, &run) { break r } std::thread::sleep(std::time::Duration::from_millis(50)) };
    runner.take();
    let ids: Vec<String> = (0..steps.len()).map(|i| format!("dddddddd-0000-0000-0000-{:012}", i + 1)).collect();
    let record = |done: usize| -> Vec<(&str, &str, u64, &str, Option<&str>, Option<u64>)> {
        steps.iter().enumerate().take(done + 1).map(|(i, (name, result))| if i < done { (ids[i].as_str(), *name, i as u64 + 1, "completed", Some(*result), Some(i as u64 + 1)) } else { (ids[i].as_str(), *name, i as u64 + 1, "inProgress", None, None) }).collect()
    };
    for (i, (name, _)) in steps.iter().enumerate() {
        runner.steps(&record(i));
        runner.lines(&ids[i], 1, &[format!("{name} going")]);
        runner.log(i as u64 + 1, &format!("2026-10-15T10:00:0{}.0000000Z {name} going", i % 10));
    }
    runner.steps(&record(steps.len()));
    runner.finish("succeeded");
    let _ = cli.wait_with_output();
}

pub fn played(served: &Served, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make test\n").unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(Command::new("git").current_dir(dir).args(args).status().unwrap().success()) }
    let cli = Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(dir).args(["runs", "create", "--workflow=.github/workflows/ci.yml", "--quiet"])
        .env("SUPERCI_HOME", dir.join(".cli")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &served.run_key).env("SUPERCI_STAND_IN", &served.aws.url).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let run = loop { if let Some((k, _)) = served.store.under("run:").into_iter().next() { break k.trim_start_matches("run:").to_string() } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let runner = loop { if let Some(r) = Runner::of_run(served, &run) { break r } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let step = "dddddddd-0000-0000-0000-000000000004";
    runner.take();
    runner.steps(&[(step, "Run make test", 1, "inProgress", None, None)]);
    let lines: Vec<String> = (1..=12).map(|n| format!("\u{1b}[32mtest {n} passed\u{1b}[0m")).collect();
    runner.lines(step, 1, &lines);
    runner.log(1, &lines.iter().map(|l| format!("2026-10-15T10:00:01.0000000Z {l}")).collect::<Vec<_>>().join("\n"));
    runner.steps(&[(step, "Run make test", 1, "completed", Some("succeeded"), Some(1))]);
    runner.finish("succeeded");
    let _ = cli.wait_with_output();
}

/// A dashboard running for a test: ended when the test ends, a failed one too (`kill` ends it sooner).
pub struct Dashboard(std::process::Child);
// (Waited for as it ends: one still ending can write its sign-ins over what a test writes next.)
impl Dashboard { pub fn kill(&mut self) -> std::io::Result<()> { let _ = self.0.kill(); self.0.wait().map(|_| ()) } }
impl Drop for Dashboard { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

/// A dashboard on this computer for `home`'s workspace, and its link (with the key that opens it).
pub fn dashboard(home: &std::path::Path) -> (Dashboard, String) { dashboard_with(home, &[]) }

/// The same, with these settings of its own.
pub fn dashboard_with(home: &std::path::Path, envs: &[(&str, &str)]) -> (Dashboard, String) {
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
    let mut child = command(home).args(["dashboard", "--no-browser", "--workspace=default"]).env("SUPERCI_PORT", port.to_string()).envs(envs.iter().copied())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
    let link = lines.find_map(|l| l.split_whitespace().find(|w| w.starts_with("http://localhost")).map(str::to_string)).expect("the dashboard's link");
    // (What it prints later is read too: a closed pipe would end it, a full one stop it. What it says of itself is
    // passed on, for a test that fails.)
    std::thread::spawn(move || lines.for_each(drop));
    let errors = BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok);
    std::thread::spawn(move || errors.for_each(|l| eprintln!("dashboard: {l}")));
    (Dashboard(child), link)
}

/// A dashboard as `dashboard_with` starts it, and each line it wrote to its errors (a panic among them), kept for the
/// test to read.
pub fn dashboard_heard(home: &std::path::Path, envs: &[(&str, &str)]) -> (Dashboard, String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    // (Started again on another port when the one picked was taken meanwhile: many start side by side in a sweep.)
    let (mut child, link) = (0..5).find_map(|_| {
        let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        let mut child = command(home).args(["dashboard", "--no-browser", "--workspace=default"]).env("SUPERCI_PORT", port.to_string()).envs(envs.iter().copied())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
        let link = lines.find_map(|l| l.split_whitespace().find(|w| w.starts_with("http://localhost")).map(str::to_string));
        std::thread::spawn(move || lines.for_each(drop));
        match link { Some(link) => Some((child, link)), None => { let _ = child.kill(); let _ = child.wait(); None } }
    }).expect("the dashboard's link");
    let heard = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let kept = heard.clone();
    let errors = BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok);
    std::thread::spawn(move || errors.for_each(|l| kept.lock().unwrap().push(l)));
    (Dashboard(child), link, heard)
}

/// What the dashboard answers for `path`, as its own page asks (with the cookie its link set): status and body.
pub fn part(link: &str, path: &str) -> (u16, String) {
    let (base, key) = link.split_once("/?k=").unwrap();
    let port = base.rsplit(':').next().unwrap();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let mut r = agent.get(&format!("{base}{path}")).header("cookie", &format!("superci_local_{port}={key}")).header("sec-fetch-site", "same-origin").call().unwrap();
    (r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default())
}

/// A page's words, without its markup.
pub fn words(html: &str) -> String {
    // (Without its styles and scripts.)
    let mut html = html.to_string();
    for (open, close) in [("<style", "</style>"), ("<script", "</script>")] {
        while let Some(a) = html.find(open) { let b = html[a..].find(close).map(|b| a + b + close.len()).unwrap_or(html.len()); html.replace_range(a..b, " "); }
    }
    let mut out = String::new();
    let mut tag = false;
    for c in html.chars() {
        match c { '<' => { tag = true; out.push(' ') } '>' => tag = false, c if !tag => out.push(c), _ => {} }
    }
    out.replace("&#39;", "'").replace("&amp;", "&").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A form of the dashboard's own pages sent (`fields`), as a browser sends it: status, where it leads, and the body.
pub fn post(link: &str, path: &str, fields: &[(&str, &str)]) -> (u16, String, String) {
    let (base, key) = link.split_once("/?k=").unwrap();
    let port = base.rsplit(':').next().unwrap();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
    let mut r = agent.post(&format!("{base}{path}")).header("cookie", &format!("superci_local_{port}={key}")).header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded").send(body).unwrap();
    let to = r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    (r.status().as_u16(), to, r.body_mut().read_to_string().unwrap_or_default())
}

/// The same for a page reached with a GET, with where it leads.
pub fn get(link: &str, path: &str) -> (u16, String, String) {
    let (base, key) = link.split_once("/?k=").unwrap();
    let port = base.rsplit(':').next().unwrap();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let mut r = agent.get(&format!("{base}{path}")).header("cookie", &format!("superci_local_{port}={key}")).header("sec-fetch-site", "same-origin").call().unwrap();
    let to = r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    (r.status().as_u16(), to, r.body_mut().read_to_string().unwrap_or_default())
}

/// A workspace signed in to an AWS account with no control plane yet (the account's AWS: `account`).
pub fn aws_only(account: &superci_testkit::Aws) -> std::path::PathBuf {
    let home = temp("aws-only");
    let ws = home.join("workspaces/default");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("sign-ins.json"), serde_json::json!({ "aws": { "region": "us-east-1", "account_id": "111111111111", "who": "arn:aws:sts::111111111111:assumed-role/Admin/someone",
        "creds": { "access_key_id": "ASIAEXAMPLE", "secret_access_key": "example", "session_token": "example", "expires_at_ms": 4102444800000u64 }, "refresh_token": "", "dpop": { "private_b64": "", "kid": "" } } }).to_string()).unwrap();
    std::fs::write(home.join("aws-at"), &account.url).unwrap();
    home
}

/// A dashboard whose time the test moves on (`later`), with these settings of its own.
pub fn dashboard_in_time(home: &std::path::Path, envs: &[(&str, &str)]) -> (Dashboard, String) {
    let file = home.join("later-ms").display().to_string();
    dashboard_with(home, &[&[("SUPERCI_LATER", file.as_str())][..], envs].concat())
}

/// That dashboard's time moved on by `ms`: what it does after minutes, without waiting them.
pub fn later(home: &std::path::Path, ms: u64) {
    let file = home.join("later-ms");
    let was: u64 = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    std::fs::write(file, (was + ms).to_string()).unwrap();
}

/// The CLI run in `home`'s workspace with these settings of its own, ended if not over in `secs`: whether it did
/// well, and what it said (out, then errors).
pub fn superci_in_time(home: &std::path::Path, args: &[&str], envs: &[(&str, &str)], secs: u64) -> (bool, String) {
    let child = command(home).args(args).envs(envs.iter().copied()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });
    let out = match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(out) => out.unwrap(),
        Err(_) => { let _ = Command::new("kill").args(["-9", &pid.to_string()]).status(); panic!("{args:?}: not over in {secs} s") }
    };
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

/// `home`'s workspace keeping its control plane at `url` in place of the one served (a server in front of it).
pub fn kept_at(home: &std::path::Path, url: &str) {
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"]["Aws"]["url"] = url.into();
    std::fs::write(file, kept.to_string()).unwrap();
}

/// A dashboard on this computer for `home`, started as `superci dashboard` is with these arguments (none: no
/// workspace named, as one started with several workspaces kept), and these settings of its own; its link.
pub fn dashboard_args(home: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> (Dashboard, String) {
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
    let mut child = command(home).args([&["dashboard", "--no-browser"][..], args].concat()).env("SUPERCI_PORT", port.to_string()).envs(envs.iter().copied())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
    let link = lines.find_map(|l| l.split_whitespace().find(|w| w.starts_with("http://localhost")).map(str::to_string)).expect("the dashboard's link");
    std::thread::spawn(move || lines.for_each(drop));
    let errors = BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok);
    std::thread::spawn(move || errors.for_each(|l| eprintln!("dashboard: {l}")));
    (Dashboard(child), link)
}

/// One of Modal's calls answered as a recording has it (each answer protobuf, in base64): for what a real workspace
/// was not caught answering.
pub fn modal_answered(method: &str, answers: &[Vec<u8>]) -> String {
    use base64::Engine;
    let answers: Vec<String> = answers.iter().map(|a| base64::engine::general_purpose::STANDARD.encode(a)).collect();
    serde_json::json!({ "method": "POST", "url": format!("https://api.modal.com/modal.client.ModalClient/{method}"), "target": "", "asked": "", "status": 200, "headers": [],
        "answer": serde_json::json!({ "answers": answers }).to_string() }).to_string() + "\n"
}

/// A protobuf message's field: its number, and a number, a double or bytes (a string, another message).
pub enum Pb { N(u32, u64), D(u32, f64), B(u32, Vec<u8>) }

/// A protobuf message of these fields.
pub fn pb(fields: &[Pb]) -> Vec<u8> {
    let varint = |out: &mut Vec<u8>, mut n: u64| loop { let b = (n & 0x7f) as u8; n >>= 7; if n == 0 { out.push(b); break } out.push(b | 0x80) };
    let mut out = vec![];
    for f in fields {
        match f {
            Pb::N(k, n) => { varint(&mut out, (*k as u64) << 3); varint(&mut out, *n) }
            Pb::D(k, d) => { varint(&mut out, ((*k as u64) << 3) | 1); out.extend(d.to_le_bytes()) }
            Pb::B(k, b) => { varint(&mut out, ((*k as u64) << 3) | 2); varint(&mut out, b.len() as u64); out.extend(b) }
        }
    }
    out
}
