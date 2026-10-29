//! The Modal control plane's program as plane.py runs it: the real `superci-plane`, given each request as one JSON
//! line on stdin, and what only Modal's client can do (its Dict, wake-ups, sandboxes) answered here as plane.py
//! answers it. Its calls to GitHub are answered by the test kit's stand-in.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use superci_testkit::{Aws, GitHub, GitLab};

const PLANE: &str = "mdplane00001";

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

/// plane.py's side: the program it started, the Dict it keeps for it, and what was asked of it.
struct PlanePy {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    dict: BTreeMap<String, String>,
    asked: Vec<Value>,
    /// What the next question of this op is answered with instead (an error, an answer without what it should say).
    answers: BTreeMap<String, Value>,
}

impl Drop for PlanePy {
    // (Stopped as Modal stops a container, so a coverage tool's buffered evidence is written; killed if it stays.)
    fn drop(&mut self) {
        self.input.take();
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM); }
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until { if let Ok(Some(_)) = self.child.try_wait() { return } std::thread::sleep(Duration::from_millis(20)) }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PlanePy {
    fn start(aws: &Aws) -> PlanePy { Self::start_with(&aws.url, Some("AcmeCI")) }

    /// Started with the clouds' stand-in at `stand_in`, and its label from the deploy (none: the default).
    fn start_with(stand_in: &str, label: Option<&str>) -> PlanePy {
        let mut c = Command::new(env!("CARGO_BIN_EXE_superci-plane"));
        c.env("PLANE_ID", PLANE).env("SUPERCI_STAND_IN", stand_in).env_remove("LABEL").stdin(Stdio::piped()).stdout(Stdio::piped());
        if let Some(l) = label { c.env("LABEL", l); }
        let mut child = c.spawn().unwrap();
        let (input, output) = (child.stdin.take(), BufReader::new(child.stdout.take().unwrap()));
        PlanePy { child, input, output, dict: BTreeMap::new(), asked: vec![], answers: BTreeMap::new() }
    }

    fn line(&mut self) -> Option<Value> {
        let mut line = String::new();
        if self.output.read_line(&mut line).ok()? == 0 { return None }
        Some(serde_json::from_str(&line).unwrap())
    }

    fn send(&mut self, v: &Value) { let i = self.input.as_mut().unwrap(); writeln!(i, "{v}").unwrap(); i.flush().unwrap(); }

    /// A message given, its questions answered as plane.py answers them, until its answer (or the program's end).
    fn exchange(&mut self, msg: Value) -> Option<Value> {
        self.send(&msg);
        loop {
            let q = self.line()?;
            if q["type"] != "ask" { return Some(q) }
            self.asked.push(q.clone());
            let op = q["op"].as_str().unwrap().to_string();
            if let Some(a) = self.answers.remove(&op) { self.send(&a); continue }
            let key = q["key"].as_str().unwrap_or_default().to_string();
            let a = match op.as_str() {
                "get" => json!({ "value": self.dict.get(&key) }),
                "get_many" => json!({ "values": q["keys"].as_array().unwrap().iter().map(|k| self.dict.get(k.as_str().unwrap())).collect::<Vec<_>>() }),
                "put" => { self.dict.insert(key, q["value"].as_str().unwrap().into()); json!({}) }
                "put_if_absent" => { let created = !self.dict.contains_key(&key); if created { self.dict.insert(key, q["value"].as_str().unwrap().into()); } json!({ "created": created }) }
                "delete" => { self.dict.remove(&key); json!({}) }
                "list" => { let p = q["prefix"].as_str().unwrap(); json!({ "items": self.dict.iter().filter(|(k, _)| k.starts_with(p)).map(|(k, v)| json!([k, v])).collect::<Vec<_>>() }) }
                "migrate" => json!({ "done": true }),
                "wake" => json!({}),
                "sandbox_start" => json!({ "id": format!("sb-{}", self.asked.len()) }),
                "sandbox_stop" => json!({}),
                other => panic!("plane.py has no {other}"),
            };
            self.send(&a);
        }
    }

    fn ops(&self) -> Vec<String> { self.asked.iter().map(|q| q["op"].as_str().unwrap().to_string()).collect() }
}

/// A request through the endpoint, as plane.py hands it on (its body in base64), with the settings the dashboard's Dict
/// has.
fn request(method: &str, path: &str, headers: Value, body: &[u8], secrets: &Value) -> Value {
    json!({ "kind": "request", "method": method, "url": format!("https://acmeorg--superci-plane-{PLANE}.modal.run{path}"), "headers": headers, "body": STANDARD.encode(body), "secrets": secrets })
}

fn body(answer: &Value) -> String { String::from_utf8(STANDARD.decode(answer["body"].as_str().unwrap()).unwrap()).unwrap() }

