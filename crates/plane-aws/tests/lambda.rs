//! The AWS control plane's program as Lambda runs it: the real `bootstrap`, given its requests by a stand-in for
//! Lambda's runtime API here, its calls to AWS answered by the test kit's stand-in (DynamoDB's items, SSM's
//! parameters, S3's objects, Lambda's calls). Once as the control plane (a request through its URL, the sweep, its own
//! wake-up), once as the layer store's converter (with a stand-in for mkfs.erofs).
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};
use superci_testkit::{Aws, GitHub};

const PLANE: &str = "lambdaplane1";
const TABLE: &str = "superci-plane-lambdaplane1";

fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

/// Lambda's runtime API, as a function's program sees it: its next request (waited for), and its answer to each.
#[derive(Default)]
struct Runtime { queue: Mutex<VecDeque<(String, Value)>>, queued: Condvar, answers: Mutex<BTreeMap<String, (String, Value)>>, answered: Condvar }

impl Runtime {
    fn serve() -> (Arc<Runtime>, String) { Self::serve_on(TcpListener::bind("127.0.0.1:0").unwrap()) }

    fn serve_on(listener: TcpListener) -> (Arc<Runtime>, String) {
        let at = listener.local_addr().unwrap().to_string();
        let rt = Arc::new(Runtime::default());
        let r = rt.clone();
        std::thread::spawn(move || for stream in listener.incoming().flatten() { let r = r.clone(); std::thread::spawn(move || r.exchange(stream)); });
        (rt, at)
    }

    fn exchange(&self, mut stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() { return }
        let path = line.split(' ').nth(1).unwrap_or_default().to_string();
        let mut length = 0;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h.trim().is_empty() { break }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") { length = v.trim().parse().unwrap_or(0) }
        }
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        let answer = |stream: &mut TcpStream, extra: &str, body: &str| { let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()); };
        if path.ends_with("/invocation/next") {
            let mut q = self.queue.lock().unwrap();
            while q.is_empty() { q = self.queued.wait(q).unwrap() }
            let (id, event) = q.pop_front().unwrap();
            drop(q);
            answer(&mut stream, &format!("lambda-runtime-aws-request-id: {id}\r\nlambda-runtime-invoked-function-arn: arn:aws:lambda:us-east-1:111111111111:function:{TABLE}\r\n"), &event.to_string());
        } else if let Some(rest) = path.strip_prefix("/2018-06-01/runtime/invocation/") {
            let (id, kind) = rest.split_once('/').unwrap_or_default();
            self.answers.lock().unwrap().insert(id.to_string(), (kind.to_string(), serde_json::from_slice(&body).unwrap_or(Value::Null)));
            self.answered.notify_all();
            answer(&mut stream, "", "{}");
        }
    }

    /// A request given to the function, and how it answered (`response` or `error`, and what it said).
    fn invoke(&self, event: Value) -> (String, Value) {
        let id = format!("request-{}", now_ms() % 1_000_000_007 + self.answers.lock().unwrap().len() as u64);
        self.queue.lock().unwrap().push_back((id.clone(), event));
        self.queued.notify_all();
        let until = Instant::now() + Duration::from_secs(60);
        let mut answers = self.answers.lock().unwrap();
        loop {
            if let Some(a) = answers.remove(&id) { return a }
            assert!(Instant::now() < until, "the function did not answer");
            answers = self.answered.wait_timeout(answers, Duration::from_millis(200)).unwrap().0;
        }
    }
}

/// The program, started as Lambda starts it (`env`: what this function's are).
struct Function(Child);
impl Drop for Function { fn drop(&mut self) { stop(&mut self.0) } }

/// Stopped as Lambda stops it (SIGTERM), so a coverage tool's buffered evidence is written; killed if it stays.
fn stop(c: &mut Child) {
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM); }
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until { if let Ok(Some(_)) = c.try_wait() { return } std::thread::sleep(Duration::from_millis(20)) }
    let _ = c.kill();
    let _ = c.wait();
}

