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
    assert_eq!(kept["runners"], serde_json::json!([{ "name": "modal-acme-two", "account": "modal-acme-two", "gone": true }]), "{kept}");
    assert!(!kept["launchers"].as_array().unwrap().iter().any(|a| a["cloud"] == "modal-acme-two") && !kept["routing"]["order"].as_array().unwrap().iter().any(|p| p["cloud"] == "modal-acme-two"), "{kept}");
    let (ok, said) = run(&["runners", "delete", "modal-acme-two", "--confirm"]);
    assert!(!ok && said.contains("Say which"), "{said}");
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
    assert!(ok && said.contains("Its Cloudflare launchers cloudflare-globex-inc were not updated: this sign-in does not reach their accounts"), "{said}");
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
        (&["runners", "create", "aws", "--account=111111111111", "--region=eu-west-1"], "runs jobs as aws"), (&["runners", "create", "aws", "--account=222222222222", "--region=eu-west-1", "--name=a-word-of-twenty-three-c"], "no room for -on-demand"), (&["runners", "create", "aws", "--account=222222222222", "--region=nowhere"], "Choose a region")] {
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
    let theirs: serde_json::Value = serde_json::from_str(&new.store.0.lock().unwrap()["settings"]).unwrap();
    assert_eq!(theirs["runners"].as_array().unwrap().len(), 4, "{theirs}");
    assert!(new.store.0.lock().unwrap().contains_key("aws:connection:aws-acme"), "its connection, in the new one");
    assert!(!new.store.0.lock().unwrap().contains_key("aws:connection:aws-globex"), "not signed in to");
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("aws-globex stayed behind")).unwrap_or_default();
    assert!(page.contains("sign in to AWS account 333333333333"), "{page}");
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("testplane0002", PLANE), ("http://new-plane.test", &served.url)]);
    let (status, to, said) = common::post(&link, "/plane/delete", &[("plane", PLANE)]);
    assert!(status == 303, "{status} {to} {}", words(&said));
    let page = (0..80).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=planes&fragment=1").1) }).find(|p| p.contains("Its role in AWS account 333333333333")).unwrap_or_default();
    assert!(page.contains("sign in with AWS to remove it"), "{page}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}