fn aws() -> Aws {
    let aws = Aws::serve();
    *aws.github.lock().unwrap() = Some(GitHub::default());
    aws
}

#[test]
fn the_control_plane_answers_plane_py_keeps_its_dict_and_starts_sandboxes_as_modal_runs_it() {
    let aws = aws();
    let gh = aws.github.lock().unwrap().clone().unwrap();
    let secrets = json!({ "CONTAINERS": "on", "GITHUB_APP": GitHub::app().to_string(), "GITLAB": GitLab::connection().to_string(), "DEVICE_KEY_4102444800_TEST": "dashboard-key-for-tests-0001", "UNREAD": 1 });
    let mut py = PlanePy::start(&aws);
    // Its endpoint: what it answers, its label from the deploy, its state in the Dict.
    let health = py.exchange(request("GET", "/health", json!({ "Accept": "application/json", "x-number": 1 }), b"", &secrets)).unwrap();
    assert_eq!((health["type"].as_str(), health["status"].as_u64()), (Some("response"), Some(200)), "{health}");
    let said: Value = serde_json::from_str(&body(&health)).unwrap();
    assert_eq!((said["plane"].as_str(), said["label"].as_str()), (Some(PLANE), Some("acmeci")), "{said}");
    // A message saying nothing of its method, address or body: asked as a GET of /.
    let bare = py.exchange(json!({ "kind": "request", "secrets": {} })).unwrap();
    assert_eq!(bare["type"], "response", "{bare}");
    // A job queued at GitHub: its sandbox started by plane.py, as large as asked, with its runner's settings.
    let (_, hook) = gh.queue("build", "acmeci", now_ms());
    let started = py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1, "content-type": "application/json" }), &hook.0, &secrets)).unwrap();
    assert!(started["status"].as_u64().unwrap() < 300, "{started} {}", body(&started));
    let start = py.asked.iter().find(|q| q["op"] == "sandbox_start").cloned().unwrap_or_else(|| panic!("{:?}", py.ops()));
    assert!(start["jit"].as_str().is_some_and(|j| !j.is_empty()) && start["cpu"].as_u64().is_some(), "{start}");
    // One plane.py starts without saying its sandbox, one it could not start: the job not left as started.
    for answer in [json!({}), json!({ "error": "Modal has no room" })] {
        py.answers.insert("sandbox_start".into(), answer);
        let (_, hook) = gh.queue("test", "acmeci", now_ms());
        py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
    }
    // A label it cannot read: a sandbox that fails the job, saying why.
    let (_, hook) = gh.queue("odd", "acmeci/cpu=banana", now_ms());
    py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
    assert!(py.asked.iter().any(|q| q["op"] == "sandbox_start" && q["fail"].is_string()), "{:?}", py.ops());
    // A GitLab job: no sandbox (Modal runs no Docker).
    let lab = GitLab::default();
    let (_, pending) = lab.pending("gl-test", &["acmeci"], now_ms());
    let before = py.ops().iter().filter(|o| *o == "sandbox_start").count();
    py.exchange(request("POST", "/gitlab/webhook", json!({ "x-gitlab-event": "Job Hook", "x-gitlab-token": superci_testkit::gitlab::HOOK_SECRET }), &pending, &secrets)).unwrap();
    assert_eq!(py.ops().iter().filter(|o| *o == "sandbox_start").count(), before);
    // The first job's log, where GitHub keeps it (a link GitHub gives without following it).
    let first: u64 = start["name"].as_str().unwrap().rsplit('-').next().unwrap().parse().unwrap_or(0);
    let log = py.exchange(request("GET", &format!("/job/log?id={first}&link=1"), json!({ "authorization": "Bearer dashboard-key-for-tests-0001" }), b"", &secrets)).unwrap();
    eprintln!("log: {}", body(&log));
    // The first job done at GitHub: its sandbox stopped.
    let (id, _) = (first, ());
    py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": gh.start(id).1 }), &gh.start(id).0, &secrets)).unwrap();
    let fin = gh.finish(id, "success", "ok", now_ms());
    py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": fin.1 }), &fin.0, &secrets)).unwrap();
    // One cancelled at GitHub before its runner took it: its sandbox stopped.
    let (cancelled, hook) = gh.queue("lint", "acmeci", now_ms());
    py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
    let fin = gh.finish(cancelled, "cancelled", "", now_ms());
    py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": fin.1 }), &fin.0, &secrets)).unwrap();
    assert!(py.ops().iter().any(|o| o == "sandbox_stop"), "{:?}", py.ops());
    // A list from the Dict with entries that are not a name and a value: left out; one not made.
    py.answers.insert("list".into(), json!({ "items": [[1, "x"], ["k", 2], "neither"] }));
    py.answers.insert("put_if_absent".into(), json!({ "error": "the Dict is busy" }));
    py.exchange(json!({ "kind": "alarm", "secrets": secrets })).unwrap();
    py.answers.insert("migrate".into(), json!({ "error": "the old Dict is not there" }));
    py.exchange(json!({ "kind": "alarm", "secrets": secrets })).unwrap();
    eprintln!("asked: {:?}", py.ops());
    // Its upkeep, as the schedule calls it.
    let done = py.exchange(json!({ "kind": "alarm", "secrets": secrets })).unwrap();
    assert_eq!(done, json!({ "type": "done", "error": null }));
    // The Dict not answering (what it lists, then what it holds): said, the upkeep's error given back.
    for op in ["list", "get"] {
        py.answers.insert(op.into(), json!({ "error": "the Dict is not there" }));
        let failed = py.exchange(json!({ "kind": "alarm", "secrets": secrets })).unwrap();
        assert!(failed["error"].as_str().is_some_and(|e| e.contains("the Dict is not there")), "{op}: {failed}");
        py.answers.clear();
    }
    // plane.py gone while the program waits for an answer: said as the request's answer, and the program ends.
    py.send(&request("GET", "/health", json!({}), b"", &secrets));
    let q = py.line().unwrap();
    assert_eq!(q["type"], "ask", "{q}");
    py.input.take();
    let last = py.line().unwrap();
    assert_eq!((last["status"].as_u64(), body(&last).as_str()), (Some(500), "plane.py went away"));
    assert_eq!(py.line(), None);
}

