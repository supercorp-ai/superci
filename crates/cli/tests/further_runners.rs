//! A further runner from the CLI and the dashboard: a second Modal workspace signed in to, its launcher deployed as Modal
//! answered a real one (`fixtures/modal/further`, recorded 2026-11-02 with a Modal control plane in one workspace and a
//! further runner in another, all deleted after), and everything after it under its own word.
mod common;
use common::{part, words};
use superci_testkit::{serve, PLANE};

/// A control plane in AWS with a Modal launcher in `acmeorg`, and this computer signed in to `acmeorg` and `acme-two`.
fn two_workspaces() -> (superci_testkit::Served, std::path::PathBuf) {
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &format!(r#"[{{"cloud":"modal","url":"https://acmeorg--superci-runners-{PLANE}.modal.run"}}]"#));
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let session = |w: &str| serde_json::json!({ "token_id": format!("ak-{w}"), "token_secret": format!("as-{w}"), "workspace": w });
    kept["modal"] = session("acmeorg");
    kept["modal_more"] = serde_json::json!([session("acme-two")]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let mut w = served.aws.workers.lock().unwrap();
    for workspace in ["acmeorg", "acme-two"] { w.insert(format!("{workspace}--superci-runners-{PLANE}.modal.run"), served.url.clone()); }
    drop(w);
    (served, home)
}

/// Modal answering one deploy of a launcher, as it answered a real one.
fn deploy_answers(served: &superci_testkit::Served) {
    // (No app of its control plane's listed: this one is served here.)
    let deployed: Vec<&str> = include_str!("../fixtures/modal/further/01-add.jsonl").lines().filter(|l| !l.contains("/AppList\"")).collect();
    served.aws.replaying(include_str!("../fixtures/runners/04-modal-agent.jsonl").lines().find(|l| l.contains("/AppList\"")).unwrap(), &[]);
    served.aws.replaying(&deployed.join("\n"), &[("mdplane00002", PLANE)]);
}

fn stored(served: &superci_testkit::Served) -> serde_json::Value {
    serde_json::from_str(&served.store.0.lock().unwrap()["settings"]).unwrap()
}

#[test]
fn a_second_modal_workspace_gets_a_runner_of_its_own_word_kept_through_deleting_it() {
    let (served, home) = two_workspaces();
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    deploy_answers(&served);
    // A workspace not signed in to: said, nothing deployed.
    let (ok, said) = run(&["runners", "create", "modal", "--account=globex"]);
    assert!(!ok && said.contains("Sign in with Modal to the workspace globex first: `superci login modal --add`."), "{said}");
    // Words that cannot be its own, before anything is deployed.
    for (name, why) in [("Bad_Word", "a word is 2 to 32"), ("cloudflare", "another runner's word")] {
        let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two", &format!("--name={name}")]);
        assert!(!ok && said.contains(why), "{name}: {said}");
    }
    // Offered on the dashboard's Add page, with the word it would have.
    let (mut child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=add-runners&fragment=1").1) }).find(|p| p.contains("as the runner")).unwrap_or_default();
    assert!(page.contains("in the Modal workspace acme-two, as the runner modal-acme-two"), "{page}");
    let _ = child.kill();
    // Its launcher in its own workspace, under the provider and the workspace's name.
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok && said.contains("modal-acme-two runs jobs now."), "{said}");
    let kept = stored(&served);
    assert_eq!(kept["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two" }, { "name": "modal-acme-two-functions", "account": "modal-acme-two", "function": true }]), "{kept}");
    assert_eq!(kept["accounts"], serde_json::json!([{ "name": "modal-acme-two", "provider": "modal", "cloud_id": "acme-two" }]), "{kept}");
    assert!(kept["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two" && a["url"].as_str().unwrap().starts_with("https://acme-two--")), "{kept}");
    // Listed, and changed, by its word; the first stays as it was.
    let (ok, said) = run(&["runners", "update", "modal-acme-two", "--max-jobs=3", "--monthly-usd=25"]);
    assert!(ok && said.contains("modal-acme-two"), "{said}");
    let order = stored(&served)["routing"]["order"].clone();
    assert!(order.as_array().unwrap().iter().any(|p| p["cloud"] == "modal-acme-two" && p["max_jobs"] == 3 && p["monthly_usd"] == 25.0), "{order}");
    let (ok, said) = run(&["runners", "order", "modal-acme-two", "modal"]);
    assert!(ok && said.contains("Order: modal-acme-two, modal."), "{said}");
    // Its account's limits, by the account's word; AWS's own budget is AWS's alone.
    let (ok, said) = run(&["accounts", "update", "modal-acme-two", "--monthly-usd=100", "--daily-usd=10"]);
    assert!(ok && said.contains("modal-acme-two spent $0.00 on machines this month, of $100 allowed.") && said.contains("$10 allowed"), "{said}");
    let (ok, said) = run(&["accounts", "update", "modal", "--monthly-usd=50"]);
    assert!(ok && said.contains("The Modal account spent $0.00 on machines this month, of $50 allowed."), "{said}");
    let (ok, said) = run(&["limits", "retrieve"]);
    assert!(ok && said.contains("The account modal-acme-two spent $0.00") && said.contains("superci accounts update modal-acme-two"), "{said}");
    let kept = stored(&served);
    assert_eq!((&kept["routing"]["account_usd"]["modal-acme-two"], &kept["routing"]["account_daily_usd"]["modal-acme-two"]), (&serde_json::json!(100.0), &serde_json::json!(10.0)), "{kept}");
    for (args, why) in [(&["accounts", "update", "modal-acme-two", "--aws-budget=emails"][..], "a budget in the account itself is AWS's"), (&["accounts", "update", "globex", "--monthly-usd=5"], "globex is not an account of this CI"), (&["accounts", "update"], "Say which")] {
        let (ok, said) = run(args);
        assert!(!ok && said.contains(why), "{args:?}: {said}");
    }
    // The dashboard: its row, under its word, with Modal's logo and its workspace; nothing left to add.
    let (mut child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&fragment=1").1) }).find(|p| p.contains("modal-acme-two")).unwrap_or_default();
    assert!(page.contains("Sandboxes in the Modal workspace acme-two"), "{page}");
    let settings = words(&part(&link, "/?p=runners&fragment=1").1);
    assert!(settings.contains("Its runners together") && settings.contains("Modal account"), "{settings}");
    let (status, _, said) = common::post(&link, "/routing", &[("action", "account_usd"), ("account", "globex"), ("usd", "5")]);
    assert!(status == 400 && words(&said).contains("Which account?"), "{status}");
    let (status, _, said) = common::post(&link, "/routing", &[("action", "account_usd"), ("account", "modal"), ("usd", "5"), ("budget", "emails")]);
    assert!(status == 400 && words(&said).contains("A budget in the account itself is AWS's"), "{status}");
    assert!(!words(&part(&link, "/?p=add-runners&fragment=1").1).contains("as the runner"));
    let _ = child.kill();
    // Asked for again under another word, or its word asked for by another workspace: a word is for good.
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two", "--name=modal-staging"]);
    assert!(!ok && said.contains("runs jobs as modal-acme-two"), "{said}");
    // Deleted: out of the order and the launchers, its record kept, marked gone.
    let removed: Vec<&str> = include_str!("../fixtures/modal/further/03-delete.jsonl").lines().filter(|l| !l.contains("/AppList\"")).collect();
    served.aws.replaying(&removed.join("\n"), &[("mdplane00002", PLANE)]);
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(ok && said.contains("Removed modal-acme-two."), "{said}");
    let kept = stored(&served);
    assert_eq!(kept["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two", "gone": true }, { "name": "modal-acme-two-functions", "account": "modal-acme-two", "gone": true, "function": true }]), "{kept}");
    assert!(!kept["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two") && !kept["routing"]["order"].as_array().unwrap().iter().any(|p| p["cloud"] == "modal-acme-two"), "{kept}");
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(!ok && said.contains("Say which"), "{said}");
    // Its word comes back for its account, with its account as it was.
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok, "{said}");
    assert_eq!(stored(&served)["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two" }, { "name": "modal-acme-two-functions", "account": "modal-acme-two", "function": true }]));
    let _ = std::fs::remove_dir_all(&home);
}

/// Further runners already on the control plane: `words`, each in a workspace named after the word.
fn with_further(served: &superci_testkit::Served, plane: &str, words: &[&str]) {
    let launcher = |word: &str, workspace: &str| serde_json::json!({ "cloud": word, "url": format!("https://{workspace}--superci-runners-{plane}.modal.run") });
    let mut launchers = vec![launcher("modal", "acmeorg")];
    launchers.extend(words.iter().map(|w| launcher(w, w.trim_start_matches("modal-"))));
    served.aws.put(&format!("/superci/{plane}/LAUNCHERS"), &serde_json::Value::Array(launchers.clone()).to_string());
    let record = serde_json::json!({ "version": 1, "routing": {}, "launchers": launchers,
        "accounts": words.iter().map(|w| serde_json::json!({ "name": w, "provider": "modal", "cloud_id": w.trim_start_matches("modal-") })).collect::<Vec<_>>(),
        "runners": words.iter().flat_map(|w| [serde_json::json!({ "name": w, "account": w }), serde_json::json!({ "name": format!("{w}-functions"), "account": w, "function": true })]).collect::<Vec<_>>() });
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
}

