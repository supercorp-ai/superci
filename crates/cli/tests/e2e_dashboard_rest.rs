//! The dashboard's rarer ways, end to end: a sign-in its cloud does not renew, a cloud that refuses one call, a
//! control plane that does not answer as asked, and moves and updates whose every step can stop. Whatever stops, the
//! dashboard says why and nothing is left half done that it says is done.
mod common;
use superci_testkit::{serve, Served};

fn sign_ins(home: &std::path::Path) -> std::path::PathBuf { home.join("workspaces/default/sign-ins.json") }

/// The workspace's sign-ins changed as `change` says.
fn kept_as(home: &std::path::Path, change: impl Fn(&mut serde_json::Value)) {
    let file = sign_ins(home);
    let mut k: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    change(&mut k);
    std::fs::write(&file, k.to_string()).unwrap();
}

/// Other control planes kept in the workspace beside the one in use (listed at once, before any cloud is asked).
fn beside(home: &std::path::Path, planes: serde_json::Value) { kept_as(home, |k| k["planes_more"] = planes.clone()) }

/// The workspace's AWS sign-in with its credentials over, and AWS's sign-in failing as it renews them (not ending
/// it: AWS did not answer).
fn aws_renewal_fails(served: &Served, home: &std::path::Path) {
    kept_as(home, |k| {
        k["aws"]["creds"]["expires_at_ms"] = 0.into();
        k["aws"]["refresh_token"] = "superci-refresh".into();
        k["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    });
    served.aws.fail("signin.aws.amazon.com/v1/token", 500, r#"{"error":"server_error","message":"We encountered an internal error."}"#, None);
}

/// A form the dashboard's pages send, and its answer: status and words.
fn sent(link: &str, path: &str, fields: &[(&str, &str)]) -> (u16, String) {
    let (status, _, body) = common::post(link, path, fields);
    (status, common::words(&body))
}

/// Every change that needs AWS's sign-in, while AWS does not renew it: each says so, and none goes on without it.
#[test]
fn every_change_that_needs_aws_says_so_while_aws_does_not_renew_its_sign_in() {
    let served = serve(move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(own_role())));
    served.connect_github();
    let home = common::signed_in(&served);
    beside(&home, serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": "http://127.0.0.1:9", "plane_id": "otherplane01", "label": "superci" } }]));
    aws_renewal_fails(&served, &home);
    // (Signed in to Cloudflare too: the control plane's launcher there comes along with a move.)
    kept_as(&home, |k| k["cloudflare"] = cloudflare_signed_in());
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    let networks = "us-east-1 subnet-0123456789abcdef0 sg-0123456789abcdef0";
    for (path, fields) in [
        ("/runners/aws-own", &[][..]),
        ("/name", &[("name", "Acme CI")]),
        ("/runners/remove", &[("cloud", "aws"), ("stop", "on")]),
        ("/plane/aws", &[("region", "us-east-1")]),
        ("/aws/image", &[("region", "us-east-1")]),
        ("/plane/update", &[("plane", superci_testkit::PLANE)]),
        ("/permissions/aws", &[("plane", superci_testkit::PLANE)]),
        ("/routing", &[("action", "account_usd"), ("account", "aws"), ("usd", "100"), ("budget", "emails")]),
        ("/routing", &[("action", "account"), ("cloud", "aws"), ("networks", networks)]),
        ("/routing", &[("action", "pool"), ("cloud", "aws"), ("region_0", "eu-north-1")]),
        ("/aws/connect", &[("region", "us-east-1")]),
        ("/plane/move", &[("plane", "otherplane01")]),
        ("/plane/delete", &[("plane", "otherplane01")]),
    ] {
        let (status, said) = sent(&link, path, fields);
        assert!(status >= 400 && said.contains("AWS"), "{path} {fields:?}: {status} {said}");
    }
    // The commands that read the account say so too.
    for args in [&["accounts", "retrieve", "aws"][..], &["planes", "allow", superci_testkit::PLANE, "--dry-run"]] {
        let (ok, said) = common::superci_in_time(&home, args, &[("SUPERCI_PAUSE_MS", "1")], 60);
        assert!(!ok && said.contains("AWS"), "{args:?}: {said}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A Cloudflare sign-in that takes, good for years.
fn cloudflare_signed_in() -> serde_json::Value { serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" }) }

/// The served control plane kept as a Cloudflare one: what is written for it goes to its Worker's secrets, which it
/// does not read (a change written there shows nowhere).
fn as_cloudflare(home: &std::path::Path, served: &Served) {
    let url = served.url.clone();
    kept_as(home, |k| k["plane"] = serde_json::json!({ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci", "url": url, "plane_id": superci_testkit::PLANE, "label": "superci" } }));
}

/// Cloudflare's answer of an internal error.
const CF_FAILED: &str = r#"{"success":false,"errors":[{"code":10000,"message":"Internal server error"}],"messages":[],"result":null}"#;

/// Waits up to 15 seconds for `holds`, else fails saying `what`.
fn until(what: &str, mut holds: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !holds() { assert!(std::time::Instant::now() < end, "waited for {what}"); std::thread::sleep(std::time::Duration::from_millis(100)) }
}

/// Every change that needs Cloudflare's sign-in, while Cloudflare does not renew it (not ending it: Cloudflare did not
/// answer): each says so; an update started says so where it shows.
#[test]
fn every_change_that_needs_cloudflare_says_so_while_cloudflare_does_not_renew_its_sign_in() {
    let served = serve(|_| {});
    served.connect_github();
    // (Its runners a launcher's in Cloudflare, not containers of its own.)
    served.aws.params.lock().unwrap().remove(&format!("/superci/{}/CONTAINERS", superci_testkit::PLANE));
    let home = common::signed_in(&served);
    as_cloudflare(&home, &served);
    beside(&home, serde_json::json!([{ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci-other", "url": "http://127.0.0.1:9", "plane_id": "cfplane00009", "label": "superci" } }]));
    kept_as(&home, |k| k["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 0, "refresh": "cloudflare-refresh-for-tests" }));
    served.aws.fail("dash.cloudflare.com/oauth2/token", 500, r#"{"error":"server_error"}"#, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    for (path, fields) in [
        ("/name", &[("name", "Acme CI")][..]),
        ("/keys/remove", &[("name", "DEVICE_KEY_4102444800_OTHER")]),
        ("/runners/remove", &[("cloud", "cloudflare"), ("stop", "on")]),
        ("/plane/delete", &[("plane", "cfplane00009")]),
    ] {
        let (status, said) = sent(&link, path, fields);
        assert!(status >= 400 && (said.contains("Cloudflare") || said.contains("500")), "{path} {fields:?}: {status} {said}");
    }
    let (status, said) = sent(&link, "/plane/update", &[("plane", superci_testkit::PLANE)]);
    assert!(status == 303, "{status} {said}");
    until("the update said to stop", || common::words(&common::part(&link, "/?p=planes&fragment=1").1).contains("The update stopped: Sign in with Cloudflare first."));
    let _ = std::fs::remove_dir_all(&home);
}

/// Setting a control plane up in Cloudflare while its accounts are not read, and in an account the sign-in does not
/// reach: each said, nothing deployed.
#[test]
fn a_cloudflare_control_plane_is_set_up_only_in_an_account_the_sign_in_reaches() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    kept_as(&home, |k| k["cloudflare"] = cloudflare_signed_in());
    served.aws.fail("/client/v4/accounts?per_page", 500, CF_FAILED, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/plane/cloudflare", &[("account", "1111111111111111111111111111111a")]);
    assert!(status >= 400 && said.contains("Internal server error"), "{status} {said}");
    served.aws.fail_none();
    served.aws.fail("/client/v4/accounts?per_page", 200, r#"{"success":true,"errors":[],"messages":[],"result":[{"id":"2222222222222222222222222222222b","name":"Globex"}]}"#, None);
    let (status, said) = sent(&link, "/plane/cloudflare", &[("account", "1111111111111111111111111111111a")]);
    assert!(status >= 400 && said.contains("the sign-in cannot reach that account"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Runners added to a control plane that does not answer: nothing is deployed, and the page says to try again.
#[test]
fn runners_are_not_added_to_a_control_plane_that_does_not_answer() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    common::kept_at(&home, "http://127.0.0.1:9");
    kept_as(&home, |k| {
        k["cloudflare"] = cloudflare_signed_in();
        k["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    });
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    for path in ["/runners/modal", "/runners/cloudflare"] {
        let (status, said) = sent(&link, path, &[]);
        assert!(status == 503 && said.contains("The control plane did not answer yet"), "{path}: {status} {said}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// Signed in to an AWS account with no control plane in it: a job's log and a run's artifact are of none; and with AWS
/// not answering at all, `status` says so.
#[test]
fn a_job_or_an_artifact_asked_for_where_no_control_plane_is_found_is_of_none() {
    let served = serve(|_| {});
    let home = common::aws_only(&served.aws);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    for path in ["/job/view?id=1", "/run/artifact?run=1&name=coverage"] {
        let (status, _, said) = common::get(&link, path);
        assert!(status == 404 && common::words(&said).contains("No control plane"), "{path}: {status} {}", common::words(&said));
    }
    served.arm(superci_testkit::Fault::Outage(1));
    let (ok, said) = common::superci_in_time(&home, &["status"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(!ok && said.contains("AWS"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A sign-in with Modal that Modal does not start, and one with Cloudflare whose last step Cloudflare refuses: each said.
#[test]
fn sign_ins_that_modal_does_not_start_or_cloudflare_does_not_finish_say_why() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // (On Cloudflare's own port: its sign-in comes back to no other.)
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() }.to_string();
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_OAUTH_PORT", port.as_str()), ("SUPERCI_PORT", port.as_str())]);
    // (Its key's cookie on that port is the usual one.)
    let key = link.split_once("/?k=").unwrap().1.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let ask = |method: &str, path: &str| {
        let url = format!("http://127.0.0.1:{port}{path}");
        let mut r = if method == "POST" { agent.post(&url).header("cookie", &format!("superci_local={key}")).header("sec-fetch-site", "same-origin").header("content-type", "application/x-www-form-urlencoded").send("next=").unwrap() }
            else { agent.get(&url).header("cookie", &format!("superci_local={key}")).header("sec-fetch-site", "same-origin").call().unwrap() };
        let to = r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        (r.status().as_u16(), to, r.body_mut().read_to_string().unwrap_or_default())
    };
    served.arm(superci_testkit::Fault::Outage(1));
    for (method, path) in [("GET", "/connect/modal"), ("POST", "/modal/signin")] {
        let (status, _, said) = ask(method, path);
        assert!(status >= 400, "{path}: {status} {}", common::words(&said));
    }
    served.faults().calm();
    let (_, to, page) = ask("GET", "/connect/cloudflare");
    let at = if to.is_empty() { page } else { to };
    let state = at.split("state=").nth(1).and_then(|r| r.split(['&', '"', '\'']).next()).expect("a state").to_string();
    served.aws.fail("dash.cloudflare.com/oauth2/token", 500, r#"{"error":"server_error"}"#, None);
    let (status, _, said) = ask("GET", &format!("/oauth/callback?code=a-code-from-cloudflare&state={state}"));
    assert!(status >= 400 && common::words(&said).contains("server_error"), "{status} {}", common::words(&said));
    let _ = std::fs::remove_dir_all(&home);
}

/// A server that answers every request with these bytes and closes: a site that answers as it should not.
fn answering_with(bytes: &'static [u8]) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || for mut s in listener.incoming().flatten() {
        use std::io::{Read, Write};
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let _ = s.write_all(bytes);
    });
    url
}

/// A workspace whose control plane is the served one kept as Cloudflare's, signed in to Cloudflare only (what it
/// writes goes to the Worker's secrets, which that control plane does not read).
fn cloudflare_only(served: &Served) -> std::path::PathBuf {
    let home = common::signed_in(served);
    as_cloudflare(&home, served);
    kept_as(&home, |k| { k.as_object_mut().unwrap().remove("aws"); k["cloudflare"] = cloudflare_signed_in(); });
    home
}

/// A change written to the Worker's secrets of a control plane that reads its settings elsewhere: never shown, so
/// said as not taken (its own network, its regions, Cloudflare's location and image); and its containers that cannot
/// be switched off say so, its settings for runs going nowhere.
#[test]
fn a_change_the_control_plane_does_not_show_is_said_as_not_taken() {
    let served = serve(|_| {});
    served.connect_github();
    let home = cloudflare_only(&served);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    for fields in [
        &[("action", "account"), ("cloud", "aws"), ("networks", "us-east-1 subnet-0123456789abcdef0 sg-0123456789abcdef0")][..],
        &[("action", "pool"), ("cloud", "aws"), ("region_0", "eu-north-1")],
        &[("action", "pool"), ("cloud", "cloudflare"), ("location", "weur")],
        &[("action", "account"), ("cloud", "cloudflare"), ("image", "https://images.example.com/full")],
    ] {
        let (status, said) = sent(&link, "/routing", fields);
        assert!(status >= 400 && said.contains("did not show it within twenty seconds"), "{fields:?}: {status} {said}");
    }
    served.aws.fail("/secrets|\"CONTAINERS\"", 500, CF_FAILED, None);
    let (status, said) = sent(&link, "/runners/remove", &[("cloud", "cloudflare"), ("stop", "on")]);
    assert!(status >= 400 && said.contains("Internal server error"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Stopping SuperCI beside another CI and a control plane that serves nothing: the other CI goes on, and so does the
/// one that serves nothing (it may be that CI's to move to). Beside one alone that cannot be deleted: said.
#[test]
fn leaving_beside_another_ci_leaves_it_and_says_what_could_not_go() {
    for beside_a_ci in [true, false] {
        let served = serve(|_| {});
        served.connect_github();
        // (Another CI: GitHub's App connected, as its settings say.)
        served.aws.put("/superci/testplane0002/GITHUB_APP", &superci_testkit::GitHub::app().to_string());
        let aws = served.aws.clone();
        let ci = superci_testkit::serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
        let spare = superci_testkit::serve_as("testplane0003", |_| {});
        let home = common::signed_in(&served);
        let at = |url: &str, id: &str| serde_json::json!({ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": url, "plane_id": id, "label": "superci" } });
        let mut more = vec![at(&spare.url, "testplane0003")];
        if beside_a_ci { more.push(at(&ci.url, "testplane0002")) }
        beside(&home, serde_json::Value::Array(more));
        // (From its page, as kept: a command looks in the account first, where the test kit lists none of these.)
        let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
        let (_, said) = sent(&link, "/plane/leave?c=testplane0001", &[("confirm", "superci")]);
        if beside_a_ci { assert!(said.contains("That control plane is stopped") && said.contains("serves a code host of its own"), "{said}") }
        // (Neither could be deleted here: the test kit's AWS does not answer for their schedules.)
        else { assert!(said.contains("SuperCI is stopped") && said.matches("has no answer").count() == 2, "{said}") }
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// GitHub's App made again for the organization connected already, while the control plane's cloud will not keep
/// it; one made for another organization; and one whose control plane is gone when GitHub comes back.
#[test]
fn an_app_made_on_github_that_cannot_be_kept_says_so() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "1")]);
    let made = || {
        let (status, _, form) = common::post(&link, "/github/start", &[("gh-where", "public"), ("host", ""), ("login", "acme")]);
        assert_eq!(status, 200, "{}", common::words(&form));
        let state = form.split("state=").nth(1).unwrap().split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_').next().unwrap().to_string();
        state
    };
    let back = |state: &str| { let (status, _, said) = common::get(&link, &format!("/github/callback?code=a1b2c3d4e5f6&state={state}")); (status, common::words(&said)) };
    served.aws.fail("AmazonSSM.PutParameter|GITHUB_APP", 500, &superci_testkit::json_error("InternalServerError", "An internal error occurred. Try again."), None);
    let (status, said) = back(&made());
    assert!(status >= 400 && said.contains("internal error"), "the first one's place: {status} {said}");
    // Another organization's, beside it.
    let app = superci_testkit::GitHub::app();
    served.github.answer_with("POST", "api.github.com/app-manifests/", 201, &[], &serde_json::json!({ "id": 3003, "slug": "superci-globex", "owner": { "login": "globex", "id": 9920, "type": "Organization" },
        "name": "SuperCI globex", "html_url": "https://github.com/apps/superci-globex", "events": ["workflow_job"], "webhook_secret": app["webhook_secret"], "pem": app["pem"] }).to_string());
    let (status, said) = back(&made());
    assert!(status >= 400 && said.contains("internal error"), "beside it: {status} {said}");
    served.aws.fail_none();
    // Signed out meanwhile: no control plane to keep it in.
    let state = made();
    assert_eq!(common::post(&link, "/signout", &[]).0, 303);
    let (status, said) = back(&state);
    assert!(status >= 400 && said.contains("no control plane yet"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The workspace's picture, which GitHub does not give (no answer, or one cut short): left out, the page as it was.
#[test]
fn a_picture_github_does_not_give_is_left_out() {
    for pictures in ["http://127.0.0.1:9".to_string(), answering_with(b"HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 1000\r\nconnection: close\r\n\r\nshort")] {
        let served = serve(|_| {});
        served.connect_github();
        let home = common::signed_in(&served);
        let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PICTURES_AT", pictures.as_str())]);
        for _ in 0..3 { assert_eq!(common::part(&link, "/?p=overview&fragment=1").0, 200); std::thread::sleep(std::time::Duration::from_millis(300)) }
        let about: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("workspaces/default/workspace.json")).unwrap_or_default()).unwrap_or_default();
        assert!(about["picture"].as_str().is_none_or(str::is_empty), "{about}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// `superci dashboard` opens its page in the browser, and so does a task; asked for a port that is Cloudflare's own
/// and taken, it says so.
#[test]
fn the_dashboard_opens_in_the_browser_and_a_port_asked_for_and_taken_is_said() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // A browser that notes what it is handed (SUPERCI_BROWSER: a debug build opens nothing else).
    let bin = common::temp("browser");
    let opened = bin.join("opened");
    for name in ["open", "xdg-open"] {
        std::fs::write(bin.join(name), format!("#!/bin/sh\necho \"$1\" >> {}\n", opened.display())).unwrap();
        std::fs::set_permissions(bin.join(name), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() }.to_string();
    let mut c = common::command(&home).args(["dashboard", "--workspace=default"]).env("SUPERCI_PORT", &port).env("SUPERCI_BROWSER", bin.join("open")).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut out = std::io::BufReader::new(c.stdout.take().unwrap());
    let mut said = String::new();
    while !said.contains("It is opening in your browser") { let mut l = String::new(); if std::io::BufRead::read_line(&mut out, &mut l).unwrap_or(0) == 0 { break } said.push_str(&l) }
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = c.kill();
    let _ = c.wait();
    assert!(said.contains("It is opening in your browser"), "{said}");
    // A task (connecting GitHub) opens its page too.
    let mut c = common::command(&home).args(["connections", "create", "github", "acme", "--workspace=default"]).env("SUPERCI_PORT", { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() }.to_string()).env("SUPERCI_BROWSER", bin.join("open")).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut out = std::io::BufReader::new(c.stdout.take().unwrap());
    let mut said = String::new();
    while !said.contains("Create the GitHub App") { let mut l = String::new(); if std::io::BufRead::read_line(&mut out, &mut l).unwrap_or(0) == 0 { break } said.push_str(&l) }
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = c.kill();
    let _ = c.wait();
    assert!(said.contains("Create the GitHub App"), "{said}");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let o = common::command(&home).args(["dashboard", "--no-browser", "--workspace=default"]).env("SUPERCI_PORT", &port).env("SUPERCI_OAUTH_PORT", &port).output().unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains(&format!("port {port} is in use")), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&bin);
}

/// A read key made while the control plane answers to anyone and not with its status: said as not answering.
#[test]
fn a_key_is_not_made_while_the_control_plane_does_not_give_its_status() {
    let served = serve(|_| {});
    let plane = superci_testkit::front(&served.url, |method, target, status, body| {
        if method == "GET" && target.starts_with("/status") { return (500, br#"{"error":"busy"}"#.to_vec()) }
        (status, body)
    });
    *served.aws.plane_url.lock().unwrap() = plane.clone();
    let home = common::signed_in(&served);
    common::kept_at(&home, &plane);
    let (ok, said) = common::superci_in_time(&home, &["keys", "create", "agent"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(!ok && said.contains("The control plane did not answer"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Two control planes in one AWS account: the one in use with everything a move carries (a further organization's
/// App, a further GitLab, a secret for runs, its own network, Cloudflare's image and location, a label and a name),
/// and one standing by from a move begun before, with what the one in use no longer has (an organization's App, a
/// GitLab, a secret). The new one at `front(new)`'s address, when one is given. A dashboard on the workspace.
struct Moving { served: Served, new: Served, home: std::path::PathBuf, _child: common::Dashboard, link: String }

impl Drop for Moving { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.home); } }

fn moving(front: Option<fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>)>) -> Moving { moving_with(front, |_, _| {}) }

/// The same, with `before` done to the control plane in use and the workspace before the dashboard starts.
fn moving_with(front: Option<fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>)>, before: impl Fn(&Served, &std::path::Path)) -> Moving {
    let served = serve(|_| {});
    served.connect_github();
    served.connect_gitlab();
    let mut app = superci_testkit::GitHub::app();
    let secret = |name: &str| (superci_core::secrets::setting(name, &Default::default()), serde_json::json!({ "name": name, "value": "x", "updated_ms": 1 }).to_string());
    let at = |id: &str, k: &str| format!("/superci/{id}/{k}");
    app["id"] = 2002.into();
    app["owner"] = "globex".into();
    let mut gitlab = superci_testkit::GitLab::connection();
    gitlab["id"] = "gnew01".into();
    let (npm, npm_kept) = secret("NPM_TOKEN");
    for (k, v) in [("CONTAINERS", "off".to_string()), ("GITHUB_APP_2002", app.to_string()), ("GITLAB_gnew01", gitlab.to_string()), (npm.as_str(), npm_kept), ("LABELS", r#"["big","superci"]"#.into()), ("NAME", "Acme CI".into()),
        ("AWS_NETWORKS", r#"{"us-east-1":{"subnets":["subnet-0123456789abcdef0"],"security_groups":["sg-0123456789abcdef0"],"private":false}}"#.into()), ("CF_IMAGE", "https://images.example.com/full".into()), ("CF_LOCATION", "weur".into())] {
        served.aws.put(&at(superci_testkit::PLANE, k), &v);
    }
    // The new one, standing by, with what the one in use no longer has.
    let mut stale = superci_testkit::GitHub::app();
    stale["id"] = 9.into();
    let mut old_gitlab = superci_testkit::GitLab::connection();
    old_gitlab["id"] = "gold".into();
    let (old, old_kept) = secret("OLD_TOKEN");
    for (k, v) in [("GITHUB_APP", superci_testkit::GitHub::app().to_string()), ("GITHUB_APP_9", stale.to_string()), ("GITLAB_gold", old_gitlab.to_string()), (old.as_str(), old_kept)] { served.aws.put(&at("testplane0002", k), &v) }
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
    new.store.0.lock().unwrap().insert("standby".into(), "1".into());
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&at("testplane0002", &k), &v) }
    let url = match front { Some(f) => superci_testkit::front(&new.url, f), None => new.url.clone() };
    *new.aws.plane_url.lock().unwrap() = url.clone();
    // (The account lists both, as AWS answered for two.)
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &url)]);
    let home = common::signed_in(&served);
    kept_as(&home, |k| {
        k["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": url, "plane_id": "testplane0002", "label": "superci" } }]);
        k["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    });
    before(&served, &home);
    let (child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "1")]);
    Moving { served, new, home, _child: child, link }
}

impl Moving {
    /// The move started, and how it ended as its row says it (what it stopped with, or that it moved).
    fn moved(&self) -> String {
        let (status, to, said) = common::post(&self.link, "/plane/move?c=testplane0001", &[("plane", "testplane0002")]);
        assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", common::words(&said));
        let mut page = String::new();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !(page.contains("Try again") || page.contains("Moved to")) {
            assert!(std::time::Instant::now() < end, "the move not over: {page}");
            std::thread::sleep(std::time::Duration::from_millis(100));
            page = common::words(&common::part(&self.link, "/?p=planes&fragment=1&c=testplane0001").1);
        }
        page
    }
}

/// A move whose every write to the new control plane's settings can fail: each stops it there, said, and the old one
/// stays in use (its own network, Cloudflare's image and location, its labels and name, each further App, GitLab and
/// secret, and what the new one had from before that goes).
#[test]
fn a_move_stops_where_a_write_to_the_new_control_plane_fails_and_says_so() {
    for (action, setting) in [("PutParameter", "GITHUB_APP_2002"), ("DeleteParameter", "GITHUB_APP_9"), ("PutParameter", "GITLAB_gnew01"), ("DeleteParameter", "GITLAB_gold"),
        ("PutParameter", "SECRET_NPM_TOKEN"), ("DeleteParameter", "SECRET_OLD_TOKEN"), ("PutParameter", "AWS_NETWORKS"), ("PutParameter", "CF_IMAGE"), ("PutParameter", "LABELS"),
        ("PutParameter", "NAME"), ("PutParameter", "CF_LOCATION")] {
        let m = moving(None);
        m.served.aws.fail(&format!("AmazonSSM.{action}|/superci/testplane0002/{setting}"), 500, &superci_testkit::json_error("InternalServerError", "An internal error occurred. Try again."), None);
        let page = m.moved();
        assert!(page.contains("An internal error occurred") && page.contains("Try again"), "{setting}: {page}");
        assert!(m.new.store.under("standby").len() == 1, "{setting}: still standing by");
    }
}

/// A new control plane that refuses its part of a move outright, one that never takes its move token, and one that
/// takes GitHub over but for an organization whose App would not move: each said.
#[test]
fn a_move_says_a_new_control_plane_that_refuses_it_never_takes_it_or_could_not_move_an_app() {
    let refusing: fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>) = |method, target, status, body| if method == "POST" && target.starts_with("/move/import") { (400, br#"{"error":"no"}"#.to_vec()) } else { (status, body) };
    let page = moving(Some(refusing)).moved();
    assert!(page.contains("answered 400"), "{page}");
    let never: fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>) = |method, target, status, body| if method == "POST" && target.starts_with("/move/import") { (403, b"no move token, or one used already".to_vec()) } else { (status, body) };
    let page = moving(Some(never)).moved();
    assert!(page.contains("did not take its move token"), "{page}");
    let partly: fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>) = |method, target, status, body| {
        if method != "POST" || !target.starts_with("/move/claim") || status != 200 { return (status, body) }
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        v["github_failed"] = serde_json::json!(["globex: Not Found"]);
        (200, v.to_string().into_bytes())
    };
    let page = moving(Some(partly)).moved();
    assert!(page.contains("GitHub would not point an App here (globex: Not Found)"), "{page}");
}

/// A move that brings a launcher in Cloudflare while no Cloudflare account is known to put it in, and one that brings
/// a launcher in Modal to a new control plane that does not give its keys: each said.
#[test]
fn a_move_bringing_a_launcher_says_where_it_cannot_go() {
    let launchers = |cloud: &'static str| move |served: &Served, home: &std::path::Path| {
        served.aws.put(&format!("/superci/{}/LAUNCHERS", superci_testkit::PLANE), &serde_json::json!([{ "cloud": cloud, "url": format!("https://superci-runners-testplane0001.{cloud}.example") }]).to_string());
        kept_as(home, |k| { k["cloudflare"] = cloudflare_signed_in(); k["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" }); });
    };
    let page = moving_with(None, launchers("cloudflare")).moved();
    assert!(page.contains("no Cloudflare account"), "{page}");
    let keyless: fn(&str, &str, u16, Vec<u8>) -> (u16, Vec<u8>) = |_, target, status, body| if target.starts_with("/.well-known/jwks.json") { (500, b"{}".to_vec()) } else { (status, body) };
    let page = moving_with(Some(keyless), launchers("modal")).moved();
    assert!(page.contains("500"), "{page}");
}

/// A workspace whose control plane is the served one, its key not kept (the dashboard hands it a new one), and an
/// expired key of another device's in its settings: taken out once the new one is handed. In AWS, and in Cloudflare
/// (the served one kept as Cloudflare's).
#[test]
fn a_device_key_that_has_expired_is_taken_out_once_a_new_one_is_handed() {
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{}/DEVICE_KEY_1000_OLD", superci_testkit::PLANE), "x");
    let home = common::signed_in(&served);
    kept_as(&home, |k| { k.as_object_mut().unwrap().remove("key"); });
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    until("the expired key taken out", || { common::part(&link, "/?p=overview&fragment=1"); !served.aws.settings(superci_testkit::PLANE).iter().any(|(k, _)| k == "DEVICE_KEY_1000_OLD") });
    let _ = std::fs::remove_dir_all(&home);
    let served = serve(|_| {});
    let home = cloudflare_only(&served);
    kept_as(&home, |k| { k.as_object_mut().unwrap().remove("key"); });
    served.aws.fail("/workers/scripts/superci/secrets", 200, r#"{"success":true,"errors":[],"messages":[],"result":[{"name":"DEVICE_KEY_1000_OLD","type":"secret_text"}]}"#, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let gone = || served.aws.asked.lock().unwrap().iter().any(|a| a["method"] == "DELETE" && a["url"].as_str().is_some_and(|u| u.ends_with("/secrets/DEVICE_KEY_1000_OLD")));
    until("the expired key taken out", || { common::part(&link, "/?p=overview&fragment=1"); gone() });
    let _ = std::fs::remove_dir_all(&home);
}

/// Signing out of a workspace whose control plane is in Cloudflare: its key taken out there. A device key taken out of
/// one in Modal: through Modal, which says what it says.
#[test]
fn a_key_is_taken_out_of_a_control_plane_in_cloudflare_or_modal_with_its_sign_in() {
    let served = serve(|_| {});
    let home = cloudflare_only(&served);
    let (ok, said) = common::superci_in_time(&home, &["logout"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(ok && said.contains("no longer takes this machine's key"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let url = served.url.clone();
    kept_as(&home, |k| {
        k["plane"] = serde_json::json!({ "Modal": { "workspace": "acmeorg", "url": url, "plane_id": superci_testkit::PLANE, "label": "superci" } });
        k["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    });
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/keys/remove", &[("name", "DEVICE_KEY_4102444800_OTHER")]);
    assert!(status == 303 || said.contains("That key was not taken out"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane in Cloudflare from before the new names, its launchers said as agents: its update writes them as
/// LAUNCHERS first, and stops there when Cloudflare will not keep that: nothing is deployed.
#[test]
fn an_update_that_cannot_move_a_control_planes_old_names_stops_before_deploying() {
    let served = serve(|_| {});
    let plane = superci_testkit::front(&served.url, |_, target, status, body| {
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        if target.starts_with("/health") { v["version"] = "0.19.12".into() }
        else if target.starts_with("/status") { v["agents"] = serde_json::json!([{ "cloud": "modal", "url": "https://acme--superci-runners.modal.run" }]) }
        else { return (status, body) }
        (status, v.to_string().into_bytes())
    });
    *served.aws.plane_url.lock().unwrap() = plane.clone();
    let home = cloudflare_only(&served);
    kept_as(&home, |k| k["plane"]["Cloudflare"]["url"] = plane.clone().into());
    served.aws.fail("/secrets|LAUNCHERS", 500, CF_FAILED, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/plane/update", &[("plane", superci_testkit::PLANE)]);
    assert!(status == 303, "{status} {said}");
    // (Its launchers written first, refused: nothing deployed after.)
    until("its launchers refused", || served.aws.asked.lock().unwrap().iter().any(|a| a["method"] == "PUT" && a["status"] == 500 && a["asked"].as_str().is_some_and(|s| s.contains("LAUNCHERS"))));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/workers/scripts/superci"))), "nothing deployed");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane in Cloudflare whose machines start in the AWS account signed in to: its update gives its role
/// there what this version asks for, which needs AWS's sign-in, not renewed now: said, nothing deployed.
#[test]
fn an_update_that_gives_an_aws_role_needs_aws_s_sign_in() {
    let creds = superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX };
    let served = serve(move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(creds.clone())));
    let home = common::signed_in(&served);
    as_cloudflare(&home, &served);
    kept_as(&home, |k| k["cloudflare"] = cloudflare_signed_in());
    aws_renewal_fails(&served, &home);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/plane/update", &[("plane", superci_testkit::PLANE)]);
    assert!(status >= 400 && said.contains("AWS"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// What an AWS role lacks, asked of a control plane that does not answer (read with what is kept of its regions) and
/// of one whose machines are in no AWS account: each said.
#[test]
fn what_an_aws_role_lacks_is_read_of_control_planes_that_do_not_answer_or_have_none() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    beside(&home, serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": "http://127.0.0.1:9", "plane_id": "otherplane01", "label": "superci" } },
        { "Modal": { "workspace": "acmeorg", "url": "http://127.0.0.1:9", "plane_id": "mdplane00009", "label": "superci" } }]));
    let (_, said) = common::superci_in_time(&home, &["planes", "allow", "otherplane01", "--dry-run"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(said.contains("otherplane01"), "{said}");
    let (ok, said) = common::superci_in_time(&home, &["planes", "allow", "mdplane00009", "--dry-run"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(!ok && said.contains("Its machines do not run in an AWS account"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane in Cloudflare deleted from beside the one in use, its machines in the AWS account signed in to:
/// once deleted in Cloudflare, its part in AWS needs AWS's sign-in, not renewed now: said.
#[test]
fn a_control_plane_deleted_with_a_part_in_aws_needs_aws_s_sign_in_for_that_part() {
    let served = serve(|_| {});
    let creds = superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX };
    let other = superci_testkit::serve_as("testplane0002", move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(creds.clone())));
    let home = common::signed_in(&served);
    let url = other.url.clone();
    kept_as(&home, |k| {
        k["planes_more"] = serde_json::json!([{ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci-other", "url": url, "plane_id": "testplane0002", "label": "superci" } }]);
        k["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
        k["cloudflare"] = cloudflare_signed_in();
    });
    aws_renewal_fails(&served, &home);
    served.aws.fail("/containers/applications", 200, r#"{"success":true,"errors":[],"messages":[],"result":[]}"#, None);
    served.aws.fail("/workers/scripts/superci-other", 200, r#"{"success":true,"errors":[],"messages":[],"result":null}"#, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/plane/delete", &[("plane", "testplane0002")]);
    assert!(status >= 400 && said.contains("AWS"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Modal's runners for a control plane that does not give its keys: said, nothing deployed.
#[test]
fn modal_runners_are_not_added_to_a_control_plane_that_does_not_give_its_keys() {
    let served = serve(|_| {});
    let plane = superci_testkit::front(&served.url, |_, target, status, body| if target.starts_with("/.well-known/jwks.json") { (500, b"{}".to_vec()) } else { (status, body) });
    *served.aws.plane_url.lock().unwrap() = plane.clone();
    let home = common::signed_in(&served);
    common::kept_at(&home, &plane);
    kept_as(&home, |k| k["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" }));
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/runners/modal", &[]);
    assert!(status >= 400 && said.contains("500"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// An AWS control plane updated while AWS will not make its machines' network: said where it stopped.
#[test]
fn an_update_that_cannot_make_the_machines_network_says_so() {
    let creds = superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX };
    let served = serve(move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(creds.clone())));
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    // (Once its new code is up: what makes the network fails from then on.)
    let aws = served.aws.clone();
    served.aws.during(&format!("functions/superci-plane-{}/code", superci_testkit::PLANE), move || aws.fail("DescribeVpcs", 500, &superci_testkit::ec2_error("InternalError", "An internal error has occurred"), None));
    let (ok, said) = common::superci_in_time(&home, &["planes", "update"], &[("SUPERCI_PAUSE_MS", "1")], 120);
    assert!(!ok && said.contains("internal error"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Credentials the served control plane's own AWS role gives it: its machines start in the account.
fn own_role() -> superci_core::aws::Credentials { superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX } }

/// AWS's budget for the account, asked for from Limits: made with the sign-in of whoever asks (here AWS's Budgets
/// not answering as asked: said).
#[test]
fn an_aws_budget_is_made_with_the_sign_in_of_whoever_asks() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/routing", &[("action", "account_usd"), ("account", "aws"), ("usd", "100"), ("budget", "emails")]);
    assert!(status == 303 || said.contains("AWS did not take the budget"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The account's own copy of the image jobs run on, while the list of builds its maker publishes cannot be read: what
/// the account allows is still said.
#[test]
fn an_accounts_copy_is_looked_at_while_the_newest_build_cannot_be_read() {
    let served = serve(|_| {});
    let home = common::aws_only(&served.aws);
    served.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[("ca-central-1", "us-east-1")]);
    served.aws.replaying(include_str!("../fixtures/aws/images/05-update-newest.jsonl"), &[("20261006080844", "20250101000000"), ("ca-central-1", "us-east-1")]);
    served.github.answer_with("GET", "runner-images-for-aws/releases", 500, &[], r#"{"message":"Server Error"}"#);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    until("the account looked at", || common::words(&common::part(&link, "/?p=plane&fragment=1").1).contains("AWS account"));
    let _ = std::fs::remove_dir_all(&home);
}

/// The image copied into a region, AWS refusing the copy: what AWS said, with the region it was asked for in; the
/// maker's list of images not read: said.
#[test]
fn an_image_copy_aws_refuses_is_said_with_its_region() {
    let served = serve(|_| {});
    let home = common::aws_only(&served.aws);
    served.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[]);
    served.aws.fail("CopyImage", 400, &superci_testkit::ec2_error("InvalidAMIID.NotFound", "The image id '[ami-0123456789abcdef0]' does not exist"), None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/aws/image", &[("region", "ca-central-1")]);
    assert!(status >= 400 && said.contains("ca-central-1") && said.contains("does not exist"), "{status} {said}");
    // The maker's list of images not read: nothing to copy from, said.
    served.github.fail("GET", "runner-images-for-aws/releases", 503, "unavailable");
    let (status, said) = sent(&link, "/aws/image", &[("region", "ca-central-1")]);
    assert!(status >= 400 && said.contains("list of runner images could not be read"), "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A name checked against the other CIs in the account: a control plane there that is not a Lambda function (one
/// elsewhere, whose machines start there) has no name to clash with.
#[test]
fn a_name_is_checked_only_against_the_accounts_own_control_planes() {
    let served = serve(|_| {});
    let role = |name: &str, description: &str| format!("<member><Path>/</Path><RoleName>{name}</RoleName><RoleId>AROAEXAMPLE000000001</RoleId><Arn>arn:aws:iam::111111111111:role/{name}</Arn><CreateDate>2026-10-18T10:00:00Z</CreateDate><Description>{description}</Description><MaxSessionDuration>3600</MaxSessionDuration></member>");
    let roles = [role(&format!("superci-plane-{}", superci_testkit::PLANE), &format!("Runs the SuperCI control plane {} (Lambda, us-east-1, runs-on superci).", served.url)),
        role("superci-plane-cfplane00001", "Lets the SuperCI control plane https://superci.acme.workers.dev start machines in this account.")].concat();
    served.aws.fail("ListRoles", 200, &format!("<ListRolesResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\"><ListRolesResult><IsTruncated>false</IsTruncated><Roles>{roles}</Roles></ListRolesResult><ResponseMetadata><RequestId>00000000-0000-4000-8000-000000000001</RequestId></ResponseMetadata></ListRolesResponse>"), None);
    let home = common::signed_in(&served);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/name", &[("name", "Acme CI")]);
    assert!(status == 303, "{status} {said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Cloudflare's runners for a control plane in AWS go into the account named when the sign-in reaches it; and an
/// update of that control plane whose Cloudflare launcher Cloudflare will not take says so.
#[test]
fn cloudflare_runners_go_into_the_account_named_and_an_update_says_a_launcher_cloudflare_did_not_take() {
    let served = serve(move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(own_role())));
    served.aws.put(&format!("/superci/{}/LAUNCHERS", superci_testkit::PLANE), r#"[{"cloud":"cloudflare","url":"https://superci-runners-testplane0001.acme.workers.dev"}]"#);
    served.aws.put(&format!("/superci/{}/CONTAINERS", superci_testkit::PLANE), "off");
    let home = common::signed_in(&served);
    kept_as(&home, |k| k["cloudflare"] = cloudflare_signed_in());
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    served.aws.fail("/client/v4/accounts?per_page", 200, r#"{"success":true,"errors":[],"messages":[],"result":[{"id":"1111111111111111111111111111111a","name":"Acme"},{"id":"2222222222222222222222222222222b","name":"Globex"}]}"#, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    // (The accounts the sign-in reaches, read as one is set up in an account it does not.)
    let (status, said) = sent(&link, "/plane/cloudflare", &[("account", "3333333333333333333333333333333c")]);
    assert!(status >= 400 && said.contains("the sign-in cannot reach that account"), "{status} {said}");
    let (status, said) = sent(&link, "/runners/cloudflare", &[("account", "1111111111111111111111111111111a")]);
    assert!(status >= 400, "deployed nowhere here: {status} {said}");
    let (status, said) = sent(&link, "/plane/update", &[("plane", superci_testkit::PLANE)]);
    assert!(status == 303, "{status} {said}");
    // (Its launcher tried a second time: by the update, after the runners added above.)
    let tried = || served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/workers/scripts/superci-runners-testplane0001/settings"))).count() >= 2;
    until("its launcher tried", tried);
    // (Its update ends saying so, a moment after.)
    std::thread::sleep(std::time::Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(&home);
}

/// AWS's runners for a control plane in Cloudflare: the role made, the control plane handed what it assumes it with,
/// and its answer awaited (connected; not yet able to use it, said; or settings Cloudflare would not keep, said).
#[test]
fn aws_runners_for_a_control_plane_in_cloudflare_connect_or_say_what_stopped_them() {
    for (callback, refused) in [(Some(r#"{"connected":true}"#), None), (Some(r#"{"connected":false,"error":"AccessDenied: not yet"}"#), None), (None, Some("AWS_CONNECT")), (None, Some("AWS_REGIONS"))] {
        let served = serve(|_| {});
        let plane = match callback {
            Some(said) => { let said = said.to_string(); superci_testkit::front(&served.url, move |method, target, status, body| if method == "POST" && target.starts_with("/aws/callback") { (200, said.clone().into_bytes()) } else { (status, body) }) }
            None => served.url.clone(),
        };
        *served.aws.plane_url.lock().unwrap() = plane.clone();
        let home = common::signed_in(&served);
        as_cloudflare(&home, &served);
        kept_as(&home, |k| { k["plane"]["Cloudflare"]["url"] = plane.clone().into(); k["cloudflare"] = cloudflare_signed_in(); });
        // (Its role and network, as AWS answered them for a control plane in Cloudflare.)
        for region in ["us-east-1", "us-east-2", "us-west-2"] { served.aws.replaying(include_str!("../fixtures/runners/08-move.jsonl"), &[("http://new-plane.test", &plane), ("us-east-1", region)]) }
        if let Some(name) = refused { served.aws.fail(&format!("/secrets|{name}"), 500, CF_FAILED, None) }
        let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
        let (status, said) = sent(&link, "/aws/connect", &[("region", "us-east-1")]);
        let want = match (callback, refused) { (Some(c), _) if c.contains("true") => "", (Some(_), _) => "the control plane could not use it yet", _ => "Internal server error" };
        assert!(if want.is_empty() { status == 303 } else { status >= 400 && said.contains(want) }, "{callback:?} {refused:?}: {status} {said}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// A control plane in Cloudflare whose machines start in the AWS account signed in to: its update gives its role
/// there what this version asks for, and says so to the control plane.
#[test]
fn an_update_of_a_control_plane_in_cloudflare_gives_its_aws_role_what_it_asks_for() {
    let served = serve(|_| {});
    let cf = superci_testkit::serve_as("cfplane00003", move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(own_role())));
    let home = common::signed_in(&served);
    let url = cf.url.clone();
    kept_as(&home, |k| {
        k["plane"] = serde_json::json!({ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci", "url": url, "plane_id": "cfplane00003", "label": "superci" } });
        k["key"]["planes"] = serde_json::json!(["cfplane00003"]);
        k["cloudflare"] = cloudflare_signed_in();
    });
    served.aws.replaying(include_str!("../fixtures/update/05-update-cf.jsonl"), &[("http://new-plane.test", &cf.url)]);
    // (Its role, as AWS answered for a control plane in Cloudflare.)
    served.aws.replaying(include_str!("../fixtures/runners/08-move.jsonl"), &[("http://new-plane.test", &cf.url)]);
    cf.store.0.lock().unwrap().insert("denied:aws:ec2:RunInstances".into(), r#"{"at":1}"#.into());
    // (Its address as Cloudflare gives it after the deploy, served here.)
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let (status, said) = sent(&link, "/plane/update", &[("plane", "cfplane00003")]);
    assert!(status == 303, "{status} {said}");
    let given = || served.aws.asked.lock().unwrap().iter().any(|a| a["status"] == 200 && a["asked"].as_str().is_some_and(|s| s.contains("PutRolePolicy")));
    until("its role given what it asks for", given);
    // (Then the control plane is told its role has it: what it noted AWS refusing goes.)
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !cf.store.under("denied:aws:").is_empty() {
        let last: Vec<String> = served.aws.asked.lock().unwrap().iter().rev().take(6).map(|a| format!("{} {} {}", a["status"], a["url"], a["asked"].as_str().unwrap_or_default().chars().take(80).collect::<String>())).collect();
        assert!(std::time::Instant::now() < end, "never told: {last:#?}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// Cloudflare's own containers for a control plane in Cloudflare that had a launcher there from before: the launcher's
/// place given up, which Cloudflare will not keep: said.
#[test]
fn containers_that_replace_a_launcher_from_before_say_when_cloudflare_will_not_keep_that() {
    let served = serve(|_| {});
    let aws = served.aws.clone();
    let cf = superci_testkit::serve_as("cfplane00002", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        c.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
    });
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    served.aws.worker_secrets.lock().unwrap().entry("superci".into()).or_default().insert("LAUNCHERS".into(), r#"[{"cloud":"cloudflare","url":"https://superci-runners-cfplane00002.acme.workers.dev"}]"#.into());
    let home = common::signed_in(&served);
    kept_as(&home, |k| {
        k["plane"] = serde_json::json!({ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci", "url": "https://superci.acme.workers.dev", "plane_id": "cfplane00002", "label": "superci" } });
        k.as_object_mut().unwrap().remove("key");
        k["cloudflare"] = cloudflare_signed_in();
    });
    served.aws.replaying(include_str!("../fixtures/move/03-cf-containers.jsonl"), &[]);
    // (One from before settings were written through it: its launchers' setting refused by Cloudflare; the deploy's
    // upload, which names them too, taken.)
    cf.answer_as("0.19.21", &["settings", "settings_version"]);
    served.aws.fail("scripts/superci/secrets|LAUNCHERS", 500, CF_FAILED, None);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    // (Its key handed as a page is first read.)
    until("its key handed", || { common::part(&link, "/?p=runners&fragment=1"); served.aws.worker_settings("superci").iter().any(|(k, _)| k.starts_with("DEVICE_KEY_")) });
    let (status, said) = sent(&link, "/runners/cloudflare", &[]);
    assert!(status >= 400 && said.contains("Internal server error"), "{status} {said}");
    // (Refused at the launcher's place given up: its containers were switched on before it.)
    assert!(served.aws.worker_settings("superci").iter().any(|(k, v)| k == "CONTAINERS" && v == "on"), "{said}");
    // One that keeps its settings, its store refusing them (the launcher listed again): refused the same, the launcher
    // still listed.
    cf.answer_as("", &[]);
    served.aws.worker_secrets.lock().unwrap().entry("superci".into()).or_default().insert("LAUNCHERS".into(), r#"[{"cloud":"cloudflare","url":"https://superci-runners-cfplane00002.acme.workers.dev"}]"#.into());
    cf.store_refuses(Some(superci_core::plane::SETTINGS));
    let (status, said) = sent(&link, "/runners/cloudflare", &[]);
    assert!(status >= 400 && said.contains("the control plane answered 500"), "{status} {said}");
    assert!(served.aws.worker_settings("superci").iter().any(|(k, v)| k == "LAUNCHERS" && v.contains("cloudflare")));
    let _ = std::fs::remove_dir_all(&home);
}

/// What a control plane's AWS role lacks, read while the control plane does not give its status: with the regions it
/// is known by. And the account looked at with credentials AWS ended early, while AWS's sign-in does not renew them:
/// said.
#[test]
fn an_aws_role_and_account_are_read_while_the_control_plane_or_aws_do_not_answer_as_asked() {
    let served = serve(move |c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(own_role())));
    let plane = superci_testkit::front(&served.url, |method, target, status, body| if method == "GET" && target.starts_with("/status") { (500, br#"{"error":"busy"}"#.to_vec()) } else { (status, body) });
    *served.aws.plane_url.lock().unwrap() = plane.clone();
    let home = common::signed_in(&served);
    common::kept_at(&home, &plane);
    let (_, said) = common::superci_in_time(&home, &["planes", "allow", superci_testkit::PLANE, "--dry-run"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(said.contains(superci_testkit::PLANE) || said.contains("AWS"), "{said}");
    // AWS saying the credentials are over before they said they would be, and its sign-in not answering.
    kept_as(&home, |k| {
        k["aws"]["refresh_token"] = "superci-refresh".into();
        k["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    });
    served.aws.fail("ec2.", 400, &superci_testkit::ec2_error("ExpiredToken", "The security token included in the request is expired"), None);
    served.aws.fail("signin.aws.amazon.com/v1/token", 500, r#"{"error":"server_error"}"#, None);
    let (ok, said) = common::superci_in_time(&home, &["accounts", "retrieve", "aws"], &[("SUPERCI_PAUSE_MS", "1")], 60);
    assert!(!ok && said.contains("server_error"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A page drawn, then a teammate changing a setting on another computer, then the page's change: a field the page
/// showed and the teammate changed is refused, saying what it is now; another field is taken beside the teammate's;
/// a command, which showed nothing, is meant as said.
#[test]
fn a_setting_changed_on_another_computer_since_the_page_showed_it_is_refused_and_another_is_kept() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    // What the page showed, as each of its routing forms carries it.
    let page = common::part(&link, "/?p=settings&fragment=1").1;
    let form = &page[page.find(r#"action="/routing""#).expect("a routing form")..];
    let value = |name: &str| form.split(&format!(r#"name="{name}" value=""#)).nth(1).and_then(|v| v.split('"').next()).unwrap_or_default()
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    let (seen, plane) = (value("seen"), value("seen_plane"));
    assert!(seen.starts_with('{') && plane == superci_testkit::PLANE, "{seen} {plane}");
    // The teammate's change, through the control plane.
    let teammate = serde_json::json!({ "changes": [{ "op": "set", "path": ["routing", "max_cpu"], "value": 4 }] });
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let r = agent.post(&format!("{}/settings", served.url)).header("authorization", &format!("Bearer {}", served.key)).send(teammate.to_string()).unwrap();
    assert_eq!(r.status(), 200);
    let routing = || served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| serde_json::from_str::<serde_json::Value>(&v).unwrap()).unwrap();
    let (status, said) = sent(&link, "/routing", &[("action", "max_cpu"), ("max_cpu", "16"), ("seen", &seen), ("seen_plane", &plane)]);
    assert_eq!(status, 409, "{said}");
    assert!(said.contains("Changed on another computer meanwhile") && said.contains("the largest machine is 4 now"), "{said}");
    assert_eq!(routing()["max_cpu"], 4);
    let (status, said) = sent(&link, "/routing", &[("action", "max_hours"), ("max_hours", "3"), ("seen", &seen), ("seen_plane", &plane)]);
    assert_eq!(status, 303, "{said}");
    assert_eq!((&routing()["max_cpu"], &routing()["max_minutes"]), (&serde_json::json!(4), &serde_json::json!(180)));
    // What another control plane's page showed is not taken for this one's: the change is meant as said.
    let (status, said) = sent(&link, "/routing", &[("action", "max_cpu"), ("max_cpu", "16"), ("seen", &seen), ("seen_plane", "otherplane01")]);
    assert_eq!(status, 303, "{said}");
    assert_eq!(routing()["max_cpu"], 16);
    let _ = std::fs::remove_dir_all(&home);
}