/// A line that is not a message, or not even text, ends it, as plane.py going away does.
#[test]
fn a_line_that_is_no_message_ends_the_program() {
    for line in [&b"not json\n"[..], b"\xff\xfe\n"] {
        let aws = aws();
        let mut py = PlanePy::start(&aws);
        let i = py.input.as_mut().unwrap();
        i.write_all(line).unwrap();
        i.flush().unwrap();
        assert_eq!(py.line(), None);
        assert!(py.child.wait().unwrap().success());
    }
}

/// An address that answers each request as given: a header that is not text, then a body cut short.
fn odd_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || for mut s in listener.incoming().flatten() {
        let mut head = [0u8; 4096];
        let _ = std::io::Read::read(&mut s, &mut head);
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nx-odd: \xff\xfe\r\ncontent-length: 1000\r\nconnection: close\r\n\r\nshort");
    });
    at
}

/// GitHub not reached, or answering amiss (a header that is not text, a body cut short), or an address no request
/// can be made to: the job not started, said in its answer; with no label from the deploy, the default one.
#[test]
fn github_out_of_reach_leaves_the_job_unstarted() {
    let secrets = json!({ "CONTAINERS": "on", "GITHUB_APP": GitHub::app().to_string() });
    let closed = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); format!("http://{}", l.local_addr().unwrap()) };
    for at in [odd_server(), closed, "http://not a host".to_string()] {
        let gh = GitHub::default();
        let mut py = PlanePy::start_with(&at, None);
        let health = py.exchange(request("GET", "/health", json!({}), b"", &secrets)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&body(&health)).unwrap()["label"], "superci");
        let (_, hook) = gh.queue("build", "superci", now_ms());
        let answer = py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
        assert_eq!(answer["type"], "response", "{answer}");
        assert!(!py.ops().iter().any(|o| o == "sandbox_start"), "{at}: {:?}", py.ops());
    }
}

/// A run from a computer: its job's sandbox started with the control plane's own runner service, which hands the job to
/// the runner once, then tells it to wait, at once (plane.py holds that answer, once it has let go of the program, so
/// that the runner's other calls are not held behind it).
#[test]
fn a_run_from_a_computer_hands_its_job_once_then_tells_the_runner_to_wait() {
    let aws = aws();
    let key = "superci_run_0123456789abcdef0123456789abcdef";
    let secrets = json!({ "CONTAINERS": "on", "GITHUB_APP": GitHub::app().to_string(), "READ_KEY_4102444800_AGENT": format!("{}:runs", superci_core::crypto::sha256_hex(key.as_bytes())) });
    let mut py = PlanePy::start(&aws);
    let workflow = "jobs:\n  test:\n    runs-on: acmeci\n    steps:\n      - run: echo hi\n";
    let made = py.exchange(request("POST", "/runs", json!({ "authorization": format!("Bearer {key}") }), json!({ "workflow": workflow, "path": ".github/workflows/ci.yml", "repository": "acme/app" }).to_string().as_bytes(), &secrets)).unwrap();
    assert_eq!(made["status"].as_u64(), Some(200), "{}", body(&made));
    let start = py.asked.iter().find(|q| q["op"] == "sandbox_start").cloned().unwrap_or_else(|| panic!("{:?}", py.ops()));
    let unb64 = |v: &Value| -> Value { serde_json::from_slice(&STANDARD.decode(v.as_str().unwrap()).unwrap()).unwrap() };
    let server = unb64(&unb64(&start["jit"])[".runner"])["ServerUrl"].as_str().unwrap().to_string();
    let path = server.split_once(".modal.run").unwrap().1.to_string();
    let message = |py: &mut PlanePy| py.exchange(request("GET", &format!("{path}/_apis/distributedtask/pools/1/Message/1"), json!({}), b"", &secrets)).unwrap();
    let first = message(&mut py);
    assert!(body(&first).contains("PipelineAgentJobRequest"), "{}", body(&first));
    let began = Instant::now();
    let then = message(&mut py);
    assert_eq!((then["status"].as_u64(), then["headers"]["x-superci-hold"].as_str()), (Some(204), Some("20000")), "{then}");
    assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
}

