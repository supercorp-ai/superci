//! The test kit's own stand-ins held to what they answer when asked what no test of SuperCI's asks of them: a request
//! that is none, an address or a body they do not know, what they were told to do failing. A stand-in that took these
//! for something else would let a wrong call of SuperCI's pass.
use super::*;
use crate::github::{OWNER, REPO};
use crate::worker::http;
use superci_core::io::Http;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Bytes on a connection of their own, and all that came back.
fn raw(url: &str, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
    s.write_all(bytes).unwrap();
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// What is no request, as anything serving a port may be sent: left alone, and the next one answered.
fn leaves_what_is_no_request_alone(url: &str) {
    assert_eq!(raw(url, b"\xff\xfe\r\n\r\n"), "", "a first line that is not text");
    assert_eq!(raw(url, b"GET /health HTTP/1.1\r\n\xff\xfe\r\n\r\n"), "", "a header that is not text");
    assert_eq!(raw(url, b"POST /health HTTP/1.1\r\ncontent-length: 100\r\n\r\nabc"), "", "a body cut short");
}

fn varint_of(mut n: u64) -> Vec<u8> { let mut out = vec![]; loop { let b = (n & 0x7f) as u8; n >>= 7; if n == 0 { out.push(b); return out } out.push(b | 0x80) } }
fn number(no: u64, v: u64) -> Vec<u8> { [varint_of(no << 3), varint_of(v)].concat() }
fn field(no: u64, bytes: &[u8]) -> Vec<u8> { [varint_of(no << 3 | 2), varint_of(bytes.len() as u64), bytes.to_vec()].concat() }
fn b64(bytes: &[u8]) -> String { base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes) }

const MODAL: &str = "https://api.modal.com/modal.client.ModalClient";

fn recording(method: &str, answer: Value) -> String { json!({ "method": "POST", "url": format!("{MODAL}/{method}"), "target": "", "asked": "", "status": 200, "answer": answer }).to_string() }

#[test]
fn a_port_given_is_another_each_time_and_below_what_the_system_gives_out() {
    let (a, b) = (free_port(), free_port());
    assert_ne!(a, b);
    assert!((20_000..32_000).contains(&a));
}

#[test]
fn waiting_comes_to_an_end_whichever_way_it_goes() {
    assert!(within(std::time::Duration::from_secs(5), || true));
    assert!(!within(std::time::Duration::ZERO, || false));
    let mut tries = 0;
    assert!(within(std::time::Duration::from_secs(5), || { tries += 1; tries == 2 }));
}

#[test]
fn the_store_refuses_and_is_written_meanwhile_as_it_is_told() {
    let served = serve(|_| {});
    let store = &served.store;
    block_on(store.put("a", "1".into())).unwrap();
    served.store_refuses(Some("a"));
    assert!(block_on(store.put("a", "2".into())).is_err());
    assert!(block_on(store.put_if_absent("a2", "2".into())).is_err());
    assert!(block_on(store.swap("a", Some("1"), "2".into())).is_err());
    served.store_refuses(None);
    assert!(!block_on(store.put_if_absent("a", "2".into())).unwrap());
    // Changed meanwhile, then taken out meanwhile: the write finds it so.
    served.written_meanwhile("a", 1, |_| Some("changed".to_string()));
    assert!(!block_on(store.swap("a", Some("1"), "2".into())).unwrap());
    assert_eq!(block_on(store.get("a")).unwrap().as_deref(), Some("changed"));
    served.written_meanwhile("a", 1, |_| None);
    assert!(!block_on(store.swap("a", Some("changed"), "2".into())).unwrap());
    assert_eq!(block_on(store.get("a")).unwrap(), None);
    assert!(block_on(store.swap("a", None, "2".into())).unwrap());
}