fn start(runtime: &str, stand_in: &str, env: &[(&str, &str)]) -> Function {
    let mut c = Command::new(env!("CARGO_BIN_EXE_bootstrap"));
    // (Only what the program reads is set or taken away: the rest, a coverage tool's among it, stays.)
    for k in ["PLANE_ID", "TABLE", "LABEL", "RUNNER_REGION", "INSTANCE_TYPES", "AWS_LAMBDA_FUNCTION_NAME", "SUPERCI_CONVERTER", "LAMBDA_TASK_ROOT", "AWS_PROFILE"] { c.env_remove(k); }
    c.env("AWS_LAMBDA_RUNTIME_API", runtime).env("AWS_REGION", "us-east-1").env("SUPERCI_STAND_IN", stand_in)
        .env("AWS_ACCESS_KEY_ID", "ASIAEXAMPLEFUNCTION1").env("AWS_SECRET_ACCESS_KEY", "example-secret").env("AWS_SESSION_TOKEN", "example-token");
    for (k, v) in env { c.env(k, v); }
    // (A folder of its own for what it makes: Lambda's /tmp is the function's alone.)
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp = std::env::temp_dir().join(format!("superci-lambda-tmp-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    std::fs::create_dir_all(&tmp).unwrap();
    c.env("TMPDIR", &tmp);
    Function(c.spawn().unwrap())
}

/// A request through the function's URL, as Lambda gives it (payload 2.0).
fn url_event(method: &str, path: &str, headers: Value, body: &str) -> Value {
    json!({ "version": "2.0", "rawPath": path, "rawQueryString": "", "headers": headers, "isBase64Encoded": true, "body": base64::engine::general_purpose::STANDARD.encode(body),
        "requestContext": { "domainName": "lambdaplane1.lambda-url.us-east-1.on.aws", "http": { "method": method } } })
}

fn item(aws: &Aws, key: &str) -> Option<String> {
    let p = key.split(':').next().unwrap();
    aws.items.lock().unwrap().get(&(TABLE.to_string(), p.to_string(), key.to_string())).and_then(|i| i["v"]["S"].as_str().map(str::to_string))
}

fn aws() -> Aws {
    let aws = Aws::serve();
    *aws.github.lock().unwrap() = Some(GitHub::default());
    aws
}

#[test]
fn the_control_plane_answers_its_url_sweeps_and_wakes_itself_as_lambda_runs_it() {
    let aws = aws();
    aws.put(&format!("/superci/{PLANE}/MAX_CPU"), "16");
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, &aws.url, &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("LABEL", "AcmeCI"), ("RUNNER_REGION", "us-east-1"), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    // Its URL: what it answers, its settings read from SSM, its state in its table.
    let (kind, answer) = runtime.invoke(url_event("GET", "/health", json!({}), ""));
    assert_eq!((kind.as_str(), answer["statusCode"].as_u64()), ("response", Some(200)), "{answer}");
    let health: Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(answer["body"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!((health["plane"].as_str(), health["label"].as_str(), health["version"].as_str()), (Some(PLANE), Some("acmeci"), Some(env!("CARGO_PKG_VERSION"))), "{health}");
    assert_eq!(item(&aws, "plane_url").as_deref(), Some("\"https://lambdaplane1.lambda-url.us-east-1.on.aws\""));
    let reads = || aws.asked.lock().unwrap().iter().filter(|a| a["target"] == "AmazonSSM.GetParametersByPath").count();
    // (Settings read at most every 15 seconds; at once for a key it has not seen, a new session's.)
    let before = reads();
    runtime.invoke(url_event("GET", "/health", json!({}), ""));
    assert_eq!(reads(), before, "read a moment ago");
    std::thread::sleep(Duration::from_millis(1100));
    runtime.invoke(url_event("GET", "/status", json!({ "authorization": "Bearer a-new-sessions-key-0123456789" }), ""));
    assert_eq!(reads(), before + 1, "a key it has not seen: read again at once");
    // The schedule's sweep; one while another goes is left to that one.
    let (kind, swept) = runtime.invoke(json!({ "superci": "sweep" }));
    assert_eq!((kind.as_str(), &swept), ("response", &json!({ "swept": true })));
    assert_eq!(item(&aws, "aws:sweeping"), None, "its note taken away once done");
    aws.items.lock().unwrap().insert((TABLE.into(), "aws".into(), "aws:sweeping".into()), json!({ "p": { "S": "aws" }, "k": { "S": "aws:sweeping" }, "v": { "S": now_ms().to_string() } }));
    assert_eq!(runtime.invoke(json!({ "superci": "sweep" })).1, json!({ "swept": false }));
    // One left by a sweep cut off long ago is not waited for.
    aws.items.lock().unwrap().insert((TABLE.into(), "aws".into(), "aws:sweeping".into()), json!({ "p": { "S": "aws" }, "k": { "S": "aws:sweeping" }, "v": { "S": (now_ms() - 600_000).to_string() } }));
    assert_eq!(runtime.invoke(json!({ "superci": "sweep" })).1, json!({ "swept": true }));
    // Its own wake-up: the time waited out, then a sweep, its note gone.
    aws.items.lock().unwrap().insert((TABLE.into(), "aws".into(), "aws:waking".into()), json!({ "p": { "S": "aws" }, "k": { "S": "aws:waking" }, "v": { "S": now_ms().to_string() } }));
    let began = Instant::now();
    assert_eq!(runtime.invoke(json!({ "superci": "wake", "at": now_ms() + 1500 })).1, json!({ "swept": true }));
    assert!(began.elapsed() >= Duration::from_millis(1200), "waited for its time: {:?}", began.elapsed());
    assert_eq!(item(&aws, "aws:waking"), None);
    assert!(aws.unanswered.lock().unwrap().iter().all(|u| !u.contains("dynamodb")), "{:?}", aws.unanswered.lock().unwrap());
}

/// A stand-in for mkfs.erofs: the tar it is given, written where it is told (its last argument); a tar that says
/// "refuse" refused.
fn mkfs_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("superci-lambda-converter-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("mkfs.erofs");
    // (A tar that says "nofile" makes nothing, one that says "folder" a folder where the file should be.)
    std::fs::write(&p, "#!/bin/sh\nfor a; do out=$a; done\ncat > \"$out\"\nif grep -q refuse \"$out\"; then echo 'unusual entry' >&2; exit 1; fi\nif grep -q nofile \"$out\"; then rm \"$out\"; fi\nif grep -q folder \"$out\"; then rm \"$out\"; mkdir \"$out\"; fi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut out = Command::new("gzip").arg("-c").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    out.stdin.take().unwrap().write_all(b).unwrap();
    out.wait_with_output().unwrap().stdout
}

fn sha(b: &[u8]) -> String {
    let out = Command::new("shasum").args(["-a", "256"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = out;
    out.stdin.take().unwrap().write_all(b).unwrap();
    format!("sha256:{}", String::from_utf8(out.wait_with_output().unwrap().stdout).unwrap().split(' ').next().unwrap())
}

#[test]
fn the_converter_makes_a_layers_file_from_a_machines_upload_as_lambda_runs_it() {
    let aws = aws();
    let gh = aws.github.lock().unwrap().clone().unwrap();
    let (runtime, at) = Runtime::serve();
    let dir = mkfs_dir();
    let _f = start(&at, &aws.url, &[("SUPERCI_CONVERTER", "1"), ("LAMBDA_TASK_ROOT", dir.to_str().unwrap())]);
    let bucket = "superci-plane-lambdaplane1-us-east-1";
    let job = |digest: &str, size: usize, source: Value| {
        let hex = &digest[7..];
        json!({ "digest": digest, "size": size, "region": "us-east-1", "bucket": bucket, "source": source,
            "erofs_key": format!("layers/{hex}.erofs"), "blob_key": format!("layers/{hex}.blob"), "done_key": format!("layers/done/{hex}.json") })
    };
    let object = |key: &str| gh.objects().into_iter().find(|(k, _)| k == &format!("{bucket}/{key}")).map(|(_, n)| n);
    let staged = |blob: &[u8], tail: &str| { let key = format!("layers/staging/{}-{tail}", &sha(blob)[7..]); gh.put_object(&format!("{bucket}/{key}"), blob); key };
    // From staging (a machine's upload, after its job): its tar made a file, the blob copied in S3 and taken from
    // staging, what was made said beside them.
    let tar = b"a layer's tar".repeat(500);
    let blob = gzip(&tar);
    let d = sha(&blob);
    let key = staged(&blob, "abc");
    let (kind, done) = runtime.invoke(job(&d, blob.len(), json!({ "key": key })));
    assert_eq!(kind, "response");
    assert_eq!((done["digest"].as_str(), done["diff_id"].as_str(), done["erofs"]["size"].as_u64(), done["error"].as_str(), done["blob"]["key"].as_str()), (Some(d.as_str()), Some(sha(&tar).as_str()), Some(tar.len() as u64), None, Some(format!("layers/{}.blob", &d[7..]).as_str())), "{done}");
    assert_eq!((object(&format!("layers/{}.erofs", &d[7..])), object(&format!("layers/{}.blob", &d[7..])), object(&key)), (Some(tar.len()), Some(blob.len()), None));
    assert!(object(&format!("layers/done/{}.json", &d[7..])).is_some());
    // What it refuses, said as such: a tar mkfs.erofs takes not, a blob not of its digest, a digest of nothing, a blob
    // from anywhere but staging.
    let odd = gzip(b"refuse this");
    let (_, done) = runtime.invoke(job(&sha(&odd), odd.len(), json!({ "key": staged(&odd, "def") })));
    assert!(done["error"].as_str().is_some_and(|e| e.starts_with("mkfs.erofs refused the layer") && e.ends_with("unusual entry")), "{done}");
    let wrong = format!("sha256:{}", "c".repeat(64));
    let (_, done) = runtime.invoke(job(&wrong, blob.len(), json!({ "key": staged(&blob, "ghi") })));
    assert!(done["error"].as_str().unwrap().contains("not sha256:cccc"), "{done}");
    assert!(object(&format!("layers/{}.erofs", "c".repeat(64))).is_none(), "nothing made of it");
    let (_, done) = runtime.invoke(job("sha256:nope", 1, json!({ "key": "x" })));
    assert_eq!(done["error"], "not a sha256 digest");
    let (_, done) = runtime.invoke(job(&wrong, 5 << 30, json!({ "key": "x" })));
    assert!(done["error"].as_str().unwrap().contains("converted up to"), "{done}");
    let (_, done) = runtime.invoke(job(&d, blob.len(), json!({ "url": "https://registry-1.docker.io/v2/library/alpine/blobs/x" })));
    assert_eq!(done["error"], "no blob in staging");
    let (kind, done) = runtime.invoke(json!({ "something": "else" }));
    assert!(kind == "response" && done["error"].as_str().unwrap().starts_with("not a layer to convert"), "{done}");
}

#[test]
fn a_request_it_cannot_answer_is_an_error_to_lambda_and_the_next_one_is_answered() {
    // AWS out of reach (its stand-in's address taken away): its settings cannot be read.
    let aws = aws();
    let (runtime, at) = Runtime::serve();
    let f = start(&at, "http://127.0.0.1:9", &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    let (kind, said) = runtime.invoke(json!({ "superci": "sweep" }));
    assert_eq!((kind.as_str(), said["errorType"].as_str()), ("error", Some("superci")), "{said}");
    assert!(said["errorMessage"].as_str().is_some_and(|m| m.contains("ssm.us-east-1.amazonaws.com")), "{said}");
    drop(f);
    // Back: the same program answers. (A runtime of its own: the one before may still have the last one's wait.)
    let (runtime, at) = Runtime::serve();
    let f = start(&at, &aws.url, &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    assert_eq!(runtime.invoke(json!({ "superci": "sweep" })), ("response".to_string(), json!({ "swept": true })));
    drop(f);
}

#[test]
fn a_queued_job_gets_its_machine_after_the_webhook_is_answered_as_aws_answered() {
    // The App in its settings; GitHub's stand-in beside AWS's; AWS answering as it did on Test (its first spot pool
    // full, then a fleet that gave a machine).
    let aws = aws();
    let github = aws.github.lock().unwrap().clone().unwrap();
    *github.aws.lock().unwrap() = Some(aws.clone());
    aws.put(&format!("/superci/{PLANE}/GITHUB_APP"), &GitHub::app().to_string());
    aws.replaying(include_str!("../../cli/fixtures/aws/launch/spot.jsonl"), &[]);
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, &aws.url, &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("RUNNER_REGION", "us-east-1"), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    let (id, (body, signature)) = github.queue("tests", "superci/cpu=2", now_ms());
    let began = Instant::now();
    let (kind, answer) = runtime.invoke(url_event("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": signature, "content-type": "application/json" }), &String::from_utf8(body).unwrap()));
    assert_eq!((kind.as_str(), answer["statusCode"].as_u64()), ("response", Some(202)), "taken: {answer}");
    // Answered first (GitHub waits ten seconds at most); the machine asked of AWS after, before the next request.
    let job = loop {
        if let Some(j) = item(&aws, &format!("job:{id}")).map(|v| serde_json::from_str::<Value>(&v).unwrap()).filter(|j| j["machine_id"].is_string() || j["state"] == "failed") { break j }
        assert!(began.elapsed() < Duration::from_secs(30), "{:?}", aws.unanswered.lock().unwrap());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!((job["cloud"].as_str(), job["machine_id"].as_str(), job["machine_type"].as_str(), job["zone"].as_str()), (Some("aws"), Some("i-00000000000000001"), Some("c8a.large"), Some("us-east-1d")), "{job}\n{:?}", aws.unanswered.lock().unwrap());
    assert!(github.unanswered.lock().unwrap().is_empty(), "{:?}", github.unanswered.lock().unwrap());
}

/// Its settings over several pages of SSM's, one of them not a setting (no value); its instance types from the deploy;
/// a request with its query, without one, and with a header that is not text; a move's step, read fresh.
#[test]
fn its_settings_are_read_page_after_page_and_its_requests_as_lambda_gives_them() {
    let aws = aws();
    for i in 0..12 { aws.put(&format!("/superci/{PLANE}/SETTING_{i:02}"), "x"); }
    aws.put(&format!("/superci/{PLANE}/MAX_CPU"), "16");
    aws.fail("AmazonSSM.GetParametersByPath", 200, &json!({ "Parameters": [{ "Name": format!("/superci/{PLANE}/NO_VALUE") }], "NextToken": "next-0" }).to_string(), Some(1));
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, &aws.url, &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("INSTANCE_TYPES", "c7i.large, ,m7i.large"), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    let mut event = url_event("GET", "/health", json!({ "x-number": 1 }), "");
    event["rawQueryString"] = json!("full=1");
    let (kind, answer) = runtime.invoke(event.clone());
    assert_eq!((kind.as_str(), answer["statusCode"].as_u64()), ("response", Some(200)), "{answer}");
    event.as_object_mut().unwrap().remove("rawQueryString");
    assert_eq!(runtime.invoke(event).1["statusCode"].as_u64(), Some(200));
    let reads = || aws.asked.lock().unwrap().iter().filter(|a| a["target"] == "AmazonSSM.GetParametersByPath").count();
    assert!(reads() >= 3, "every page: {}", reads());
    // (Fresh: at most every second.)
    std::thread::sleep(Duration::from_millis(1100));
    let before = reads();
    let (kind, _) = runtime.invoke(url_event("POST", "/move/export", json!({}), "{}"));
    assert_eq!(kind, "response");
    assert!(reads() > before, "a move's step reads them fresh");
}

/// Lambda's runtime API not there yet when the program starts: it asks again until it is, as the control plane and
/// as the converter.
#[test]
fn the_program_waits_for_lambdas_runtime_to_be_there() {
    let aws = aws();
    for env in [vec![("PLANE_ID", PLANE), ("TABLE", TABLE)], vec![("SUPERCI_CONVERTER", "1")]] {
        let port = superci_testkit::free_port();
        let _f = start(&format!("127.0.0.1:{port}"), &aws.url, &env);
        std::thread::sleep(Duration::from_millis(1500));
        let (runtime, _) = Runtime::serve_on(TcpListener::bind(("127.0.0.1", port)).unwrap());
        let (kind, _) = runtime.invoke(if env.len() == 1 { json!({ "something": "else" }) } else { json!({ "superci": "sweep" }) });
        assert_eq!(kind, "response");
    }
}

/// What was left for after the answer failing (DynamoDB refusing the job's record once its machine is asked for): said
/// in the function's log, and the next request answered.
#[test]
fn what_is_left_for_after_the_answer_may_fail_and_the_next_request_is_answered() {
    let aws = aws();
    let github = aws.github.lock().unwrap().clone().unwrap();
    *github.aws.lock().unwrap() = Some(aws.clone());
    aws.put(&format!("/superci/{PLANE}/GITHUB_APP"), &GitHub::app().to_string());
    aws.replaying(include_str!("../../cli/fixtures/aws/launch/spot.jsonl"), &[]);
    let refusing = aws.clone();
    aws.during("CreateFleet", move || refusing.fail("DynamoDB_20120810.PutItem", 400, &superci_testkit::json_error("ValidationException", "One or more parameter values were invalid"), None));
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, &aws.url, &[("PLANE_ID", PLANE), ("TABLE", TABLE), ("RUNNER_REGION", "us-east-1"), ("AWS_LAMBDA_FUNCTION_NAME", TABLE)]);
    let (_, (body, signature)) = github.queue("tests", "superci/cpu=2", now_ms());
    let (kind, answer) = runtime.invoke(url_event("POST", "/webhook", json!({ "x-github-event": "workflow_job", "x-hub-signature-256": signature, "content-type": "application/json" }), &String::from_utf8(body).unwrap()));
    assert_eq!((kind.as_str(), answer["statusCode"].as_u64()), ("response", Some(202)), "{answer}");
    let (kind, answer) = runtime.invoke(url_event("GET", "/health", json!({}), ""));
    assert_eq!((kind.as_str(), answer["statusCode"].as_u64()), ("response", Some(200)), "{answer}");
}

/// What the converter says when S3 does not take a step (the file, the blob's copy, what was made), when the blob is
/// not there, when mkfs.erofs makes nothing it can read, and when S3 is out of reach: each said in what was made.
#[test]
fn the_converter_says_which_step_s3_or_mkfs_did_not_take() {
    let dir = mkfs_dir();
    let bucket = "superci-plane-lambdaplane1-us-east-1";
    let job = |blob: &[u8], key: &str, bucket: &str| {
        let d = sha(blob);
        let hex = d[7..].to_string();
        json!({ "digest": d, "size": blob.len(), "region": "us-east-1", "bucket": bucket, "source": { "key": key },
            "erofs_key": format!("layers/{hex}.erofs"), "blob_key": format!("layers/{hex}.blob"), "done_key": format!("layers/done/{hex}.json") })
    };
    let converted = |aws: &Aws, blob: &[u8], fail: Option<(&str, &str, u16, &str)>| -> Value {
        let gh = aws.github.lock().unwrap().clone().unwrap();
        let key = format!("layers/staging/{}-x", &sha(blob)[7..]);
        gh.put_object(&format!("{bucket}/{key}"), blob);
        if let Some((method, part, status, body)) = fail { gh.answer_with(method, part, status, &[], body) }
        let (runtime, at) = Runtime::serve();
        let _f = start(&at, &aws.url, &[("SUPERCI_CONVERTER", "1"), ("LAMBDA_TASK_ROOT", dir.to_str().unwrap())]);
        runtime.invoke(job(blob, &key, bucket)).1
    };
    let error = |done: &Value| done["error"].as_str().unwrap_or_default().to_string();
    let s3_error = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
    let done = converted(&aws(), &gzip(b"one layer"), Some(("PUT", ".erofs", 403, s3_error)));
    assert!(error(&done).starts_with("putting the file: S3 answered 403"), "{done}");
    let done = converted(&aws(), &gzip(b"two layer"), Some(("PUT", ".blob", 200, s3_error)));
    assert_eq!(error(&done), "copying the blob: S3 failed", "{done}");
    let done = converted(&aws(), &gzip(b"red layer"), Some(("PUT", ".blob", 500, s3_error)));
    assert!(error(&done).starts_with("copying the blob: S3 answered 500"), "{done}");
    let done = converted(&aws(), &gzip(b"blue layer"), Some(("PUT", "layers/done/", 500, s3_error)));
    assert!(done["unsaid"].as_str().is_some_and(|u| u.starts_with("writing what was made")), "{done}");
    let done = converted(&aws(), &gzip(b"a nofile layer"), None);
    assert!(!error(&done).is_empty() && done["erofs"].is_null(), "{done}");
    let done = converted(&aws(), &gzip(b"a folder layer"), None);
    assert!(!error(&done).is_empty() && done["erofs"].is_null(), "{done}");
    // Not in staging any more (taken by a converter before), a bucket no address is made of, S3 out of reach.
    let aws = aws();
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, &aws.url, &[("SUPERCI_CONVERTER", "1"), ("LAMBDA_TASK_ROOT", dir.to_str().unwrap())]);
    let blob = gzip(b"gone");
    let done = runtime.invoke(job(&blob, &format!("layers/staging/{}-gone", &sha(&blob)[7..]), bucket)).1;
    assert!(error(&done).starts_with("fetching the blob: HTTP 404"), "{done}");
    let done = runtime.invoke(job(&blob, "layers/staging/x", "a bucket")).1;
    assert!(!error(&done).is_empty(), "{done}");
    // Digests that are not a sha256's (another kind; not hex); a size that is not the blob's.
    for digest in [format!("md5:{}", "a".repeat(67)), format!("sha256:{}", "z".repeat(64))] {
        let mut odd = job(&blob, "layers/staging/x", bucket);
        odd["digest"] = json!(digest);
        assert_eq!(error(&runtime.invoke(odd).1), "not a sha256 digest");
    }
    let gh = aws.github.lock().unwrap().clone().unwrap();
    let key = format!("layers/staging/{}-size", &sha(&blob)[7..]);
    gh.put_object(&format!("{bucket}/{key}"), &blob);
    let mut sized = job(&blob, &key, bucket);
    sized["size"] = json!(blob.len() + 5);
    assert!(error(&runtime.invoke(sized).1).contains("bytes), not"), "the size it was said to be");
    let (runtime, at) = Runtime::serve();
    let _f = start(&at, "http://127.0.0.1:9", &[("SUPERCI_CONVERTER", "1"), ("LAMBDA_TASK_ROOT", dir.to_str().unwrap())]);
    let done = runtime.invoke(job(&blob, "layers/staging/x", bucket)).1;
    assert!(error(&done).starts_with("fetching the blob") && done["unsaid"].is_string(), "{done}");
}