/// A GitHub job its routing sends to AWS (through the role its AWS account gave it), AWS refusing the first request
/// for its rate: asked again after a wait, as the other control planes do, and its machine started.
#[test]
fn a_job_on_aws_is_asked_again_after_a_wait_when_aws_refuses_it_for_its_rate() {
    let aws = aws();
    let gh = aws.github.lock().unwrap().clone().unwrap();
    aws.replaying(include_str!("../../cli/fixtures/aws/launch/spot.jsonl"), &[]);
    aws.fail("RunInstances", 503, &superci_testkit::ec2_error("RequestLimitExceeded", "Request limit exceeded."), Some(1));
    let routing = json!({ "default": null, "rules": [], "order": [{ "cloud": "aws", "max_jobs": 100 }] });
    let secrets = json!({ "GITHUB_APP": GitHub::app().to_string(), "ROUTING": routing.to_string() });
    let mut py = PlanePy::start(&aws);
    py.dict.insert("aws".into(), json!({ "account_id": "111111111111", "region": "us-east-1", "role_arn": format!("arn:aws:iam::111111111111:role/superci-plane-{PLANE}"), "connected": true }).to_string());
    py.dict.insert("aws:no-fleet".into(), now_ms().to_string());
    let (_, hook) = gh.queue("build", "acmeci", now_ms());
    let answer = py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
    assert!(answer["status"].as_u64().unwrap() < 300, "{answer} {}", body(&answer));
    let runs = aws.asked.lock().unwrap().iter().filter(|a| a["asked"].as_str().unwrap_or_default().contains("Action=RunInstances")).count();
    let asked: Vec<String> = aws.asked.lock().unwrap().iter().map(|a| format!("{} {}", a["url"], a["asked"].as_str().unwrap_or_default().chars().take(60).collect::<String>())).collect();
    assert!(runs >= 2, "{runs} RunInstances: {}\n{asked:#?}", py.dict["job:90000000001"]);
}

/// The status a page shows reads the jobs it lists, their machines' starts and each day's totals several at once: as
/// many questions to plane.py (each a call to Modal's API) for fifty jobs as for five.
#[test]
fn the_status_reads_its_records_several_at_once_however_many_jobs_there_are() {
    let aws = aws();
    let gh = aws.github.lock().unwrap().clone().unwrap();
    let secrets = json!({ "CONTAINERS": "on", "GITHUB_APP": GitHub::app().to_string(), "DEVICE_KEY_4102444800_TEST": "dashboard-key-for-tests-0001" });
    let mut py = PlanePy::start(&aws);
    // (Its jobs indexed, as every control plane's are once its upkeep has run.)
    py.dict.insert("indexed".into(), now_ms().to_string());
    let (mut questions, mut queued) = (vec![], 0);
    for jobs in [5, 50] {
        while queued < jobs {
            queued += 1;
            let (_, hook) = gh.queue("build", "acmeci", now_ms());
            py.exchange(request("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": hook.1 }), &hook.0, &secrets)).unwrap();
        }
        // (Asked twice: the first after a new key also notes it.)
        let status = |py: &mut PlanePy| py.exchange(request("GET", "/status", json!({ "authorization": "Bearer dashboard-key-for-tests-0001" }), b"", &secrets)).unwrap();
        status(&mut py);
        let before = py.asked.len();
        let status = status(&mut py);
        let said: Value = serde_json::from_str(&body(&status)).unwrap();
        assert_eq!(said["jobs"].as_array().map(Vec::len), Some(jobs), "{}", body(&status));
        let asked: Vec<String> = py.ops()[before..].to_vec();
        assert!(asked.iter().any(|o| o == "get_many"), "{asked:?}");
        questions.push(asked.len());
    }
    assert_eq!(questions[0], questions[1], "as many questions for 50 jobs as for 5");
}