#[test]
fn what_a_served_control_plane_is_sent_that_is_no_request_is_left_alone() {
    let served = serve(|_| {});
    leaves_what_is_no_request_alone(&served.url);
    assert!(raw(&served.url, b"GET /health HTTP/1.1\r\nno colon\r\n\r\n").starts_with("HTTP/1.1 "));
    leaves_what_is_no_request_alone(&served.aws.url);
    for served in [serve_with_ends(|_| {}), serve_deferring(|_| {})] {
        leaves_what_is_no_request_alone(&served.url);
        assert!(raw(&served.url, b"GET /health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
        // Stopped answering: its connections closed at once. Its machines off, and said to be at another address.
        served.down.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(raw(&served.url, b"GET /health HTTP/1.1\r\n\r\n"), "");
        served.down.store(false, std::sync::atomic::Ordering::SeqCst);
        served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
        *served.public.lock().unwrap() = Some("https://plane.example".into());
        assert!(raw(&served.url, b"GET /health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
        served.aws.params.lock().unwrap().remove(&format!("/superci/{PLANE}/CONTAINERS"));
        assert!(raw(&served.url, b"GET /health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    }
}

#[test]
fn a_control_plane_that_panics_is_kept_and_answers_the_next_request() {
    for served in [serve(|_| {}), serve_with_ends(|_| {})] {
        served.connect_github();
        served.github.during("POST", "", || panic!("in a call to GitHub"));
        let (_, webhook) = served.github.queue("build", "superci", served.now_ms());
        served.webhook(webhook);
        // (Kept once it has unwound, and its connection, which the webhook's answer ended with, closed before that.)
        assert!(within(std::time::Duration::from_secs(5), || served.faults().panics().iter().any(|p| p.contains("in a call to GitHub"))), "{:?}", served.faults().panics());
        assert!(raw(&served.url, b"GET /health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    }
}

#[test]
fn its_upkeep_is_held_to_answering() {
    let served = serve(|_| {});
    served.upkeep();
    served.down.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(catch_unwind(AssertUnwindSafe(|| served.upkeep())).is_err());
    let mut gone = serve(|_| {});
    gone.url = "http://127.0.0.1:1".into();
    assert_eq!(gone.try_upkeep(), (0, String::new()));
}

#[test]
fn ending_lets_go_of_github_whether_it_is_held_or_not() {
    let served = serve(|_| {});
    served.aws.github.lock().unwrap().take();
    drop(served);
    drop(serve(|_| {}));
}

#[test]
fn a_control_plane_that_leaves_work_for_later_does_it_and_says_how_its_upkeep_went() {
    let served = serve_deferring(|_| {});
    assert_eq!(served.try_upkeep().0, 200);
    assert!(served.run_later().iter().all(|done| done["error"].is_null()));
    served.arm(Fault::StoreDown(1));
    assert_eq!(served.try_upkeep().0, 500);
    served.faults().calm();
    // (What one thing left is given the address of what left it.)
    served.connect_github();
    served.github.queue("build", "superci", served.now_ms());
    let (_, webhook) = served.github.queue("build", "superci", served.now_ms());
    served.webhook(webhook);
    let done = served.run_later();
    assert!(!done.is_empty());
}

#[test]
fn a_modal_settings_dict_is_read_as_far_as_it_makes_sense() {
    let pickled = |text: &[u8], short: bool| -> Vec<u8> {
        let head = [0x80, 4, 0x95, 0, 0, 0, 0, 0, 0, 0, 0];
        let len = if short { vec![0x8c, text.len() as u8] } else { [vec![b'X'], (text.len() as u32).to_le_bytes().to_vec()].concat() };
        [head.to_vec(), len, text.to_vec()].concat()
    };
    assert_eq!(modal_entries(&[]), []);
    assert_eq!(modal_entries(&[0x80]), [], "a key cut short");
    assert_eq!(modal_entries(&[0x08]), [], "a number cut short");
    assert_eq!(modal_entries(&[0x12]), [], "a length cut short");
    assert_eq!(modal_entries(&[0x0d, 1, 2, 3, 4]), [], "a kind it does not read");
    let entry = |k: &[u8], v: &[u8]| field(2, &[number(5, 1), field(1, k), field(2, v)].concat());
    assert_eq!(modal_entries(&[number(1, 1), entry(&pickled(b"K", true), &pickled(b"V", false)), field(3, b"other")].concat()), [("K".to_string(), "V".to_string())]);
    // Another pickle than Modal's, none at all, and no field for it: nothing read.
    assert_eq!(modal_entries(&[entry(&[0; 12], b""), field(2, &[])].concat()), [(String::new(), String::new()), (String::new(), String::new())]);
    assert_eq!(modal_entries(&entry(&[pickled(b"", true)[..11].to_vec(), vec![0x8c]].concat(), &[pickled(b"", true)[..11].to_vec(), vec![b'X', 1]].concat())), [(String::new(), String::new())]);
}

#[test]
fn a_protobuf_message_is_read_as_far_as_it_goes() {
    assert_eq!(proto_fields(&[]), []);
    assert_eq!(proto_fields(&[0x80]), []);
    assert_eq!(proto_fields(&number(3, 300)), [(3, 300, vec![])]);
    assert_eq!(proto_fields(&[0x08]), [(1, 0, vec![])]);
    assert_eq!(proto_fields(&[0x0d, 1, 2, 3, 4]), [(1, 0, vec![1, 2, 3, 4])]);
    assert_eq!(proto_fields(&[0x0d, 1, 2, 3]), [], "a fixed32 cut short");
    assert_eq!(proto_fields(&field(2, b"ab")), [(2, 0, b"ab".to_vec())]);
    assert_eq!(proto_fields(&[0x12]), [(2, 0, vec![])]);
    assert_eq!(proto_fields(&[0x12, 9, b'a']), [(2, 0, b"a".to_vec())]);
    assert_eq!(proto_fields(&[0x1b]), [], "a kind it does not read");
}

#[test]
fn modal_refuses_a_go_client_it_cannot_tell_the_version_of_when_it_creates_an_app() {
    let headers = |pairs: &[(&'static str, &'static str)]| { let mut h = http::HeaderMap::new(); for (k, v) in pairs { h.insert(*k, v.parse().unwrap()); } h };
    let creates = number(3, 1);
    let go = ("x-modal-client-type", "9");
    let refused = |h: &[(&'static str, &'static str)], method: &str, message: &[u8]| modal_refuses(method, &headers(h), message).is_some();
    assert!(refused(&[go, ("x-modal-libmodal-version", "0.11.1")], "AppGetOrCreate", &creates));
    assert!(refused(&[go, ("x-modal-libmodal-version", "modal-go/x")], "AppGetOrCreate", &creates));
    assert!(!refused(&[go, ("x-modal-libmodal-version", "modal-go/0.11.1")], "AppGetOrCreate", &creates));
    assert!(!refused(&[go, ("x-modal-libmodal-version", "modal-go/v0.7.3")], "AppGetOrCreate", &creates));
    assert!(!refused(&[go], "AppGetOrCreate", &creates), "it says none");
    assert!(!refused(&[("x-modal-client-type", "1"), ("x-modal-libmodal-version", "0.11.1")], "AppGetOrCreate", &creates));
    assert!(!refused(&[go, ("x-modal-libmodal-version", "0.11.1")], "AppGetOrCreate", &number(3, 0)), "it does not create one");
    assert!(!refused(&[go, ("x-modal-libmodal-version", "0.11.1")], "AppGetOrCreate", &number(2, 1)));
    assert!(!refused(&[go, ("x-modal-libmodal-version", "0.11.1")], "AppGet", &creates));
}

#[test]
fn modal_refuses_a_build_not_told_its_image_builder() {
    let told = |key: &[u8]| field(40, &field(1, &field(1, key)));
    let runs = field(1, b"modal deploy app.py");
    let create = |definition: &[u8]| field(2, definition);
    assert!(modal_builder_refuses("SandboxCreate", &create(&runs)));
    assert!(modal_builder_refuses("SandboxCreate", &create(&[runs.clone(), told(b"ANOTHER")].concat())));
    assert!(!modal_builder_refuses("SandboxCreate", &create(&[runs.clone(), told(b"MODAL_IMAGE_BUILDER_VERSION")].concat())));
    assert!(!modal_builder_refuses("SandboxCreate", &create(&field(1, b"sleep 1"))));
    assert!(!modal_builder_refuses("SandboxCreate", &[]));
    let stdin = |script: &str| field(2, format!("{}\nthe rest", b64(script.as_bytes())).as_bytes());
    let import = "\nimport modal\n";
    let tell = "os.environ[\"MODAL_IMAGE_BUILDER_VERSION\"] = \"2026.10\"\n";
    assert!(modal_builder_refuses("SandboxStdinWrite", &stdin(import)));
    assert!(modal_builder_refuses("SandboxStdinWrite", &stdin(&format!("{import}{tell}"))));
    assert!(!modal_builder_refuses("SandboxStdinWrite", &stdin(&format!("{tell}{import}"))));
    assert!(!modal_builder_refuses("SandboxStdinWrite", &stdin(tell)));
    assert!(!modal_builder_refuses("SandboxStdinWrite", &field(2, b"not base64!")));
    assert!(!modal_builder_refuses("SandboxStdinWrite", &[]));
    assert!(!modal_builder_refuses("AppGet", &[]));
}

#[test]
fn modal_answers_as_its_recordings_say_and_refuses_what_it_refuses() {
    let aws = Aws::serve();
    let none = http::HeaderMap::new();
    let refused = aws.modal_answer("SandboxCreate", &none, &field(2, &field(1, b"modal deploy")));
    assert_eq!(refused.1.unwrap().1, MODAL_BUILDER_REFUSED);
    assert!(aws.modal_answer("NoRecording", &none, &[]).1.unwrap().1.contains("has no answer"));
    assert!(aws.unanswered.lock().unwrap().iter().any(|u| u.ends_with("/NoRecording")));
    assert_eq!(aws.modal_answer("SandboxStdinWrite", &none, &[]).0, [Vec::<u8>::new()]);
    aws.replaying(&[recording("Odd", json!(json!({ "answers": [1, "AAAA", b64(b"ab")] }).to_string())),
        recording("Plain", json!(json!({ "error": "Modal Plain: it broke" }).to_string())), recording("Bare", json!(json!({ "error": "it broke" }).to_string())),
        recording("Old", json!(json!({ "error": "(Modal Old: refused)" }).to_string())),
        recording("Signed", json!(json!({ "error": "the sign-in (Modal Signed: no longer accepted)" }).to_string()))].join("\n"), &[]);
    assert_eq!(aws.modal_answer("Odd", &none, &[]).0, [vec![0, 0, 0], b"ab".to_vec()]);
    assert_eq!(aws.modal_answer("Plain", &none, &[]).1, Some((2, "it broke".to_string())));
    assert_eq!(aws.modal_answer("Bare", &none, &[]).1, Some((2, "it broke".to_string())));
    assert_eq!(aws.modal_answer("Old", &none, &[]).1, Some((2, "(Modal Old: refused)".to_string())));
    assert_eq!(aws.modal_answer("Signed", &none, &[]).1, Some((16, "no longer accepted".to_string())));
    aws.faults.arm(Fault::Call(1));
    assert_eq!(aws.modal_answer("Plain", &none, &[]).1, Some((13, "internal error".to_string())));
    aws.faults.calm();
    // What a settings Dict is given, kept.
    aws.replaying(&recording("DictUpdate", json!(json!({ "answers": [] }).to_string())), &[]);
    aws.modal_answer("DictUpdate", &none, &[]);
    assert!(aws.modal_settings.lock().unwrap().is_empty());
}

#[test]
fn a_modal_sign_in_is_given_its_token_only_while_a_call_waits() {
    let aws = Aws::serve();
    let wait = "TokenFlowWait";
    aws.replaying(&[recording(wait, json!(json!({}).to_string())), recording(wait, json!("not json")), json!({ "method": "POST", "url": format!("{MODAL}/{wait}"), "target": "", "asked": "", "status": 200 }).to_string(),
        recording(wait, json!(json!({ "answers": ["not base64!"] }).to_string())),
        recording(wait, json!(json!({ "answers": [b64(&number(3, 1))] }).to_string())), recording(wait, json!(json!({ "answers": [b64(&[number(3, 0), number(4, 1), field(1, b"token")].concat())] }).to_string()))].join("\n"), &[]);
    // Its time up, however it was said (or not): told to wait on. (The first of field 1 is a number, not its time.)
    let none = http::HeaderMap::new();
    let timed_out = aws.modal_answer(wait, &none, &[number(1, 1), [vec![0x15], 0f32.to_le_bytes().to_vec()].concat()].concat());
    assert_eq!(timed_out.0, [vec![0x18, 0x01]]);
    let waiter = { let aws = aws.clone(); std::thread::spawn(move || aws.modal_answer("TokenFlowWait", &http::HeaderMap::new(), &[vec![0x0d], 20f32.to_le_bytes().to_vec()].concat()).0) };
    aws.modal_approve();
    assert!(String::from_utf8_lossy(&waiter.join().unwrap()[0]).contains("token"));
    // Approved with none waiting (after five seconds): the next is told to wait on.
    aws.modal_approve();
    assert_eq!(aws.modal_wait(0.0), [0x18, 0x01]);
}

/// A gRPC call to Modal's stand-in with this message: its status and message, wherever they came, and each answer.
fn grpc(aws: &Aws, method: &str, message: &[u8]) -> (String, String, Vec<u8>) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let tcp = tokio::net::TcpStream::connect(aws.url.trim_start_matches("http://")).await.unwrap();
        let (client, connection) = h2::client::handshake(tcp).await.unwrap();
        tokio::spawn(connection);
        let mut client = client.ready().await.unwrap();
        let request = http::Request::builder().method("POST").uri(format!("http://api.modal.com/modal.client.ModalClient/{method}")).body(()).unwrap();
        let (response, mut send) = client.send_request(request, message.is_empty()).unwrap();
        if !message.is_empty() { send.send_data(bytes::Bytes::copy_from_slice(&[&[0, 0, 0, 0, message.len() as u8], message].concat()), true).unwrap(); }
        let response = response.await.unwrap();
        let said = |h: &http::HeaderMap, n: &str| h.get(n).map(|v| v.to_str().unwrap().to_string());
        let (status, text) = (said(response.headers(), "grpc-status"), said(response.headers(), "grpc-message"));
        let mut body = response.into_body();
        let mut answers = vec![];
        while let Some(chunk) = body.data().await { let chunk = chunk.unwrap(); let _ = body.flow_control().release_capacity(chunk.len()); answers.extend_from_slice(&chunk); }
        let trailers = body.trailers().await.unwrap().unwrap_or_default();
        (status.or(said(&trailers, "grpc-status")).unwrap_or_default(), text.or(said(&trailers, "grpc-message")).unwrap_or_default(), answers)
    })
}

#[test]
fn modal_is_reached_over_grpc_as_a_client_reaches_it() {
    let aws = Aws::serve();
    // (HTTP/2's preface begun and left: nothing said, and the next one served.)
    raw(&aws.url, b"PRI nonsense");
    aws.replaying(&[recording("Answers", json!(json!({ "answers": [b64(b"one")] }).to_string())), recording("Refuses", json!(json!({ "error": "Modal Refuses: 100% caf\u{e9}" }).to_string())),
        recording("Broke", json!(json!({ "answers": [b64(b"before")], "error": "Modal Broke: after" }).to_string()))].join("\n"), &[]);
    let (status, _, answers) = grpc(&aws, "Answers", b"");
    assert_eq!(status, "0");
    assert!(answers.ends_with(b"one"));
    assert!(grpc(&aws, "Answers", b"a message").2.ends_with(b"one"));
    let (status, text, answers) = grpc(&aws, "Refuses", b"");
    assert_eq!((status.as_str(), text.as_str(), answers.is_empty()), ("2", "100%25 caf%C3%A9", true));
    let (status, text, answers) = grpc(&aws, "Broke", b"");
    assert_eq!((status.as_str(), text.as_str()), ("2", "after"));
    assert!(answers.ends_with(b"before"));
    *aws.modal_warning.lock().unwrap() = Some("soon".into());
    assert_eq!(grpc(&aws, "Refuses", b"").0, "2");
    assert_eq!(grpc(&aws, "Answers", b"").0, "0");
}

#[test]
fn cloudflares_stand_in_keeps_a_workers_secrets_as_they_are_written() {
    let aws = Aws::serve();
    let at = "/api.cloudflare.com/client/v4/accounts/a/workers/scripts/s";
    let put = |body: &str| http(&aws.url, "PUT", &format!("{at}/secrets"), &[], body.as_bytes()).0;
    for body in ["not json", "{}", "{\"name\":\"NO_TEXT\"}", "{\"text\":\"no name\"}"] { put(body); }
    assert!(aws.worker_secrets.lock().unwrap().is_empty());
    put("{\"name\":\"K\",\"text\":\"v\"}");
    assert_eq!(aws.worker_secrets.lock().unwrap()["s"]["K"], "v");
    http(&aws.url, "DELETE", &format!("{at}/secrets/K"), &[], b"");
    http(&aws.url, "DELETE", &format!("{at}x/secrets/K"), &[], b"");
    assert!(aws.worker_secrets.lock().unwrap()["s"].is_empty());
    http(&aws.url, "GET", &format!("{at}/other"), &[], b"");
    http(&aws.url, "DELETE", at, &[], b"");
    assert!(aws.worker_secrets.lock().unwrap().is_empty());
}

#[test]
fn a_worker_on_workers_dev_that_answers_nothing_is_answered_nothing() {
    let aws = Aws::serve();
    aws.workers.lock().unwrap().insert("dead.acme.workers.dev".into(), "http://127.0.0.1:1".into());
    assert_eq!(http(&aws.url, "GET", "/dead.acme.workers.dev/x", &[], b"").0, 0);
}

#[test]
fn sts_answers_a_control_plane_assuming_its_role_however_it_asks() {
    let aws = Aws::serve();
    let assuming = |path: &str, body: &[u8]| http(&aws.url, "POST", path, &[], body).1.contains("AssumeRoleWithWebIdentityResponse");
    assert!(assuming("/sts.us-east-1.amazonaws.com/?Action=AssumeRoleWithWebIdentity", b""));
    assert!(assuming("/sts.us-east-1.amazonaws.com/", b"Action=AssumeRoleWithWebIdentity"));
    assert!(!assuming("/sts.us-east-1.amazonaws.com/", b"Action=GetCallerIdentity"));
    assert!(!assuming("/ec2.us-east-1.amazonaws.com/", b"Action=AssumeRoleWithWebIdentity"));
}

#[test]
fn a_parameter_is_kept_as_ssm_keeps_it() {
    let aws = Aws::serve();
    let ask = |target: &str, a: Value| { let (s, v) = aws.handle("POST", "https://ssm.us-east-1.amazonaws.com/", target, &a.to_string()); (s, serde_json::from_str::<Value>(&v).unwrap()) };
    assert_eq!(ask("AmazonSSM.PutParameter", json!({ "Name": "/a/b", "Value": "1" })).0, 200);
    let (status, said) = ask("AmazonSSM.PutParameter", json!({ "Name": "/a/b", "Value": "2" }));
    assert_eq!((status, said["__type"].as_str()), (400, Some("ParameterAlreadyExists")));
    assert_eq!(ask("AmazonSSM.PutParameter", json!({ "Name": "/a/b", "Value": "2", "Overwrite": true })).1["Version"], 2);
    let (_, got) = ask("AmazonSSM.GetParameters", json!({ "Names": ["/a/b", "/a/none"] }));
    assert_eq!((got["Parameters"].as_array().unwrap().len(), got["InvalidParameters"][0].as_str()), (1, Some("/a/none")));
    ask("AmazonSSM.PutParameter", json!({ "Name": "/a/c/d", "Value": "3" }));
    let names = |a: Value| ask("AmazonSSM.GetParametersByPath", a).1["Parameters"].as_array().unwrap().len();
    assert_eq!(names(json!({ "Path": "/a/" })), 1);
    assert_eq!(names(json!({ "Path": "/a/", "Recursive": true })), 2);
    let (_, gone) = ask("AmazonSSM.DeleteParameters", json!({ "Names": ["/a/b", "/a/none"] }));
    assert_eq!((gone["DeletedParameters"][0].as_str(), gone["InvalidParameters"][0].as_str()), (Some("/a/b"), Some("/a/none")));
    // Written as the dashboard writes one: what is not a control plane's routing goes to SSM; a control plane's, there
    // too until it keeps its settings in its record.
    aws.put("/other/ROUTING", "x");
    aws.put("/superci/p/OTHER", "x");
    aws.put("/superci/p/ROUTING", "[]");
    assert_eq!(aws.params.lock().unwrap().len(), 4);
    *aws.plane_store.lock().unwrap() = Some(Default::default());
    aws.put("/superci/p/LAUNCHERS", "[]");
    assert_eq!(aws.params.lock().unwrap().len(), 5);
    aws.plane_store.lock().unwrap().as_ref().unwrap().lock().unwrap().insert(superci_core::plane::SETTINGS.into(), "not json".into());
    aws.put("/superci/p/LAUNCHERS", "[1]");
    assert_eq!(aws.settings("p").iter().find(|(k, _)| k == "LAUNCHERS").unwrap().1, "[1]");
    aws.plane_store.lock().unwrap().as_ref().unwrap().lock().unwrap().insert(superci_core::plane::SETTINGS.into(), json!({ "version": 1, "routing": [2], "launchers": [] }).to_string());
    aws.put("/superci/p/LAUNCHERS", "[3]");
    assert_eq!(aws.settings("p").iter().find(|(k, _)| k == "LAUNCHERS").unwrap().1, "[3]");
}

#[test]
fn only_a_control_planes_code_uploaded_ends_its_old_version() {
    let aws = Aws::serve();
    let code = "https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-x/code";
    let old = || aws.old_version.lock().unwrap().is_some();
    *aws.old_version.lock().unwrap() = Some("0.19.0".into());
    for (method, url) in [("GET", code), ("PUT", "https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/other/code"), ("PUT", "https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-x/configuration"), ("PUT", code)] {
        aws.handle(method, url, "", "");
        assert!(old(), "{method} {url}");
    }
    aws.replaying(&json!({ "method": "PUT", "url": code, "target": "", "asked": "", "status": 200, "answer": "{}" }).to_string(), &[]);
    aws.handle("PUT", code, "", "");
    assert!(!old());
}

#[test]
fn a_recording_without_what_an_answer_has_is_no_answer() {
    let aws = Aws::serve();
    let url = "https://x.us-east-1.amazonaws.com/";
    let line = |target: &str, rest: Value| { let mut l = json!({ "method": "POST", "url": url, "target": target, "asked": "", "status": 200, "answer": "the answer" }); for (k, v) in rest.as_object().unwrap() { l[k] = v.clone() } l.to_string() };
    aws.replaying(&[line("No.Answer", json!({ "answer": null })), line("No.Status", json!({ "status": null })), line("Odd.Headers", json!({ "headers": [[1, "a"], ["b", 2], ["c", "d"]] }))].join("\n"), &[]);
    assert!(aws.handle("POST", url, "No.Answer", "").1.contains("SuperCITestkitUnknown"));
    assert!(aws.handle("POST", url, "No.Status", "").1.contains("SuperCITestkitUnknown"));
    assert_eq!(aws.handle_full("POST", url, "Odd.Headers", "").1, [("c".to_string(), "d".to_string())]);
}

#[test]
fn dynamodb_answers_its_item_calls_as_it_does() {
    let aws = Aws::serve();
    let ask = |op: &str, a: Value| { let (s, v) = aws.handle("POST", "https://dynamodb.us-east-1.amazonaws.com/", &format!("DynamoDB_20120810.{op}"), &a.to_string()); (s, serde_json::from_str::<Value>(&v).unwrap()) };
    let item = |p: &str, k: &str, v: u64| json!({ "p": { "S": p }, "k": { "S": k }, "v": { "N": v.to_string() } });
    assert_eq!(ask("PutItem", json!({ "TableName": "t", "Item": item("p", "a", 1), "ConditionExpression": "attribute_not_exists(k)" })).0, 200);
    assert_eq!(ask("PutItem", json!({ "TableName": "t", "Item": item("p", "a", 1), "ConditionExpression": "attribute_not_exists(k)" })).0, 400);
    assert_eq!(ask("PutItem", json!({ "TableName": "t", "Item": item("p", "b", 1), "ConditionExpression": "v = :was", "ExpressionAttributeValues": { ":was": { "N": "1" } } })).0, 400);
    assert_eq!(ask("PutItem", json!({ "TableName": "t", "Item": item("p", "a", 2), "ConditionExpression": "v = :was", "ExpressionAttributeValues": { ":was": { "N": "9" } } })).0, 400);
    assert_eq!(ask("PutItem", json!({ "TableName": "t", "Item": item("p", "a", 2), "ConditionExpression": "v = :was", "ExpressionAttributeValues": { ":was": { "N": "1" } } })).0, 200);
    for k in ["b", "c"] { ask("PutItem", json!({ "TableName": "t", "Item": item("p", k, 1) })); }
    ask("PutItem", json!({ "TableName": "t", "Item": item("other", "z", 1) }));
    ask("PutItem", json!({ "TableName": "t2", "Item": item("p", "z", 1) }));
    assert_eq!(ask("GetItem", json!({ "TableName": "t", "Key": { "p": { "S": "p" }, "k": { "S": "a" } } })).1["Item"]["v"]["N"], "2");
    assert_eq!(ask("GetItem", json!({ "TableName": "t", "Key": { "p": { "S": "p" }, "k": { "S": "none" } } })).1, json!({}));
    let query = |extra: Value| { let mut a = json!({ "TableName": "t", "ExpressionAttributeValues": { ":p": { "S": "p" } } }); for (k, v) in extra.as_object().unwrap() { a[k] = v.clone() } ask("Query", a).1 };
    let keys = |q: &Value| q["Items"].as_array().unwrap().iter().map(|i| i["k"]["S"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(keys(&query(json!({}))), ["a", "b", "c"]);
    assert_eq!(keys(&query(json!({ "ScanIndexForward": false }))), ["c", "b", "a"]);
    let first = query(json!({ "Limit": 2 }));
    assert_eq!(keys(&first), ["a", "b"]);
    assert_eq!(first["LastEvaluatedKey"]["k"]["S"], "b");
    assert_eq!(keys(&query(json!({ "ExclusiveStartKey": first["LastEvaluatedKey"] }))), ["c"]);
    assert_eq!(keys(&query(json!({ "ExclusiveStartKey": { "k": { "S": "not there" } } }))), ["a", "b", "c"]);
    let got = ask("BatchGetItem", json!({ "RequestItems": { "t": { "Keys": [{ "p": { "S": "p" }, "k": { "S": "a" } }, { "p": { "S": "p" }, "k": { "S": "none" } }] }, "t2": {} } })).1;
    assert_eq!((got["Responses"]["t"].as_array().unwrap().len(), got["Responses"]["t2"].as_array().unwrap().len()), (1, 0));
    assert_eq!(ask("BatchGetItem", json!({})).1["Responses"], json!({}));
    assert_eq!(ask("DeleteItem", json!({ "TableName": "t", "Key": { "p": { "S": "p" }, "k": { "S": "a" } } })).0, 200);
    assert_eq!(ask("CreateTable", json!({})).0, 400);
}

#[test]
fn a_runner_is_found_only_where_its_run_has_one() {
    let served = serve(|_| {});
    assert!(Runner::of_run(&served, "1").is_none());
    assert!(Runner::of_job(&served, "1", "build").is_none());
    block_on(served.store.put("run:2", "{}".into())).unwrap();
    assert!(Runner::of_run(&served, "2").is_none());
    assert!(Runner::of_job(&served, "2", "build").is_none());
    block_on(served.store.put("run:3", json!({ "jobs": [{ "name": "build" }, { "name": "test", "token": "t" }] }).to_string())).unwrap();
    assert!(Runner::of_run(&served, "3").is_some());
    assert!(Runner::of_job(&served, "3", "build").is_none());
    assert!(Runner::of_job(&served, "3", "test").is_some());
}

#[test]
fn a_field_is_left_out_where_it_is_and_nothing_else_happens() {
    let mut v = json!({ "a": { "b": 1, "c": 2 }, "l": [{ "x": 1 }, { "x": 2 }], "o": { "p": { "x": 1 } }, "n": 1 });
    leave_out(&mut v, &[]);
    leave_out(&mut v, &["a", "b"]);
    leave_out(&mut v, &["l", "*", "x"]);
    leave_out(&mut v, &["o", "*", "x"]);
    leave_out(&mut v, &["n", "*", "x"]);
    leave_out(&mut v, &["none", "x"]);
    leave_out(&mut v, &["n", "x"]);
    assert_eq!(v, json!({ "a": { "c": 2 }, "l": [{}, {}], "o": { "p": {} }, "n": 1 }));
}

#[test]
fn what_stands_in_front_of_a_control_plane_leaves_what_is_no_request_alone_and_says_when_nothing_answers() {
    let served = serve(|_| {});
    let nothing_there = "http://127.0.0.1:1";
    let front_of = |to: &str| front(to, |_, _, status, body| (status, body));
    let cut_short = |url: &str| { assert_eq!(raw(url, b"\xff\xfe\r\n\r\n"), "") };
    for url in [front_of(&served.url), without_routes(&served.url, &["/gone"]), busy_for(&served.url, "/busy", 0)] {
        cut_short(&url);
        assert_eq!(http(&url, "GET", "/health", &[("x-other", "1")], b"").0, 200);
    }
    assert_eq!(http(&front_of(nothing_there), "GET", "/health", &[], b"").0, 502);
    assert_eq!(http(&without_routes(nothing_there, &[]), "GET", "/health", &[], b"").0, 0);
    assert_eq!(http(&busy_for(nothing_there, "/busy", 0), "GET", "/health", &[], b"").0, 0);
    assert_eq!(http(&without_routes(&served.url, &["/gone"]), "GET", "/gone", &[], b"").0, 404);
    let busy = busy_for(&served.url, "/health", 1);
    assert_eq!(http(&busy, "GET", "/health", &[], b"").0, 503);
    assert_eq!(http(&busy, "GET", "/health", &[], b"").0, 200);
}

/// A call to GitHub's stand-in (or S3's, or a launcher's): its status and body.
fn ask(g: &GitHub, method: &str, url: &str, body: &[u8]) -> (u16, String) {
    let res = block_on(g.send(Request::new(method, url).with_body(body.to_vec()))).unwrap();
    (res.status, String::from_utf8_lossy(&res.body).into_owned())
}

#[test]
fn github_answers_only_for_what_it_has() {
    let g = GitHub::default();
    let api = "https://api.github.com";
    let (id, _) = g.queue("build", "superci", 0);
    let run = g.job(id)["run_id"].as_u64().unwrap();
    // Nothing of another repository's, and nothing by what is no number.
    for (method, path) in [("GET", "repos/other/app".to_string()), ("GET", format!("repos/other/app/actions/jobs/{id}")), ("POST", format!("repos/other/app/actions/runs/{run}/cancel")), ("GET", format!("repos/other/app/actions/runs/{run}/logs")),
        ("GET", format!("repos/other/app/actions/jobs/{id}/logs")), ("GET", format!("repos/other/app/actions/runs/{run}")), ("GET", format!("repos/other/app/actions/runs/{run}/jobs")), ("GET", "repos/other/app/actions/runs".to_string()),
        ("POST", format!("repos/other/app/actions/jobs/{id}/rerun")), ("POST", format!("repos/other/app/actions/runs/{run}/rerun-failed-jobs")),
        ("GET", format!("repos/{REPO}/actions/jobs/x")), ("POST", format!("repos/{REPO}/actions/runs/x/cancel")), ("GET", format!("repos/{REPO}/actions/runs/x/logs")), ("GET", format!("repos/{REPO}/actions/jobs/x/logs")),
        ("GET", format!("repos/{REPO}/actions/runs/x")), ("GET", format!("repos/{REPO}/actions/runs/x/jobs")), ("GET", format!("repos/{REPO}/actions/runs/1")),
        ("POST", "app/installations/1/access_tokens".to_string()), ("DELETE", "app/installations/1".to_string()), ("POST", "app/hook/deliveries/x/attempts".to_string()),
        ("GET", format!("orgs/{OWNER}/actions/runners/x")), ("GET", format!("orgs/{OWNER}/actions/runners/1")), ("DELETE", format!("orgs/{OWNER}/actions/runners/x"))] {
        assert_eq!(ask(&g, method, &format!("{api}/{path}"), b"").0, 404, "{method} {path}");
    }
    assert_eq!(ask(&g, "POST", &format!("{api}/repos/{REPO}/actions/runs/1/cancel"), b"").0, 404);
    assert_eq!(ask(&g, "POST", &format!("{api}/repos/{REPO}/actions/runs/{run}/cancel"), b"").0, 202);
    // A runner asked for with what it needs missing.
    let jit = format!("{api}/orgs/{OWNER}/actions/runners/generate-jitconfig");
    for body in [&b"not json"[..], b"{}", b"{\"name\":\"r\"}"] { assert_eq!(ask(&g, "POST", &jit, body).0, 404, "{}", String::from_utf8_lossy(body)); }
    // One made, with its job on it and not: busy only while its job runs.
    let made: Value = serde_json::from_str(&ask(&g, "POST", &jit, b"{\"name\":\"r2\",\"labels\":[\"superci\"]}").1).unwrap();
    let rid = made["runner"]["id"].as_u64().unwrap();
    let runner = |g: &GitHub| serde_json::from_str::<Value>(&ask(g, "GET", &format!("{api}/orgs/{OWNER}/actions/runners/{rid}"), b"").1).unwrap();
    assert_eq!(runner(&g)["busy"], false);
    g.start_elsewhere(id, "another");
    assert_eq!(runner(&g)["busy"], false);
    g.start(id);
    assert_eq!(runner(&g)["busy"], true);
    g.finish(id, "success", "the log", 0);
    assert_eq!(runner(&g)["busy"], false);
    // Told of a job it does not have: nothing.
    assert_eq!(g.cancel(1, 0).0.is_empty(), false);
    g.ended_untold(1);
    assert_eq!(g.job(1), Value::Null);
    // Its installation, as selected.
    g.set_installed(true, false);
    assert!(ask(&g, "GET", &format!("{api}/app/installations"), b"").1.contains("\"selected\""));
    assert!(ask(&g, "GET", &format!("{api}/installation/repositories"), b"").1.contains("\"selected\""));
    // Not GitHub's, nor anything's it knows.
    assert_eq!(ask(&g, "GET", "https://example.com/x", b"").0, 404);
    assert_eq!(ask(&g, "GET", "not an address", b"").0, 404);
    // Its logs' blob store.
    let logs = "https://productionresultssa0.blob.core.windows.net/actions-results/logs";
    assert_eq!(ask(&g, "GET", &format!("{logs}/x"), b"").0, 404);
    assert_eq!(ask(&g, "GET", &format!("{logs}/run-x"), b"").0, 404);
    assert_eq!(ask(&g, "GET", &format!("{logs}/{id}"), b"").1, "the log");
    // Its key set, at its own address only; a token's address that is something else.
    assert_eq!(ask(&g, "GET", "https://token.actions.githubusercontent.com/other", b"").0, 404);
    assert_eq!(ask(&g, "GET", "https://other.example/.well-known/jwks", b"").0, 404);
    // STS, however it is asked, and only for this.
    assert_eq!(ask(&g, "POST", "https://sts.us-east-1.amazonaws.com/?Action=AssumeRoleWithWebIdentity", b"").0, 200);
    assert_eq!(ask(&g, "POST", "https://sts.us-east-1.amazonaws.com/", b"Action=AssumeRoleWithWebIdentity").0, 200);
    assert_eq!(ask(&g, "POST", "https://sts.us-east-1.amazonaws.com/", b"Action=Other").0, 404);
    assert_eq!(ask(&g, "POST", "https://sts.example.com/", b"Action=AssumeRoleWithWebIdentity").0, 404);
}

#[test]
fn s3s_stand_in_keeps_objects_as_s3_does() {
    let g = GitHub::default();
    let at = "https://b.s3.us-east-1.amazonaws.com";
    assert_eq!(ask(&g, "GET", &format!("{at}/"), b"").0, 404, "no such bucket");
    assert_eq!(ask(&g, "PUT", &format!("{at}/"), b"").0, 200);
    assert_eq!(ask(&g, "PUT", &format!("{at}/"), b"").0, 409);
    assert_eq!(ask(&g, "HEAD", &format!("{at}/"), b"").0, 200);
    assert_eq!(ask(&g, "PUT", &format!("{at}/a/k"), b"one").0, 200);
    block_on(g.send(Request::new("PUT", &format!("{at}/a/m")).with_body(b"x".to_vec()).with_header("X-Amz-Meta-Kept", "1").with_header("x-amz-checksum-sha256", "c").with_header("other", "2"))).unwrap();
    assert!(ask(&g, "GET", &format!("{at}/?prefix=a/"), b"").1.contains("<KeyCount>2</KeyCount>"));
    assert!(ask(&g, "GET", &format!("{at}/?prefix=none"), b"").1.contains("<KeyCount>0</KeyCount>"));
    assert!(ask(&g, "GET", &format!("{at}/?other=1"), b"").1.contains("<KeyCount>2</KeyCount>"));
    // A copy, of what is there and what is not.
    let copy = |from: &str| block_on(g.send(Request::new("PUT", &format!("{at}/copied")).with_header("x-amz-copy-source", from))).unwrap().status;
    assert_eq!(copy("/b/a/k"), 200);
    assert_eq!(copy("/b/none"), 404);
    assert_eq!(ask(&g, "DELETE", &format!("{at}/a/k"), b"").0, 204);
    assert_eq!(ask(&g, "PATCH", &format!("{at}/a/m"), b"").0, 404, "what S3 has no answer for");
    assert_eq!(ask(&g, "DELETE", &format!("{at}/"), b"").0, 204);
    assert_eq!(ask(&g, "DELETE", "https://nothing.s3.us-east-1.amazonaws.com/", b"").0, 404);
    assert_eq!(ask(&g, "POST", &format!("{at}/"), b"").0, 404);
}

#[test]
fn a_launcher_answers_what_it_can_and_a_control_plane_here_is_asked_as_it_is() {
    let g = GitHub::default();
    let (modal, workers) = ("https://l.modal.run", "https://l.acme.workers.dev");
    assert_eq!(ask(&g, "POST", &format!("{modal}/launch"), b"{\"gitlab\":{}}").0, 400);
    assert!(ask(&g, "POST", &format!("{modal}/launch"), b"{}").1.contains("sandbox"));
    assert!(ask(&g, "POST", &format!("{workers}/launch"), b"{\"gitlab\":{}}").1.contains("standard-2"));
    assert!(ask(&g, "POST", &format!("{workers}/stop"), b"").1.contains("stopped"));
    assert_eq!(ask(&g, "POST", &format!("{workers}/other"), b"").0, 404);
    assert_eq!(ask(&g, "GET", &format!("{workers}/launch"), b"").0, 404);
    assert_eq!(ask(&g, "POST", "https://other.example/launch", b"").0, 404);
    assert_eq!(ask(&g, "POST", "mailto:a@example.com", b"").0, 404);
    // Nothing there; and there, with a query, its headers as they were.
    assert_eq!(ask(&g, "GET", "http://127.0.0.1:1/x", b"").0, 502);
    let served = serve(|_| {});
    assert_eq!(ask(&g, "GET", &format!("{}/health?a=1", served.url), b"").0, 200);
    assert_eq!(block_on(g.send(Request::new("GET", &format!("{}/health", served.url)).with_header("x-other", "1"))).unwrap().status, 200);
}

#[test]
fn gitlab_answers_only_for_what_it_has() {
    let g = GitHub::default();
    let lab = &g.gitlab;
    let token = GitLab::connection()["token"].as_str().unwrap().to_string();
    let call = |method: &str, path: &str| lab.answer(&Request::new(method, &format!("https://gitlab.com/api/v4/{path}")).with_header("private-token", &token)).map(|r| r.status);
    lab.set(1, "tag", json!(true));
    assert_eq!(lab.job(1), Value::Null);
    let project = gitlab::PROJECT;
    for (method, path) in [("GET", "projects/1/hooks".to_string()), ("POST", "projects/1/hooks".into()), ("PUT", "projects/1/hooks/1".into()), ("DELETE", "projects/1/hooks/1".into()), ("GET", "projects/1/jobs".into()), ("GET", "projects/1/jobs/1".into()),
        ("GET", "projects/1/jobs/1/trace".into()), ("POST", "projects/1/jobs/1/cancel".into()), ("PUT", format!("projects/{project}/hooks/x")), ("DELETE", format!("projects/{project}/hooks/x")), ("DELETE", format!("projects/{project}/hooks/1")),
        ("GET", format!("projects/{project}/jobs/x")), ("GET", format!("projects/{project}/jobs/x/trace")), ("POST", format!("projects/{project}/jobs/x/retry")), ("PUT", "runners/x".into()), ("PUT", "runners/1".into()), ("DELETE", "runners/x".into()), ("GET", "runners/x/jobs".into()), ("GET", "nothing".into())] {
        assert_eq!(call(method, &path), None, "{method} {path}");
    }
    // A runner's jobs: only what runs on it.
    let (a, _) = lab.pending("a", &["t"], 0);
    let (b, _) = lab.pending("b", &["t"], 0);
    let (_c, _) = lab.pending("c", &["t"], 0);
    lab.running_on(a, 7, 0);
    lab.running_on(b, 8, 0);
    lab.finish(b, "success", "", 0);
    let jobs = |rid: u64| { let r = lab.answer(&Request::new("GET", &format!("https://gitlab.com/api/v4/runners/{rid}/jobs")).with_header("private-token", &token)).unwrap(); serde_json::from_slice::<Value>(&r.body).unwrap().as_array().unwrap().len() };
    assert_eq!((jobs(7), jobs(8)), (1, 0));
}

/// Two accounts kept apart are two accounts: what one sign-in writes in SSM or a Modal settings Dict, or a recording
/// made for it, the other never reads, as a wrong account's sign-in would find nothing of the right one's in a real
/// cloud. (What SuperCI's tests of several accounts stand on: a call made with the wrong sign-in fails there.)
#[test]
fn each_account_kept_apart_keeps_its_own() {
    let aws = Aws::serve();
    aws.apart("aws:ASIATWO", "222222222222");
    let signed = |key: &str| format!("AWS4-HMAC-SHA256 Credential={key}/20261111/us-east-1/ssm/aws4_request, SignedHeaders=host, Signature=0");
    let ask = |key: &str, target: &str, a: Value| {
        let auth = signed(key);
        let (s, v) = http(&aws.url, "POST", "/ssm.us-east-1.amazonaws.com/", &[("authorization", auth.as_str()), ("x-amz-target", target), ("content-type", "application/x-amz-json-1.1")], a.to_string().as_bytes());
        (s, serde_json::from_str::<Value>(&v).unwrap_or_default())
    };
    assert_eq!(ask("ASIATWO", "AmazonSSM.PutParameter", json!({ "Name": "/superci/two/KEY", "Value": "2" })).0, 200);
    assert_eq!(ask("ASIAONE", "AmazonSSM.PutParameter", json!({ "Name": "/superci/one/KEY", "Value": "1" })).0, 200);
    // (The same name in each: two parameters, one in each account.)
    assert_eq!(ask("ASIATWO", "AmazonSSM.PutParameter", json!({ "Name": "/superci/one/KEY", "Value": "1b" })).0, 200);
    assert_eq!(ask("ASIATWO", "AmazonSSM.DeleteParameter", json!({ "Name": "/superci/one/KEY" })).0, 200);
    assert_eq!(ask("ASIATWO", "AmazonSSM.GetParameter", json!({ "Name": "/superci/one/KEY" })).1["__type"], "ParameterNotFound");
    assert_eq!(ask("ASIAONE", "AmazonSSM.GetParameter", json!({ "Name": "/superci/two/KEY" })).1["__type"], "ParameterNotFound");
    let got = ask("ASIATWO", "AmazonSSM.GetParameter", json!({ "Name": "/superci/two/KEY", "WithDecryption": true })).1;
    assert_eq!((got["Parameter"]["Value"].as_str(), got["Parameter"]["ARN"].as_str().is_some_and(|a| a.contains(":222222222222:"))), (Some("2"), true), "{got}");
    let one = ask("ASIAONE", "AmazonSSM.GetParameter", json!({ "Name": "/superci/one/KEY", "WithDecryption": true })).1;
    assert_eq!((one["Parameter"]["Value"].as_str(), one["Parameter"]["ARN"].as_str().is_some_and(|a| a.contains(&format!(":{ACCOUNT}:")))), (Some("1"), true), "{one}");
    let listed = |key: &str| ask(key, "AmazonSSM.GetParametersByPath", json!({ "Path": "/superci/", "Recursive": true })).1["Parameters"].as_array().unwrap().iter().map(|p| p["Name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!((listed("ASIATWO"), listed("ASIAONE")), (vec!["/superci/two/KEY".to_string()], vec!["/superci/one/KEY".to_string()]));
    assert_eq!(aws.settings_in("222222222222", "two"), [("KEY".to_string(), "2".to_string())]);
    assert!(aws.settings("two").is_empty() && aws.settings_in("222222222222", "one").is_empty() && aws.settings_in("333333333333", "two").is_empty());
    // A recording made for one sign-in answers it alone; another is answered as one with no recording is.
    // (At an address no other recording has.)
    aws.replaying_for("aws:ASIATWO", &json!({ "method": "POST", "url": "https://apart.us-east-1.amazonaws.com/", "target": "", "asked": "Action=Ask", "status": 200, "answer": "<two/>" }).to_string(), &[]);
    let roles = |key: &str| http(&aws.url, "POST", "/apart.us-east-1.amazonaws.com/", &[("authorization", signed(key).as_str())], b"Action=Ask").1;
    assert_eq!(roles("ASIATWO"), "<two/>");
    assert!(roles("ASIAONE").contains("SuperCITestkitUnknown"));
    // Modal: a workspace kept apart keeps its Dicts' entries; a sign-in Modal no longer takes is refused at every call.
    aws.apart("modal:ak-two", "acme-two");
    aws.replaying(&recording("DictUpdate", json!(json!({ "answers": [] }).to_string())), &[]);
    let pickled = |t: &str| [vec![0x80, 4, 0x95], vec![0; 8], vec![0x8c, t.len() as u8], t.as_bytes().to_vec()].concat();
    let entry = |k: &str, v: &str| field(2, &[field(1, &pickled(k)), field(2, &pickled(v))].concat());
    let token = |t: &str| { let mut h = http::HeaderMap::new(); h.insert("x-modal-token-id", t.parse().unwrap()); h };
    aws.modal_answer("DictUpdate", &token("ak-two"), &entry("ROUTING", "two"));
    aws.modal_answer("DictUpdate", &token("ak-one"), &entry("ROUTING", "one"));
    assert_eq!(aws.modal_settings.lock().unwrap().get("ROUTING").map(String::as_str), Some("one"));
    assert_eq!(aws.modal_plane_settings_in("acme-two"), [("ROUTING".to_string(), "two".to_string())]);
    assert!(aws.modal_plane_settings_in("acme-three").is_empty());
    aws.modal_refuse("ak-two");
    assert_eq!(aws.modal_answer("DictUpdate", &token("ak-two"), &entry("ROUTING", "again")).1, Some((16, "Token not found".to_string())));
    assert_eq!(aws.modal_answer("DictUpdate", &token("ak-one"), &entry("ROUTING", "again")).1, None);
    assert_eq!(aws.modal_plane_settings_in("acme-two"), [("ROUTING".to_string(), "two".to_string())]);
    // Cloudflare: an account kept apart keeps its Workers' secrets; another account's Worker of the same name is
    // another Worker.
    aws.apart_cloudflare("2222");
    aws.replaying(&json!({ "method": "PUT", "url": "x", "target": "", "asked": "", "status": 200, "answer": "{}" }).to_string(), &[]);
    let put = |account: &str, v: &str| http(&aws.url, "PUT", &format!("/api.cloudflare.com/client/v4/accounts/{account}/workers/scripts/superci/secrets"), &[("authorization", "Bearer t")], json!({ "name": "ROUTING", "text": v, "type": "secret_text" }).to_string().as_bytes()).0;
    assert_eq!((put("1111", "one"), put("2222", "two")), (200, 200));
    assert_eq!((aws.worker_settings("superci"), aws.worker_settings_in("2222", "superci")), (vec![("ROUTING".to_string(), "one".to_string())], vec![("ROUTING".to_string(), "two".to_string())]));
}
