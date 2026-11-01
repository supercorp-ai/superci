//! A further Modal runner from the CLI and the dashboard: a second Modal workspace signed in to, its launcher deployed
//! as Modal answered a deploy of one, and everything after it under its own word.
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
    let deployed: Vec<&str> = include_str!("../fixtures/runners/04-modal-agent.jsonl").lines().filter(|l| ["/AuthTokenGet\"", "/AppList\"", "/EnvironmentGetOrCreate\"", "/AppGetOrCreate\"", "/ImageGetOrCreate\"", "/SandboxCreate\"", "/SandboxWait\""].iter().any(|m| l.contains(m))).collect();
    served.aws.replaying(&deployed.join("\n"), &[("awsplane0001", PLANE)]);
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
    assert_eq!(kept["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two" }]), "{kept}");
    assert_eq!(kept["accounts"], serde_json::json!([{ "name": "modal-acme-two", "provider": "modal", "cloud_id": "acme-two" }]), "{kept}");
    assert!(kept["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two" && a["url"].as_str().unwrap().starts_with("https://acme-two--")), "{kept}");
    // Listed, and changed, by its word; the first stays as it was.
    let (ok, said) = run(&["runners", "update", "modal-acme-two", "--max-jobs=3", "--monthly-usd=25"]);
    assert!(ok && said.contains("modal-acme-two"), "{said}");
    let order = stored(&served)["routing"]["order"].clone();
    assert!(order.as_array().unwrap().iter().any(|p| p["cloud"] == "modal-acme-two" && p["max_jobs"] == 3 && p["monthly_usd"] == 25.0), "{order}");
    let (ok, said) = run(&["runners", "order", "modal-acme-two", "modal"]);
    assert!(ok && said.contains("Order: modal-acme-two, modal."), "{said}");
    // The dashboard: its row, under its word, with Modal's logo and its workspace; nothing left to add.
    let (mut child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&fragment=1").1) }).find(|p| p.contains("modal-acme-two")).unwrap_or_default();
    assert!(page.contains("Sandboxes in the Modal workspace acme-two"), "{page}");
    assert!(!words(&part(&link, "/?p=add-runners&fragment=1").1).contains("as the runner"));
    let _ = child.kill();
    // Asked for again under another word, or its word asked for by another workspace: a word is for good.
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two", "--name=modal-staging"]);
    assert!(!ok && said.contains("runs jobs as modal-acme-two"), "{said}");
    // Updating the control plane deploys both, each in its own workspace; one not signed in to is named.
    // Deleted: out of the order and the launchers, its record kept, marked gone.
    served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/modal/06-remove.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(ok && said.contains("Removed modal-acme-two."), "{said}");
    let kept = stored(&served);
    assert_eq!(kept["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two", "gone": true }]), "{kept}");
    assert!(!kept["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two") && !kept["routing"]["order"].as_array().unwrap().iter().any(|p| p["cloud"] == "modal-acme-two"), "{kept}");
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(!ok && said.contains("Unknown provider"), "{said}");
    // Its word comes back for its account, with its account as it was.
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok, "{said}");
    assert_eq!(stored(&served)["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two" }]));
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
        "runners": words.iter().map(|w| serde_json::json!({ "name": w, "account": w })).collect::<Vec<_>>() });
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

/// A move brings the further runners along: each launcher deployed again for the new control plane in its own workspace,
/// the records with them; one whose workspace is not signed in to stays behind, said.
#[test]
fn a_move_brings_further_runners_with_their_records_and_says_which_stayed_behind() {
    let (served, home) = two_workspaces();
    with_further(&served, PLANE, &["modal-acme-two", "modal-globex"]);
    let aws = served.aws.clone();
    let new = superci_testkit::serve_as("testplane0002", move |c| c.read_settings(&aws.settings("testplane0002")));
    for (k, v) in new.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    served.aws.put("/superci/testplane0002/CONTAINERS", "off");
    { let mut w = served.aws.workers.lock().unwrap(); for workspace in ["acmeorg", "acme-two"] { w.insert(format!("{workspace}--superci-runners-testplane0002.modal.run"), new.url.clone()); } }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": new.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &new.url)]);
    deploy_answers(&served);
    let (mut child, link) = common::dashboard_with(&home, &[("SUPERCI_PAUSE_MS", "1"), ("SUPERCI_MOVE_PAUSE_MS", "50")]);
    let (status, to, said) = common::post(&link, "/plane/move", &[("plane", "testplane0002")]);
    assert!(status == 303 && to == "/?p=planes", "{status} {to} {}", words(&said));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while served.store.under("moved_to").is_empty() { assert!(std::time::Instant::now() < end, "never moved"); std::thread::sleep(std::time::Duration::from_millis(100)) }
    let theirs: serde_json::Value = serde_json::from_str(&new.store.0.lock().unwrap()["settings"]).unwrap();
    assert!(theirs["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two" && a["url"].as_str().unwrap().contains("acme-two--superci-runners-testplane0002")), "{theirs}");
    assert!(!theirs["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-globex"), "{theirs}");
    assert_eq!(theirs["runners"].as_array().unwrap().len(), 2, "{theirs}");
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("modal-globex stayed behind")).unwrap_or_default();
    assert!(page.contains("sign in to the Modal workspace globex"), "{page}");
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
    assert!(ok && said.contains("Its Cloudflare launchers cloudflare-globex-inc were not updated: this sign-in does not reach their accounts."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}
