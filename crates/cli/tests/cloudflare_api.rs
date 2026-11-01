//! A Cloudflare control plane from the CLI and the dashboard, Cloudflare's API answering as it answered a real account
//! (`fixtures/cloudflare`, recorded 2026-10-16 with a control plane made for it and deleted after), and the control
//! plane itself one served on this computer, reading the secrets its Worker was given as its settings.
mod common;
use common::{part, words};
use superci_testkit::{serve, serve_as};

/// The workspace signed in to Cloudflare too.
fn with_cloudflare(home: &std::path::Path) {
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
}

#[test]
fn a_cloudflare_control_plane_is_deployed_given_its_containers_shown_and_deleted_as_cloudflare_answered() {
    // The workspace's control plane in AWS, serving GitHub (the one in use); the Cloudflare one comes beside it.
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    // (The Worker's secrets are its settings.)
    let cf = serve_as("cfplane00001", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        if let Some((_, on)) = settings.iter().find(|(k, _)| k == "CONTAINERS") { c.containers = on == "on" }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00001/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    let home = common::signed_in(&served);
    with_cloudflare(&home);
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/cloudflare/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", "cfplane00001").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // Deployed: the Worker uploaded, given its address, answering.
    step("02-deploy");
    let (ok, said) = run(&["planes", "create", "cloudflare"]);
    assert!(ok && said.contains("The control plane is running: https://superci.acme.workers.dev (Cloudflare · Acme)"), "{said}");
    // Found in the account, and read.
    step("03-use");
    let (ok, said) = run(&["planes", "list"]);
    assert!(ok && said.contains("cfplane00001") && said.contains("Cloudflare · Acme"), "{said}");
    // One control plane per Cloudflare account: a second is refused.
    let (ok, said) = run(&["planes", "create", "cloudflare"]);
    assert!(!ok && said.contains("Your control plane is in this Cloudflare account already"), "{said}");
    let (ok, said) = run(&["status", "--plane=cfplane00001"]);
    assert!(ok && said.contains("Control plane Cloudflare · Acme") && said.contains("https://superci.acme.workers.dev"), "{said}");
    // Its own containers: the image prepared, the container application made, switched on.
    step("04-containers");
    let (ok, said) = run(&["runners", "create", "cloudflare", "--plane=cfplane00001"]);
    assert!(ok && said.contains("cloudflare runs jobs now."), "{said}");
    assert_eq!(served.aws.worker_settings("superci").into_iter().find(|(k, _)| k == "CONTAINERS").map(|(_, v)| v).as_deref(), Some("on"));
    let (ok, said) = run(&["runners", "list", "--plane=cfplane00001"]);
    assert!(ok && said.contains("1. cloudflare"), "{said}");
    // Secrets on Cloudflare: the Worker hands its control plane those a run is given with each request, so a
    // control plane there keeps 12, about 4.8 KB each; past either, refused before anything is written.
    {
        let mut kept = served.aws.worker_secrets.lock().unwrap();
        let worker = kept.entry("superci".into()).or_default();
        for i in 0..12 { worker.insert(format!("SECRET_KEPT{i}_ALL"), serde_json::json!({ "name": format!("KEPT{i}"), "value": "v", "updated_ms": 1 }).to_string()); }
    }
    let secret = |name: &str, value: &str| {
        let o = common::command(&home).args(["secrets", "create", name, "--value-env=THE_VALUE", "--plane=cfplane00001", "--json"]).env("THE_VALUE", value).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let said = secret("THIRTEENTH", "v");
    assert!(said.contains("keeps 12 secrets at most"), "{said}");
    let said = secret("BIGONE", &"\"".repeat(4000));
    assert!(said.contains("about 4.8 KB at most"), "{said}");
    served.aws.worker_secrets.lock().unwrap().get_mut("superci").unwrap().retain(|k, _| !k.starts_with("SECRET_"));
    // The dashboard: its Runners and Add runners pages, Cloudflare's spending read.
    step("05-dashboard");
    let (_child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&c=cfplane00001&fragment=1").1) }).find(|p| p.contains("Cloudflare") && !p.contains("Reading your control plane")).unwrap_or_default();
    assert!(page.contains("Cloudflare Containers"), "{page}");
    drop(_child);
    // Taken out, then the control plane deleted with its Worker and container application.
    step("06-remove");
    let (ok, said) = run(&["runners", "delete", "cloudflare", "--plane=cfplane00001", "--confirm"]);
    assert!(ok && said.contains("Removed Cloudflare."), "{said}");
    step("07-delete");
    let (ok, said) = run(&["planes", "delete", "cfplane00001", "--confirm"]);
    assert!(ok && said.contains("Deleted Cloudflare · Acme."), "{said}");
    // (Every call answered as recorded, but the tidy of expired keys after a key is handed over, which the real
    // account did not need: it gives up quietly.)
    assert!(served.aws.unanswered.lock().unwrap().iter().all(|u| !u.contains("cloudflare.com") || u.ends_with("/workers/scripts/superci/secrets")), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_cloudflare_control_plane_gets_modals_runners_then_everything_moves_to_a_modal_one_as_both_answered() {
    // The workspace's control plane in AWS, serving GitHub (the one in use); a Cloudflare one and a Modal one beside it,
    // each served here, reading its settings where its cloud keeps them (the Worker's secrets, the settings Dict).
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let cf = serve_as("cfplane00002", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        if let Some((_, on)) = settings.iter().find(|(k, _)| k == "CONTAINERS") { c.containers = on == "on" }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00002/CONTAINERS");
    let aws = served.aws.clone();
    let md = serve_as("mdplane00002", move |c| {
        let settings = aws.modal_plane_settings();
        c.read_settings(&settings);
        c.own_cloud = "modal".into();
        c.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
    });
    md.aws.params.lock().unwrap().remove("/superci/mdplane00002/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    served.aws.workers.lock().unwrap().insert("acmeorg--superci-plane-mdplane00002.modal.run".into(), md.url.clone());
    // (The Modal launcher's health, which the dashboard waits for once it is deployed: answered by a server here.)
    served.aws.workers.lock().unwrap().insert("acmeorg--superci-runners-cfplane00002.modal.run".into(), md.url.clone());
    let home = common::signed_in(&served);
    with_cloudflare(&home);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/move/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let run = |id: &str, args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", id).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    step("01-deploy-cf");
    let (ok, said) = run("cfplane00002", &["planes", "create", "cloudflare"]);
    assert!(ok && said.contains("The control plane is running: https://superci.acme.workers.dev"), "{said}");
    step("02-deploy-modal");
    let (ok, said) = run("mdplane00002", &["planes", "create", "modal"]);
    assert!(ok && said.contains("The control plane is running: https://acmeorg--superci-plane-mdplane00002.modal.run"), "{said}");
    // The Cloudflare one: its own containers, and Modal's sandboxes through a launcher deployed in Modal.
    step("03-cf-containers");
    let (ok, said) = run("", &["runners", "create", "cloudflare", "--plane=cfplane00002"]);
    assert!(ok && said.contains("cloudflare runs jobs now."), "{said}");
    step("04-cf-modal-agent");
    let (ok, said) = run("", &["runners", "create", "modal", "--plane=cfplane00002"]);
    assert!(ok && said.contains("Deploying the Modal launcher") && said.contains("modal runs jobs now."), "{said}");
    let agents = served.aws.worker_settings("superci").into_iter().find(|(k, _)| k == "LAUNCHERS").map(|(_, v)| v).unwrap_or_default();
    assert!(agents.contains("\"cloud\":\"modal\"") && agents.contains("acmeorg--superci-runners-cfplane00002"), "{agents}");
    let (ok, said) = run("", &["runners", "list", "--plane=cfplane00002"]);
    assert!(ok && said.contains("1. cloudflare") && said.contains("2. modal"), "{said}");
    step("06-dashboard");
    let (_child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&c=cfplane00002&fragment=1").1) }).find(|p| p.contains("Modal") && !p.contains("Reading your control plane")).unwrap_or_default();
    assert!(page.contains("Cloudflare") && page.contains("Modal"), "{page}");
    drop(_child);
    // Everything moved to the Modal one: its settings and history, then its runners (Cloudflare's through an agent
    // deployed for it, Modal's its own sandboxes), then the switch; the Cloudflare one says where it went.
    step("07-move");
    let (ok, said) = run("", &["planes", "move", "mdplane00002", "--plane=cfplane00002"]);
    assert!(ok && said.contains("Bringing your runner providers") && said.contains("Moved."), "{said}");
    assert!(served.aws.modal_plane_settings().iter().any(|(k, v)| k == "LAUNCHERS" && v.contains("\"cloud\":\"cloudflare\"")), "{:?}", served.aws.modal_plane_settings());
    assert!(served.aws.modal_plane_settings().iter().any(|(k, v)| k == "CONTAINERS" && v == "on"));
    assert_eq!(cf.store.under("moved_to").len(), 1);
    step("08-after-move");
    let (ok, said) = run("", &["runners", "list", "--plane=mdplane00002"]);
    assert!(ok && said.contains("1. modal") && said.contains("2. cloudflare"), "{said}");
    // The Cloudflare agent taken out of the Modal one; then both deleted (with what was made for each).
    step("09-md-remove-cf");
    let (ok, said) = run("", &["runners", "delete", "cloudflare", "--plane=mdplane00002", "--confirm"]);
    assert!(ok && said.contains("Removed Cloudflare."), "{said}");
    step("10-delete-cf");
    let (ok, said) = run("", &["planes", "delete", "cfplane00002", "--confirm"]);
    assert!(ok && said.contains("Deleted Cloudflare · Acme."), "{said}");
    step("11-delete-md");
    let (ok, said) = run("", &["planes", "delete", "mdplane00002", "--confirm"]);
    assert!(ok && said.contains("Deleted Modal · acmeorg."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_cloudflare_control_planes_containers_are_set_and_cloudflare_and_modal_control_planes_updated_as_both_answered() {
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let cf = serve_as("cfplane00003", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        if let Some((_, on)) = settings.iter().find(|(k, _)| k == "CONTAINERS") { c.containers = on == "on" }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00003/CONTAINERS");
    let aws = served.aws.clone();
    let md = serve_as("mdplane00003", move |c| { c.read_settings(&aws.modal_plane_settings()); c.own_cloud = "modal".into(); c.containers = false; });
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    served.aws.workers.lock().unwrap().insert("acmeorg--superci-plane-mdplane00003.modal.run".into(), md.url.clone());
    let home = common::signed_in(&served);
    with_cloudflare(&home);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/update/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let run = |id: &str, args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", id).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    step("01-deploy-cf");
    assert!(run("cfplane00003", &["planes", "create", "cloudflare"]).0);
    // (What each Modal step had Modal's own client do, in its sandbox: the last such command.)
    let last_command = || served.aws.asked.lock().unwrap().iter().rev().find(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/SandboxCreate")))
        .map(|a| { use base64::Engine; String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(a["asked"].as_str().unwrap_or_default()).unwrap_or_default()).into_owned() }).unwrap_or_default();
    step("02-deploy-modal");
    assert!(run("mdplane00003", &["planes", "create", "modal"]).0);
    // (A new one: nothing of it runs yet to stop.)
    assert!(last_command().contains("modal deploy app.py") && !last_command().contains("recreate"), "{}", last_command());
    step("03-cf-containers");
    assert!(run("", &["runners", "create", "cloudflare", "--plane=cfplane00003"]).0);
    // Its containers: where they start, GitHub's full image, how many at once (each a secret of its Worker).
    step("04-cf-settings");
    let (ok, said) = run("", &["runners", "update", "cloudflare", "--location=weur", "--max-jobs=3", "--image=https://images.example.com/full", "--plane=cfplane00003"]);
    assert!(ok && said.contains("up to 3 jobs at once"), "{said}");
    let secret = |n: &str| served.aws.worker_settings("superci").into_iter().find(|(k, _)| k == n).map(|(_, v)| v).unwrap_or_default();
    assert_eq!((secret("CF_LOCATION").as_str(), secret("CF_IMAGE").as_str()), ("weur", "https://images.example.com/full"));
    // Each updated to this version: the Worker uploaded again; Modal's app deployed again.
    step("05-update-cf");
    let (ok, said) = run("", &["planes", "update", "cfplane00003", "--plane=cfplane00003"]);
    assert!(ok && said.contains(&format!("Updated to {}", env!("CARGO_PKG_VERSION"))), "{said}");
    step("06-update-md");
    let (ok, said) = run("", &["planes", "update", "mdplane00003", "--plane=mdplane00003"]);
    assert!(ok && said.contains(&format!("Updated to {}", env!("CARGO_PKG_VERSION"))), "{said}");
    // (Its old container stopped as the new one starts, unless Modal says jobs still run in its own app.)
    assert!(last_command().contains("modal.Sandbox.list(app_id=app.app_id)") && last_command().contains("modal deploy --strategy=recreate app.py") && last_command().contains("superci-plane-mdplane00003"), "{}", last_command());
    step("07-delete-cf");
    assert!(run("", &["planes", "delete", "cfplane00003", "--confirm"]).0);
    step("08-delete-md");
    assert!(run("", &["planes", "delete", "mdplane00003", "--confirm"]).0);
    assert!(last_command().contains("modal app stop --yes superci-jobs-mdplane00003"), "{}", last_command());
    assert!(served.aws.unanswered.lock().unwrap().iter().all(|u| !(u.contains("cloudflare.com") || u.contains("modal.com")) || u.ends_with("/workers/scripts/superci/secrets")), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}
