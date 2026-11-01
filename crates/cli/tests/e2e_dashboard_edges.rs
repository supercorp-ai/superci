//! The dashboard's edges, as they come about: two SuperCIs on one folder (a sign-in renewed or ended by the other
//! while this one waited for it), a sign-in coming back while a look at the account is under way, a dashboard with
//! no folder to keep anything in, a port it may not have, and the pages after a move or beside a GitLab's jobs.
mod common;
use superci_testkit::{serve, serve_as, PLANE};

/// Waits up to 15 seconds for `holds`, else fails saying `what`.
fn until(what: &str, holds: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !holds() { assert!(std::time::Instant::now() < end, "waited for {what}"); std::thread::sleep(std::time::Duration::from_millis(100)) }
}

fn sign_ins(home: &std::path::Path) -> std::path::PathBuf { home.join("workspaces/default/sign-ins.json") }

fn read(file: &std::path::Path) -> serde_json::Value { serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap() }

/// The workspace's sign-ins changed as `change` says.
fn kept_as(home: &std::path::Path, change: impl Fn(&mut serde_json::Value)) {
    let file = sign_ins(home);
    let mut k = read(&file);
    change(&mut k);
    std::fs::write(&file, k.to_string()).unwrap();
}

/// Held as another SuperCI holds it while it renews a sign-in (see `Store::renewing`): let go when dropped.
fn renewing(home: &std::path::Path) -> std::fs::File {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("workspaces/default/.renewing")).unwrap();
    f.lock().unwrap();
    f
}

/// What the stand-in for AWS (and Cloudflare) was asked at an address holding `part`.
fn asked_at(served: &superci_testkit::Served, part: &str) -> usize { served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.contains(part))).count() }

/// A dashboard on Cloudflare's own port (its sign-in comes back to no other), with these settings of its own: what it
/// answers to a GET as a page asks (its cookie on that port the usual one), and the dashboard.
fn on_cloudflares_port(home: &std::path::Path, envs: &[(&str, &str)]) -> (common::Dashboard, impl Fn(&str) -> (u16, String, String)) {
    let port = superci_testkit::free_port().to_string();
    let (child, link) = common::dashboard_with(home, &[&[("SUPERCI_OAUTH_PORT", port.as_str()), ("SUPERCI_PORT", port.as_str())][..], envs].concat());
    let key = link.split_once("/?k=").unwrap().1.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let get = move |path: &str| {
        let mut r = agent.get(&format!("http://127.0.0.1:{port}{path}")).header("cookie", &format!("superci_local={key}")).header("sec-fetch-site", "same-origin").call().unwrap();
        let to = r.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        (r.status().as_u16(), to, r.body_mut().read_to_string().unwrap_or_default())
    };
    (child, get)
}

/// Signs in to Cloudflare through that dashboard, as the browser does.
fn cloudflare_sign_in(get: &impl Fn(&str) -> (u16, String, String)) {
    let (_, to, said) = get("/connect/cloudflare");
    let to = if to.is_empty() { said.split("https://dash.cloudflare.com/oauth2/auth?").nth(1).map(|r| format!("https://dash.cloudflare.com/oauth2/auth?{}", r.split(['"', '\'']).next().unwrap().replace("&amp;", "&"))).unwrap_or_default() } else { to };
    let state = to.split("state=").nth(1).and_then(|r| r.split('&').next()).expect("a state").to_string();
    let (status, _, done) = get(&format!("/oauth/callback?code=a-code-from-cloudflare&state={state}"));
    assert!(status < 400 && common::words(&done).contains("Signed in with Cloudflare"), "{status} {}", common::words(&done));
}

/// Cloudflare's answer to a sign-in, as recorded, its token good for `secs` and renewed with `refresh`.
fn cloudflare_signs_in_for(secs: u64, refresh: &str) -> String {
    include_str!("../fixtures/cloudflare/sign-in/sign-in.jsonl").replace("\\\"expires_in\\\":3600", &format!("\\\"expires_in\\\":{secs}")).replace("example-refresh_token-1", refresh)
}

