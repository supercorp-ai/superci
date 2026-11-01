//! Runner providers across clouds: a control plane in AWS given AWS's machines, Cloudflare's containers and Modal's
//! sandboxes through launchers, then everything moved to a Cloudflare control plane (AWS's machines through a role
//! it is given), and both deleted, AWS, Cloudflare and Modal answering as they answered a real account (`fixtures/
//! runners`, recorded 2026-10-16 with throwaway control planes, deleted after); each control plane served here.
mod common;
use common::{part, words};
use superci_testkit::{serve, serve_as};

#[test]
fn an_aws_control_plane_gets_every_providers_runners_then_moves_to_cloudflare_as_the_clouds_answered() {
    // The workspace's control plane in AWS, serving GitHub (the one in use); another in the same account, made here.
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let creds = superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX };
    let own = creds.clone();
    let second = serve_as("awsplane0001", move |c| {
        c.read_settings(&aws.settings("awsplane0001"));
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(own.clone()));
        c.aws_runners_off = aws.settings("awsplane0001").iter().any(|(k, v)| k == "AWS_RUNNERS" && v == "off");
    });
    second.aws.params.lock().unwrap().remove("/superci/awsplane0001/CONTAINERS");
    let aws = served.aws.clone();
    let cf = serve_as("cfplane00004", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        c.containers = settings.iter().any(|(k, v)| k == "CONTAINERS" && v == "on");
        // (As the Worker reads its AWS connection in progress: the region and the one-time token.)
        if let Some(v) = settings.iter().find(|(k, _)| k == "AWS_CONNECT").and_then(|(_, v)| serde_json::from_str::<serde_json::Value>(v).ok()) {
            c.aws_region = v["region"].as_str().map(str::to_string);
            c.aws_connect_token = v["token"].as_str().map(str::to_string);
        }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00004/CONTAINERS");
    {
        let mut w = served.aws.workers.lock().unwrap();
        w.insert("superci.acme.workers.dev".into(), cf.url.clone());
        // (The launchers' health, which the dashboard waits for once each is deployed: a server here answers.)
        for agent in ["acmeorg--superci-runners-awsplane0001.modal.run", "acmeorg--superci-runners-cfplane00004.modal.run", "superci-runners-awsplane0001.acme.workers.dev"] { w.insert(agent.into(), cf.url.clone()); }
    }
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    // (The workspace looks after the one the recording has in the account: the one served here, as it is called there.)
    kept["plane"]["Aws"]["plane_id"] = "otherplane01".into();
    kept["key"]["planes"] = serde_json::json!(["otherplane01"]);
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let step = |name: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/runners/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[("http://new-plane.test", &second.url)]);
    let run = |id: &str, args: &[&str]| {
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", id).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    step("01-deploy-aws");
    let (ok, said) = run("awsplane0001", &["planes", "create", "aws", "--region=us-east-1"]);
    assert!(ok && said.contains("The control plane is running"), "{said}");
    step("02-aws-runners");
    let (ok, said) = run("", &["runners", "create", "aws", "--plane=awsplane0001"]);
    assert!(ok && said.contains("aws runs jobs now."), "{said}");
    step("03-cf-agent");
    let (ok, said) = run("", &["runners", "create", "cloudflare", "--plane=awsplane0001"]);
    assert!(ok && said.contains("cloudflare runs jobs now."), "{said}");
    step("04-modal-agent");
    let (ok, said) = run("", &["runners", "create", "modal", "--plane=awsplane0001"]);
    assert!(ok && said.contains("modal runs jobs now."), "{said}");
    // (Written through the control plane served for it: kept in its store.)
    let agents = second.aws.settings("awsplane0001").into_iter().find(|(k, _)| k == "LAUNCHERS").map(|(_, v)| v).unwrap_or_default();
    assert!(agents.contains("\"cloud\":\"cloudflare\"") && agents.contains("\"cloud\":\"modal\""), "{agents}");
    let (ok, said) = run("", &["runners", "list", "--plane=awsplane0001"]);
    assert!(ok && said.contains("aws ") && said.contains("cloudflare") && said.contains("modal"), "{said}");
    step("06-dashboard");
    let (_child, link) = common::dashboard(&home);
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); words(&part(&link, "/?p=runners&c=awsplane0001&fragment=1").1) }).find(|p| p.contains("Modal") && !p.contains("Reading your control plane")).unwrap_or_default();
    assert!(page.contains("AWS spot") && page.contains("Cloudflare") && page.contains("Modal"), "{page}");
    drop(_child);
    // A Cloudflare control plane, then everything moved there: AWS's machines through a role it is given, Cloudflare's
    // its own containers, Modal's through an agent deployed for it.
    step("07-deploy-cf");
    let (ok, said) = run("cfplane00004", &["planes", "create", "cloudflare"]);
    assert!(ok && said.contains("The control plane is running: https://superci.acme.workers.dev"), "{said}");
    step("08-move");
    let (ok, said) = run("", &["planes", "move", "cfplane00004", "--plane=awsplane0001"]);
    assert!(ok && said.contains("Moved."), "{said}");
    // AWS's machines taken out of the Cloudflare one (its role and their network deleted), then both deleted.
    step("10-cf-remove-aws");
    let (ok, said) = run("", &["runners", "delete", "aws", "--plane=cfplane00004", "--confirm"]);
    assert!(ok && said.contains("Removed AWS."), "{said}");
    step("11-delete-cf");
    let (ok, said) = run("", &["planes", "delete", "cfplane00004", "--confirm"]);
    assert!(ok && said.contains("Deleted Cloudflare · Acme."), "{said}");
    step("12-delete-aws");
    let (ok, said) = run("", &["planes", "delete", "awsplane0001", "--confirm"]);
    assert!(ok && said.contains("Deleted AWS"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}
