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
    // Its launcher in its own workspace, under the provider and the workspace's name.
    deploy_answers(&served);
    let (ok, said) = run(&["runners", "create", "modal", "--account=acme-two"]);
    assert!(ok && said.contains("modal runs jobs now."), "{said}");
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