fn sandboxes_made(served: &superci_testkit::Served) -> usize { served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/SandboxCreate"))).count() }

/// Updating deploys each Modal launcher in its own workspace, and says which it could not reach.
#[test]
fn an_update_deploys_each_modal_launcher_with_its_own_workspaces_sign_in_and_names_those_it_cannot() {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-acme-two", "modal-globex"]);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update-with-launchers.jsonl"), &[("mixplane0001", PLANE), ("http://new-plane.test", &served.url)]);
    deploy_answers(&served);
    let o = common::command(&home).args(["planes", "update"]).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("Its Modal launchers modal-globex were not updated: sign in to their workspaces (`superci login modal --add`)"), "{said}");
    assert_eq!(sandboxes_made(&served), 2, "one in each workspace signed in to");
    let _ = std::fs::remove_dir_all(&home);
}

/// A move whose new control plane had its runners changed by another computer as they were written: stopped, saying so.
#[test]
fn a_move_whose_new_control_plane_changed_its_runners_meanwhile_says_so() {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-acme-two", "modal-globex"]);
    // (And one removed: no launcher to bring. A Cloudflare runner in a second account, made again in its account for the
    // new one; one whose account is not kept stays behind.)
    let second = "2222222222222222222222222222222b";
    let mut record = stored(&served);
    record["runners"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "modal-old", "account": "modal-globex", "gone": true }), serde_json::json!({ "name": "cf-globex", "account": "cf-globex" }), serde_json::json!({ "name": "cf-lost", "account": "cf-lost" })]);
    record["accounts"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "cf-globex", "provider": "cloudflare", "cloud_id": second }), serde_json::json!({ "name": "cf-lost", "provider": "cloudflare" })]);
    record["launchers"].as_array_mut().unwrap().push(serde_json::json!({ "cloud": "cf-globex", "url": format!("https://superci-runners-{PLANE}.globex.workers.dev") }));
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &record["launchers"].to_string());
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    let cf_calls: Vec<String> = include_str!("../fixtures/runners/03-cf-agent.jsonl").lines().filter(|l| !l.contains("/accounts?per_page") && !l.contains("iam.amazonaws.com") && !l.contains("api.modal.com")).map(str::to_string).collect();
    served.aws.replaying(&cf_calls.join("\n"), &[("1111111111111111111111111111111a", second), ("awsplane0001", "testplane0002"), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
    served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{second}","name":"Globex","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    served.aws.put("/superci/testplane0002/CONTAINERS", "off");
    { let mut w = served.aws.workers.lock().unwrap(); for workspace in ["acmeorg", "acme-two"] { w.insert(format!("{workspace}--superci-runners-testplane0002.modal.run"), new.url.clone()); } w.insert("superci-runners-testplane0002.globex.workers.dev".into(), new.url.clone()); }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    // (The new one's runners changed by another computer as they are written.)
    // (Its third write of settings, its runners' records: refused as changed meanwhile.)
    let posts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let front = superci_testkit::front(&new.url, move |method, target, status, body| {
        let refused = method == "POST" && target.starts_with("/settings") && posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2;
        if refused { (409, br#"{"conflicts":[{"path":["runners"],"now":[]}]}"#.to_vec()) } else { (status, body) }
    });
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": front, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    deploy_answers(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "50")]);
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", words(&said));
    let page = (0..120).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("Changed on another computer meanwhile")).unwrap_or_else(|| words(&part(&link, "/?p=planes&fragment=1").1));
    assert!(page.contains("Changed on another computer meanwhile"), "{page}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane with further Modal and Cloudflare runners (one removed, one whose account is not kept), a second
/// one in the same account to move to, and this computer signed in to both workspaces and Cloudflare.
fn moving_further_runners() -> (superci_testkit::Served, superci_testkit::Served, std::path::PathBuf) {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-acme-two", "modal-globex"]);
    // (And one removed: no launcher to bring. A Cloudflare runner in a second account, made again in its account for the
    // new one; one whose account is not kept stays behind.)
    let second = "2222222222222222222222222222222b";
    let mut record = stored(&served);
    record["runners"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "modal-old", "account": "modal-globex", "gone": true }), serde_json::json!({ "name": "cf-globex", "account": "cf-globex" }), serde_json::json!({ "name": "cf-lost", "account": "cf-lost" })]);
    record["accounts"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "cf-globex", "provider": "cloudflare", "cloud_id": second }), serde_json::json!({ "name": "cf-lost", "provider": "cloudflare" })]);
    record["launchers"].as_array_mut().unwrap().push(serde_json::json!({ "cloud": "cf-globex", "url": format!("https://superci-runners-{PLANE}.globex.workers.dev") }));
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &record["launchers"].to_string());
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    let cf_calls: Vec<String> = include_str!("../fixtures/runners/03-cf-agent.jsonl").lines().filter(|l| !l.contains("/accounts?per_page") && !l.contains("iam.amazonaws.com") && !l.contains("api.modal.com")).map(str::to_string).collect();
    served.aws.replaying(&cf_calls.join("\n"), &[("1111111111111111111111111111111a", second), ("awsplane0001", "testplane0002"), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
    served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{second}","name":"Globex","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    served.aws.put("/superci/testplane0002/CONTAINERS", "off");
    { let mut w = served.aws.workers.lock().unwrap(); for workspace in ["acmeorg", "acme-two"] { w.insert(format!("{workspace}--superci-runners-testplane0002.modal.run"), new.url.clone()); } w.insert("superci-runners-testplane0002.globex.workers.dev".into(), new.url.clone()); }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": new.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    deploy_answers(&served);
    (served, new, home)
}

/// A move brings the further runners along: each launcher deployed again for the new control plane in its own workspace,
/// the records with them; one whose workspace is not signed in to stays behind, said.
#[test]
fn a_move_brings_further_runners_with_their_records_and_says_which_stayed_behind() {
    let (served, new, home) = moving_further_runners();
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "50")]);
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", words(&said));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.store.under("moved_to").is_empty() { assert!(std::time::Instant::now() < end, "never moved: {}", words(&part(&link, "/?p=planes&fragment=1").1)); std::thread::sleep(std::time::Duration::from_millis(100)) }
    let theirs: serde_json::Value = serde_json::from_str(&new.store.0.lock().unwrap()["settings"]).unwrap();
    assert!(theirs["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two" && a["url"].as_str().unwrap().contains("acme-two--superci-runners-testplane0002")), "{theirs}");
    assert!(!theirs["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-globex"), "{theirs}");
    assert_eq!(theirs["runners"].as_array().unwrap().len(), 7, "{theirs}");
    assert!(theirs["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "cf-globex"), "{theirs}");
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("modal-globex stayed behind")).unwrap_or_default();
    assert!(page.contains("sign in to the Modal workspace globex") && page.contains("cf-lost stayed behind"), "{page}");
    served.aws.fail("/workers/scripts/superci-runners", 200, r#"{"result":null,"success":true,"errors":[],"messages":[]}"#, None);
    // The old one deleted: what is in a workspace not signed in to is said as left.
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    let (status, to, said) = common::post(&link, "/plane/delete", &[("plane", PLANE)]);
    assert!(status == 303, "{status} {to} {}", words(&said));
    let page = (0..80).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("Its launcher in the Modal workspace globex")).unwrap_or_default();
    assert!(page.contains("sign in there to remove it"), "{page}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// A second Cloudflare account the sign-in reaches: its launcher there under a word of its own, deleted with only that
/// account's Worker gone (each account's launcher has the same name).
#[test]
fn a_second_cloudflare_account_gets_a_runner_of_its_own_and_only_its_launcher_is_deleted() {
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let (first, second) = ("1111111111111111111111111111111a", "2222222222222222222222222222222b");
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &format!(r#"[{{"cloud":"cloudflare","url":"https://superci-runners-{PLANE}.acme.workers.dev"}}]"#));
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let cf_calls: Vec<String> = include_str!("../fixtures/runners/03-cf-agent.jsonl").lines().filter(|l| !l.contains("/accounts?per_page") && !l.contains("iam.amazonaws.com") && !l.contains("api.modal.com")).map(str::to_string).collect();
    served.aws.replaying(&cf_calls.join("\n"), &[(first, second), ("awsplane0001", PLANE), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
    served.aws.replaying(&cf_calls.join("\n"), &[("awsplane0001", PLANE)]);
    served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{first}","name":"Acme","type":"standard"}},{{"id":"{second}","name":"Globex Inc","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
    { let mut w = served.aws.workers.lock().unwrap(); w.insert(format!("superci-runners-{PLANE}.globex.workers.dev"), served.url.clone()); }
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // The first's own account asked for again: its word stays.
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={first}"), "--name=cloudflare-two"]);
    assert!(!ok && said.contains("runs jobs as cloudflare"), "{said}");
    // The other: its own word, from the account's name.
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}")]);
    assert!(ok && said.contains("cloudflare-globex-inc runs jobs now."), "{said}");
    let record: serde_json::Value = serde_json::from_str(&served.store.0.lock().unwrap()["settings"]).unwrap();
    assert_eq!(record["accounts"], serde_json::json!([{ "name": "cloudflare-globex-inc", "provider": "cloudflare", "cloud_id": second }]), "{record}");
    assert!(record["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "cloudflare-globex-inc" && a["url"].as_str().unwrap().contains(".globex.workers.dev")), "{record}");
    // Updated: each launcher in its own account, the first's found by its address.
    served.aws.replaying(include_str!("../fixtures/aws/planes/update-with-launchers.jsonl"), &[("mixplane0001", PLANE), ("http://new-plane.test", &served.url)]);
    served.aws.replaying(&cf_calls.join("\n"), &[(first, second), ("awsplane0001", PLANE), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
    served.aws.replaying(&cf_calls.join("\n"), &[("awsplane0001", PLANE)]);
    served.aws.asked.lock().unwrap().clear();
    let (ok, said) = run(&["planes", "update"]);
    assert!(ok && said.contains("Updated to") && !said.contains("was not updated") && !said.contains("were not updated"), "{said}");
    let put_in = |id: &str| served.aws.asked.lock().unwrap().iter().any(|a| a["method"] == "PUT" && a["url"].as_str().is_some_and(|u| u.contains(id) && u.contains(&format!("/workers/scripts/superci-runners-{PLANE}"))));
    assert!(put_in(first) && put_in(second), "both accounts' launchers");
    // Deleted: its account's Worker, not the first's.
    served.aws.asked.lock().unwrap().clear();
    served.aws.fail("/workers/scripts/superci-runners", 200, r#"{"result":null,"success":true,"errors":[],"messages":[]}"#, None);
    let (ok, said) = run(&["runners", "delete", "cloudflare-globex-inc", "--confirm"]);
    assert!(ok && said.contains("Removed cloudflare-globex-inc."), "{said}");
    let deleted: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["method"] == "DELETE" && a["url"].as_str().is_some_and(|u| u.contains("/workers/scripts/"))).map(|a| a["url"].as_str().unwrap().to_string()).collect();
    assert!(!deleted.is_empty() && deleted.iter().all(|u| u.contains(second)), "{deleted:?}");
    // Back, then the first deleted: a further one's account, whose launcher has the same name, left alone.
    served.aws.replaying(&cf_calls.join("\n"), &[(first, second), ("awsplane0001", PLANE), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
    served.aws.fail_none();
    served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{first}","name":"Acme","type":"standard"}},{{"id":"{second}","name":"Globex Inc","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}")]);
    assert!(ok && said.contains("cloudflare-globex-inc runs jobs now."), "{said}");
    // (Beside them, a Cloudflare runner removed before and a Modal one: neither's account counts.)
    let mut record = stored(&served);
    record["accounts"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "cf-old", "provider": "cloudflare", "cloud_id": first }), serde_json::json!({ "name": "modal-x", "provider": "modal", "cloud_id": "x" })]);
    record["runners"].as_array_mut().unwrap().extend([serde_json::json!({ "name": "cf-old", "account": "cf-old", "gone": true }), serde_json::json!({ "name": "modal-x", "account": "modal-x" })]);
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    served.aws.asked.lock().unwrap().clear();
    served.aws.fail("/workers/scripts/superci-runners", 200, r#"{"result":null,"success":true,"errors":[],"messages":[]}"#, None);
    let (ok, said) = run(&["runners", "delete", "cloudflare", "--confirm"]);
    assert!(ok && said.contains("Removed Cloudflare."), "{said}");
    let deleted: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["method"] == "DELETE" && a["url"].as_str().is_some_and(|u| u.contains("/workers/scripts/"))).map(|a| a["url"].as_str().unwrap().to_string()).collect();
    assert!(!deleted.is_empty() && deleted.iter().all(|u| u.contains(first)), "{deleted:?}");
    // Its account out of this sign-in's reach: an update says so, and a delete asks to sign in.
    served.aws.fail_none();
    served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{first}","name":"Acme","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
    let (ok, said) = run(&["runners", "delete", "cloudflare-globex-inc", "--confirm"]);
    assert!(!ok && said.contains("Sign in"), "{said}");
    served.aws.replaying(include_str!("../fixtures/aws/planes/update-with-launchers.jsonl"), &[("mixplane0001", PLANE), ("http://new-plane.test", &served.url)]);
    let (ok, said) = run(&["planes", "update"]);
    assert!(ok && said.contains("Its Cloudflare launchers cloudflare-globex-inc were not updated: this sign-in does not reach their accounts"), "{said}");
    // SuperCI left: its control plane deleted, its launchers in Cloudflare with it, a further one's among them.
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    served.aws.fail("/workers/scripts/superci-runners", 200, r#"{"result":null,"success":true,"errors":[],"messages":[]}"#, None);
    let (ok, said) = run(&["leave", "--confirm"]);
    assert!(ok, "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A second AWS account signed in to: its role and networks made with its own sign-in, its two runners under its word,
/// and only that account's part removed.
#[test]
fn a_second_aws_account_gets_its_runners_with_its_own_sign_in() {
    let served = serve(|c| (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX })));
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut other = kept["aws"].clone();
    other["account_id"] = "222222222222".into();
    kept["aws_more"] = serde_json::json!([other]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // Not signed in to, the first's own, no region, a word with no room for its on-demand one.
    for (args, why) in [(&["runners", "create", "aws", "--account=333333333333", "--region=eu-west-1"][..], "Sign in to account 333333333333"), (&["runners", "create", "aws", "--account=222222222222"], "Say where its machines start"),
        (&["runners", "create", "aws", "--account=111111111111", "--region=eu-west-1"], "runs jobs as aws"), (&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=a-word-of-twenty-three-c"], "no room for its second runner"), (&["runners", "create", "aws", "--account=222222222222", "--region=nowhere"], "Choose a region")] {
        let (ok, said) = run(args);
        assert!(!ok && said.contains(why), "{args:?}: {said}");
    }
    // AWS answering as it answered a real connection: its identity provider, its role, the machines' networks.
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    for region in ["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-north-1"] { served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", &format!("ec2.{region}")), &[]) }
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"]);
    assert!(ok && said.contains("aws-acme runs jobs now."), "{said}");
    let record = stored(&served);
    assert_eq!(record["accounts"], serde_json::json!([{ "name": "aws-acme", "provider": "aws", "cloud_id": "222222222222" }]), "{record}");
    assert_eq!(record["runners"], serde_json::json!([{ "name": "aws-acme", "account": "aws-acme" }, { "name": "aws-acme-on-demand", "account": "aws-acme", "on_demand": true }]), "{record}");
    let (ok, said) = run(&["runners", "list"]);
    assert!(ok && said.contains("aws-acme") && said.contains("aws-acme-on-demand") && said.contains("eu-west-1"), "{said}");
    // Its networks only: a bucket's name is the same in every account, and the CI's one cache is in the first.
    let buckets: Vec<(String, String)> = served.aws.asked.lock().unwrap().iter().filter(|x| x["method"] == "PUT" && x["url"].as_str().is_some_and(|u| u.contains(".s3.") && u.contains("lifecycle"))).map(|x| (x["url"].as_str().unwrap().to_string(), x["target"].as_str().unwrap_or_default().to_string())).collect();
    assert!(buckets.iter().all(|(u, _)| u.contains(&format!("superci-plane-{PLANE}-eu-"))), "{buckets:?}");
    let (ok, said) = run(&["runners", "update", "aws-acme", "--max-jobs=3"]);
    assert!(ok, "{said}");
    // Its regions, its own: its connection says them, and the first's stay.
    let (ok, said) = run(&["runners", "update", "aws-acme", "--regions=eu-west-2,eu-west-1"]);
    assert!(ok && said.contains("eu-west-2"), "{said}");
    let connection: serde_json::Value = serde_json::from_str(&served.store.0.lock().unwrap()["aws:connection:aws-acme"]).unwrap();
    assert!(connection["regions"].as_array().unwrap().iter().any(|r| r == "eu-west-2") && connection["account_id"] == "222222222222", "{connection}");
    let (mut child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&fragment=1").1) }).find(|p| p.contains("AWS account 222222222222")).unwrap_or_default();
    assert!(page.contains("aws-acme-on-demand"), "{page}");
    let _ = child.kill();
    // Removed by its word, its on-demand one with it; its record kept.
    let (ok, said) = run(&["runners", "delete", "aws-acme-on-demand", "--confirm"]);
    assert!(!ok && said.contains("goes with aws-acme"), "{said}");
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(ok && said.contains("Removed aws-acme."), "{said}");
    let record = stored(&served);
    assert!(record["runners"].as_array().unwrap().iter().all(|r| r["gone"] == true) && !record["routing"]["order"].as_array().is_some_and(|o| o.iter().any(|p| p["cloud"].as_str().is_some_and(|c| c.starts_with("aws-acme")))), "{record}");
    let _ = std::fs::remove_dir_all(&home);
}

/// An AWS control plane with further AWS accounts already connected: `words`, each `(word, account)`.
fn with_further_aws(served: &superci_testkit::Served, words: &[(&str, &str)]) {
    let record = serde_json::json!({ "version": 1, "routing": {}, "launchers": [],
        "accounts": words.iter().map(|(w, id)| serde_json::json!({ "name": w, "provider": "aws", "cloud_id": id })).collect::<Vec<_>>(),
        "runners": words.iter().flat_map(|(w, _)| [serde_json::json!({ "name": w, "account": w }), serde_json::json!({ "name": format!("{w}-on-demand"), "account": w, "on_demand": true })]).collect::<Vec<_>>() });
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    for (w, id) in words {
        served.store.0.lock().unwrap().insert(format!("aws:connection:{w}"), serde_json::json!({ "account_id": id, "region": "eu-west-1", "role_arn": format!("arn:aws:iam::{id}:role/superci-plane-{PLANE}"), "connected": true, "error": null, "name": w, "regions": ["eu-west-1"] }).to_string());
    }
}

fn own_aws(c: &mut superci_core::plane::Config) {
    (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
}

/// Signed in to the first AWS account and to 222222222222.
fn signed_in_two_aws(served: &superci_testkit::Served) -> std::path::PathBuf {
    let home = common::signed_in(served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut other = kept["aws"].clone();
    other["account_id"] = "222222222222".into();
    kept["aws_more"] = serde_json::json!([other]);
    std::fs::write(&file, kept.to_string()).unwrap();
    home
}

/// Updating gives each further AWS account's role what this version asks for, with its own sign-in, and names the rest.
#[test]
fn an_update_gives_each_further_aws_role_its_permissions_and_names_those_it_cannot() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    with_further_aws(&served, &[("aws-acme", "222222222222"), ("aws-globex", "333333333333")]);
    let home = signed_in_two_aws(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", "ec2.eu-west-1"), &[]);
    let o = common::command(&home).args(["planes", "update"]).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("The roles of aws-globex were not updated: sign in to their AWS accounts"), "{said}");
    // AWS refusing a further account's role its permissions: the update stops, saying so.
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    let asked = served.aws.asked.lock().unwrap().len();
    served.aws.during("Action=PutRolePolicy", || {});
    served.aws.fail("Action=PutRolePolicy", 403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>may not put role policies</Message></Error></ErrorResponse>", None);
    let o = common::command(&home).args(["planes", "update"]).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    let puts: Vec<String> = served.aws.asked.lock().unwrap()[asked..].iter().filter(|a| a["asked"].as_str().unwrap_or_default().contains("PutRolePolicy")).map(|a| a.to_string().chars().take(300).collect()).collect();
    assert!(!o.status.success() && said.contains("may not put role policies"), "{said}\n{puts:#?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A move whose new control plane cannot use a further account's role (AWS refuses it there): moved all the same, and
/// the account said with what AWS answered.
#[test]
fn a_move_whose_new_control_plane_cannot_use_a_further_accounts_role_says_so() {
    let served = serve(own_aws);
    served.connect_github();
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    with_further_aws(&served, &[("aws-acme", "222222222222"), ("aws-globex", "333333333333")]);
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| { c.read_settings(&aws.settings("testplane0002")); own_aws(c) });
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    served.aws.put("/superci/testplane0002/CONTAINERS", "off");
    let home = signed_in_two_aws(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    // (The new one saying AWS does not let it use the role: AWS refusing it what it checks with.)
    // (Then, asked again, failing outright.)
    let asked = std::sync::atomic::AtomicU32::new(0);
    let front = superci_testkit::front(&new.url, move |method, target, status, body| {
        if method != "POST" || !target.starts_with("/aws/accounts") { return (status, body) }
        if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { (200, br#"{"connected":false,"error":"UnauthorizedOperation: may not see images"}"#.to_vec()) } else { (500, br#"{"error":"may not see images"}"#.to_vec()) }
    });
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": front, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", "ec2.eu-west-1"), &[]);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "50")]);
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", words(&said));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.store.under("moved_to").is_empty() { assert!(std::time::Instant::now() < end, "never moved"); std::thread::sleep(std::time::Duration::from_millis(100)) }
    let page = (0..120).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("AWS did not let the new control plane use its role")).unwrap_or_else(|| words(&part(&link, "/?p=planes&fragment=1").1));
    assert!(page.contains("aws-acme: AWS did not let the new control plane use its role yet (the control plane answered 500"), "{page}");
    // The old one deleted, AWS answering everything: its part in the further account taken away, nothing said left there.
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    let (status, to, said) = common::post(&link, "/plane/delete", &[("plane", PLANE)]);
    assert!(status == 303, "{status} {to} {}", words(&said));
    let page = (0..80).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("Its role in AWS account 333333333333")).unwrap_or_default();
    assert!(page.contains("Its role in AWS account 333333333333") && !page.contains("Its machines' network in AWS account 222222222222"), "{page}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// A move brings a further AWS account along: a role for the new control plane there, its connection in the new one;
/// one not signed in to stays behind, said. Deleting the old one then takes its part in the account signed in to.
#[test]
fn a_move_brings_further_aws_accounts_and_deleting_the_old_control_plane_cleans_up() {
    let served = serve(own_aws);
    served.connect_github();
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    with_further_aws(&served, &[("aws-acme", "222222222222"), ("aws-globex", "333333333333")]);
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| { c.read_settings(&aws.settings("testplane0002")); own_aws(c) });
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    served.aws.put("/superci/testplane0002/CONTAINERS", "off");
    let home = signed_in_two_aws(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": new.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", "ec2.eu-west-1"), &[]);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "50")]);
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", words(&said));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.store.under("moved_to").is_empty() { assert!(std::time::Instant::now() < end, "never moved"); std::thread::sleep(std::time::Duration::from_millis(100)) }
    let connected: serde_json::Value = serde_json::from_str(new.store.0.lock().unwrap().get("aws:connection:aws-acme").map_or("{}", String::as_str)).unwrap();
    assert_eq!(connected["account_id"], "222222222222", "{connected}");
    let theirs: serde_json::Value = serde_json::from_str(&new.store.0.lock().unwrap()["settings"]).unwrap();
    assert_eq!(theirs["runners"].as_array().unwrap().len(), 4, "{theirs}");
    assert!(new.store.0.lock().unwrap().contains_key("aws:connection:aws-acme"), "its connection, in the new one");
    assert!(!new.store.0.lock().unwrap().contains_key("aws:connection:aws-globex"), "not signed in to");
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("aws-globex stayed behind")).unwrap_or_default();
    assert!(page.contains("sign in to AWS account 333333333333"), "{page}");
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    // (AWS refusing to say what networks there are: said as left in the account signed in to.)
    served.aws.fail("Action=DescribeVpcs", 403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>may not</Message></Error></Errors></Response>", None);
    let (status, to, said) = common::post(&link, "/plane/delete", &[("plane", PLANE)]);
    assert!(status == 303, "{status} {to} {}", words(&said));
    let page = (0..80).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("Its role in AWS account 333333333333")).unwrap_or_default();
    assert!(page.contains("sign in with AWS to remove it") && page.contains("Its machines' network in AWS account 222222222222"), "{page}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane as 0.19.21 left it, its settings in its cloud with repository rules and a default provider, no record
/// of its own: updated, what it had stands (the order, limits, launchers), the rules and default gone; the first change
/// makes its record without losing any of it; an older SuperCI's later write is taken, and leaves further runners be.
#[test]
fn settings_as_0_19_21_left_them_stand_after_an_update_and_an_older_write_leaves_further_runners() {
    let served = serve(|_| {});
    served.connect_github();
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &format!(r#"[{{"cloud":"modal","url":"https://acmeorg--superci-runners-{PLANE}.modal.run"}}]"#));
    served.aws.put(&format!("/superci/{PLANE}/ROUTING"), r#"{"default":"modal","rules":[{"repo":"acme/*","cloud":"modal"}],"order":[{"cloud":"modal","max_jobs":3,"monthly_usd":40.0}],"monthly_usd":100.0,"account_usd":{"aws":50.0},"public_repos":["acme/site"],"max_cpu":16}"#);
    let home = common::signed_in(&served);
    let (code, status) = common::json(&home, &["status"]);
    let routing = &status["status"]["routing"];
    assert_eq!(code, 0, "{status}");
    assert_eq!((&routing["order"][0]["max_jobs"], &routing["monthly_usd"], &routing["account_usd"]["aws"], &routing["public_repos"][0], &routing["max_cpu"]), (&serde_json::json!(3), &serde_json::json!(100.0), &serde_json::json!(50.0), &serde_json::json!("acme/site"), &serde_json::json!(16)), "{routing}");
    assert!(routing.get("rules").is_none() && routing.get("default").is_none(), "{routing}");
    // The first change: its record, with everything it had.
    let o = common::command(&home).args(["runners", "update", "modal", "--max-jobs=5"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let kept = stored(&served);
    assert_eq!((&kept["routing"]["order"][0]["max_jobs"], &kept["routing"]["order"][0]["monthly_usd"], &kept["routing"]["monthly_usd"], &kept["routing"]["max_cpu"], &kept["launchers"][0]["cloud"]), (&serde_json::json!(5), &serde_json::json!(40.0), &serde_json::json!(100.0), &serde_json::json!(16), &serde_json::json!("modal")), "{kept}");
    // A further runner, then an older SuperCI's whole write of what it knows: taken, the records left.
    let mut record = kept.clone();
    (record["accounts"], record["runners"]) = (serde_json::json!([{ "name": "modal-two", "provider": "modal", "cloud_id": "two" }]), serde_json::json!([{ "name": "modal-two", "account": "modal-two" }]));
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    served.aws.put(&format!("/superci/{PLANE}/ROUTING"), r#"{"order":[{"cloud":"modal","max_jobs":9}],"default":null,"rules":[]}"#);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, status) = common::json(&home, &["status"]);
        if status["status"]["routing"]["order"][0]["max_jobs"] == 9 { assert_eq!(status["status"]["runners"][0]["name"], "modal-two", "{status}"); break }
        assert!(std::time::Instant::now() < end, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// Everything that can stop a second AWS account being added or removed, each said as what it is, nothing half done.
#[test]
fn a_second_aws_accounts_refusals_are_each_said() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = signed_in_two_aws(&served);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let answers = || {
        let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
        for region in ["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-north-1"] { served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", &format!("ec2.{region}")), &[]) }
    };
    let add = ["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"];
    // AWS refusing its role: said as AWS said it, no record written.
    answers();
    served.aws.fail("Action=CreateRole", 403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not allowed to create roles</Message></Error></ErrorResponse>", Some(1));
    let (ok, said) = run(&add);
    assert!(!ok && said.contains("not allowed to create roles"), "{said}");
    assert!(served.store.0.lock().unwrap().get("settings").is_none_or(|s| !s.contains("aws-acme")), "nothing kept");
    served.aws.fail_none();
    // The role never usable: said, and its records there for the next try, which keeps the same word.
    answers();
    served.aws.fail("Action=DescribeImages", 403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not authorized to assume it</Message></Error></ErrorResponse>", None);
    let (ok, said) = run(&add);
    assert!(!ok && said.contains("not authorized to assume it"), "{said}");
    let connection: serde_json::Value = serde_json::from_str(&served.store.0.lock().unwrap()["aws:connection:aws-acme"]).unwrap();
    assert_eq!(connection["connected"], false, "{connection}");
    let (ok, said) = run(&["runners", "list"]);
    assert!(ok && !said.contains("aws-acme"), "not connected, not listed: {said}");
    served.aws.fail_none();
    answers();
    let (ok, said) = run(&add);
    assert!(ok && said.contains("aws-acme runs jobs now."), "{said}");
    // Another word for the same account, or its word for another: refused.
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-other"]);
    assert!(!ok && said.contains("runs jobs as aws-acme"), "{said}");
    // Regions: not one of the list.
    let (ok, said) = run(&["runners", "update", "aws-acme", "--regions=nowhere"]);
    assert!(!ok && said.contains("Choose a region"), "{said}");
    // With jobs on it: said, and stopped only when asked.
    let (id, q) = served.github.queue("job", "superci/provider=aws", served.now_ms());
    let _ = (id, q);
    let mut record: serde_json::Value = serde_json::from_str(&served.store.0.lock().unwrap()["settings"]).unwrap();
    record["routing"]["order"] = serde_json::json!([{ "cloud": "aws-acme" }, { "cloud": "aws-acme-on-demand" }]);
    served.store.0.lock().unwrap().insert("settings".into(), record.to_string());
    served.store.0.lock().unwrap().insert("job:1".into(), serde_json::json!({ "job_id": 1, "run_id": 3, "repo": "acme/app", "state": "running", "at_ms": served.now_ms(), "runner": "r", "runner_id": null, "installation_id": 42, "cloud": "aws-acme-on-demand", "machine_id": "i-1", "machine_type": "c8a.xlarge", "error": null, "seen_in_progress": true, "on_demand": true }).to_string());
    served.store.0.lock().unwrap().insert("live:1".into(), String::new());
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(!ok && said.contains("1 job is running on aws-acme"), "{said}");
    served.store.0.lock().unwrap().remove("live:1");
    served.store.0.lock().unwrap().remove("job:1");
    // Not signed in there any more: asks to sign in, or forgets it here only, saying what was left.
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept.as_object_mut().unwrap().remove("aws_more");
    std::fs::write(&file, kept.to_string()).unwrap();
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(!ok && said.contains("Sign in to account 222222222222"), "{said}");
    let (ok, said) = run(&["runners", "update", "aws-acme", "--regions=eu-west-2,eu-central-1"]);
    assert!(!ok && said.contains("Sign in to account 222222222222"), "{said}");
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm", "--only-here"]);
    assert!(ok && said.contains("Removed aws-acme.") && said.contains("in AWS account 222222222222: the role"), "{said}");
    assert!(!served.store.0.lock().unwrap().contains_key("aws:connection:aws-acme"), "forgotten");
    // Its word, for another account: for good.
    let mut other = kept["aws"].clone();
    other["account_id"] = "333333333333".into();
    kept["aws_more"] = serde_json::json!([other]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let (ok, said) = run(&["runners", "create", "aws", "--account=333333333333", "--region=eu-west-1", "--name=aws-acme"]);
    assert!(!ok && said.contains("another runner's word"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A second AWS account on a control plane that cannot have one: from before them, or with no first account yet. And with
/// the first signed out, it is added all the same, saying its jobs lack the cache there until it is signed in.
#[test]
fn a_second_aws_account_needs_a_control_plane_that_keeps_it_and_says_when_its_cache_waits() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = signed_in_two_aws(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1")]);
    let add = [("account", "222222222222"), ("region", "eu-west-1")];
    served.answer_as("0.19.21", &["settings", "settings_version"]);
    let (status, _, said) = common::post(&link, "/runners/aws", &add);
    assert!(status == 409 && words(&said).contains("A further runner needs a newer control plane"), "{status} {}", words(&said));
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
    // Signed in to the further account alone.
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = signed_in_two_aws(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let other = kept["aws_more"][0].clone();
    (kept["aws"], kept["aws_more"]) = (other, serde_json::json!([]));
    std::fs::write(&file, kept.to_string()).unwrap();
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    for region in ["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-north-1"] { served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", &format!("ec2.{region}")), &[]) }
    let o = common::command(&home).args(["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme", "--plane=testplane0001"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!served.aws.asked.lock().unwrap().iter().any(|a| a["method"] == "PUT" && a["url"].as_str().is_some_and(|u| u.contains(&format!("superci-plane-{PLANE}-eu-")))), "no bucket made by a further account");
    // A region added: its network made; no cache there, the first account not signed in to.
    let o = common::command(&home).args(["runners", "update", "aws-acme-on-demand", "--regions=eu-north-1", "--plane=testplane0001"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&home);
}

/// A second Cloudflare account's refusals, each said; and its word kept through deleting it.
#[test]
fn a_second_cloudflare_accounts_refusals_are_each_said() {
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let (first, second, third) = ("1111111111111111111111111111111a", "2222222222222222222222222222222b", "3333333333333333333333333333333c");
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &format!(r#"[{{"cloud":"cloudflare","url":"https://superci-runners-{PLANE}.acme.workers.dev"}}]"#));
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let accounts = |ids: &[(&str, &str)]| serde_json::json!({ "result": ids.iter().map(|(i, n)| serde_json::json!({ "id": i, "name": n, "type": "standard" })).collect::<Vec<_>>(), "success": true, "errors": [], "messages": [] }).to_string();
    served.aws.fail("/accounts?per_page", 200, &accounts(&[(first, "Acme"), (second, "Globex"), (third, "Initech")]), None);
    let cf: Vec<String> = include_str!("../fixtures/runners/03-cf-agent.jsonl").lines().filter(|l| !l.contains("/accounts?per_page") && !l.contains("iam.amazonaws.com") && !l.contains("api.modal.com")).map(str::to_string).collect();
    let answers = |id: &str, sub: &str| { let (to, sub) = (id.to_string(), format!("subdomain\\\":\\\"{sub}")); served.aws.replaying(&cf.join("\n"), &[(first, to.as_str()), ("awsplane0001", PLANE), ("subdomain\\\":\\\"acme", sub.as_str())]) };
    { let mut w = served.aws.workers.lock().unwrap(); for sub in ["globex", "initech"] { w.insert(format!("superci-runners-{PLANE}.{sub}.workers.dev"), served.url.clone()); } }
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // An account the sign-in does not reach; a word that cannot be one, or is another's.
    answers(third, "initech");
    answers(second, "globex");
    answers(first, "acme");
    let (ok, said) = run(&["runners", "create", "cloudflare", "--account=4444444444444444444444444444444d"]);
    assert!(!ok && said.contains("does not reach that account"), "{said}");
    answers(second, "globex");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}"), "--name=Not A Word"]);
    assert!(!ok && said.contains("a word is 2 to 32"), "{said}");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}"), "--name=modal"]);
    assert!(!ok && said.contains("another runner's word"), "{said}");
    // Cloudflare refusing the launcher: said, nothing kept.
    served.aws.fail("/workers/scripts/superci-runners", 500, r#"{"success":false,"errors":[{"code":10000,"message":"Internal server error"}],"messages":[],"result":null}"#, None);
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}")]);
    assert!(!ok && said.contains("Internal server error"), "{said}");
    assert!(served.store.0.lock().unwrap().get("settings").is_none_or(|s| !s.contains("cloudflare-globex")), "nothing kept");
    served.aws.fail_none();
    served.aws.fail("/accounts?per_page", 200, &accounts(&[(first, "Acme"), (second, "Globex"), (third, "Initech")]), None);
    // Asked for by name, then its word stays with its account: another word, or another account under it, refused.
    answers(second, "globex");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}"), "--name=cf-globex"]);
    assert!(ok && said.contains("cf-globex runs jobs now."), "{said}");
    answers(second, "globex");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}"), "--name=cf-other"]);
    assert!(!ok && said.contains("runs jobs as cf-globex"), "{said}");
    answers(third, "initech");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={third}"), "--name=cf-globex"]);
    assert!(!ok && said.contains("another runner's word"), "{said}");
    // Its limits, by its word.
    let (ok, said) = run(&["runners", "update", "cf-globex", "--max-jobs=2", "--monthly-usd=10"]);
    assert!(ok && said.contains("cf-globex"), "{said}");
    // The dashboard offers what is left; none for the account it runs on.
    let (mut child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=add-runners&fragment=1").1) }).find(|p| p.contains("Cloudflare account Initech")).unwrap_or_default();
    assert!(page.contains("Cloudflare account Initech") && !page.contains("Cloudflare account Globex"), "{page}");
    let runners = words(&part(&link, "/?p=runners&fragment=1").1);
    assert!(runners.contains("cf-globex") && runners.contains("Containers sized per job, in a Cloudflare account of its own"), "{runners}");
    let _ = child.kill();
    // Deleted, a job's word kept; added again it comes back as it was, under its own account.
    served.aws.fail("/workers/scripts/superci-runners", 200, r#"{"result":null,"success":true,"errors":[],"messages":[]}"#, None);
    let (ok, said) = run(&["runners", "delete", "cf-globex", "--confirm", "--only-here"]);
    assert!(ok && said.contains("Removed cf-globex."), "{said}");
    served.aws.fail_none();
    served.aws.fail("/accounts?per_page", 200, &accounts(&[(first, "Acme"), (second, "Globex"), (third, "Initech")]), None);
    answers(second, "globex");
    let (ok, said) = run(&["runners", "create", "cloudflare", &format!("--account={second}")]);
    assert!(ok && said.contains("cf-globex runs jobs now."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Modal's Functions by command, on the first runner and a further one: a row of each, taken out until put in, deployed
/// first by each one's own workspace; an image added while it is on, deployed again; what its preemptions came to said
/// in the status and the list.
#[test]
fn functions_by_command_on_each_modal_runner_and_their_preemptions_said() {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-acme-two"]);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    deploy_answers(&served);
    let (ok, said) = run(&["runner_images", "list"]);
    assert!(ok && said.contains("No image of your own"), "{said}");
    let (ok, said) = run(&["runner_images", "create", "own-ci", "--reference=ghcr.io/acme/ci:1", "--modal-secret=not a secret"]);
    assert!(!ok && said.contains("Modal Secret"), "{said}");
    // Each Modal runner has its row, taken out.
    let (_, data) = common::json(&home, &["runners", "list"]);
    let row = |data: &serde_json::Value, cloud: &str| data["order"].as_array().unwrap().iter().find(|p| p["cloud"] == cloud).cloned().unwrap_or_default();
    for cloud in ["modal-functions", "modal-acme-two-functions"] { assert_eq!(row(&data, cloud)["taken_out"], true, "{data}") }
    let made = |served: &superci_testkit::Served| sandboxes_made(served);
    let before = made(&served);
    // (The first workspace named: the first Modal runner, by the launcher's address.)
    let (ok, said) = run(&["runners", "create", "modal", "--account=acmeorg"]);
    assert!(ok && said.contains("modal runs jobs now."), "{said}");
    let (ok, said) = run(&["runners", "order", "modal-acme-two-functions", "modal-acme-two", "modal"]);
    assert!(ok && said.contains("Order: modal-acme-two-functions, modal-acme-two, modal."), "{said}");
    assert!(made(&served) > before, "its Functions deployed");
    let (ok, said) = run(&["runners", "list"]);
    assert!(ok && said.contains("Preempted: 0 Modal Functions this month · 0 jobs run again."), "{said}");
    let (_, data) = common::json(&home, &["runners", "list"]);
    assert!(row(&data, "modal-acme-two-functions")["taken_out"] != true && row(&data, "modal-functions")["taken_out"] == true, "{data}");
    // An image added while it runs Functions: its Function first, in that workspace; with a Secret by a good name.
    deploy_answers(&served);
    let before = made(&served);
    let (ok, said) = run(&["runner_images", "create", "own-ci", "--reference=ghcr.io/acme/ci:1", "--modal-secret=ghcr-read"]);
    assert!(ok && said.contains("Jobs on Modal start from ghcr.io/acme/ci:1"), "{said}");
    assert_eq!(stored(&served)["routing"]["images"]["own-ci"]["modal_secret"], "ghcr-read");
    assert!(made(&served) > before);
    let (ok, said) = run(&["runner_images", "list"]);
    assert!(ok && said.contains("own-ci"), "{said}");
    // A first preempted and a first run again: said in the one; then several, in the plural.
    let now = served.now_ms();
    served.store.0.lock().unwrap().insert(format!("modal:preempted:{now}:1"), now.to_string());
    served.store.0.lock().unwrap().insert(format!("modal:rerun:{now}:1"), now.to_string());
    let (_, said) = run(&["status"]);
    assert!(said.contains("Preempted     1 Modal Function this month · 1 job run again"), "{said}");
    served.store.0.lock().unwrap().insert(format!("modal:preempted:{now}:2"), now.to_string());
    let (_, said) = run(&["runners", "list"]);
    assert!(said.contains("Preempted: 2 Modal Functions this month · 1 job run again."), "{said}");
    // Made again with its row on: its Functions too.
    deploy_answers(&served);
    let before = made(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok && made(&served) > before + 1, "{said}");
    // A good Secret's name for an image there is; an email is only for AWS's budget.
    let (ok, said) = run(&["runner_images", "update", "own-ci", "--modal-secret=ghcr-other"]);
    assert!(ok, "{said}");
    let (ok, said) = run(&["accounts", "update", "modal-acme-two", "--monthly-usd=5", "--email=a@example.com"]);
    assert!(!ok && said.contains("--aws-budget and --email are for aws"), "{said}");
    // Back to sandboxes: nothing deployed; this month's preemptions still said. Removed with its runner, not by itself.
    deploy_answers(&served);
    let before = made(&served);
    let (ok, _) = run(&["runners", "order", "modal-acme-two", "modal"]);
    assert!(ok && made(&served) == before);
    let (_, said) = run(&["runners", "list"]);
    assert!(said.contains("Preempted: 2 Modal Functions this month"), "{said}");
    let (ok, said) = run(&["runners", "delete", "modal-acme-two-functions", "--confirm"]);
    assert!(!ok && said.contains("goes with modal-acme-two"), "{said}");
    // Signed in to Modal already: how to add another workspace. A control plane that says no preemptions: none said.
    let (ok, said) = run(&["login", "modal"]);
    assert!(ok && said.contains("`superci login modal --add` signs in to another Modal workspace beside it."), "{said}");
    served.answer_as("0.19.21", &["preempted"]);
    let (ok, said) = run(&["status"]);
    assert!(ok && !said.contains("Preempted"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A further runner on a control plane from before them is refused for each provider, nothing deployed; and on control
/// planes in Modal and in Cloudflare, a further one is what any launcher is, beside their own machines.
#[test]
fn further_runners_on_control_planes_from_before_and_in_each_cloud() {
    let (served, home) = two_workspaces();
    let (mut child, link) = common::dashboard(&home);
    served.answer_as("0.19.21", &["settings", "settings_version"]);
    let (status, _, said) = common::post(&link, "/runners/modal", &[("account", "acme-two")]);
    assert!(status == 409 && words(&said).contains("A further runner needs a newer control plane"), "{status} {}", words(&said));
    assert_eq!(sandboxes_made(&served), 0, "nothing deployed");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// Removing a second AWS account when AWS answers: nothing left over. When AWS refuses: said, and by the organization's
/// rule for a region: that region has nothing to remove. Its regions: the same again changes nothing; a repeat counts once.
#[test]
fn a_second_aws_accounts_regions_and_removal_as_aws_answers() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = signed_in_two_aws(&served);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    let answers = || for region in ["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-north-1", "us-east-1"] { served.aws.replaying(&connect.join("\n").replace("ec2.us-east-1", &format!("ec2.{region}")), &[]) };
    answers();
    // The control plane not let to use the role (AWS refusing it what it checks with): asked a few times, then said.
    served.aws.fail("Action=DescribeImages", 403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>may not see images</Message></Error></Errors></Response>", None);
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"]);
    assert!(!ok && said.contains("AWS did not let the control plane use the role yet") && said.contains("may not see images"), "{said}");
    served.aws.fail_none();
    answers();
    // (Beside a further runner elsewhere, whose record stays.)
    served.store.0.lock().unwrap().insert("settings".into(), serde_json::json!({ "version": 1, "routing": {}, "launchers": [], "accounts": [{ "name": "modal-x", "provider": "modal", "cloud_id": "x" }], "runners": [{ "name": "modal-x", "account": "modal-x" }] }).to_string());
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"]);
    assert!(ok && stored(&served)["runners"][0]["name"] == "modal-x", "{said}");
    let regions = || { let c: serde_json::Value = serde_json::from_str(&served.store.0.lock().unwrap()["aws:connection:aws-acme"]).unwrap(); c["regions"].clone() };
    // Its spot runner's regions; a repeat counted once; its on-demand runner keeps what it had, so the account has both.
    let (ok, said) = run(&["runners", "update", "aws-acme", "--regions=eu-west-1,eu-west-1"]);
    assert!(ok, "{said}");
    let (ok, said) = run(&["runners", "update", "aws-acme-on-demand", "--regions=eu-west-2"]);
    assert!(ok, "{said}");
    let both = regions();
    assert!(both.as_array().unwrap().iter().any(|r| r == "eu-west-2"), "{both}");
    // The same again: nothing asked of AWS.
    served.aws.asked.lock().unwrap().clear();
    let (ok, said) = run(&["runners", "update", "aws-acme-on-demand", "--regions=eu-west-2"]);
    assert!(ok && regions() == both, "{said}");
    assert!(!served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().is_some_and(|u| u.contains("ec2."))), "no network made");
    // A region it has nothing in: its network made there first, then the account has it.
    let (ok, said) = run(&["runners", "update", "aws-acme-on-demand", "--regions=eu-north-1"]);
    assert!(ok && regions().as_array().unwrap().iter().any(|r| r == "eu-north-1"), "{said}");
    assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().is_some_and(|u| u.contains("ec2.eu-north-1"))), "its network made");
    // A region AWS will not make a network in: said, nothing kept. One whose cache bucket in the first account fails: made
    // all the same (the cache waits).
    served.aws.fail("ec2.eu-central-1", 403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>may not make networks</Message></Error></Errors></Response>", None);
    let (ok, said) = run(&["runners", "update", "aws-acme-on-demand", "--regions=eu-central-1"]);
    assert!(!ok && said.contains("eu-central-1: ") && said.contains("may not make networks"), "{said}");
    served.aws.fail_none();
    served.aws.fail("s3.us-east-1", 403, "<Error><Code>AccessDenied</Code><Message>may not make buckets</Message></Error>", None);
    let (ok, said) = run(&["runners", "update", "aws-acme-on-demand", "--regions=us-east-1"]);
    assert!(ok, "{said}");
    served.aws.fail_none();
    // AWS refusing what removing it asks: said as left; the record marked gone all the same.
    served.aws.fail("Action=DeleteRole", 403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>may not delete roles</Message></Error></ErrorResponse>", None);
    served.aws.fail("Action=DeleteVpc", 400, "<Response><Errors><Error><Code>DependencyViolation</Code><Message>still in use</Message></Error></Errors></Response>", None);
    // (Its machine still ending at first: waited for.)
    served.aws.fail("TerminateInstances|ec2.eu-west-1", 200, "<TerminateInstancesResponse><instancesSet></instancesSet></TerminateInstancesResponse>", Some(1));
    served.aws.fail("DescribeInstances|ec2.eu-west-1", 200, "<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceId>i-0123456789abcdef0</instanceId></item></instancesSet></item></reservationSet></DescribeInstancesResponse>", Some(2));
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(ok && said.contains("Removed aws-acme.") && said.contains("Left:"), "{said}");
    served.aws.fail_none();
    // Added again, its word back; then removed with AWS answering each thing, and an organization keeping it out of regions.
    answers();
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1"]);
    assert!(ok && said.contains("aws-acme runs jobs now."), "{said}");
    served.aws.fail("ec2.", 403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized to perform this operation. User: arn:aws:sts::222222222222:assumed-role/x is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy</Message></Error></Errors></Response>", None);
    let removal: Vec<&str> = include_str!("../fixtures/runners/10-cf-remove-aws.jsonl").lines().filter(|l| l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")).collect();
    served.aws.replaying(&removal.join("\n"), &[]);
    // (Nor may it list its roles: what is left there is said.)
    served.aws.fail("Action=ListRoles", 403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>may not list roles</Message></Error></ErrorResponse>", None);
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(ok && said.contains("Removed aws-acme.") && said.contains("may not list roles"), "{said}");
    // Added once more and removed with AWS answering everything: nothing left.
    served.aws.fail_none();
    answers();
    let (ok, said) = run(&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1"]);
    assert!(ok, "{said}");
    served.aws.fail("ec2.", 403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized to perform this operation. User: arn:aws:sts::222222222222:assumed-role/x is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy</Message></Error></Errors></Response>", None);
    served.aws.replaying(&removal.join("\n"), &[]);
    let (ok, said) = run(&["runners", "delete", "aws-acme", "--confirm"]);
    assert!(ok && said.contains("Removed aws-acme.") && !said.contains("Left:"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A further runner on a control plane in Modal: never `modal`, that being its own.
#[test]
fn a_further_runner_on_a_control_plane_in_modal() {
    let (served, home) = two_workspaces();
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"] = serde_json::json!({ "Modal": { "workspace": "acmeorg", "url": served.url, "plane_id": PLANE, "label": "superci" } });
    kept["key"]["planes"] = serde_json::json!([PLANE]);
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), "[]");
    let (mut child, link) = common::dashboard(&home);
    deploy_answers(&served);
    let (status, _, said) = common::post(&link, "/runners/modal", &[("account", "acme-two")]);
    assert_eq!(status, 303, "{}", words(&said));
    let record = stored(&served);
    assert!(record["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two"), "{record}");
    // By command: another workspace named goes as a further runner.
    let _ = child.kill();
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok && said.contains("modal-acme-two"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A further runner beside another further runner: each record kept whole when the other is made and removed.
#[test]
fn further_runners_side_by_side_keep_each_others_records() {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-globex"]);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok, "{said}");
    let names = |served: &superci_testkit::Served| stored(served)["runners"].as_array().unwrap().iter().filter(|r| r["gone"] != true).map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(&served), ["modal-globex", "modal-globex-functions", "modal-acme-two", "modal-acme-two-functions"]);
    let removed: Vec<&str> = include_str!("../fixtures/runners/04-modal-agent.jsonl").lines().filter(|l| l.contains("/AppList") || l.contains("/AppStop") || l.contains("/AppDelete")).collect();
    served.aws.replaying(&removed.join("\n"), &[("mdplane00002", PLANE)]);
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(ok, "{said}");
    assert_eq!(names(&served), ["modal-globex", "modal-globex-functions"]);
    let _ = std::fs::remove_dir_all(&home);
}

/// A further runner made as the control plane stops answering: made all the same, said by the provider it is.
#[test]
fn a_further_runner_made_as_the_control_plane_stops_answering_is_said_by_its_provider() {
    let (served, home) = two_workspaces();
    let written = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = written.clone();
    let front = superci_testkit::front(&served.url, move |method, target, status, body| {
        if method == "POST" && target.starts_with("/settings") { seen.store(true, std::sync::atomic::Ordering::SeqCst) }
        if method == "GET" && target.starts_with("/status") && seen.load(std::sync::atomic::Ordering::SeqCst) { (500, b"down".to_vec()) } else { (status, body) }
    });
    *served.aws.plane_url.lock().unwrap() = front.clone();
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"]["Aws"]["url"] = front.clone().into();
    std::fs::write(&file, kept.to_string()).unwrap();
    deploy_answers(&served);
    let o = common::command(&home).args(["runners", "create", "modal", "--account=acme-two"]).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(written.load(std::sync::atomic::Ordering::SeqCst) && said.contains("modal runs jobs now."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A command run with `fault` armed (see `superci_testkit::sweep`): it never panics, says what failed when it fails, and
/// `superci status` still answers after.
fn swept(fault: superci_testkit::Fault, served: superci_testkit::Served, home: &std::path::Path, command: &[&str]) -> superci_testkit::Outcome {
    served.arm(fault);
    let o = common::command(home).args(command).env("SUPERCI_PAUSE_MS", "1").env("SUPERCI_MOVE_PAUSE_MS", "50").output().unwrap();
    served.faults().stop();
    let (code, said) = (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr));
    let out = superci_testkit::Outcome::new(served);
    out.check(code >= 0 && !said.contains("panicked"), || format!("`{}` panicked or was killed: {said}", command.join(" ")));
    out.check(code == 0 || said.trim().trim_start_matches("superci:").trim().len() > 8, || format!("`{}` failed ({code}) without saying what: {said}", command.join(" ")));
    if std::env::var("SUPERCI_SWEEP_ONLY").is_ok() { eprintln!("$ superci {} → {code}\n{said}", command.join(" ")) }
    let after = common::command(home).args(["status", "--json"]).output().unwrap();
    // (A move stopped halfway leaves two control planes live: status asks which, as it should.)
    let which = String::from_utf8_lossy(&after.stdout).contains("Name one: --plane=ID");
    out.check(after.status.success() || which, || format!("`superci status` fails after the fault: {}{}", String::from_utf8_lossy(&after.stdout).chars().take(400).collect::<String>(), String::from_utf8_lossy(&after.stderr)));
    let _ = std::fs::remove_dir_all(home);
    out
}

/// A second AWS account's runners made, given a region, updated with the control plane, removed and moved, and a second
/// Modal workspace's and Cloudflare account's made, with each of the clouds' answers failing in turn.
#[test]
fn every_further_runner_command_holds_with_each_call_and_store_operation_failing() {
    let connect: Vec<&str> = include_str!("../fixtures/runners/08-move.jsonl").lines().filter(|l| (l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")) || l.contains("ec2.us-east-1")).collect();
    let connect = connect.join("\n");
    let aws = |further: bool| {
        let served = serve(own_aws);
        served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
        if further { with_further_aws(&served, &[("aws-acme", "222222222222")]) }
        for region in ["eu-west-1", "eu-west-2", "eu-west-3", "eu-central-1", "eu-north-1", "us-east-1"] { served.aws.replaying(&connect.replace("ec2.us-east-1", &format!("ec2.{region}")), &[]) }
        let home = signed_in_two_aws(&served);
        (served, home)
    };
    let organization = "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>You are not authorized to perform this operation. User: arn:aws:sts::222222222222:assumed-role/x is not authorized to perform: ec2:DescribeVpcs with an explicit deny in a service control policy</Message></Error></Errors></Response>";
    let removal: Vec<&str> = include_str!("../fixtures/runners/10-cf-remove-aws.jsonl").lines().filter(|l| l.contains("iam.amazonaws.com") && !l.contains("Action=ListRoles")).collect();
    let removal = removal.join("\n");
    let cases: Vec<(&str, Box<dyn Fn() -> (superci_testkit::Served, std::path::PathBuf) + Sync>, Vec<&str>)> = vec![
        ("aws create", Box::new(|| aws(false)), vec!["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"]),
        ("aws regions", Box::new(|| aws(true)), vec!["runners", "update", "aws-acme-on-demand", "--regions=eu-north-1"]),
        ("aws delete", Box::new(|| { let (served, home) = aws(true); served.aws.replaying(&removal, &[]); served.aws.fail("ec2.", 403, organization, None); (served, home) }), vec!["runners", "delete", "aws-acme", "--confirm"]),
        ("planes update", Box::new(|| {
            let (served, home) = aws(true);
            served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
            (served, home)
        }), vec!["planes", "update"]),
        ("modal create", Box::new(|| { let (served, home) = two_workspaces(); deploy_answers(&served); (served, home) }), vec!["runners", "create", "modal", "--account=acme-two"]),
        // (Moved: its role made for the new control plane in the further account; the new one lives on with it.)
        ("aws move", Box::new(|| {
            let (served, home) = aws(true);
            served.connect_github();
            let kept = served.aws.clone();
            let new = superci_testkit::serve_as("testplane0002", move |c| { c.read_settings(&kept.settings("testplane0002")); own_aws(c) });
            for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
            served.aws.put("/superci/testplane0002/CONTAINERS", "off");
            let file = home.join("workspaces/default/sign-ins.json");
            let mut signed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            signed["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": new.url, "plane_id": "testplane0002", "label": "superci" } }]);
            signed["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
            std::fs::write(&file, signed.to_string()).unwrap();
            served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
            std::mem::forget(new);
            (served, home)
        }), vec!["planes", "move", "testplane0002"]),
        ("modal move", Box::new(|| { let (served, new, home) = moving_further_runners(); std::mem::forget(new); (served, home) }), vec!["planes", "move", "testplane0002"]),
        // (Made again with its Functions in the order: deployed with it.)
        ("modal again", Box::new(|| {
            let (served, home) = two_workspaces();
            with_further(&served, PLANE, &["modal-acme-two"]);
            deploy_answers(&served);
            let o = common::command(&home).args(["runners", "order", "modal-acme-two-functions", "modal-acme-two", "modal"]).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            deploy_answers(&served);
            (served, home)
        }), vec!["runners", "create", "modal", "--account=acme-two"]),
        ("cloudflare create", Box::new(|| {
            let served = serve(|_| {});
            served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
            let (first, second) = ("1111111111111111111111111111111a", "2222222222222222222222222222222b");
            served.aws.put(&format!("/superci/{PLANE}/LAUNCHERS"), &format!(r#"[{{"cloud":"cloudflare","url":"https://superci-runners-{PLANE}.acme.workers.dev"}}]"#));
            let home = common::signed_in(&served);
            let file = home.join("workspaces/default/sign-ins.json");
            let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
            std::fs::write(&file, kept.to_string()).unwrap();
            let cf_calls: Vec<String> = include_str!("../fixtures/runners/03-cf-agent.jsonl").lines().filter(|l| !l.contains("/accounts?per_page") && !l.contains("iam.amazonaws.com") && !l.contains("api.modal.com")).map(str::to_string).collect();
            served.aws.replaying(&cf_calls.join("\n"), &[(first, second), ("awsplane0001", PLANE), ("subdomain\\\":\\\"acme", "subdomain\\\":\\\"globex")]);
            served.aws.replaying(&cf_calls.join("\n"), &[("awsplane0001", PLANE)]);
            served.aws.fail("/accounts?per_page", 200, &format!(r#"{{"result":[{{"id":"{first}","name":"Acme","type":"standard"}},{{"id":"{second}","name":"Globex Inc","type":"standard"}}],"success":true,"errors":[],"messages":[]}}"#), None);
            { let mut w = served.aws.workers.lock().unwrap(); w.insert(format!("superci-runners-{PLANE}.globex.workers.dev"), served.url.clone()); }
            (served, home)
        }), vec!["runners", "create", "cloudflare", "--account=2222222222222222222222222222222b"]),
    ];
    for (name, setup, command) in &cases {
        superci_testkit::sweep(&format!("further runners: {name}"), |fault| { let (served, home) = setup(); swept(fault, served, &home, command) });
    }
}

/// A further AWS account's sign-in that AWS will not renew: adding it, giving it a region and removing it each stop,
/// asking for the sign-in again, and nothing is changed in the control plane.
#[test]
fn a_further_aws_accounts_sign_in_aws_will_not_renew_stops_each_change() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    let home = signed_in_two_aws(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["aws_more"][0]["creds"]["expires_at_ms"] = 0.into();
    kept["aws_more"][0]["refresh_token"] = "refresh-for-tests".into();
    kept["aws_more"][0]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    let due = kept.to_string();
    served.aws.fail("signin.aws.amazon.com/v1/token", 400, r#"{"error":"INSUFFICIENT_PERMISSIONS"}"#, None);
    for (further, args) in [(false, &["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=aws-acme"][..]),
        (true, &["runners", "update", "aws-acme-on-demand", "--regions=eu-north-1"]), (true, &["runners", "delete", "aws-acme", "--confirm"])] {
        std::fs::write(&file, &due).unwrap();
        if further { with_further_aws(&served, &[("aws-acme", "222222222222")]) }
        let before = stored_or_none(&served);
        let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success() && said.contains("Sign in"), "{args:?}: {said}");
        assert_eq!(stored_or_none(&served), before, "{args:?}: nothing changed");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// What the control plane keeps of where jobs run, its runners and accounts (none: no settings yet).
fn stored_or_none(served: &superci_testkit::Served) -> Option<serde_json::Value> {
    let kept: serde_json::Value = serde_json::from_str(served.store.0.lock().unwrap().get("settings")?).ok()?;
    Some(serde_json::json!({ "routing": kept["routing"], "launchers": kept["launchers"], "accounts": kept["accounts"], "runners": kept["runners"] }))
}

/// A further AWS account removed while the control plane will not forget its connection: said, and its role and
/// network left where they are.
#[test]
fn a_further_aws_account_the_control_plane_will_not_forget_is_said() {
    let served = serve(own_aws);
    served.aws.put(&format!("/superci/{PLANE}/CONTAINERS"), "off");
    with_further_aws(&served, &[("aws-acme", "222222222222")]);
    let home = signed_in_two_aws(&served);
    let front = superci_testkit::front(&served.url, |method, target, status, body| {
        if method == "POST" && target.starts_with("/aws/forget") { (500, br#"{"error":"the store is down"}"#.to_vec()) } else { (status, body) }
    });
    *served.aws.plane_url.lock().unwrap() = front.clone();
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"]["Aws"]["url"] = front.into();
    std::fs::write(&file, kept.to_string()).unwrap();
    let o = common::command(&home).args(["runners", "delete", "aws-acme", "--confirm"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("500"), "{said}");
    assert!(!served.aws.asked.lock().unwrap().iter().any(|a| a["asked"].as_str().is_some_and(|b| b.contains("DeleteRole"))), "its role left");
    let _ = std::fs::remove_dir_all(&home);
}