/// AWS's sign-in renewing with `refresh`: credentials good for `secs`, renewed next with `next`; or, `secs` none,
/// refused as it refuses one that has ended.
fn aws_renews(refresh: &str, answer: Option<(u64, &str)>) -> String {
    let asked = serde_json::json!({ "clientId": "arn:aws:signin:::devtools/same-device", "grantType": "refresh_token", "refreshToken": refresh }).to_string();
    let (status, answer) = match answer {
        Some((secs, next)) => (200, serde_json::json!({ "accessToken": { "accessKeyId": "example-accessKeyId-9", "secretAccessKey": "example-secretAccessKey-9", "sessionToken": "example-sessionToken-9" },
            "tokenType": "urn:aws:params:oauth:token-type:access_token_sigv4", "expiresIn": secs, "refreshToken": next })),
        None => (400, serde_json::json!({ "error": "INVALID_REQUEST", "message": "The provided authorization grant is invalid, expired, revoked, or malformed" })),
    };
    serde_json::json!({ "answer": answer.to_string(), "asked": asked, "headers": [["content-type", "application/json"]], "method": "POST", "status": status, "target": "", "url": "https://us-east-1.signin.aws.amazon.com/v1/token" }).to_string() + "\n"
}

/// The workspace's AWS sign-in with its credentials over (renewed at the next use, with `refresh`).
fn aws_due(home: &std::path::Path, refresh: &str) {
    kept_as(home, |k| {
        k["aws"]["creds"]["expires_at_ms"] = 0.into();
        k["aws"]["refresh_token"] = refresh.into();
        k["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    });
}

/// Another SuperCI on the same folder finds its AWS sign-in ended while this one signs in to AWS: this one's sign-in
/// stands, kept as it came, and the other's end is not taken for it.
#[test]
fn an_aws_sign_in_made_while_another_superci_finds_its_own_ended_stays_signed_in() {
    let served = serve(|_| {});
    served.aws.replaying(&[include_str!("../fixtures/aws/sign-in/sign-in.jsonl"), include_str!("../fixtures/aws/sign-in/renew.jsonl")].concat(), &[]);
    let home = common::home(&served);
    std::fs::write(home.join("aws-at"), &served.aws.url).unwrap();
    let (_child, link) = common::dashboard(&home);
    let (_, to, page) = common::post(&link, "/aws/signin", &[("next", ""), ("w", "default")]);
    let at = if to.is_empty() { page } else { to };
    let state = at.split("state=").nth(1).and_then(|r| r.split(['&', '"']).next()).expect("a state").to_string();
    // (While AWS finishes this one's sign-in, the other writes that its own has ended.)
    let (file, once) = (sign_ins(&home), std::sync::atomic::AtomicBool::new(false));
    served.aws.during("signin.aws.amazon.com/v1/token", move || {
        if once.swap(true, std::sync::atomic::Ordering::SeqCst) { return }
        let mut k = read(&file);
        k["aws_ended"] = true.into();
        std::fs::write(&file, k.to_string()).unwrap();
    });
    let (status, _, done) = common::get(&link, &format!("/oauth/callback?code=a-code-from-aws&state={state}"));
    assert!(status < 400 && common::words(&done).contains("Signed in with AWS"), "{status} {}", common::words(&done));
    let kept = read(&sign_ins(&home));
    assert!(kept["aws"]["account_id"] == "111111111111" && kept.get("aws_ended").is_none(), "{kept}");
    let page = common::words(&common::part(&link, "/?p=overview&fragment=1").1);
    assert!(!page.contains("Your AWS sign-in has ended"), "{page}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Another SuperCI renews the AWS sign-in while this one waits to renew it too: this one takes what the other renewed
/// and does not renew it again (AWS takes what renews a sign-in once).
#[test]
fn an_aws_sign_in_another_superci_renewed_meanwhile_is_taken_and_not_renewed_again() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    aws_due(&home, "superci-refresh-the-other-one-used");
    let (_child, link) = common::dashboard(&home);
    let held = renewing(&home);
    let l = link.clone();
    let asking = std::thread::spawn(move || common::post(&l, "/aws/look", &[("next", "?p=planes")]));
    // (This one waits for the other, which renews and writes, then lets go.)
    std::thread::sleep(std::time::Duration::from_secs(2));
    kept_as(&home, |k| k["aws"]["creds"]["expires_at_ms"] = 4102444800000u64.into());
    drop(held);
    let (status, _, said) = asking.join().unwrap();
    assert!(status == 303, "{status} {}", common::words(&said));
    assert_eq!(asked_at(&served, "signin.aws.amazon.com"), 0);
    assert_eq!(read(&sign_ins(&home))["aws"]["creds"]["expires_at_ms"], 4102444800000u64);
    let page = common::words(&common::part(&link, "/?p=overview&fragment=1").1);
    assert!(!page.contains("Your AWS sign-in has ended"), "{page}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Another SuperCI finds the Cloudflare sign-in ended while this one waits to renew it: this one does not renew it
/// (Cloudflare would refuse what renews it), and says that it has expired.
#[test]
fn a_cloudflare_sign_in_another_superci_found_ended_meanwhile_is_not_renewed_here() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    kept_as(&home, |k| k["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 0, "refresh": "cloudflare-refresh-the-other-one-used" }));
    let (_child, link) = common::dashboard(&home);
    let held = renewing(&home);
    let l = link.clone();
    let asking = std::thread::spawn(move || common::part(&l, "/?p=overview&fragment=1"));
    std::thread::sleep(std::time::Duration::from_secs(2));
    kept_as(&home, |k| { k.as_object_mut().unwrap().remove("cloudflare"); k["cloudflare_ended"] = true.into(); });
    drop(held);
    let (status, _) = asking.join().unwrap();
    assert_eq!(status, 200);
    assert_eq!(asked_at(&served, "dash.cloudflare.com/oauth2/token"), 0);
    until("said as expired", || common::words(&common::part(&link, "/?p=overview&fragment=1").1).contains("Your Cloudflare sign-in has expired"));
    let _ = std::fs::remove_dir_all(&home);
}

/// With no folder to keep anything in (no home), a Cloudflare sign-in whose token runs out within minutes is renewed
/// as it is used, and nothing is written anywhere.
#[test]
fn a_cloudflare_sign_in_on_a_computer_with_no_home_folder_is_renewed_as_it_is_used() {
    let served = serve(|_| {});
    served.aws.replaying(&[include_str!("../fixtures/cloudflare/01-look.jsonl"), include_str!("../fixtures/cloudflare/sign-in/renew.jsonl"), &cloudflare_signs_in_for(60, "example-refresh_token-1")].concat(), &[]);
    let home = common::temp("no-home");
    std::fs::write(home.join("aws-at"), &served.aws.url).unwrap();
    let (_child, get) = on_cloudflares_port(&home, &[("SUPERCI_HOME", ""), ("HOME", "")]);
    cloudflare_sign_in(&get);
    until("its accounts read", || { get("/?p=overview&fragment=1"); asked_at(&served, "/client/v4/accounts?per_page=50") > 0 });
    let renewals = served.aws.asked.lock().unwrap().iter().filter(|a| a["url"] == "https://dash.cloudflare.com/oauth2/token" && a["asked"].as_str().is_some_and(|s| s.contains("grant_type=refresh_token"))).count();
    assert_eq!(renewals, 1);
    assert!(!home.join("workspaces").exists());
    let _ = std::fs::remove_dir_all(&home);
}

/// A Cloudflare sign-in made in the browser beside a token given in SUPERCI_CLOUDFLARE_TOKEN, which Cloudflare then
/// will not renew: said as Cloudflare refusing the token this run was given, and not forgotten as a kept sign-in is.
#[test]
fn a_cloudflare_sign_in_beside_a_given_token_that_cloudflare_will_not_renew_is_said_as_refused() {
    let served = serve(|_| {});
    served.aws.replaying(&[include_str!("../fixtures/cloudflare/sign-in/ended.jsonl"), &cloudflare_signs_in_for(60, "example-refresh_token-ended")].concat(), &[]);
    let home = common::signed_in(&served);
    let (_child, get) = on_cloudflares_port(&home, &[("SUPERCI_CLOUDFLARE_TOKEN", "cloudflare-token-given-for-tests")]);
    cloudflare_sign_in(&get);
    let page = common::words(&get("/?p=overview&fragment=1").2);
    assert!(page.contains("Cloudflare refuses the token in SUPERCI_CLOUDFLARE_TOKEN"), "{page}");
    assert!(read(&sign_ins(&home)).get("cloudflare_ended").is_none());
    let _ = std::fs::remove_dir_all(&home);
}

/// A CI found in the AWS account as the AWS sign-in ends (renewed once, refused the next time): listed, and AWS is not
/// asked what it is called without a sign-in.
#[test]
fn a_ci_found_as_the_aws_sign_in_ends_is_listed_without_asking_aws_about_it() {
    let role = |name: &str, description: &str| format!("<member><Path>/</Path><RoleName>{name}</RoleName><RoleId>AROAEXAMPLE{:09}</RoleId><Arn>arn:aws:iam::111111111111:role/{name}</Arn><CreateDate>2026-10-18T10:00:00Z</CreateDate><Description>{description}</Description><MaxSessionDuration>3600</MaxSessionDuration></member>", name.len());
    let served = serve(|_| {});
    served.connect_github();
    let other = serve_as("otherci000001", |_| {});
    other.connect_github();
    served.aws.fail("ListRoles", 200, &format!("<ListRolesResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\"><ListRolesResult><IsTruncated>false</IsTruncated><Roles>{}{}</Roles></ListRolesResult><ResponseMetadata><RequestId>00000000-0000-4000-8000-000000000001</RequestId></ResponseMetadata></ListRolesResponse>",
        role(&format!("superci-plane-{PLANE}"), &format!("Runs the SuperCI control plane {} (Lambda, us-east-1, runs-on superci).", served.url)),
        role("superci-plane-otherci000001", &format!("Runs the SuperCI control plane {} (Lambda, us-east-1, runs-on superci).", other.url))), None);
    served.aws.replaying(&[aws_renews("superci-refresh-short-for-tests", Some((60, "superci-refresh-ended-for-tests"))), aws_renews("superci-refresh-ended-for-tests", None)].concat(), &[]);
    let home = common::signed_in(&served);
    aws_due(&home, "superci-refresh-short-for-tests");
    let (_child, link) = common::dashboard(&home);
    // (Not named: what it is called was not read.)
    until("the other CI listed", || common::words(&common::part(&link, "/?p=planes&fragment=1").1).contains("A CI (otherci000001)"));
    assert!(served.aws.asked.lock().unwrap().iter().all(|a| !a["asked"].as_str().unwrap_or_default().contains("otherci000001/NAME")));
    let _ = std::fs::remove_dir_all(&home);
}

/// Signed out while a look at the AWS account is under way, AWS then calling its credentials expired: the look that
/// comes back signs nothing in again, and renews nothing.
#[test]
fn a_look_at_aws_that_comes_back_expired_after_signing_out_signs_nothing_in_again() {
    let served = serve(|_| {});
    served.aws.fail("DescribeVpcs", 400, &superci_testkit::ec2_error("RequestExpired", "Request has expired."), None);
    let home = common::signed_in(&served);
    let (_child, link) = common::dashboard(&home);
    let (l, once) = (link.clone(), std::sync::atomic::AtomicBool::new(false));
    served.aws.during("DescribeVpcs", move || {
        if once.swap(true, std::sync::atomic::Ordering::SeqCst) { return }
        let (l, (tx, rx)) = (l.clone(), std::sync::mpsc::channel());
        std::thread::spawn(move || { let _ = tx.send(common::post(&l, "/signout", &[]).0); });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
    });
    common::part(&link, "/?p=planes&fragment=1");
    until("signed out", || !sign_ins(&home).exists());
    common::part(&link, "/?p=planes&fragment=1");
    assert!(!sign_ins(&home).exists());
    assert_eq!(asked_at(&served, "signin.aws.amazon.com"), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// A port that is not this program's to take (one under 1024, for a person who is not root): said, and nothing is
/// served.
#[test]
fn a_port_the_dashboard_may_not_take_is_said() {
    match std::net::TcpListener::bind("127.0.0.1:80") {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        _ => { eprintln!("port 80 can be taken here (or is taken): skipped"); return }
    }
    let served = serve(|_| {});
    let home = common::home(&served);
    let (ok, said) = common::superci_in_time(&home, &["dashboard", "--no-browser", "--workspace=default"], &[("SUPERCI_PORT", "80")], 30);
    assert!(!ok && said.contains("port 80 is in use"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// After a move has ended, the control plane it moved to is no longer one "a move is under way" to: deleting the old
/// one is weighed as any other (two jobs still running there, it is not deleted).
#[test]
fn after_a_move_ends_deleting_the_old_control_plane_is_weighed_as_any_other() {
    let served = serve(|_| {});
    served.connect_github();
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let aws = served.aws.clone();
    let new = serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    let home = common::signed_in(&served);
    kept_as(&home, |k| {
        k["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": new.url, "plane_id": "testplane0002", "label": "superci" } }]);
        k["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    });
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    let (_child, link) = common::dashboard_with(&home, &[("SUPERCI_MOVE_PAUSE_MS", "50")]);
    until("both listed", || common::words(&common::part(&link, "/?p=planes&fragment=1").1).contains(&new.url));
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", common::words(&said));
    until("the move ended", || common::words(&common::part(&link, "/?p=planes&fragment=1").1).contains("send their jobs there now"));
    for id in [93u64, 94] {
        served.store.0.lock().unwrap().insert(format!("job:{id}"), serde_json::json!({ "job_id": id, "run_id": 9, "repo": "acme/app", "state": "running", "cloud": "", "provider": "local",
            "at_ms": served.now_ms() - 60_000, "installation_id": 0, "seen_in_progress": true }).to_string());
    }
    let (status, _, said) = common::post(&link, "/plane/delete", &[("plane", PLANE)]);
    assert!(status == 409 && common::words(&said).contains("2 jobs are still running there"), "{status} {}", common::words(&said));
    let _ = std::fs::remove_dir_all(&home);
}

/// Containers switched on for a Cloudflare control plane that shows them a moment after its Worker was given them
/// (as a Worker takes a new secret): waited for, then said as done.
#[test]
fn cloudflare_containers_a_control_plane_shows_a_moment_late_are_waited_for() {
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    // (Its first two answers after containers were switched on are from before.)
    let late = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let l = late.clone();
    let cf = serve_as("cfplane00001", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        let on = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
        c.containers = on && l.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2;
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00001/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    let home = common::signed_in(&served);
    kept_as(&home, |k| k["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" }));
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/cloudflare/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", "cfplane00001").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    step("02-deploy");
    let (ok, said) = run(&["planes", "create", "cloudflare"]);
    assert!(ok, "{said}");
    step("03-use");
    step("04-containers");
    let (ok, said) = run(&["runners", "create", "cloudflare", "--plane=cfplane00001"]);
    assert!(ok && said.contains("cloudflare runs jobs now."), "{said}");
    assert!(late.load(std::sync::atomic::Ordering::SeqCst) > 2);
    let _ = std::fs::remove_dir_all(&home);
}

/// A run's page lists the jobs of its own attempt only: a GitLab pipeline's job with the same numbers (the same
/// project path, pipeline and job id) and another attempt is not among them.
#[test]
fn a_runs_page_lists_only_its_attempts_jobs_beside_a_gitlab_job_with_the_same_numbers() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let (id, queued) = served.github.queue("test", "superci", served.now_ms());
    assert_eq!(served.webhook(queued), 202);
    let job: serde_json::Value = served.store.under(&format!("job:{id}"))[0].1.clone();
    let run = job["run_id"].as_u64().unwrap();
    {
        let mut j = job.clone();
        let at = job["at_ms"].as_u64().unwrap() + 1000;
        (j["provider"], j["gitlab"], j["attempt"], j["at_ms"]) = ("gitlab".into(), "".into(), 2.into(), at.into());
        let mut m = served.store.0.lock().unwrap();
        m.insert(format!("job:gl{id}"), j.to_string());
        m.insert(format!("recent:{at:013}:gl{id}"), String::new());
    }
    let (_child, link) = common::dashboard(&home);
    let page = |attempt: u64| common::words(&common::part(&link, &format!("/?p=run&repo=acme%2Fapp&id={run}&attempt={attempt}&fragment=1")).1);
    until("both attempts listed", || page(2).contains("Jobs here 1"));
    assert!(page(1).contains("Jobs here 1"), "{}", page(1));
    let _ = std::fs::remove_dir_all(&home);
}
