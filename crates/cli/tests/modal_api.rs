//! A Modal control plane from the CLI and the dashboard, Modal's API answering as it answered a real workspace with
//! nothing of SuperCI's in it yet (`fixtures/modal`, recorded 2026-10-28 with a control plane made for it and deleted
//! after; its dashboard step 2026-10-16), and the control plane itself one served on this computer, reading what its
//! settings Dict was given.
mod common;
use common::{part, words};
use superci_testkit::{serve, serve_as};

#[test]
fn a_modal_control_plane_is_deployed_given_its_sandboxes_shown_and_deleted_as_modal_answered() {
    // The workspace's control plane in AWS, serving GitHub (the one in use); the Modal one comes beside it.
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let modal = serve_as("mdplane00001", move |c| {
        let settings = aws.modal_plane_settings();
        c.read_settings(&settings);
        c.own_cloud = "modal".into();
        c.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
    });
    modal.aws.params.lock().unwrap().remove("/superci/mdplane00001/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("acmeorg--superci-plane-mdplane00001.modal.run".into(), modal.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/modal/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[]);
    let run = |args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", "mdplane00001").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // Deployed: its images built in the workspace, `modal deploy` run in a sandbox, answering.
    step("02-deploy");
    let (ok, said) = run(&["planes", "create", "modal"]);
    assert!(ok && said.contains("The control plane is running: https://acmeorg--superci-plane-mdplane00001.modal.run (Modal · acmeorg)"), "{said}");
    // Found in the workspace, and read.
    step("03-use");
    let (ok, said) = run(&["planes", "list"]);
    assert!(ok && said.contains("mdplane00001") && said.contains("Modal · acmeorg"), "{said}");
    let (ok, said) = run(&["status", "--plane=mdplane00001"]);
    assert!(ok && said.contains("Control plane Modal · acmeorg"), "{said}");
    // Its own sandboxes: switched on in its settings.
    step("04-sandboxes");
    let (ok, said) = run(&["runners", "create", "modal", "--plane=mdplane00001"]);
    assert!(ok && said.contains("modal runs jobs now."), "{said}");
    let (ok, said) = run(&["runners", "list", "--plane=mdplane00001"]);
    assert!(ok && said.contains("1. modal"), "{said}");
    // The dashboard: its Runners page, Modal's spending read.
    step("05-dashboard");
    let (_child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&c=mdplane00001&fragment=1").1) }).find(|p| p.contains("Modal") && !p.contains("Reading your control plane")).unwrap_or_default();
    assert!(page.contains("Modal"), "{page}");
    drop(_child);
    // Taken out, then deleted (its app stopped and its Dicts deleted, by Modal's own client in a sandbox).
    step("06-remove");
    let (ok, said) = run(&["runners", "delete", "modal", "--plane=mdplane00001", "--confirm"]);
    assert!(ok && said.contains("Removed Modal."), "{said}");
    step("07-delete");
    let (ok, said) = run(&["planes", "delete", "mdplane00001", "--confirm"]);
    assert!(ok && said.contains("Deleted Modal · acmeorg."), "{said}");
    assert!(served.aws.unanswered.lock().unwrap().iter().all(|u| !u.contains("api.modal.com")), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}
