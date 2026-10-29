//! The CLI's commands that read, against a control plane served on this computer (`superci_testkit`), from a
//! workspace that keeps it and this computer's key (AWS's sign-in since ended), a run played to its end first; and
//! what changes say when there is no sign-in to make them with.
mod common;
use common::{home, json, played, superci};
use superci_testkit::serve;

#[test]
fn what_the_control_plane_has_is_read_with_the_kept_key() {
    let served = serve(|_| {});
    let home = home(&served);
    played(&served, &home.join("checkout"));

    let (code, status) = json(&home, &["status"]);
    assert_eq!(code, 0);
    assert_eq!((status["control_plane"]["version"].as_str(), status["control_plane"]["online"].as_bool(), status["control_plane"]["where"].as_str()),
        (Some(env!("CARGO_PKG_VERSION")), Some(true), Some("AWS · 111111111111 · us-east-1")));
    assert!(status["signed_in"].as_str().unwrap().contains("this machine's key"), "{status}");

    let (_, jobs) = json(&home, &["jobs", "list"]);
    let jobs = jobs["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let (_, runs) = json(&home, &["runs", "list"]);
    assert_eq!(runs["runs"].as_array().map(Vec::len), Some(1), "{runs}");

    let (_, usage) = json(&home, &["usage", "retrieve"]);
    assert_eq!((usage["period"].as_str(), usage["usage"]["jobs"].as_u64(), usage["usage"]["ran"].as_u64()), (Some("day"), Some(1), Some(1)), "{usage}");
    assert_eq!(usage["usage"]["steps"].as_array().map(Vec::len), Some(24), "an hour each");
    let (_, month) = json(&home, &["usage", "retrieve", "--period=month"]);
    assert_eq!(month["usage"]["jobs"].as_u64(), Some(1), "{month}");

    let (_, caches) = json(&home, &["caches", "list"]);
    assert_eq!((caches["on"].as_bool(), caches["size_gb"].as_u64(), caches["days"].as_u64()), (Some(true), Some(25), Some(7)), "{caches}");
    let (_, keys) = json(&home, &["keys", "list"]);
    assert_eq!(keys["keys"], serde_json::json!([{ "name": "agent", "runs": true, "until": "2100-01-01" }]));
    let (_, labels) = json(&home, &["labels", "list"]);
    assert_eq!(labels["labels"], serde_json::json!(["superci"]));
    let (_, name) = json(&home, &["name", "retrieve"]);
    assert_eq!(name["name"], "SuperCI");
    let (_, runners) = json(&home, &["runners", "list"]);
    assert_eq!(runners["order"].as_array().map(Vec::len), Some(1), "{runners}");
    let (_, connections) = json(&home, &["connections", "list"]);
    assert_eq!(connections, serde_json::json!({ "github": [], "gitlab": [], "installations": [] }));

    // Limits, and nothing spent: 0, not -0 (an empty sum of floats is -0.0).
    let limits = superci(&home, &["limits", "retrieve", "--json"]);
    let text = String::from_utf8_lossy(&limits.stdout);
    assert!(!text.contains("-0.0"), "{text}");
    let limits: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!((limits["max_cpu"].as_u64(), limits["max_hours"].as_u64()), (Some(32), Some(6)));

    // Said to a person too.
    let said = String::from_utf8_lossy(&superci(&home, &["jobs", "list"]).stdout).into_owned();
    assert!(said.contains("tests") && said.contains("passed"), "{said}");

    // What cannot be done as things stand is said as it is.
    let (code, projects) = json(&home, &["projects", "list"]);
    assert_eq!((code, projects["error"].as_str()), (1, Some("GitLab is not connected.")), "{projects}");
    let (code, wiped) = json(&home, &["caches", "delete", "--repo=acme/app"]);
    assert!(code == 2 && wiped["error"].as_str().unwrap().contains("Add --confirm"), "{wiped}");
    // A change needs a sign-in to the control plane's own cloud: asked for, and nothing changed.
    for change in [&["name", "update", "Acme"][..], &["labels", "create", "gpu"], &["limits", "update", "--max-cpu=16"], &["keys", "create", "agent", "--runs"]] {
        let (code, refused) = json(&home, change);
        assert_eq!((code, refused["needs"].as_str()), (3, Some("superci login")), "{change:?}: {refused}");
    }
    assert_eq!(json(&home, &["name", "retrieve"]).1["name"], "SuperCI");
    let _ = std::fs::remove_dir_all(&home);
}

/// What a change asked of AWS, as the dashboard asks it (`fixtures/aws/settings`: recorded from a real account).
fn recorded(name: &str) -> serde_json::Value {
    let line: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/fixtures/aws/settings/{name}.jsonl", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
    serde_json::from_str(line["asked"].as_str().unwrap()).unwrap()
}

#[test]
fn changes_are_written_where_the_control_plane_reads_them_and_it_answers_with_them() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let asked = |n: usize| { let a = served.aws.asked.lock().unwrap(); serde_json::from_str::<serde_json::Value>(a[a.len() - n]["asked"].as_str().unwrap()).unwrap() };
    let ok = |args: &[&str]| { let (code, v) = json(&home, &[args, &["--confirm"]].concat()); assert_eq!(code, 0, "{args:?}: {v}"); v };

    // Its name: asked of AWS exactly as the real change was, and read back from the control plane.
    ok(&["name", "update", "Recorded"]);
    assert_eq!(asked(1), recorded("01-name"));
    assert_eq!(json(&home, &["name", "retrieve"]).1["name"], "Recorded");
    // A label more, then taken away.
    ok(&["labels", "create", "recorded"]);
    let (labels, real) = (asked(1), recorded("03-label"));
    assert_eq!((&labels["Name"], &labels["Type"], &labels["Overwrite"]), (&real["Name"], &real["Type"], &real["Overwrite"]));
    assert_eq!(labels["Value"], r#"["recorded","superci"]"#);
    assert_eq!(json(&home, &["labels", "list"]).1["labels"], serde_json::json!(["recorded", "superci"]));
    ok(&["labels", "delete", "recorded"]);
    assert_eq!(json(&home, &["labels", "list"]).1["labels"], serde_json::json!(["superci"]));
    // The largest machine a label may ask for.
    ok(&["limits", "update", "--max-cpu=16"]);
    let (routing, real) = (asked(1), recorded("05-limits"));
    assert_eq!((&routing["Name"], &routing["Type"]), (&real["Name"], &real["Type"]));
    assert_eq!(serde_json::from_str::<serde_json::Value>(routing["Value"].as_str().unwrap()).unwrap()["max_cpu"], 16);
    assert_eq!(json(&home, &["limits", "retrieve"]).1["max_cpu"], 16);
    // The default machine.
    ok(&["machine", "update", "--cpu=8"]);
    assert_eq!(asked(1), recorded("07-machine"));
    assert_eq!(json(&home, &["machine", "retrieve"]).1["machine"], serde_json::json!({ "cpu": 8 }));
    ok(&["machine", "update", "--cpu=default"]);
    assert_eq!(json(&home, &["machine", "retrieve"]).1["machine"], serde_json::json!({}));
    // No Docker image cache for any job unless its label says so.
    ok(&["machine", "update", "--docker-cache=false"]);
    assert_eq!(json(&home, &["machine", "retrieve"]).1["machine"], serde_json::json!({ "docker_cache": false }));
    ok(&["machine", "update", "--docker-cache=default"]);
    assert_eq!(json(&home, &["machine", "retrieve"]).1["machine"], serde_json::json!({}));
    // With --dry-run, what it would do, and nothing done.
    let before = served.aws.asked.lock().unwrap().len();
    let (code, would) = json(&home, &["name", "update", "Other", "--dry-run"]);
    assert_eq!((code, would["dry_run"].as_bool(), would["would"].as_str(), would["form"]["to"].as_str()), (0, Some(true), Some("call it Other"), Some("/name")), "{would}");
    assert_eq!((served.aws.asked.lock().unwrap().len(), json(&home, &["name", "retrieve"]).1["name"].as_str()), (before, Some("Recorded")));
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn runners_limits_the_cache_and_keys_are_set_and_read_back() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let ok = |args: &[&str]| { let (code, v) = json(&home, args); assert_eq!(code, 0, "{args:?}: {v}"); v };
    let routing = || serde_json::from_str::<serde_json::Value>(&served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| v).unwrap_or("{}".into())).unwrap();

    // A provider's jobs at once, its money a month, where its machines run; then back as they were.
    let runners = ok(&["runners", "update", "cloudflare", "--max-jobs=20", "--monthly-usd=50", "--location=weur"]);
    assert_eq!((runners["order"][0]["max_jobs"].as_u64(), runners["order"][0]["monthly_usd"].as_f64(), runners["cloudflare_location"].as_str()), (Some(20), Some(50.0), Some("weur")), "{runners}");
    assert_eq!(routing()["order"][0], serde_json::json!({ "cloud": "cloudflare", "max_jobs": 20, "monthly_usd": 50.0 }));
    // (What is used is the order's to say, for every provider alike.)
    let (code, off) = json(&home, &["runners", "update", "cloudflare", "--enabled=false"]);
    assert!(code == 2 && off["error"].as_str().unwrap().contains("superci runners order"), "{off}");
    let (code, regions) = json(&home, &["runners", "update", "cloudflare", "--regions=us-east-1"]);
    assert!(code == 2 && regions["error"].as_str().unwrap().contains("aws and aws-on-demand"), "{regions}");
    let back = ok(&["runners", "update", "cloudflare", "--max-jobs=none", "--monthly-usd=none"]);
    assert_eq!((back["order"][0]["taken_out"].as_bool(), back["order"][0]["max_jobs"].as_u64()), (Some(false), None), "{back}");
    let (code, wrong) = json(&home, &["runners", "update", "nowhere", "--max-jobs=2"]);
    assert!(code == 2 && wrong["error"].as_str().unwrap().contains("one of cloudflare"), "{wrong}");

    // The longest job, and what the CI may spend in a month and a day.
    assert_eq!(ok(&["limits", "update", "--max-hours=12"])["max_hours"], 12);
    let money = ok(&["limits", "update", "--monthly-usd=100", "--daily-usd=10"]);
    assert_eq!((money["monthly_usd"].as_f64(), money["daily_usd"].as_f64()), (Some(100.0), Some(10.0)), "{money}");
    assert_eq!((routing()["monthly_usd"].as_f64(), routing()["daily_usd"].as_f64()), (Some(100.0), Some(10.0)));
    let (code, refused) = json(&home, &["limits", "update", "--max-hours=500"]);
    assert!(code != 0 && refused["error"].as_str().unwrap().contains("120 hours"), "{refused}");
    // The AWS account's own limit (no budget in AWS itself: nothing asked of AWS for it).
    let account = ok(&["accounts", "update", "aws", "--monthly-usd=80", "--daily-usd=8"]);
    assert_eq!((account["monthly_usd"].as_f64(), account["daily_usd"].as_f64(), account["aws_budget"].as_str()), (Some(80.0), Some(8.0), None), "{account}");

    // The cache: what a repository keeps, how long an unused entry stays, and off.
    let cache = ok(&["caches", "update", "--size-gb=50", "--days=3"]);
    assert_eq!((cache["size_gb"].as_u64(), cache["days"].as_u64()), (Some(50), Some(3)), "{cache}");
    assert_eq!(ok(&["caches", "update", "--enabled=false"])["on"], false);
    let (code, wiped) = json(&home, &["caches", "delete", "--repo=acme/app", "--confirm"]);
    assert_eq!(code, 0, "{wiped}");

    // A run key: made (kept by the control plane as its hash), listed, ended.
    let key = ok(&["keys", "create", "builder", "--runs", "--days=7"]);
    assert_eq!((key["name"].as_str(), key["runs"].as_bool()), (Some("builder"), Some(true)), "{key}");
    let secret = key["env"]["SUPERCI_KEY"].as_str().unwrap().to_string();
    let kept: Vec<(String, String)> = served.aws.settings(superci_testkit::PLANE).into_iter().filter(|(k, _)| k.starts_with("READ_KEY_") && k.ends_with("_BUILDER")).collect();
    assert!(kept.len() == 1 && !kept[0].1.contains(&secret) && kept[0].1.ends_with(":runs"), "{kept:?}");
    assert!(ok(&["keys", "list"])["keys"].as_array().unwrap().iter().any(|k| k["name"] == "builder"));
    // It reads with no sign-in.
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).args(["jobs", "list", "--json"]).env("SUPERCI_HOME", common::temp("bare")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &secret).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // A read key beside it, said as each is.
    ok(&["keys", "create", "reader"]);
    let listed = String::from_utf8_lossy(&superci(&home, &["keys", "list"]).stdout).into_owned();
    assert!(listed.lines().any(|l| l.starts_with("builder") && l.contains("run key: reads and starts runs")) && listed.lines().any(|l| l.starts_with("reader") && l.contains("read key, until")), "{listed}");
    ok(&["keys", "delete", "reader"]);
    ok(&["keys", "delete", "builder"]);
    assert!(!ok(&["keys", "list"])["keys"].as_array().unwrap().iter().any(|k| k["name"] == "builder"));
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_run_its_job_and_the_control_plane_are_read_signed_in() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    common::played(&served, &home.join("checkout"));
    let text = |args: &[&str]| { let o = superci(&home, args); assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr)); String::from_utf8_lossy(&o.stdout).into_owned() };
    let run = json(&home, &["runs", "list"]).1["runs"][0].clone();
    let (run_id, job_id) = (run["id"].to_string(), run["job_ids"][0].to_string());

    // The run, with its jobs.
    let got = json(&home, &["runs", "retrieve", &run_id]).1;
    let one = &got["runs"][0];
    assert_eq!((one["state"].as_str(), one["run"].as_str(), one["job_list"][0]["job"].as_str(), one["job_list"][0]["ended"].as_str()), (Some("passed"), Some(".github/workflows/ci.yml"), Some("tests"), Some("passed")), "{got}");
    let said = text(&["runs", "retrieve", &run_id]);
    assert!(said.contains(".github/workflows/ci.yml") && said.contains("tests") && said.contains("passed"), "{said}");
    // Its jobs, and none of another run's (`--run`).
    let listed = json(&home, &["jobs", "list", &format!("--run={run_id}")]).1;
    assert_eq!(listed["jobs"].as_array().map(|j| j.iter().map(|j| j["id"].to_string()).collect::<Vec<_>>()), Some(vec![job_id.clone()]), "{listed}");
    assert!(json(&home, &["jobs", "list", "--run=12345"]).1["jobs"].as_array().is_some_and(|j| j.is_empty()));
    assert!(text(&["jobs", "list", "--run=12345"]).contains("No jobs of run 12345 among the latest"));
    assert_eq!(json(&home, &["jobs", "list", "--run=latest"]).0, 2);
    // Its job, with how its start went and its log (its last 1,000 bytes at least: here all of it).
    let job = json(&home, &["jobs", "retrieve", &job_id, "--bytes=200"]).1;
    assert_eq!(job["job"]["start"]["steps"].as_array().map(Vec::len), Some(2), "{job}");
    let log = job["log"].as_str().unwrap_or_default();
    assert!(job["truncated"] == false && log.contains("test 1 passed") && log.contains("test 12 passed"), "{job}");
    let said = text(&["jobs", "retrieve", &job_id]);
    assert!(said.contains("its machine was asked for") && said.contains("test 1 passed") && said.contains("test 12 passed"), "{said}");

    // Signed in: the control planes of the account, the images jobs run on in each region, the order of providers.
    let planes = json(&home, &["planes", "list"]).1;
    assert_eq!((planes["control_planes"][0]["id"].as_str(), planes["control_planes"][0]["in_use"].as_bool()), (Some("testplane0001"), Some(true)), "{planes}");
    assert!(text(&["planes", "list"]).contains("AWS · 111111111111 · us-east-1"));
    let images = json(&home, &["images", "list"]).1;
    let published: Vec<&str> = images["images"].as_array().unwrap().iter().filter(|i| i["published"] == true).filter_map(|i| i["region"].as_str()).collect();
    assert!(published.contains(&"us-east-1") && !published.contains(&"ca-central-1"), "{images}");
    assert!(text(&["images", "list"]).contains("us-east-1"));
    assert_eq!(json(&home, &["runners", "order", "cloudflare"]).1["order"], serde_json::json!(["cloudflare"]));
    let status = json(&home, &["status"]).1;
    assert_eq!(status["signed_in"], "AWS account 111111111111");
    assert!(text(&["status"]).contains("AWS account 111111111111"));
    // Usage by week, said to a person.
    assert_eq!(json(&home, &["usage", "retrieve", "--period=week"]).1["usage"]["jobs"], 1);
    let said = text(&["usage", "retrieve", "--period=week"]);
    assert!(said.contains('1'), "{said}");
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_control_plane_is_updated_as_aws_answered_a_real_account() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // (The recording's control plane is this one.)
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && said.contains(&format!("Updated to {}", env!("CARGO_PKG_VERSION"))), "{said}{}", String::from_utf8_lossy(&o.stderr));
    // Its new function went up; its role, table, address and schedule were checked.
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().map(|a| format!("{} {}", a["method"].as_str().unwrap(), a["url"].as_str().unwrap())).collect();
    assert!(asked.iter().any(|a| a.starts_with("PUT https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-testplane0001/code")), "{asked:?}");
    // The layer store's converter beside it, made (new in this version), not tried again when it fails.
    assert!(asked.iter().any(|a| a == "PUT https://lambda.us-east-1.amazonaws.com/2019-09-25/functions/superci-plane-testplane0001-convert/event-invoke-config"), "{asked:?}");
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_update_waits_for_jobs_on_cloudflare_says_a_launcher_it_could_not_update_and_what_aws_refused() {
    // Two jobs running on its Cloudflare containers: the update waits for them.
    let served = serve(|_| {});
    served.connect_github();
    for name in ["one", "two"] { let (id, q) = served.github.queue(name, "superci/provider=cloudflare", served.now_ms()); served.webhook(q); served.webhook(served.github.start(id)); }
    let home = common::signed_in(&served);
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
    assert!(!o.status.success() && said.contains("2 jobs are running on Cloudflare"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
    // A launcher in Cloudflare, which is not signed in to here: the control plane is updated, the launcher said as not.
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{}/LAUNCHERS", superci_testkit::PLANE), r#"[{"cloud":"cloudflare","url":"https://superci-runners.acme.workers.dev"}]"#);
    served.aws.put(&format!("/superci/{}/CONTAINERS", superci_testkit::PLANE), "off");
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("Its Cloudflare launcher was not updated: Cloudflare is not signed in to here."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
    // AWS refusing its new code (as AWS words a throttled call): said, as AWS said it.
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    served.aws.fail(&format!("functions/superci-plane-{}/code", superci_testkit::PLANE), 429, &superci_testkit::json_error("TooManyRequestsException", "Rate exceeded"), None);
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("Rate exceeded"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane from before 0.19.13 reads its launchers as AGENTS and its devices' keys as DASHBOARD_KEY_…: its
/// update writes LAUNCHERS before the new version goes up, and takes the old names out once that answers.
#[test]
fn a_control_plane_from_before_the_new_names_has_its_settings_moved_by_its_update() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    let launchers = r#"[{"cloud":"modal","url":"https://acme--superci-runners.modal.run"}]"#;
    let plane = superci_testkit::PLANE;
    served.aws.put(&format!("/superci/{plane}/AGENTS"), launchers);
    served.aws.put(&format!("/superci/{plane}/DASHBOARD_KEY_4102444800_TEST"), &served.key);
    served.aws.put(&format!("/superci/{plane}/DASHBOARD_KEY_1700000000_GONE"), "a-key-of-another-device");
    *served.aws.old_version.lock().unwrap() = Some("0.19.12".into());
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && said.contains(&format!("Updated to {}", env!("CARGO_PKG_VERSION"))), "{said}{}", String::from_utf8_lossy(&o.stderr));
    let names: Vec<(String, String)> = served.aws.settings(plane);
    assert!(names.iter().any(|(k, v)| k == "LAUNCHERS" && v == launchers), "{names:?}");
    assert!(!names.iter().any(|(k, _)| k == "AGENTS" || k.starts_with("DASHBOARD_KEY_")), "{names:?}");
    assert!(names.iter().any(|(k, _)| k.starts_with("DEVICE_KEY_")), "{names:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn another_control_plane_is_deleted_as_aws_answered_a_real_account_and_the_one_in_use_is_not() {
    // The one in use, and another beside it in the same account (as the recording had them).
    let served = serve(|_| {});
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    let listed = json(&home, &["planes", "list"]).1;
    let ids: Vec<&str> = listed["control_planes"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
    assert!(ids.contains(&"testplane0001") && ids.contains(&"testplane0002"), "{listed}");
    // The one in use is not deleted: that is stopping SuperCI.
    let o = superci(&home, &["planes", "delete", "testplane0001", "--confirm"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("in use"), "{}", String::from_utf8_lossy(&o.stderr));
    let (code, refused) = json(&home, &["planes", "delete", "testplane0002"]);
    assert!(code != 0 && refused["error"].as_str().unwrap().contains("--confirm"), "{refused}");
    let o = superci(&home, &["planes", "delete", "testplane0002", "--confirm"]);
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && said.contains("Deleted"), "{said}{}", String::from_utf8_lossy(&o.stderr));
    // Its settings are gone from SSM, its function, table and role from AWS; the one in use keeps its own.
    assert!(served.aws.settings("testplane0002").is_empty(), "{:?}", served.aws.settings("testplane0002"));
    assert!(!served.aws.settings(superci_testkit::PLANE).is_empty());
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().map(|a| format!("{} {} {}", a["method"].as_str().unwrap(), a["url"].as_str().unwrap(), a["asked"].as_str().unwrap().chars().take(80).collect::<String>())).collect();
    for gone in ["DELETE https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-testplane0002", "superci-plane-testplane0002\"}", "Action=DeleteRole&Version=2010-05-08&RoleName=superci-plane-testplane0002",
        "DELETE https://lambda.us-east-1.amazonaws.com/2015-03-31/functions/superci-plane-testplane0002-convert", "/aws/lambda/superci-plane-testplane0002-convert",
        "DELETE https://superci-plane-testplane0002-us-east-1.s3.us-east-1.amazonaws.com/ "] {
        assert!(asked.iter().any(|a| a.contains(gone)), "{gone}: {asked:?}");
    }
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn each_command_says_what_it_needs_and_what_it_would_do_and_changes_nothing() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    // (Its settings, without this computer's own keys: a command with a cloud sign-in hands the control plane one
    // of its own the first time it reaches it, as signing in does.)
    let settings = || served.aws.settings(superci_testkit::PLANE).into_iter().filter(|(k, _)| !k.starts_with("DEVICE_KEY_")).collect::<Vec<_>>();
    let before = settings();
    // What a command needs and is not given: said, as a usage error (exit 2).
    for (args, said) in [
        (&["limits", "update"][..], "Say what to change: `superci limits update"),
        (&["caches", "update"], "Say what to change: `superci caches update"),
        (&["caches", "update", "--enabled=maybe"], "--enabled is true or false"),
        (&["caches", "delete"], "Say whose"),
        (&["keys", "create"], "Give it a name"),
        (&["keys", "create", "agent", "--days=many"], "--days is a number of days"),
        (&["keys", "create", "agent", "--runs", "--max-cpu=8"], "A key has no limits of its own"),
        (&["keys", "delete"], "Say which: `superci keys delete NAME`"),
        (&["accounts", "update", "gcp", "--monthly-usd=5"], "Say which cloud's"),
        (&["accounts", "update", "aws"], "Say what to change: `superci accounts update aws"),
        (&["accounts", "update", "aws", "--aws-budget=maybe"], "--aws-budget is none, emails, stops or lift"),
        (&["deliveries", "retrieve", "x1"], "Say which: `superci deliveries retrieve ID`"),
        (&["runs", "cancel", "x1"], "Say which: `superci runs cancel ID`"),
        (&["runs", "retrieve", "x1"], "Say which: `superci runs retrieve ID`"),
        (&["artifacts", "list"], "Say which run: --run=ID"),
        (&["artifacts", "list", "--run=x1"], "Say which run: --run=ID"),
        (&["artifacts", "retrieve", "--run=12"], "Say which artifact"),
        (&["usage", "retrieve", "--period=year"], "--period is day, week or month"),
        (&["jobs", "list", "-x"], "-x is not a flag of superci"),
        (&["images", "create", "mars-1"], "Say where: `superci images create REGION`"),
        (&["images", "create", "ca-central-1", "--from=nope"], "--from is"),
        (&["images", "update", "mars-1"], "Say where: `superci images update REGION`"),
        (&["images", "delete", "mars-1"], "Say where: `superci images delete REGION --confirm`"),
        (&["planes", "delete"], "Say which: `superci planes delete ID --confirm`"),
        (&["planes", "create", "gcp"], "Say where: `superci planes create aws --region=us-east-1`"),
        (&["planes", "create", "aws"], "Say where: --region=us-east-1"),
        (&["runners", "create", "gcp"], "Say which: `superci runners create aws`"),
        (&["runners", "delete", "gcp"], "Say which: `superci runners delete aws --confirm`"),
        (&["jobs", "retrieve", "x1"], "Say which: `superci jobs retrieve ID`"),
        (&["login", "gcp"], "Say which: `superci login aws`"),
    ] {
        let (code, v) = json(&home, args);
        assert!(code == 2 && v["error"].as_str().unwrap_or_default().contains(said), "{args:?}: {code} {v}");
    }
    // Signed in already: said, and no sign-in begun (a further AWS account is `--add`).
    for args in [&["login", "aws"][..], &["login"]] {
        let o = superci(&home, args);
        assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("SuperCI is signed in on this computer"), "{args:?}: {}", String::from_utf8_lossy(&o.stdout));
    }
    // What deletes needs --confirm; with --dry-run, what it would do, and nothing done.
    let (code, v) = json(&home, &["caches", "delete", "--repo=acme/app"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("--confirm"), "{v}");
    for (args, would) in [
        (&["caches", "delete", "--repo=acme/app", "--confirm", "--dry-run"][..], "take every cache entry of acme/app out"),
        (&["caches", "delete", "node-modules-1", "--repo=acme/app", "--scope=refs/heads/main", "--confirm", "--dry-run"], "take the cache entry node-modules-1 of acme/app out"),
        (&["keys", "create", "bot", "--dry-run"], "make a key named bot (a read key: it only reads), good for 30 days"),
        (&["keys", "create", "bot", "--runs", "--days=7", "--dry-run"], "make a key named bot (a run key: it reads and starts runs from a computer), good for 7 days"),
        (&["keys", "delete", "agent", "--dry-run"], "end the key named agent"),
        (&["accounts", "update", "aws", "--monthly-usd=50", "--aws-budget=none", "--dry-run"], "no budget in AWS itself"),
        (&["runs", "cancel", "12", "--dry-run"], "cancel run 12"),
        (&["images", "delete", "ca-central-1", "--confirm", "--dry-run"], "remove your copy of the image jobs run on from ca-central-1"),
        (&["runners", "delete", "aws", "--only-here", "--confirm", "--dry-run"], "leaving what SuperCI made for it there"),
        (&["runners", "create", "aws", "--dry-run"], "let aws run jobs for the control plane in use"),
        (&["planes", "delete", "testplane0002", "--confirm", "--dry-run"], "delete the control plane testplane0002"),
        (&["planes", "allow", "--dry-run"], "The AWS role of the control plane testplane0001 has what this version asks for: nothing would change."),
        (&["planes", "update", "--dry-run"], &format!("deploy the control plane in use (AWS · 111111111111 · us-east-1) again: it is on {} already", env!("CARGO_PKG_VERSION"))),
        (&["leave", "--confirm", "--dry-run"], "stop SuperCI"),
        (&["leave", "--plane=testplane0001", "--confirm", "--dry-run"], "stop the control plane testplane0001"),
    ] {
        let (code, v) = json(&home, args);
        assert!(code == 0 && v["dry_run"] == true && v["would"].as_str().unwrap_or_default().contains(would), "{args:?}: {code} {v}");
    }
    // A run not among those kept; deliveries GitHub has (none here).
    let (code, v) = json(&home, &["runs", "retrieve", "999"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("not among the latest"), "{v}");
    let (code, v) = json(&home, &["deliveries", "list"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(settings(), before, "nothing changed");

    // With a read key (a coding agent's, elsewhere): what looks works, what changes is refused.
    let made = json(&home, &["keys", "create", "reader"]).1;
    // A key's name, how long it lasts, one there already, one not there: each said, nothing written.
    for (args, said) in [(&["keys", "create", "ci-2"][..], "A key's name is letters and digits"), (&["keys", "create", "abcdefghijklmnopqrstuvwxyz"], "A key's name is letters and digits"),
        (&["keys", "create", "bot", "--days=400"], "from 1 to 366 days"), (&["keys", "create", "reader"], "There is a key named reader already"), (&["keys", "delete", "nobody", "--confirm"], "There is no key named nobody")] {
        let (code, v) = json(&home, args);
        assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains(said), "{args:?}: {code} {v}");
    }
    let (plane, key) = (made["env"]["SUPERCI_PLANE"].as_str().unwrap().to_string(), made["env"]["SUPERCI_KEY"].as_str().unwrap().to_string());
    let bare = common::temp("reader-home");
    let as_reader = |args: &[&str]| {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).args(args).arg("--json").env("SUPERCI_HOME", &bare).env("SUPERCI_PLANE", &plane).env("SUPERCI_KEY", &key).output().unwrap();
        (o.status.code().unwrap_or(-1), serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap_or_default())
    };
    let (code, v) = as_reader(&["jobs", "list"]);
    assert_eq!(code, 0, "{v}");
    let (code, v) = as_reader(&["caches", "delete", "--repo=acme/app", "--confirm"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("only reads"), "{v}");
    let _ = std::fs::remove_dir_all(&bare);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_aws_accounts_own_budget_is_made_raised_made_to_stop_lifted_and_removed_as_aws_answered() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // Each step answered as a real account answered it (`fixtures/aws/budget`, its control plane's id this one's).
    let step = |file: &str| served.aws.replaying(&std::fs::read_to_string(format!("{}/fixtures/aws/budget/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap(), &[("abcdefghijkl", superci_testkit::PLANE)]);
    let ok = |args: &[&str]| { let (code, v) = json(&home, args); assert_eq!(code, 0, "{args:?}: {v}"); v };
    let refused = |args: &[&str]| { let (code, v) = json(&home, args); assert_ne!(code, 0, "{args:?}: {v}"); v["error"].as_str().unwrap_or_default().to_string() };
    // Refused before AWS is asked: a budget with no monthly limit, an address that is none, nothing to lift.
    assert!(refused(&["accounts", "update", "aws", "--aws-budget=emails", "--email=someone@example.com"]).contains("A budget in AWS needs the monthly limit"));
    assert!(refused(&["accounts", "update", "aws", "--monthly-usd=100", "--aws-budget=emails", "--email=not an address"]).contains("That is no email address"));
    assert!(refused(&["accounts", "update", "aws", "--monthly-usd=100", "--aws-budget=lift"]).contains("There is no budget in AWS to lift"));
    // Made, with AWS's emails at 80% and 100%.
    step("02-emails.jsonl");
    let made = ok(&["accounts", "update", "aws", "--monthly-usd=100", "--aws-budget=emails", "--email=someone@example.com"]);
    assert_eq!((made["monthly_usd"].as_f64(), made["aws_budget"].as_str()), (Some(100.0), Some("emails")), "{made}");
    // The limit raised: the budget kept in step, unasked.
    step("04-raise.jsonl");
    assert_eq!(ok(&["accounts", "update", "aws", "--monthly-usd=120"])["aws_budget"], "emails");
    // Made to stop new machines at the limit (AWS knew the new role only after some seconds).
    step("06-stop.jsonl");
    assert_eq!(ok(&["accounts", "update", "aws", "--aws-budget=stops"])["aws_budget"], "stops");
    // The rule taken off the control plane's role; then all of it removed.
    step("08-lift.jsonl");
    ok(&["accounts", "update", "aws", "--aws-budget=lift"]);
    step("09-remove.jsonl");
    let gone = ok(&["accounts", "update", "aws", "--aws-budget=none"]);
    assert_eq!((gone["aws_budget"].as_str(), gone["monthly_usd"].as_f64()), (None, Some(120.0)), "{gone}");
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().filter_map(|a| a["target"].as_str().map(str::to_string)).filter(|t| t.starts_with("AWSBudget")).collect();
    assert!(asked.iter().any(|t| t.ends_with("CreateBudget")) && asked.iter().any(|t| t.ends_with("DeleteBudget")), "{asked:?}");
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
}

#[test]
fn workspaces_are_listed_made_renamed_and_forgotten_from_the_cli() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let text = |args: &[&str]| { let o = superci(&home, args); (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)) };
    let (ok, said) = text(&["workspaces", "list"]);
    assert!(ok && said.contains("default") && said.contains("AWS · 111111111111 · us-east-1"), "{said}");
    // Made (as a dry run first): empty, its id from its name; a second of that id refused.
    let (code, v) = json(&home, &["workspaces", "create", "Acme", "CI", "--dry-run"]);
    assert!(code == 0 && v["would"].as_str().unwrap_or_default().contains("make an empty workspace called Acme CI, with the id acme-ci"), "{v}");
    assert!(!home.join("workspaces/acme-ci").exists());
    let (ok, said) = text(&["workspaces", "create", "Acme", "CI"]);
    assert!(ok && said.contains("The workspace Acme CI is made, with nothing in it yet. Its id is acme-ci."), "{said}");
    let (ok, said) = text(&["workspaces", "create", "Acme", "CI"]);
    assert!(!ok && said.contains("There is a workspace with the id acme-ci already"), "{said}");
    let (_, said) = text(&["workspaces", "list"]);
    assert!(said.contains("acme-ci") && said.contains("nothing set up yet"), "{said}");
    // With several, status says which one it is of.
    let (code, status) = json(&home, &["status", "--workspace=default"]);
    assert!(code == 0 && status["workspace"] == "default" && status["workspaces"] == 2, "{status}");
    // Given another id (a dry run first); one not here refused.
    let (code, v) = json(&home, &["workspaces", "update", "acme-ci", "--id=acme", "--dry-run"]);
    assert!(code == 0 && v["would"].as_str().unwrap_or_default().contains("give the workspace acme-ci the id acme"), "{v}");
    let (ok, said) = text(&["workspaces", "update", "acme-ci", "--id=acme"]);
    assert!(ok && said.contains("The workspace acme-ci has the id acme now.") && home.join("workspaces/acme").exists(), "{said}");
    let (ok, _) = text(&["workspaces", "update", "nowhere", "--id=x2"]);
    assert!(!ok);
    // Forgotten (a dry run first, and only with --confirm): signed out, nothing in its clouds deleted.
    let (code, v) = json(&home, &["workspaces", "delete", "acme", "--confirm", "--dry-run"]);
    assert!(code == 0 && v["would"].as_str().unwrap_or_default().contains("sign the workspace acme out and forget it on this machine"), "{v}");
    let (ok, said) = text(&["workspaces", "delete", "acme"]);
    assert!(!ok && said.contains("--confirm"), "{said}");
    let (ok, said) = text(&["workspaces", "delete", "acme", "--confirm"]);
    assert!(ok && said.contains("The workspace acme is forgotten on this machine.") && !home.join("workspaces/acme").exists(), "{said}");
    // The one with a control plane: its key leaves the control plane as it is signed out.
    let keys = || served.aws.settings(superci_testkit::PLANE).into_iter().filter(|(k, _)| k.starts_with("DEVICE_KEY_")).count();
    let before = keys();
    let (ok, said) = text(&["workspaces", "delete", "default", "--confirm"]);
    assert!(ok && said.contains("The workspace default is forgotten"), "{said}");
    assert!(keys() < before, "its key gone from the control plane");
    let (ok, said) = text(&["workspaces", "list"]);
    assert!(ok && said.contains("None yet"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn secrets_are_kept_for_runs_from_a_computer_from_stdin_or_a_variable_listed_without_values_and_taken_out() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let given = |args: &[&str], stdin: &str, env: &[(&str, &str)]| -> (i32, serde_json::Value) {
        use std::io::Write;
        let mut c = common::command(&home);
        c.args([args, &["--json"]].concat()).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        for (k, v) in env { c.env(k, v); }
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&o.stdout);
        (o.status.code().unwrap_or(-1), serde_json::from_str(&text).unwrap_or_else(|_| panic!("{args:?}: {text}{}", String::from_utf8_lossy(&o.stderr))))
    };
    let kept = || served.aws.settings(superci_testkit::PLANE).into_iter().filter(|(k, _)| k.starts_with("SECRET_")).collect::<Vec<_>>();
    // Nothing given: refused, with how.
    let (code, v) = given(&["secrets", "create", "NPM_TOKEN"], "", &[]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("empty"), "{v}");
    let (code, v) = given(&["secrets", "create", "GITHUB_THING"], "x", &[]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("GitHub's own"), "{v}");
    // From stdin (its last line's end left out), and from a variable; each scope a setting of its own.
    let (code, v) = given(&["secrets", "create", "npm_token"], "npm-value-0001\n", &[]);
    assert!(code == 0 && v["name"] == "NPM_TOKEN", "{v}");
    let (code, v) = given(&["secrets", "create", "NPM_TOKEN"], "again", &[]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("already"), "{v}");
    let (code, v) = given(&["secrets", "create", "NPM_TOKEN", "--repo=acme/app", "--branch=release/*", "--value-env=MY_NPM"], "", &[("MY_NPM", "npm-value-0002")]);
    assert!(code == 0, "{v}");
    let (code, v) = given(&["secrets", "create", "OTHER", "--value-env=NOT_SET_HERE"], "", &[]);
    assert!(code != 0 && v["error"] == "NOT_SET_HERE is not set.", "{v}");
    let settings = kept();
    assert_eq!(settings.iter().map(|(k, _)| k.as_str()).filter(|k| *k == "SECRET_NPM_TOKEN_ALL").count(), 1, "{settings:?}");
    assert!(settings.iter().any(|(_, v)| v.contains("\"value\":\"npm-value-0001\"")) && settings.iter().any(|(_, v)| v.contains("npm-value-0002") && v.contains("release/*")), "{settings:?}");
    // Listed: names and scopes, never a value.
    let (code, v) = given(&["secrets", "list"], "", &[]);
    assert!(code == 0 && !v.to_string().contains("npm-value"), "{v}");
    assert_eq!(v["secrets"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap(), s["scope"]["repo"].as_str())).collect::<Vec<_>>(), [("NPM_TOKEN", None), ("NPM_TOKEN", Some("acme/app"))]);
    let o = superci(&home, &["secrets", "list"]);
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("repo acme/app, branch release/*") && text.contains("every run") && !text.contains("npm-value"), "{text}");
    // A new value for one kept; none for one that is not.
    let (code, v) = given(&["secrets", "update", "NPM_TOKEN"], "npm-value-0003", &[]);
    assert!(code == 0, "{v}");
    assert!(kept().iter().any(|(_, v)| v.contains("npm-value-0003")) && !kept().iter().any(|(_, v)| v.contains("npm-value-0001")));
    let (code, v) = given(&["secrets", "update", "OTHER"], "x", &[]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("not kept"), "{v}");
    // Taken out by name and scope, when confirmed.
    let (code, _) = given(&["secrets", "delete", "NPM_TOKEN"], "", &[]);
    assert_ne!(code, 0);
    let (code, v) = given(&["secrets", "delete", "NPM_TOKEN", "--repo=acme/web", "--confirm"], "", &[]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("repo acme/app, branch release/*"), "{v}");
    let (code, v) = given(&["secrets", "delete", "NPM_TOKEN", "--confirm"], "", &[]);
    assert!(code == 0, "{v}");
    assert_eq!(kept().len(), 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_devices_that_look_after_a_ci_are_listed_with_their_names_and_one_is_taken_out() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // Another computer's dashboard, which said what it is called.
    let other = "dashboard-key-of-another-computer-0002";
    served.aws.put(&format!("/superci/{}/DEVICE_KEY_4102444800_OTHER2", superci_testkit::PLANE), other);
    let hello = ureq::post(&format!("{}/keys/hello", served.url)).header("authorization", &format!("Bearer {other}")).send_json(serde_json::json!({ "device": "Ada's laptop" })).unwrap();
    assert_eq!(hello.status(), 200);
    // Listed: this one (its own key) and the other, by name.
    let (code, v) = json(&home, &["keys", "list"]);
    let devices = v["devices"].as_array().unwrap_or_else(|| panic!("{v}"));
    assert!(code == 0 && devices.iter().any(|c| c["name"] == "DEVICE_KEY_4102444800_TEST" && c["this_device"] == true), "{v}");
    assert!(devices.iter().any(|c| c["name"] == "DEVICE_KEY_4102444800_OTHER2" && c["device"] == "Ada's laptop" && c["last_used_ms"].is_u64()), "{v}");
    let o = superci(&home, &["keys", "list"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("Ada's laptop"), "{}", String::from_utf8_lossy(&o.stdout));
    // This computer's own is not taken out here (logout does that); the other one is.
    let (code, v) = json(&home, &["keys", "delete", "DEVICE_KEY_4102444800_TEST"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("this computer's own key"), "{v}");
    let (code, v) = json(&home, &["keys", "delete", "DEVICE_KEY_4102444800_OTHER2"]);
    assert!(code == 0, "{v}");
    assert!(!served.aws.settings(superci_testkit::PLANE).iter().any(|(k, _)| k == "DEVICE_KEY_4102444800_OTHER2"), "gone from the CI");
    let gone = ureq::get(&format!("{}/keys", served.url)).header("authorization", &format!("Bearer {other}")).config().http_status_as_error(false).build().call().unwrap();
    assert_ne!(gone.status(), 200, "its key no longer reaches the CI");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn what_a_job_had_from_the_caches_docker_images_is_said_and_keeping_them_is_turned_off_and_on() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    common::played(&served, &home.join("checkout"));
    let job_id = json(&home, &["runs", "list"]).1["runs"][0]["job_ids"][0].to_string();
    // What its machine said after the job (see `layers-kept`), on its record.
    {
        let mut store = served.store.0.lock().unwrap();
        let key = format!("job:{job_id}");
        let mut j: serde_json::Value = serde_json::from_str(&store[&key]).unwrap();
        j["images"] = serde_json::json!({ "served": 103, "served_bytes": 2_700_000_000u64, "pulled": 12, "pulled_bytes": 300_000_000u64, "uploaded": 12, "uploaded_bytes": 300_000_000u64, "at": 1 });
        store.insert(key, j.to_string());
    }
    let (code, v) = json(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]);
    assert_eq!((code, v["job"]["images"].as_str()), (0, Some("103 layers from the cache (2.7 GB); 12 layers pulled (300 MB), kept for the next job")), "{v}");
    let said = String::from_utf8_lossy(&superci(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]).stdout).into_owned();
    assert!(said.contains("Docker images: 103 layers from the cache (2.7 GB); 12 layers pulled (300 MB), kept for the next job."), "{said}");
    // Pulled and not uploaded (or only some): said as that, never as kept.
    // (Kept: what was uploaded, and what the cache had already.)
    for (uploaded, had, said) in [(0, 0, "12 layers pulled (300 MB), none kept for the next job"), (5, 0, "12 layers pulled (300 MB), 5 of them kept for the next job (120 MB)"), (2, 10, "12 layers pulled (300 MB), kept for the next job")] {
        {
            let mut store = served.store.0.lock().unwrap();
            let key = format!("job:{job_id}");
            let mut j: serde_json::Value = serde_json::from_str(&store[&key]).unwrap();
            j["images"] = serde_json::json!({ "pulled": 12, "pulled_bytes": 300_000_000u64, "uploaded": uploaded, "uploaded_bytes": 120_000_000u64 * (uploaded > 0) as u64, "had": had, "had_bytes": 25_000_000u64 * had, "at": 1 });
            store.insert(key, j.to_string());
        }
        assert_eq!(json(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]).1["job"]["images"].as_str(), Some(said));
    }
    // Why the rest was not kept, as its machine said.
    {
        let mut store = served.store.0.lock().unwrap();
        let key = format!("job:{job_id}");
        let mut j: serde_json::Value = serde_json::from_str(&store[&key]).unwrap();
        j["images"] = serde_json::json!({ "pulled": 12, "pulled_bytes": 900_000_000u64, "uploaded": 8, "uploaded_bytes": 180_000_000u64, "missing": 3, "missing_bytes": 716_000_000u64, "failed": 1, "note": "the bucket answered 403 Forbidden", "at": 1 });
        store.insert(key, j.to_string());
    }
    assert_eq!(json(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]).1["job"]["images"].as_str(), Some("12 layers pulled (900 MB), 8 of them kept for the next job (180 MB): 3 layers with no whole file on the machine (716 MB), 1 layer not uploaded (the bucket answered 403 Forbidden)"));
    // The cache not set up on its machine: everything pulled, and why.
    {
        let mut store = served.store.0.lock().unwrap();
        let key = format!("job:{job_id}");
        let mut j: serde_json::Value = serde_json::from_str(&store[&key]).unwrap();
        j["images"] = serde_json::json!({ "pulled": 97, "pulled_bytes": 1_900_000_000u64, "had": 94, "had_bytes": 1_150_000_000u64, "uploaded": 3, "uploaded_bytes": 716_000_000u64, "off": "the kernel does not mount an EROFS file: Block device required (os error 15)", "at": 1 });
        store.insert(key, j.to_string());
    }
    assert_eq!(json(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]).1["job"]["images"].as_str(), Some("97 layers pulled (1.9 GB), kept for the next job; the cache was not set up on its machine (the kernel does not mount an EROFS file: Block device required (os error 15))"));
    // One whose keeping failed says why.
    {
        let mut store = served.store.0.lock().unwrap();
        let key = format!("job:{job_id}");
        let mut j: serde_json::Value = serde_json::from_str(&store[&key]).unwrap();
        j["images"] = serde_json::json!({ "error": "containerd: connection refused", "at": 1 });
        store.insert(key, j.to_string());
    }
    assert_eq!(json(&home, &["jobs", "retrieve", &job_id, "--bytes=100"]).1["job"]["images"], "not kept (containerd: connection refused)");
    // Kept unless turned off; what is kept, said.
    let said = |args: &[&str]| String::from_utf8_lossy(&superci(&home, args).stdout).into_owned();
    assert!(said(&["caches", "retrieve"]).contains("Docker images: kept for each job's next run (Ubuntu 26.04 machines); none yet."), "{}", said(&["caches", "retrieve"]));
    let (code, v) = json(&home, &["caches", "update", "--images=false"]);
    assert_eq!(code, 0, "{v}");
    let routing = served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| v).unwrap_or_default();
    assert!(routing.contains(r#""images_off":true"#), "{routing}");
    let (code, v) = json(&home, &["caches", "update", "--images=maybe"]);
    assert!(code == 2 && v["error"].as_str().unwrap_or_default().contains("--images is true or false"), "{v}");
    json(&home, &["caches", "update", "--images=true"]);
    let routing = served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| v).unwrap_or_default();
    assert!(!routing.contains("images_off"), "{routing}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_control_plane_deleted_in_its_cloud_is_dropped_from_the_workspace() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    assert!(String::from_utf8_lossy(&superci(&home, &["planes", "list"]).stdout).contains(superci_testkit::PLANE));
    // Deleted elsewhere (by someone else, or in AWS's console): the account has no role of it any more.
    let none = serde_json::json!({ "method": "POST", "url": "https://iam.amazonaws.com/", "target": "", "asked": "Action=ListRoles&Version=2010-05-08&MaxItems=1000", "status": 200,
        "answer": "<ListRolesResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\"><ListRolesResult><IsTruncated>false</IsTruncated><Roles></Roles></ListRolesResult></ListRolesResponse>" });
    served.aws.replaying(&format!("{none}\n{none}\n{none}\n{none}\n"), &[]);
    let said = String::from_utf8_lossy(&superci(&home, &["planes", "list"]).stdout).into_owned();
    assert!(!said.contains(superci_testkit::PLANE), "{said}");
    let kept = std::fs::read_to_string(home.join("workspaces/default/sign-ins.json")).unwrap();
    assert!(!kept.contains(&format!("\"plane_id\":\"{}\"", superci_testkit::PLANE)), "{kept}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The control plane in use no longer takes this computer's key (removed there, by another device's tidy or by
/// hand): it is found again with the sign-in, handed a new key, and read as before.
#[test]
fn a_control_plane_that_lost_this_computers_key_is_found_again_and_handed_a_new_one() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let plane = superci_testkit::PLANE;
    served.aws.params.lock().unwrap().remove(&format!("/superci/{plane}/DEVICE_KEY_4102444800_TEST"));
    let (code, v) = json(&home, &["status"]);
    assert_eq!(code, 0, "{v}");
    let keys: Vec<String> = served.aws.settings(plane).into_iter().map(|(k, _)| k).filter(|k| k.starts_with("DEVICE_KEY_")).collect();
    assert!(keys.len() == 1 && keys[0] != "DEVICE_KEY_4102444800_TEST", "{keys:?}");
    let kept = std::fs::read_to_string(home.join("workspaces/default/sign-ins.json")).unwrap();
    assert!(kept.contains(&keys[0]), "{kept}");
    // It said which device it is on, as a dashboard's key does (the CI's Keys name it).
    let noted = served.store.under(&format!("dkey:{}", keys[0]));
    assert!(noted.first().is_some_and(|(_, v)| v["device"].as_str().is_some_and(|d| !d.is_empty())), "{noted:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A key a SuperCI from before 0.19.13 kept (as DASHBOARD_KEY_…, which control planes no longer read): this version
/// keeps it under the new name and hands it under it, instead of taking it for one that expired.
#[test]
fn a_key_an_older_superci_kept_is_kept_and_handed_under_the_new_name() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let plane = superci_testkit::PLANE;
    served.aws.params.lock().unwrap().remove(&format!("/superci/{plane}/DEVICE_KEY_4102444800_TEST"));
    served.aws.put(&format!("/superci/{plane}/DASHBOARD_KEY_4102444800_TEST"), &served.key);
    let file = home.join("workspaces/default/sign-ins.json");
    let kept = std::fs::read_to_string(&file).unwrap().replace("DEVICE_KEY_4102444800_TEST", "DASHBOARD_KEY_4102444800_TEST");
    std::fs::write(&file, kept).unwrap();
    let (code, v) = json(&home, &["runs", "list"]);
    assert_eq!(code, 0, "{v}");
    // (Kept under the new name, and handed under it: the test kit's key, not a folder's, is replaced as well.)
    let kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let name = kept["key"]["name"].as_str().unwrap_or_default();
    assert!(name.starts_with("DEVICE_KEY_"), "{kept}");
    assert!(served.aws.settings(plane).iter().any(|(k, _)| k == name));
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane that answers and does not take this computer's key is said as that, with what to do: never as
/// "no runs yet" or a run not there.
#[test]
fn a_control_plane_that_does_not_take_this_computers_key_says_why() {
    let served = serve(|_| {});
    let home = common::home(&served);
    let plane = superci_testkit::PLANE;
    served.aws.params.lock().unwrap().remove(&format!("/superci/{plane}/DEVICE_KEY_4102444800_TEST"));
    let said = |args: &[&str]| { let (code, v) = json(&home, args); assert_ne!(code, 0, "{args:?}: {v}"); v["error"].as_str().unwrap_or_default().to_string() };
    // Replaced or expired (no sign-in here to hand a new one).
    for args in [&["runs", "list"][..], &["runs", "retrieve", "12345"]] {
        let e = said(args);
        assert!(e.contains("does not take this computer's key") && e.contains("superci login"), "{args:?}: {e}");
    }
    // Of a version from before the new names: it reads the key under its old name.
    *served.aws.old_version.lock().unwrap() = Some("0.19.11".into());
    let e = said(&["runs", "list"]);
    assert!(e.contains("runs SuperCI 0.19.11, which reads this computer's key under its old name"), "{e}");
    // Newer than this SuperCI: update it.
    *served.aws.old_version.lock().unwrap() = Some("99.0.0".into());
    let e = said(&["runs", "list"]);
    assert!(e.contains("newer than this SuperCI") && e.contains("npx @superci/cli@latest"), "{e}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Commands started side by side on a computer with no key yet (a coding agent's, several at once) hand the control
/// plane one key between them, not one each: the first makes it, the others take it.
#[test]
fn commands_side_by_side_hand_one_key_between_them() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept.as_object_mut().unwrap().remove("key");
    std::fs::write(&file, kept.to_string()).unwrap();
    let before = served.aws.settings(superci_testkit::PLANE).iter().filter(|(k, _)| k.starts_with("DEVICE_KEY_")).count();
    let children: Vec<_> = (0..6).map(|_| common::command(&home).args(["runs", "list", "--json"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()).collect();
    for c in children { let o = c.wait_with_output().unwrap(); assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)); }
    let keys: Vec<String> = served.aws.settings(superci_testkit::PLANE).into_iter().map(|(k, _)| k).filter(|k| k.starts_with("DEVICE_KEY_")).collect();
    assert_eq!(keys.len(), before + 1, "{keys:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// GitHub disconnected from the CLI: --confirm needed; one not connected, and the control plane's own first App, said
/// as such.
#[test]
fn github_is_disconnected_from_the_cli() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let owner = superci_testkit::github::OWNER;
    let (code, v) = json(&home, &["connections", "delete", "github", owner]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("--confirm"), "{v}");
    let (code, v) = json(&home, &["connections", "delete", "github", "globex", "--confirm"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("globex is not connected here"), "{v}");
    // The first App connected is the control plane's own: it stays with it, and that is said.
    let (code, v) = json(&home, &["connections", "delete", "github", owner, "--confirm"]);
    assert!(code != 0 && v["error"].as_str().unwrap_or_default().contains("is the first this control plane was connected with"), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Every limit spending counts against is listed, each with the command that changes it: the CI's, its AWS account's
/// (for a month and a day, with whether AWS itself emails), and a provider's own.
#[test]
fn limits_retrieve_lists_every_limit_with_the_command_that_changes_it() {
    let served = serve(|_| {});
    served.aws.put(&format!("/superci/{}/ROUTING", superci_testkit::PLANE), &serde_json::json!({ "monthly_usd": 200.0, "account_usd": { "aws": 100.0 }, "account_daily_usd": { "aws": 10.0 },
        "order": [{ "cloud": "aws" }, { "cloud": "cloudflare", "monthly_usd": 20.0 }] }).to_string());
    let home = common::signed_in(&served);
    let o = superci(&home, &["limits", "retrieve"]);
    let said = String::from_utf8_lossy(&o.stdout).into_owned();
    for line in ["The CI spent $0.00 on machines this month, of $200 allowed (`superci limits update --monthly-usd=N --daily-usd=N`)",
        "The AWS account spent $0.00 on machines this month, of $100 allowed (`superci accounts update aws --monthly-usd=N --daily-usd=N`)", "Today (UTC): $0.00 of $10 allowed.",
        "No budget in AWS itself, so AWS sends no email at 80% or 100%", "Cloudflare spent $0.00 this month, of $20 allowed (`superci runners update cloudflare --monthly-usd=N`)."] {
        assert!(said.contains(line), "{line}\n\n{said}");
    }
    let (_, v) = json(&home, &["limits", "retrieve"]);
    assert_eq!((v["aws_account"]["monthly_usd"].as_f64(), v["aws_account"]["daily_usd"].as_f64(), v["providers"][0]["cloud"].as_str()), (Some(100.0), Some(10.0), Some("cloudflare")), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The image jobs run on, copied into a region that lacks it, as AWS answered a real copy into ca-central-1: made,
/// on its way (an update says so), listed while AWS makes it and once it is there, the newest build already, removed.
#[test]
fn the_runner_image_is_copied_in_listed_and_removed_as_aws_answered_a_real_copy() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let step = |recording: &str, args: &[&str]| -> String {
        served.aws.replaying(recording, &[]);
        let o = superci(&home, args);
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        said
    };
    let said = step(include_str!("../fixtures/aws/images/01-create.jsonl"), &["images", "create", "ca-central-1"]);
    assert!(said.contains("Copying the image jobs run on into ca-central-1, from us-east-1") && said.contains("`superci images delete ca-central-1 --confirm` removes it"), "{said}");
    let said = step(include_str!("../fixtures/aws/images/02-coming.jsonl"), &["images", "update", "ca-central-1"]);
    assert!(said.contains("A copy is on its way into ca-central-1 already"), "{said}");
    let said = step(include_str!("../fixtures/aws/images/03-list-pending.jsonl"), &["images", "list"]);
    assert!(said.lines().any(|l| l.starts_with("ca-central-1") && l.contains("is being made (pending)")), "{said}");
    let said = step(include_str!("../fixtures/aws/images/04-list-there.jsonl"), &["images", "list"]);
    assert!(said.lines().any(|l| l.starts_with("ca-central-1") && l.contains("your copy ami-") && l.contains("runs-on-v2.2-ubuntu24-full-x64-")), "{said}");
    let said = step(include_str!("../fixtures/aws/images/05-update-newest.jsonl"), &["images", "update", "ca-central-1"]);
    assert!(said.contains("Your copy in ca-central-1 is of the newest build"), "{said}");
    let said = step(include_str!("../fixtures/aws/images/06-delete.jsonl"), &["images", "delete", "ca-central-1", "--confirm"]);
    assert!(said.contains("Removed your copy of the image from ca-central-1, with its storage."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A limit an older control plane does not keep is refused, saying to update it: a day's before 0.16.0, the whole CI's
/// before 0.15.0. Nothing is written.
#[test]
fn a_limit_an_older_control_plane_does_not_keep_is_refused_saying_to_update_it() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let routing = || served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| v);
    let before = routing();
    for (version, args, said) in [("0.15.3", &["limits", "update", "--monthly-usd=50", "--daily-usd=5"][..], "A limit for a day, and a budget in AWS itself, need a newer control plane"),
        ("0.14.0", &["limits", "update", "--monthly-usd=50"], "A limit for the whole CI or for a cloud account needs a newer control plane")] {
        *served.aws.old_version.lock().unwrap() = Some(version.into());
        let o = superci(&home, args);
        let e = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
        assert!(!o.status.success() && e.contains(said) && e.contains("superci planes update"), "{version}: {e}");
        assert_eq!(routing(), before, "nothing written");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A command that needs a cloud whose sign-in ended (noted by an earlier run) says so, with what to do, and changes
/// nothing: AWS's, Cloudflare's and Modal's.
#[test]
fn a_command_needing_a_sign_in_that_ended_says_so() {
    let served = serve(|_| {});
    for (flag, args, said) in [
        ("aws_ended", &["limits", "update", "--max-cpu=8"][..], "SuperCI's AWS sign-in has ended"),
        ("cloudflare_ended", &["runners", "create", "cloudflare"], "Cloudflare sign-in has expired"),
        ("modal_ended", &["runners", "create", "modal"], "Modal no longer accepts SuperCI's sign-in"),
    ] {
        let home = common::home(&served);
        let file = home.join("workspaces/default/sign-ins.json");
        let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        kept[flag] = true.into();
        std::fs::write(&file, kept.to_string()).unwrap();
        let o = superci(&home, args);
        let e = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
        assert!(!o.status.success() && e.contains(said), "{flag}: {e}");
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// A key for a coding agent: a name that cannot be one refused; a control plane too old for keys, or for keys that
/// start runs, refused with what to do. Nothing is kept.
#[test]
fn a_key_that_cannot_be_made_is_refused_saying_why() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let keys = || served.aws.settings(superci_testkit::PLANE).into_iter().filter(|(k, _)| k.starts_with("READ_KEY_")).count();
    let before = keys();
    let refused = |args: &[&str], said: &str| {
        let o = superci(&home, args);
        let e = String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout);
        assert!(!o.status.success() && e.contains(said), "{args:?}: {e}");
    };
    refused(&["keys", "create", "not-a-name"], "A key's name is letters and digits, 24 at most");
    refused(&["keys", "create", "abcdefghijklmnopqrstuvwxy"], "A key's name is letters and digits, 24 at most");
    *served.aws.old_version.lock().unwrap() = Some("0.10.0".into());
    refused(&["keys", "create", "agent"], "read keys need control plane 0.11.0 or newer");
    *served.aws.old_version.lock().unwrap() = Some("0.17.0".into());
    refused(&["keys", "create", "builder", "--runs"], "keys that start runs need control plane 0.18.0 or newer");
    assert_eq!(keys(), before, "nothing kept");
    let _ = std::fs::remove_dir_all(&home);
}

/// What a command says when a cloud's sign-in is over: one noted as ended (it asks to sign in there), one the cloud
/// refuses as the command goes (Cloudflare's 9109, Modal's Unauthenticated, as each answered): said as ended, with what to
/// do. A token given by name for the run, refused, is said as the cloud said it (signing in would not replace it).
#[test]
fn a_command_whose_cloud_sign_in_is_over_or_refused_says_which_and_what_to_do() {
    let served = serve(|_| {});
    let run = |kept_extra: &[(&str, serde_json::Value)], env: &[(&str, &str)], recording: &str, args: &[&str]| -> (bool, String) {
        let home = common::signed_in(&served);
        let file = home.join("workspaces/default/sign-ins.json");
        let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        for (k, v) in kept_extra { kept[*k] = v.clone() }
        std::fs::write(&file, kept.to_string()).unwrap();
        if !recording.is_empty() { served.aws.replaying(recording, &[]) }
        let o = common::command(&home).args(args).envs(env.iter().copied()).output().unwrap();
        let _ = std::fs::remove_dir_all(&home);
        (o.status.success(), String::from_utf8_lossy(&o.stderr).to_string() + &String::from_utf8_lossy(&o.stdout))
    };
    let cf = serde_json::json!({ "access": "example-access_token-ended", "expires_at_ms": 4102444800000u64, "refresh": "example-refresh_token-ended" });
    let modal = serde_json::json!({ "token_id": "ak-superciendedrecording00", "token_secret": "as-superciendedrecording00", "workspace": "acmeci" });
    let cf_ended = include_str!("../fixtures/cloudflare/sign-in/ended.jsonl");
    let modal_ended = include_str!("../fixtures/modal/sign-in/ended.jsonl");
    // Noted as ended, and the command needs that cloud: said as ended.
    let (ok, said) = run(&[("cloudflare_ended", true.into())], &[], "", &["planes", "create", "cloudflare"]);
    assert!(!ok && said.contains("Cloudflare sign-in has expired"), "{said}");
    let (ok, said) = run(&[("modal_ended", true.into())], &[], "", &["planes", "create", "modal"]);
    assert!(!ok && said.contains("Modal no longer accepts"), "{said}");
    // Refused by the cloud as the command goes: said as ended.
    let (ok, said) = run(&[("cloudflare", cf.clone())], &[], cf_ended, &["planes", "create", "cloudflare"]);
    assert!(!ok && said.contains("Cloudflare sign-in has expired"), "{said}");
    let (ok, said) = run(&[("modal", modal.clone())], &[], modal_ended, &["planes", "create", "modal"]);
    assert!(!ok && said.contains("Modal no longer accepts"), "{said}");
    // A token given for the run, refused: as the cloud said it, not as a sign-in that ended.
    let (ok, said) = run(&[], &[("SUPERCI_CLOUDFLARE_TOKEN", "a-token-cloudflare-refuses")], cf_ended, &["planes", "create", "cloudflare"]);
    assert!(!ok && !said.contains("sign-in has expired") && (said.contains("9109") || said.contains("Invalid access token")), "{said}");
    let (ok, said) = run(&[], &[("SUPERCI_MODAL_TOKEN_ID", "ak-superciendedrecording00"), ("SUPERCI_MODAL_TOKEN_SECRET", "as-superciendedrecording00")], modal_ended, &["planes", "create", "modal"]);
    assert!(!ok && said.contains("Token not found"), "{said}");
}

#[test]
fn a_region_is_added_with_its_network_and_its_network_deleted_as_aws_answered_a_real_account() {
    let served = serve(|c| {
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
    });
    let home = common::signed_in(&served);
    let plane = superci_testkit::PLANE;
    // Stockholm beside N. Virginia: its network made in the account first, then the control plane told.
    served.aws.replaying(include_str!("../fixtures/aws/regions/add.jsonl"), &[("mixplane0001", plane)]);
    let o = superci(&home, &["runners", "update", "aws", "--regions=us-east-1,eu-north-1"]);
    assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let regions = served.aws.settings(plane).into_iter().find(|(k, _)| k == "AWS_REGIONS").map(|(_, v)| v).unwrap_or_default();
    assert_eq!(regions, r#"["us-east-1","eu-north-1"]"#);
    let asked = |action: &str| served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().unwrap_or_default().contains("eu-north-1") && a["asked"].as_str().unwrap_or_default().contains(&format!("Action={action}")));
    assert!(asked("CreateVpc") && asked("CreateSubnet") && asked("CreateVpcEndpoint"), "its network made there");
    // Its network there deleted (with its cache bucket), as removing AWS deletes it.
    served.aws.replaying(include_str!("../fixtures/aws/regions/delete.jsonl"), &[("mixplane0001", plane)]);
    let o = superci(&home, &["debug", "networks", "eu-north-1"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("deleted in eu-north-1"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(asked("DeleteVpc"), "its network deleted");
    let o = superci(&home, &["debug", "networks", "mars-1"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("Say which region"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_control_plane_is_updated_with_its_cloudflare_and_modal_launchers_as_a_real_account_answered() {
    let served = serve(|_| {});
    let plane = superci_testkit::PLANE;
    served.aws.put(&format!("/superci/{plane}/LAUNCHERS"), r#"[{"cloud":"cloudflare","url":"https://superci-runners-testplane0001.acme.workers.dev"},{"cloud":"modal","url":"https://acme--superci-runners-testplane0001.modal.run"}]"#);
    served.aws.put(&format!("/superci/{plane}/CONTAINERS"), "off");
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.replaying(include_str!("../fixtures/aws/planes/update-with-launchers.jsonl"), &[("mixplane0001", plane), ("http://new-plane.test", &served.url)]);
    // (The launchers' health, which the update waits for: a server here answers for each.)
    { let mut w = served.aws.workers.lock().unwrap(); for host in [format!("superci-runners-{plane}.acme.workers.dev"), format!("acmeorg--superci-runners-{plane}.modal.run")] { w.insert(host, served.url.clone()); } }
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("Updated to") && !said.contains("was not updated"), "{said}");
    // Its Cloudflare launcher went up with it, on the image of GitHub's runner its containers run.
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().map(|a| format!("{} {}", a["method"].as_str().unwrap_or_default(), a["url"].as_str().unwrap_or_default())).collect();
    assert!(asked.iter().any(|a| a.starts_with("PUT https://api.cloudflare.com/") && a.contains(&format!("/workers/scripts/superci-runners-{plane}"))), "{asked:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A job's tests as its log's lines say them, each process's apart where lines are marked by it, and what is not a
/// runner's line not taken for one; a run's other jobs, another attempt's, and one that never ran, said as they are.
#[test]
fn a_jobs_tests_are_read_by_process_from_lines_that_only_look_like_a_runners_and_a_runs_other_jobs_are_said_as_they_stand() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let (job_id, q) = served.github.queue("tests", "superci", served.now_ms());
    served.webhook(q);
    served.webhook(served.github.start(job_id));
    let log = [
        "[web] starting the server",
        "Running checks",
        "Running 3 checks using 2 processes",
        "[slot 1] Running 8 tests using 1 worker",
        "[slot 1]   ✓  1 [chromium] › e2e/a.spec.ts:3:1 › logs in (2xs)",
        "[slot 1]   ✘  2 [chromium] › e2e/a.spec.ts:5:1 › sometimes (1.0s)",
        "[slot 1]   ✓  3 [chromium] › e2e/a.spec.ts:5:1 › sometimes (retry #1)",
        "[slot 1]   -  4 [chromium] › e2e/a.spec.ts:7:1 › later",
        "[slot 1]   ✓  5 [chromium] › e2e/a.spec.ts:9:1 › odd (retry #)",
        "[slot 1]   ✓  6 [chromium] › e2e/a.spec.ts:11:1 › odder (retry #x)",
        "[slot 1]   ✓  7 [chromium › e2e/a.spec.ts:13:1 › unclosed",
        "[slot 1]   ✘  8 [chromium] › e2e/a.spec.ts:15:1 › known bug (1.0s)",
        "[slot 1]   1) [chromium] › e2e/b.spec.ts:20:1 › breaks ───",
        "[slot 1]     Error: nope",
        "[slot 1]     2 workers",
        "[slot 1]   1 failed",
        "[slot 1]   1 flaky",
        "[slot 1]   1 skipped",
        "[slot 1]   5 passed (3.0s)",
        "[slot 2] test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out",
        " FAIL  src/sum.test.ts > sums > adds",
        "AssertionError: expected 3 to be 4",
        "FAIL as it says, but not a test",
        "      Tests  1 failed (1)",
        "=== 2 failed in 1.0s ===",
    ].iter().map(|l| format!("2026-10-15T10:00:01.0000000Z {l}")).collect::<Vec<_>>().join("\n");
    served.webhook(served.github.finish(job_id, "success", &log, served.now_ms()));
    let said = String::from_utf8_lossy(&superci(&home, &["jobs", "retrieve", &job_id.to_string(), "--tests"]).stdout).into_owned();
    // Each process's own counts, a skipped and a flaky one among them; a title's own brackets kept as its own.
    assert!(said.contains("Tests: 4 failed, 9 passed, 2 skipped (vitest, pytest, playwright, cargo), of the passed 1 failed as expected, 1 flaky"), "{said}");
    assert!(said.contains("[slot 1] 1 failed, 6 passed, 1 skipped, 1 failed as expected, 1 flaky") && said.contains("[slot 2] 0 failed, 3 passed, 1 skipped"), "{said}");
    assert!(!said.contains("[web]") && said.contains("✓ [chromium] logs in (2xs)") && said.contains("✓ [chromium] odder (retry #x)") && !said.contains("unclosed"), "{said}");
    assert!(said.contains("✗ sums > adds  (src/sum.test.ts)") && said.contains("      FAIL as it says, but not a test") && said.contains("      2 workers"), "{said}");

    // Its run with two jobs more: one ended though no machine of ours is said for it, and one of the run's next
    // attempt, said with that attempt only.
    let edit = |id: u64, f: &dyn Fn(&mut serde_json::Value)| { let mut m = served.store.0.lock().unwrap(); let k = format!("job:{id}"); let mut j: serde_json::Value = serde_json::from_str(&m[&k]).unwrap(); f(&mut j); m.insert(k, j.to_string()); };
    let run = served.store.under(&format!("job:{job_id}"))[0].1["run_id"].clone();
    let (lint, q) = served.github.queue("lint", "superci", served.now_ms());
    served.webhook(q);
    served.webhook(served.github.start(lint));
    served.webhook(served.github.finish(lint, "success", "2026-10-15T10:00:01.0000000Z lint ok", served.now_ms()));
    let (again, q) = served.github.queue("again", "superci", served.now_ms());
    served.webhook(q);
    edit(lint, &|j| { j["run_id"] = run.clone(); j["machine_id"] = serde_json::Value::Null; });
    edit(again, &|j| { j["run_id"] = run.clone(); j["attempt"] = 2.into(); for k in ["cpu", "machine_type", "usd_per_hour", "waiting_usd_per_hour", "machine_id"] { j[k] = serde_json::Value::Null; } j["cloud"] = "".into(); j["state"] = "queued".into(); });
    let said = String::from_utf8_lossy(&superci(&home, &["runs", "retrieve", &run.to_string(), "--tests"]).stdout).into_owned();
    let lines: Vec<&str> = said.lines().collect();
    assert!(lines[0].ends_with("(2 jobs · 0s · $0.000 · Last to end: lint, 0s)") && lines.iter().any(|l| l.contains("lint · app") && l.contains("passed")), "{said}");
    assert!(said.contains("    No test results in its log (none of Vitest, Jest, Playwright, pytest, cargo or Go said any)."), "{said}");
    // (One not started yet: nothing more to say of it.)
    let next = lines.iter().position(|l| l.contains("· attempt 2  (1 job")).unwrap_or_else(|| panic!("{said}"));
    assert!(lines[next + 2].ends_with("starting  again · app · Build and Test"), "{said}");
    assert!(!lines[..next].iter().any(|l| l.contains("again")), "{said}");

    // Kept as a job of no code host's, of no run, and swept before it ended: its tests read all the same, with no
    // artifacts to fetch, nor a run's page.
    let edit = |id: u64, f: &dyn Fn(&mut serde_json::Value)| { let mut m = served.store.0.lock().unwrap(); let k = format!("job:{id}"); let mut j: serde_json::Value = serde_json::from_str(&m[&k]).unwrap(); f(&mut j); m.insert(k, j.to_string()); };
    edit(job_id, &|j| { j.as_object_mut().unwrap().remove("provider"); j["run_id"] = 0.into(); j["state"] = "swept".into(); j["conclusion"] = "success".into(); });
    let (code, got) = json(&home, &["jobs", "retrieve", &job_id.to_string(), "--tests"]);
    assert!(code == 0 && got["tests"]["passed"] == 9 && got["tests"]["artifacts"].is_null() && got["tests"]["artifacts_at"].is_null(), "{got}");
    // Taken for a run from a computer's, of a run not kept: none of its log here, and no artifacts said.
    edit(job_id, &|j| { j["provider"] = "local".into(); j["run_id"] = 999.into(); j["state"] = "done".into(); });
    let (code, got) = json(&home, &["jobs", "retrieve", &job_id.to_string(), "--tests"]);
    assert!(code == 0 && got["tests"]["runners"] == serde_json::json!([]) && got["tests"]["artifacts"].is_null(), "{got}");
    let _ = std::fs::remove_dir_all(&home);
}

/// With a key given by name (SUPERCI_PLANE, SUPERCI_KEY) and no sign-in, a change is refused as that key only reads;
/// what the control plane answers to a key it does not take, or at an address where it is not, is said as it
/// answered; and with this computer's key what needs no cloud sign-in is done.
#[test]
fn a_key_given_by_name_only_reads_and_what_the_control_plane_refuses_or_has_not_is_said_as_it_answered() {
    let served = serve(|_| {});
    served.connect_github();
    let home = home(&served);
    let with = |plane: &str, key: &str, args: &[&str]| {
        let o = common::command(&home).env("SUPERCI_PLANE", plane).env("SUPERCI_KEY", key).args(args).arg("--json").output().unwrap();
        (o.status.code().unwrap_or(-1), serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap_or_else(|_| panic!("{args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
    };
    for change in [&["planes", "allow"][..], &["planes", "move", "testplane0002"]] {
        let (code, v) = with(&served.url, &served.run_key, change);
        assert!(code != 0 && v["error"].as_str().is_some_and(|e| e.starts_with("SUPERCI_KEY only reads.")), "{change:?}: {v}");
    }
    // The cache asked with no key it could take (as an older control plane, with none, answers), and with one it does not take.
    let (code, v) = with(&served.url, "short", &["caches", "list"]);
    assert!(code == 1 && v["error"].as_str().is_some_and(|e| e.starts_with("This control plane keeps no cache yet")), "{v}");
    let (code, v) = with(&served.url, "superci_run_not_a_key", &["caches", "list"]);
    assert!(code == 1 && v["error"].as_str().is_some_and(|e| e.starts_with("the control plane answered 40") && !e.contains("404")), "{v}");
    // A run cancelled with a key it does not take, and one GitHub would not cancel: said as each answered.
    let (code, v) = with(&served.url, "superci_run_not_a_key", &["runs", "cancel", "123"]);
    assert!(code == 1 && v["error"].as_str().is_some_and(|e| e.starts_with("the control plane answered 40") && !e.contains("No run 123")), "{v}");
    let (id, q) = served.github.queue("tests", "superci", served.now_ms());
    served.webhook(q);
    let run = served.store.under(&format!("job:{id}"))[0].1["run_id"].to_string();
    served.github.fail("POST", "/cancel", 500, "GitHub had a bad day");
    let (code, v) = json(&home, &["runs", "cancel", &run]);
    assert!(code == 1 && v["error"].as_str().is_some_and(|e| e.contains("GitHub had a bad day")), "{v}");

    // A repository's cache of 2.5 GB, said in GB; its entries taken out with this computer's key alone.
    let now = served.now_ms();
    served.store.0.lock().unwrap().insert("cache:gh7:refs/heads/main:v1:deps-1".into(), serde_json::json!({ "id": "e1", "region": "us-east-1", "size": 2_500_000_000u64, "made": now, "used": now, "done": true }).to_string());
    let said = String::from_utf8_lossy(&superci(&home, &["caches", "list"]).stdout).into_owned();
    assert!(said.lines().any(|l| l.contains("1 entries") && l.contains("2.5 GB")), "{said}");
    let o = superci(&home, &["caches", "delete", "--repo=gh7", "--confirm"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("gh7's cache entries are out."), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(served.store.under("cache:gh7:").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

/// What is read where there is little to read: no control plane yet, a day with nothing run, no read or run keys and
/// no device's key to list (read with a run key), a job of a run that has no job by that name, a copy from where
/// no image is named; a job whose machine was not AWS's first choice says which it could not have.
#[test]
fn what_little_there_is_is_said_as_it_stands() {
    // No control plane yet: none, said as none.
    let account = superci_testkit::Aws::serve();
    let alone = common::aws_only(&account);
    let (code, status) = json(&alone, &["status"]);
    assert_eq!((code, &status["control_plane"], status["signed_in"].as_str()), (0, &serde_json::Value::Null, Some("AWS account 111111111111")), "{status}");
    let said = String::from_utf8_lossy(&superci(&alone, &["status"]).stdout).into_owned();
    assert!(said.contains("Control plane none found in the clouds signed in to. Set one up: `superci planes create aws --region=us-east-1`"), "{said}");
    let served = serve(|_| {});
    served.connect_github();
    let home = home(&served);
    // A day with nothing run: nothing saved, and no wait to say.
    let said = String::from_utf8_lossy(&superci(&home, &["usage", "retrieve"]).stdout).into_owned();
    assert!(said.contains("Spent $0.00; the same jobs on the code host's own runners: $0.00.\n") && !said.contains("saved") && !said.contains("Median wait"), "{said}");

    // No read or run key: said, with this device's key.
    let agent = format!("/superci/{}/READ_KEY_4102444800_AGENT", served.plane);
    let kept = served.aws.params.lock().unwrap().remove(&agent).unwrap();
    let said = String::from_utf8_lossy(&superci(&home, &["keys", "list"]).stdout).into_owned();
    assert!(said.starts_with("No read or run keys.") && said.contains("Device keys: each device") && said.contains("(this one)"), "{said}");
    served.aws.params.lock().unwrap().insert(agent, kept);
    // A workspace that keeps a run key in place of a device's: the control plane lists no device's keys to it.
    let reader = common::temp("run-key-home");
    std::fs::create_dir_all(reader.join("workspaces/default")).unwrap();
    let signs = std::fs::read_to_string(home.join("workspaces/default/sign-ins.json")).unwrap().replace(&served.key, &served.run_key);
    std::fs::write(reader.join("workspaces/default/sign-ins.json"), signs).unwrap();
    let (code, keys) = json(&reader, &["keys", "list"]);
    assert!(code == 0 && keys["devices"] == serde_json::json!([]), "{keys}");
    assert!(!String::from_utf8_lossy(&superci(&reader, &["keys", "list"]).stdout).contains("Device keys"));

    // A job whose machine AWS had no spot c7a.large for, nor m7a.large: said, as the run's job by its name; a name
    // the run has no job by, said with the names it has.
    let (id, q) = served.github.queue("tests", "superci", served.now_ms());
    served.webhook(q);
    let run = served.store.under(&format!("job:{id}"))[0].1["run_id"].to_string();
    {
        let mut m = served.store.0.lock().unwrap();
        let k = format!("job:{id}");
        let mut j: serde_json::Value = serde_json::from_str(&m[&k]).unwrap();
        j["machine_type"] = "c8a.large".into();
        j["asks"] = serde_json::json!([{ "at": 1, "kind": "c7a.large", "spot": true, "said": "InsufficientInstanceCapacity", "ms": 5 }, { "at": 2, "kind": "m7a.large", "spot": true, "said": "InsufficientInstanceCapacity", "ms": 5 }]);
        m.insert(k, j.to_string());
    }
    let said = String::from_utf8_lossy(&superci(&home, &["jobs", "retrieve", &format!("--run={run}"), "--job=tests"]).stdout).into_owned();
    assert!(said.contains("\nNot the first choice: AWS had no spot c7a.large or m7a.large.\n"), "{said}");
    let (code, v) = json(&home, &["jobs", "retrieve", &format!("--run={run}"), "--job=lint"]);
    assert_eq!((code, v["error"].as_str()), (1, Some(format!("Run {run} has no job lint (its jobs: tests).").as_str())), "{v}");
    // A copy of the image from a region with no image named: how to name one.
    let (code, v) = json(&common::signed_in(&served), &["images", "create", "ca-central-1", "--from=eu-west-1:not-an-image"]);
    assert_eq!((code, v["error"].as_str()), (2, Some("--from is a region and an image: --from=eu-west-1:ami-0123456789abcdef0.")), "{v}");
    // A key the control plane does not take: its status not read, as the times asked say (SUPERCI_TIMING).
    let stale = common::temp("stale-key");
    std::fs::create_dir_all(stale.join("workspaces/default")).unwrap();
    std::fs::write(stale.join("workspaces/default/sign-ins.json"), std::fs::read_to_string(home.join("workspaces/default/sign-ins.json")).unwrap().replace(&served.key, "a-key-no-control-plane-takes")).unwrap();
    let o = common::command(&stale).env("SUPERCI_TIMING", "1").args(["status"]).output().unwrap();
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("ms status (not read)") && e.contains("The control plane does not take this computer's key"), "{e}");
    let _ = std::fs::remove_dir_all(&stale);
    // The control plane not answering: where it is, and its jobs not read.
    served.down.store(true, std::sync::atomic::Ordering::SeqCst);
    let said = String::from_utf8_lossy(&superci(&home, &["status"]).stdout).into_owned();
    assert!(said.contains("Control plane AWS · 111111111111 · us-east-1 · not answering\n") && said.contains("Jobs can run  not yet:") && said.trim_end().ends_with("Its jobs could not be read just now (it did not answer with this machine's key yet). Try again in a moment."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&reader);
    let _ = std::fs::remove_dir_all(&alone);
}

/// The account's own copies of the runner image as AWS lists them: one of an older build than its maker's newest
/// (said, its update checked with a dry run), one before it removed, ones whose names carry no build; a copy asked
/// for where one is; where it would come from, by the maker's list as GitHub gives it or GitHub not giving it; and an
/// account that may use no region.
#[test]
fn the_accounts_copies_of_the_image_are_said_by_their_builds_and_an_older_one_is_removed_as_aws_answered() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let image = |id: &str, name: &str| format!("<item><imageId>{id}</imageId><imageState>available</imageState><name>{name}</name><blockDeviceMapping><item><ebs><snapshotId>snap-{}</snapshotId></ebs></item></blockDeviceMapping></item>", &id[4..]);
    let copies = |items: &[String]| format!("<DescribeImagesResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\"><imagesSet>{}</imagesSet></DescribeImagesResponse>", items.concat());
    let x64 = "runs-on-v2.2-ubuntu24-full-x64-";
    served.aws.fail("DescribeImages|tag%3Asuperci|ec2.eu-west-2.", 200, &copies(&[image("ami-0a00000000000001", &format!("{x64}20261002111320")), image("ami-0a00000000000002", &format!("{x64}20260916102456")),
        image("ami-0a00000000000003", &format!("{x64}custom")), image("ami-0a00000000000004", &format!("{x64}2026100208x320"))]), None);
    served.aws.fail("DescribeImages|tag%3Asuperci|ec2.eu-west-1.", 200, &copies(&[image("ami-0b00000000000001", &format!("{x64}20261002111320"))]), None);
    served.aws.fail("DeregisterImage", 200, "<DeregisterImageResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\"><return>true</return></DeregisterImageResponse>", None);
    served.aws.fail("DeleteSnapshot", 200, "<DeleteSnapshotResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\"><return>true</return></DeleteSnapshotResponse>", None);
    let said = |args: &[&str]| { let o = superci(&home, args); (o.status.code(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)) };
    let (code, listed) = said(&["images", "list"]);
    let line = |region: &str| listed.lines().find(|l| l.starts_with(region)).unwrap_or_default().to_string();
    assert_eq!(code, Some(0), "{listed}");
    assert!(line("eu-west-2").contains("your copy ami-0a00000000000001 (runs-on-v2.2-ubuntu24-full-x64-20261002111320): the build of 2 October 2026; one of 6 October 2026 is out")
        && line("eu-west-2").ends_with("(the copy before it is removed)"), "{listed}");
    assert!(line("eu-west-1").contains("the build of 2 October 2026; one of 6 October 2026 is out") && !line("eu-west-1").contains("removed"), "{listed}");
    // (Only the older build is removed, with its snapshot: not those whose names carry none.)
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().filter_map(|a| a["asked"].as_str().filter(|s| s.contains("DeregisterImage") || s.contains("DeleteSnapshot")).map(str::to_string)).collect();
    assert_eq!(asked, ["Action=DeregisterImage&Version=2016-11-15&ImageId=ami-0a00000000000002", "Action=DeleteSnapshot&Version=2016-11-15&SnapshotId=snap-0a00000000000002"]);
    let (code, update) = said(&["images", "update", "eu-west-2", "--dry-run"]);
    assert!(code == Some(0) && update.starts_with("Would copy the build of 6 October 2026 into eu-west-2, from eu-west-1 (") && update.contains("in place of your copy of 2 October 2026. Nothing was changed."), "{update}");
    let (code, create) = said(&["images", "create", "eu-west-2"]);
    assert!(code == Some(1) && create.contains("eu-west-2 has your copy of the image already: ami-0a00000000000001 (available)."), "{create}");

    // Where a copy into a region with none would come from: the maker's newest build that lists this image, past
    // builds that list none or whose names carry no build; then GitHub not answering for the list.
    let table = "### `ubuntu24-full-x64`\n\n| Region | AMI |\n| --- | --- |\n| us-east-1 | `ami-0123456789abcdef0` |\n";
    served.github.answer_with("GET", "runner-images-for-aws/releases", 200, &[("content-type", "application/json")], &serde_json::json!([
        { "tag_name": "linux-20261012000000", "body": "### `ubuntu24-full-arm64`\n\n| us-east-1 | `ami-0123456789abcdef1` |\n" },
        { "tag_name": "linux-2026", "body": table }, { "tag_name": "linux-2026101x000000", "body": table }, { "tag_name": "linux-20261011000000", "body": table }]).to_string());
    let (code, create) = said(&["images", "create", "eu-north-1", "--dry-run"]);
    assert!(code == Some(0) && create.contains("Would copy the image jobs run on into eu-north-1, from us-east-1 (ami-0123456789abcdef0)."), "{create}");
    served.github.fail_none();
    served.github.fail("GET", "runner-images-for-aws/releases", 503, "unavailable");
    let (code, create) = said(&["images", "create", "eu-north-1", "--dry-run"]);
    assert!(code == Some(1) && create.contains("The list of runner images could not be read from GitHub (it answered 503)."), "{create}");
    let _ = std::fs::remove_dir_all(&home);

    // An account AWS answers nothing of in any region: no region to use, its images nowhere, and nothing asked of
    // spot or on-demand machines.
    let account = superci_testkit::Aws::serve();
    account.fail("DescribeVpcs", 500, &superci_testkit::ec2_error("InternalError", "We encountered an internal error."), None);
    let alone = common::aws_only(&account);
    let said = |args: &[&str]| String::from_utf8_lossy(&superci(&alone, args).stdout).into_owned();
    assert_eq!(said(&["images", "list"]).trim(), "This account can use none of the regions SuperCI works in.");
    let look = said(&["accounts", "retrieve", "aws"]);
    assert!(look.contains("Regions       none allowed\n") && !look.contains("Spot ") && !look.contains("On-demand ") && !look.contains("At once"), "{look}");
    // One that may run 16 vCPUs at once: four jobs of the default size, and where to ask for more.
    account.fail_none();
    account.fail("servicequotas.", 200, r#"{"Quota":{"QuotaCode":"L-34B43A08","ServiceCode":"ec2","Value":16.0}}"#, None);
    let look = said(&["accounts", "retrieve", "aws"]);
    assert!(look.contains("At once       spot 16 vCPUs · on-demand 16 vCPUs") && look.contains("16 vCPUs is 4 jobs of the default size at a time. Ask AWS for more: "), "{look}");
    // AWS failing to say whether the image is in a region: not known there.
    account.fail("DescribeImages|ec2.ap-south-1.", 500, &superci_testkit::ec2_error("InternalError", "We encountered an internal error."), None);
    let images = said(&["images", "list"]);
    assert!(images.lines().any(|l| l.starts_with("ap-south-1") && l.ends_with("not known")), "{images}");
    let _ = std::fs::remove_dir_all(&alone);
}

/// Workspaces as they are kept: one looking after a CI it is signed out of, with its code hosts; one signed in to a
/// further AWS account only; a name that makes an id with digits and runs of what is not a letter; the first one
/// named before any is kept; one whose sign-ins cannot be removed, said; and what names no command or workspace.
#[test]
fn workspaces_kept_signed_out_or_in_another_account_only_are_listed_as_they_stand_and_what_names_none_is_said() {
    let served = serve(|_| {});
    let root = home(&served);
    std::fs::write(root.join("workspaces/default/workspace.json"), serde_json::json!({ "name": "Acme", "hosts": [["github", "acme"], ["gitlab", "gitlab.example.com"]] }).to_string()).unwrap();
    // (Signed in to a further AWS account, and to none as the first.)
    let kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(common::signed_in(&served).join("workspaces/default/sign-ins.json")).unwrap()).unwrap();
    std::fs::create_dir_all(root.join("workspaces/other")).unwrap();
    std::fs::write(root.join("workspaces/other/sign-ins.json"), serde_json::json!({ "aws_more": [kept["aws"]] }).to_string()).unwrap();
    // A name with a dash between words and a digit: its id with one dash where the name has several marks between words.
    let (code, made) = json(&root, &["workspaces", "create", "Build - Farm 2"]);
    assert_eq!((code, made["workspace"].as_str()), (0, Some("build-farm-2")), "{made}");
    let said = String::from_utf8_lossy(&superci(&root, &["workspaces", "list"]).stdout).into_owned();
    let line = |id: &str| said.lines().find(|l| l.split_whitespace().next() == Some(id)).unwrap_or_else(|| panic!("{id}: {said}")).trim_end().to_string();
    assert_eq!(line("default"), format!("{:<24} Acme · GitHub acme · GitLab gitlab.example.com · AWS · 111111111111 · us-east-1 · signed out", "default"));
    assert_eq!((line("other").as_str(), line("build-farm-2").ends_with("Build - Farm 2 · nothing set up yet")), ("other", true), "{said}");
    // The first workspace, named where none is kept yet: it is that one, with nothing signed in.
    let empty = common::temp("no-workspaces");
    let o = common::command(&empty).args(["status", "--workspace=default"]).output().unwrap();
    assert!(o.status.code() == Some(3) && String::from_utf8_lossy(&o.stderr).contains("SuperCI is not signed in on this computer."), "{}", String::from_utf8_lossy(&o.stderr));
    // A sign-in in a workspace not kept here; a command that is no word at all.
    let o = superci(&root, &["login", "--workspace=nowhere"]);
    assert!(o.status.code() == Some(1) && String::from_utf8_lossy(&o.stderr).contains("--workspace names the workspace nowhere, and none is called that on this machine (here: build-farm-2, default, other)."), "{}", String::from_utf8_lossy(&o.stderr));
    let o = superci(&root, &["", "--workspace=other"]);
    assert!(o.status.code() == Some(2) && String::from_utf8_lossy(&o.stderr).starts_with("superci: \n\nSuperCI — GitHub Actions and GitLab CI jobs on your own clouds"), "{}", String::from_utf8_lossy(&o.stderr));
    // Sign-ins kept where a folder is: not removed, said.
    std::fs::create_dir_all(empty.join("workspaces/odd/sign-ins.json")).unwrap();
    let o = common::command(&empty).args(["logout", "--workspace=odd"]).output().unwrap();
    assert!(o.status.code() == Some(1) && String::from_utf8_lossy(&o.stderr).contains("workspaces/odd/sign-ins.json: Is a directory"), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}

/// Where jobs come from and what runs them, as a CI that has little of either says it: GitHub's App installed
/// nowhere yet, a second GitLab by its name beside the first, and no runner provider at all.
#[test]
fn a_ci_whose_app_is_installed_nowhere_with_two_gitlabs_and_no_runner_provider_says_so() {
    let served = serve(|_| {});
    served.connect_github();
    served.github.set_installed(false, true);
    served.connect_gitlab();
    let mut other = superci_testkit::GitLab::connection();
    other["id"] = "other".into();
    other["url"] = "https://gitlab.example.com".into();
    served.aws.put(&format!("/superci/{}/GITLAB_OTHER", served.plane), &other.to_string());
    served.aws.params.lock().unwrap().remove(&format!("/superci/{}/CONTAINERS", served.plane));
    let home = home(&served);
    let said = |args: &[&str]| { let o = superci(&home, args); (o.status.code(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)) };
    assert_eq!(said(&["connections", "list"]), (Some(0), "github  acme  its App is not installed yet\ngitlab  https://gitlab.com\ngitlab  https://gitlab.example.com  (other)\n".to_string()));
    let (code, runners) = said(&["runners", "list"]);
    assert!(code == Some(0) && runners.starts_with("No runner provider yet: `superci runners create aws` (or cloudflare, or modal).\n"), "{runners}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Changes checked before they are made: a machine no label could ask for, a provider's networks given as none, what
/// a control plane's AWS role lacks where AWS has none of it; GitHub's deliveries, one sent again among them.
#[test]
fn changes_are_checked_before_they_are_made_and_githubs_deliveries_are_said_as_github_lists_them() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let said = |args: &[&str]| { let o = superci(&home, args); (o.status.code(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)) };
    let (code, refused) = said(&["machine", "update", "--os=windows", "--image=ubuntu24"]);
    assert!(code == Some(2) && refused.starts_with("superci: image=ubuntu24 is an Ubuntu for Linux machines, and the label says os=windows\n"), "{refused}");
    assert_eq!(said(&["runners", "update", "cloudflare", "--networks=none", "--dry-run"]), (Some(0), "Would update cloudflare: --networks=none. Nothing was changed.\n".to_string()));
    // (AWS has no policy of the role's: all of it is lacking.)
    served.aws.fail("GetRolePolicy", 404, "<ErrorResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\"><Error><Type>Sender</Type><Code>NoSuchEntity</Code><Message>The role policy with name runner-machines cannot be found.</Message></Error><RequestId>00000000-0000-4000-8000-000000000001</RequestId></ErrorResponse>", None);
    let (code, would) = said(&["planes", "allow", "--dry-run"]);
    assert!(code == Some(0) && would.starts_with("Would give the AWS role of the control plane testplane0001 what it lacks: Start machines, each tagged as this control plane's (since 0.1.0);"), "{would}");
    served.github.answer_with("GET", "/app/hook/deliveries", 200, &[("content-type", "application/json")], &serde_json::json!([
        { "id": 2, "guid": "b", "delivered_at": "2026-10-15T10:01:00Z", "redelivery": true, "status": "OK", "status_code": 200, "event": "workflow_job", "action": "queued" },
        { "id": 1, "guid": "a", "delivered_at": "2026-10-15T10:00:00Z", "redelivery": false, "status": "Invalid HTTP Response: 503", "status_code": 503, "event": "workflow_job", "action": "queued" }]).to_string());
    assert_eq!(said(&["deliveries", "list"]), (Some(0), "2026-10-15 10:01:00  workflow_job  queued       200 OK  (sent again)  2\n2026-10-15 10:00:00  workflow_job  queued       503 Invalid HTTP Response: 503  1\n".to_string()));
    let _ = std::fs::remove_dir_all(&home);
}

/// A run from a computer checked with --dry-run: a workflow of two jobs, said with a variable a shell would run; one
/// with none, refused. A key the control plane does not take, for a run and for following one, said as it answered.
#[test]
fn a_dry_run_of_a_workflow_of_two_jobs_or_none_and_a_key_not_taken_are_said_as_they_stand() {
    let served = serve(|_| {});
    let dir = common::temp("dry-run");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "jobs:\n  build:\n    runs-on: superci\n    steps:\n      - run: make\n  test:\n    runs-on: superci\n    steps:\n      - run: make test\n").unwrap();
    std::fs::write(dir.join(".github/workflows/none.yml"), "on: push\njobs: {}\n").unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(std::process::Command::new("git").current_dir(&dir).args(args).status().unwrap().success()) }
    let run = |key: &str, args: &[&str]| {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(&dir).args(args).env("SUPERCI_HOME", dir.join(".home")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", key).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let (code, out) = run(&served.run_key, &["runs", "create", "--workflow=.github/workflows/ci.yml", "--dry-run", "--var=WHERE=a;b", "--json"]);
    let would: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}"));
    assert!(code == Some(0) && would["jobs"] == serde_json::json!(["build", "test"]) && would["would"].as_str().is_some_and(|w| w.contains(": its jobs build, test, on this control plane's machines")), "{would}");
    assert!(would["steps"][0].as_str().is_some_and(|s| s.starts_with("Note: --var WHERE's value has characters a shell runs (;)")), "{would}");
    assert_eq!(run(&served.run_key, &["runs", "create", "--workflow=.github/workflows/none.yml", "--dry-run"]), (Some(1), "superci: .github/workflows/none.yml has no jobs.\n".to_string()));
    for args in [&["runs", "create", "--workflow=.github/workflows/ci.yml"][..], &["runs", "wait", "123"]] {
        let (code, said) = run("superci_run_0000000000000000000000000000dead", args);
        assert!(code == Some(1) && said.starts_with("superci: the control plane said 401: This control plane does not take this key"), "{args:?}: {said}");
    }
    assert!(served.store.under("run:").is_empty(), "no run started");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A debug build's own commands with what they take left out: a machine started where none is said (us-east-1), and
/// a header given with no name and value in it, left out of what is signed.
#[cfg(debug_assertions)]
#[test]
fn a_debug_launch_with_no_region_starts_in_us_east_1_and_a_header_without_a_colon_is_left_out() {
    let served = serve(|_| {});
    served.aws.replaying(include_str!("../fixtures/aws/launch/spot.jsonl"), &[]);
    let home = common::signed_in(&served);
    let (code, v) = json(&home, &["debug", "launch"]);
    let said = v["answer"].as_str().unwrap_or_default();
    assert!(code == 0 && (said.starts_with("started ") || said.starts_with("not started: ")), "{v}");
    assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().is_some_and(|u| u.contains("ec2.us-east-1.")) && a["asked"].as_str().is_some_and(|s| s.contains("RunInstances") || s.contains("CreateFleet"))), "asked in us-east-1");
    let (code, v) = json(&home, &["debug", "rest", "us-east-1", "s3", "GET", "https://bucket-for-tests.s3.us-east-1.amazonaws.com/a.txt", "--header=no-colon-here"]);
    assert!(code == 0 && v["answer"].as_str().and_then(|a| a.split(' ').next()).is_some_and(|s| s.parse::<u16>().is_ok()), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The runner service a debug build serves on this computer leaves what is no request alone (a first line or a header
/// that is not text, a header with no colon, a body cut short) and goes on to serve the job to its runner.
#[cfg(debug_assertions)]
#[test]
fn the_runner_service_here_leaves_what_is_no_request_alone_and_serves_the_job() {
    use std::io::Write;
    let dir = common::temp("runner-service-odd");
    let workflow = dir.join("ci.yml");
    std::fs::write(&workflow, "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: echo ok\n").unwrap();
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
    let root = format!("http://127.0.0.1:{port}/runner/rig");
    let jit = dir.join("jit");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).env("SUPERCI_HOME", dir.join(".home"))
        .args(["debug", "runner-service", workflow.to_str().unwrap(), "tests", &root, &port.to_string(), jit.to_str().unwrap(), "--json"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() { assert!(std::time::Instant::now() < until, "it did not listen"); std::thread::sleep(std::time::Duration::from_millis(50)) }
    // (A header that is not text ends the headers: what came before is answered.)
    for (odd, answered) in [(&b"\xff\xfe\r\n\r\n"[..], false), (b"GET /runner/rig/x HTTP/1.1\r\nno colon here\r\n\xff\xfe\r\n\r\n", true), (b"POST /runner/rig/x HTTP/1.1\r\ncontent-length: 100\r\n\r\nabc", false)] {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(odd).unwrap();
        let _ = s.shutdown(std::net::Shutdown::Write);
        let mut back = String::new();
        let _ = std::io::Read::read_to_string(&mut s, &mut back);
        assert_eq!(back.starts_with("HTTP/1.1 404"), answered, "{back}");
        assert_eq!(back.is_empty(), !answered, "{back}");
    }
    let runner = superci_testkit::Runner { at: format!("{root}/_apis/v1") };
    runner.take();
    let s = "dddddddd-0000-0000-0000-000000000004";
    runner.steps(&[(s, "Run echo ok", 1, "inProgress", None, None)]);
    runner.lines(s, 1, &["ok".to_string()]);
    runner.steps(&[(s, "Run echo ok", 1, "completed", Some("succeeded"), Some(1))]);
    runner.finish("succeeded");
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let out = child.wait_with_output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)));
    assert!(v["answer"].as_str().is_some_and(|a| a.starts_with("the job ended: succeeded") && a.contains("Run echo ok succeeded")), "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A secret typed at a terminal (hidden), and one a run started with a run key used: listed as used by that key.
#[test]
fn a_secret_typed_at_a_terminal_is_kept_and_one_a_run_key_used_is_listed_as_used_by_that_key() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // (A terminal of its own: `script` gives the command one, and types what is written to it.)
    let mut typed = std::process::Command::new("script").args(["-qec", &format!("{} secrets create TYPED", env!("CARGO_BIN_EXE_superci")), "/dev/null"])
        .env("SUPERCI_HOME", &home).env("SUPERCI_STAND_IN", &served.aws.url).env_remove("SUPERCI_PLANE").env_remove("SUPERCI_KEY")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    std::io::Write::write_all(typed.stdin.as_mut().unwrap(), b"s3cret-typed\n").unwrap();
    let out = typed.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success() && said.contains("TYPED's value (typed hidden; Enter when done): ") && said.contains("TYPED is kept for every run"), "{said}{}", String::from_utf8_lossy(&out.stderr));
    let o = common::command(&home).args(["secrets", "create", "TOKEN", "--value-env=THE_TOKEN"]).env("THE_TOKEN", "s3cret").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    common::played_steps(&served, &home.join("checkout"), "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: echo ${{ secrets.TOKEN }}\n", &[("Run echo", "succeeded")]);
    let said = String::from_utf8_lossy(&superci(&home, &["secrets", "list"]).stdout).into_owned();
    let line = |name: &str| said.lines().find(|l| l.starts_with(name)).unwrap_or_default().to_string();
    assert!(line("TOKEN").ends_with("used just now by the key agent") && line("TYPED").ends_with("not used yet"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// What a person does in the browser, begun from the terminal with --no-browser: a sign-in to one more AWS account
/// beside the one signed in to, and GitHub's App for an organization, each a link to open.
#[test]
fn another_aws_account_and_githubs_app_are_begun_from_the_terminal_with_a_link_to_open() {
    use std::io::BufRead;
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    for args in [&["login", "aws", "--add", "--no-browser"][..], &["connections", "create", "github", "acme", "--no-browser"]] {
        let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        let mut child = common::command(&home).args(args).env("SUPERCI_PORT", port.to_string()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok);
        let mut said = vec![];
        for l in lines.by_ref().take(20) { let link = l.contains(&format!("http://localhost:{port}/")); said.push(l); if link { break } }
        let _ = child.kill();
        let _ = child.wait();
        assert!(said.last().is_some_and(|l| l.contains(&format!("http://localhost:{port}/"))), "{args:?}: {said:?}");
        if args[0] == "connections" { assert!(said[0].starts_with("Only an owner of acme can create its App"), "{said:?}") }
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A run of two jobs from a computer, the second waiting on the first: its lines in groups, a post sent again from
/// a line already out, a step over once the next began, two steps whose logs never came (their tests counted from
/// their lines), a line whose time is not where a runner puts it; then the second failing with a step still going and
/// one never begun, what its runner logged while slow to listen said after it.
#[test]
fn a_run_of_two_jobs_is_followed_through_groups_lines_sent_again_and_steps_whose_logs_or_results_never_came() {
    use std::io::BufRead;
    let served = serve(|_| {});
    served.connect_github();
    let dir = common::temp("two-jobs");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "jobs:\n  build:\n    runs-on: superci\n    steps:\n      - run: make\n      - run: make test\n      - run: make docs\n  check:\n    needs: build\n    runs-on: superci\n    steps:\n      - run: make check\n      - run: make lint\n      - run: make fmt\n").unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(std::process::Command::new("git").current_dir(&dir).args(args).status().unwrap().success()) }
    let mut cli = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(&dir).args(["runs", "create", "--workflow=.github/workflows/ci.yml", "--repo=acme/app"])
        .env("SUPERCI_HOME", dir.join(".home")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &served.run_key).env("SUPERCI_LOGS_WAIT_S", "3").env("SUPERCI_STAND_IN", &served.aws.url)
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let p = printed.clone();
    let out = cli.stdout.take().unwrap();
    let reader = std::thread::spawn(move || for l in std::io::BufReader::new(out).lines().map_while(Result::ok) { p.lock().unwrap().push(l) });
    let until = |what: &str| { let end = std::time::Instant::now() + std::time::Duration::from_secs(20); while !printed.lock().unwrap().iter().any(|l| l.contains(what)) { assert!(std::time::Instant::now() < end, "no {what:?} in {:?}", printed.lock().unwrap()); std::thread::sleep(std::time::Duration::from_millis(50)) } };
    let run = loop { if let Some((k, _)) = served.store.under("run:").into_iter().next() { break k.trim_start_matches("run:").to_string() } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let build = loop { if let Some(r) = superci_testkit::Runner::of_job(&served, &run, "build") { break r } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let (a, b, c) = ("aaaaaaaa-0000-0000-0000-000000000001", "bbbbbbbb-0000-0000-0000-000000000002", "cccccccc-0000-0000-0000-000000000003");
    build.take();
    build.steps(&[(a, "Run make", 1, "inProgress", None, None), (b, "Run make test", 2, "pending", None, None), (c, "Run make docs", 3, "pending", None, None)]);
    let lines = |v: &[&str]| v.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    build.lines(a, 1, &lines(&["##[group]Setting up", "inside the group", "##[endgroup]"]));
    build.lines(a, 2, &lines(&["inside the group", "##[endgroup]", "after the group"]));
    until("after the group");
    // The next step begun while its record still says this one goes on (its log there already): this one is over.
    build.log(1, "2026-10-15T10:00:01.0000000Z ##[group]Setting up\n2026-10-15T10:00:01.0000000Z inside the group\n2026-10-15T10:00:01.0000000Z ##[endgroup]\n2026-10-15T10:00:02.0000000Z after the group");
    build.records(vec![serde_json::json!({ "id": a, "type": "Task", "name": "Run make", "order": 1, "state": "inProgress", "result": null, "log": { "id": 1 }, "startTime": "2026-10-15T10:00:00Z" }),
        serde_json::json!({ "id": b, "type": "Task", "name": "Run make test", "order": 2, "state": "inProgress", "result": null, "startTime": "2026-10-15T10:00:02Z" })]);
    build.lines(b, 1, &lines(&["test a ... ok", "2026-10-15T10:00:01 is when this one began, as its Z says much later on"]));
    until("test a ... ok");
    build.lines(c, 1, &lines(&["test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"]));
    build.steps(&[(a, "Run make", 1, "completed", Some("succeeded"), Some(1)), (b, "Run make test", 2, "completed", Some("succeeded"), None), (c, "Run make docs", 3, "completed", Some("succeeded"), None)]);
    build.finish("succeeded");
    // The second, once the first passed: its runner slow to listen, then a step that fails, one still going, one never begun.
    let check = loop { if let Some(r) = superci_testkit::Runner::of_job(&served, &run, "check") { break r } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let job = served.store.under("job:").into_iter().find(|(_, j)| j["name"] == "check").unwrap().1["job_id"].to_string();
    served.store.0.lock().unwrap().insert(format!("hello:{job}"), serde_json::json!({ "script_ms": served.now_ms(), "boot_s": 9.5, "log": "[RUNNER 2026-10-15 10:00:00Z INFO] Waiting for GitHub\n[RUNNER 2026-10-15 10:00:12Z INFO] Listening for Jobs" }).to_string());
    let (x, y, z) = ("dddddddd-0000-0000-0000-000000000004", "eeeeeeee-0000-0000-0000-000000000005", "ffffffff-0000-0000-0000-000000000006");
    check.take();
    check.steps(&[(x, "Run make check", 1, "inProgress", None, None)]);
    check.lines(x, 1, &lines(&["checking", "check failed"]));
    check.log(1, "2026-10-15T10:00:03.0000000Z checking\n2026-10-15T10:00:03.0000000Z check failed");
    check.records(vec![
        serde_json::json!({ "id": x, "type": "Task", "name": "Run make check", "order": 1, "state": "completed", "result": "failed", "log": { "id": 1 }, "startTime": "2026-10-15T10:00:00Z", "finishTime": "2026-10-15T10:00:04Z" }),
        serde_json::json!({ "id": y, "type": "Task", "name": "Run make lint", "order": 2, "state": "inProgress", "result": null, "startTime": "2026-10-15T10:00:04Z" }),
        serde_json::json!({ "id": z, "type": "Task", "name": "Run make fmt", "order": 3, "state": "pending", "result": null })]);
    check.finish("failed");
    let status = cli.wait().unwrap();
    reader.join().unwrap();
    let said = printed.lock().unwrap().join("\n");
    assert_eq!(status.code(), Some(1), "{said}");
    // Each line once, a group's heading as a line and its end as nothing; a line's own time kept as its own.
    for line in ["[build] Setting up", "[build] inside the group", "[build] after the group", "[build] 2026-10-15T10:00:01 is when this one began, as its Z says much later on"] {
        assert_eq!(said.lines().filter(|l| *l == line).count(), 1, "{line}: {said}");
    }
    assert!(!said.contains("endgroup") && said.contains("\n[check] checking\n[check] check failed\n"), "{said}");
    // The tests of steps whose logs never came, counted from their lines, and said so.
    assert!(said.contains("  Tests: 0 failed, 2 passed (cargo)\n  (Counted from what came: 2 steps' logs had not come. `superci runs retrieve "), "{said}");
    // A step still going and one never begun when its job failed: neither passed nor failed.
    assert!(said.contains("  ✗ Run make check 4 s\n  · Run make lint\n  · Run make fmt\n"), "{said}");
    assert!(said.contains("  What its runner logged while it was slow to reach the control plane") && said.contains("    [RUNNER 2026-10-15 10:00:12Z INFO] Listening for Jobs"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GitHub job's log in steps, as its page draws it: a line past the next step's second goes to it, a post step's
/// lines that are not its cleanup's stay where they were, a step whose time GitHub gives cut short, a carriage return
/// left out; the steps of a parallel group under its row, not all in the background; and a job named with slashes,
/// its steps run side by side, looked for in its run's logs by its name made a folder's.
#[test]
fn a_github_jobs_log_is_put_in_its_steps_by_their_times_and_its_parallel_groups_and_one_named_with_slashes_found_in_its_runs_logs() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let ended = |name: &str, log: &str| { let (id, q) = served.github.queue(name, "superci", served.now_ms()); served.webhook(q); served.webhook(served.github.start(id)); served.webhook(served.github.finish(id, "success", log, served.now_ms())); id };
    let log = ["2026-10-15T10:00:00.0000000Z Set up the runner", "2026-10-15T10:00:01.0000000Z ##[group]Run make", "2026-10-15T10:00:01.0000000Z making\rstill making",
        "2026-10-15T10:00:03.0000000Z made later", "2026-10-15T10:00:05.0000000Z not the cleanup yet", "2026-10-15T10:00:06.0000000Z Cleaning up orphan processes"].join("\n");
    let id = ended("tests", &log);
    let step = |name: &str, n: u64, from: &str, to: &str| serde_json::json!({ "name": name, "number": n, "status": "completed", "conclusion": "success", "started_at": from, "completed_at": to });
    served.github.set_steps(id, serde_json::json!([step("Set up job", 1, "2026-10-15T10:00:00Z", "2026-10-15T10:00:01Z"), step("Run make", 2, "2026-10-15T10:00:01Z", "2026-10-15T10:00:02Z"),
        step("Run a", 3, "2026-10-15T10:00:02Z", "2026-10-15T10:00:04Z"), step("Run b", 4, "2026-10-15T10:00:02Z", "2026-10-15T10:00:04Z"), step("Parallel group", 5, "2026-10-15T10:00:02Z", "2026-10-15T10:00:04Z"),
        step("Post Run make", 6, "2026-10-15T10:00:05Z", "2026-10-15T10:00:06Z"), step("Complete job", 7, "2026-10-15T10:00:06Z", "2026-10-15T10:00:06Z"), step("Odd", 8, "2026-10-15", "2026-10-15")]));
    // (Its shape, as the control plane keeps a job's: a parallel group's two steps, one in the background, then its row.)
    served.store.0.lock().unwrap().insert(format!("shape:{id}"), serde_json::json!([{ "id": "make" }, { "id": "a", "group": "g" }, { "id": "b", "group": "g", "bg": true }, { "id": "g", "group": "g", "control": "parallel" }]).to_string());
    let (mut child, link) = common::dashboard(&home);
    let (_, view) = common::part(&link, &format!("/job/view?id={id}"));
    let view: serde_json::Value = serde_json::from_str(&view).unwrap_or_else(|_| panic!("{view}"));
    let steps = view["steps"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, ["Set up job", "Run make", "Parallel group", "Run a", "Run b", "Post Run make", "Complete job", "Odd"], "{view}");
    let lines = |name: &str| steps.iter().find(|s| s["name"] == name).map(|s| s["lines"].as_array().unwrap().iter().map(|l| l[2].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>()).unwrap_or_default();
    // (The carriage return left out of what is shown.)
    assert_eq!(lines("Run make"), ["Run make", "makingstill making"], "{view}");
    assert_eq!((lines("Parallel group"), lines("Post Run make"), lines("Complete job")), (vec!["made later".to_string(), "not the cleanup yet".into()], vec![], vec!["Cleaning up orphan processes".to_string()]), "{view}");
    let row = &steps[2];
    assert!(row["group"] == 2 && row["bg"].is_null() && steps[4]["bg"] == true && steps[5]["child"].is_null() && steps[7]["ms"].is_null(), "{view}");
    // Named with slashes, its steps side by side: its own lines looked for in its run's logs, by its folder's name.
    let side = ended("build / linux\\x64", "2026-10-15T22:00:51.0000000Z mixed");
    served.github.set_steps(side, serde_json::from_str(include_str!("../fixtures/github/parallel-steps.json")).unwrap());
    served.github.run_ended(side, include_bytes!("../fixtures/github/run-logs.zip").to_vec());
    let (_, view) = common::part(&link, &format!("/job/view?id={side}"));
    let view: serde_json::Value = serde_json::from_str(&view).unwrap_or_else(|_| panic!("{view}"));
    // (Its folder is not among those of the run's logs: its lines as they came, said as mixed.)
    assert!(view["mixed"] == true && view["steps"][0]["lines"][0][2] == "mixed", "{view}");
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// Another control plane with more settings than SSM gives at once: deleted with every one of them, read page by page.
#[test]
fn another_control_plane_with_more_settings_than_ssm_lists_at_once_is_deleted_with_every_one() {
    let served = serve(|_| {});
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    for n in 0..25 { served.aws.put(&format!("/superci/testplane0002/SECRET_KEPT{n}_ALL"), "{}") }
    let o = superci(&home, &["planes", "delete", "testplane0002", "--confirm"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Deleted"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(served.aws.settings("testplane0002").is_empty(), "{:?}", served.aws.settings("testplane0002"));
    let pages = served.aws.asked.lock().unwrap().iter().filter(|a| a["target"] == "AmazonSSM.GetParametersByPath" && a["asked"].as_str().is_some_and(|s| s.contains("testplane0002") && s.contains("NextToken"))).count();
    assert!(pages >= 2, "asked page by page: {pages}");
    let _ = std::fs::remove_dir_all(&home);
}

/// An update whose image converter Lambda failed to take in: the control plane is updated all the same (the converter
/// is not what runs it), and the converter is not waited for again.
#[test]
fn an_update_whose_converter_lambda_failed_to_take_in_still_updates_the_control_plane() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/update.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
    served.aws.fail("functions/superci-plane-testplane0001-convert/configuration", 200, r#"{"FunctionName":"superci-plane-testplane0001-convert","State":"Active","LastUpdateStatus":"Failed","LastUpdateStatusReason":"The code is not a bootstrap Lambda can run."}"#, None);
    let o = superci(&home, &["planes", "update"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains(&format!("Updated to {}", env!("CARGO_PKG_VERSION"))), "{said}");
    let asked = served.aws.asked.lock().unwrap().iter().filter(|a| a["method"] == "GET" && a["url"].as_str().is_some_and(|u| u.ends_with("-convert/configuration"))).count();
    assert_eq!(asked, 1, "its state asked once: Failed");
    let _ = std::fs::remove_dir_all(&home);
}

/// An AWS sign-in renewed as it is used, AWS refusing: one whose user may not sign in developer tools, and one AWS
/// answers with no JSON at all; either is over, and a sign-in is asked for.
#[test]
fn an_aws_sign_in_aws_will_not_renew_is_said_as_aws_refused_it() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["aws"]["creds"]["expires_at_ms"] = 0.into();
    kept["aws"]["refresh_token"] = "refresh-for-tests".into();
    kept["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    let due = kept.to_string();
    for (answer, said) in [(r#"{"error":"INSUFFICIENT_PERMISSIONS"}"#, "AWS says this sign-in may not be used by developer tools"), ("Bad Request", "AWS sign-in: 400 Bad Request")] {
        std::fs::write(&file, &due).unwrap();
        served.aws.fail_none();
        served.aws.fail("signin.aws.amazon.com/v1/token", 400, answer, None);
        let o = superci(&home, &["accounts", "retrieve", "aws"]);
        let e = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        // (Its renewal refused: no longer signed in to it, and a sign-in asked for.)
        assert!(o.status.code() == Some(3) && e.contains("Sign in with AWS first."), "{answer}: {e}");
        let renewal = served.aws.asked.lock().unwrap().iter().rev().find(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/v1/token"))).cloned().unwrap_or_default();
        assert_eq!((renewal["status"].as_u64(), renewal["answer"].as_str()), (Some(400), Some(answer)), "{said}");
        assert!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&file).unwrap()).unwrap()["aws"].is_null(), "{said}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A recording made for the test kit (`SUPERCI_RECORD`) without `SUPERCI_RECORD_SIGN_INS` keeps no sign-in's
/// exchange, whose tokens are real: AWS's renewal and Cloudflare's are left out, what the commands asked after them
/// kept. One named where no file can be written is not made, and the command does its work all the same.
#[test]
fn a_recording_without_sign_ins_leaves_out_aws_and_cloudflare_renewals_and_one_that_cannot_be_written_is_skipped() {
    let served = serve(|_| {});
    served.aws.replaying(&[include_str!("../fixtures/aws/sign-in/renew.jsonl"), include_str!("../fixtures/cloudflare/sign-in/renew.jsonl")].concat(), &[]);
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["aws"]["creds"]["expires_at_ms"] = 0.into();
    kept["aws"]["refresh_token"] = "example-refreshToken-1".into();
    kept["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "example-access_token-0", "expires_at_ms": 0, "refresh": "example-refresh_token-1" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let record = home.join("recorded.jsonl");
    for args in [&["debug", "iam", "ListRoles", "MaxItems=1000", "--json"][..], &["debug", "cloudflare", "GET", "/accounts"]] {
        let o = common::command(&home).args(args).env("SUPERCI_RECORD", &record).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    // Both renewed (and what each gave kept), neither recorded.
    let kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!((kept["aws"]["refresh_token"].as_str(), kept["cloudflare"]["refresh"].as_str()), (Some("example-refreshToken-2"), Some("example-refresh_token-2")), "{kept}");
    let recorded = std::fs::read_to_string(&record).unwrap();
    assert!(recorded.contains("Action=ListRoles") && recorded.contains("/client/v4/accounts"), "{recorded}");
    assert!(!recorded.contains("signin.aws.amazon.com") && !recorded.contains("/oauth2/token") && !recorded.contains("refreshToken"), "{recorded}");
    // A folder in place of the file: nothing written, the answer given.
    let o = common::command(&home).args(["debug", "iam", "ListRoles", "MaxItems=1000", "--json"]).env("SUPERCI_RECORD", home.join("workspaces")).output().unwrap();
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("ListRolesResponse"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&home);
}

/// AWS saying in each region, in place of an answer, that the credentials are over, however it words it (its code
/// after another's words, its code in a message of its own, or no code at all): the look at the account renews the
/// sign-in, and, AWS refusing that, says it ended with what to do.
#[test]
fn credentials_aws_says_are_over_however_it_words_it_end_the_look_at_the_account_with_a_sign_in_to_make_again() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["aws"]["refresh_token"] = "refresh-for-tests".into();
    kept["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
    let signed = kept.to_string();
    for over in ["RequestExpired: Request has expired.", "<Error>ExpiredToken: the token is over</Error>", "The security token included in the request is expired"] {
        std::fs::write(&file, &signed).unwrap();
        served.aws.fail_none();
        served.aws.fail("DescribeVpcs", 400, over, None);
        let o = superci(&home, &["accounts", "retrieve", "aws"]);
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success() && said.contains("SuperCI's AWS sign-in has ended") && said.contains("superci login aws"), "{over}: {said}");
        // (Renewal asked for, and refused: the sign-in is no longer kept.)
        assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/v1/token"))), "{over}");
        assert!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&file).unwrap()).unwrap()["aws"].is_null(), "{over}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// An account of AWS's new sign-up pinned to Stockholm is a project; the same account whose N. Virginia is refused
/// by the sign-in's own permissions, not by its organization, is not one: it is restricted.
#[test]
fn an_account_kept_to_one_home_region_is_a_project_only_when_its_organization_refuses_n_virginia() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/project-activated/look.jsonl"), &[]);
    let (code, v) = json(&home, &["accounts", "retrieve", "aws"]);
    assert!(code == 0 && v["kind"]["project"]["region"] == "eu-north-1", "{v}");
    served.aws.fail("DescribeVpcs|ec2.us-east-1", 403, &superci_testkit::ec2_error("UnauthorizedOperation", "You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/Admin/someone is not authorized to perform: ec2:DescribeVpcs because no identity-based policy allows the ec2:DescribeVpcs action"), None);
    let (code, v) = json(&home, &["accounts", "retrieve", "aws"]);
    assert!(code == 0 && v["kind"] == "restricted" && v["allowed_regions"] == serde_json::json!(["eu-north-1"]), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A region's network and cache bucket deleted where the account's organization refuses them: nothing of the control
/// plane's can be there, so it is taken as done, EC2's refusal or S3's. The roles that say which control planes use
/// the account are read page by page.
#[test]
fn a_network_and_cache_bucket_the_organization_keeps_out_are_taken_as_deleted_and_roles_are_read_page_by_page() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let scp = "You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/Admin/someone is not authorized to perform: ec2:DescribeInstances with an explicit deny in a service control policy: arn:aws:organizations::222222222222:policy/o-aaaaaaaaaa/service_control_policy/p-aaaaaaaa";
    let first = r#"<ListRolesResponse xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><ListRolesResult><IsTruncated>true</IsTruncated><Marker>page-two</Marker><Roles><member><RoleName>superci-plane-otherplane02</RoleName><Description>Assumed by the SuperCI control plane https://superci.acme.workers.dev (OpenID Connect) to run GitHub Actions jobs.</Description></member></Roles></ListRolesResult></ListRolesResponse>"#;
    served.aws.fail("ListRoles", 200, first, Some(1));
    served.aws.fail("ec2.eu-west-2", 403, &superci_testkit::ec2_error("UnauthorizedOperation", scp), None);
    let o = superci(&home, &["debug", "networks", "eu-west-2"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("deleted in eu-west-2"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let pages: Vec<String> = served.aws.asked.lock().unwrap().iter().filter_map(|a| a["asked"].as_str().filter(|s| s.contains("Action=ListRoles")).map(str::to_string)).collect();
    assert!(pages.len() == 2 && pages[1].contains("Marker=page-two"), "{pages:?}");
    // Its network gone already, its bucket refused by S3 under the same policy.
    served.aws.fail_none();
    served.aws.fail("ec2.eu-west-2", 200, "<DescribeResponse xmlns=\"http://ec2.amazonaws.com/doc/2016-11-15/\"><requestId>1</requestId></DescribeResponse>", None);
    served.github.answer_with("GET", "?list-type=2", 403, &[("content-type", "application/xml")], &format!("<Error><Code>AccessDenied</Code><Message>User is not authorized to perform: s3:ListBucket with an explicit deny in a service control policy</Message></Error>"));
    let o = superci(&home, &["debug", "networks", "eu-west-2"]);
    served.github.fail_none();
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("deleted in eu-west-2"), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["asked"].as_str().is_some_and(|s| s.contains("Action=DescribeInstances") && a["url"].as_str().is_some_and(|u| u.contains("eu-west-2")))));
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane named by its address (SUPERCI_PLANE) on Modal: read with the key given, and said to be Modal's,
/// known only by its address.
#[test]
fn a_control_plane_given_by_its_modal_address_is_read_and_said_to_be_modals() {
    let served = serve(|_| {});
    served.connect_github();
    let home = home(&served);
    served.aws.workers.lock().unwrap().insert("acme--superci-plane-testplane0001.modal.run".into(), served.url.clone());
    std::fs::write(home.join("aws-at"), &served.aws.url).unwrap();
    let o = common::command(&home).env("SUPERCI_PLANE", "https://acme--superci-plane-testplane0001.modal.run").env("SUPERCI_KEY", &served.run_key).args(["status"]).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("Control plane Modal") && said.contains("https://acme--superci-plane-testplane0001.modal.run"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Playwright's lines that only begin as its first one does (no workers said, or none used) are no test's; and a
/// failure's error runs on past lines that only look like the next failure's heading.
#[test]
fn playwrights_first_line_is_known_by_its_workers_and_a_failures_error_runs_past_headings_that_are_none() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let (job_id, q) = served.github.queue("e2e", "superci", served.now_ms());
    served.webhook(q);
    served.webhook(served.github.start(job_id));
    let log = [
        "Running 2 tests using 1 worker",
        "Running the server first",
        "Running 2 suites using 2 processes",
        "  ✓  1 [chromium] › e2e/a.spec.ts:3:1 › logs in (1.0s)",
        "  ✘  2 [chromium] › e2e/a.spec.ts:5:1 › breaks (1.0s)",
        "  1) [chromium] › e2e/a.spec.ts:5:1 › breaks ───",
        "    Error: expected ok",
        "    ) [not a numbered failure]",
        "    a) [nor this]",
        "  1 failed",
        "  1 passed (2.0s)",
    ].iter().map(|l| format!("2026-10-15T10:00:01.0000000Z {l}")).collect::<Vec<_>>().join("\n");
    served.webhook(served.github.finish(job_id, "failure", &log, served.now_ms()));
    let said = String::from_utf8_lossy(&superci(&home, &["jobs", "retrieve", &job_id.to_string(), "--tests"]).stdout).into_owned();
    assert!(said.contains("Tests: 1 failed, 1 passed (playwright)"), "{said}");
    assert!(said.contains("Error: expected ok") && said.contains(") [not a numbered failure]") && said.contains("a) [nor this]") && !said.contains("Running the server first"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The AWS account's own budget, as AWS answers in states a real account was not caught in: at the limit and
/// stopped, set again to the same limit (nothing lifted); lifted again with an action of another state beside it
/// (only the one that ran is undone and reset); and its stop refused by AWS each time asked (said, after asking
/// again a few times).
#[test]
fn the_aws_budget_set_again_at_its_limit_lifted_beside_another_action_and_refused_its_stop_is_said_as_aws_answered() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let budget = r#"{"Budget":{"BudgetName":"superci-testplane0001","BudgetLimit":{"Amount":"100.0","Unit":"USD"},"CalculatedSpend":{"ActualSpend":{"Amount":"101.5","Unit":"USD"}},"TimeUnit":"MONTHLY","BudgetType":"COST"}}"#;
    let iam_ok = r#"<Response xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><ResponseMetadata><RequestId>1</RequestId></ResponseMetadata></Response>"#;
    let asked = |target: &str| served.aws.asked.lock().unwrap().iter().filter(|a| a["target"] == format!("AWSBudgetServiceGateway.{target}").as_str()).count();
    let run = |args: &[&str]| { let o = common::command(&home).args(args).env("SUPERCI_PAUSE_MS", "1").output().unwrap(); String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr) };
    served.aws.fail("AWSBudgetServiceGateway.DescribeBudget", 200, budget, None);
    served.aws.fail("AWSBudgetServiceGateway.DescribeBudgetActionsForBudget", 200, r#"{"Actions":[{"ActionId":"a1","Status":"EXECUTION_SUCCESS"},{"ActionId":"a2","Status":"STANDBY"}]}"#, None);
    served.aws.fail("AWSBudgetServiceGateway.ExecuteBudgetAction", 200, "{}", None);
    served.aws.fail("DetachRolePolicy", 200, iam_ok, None);
    // At its limit and stopped, set to the same limit: as it was.
    assert_eq!(run(&["debug", "budget", "set", "100", "ops@example.com", "stop"]).trim(), "set");
    assert_eq!(asked("ExecuteBudgetAction") + asked("UpdateBudget"), 0);
    // Lifted to be judged again: the action that ran undone and reset, the other left.
    assert_eq!(run(&["debug", "budget", "lift", "again"]).trim(), "lifted");
    let executed: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["target"] == "AWSBudgetServiceGateway.ExecuteBudgetAction").filter_map(|a| a["asked"].as_str().map(str::to_string)).collect();
    assert!(executed.len() == 2 && executed.iter().all(|e| e.contains("\"a1\"")) && executed[0].contains("REVERSE_BUDGET_ACTION") && executed[1].contains("RESET_BUDGET_ACTION"), "{executed:?}");
    // No action yet, and AWS refusing to make one each time: asked again, then said.
    served.aws.fail_none();
    served.aws.fail("AWSBudgetServiceGateway.DescribeBudget", 200, budget, None);
    served.aws.fail("AWSBudgetServiceGateway.DescribeBudgetActionsForBudget", 200, r#"{"Actions":[]}"#, None);
    for a in ["CreatePolicy", "CreateRole", "PutRolePolicy"] { served.aws.fail(a, 200, iam_ok, None) }
    served.aws.fail("AWSBudgetServiceGateway.CreateBudgetAction", 400, &superci_testkit::json_error("AccessDeniedException", "User is not allowed to pass this role to AWS Budgets."), None);
    let said = run(&["debug", "budget", "set", "100", "ops@example.com", "stop"]);
    assert!(said.contains("refused or failed: AccessDeniedException: User is not allowed to pass this role"), "{said}");
    assert_eq!(asked("CreateBudgetAction"), 9);
    let _ = std::fs::remove_dir_all(&home);
}

/// A job asked for by its run alone, when the run has two: which to say is asked for, with their names.
#[test]
fn a_job_asked_for_by_a_run_of_two_jobs_alone_asks_which_with_their_names() {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    let (tests, q) = served.github.queue("tests", "superci", served.now_ms());
    served.webhook(q);
    let (lint, q) = served.github.queue("lint", "superci", served.now_ms());
    served.webhook(q);
    let run = served.store.under(&format!("job:{tests}"))[0].1["run_id"].clone();
    {
        let mut m = served.store.0.lock().unwrap();
        let k = format!("job:{lint}");
        let mut j: serde_json::Value = serde_json::from_str(&m[&k]).unwrap();
        j["run_id"] = run.clone();
        m.insert(k, j.to_string());
    }
    let (code, v) = json(&home, &["jobs", "retrieve", &format!("--run={run}")]);
    let e = v["error"].as_str().unwrap_or_default();
    assert!(code == 2 && e.starts_with(&format!("Run {run} has several jobs: say which with --job=NAME")) && e.contains("tests") && e.contains("lint"), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane from before a job's log and GitHub's deliveries could be read from it (it answers 404 there):
/// each says which version it needs, and the command that updates it.
#[test]
fn a_control_plane_older_than_job_logs_and_deliveries_says_which_version_reads_them() {
    let served = serve(|_| {});
    served.connect_github();
    let home = home(&served);
    let older = superci_testkit::without_routes(&served.url, &["/job/log", "/github/deliveries"]);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"]["Aws"]["url"] = older.clone().into();
    std::fs::write(&file, kept.to_string()).unwrap();
    let (id, q) = served.github.queue("tests", "superci", served.now_ms());
    served.webhook(q);
    served.webhook(served.github.start(id));
    served.webhook(served.github.finish(id, "success", "2026-10-15T10:00:01.0000000Z ok", served.now_ms()));
    let (code, v) = json(&home, &["jobs", "retrieve", &id.to_string()]);
    assert!(code == 1 && v["error"] == "Reading a job's log needs control plane 0.11.0 or newer: `superci planes update`.", "{v}");
    let (code, v) = json(&home, &["deliveries", "list"]);
    assert!(code == 1 && v["error"] == "This needs control plane 0.11.2 or newer: `superci planes update`.", "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Two control planes in the workspace's clouds that each serve GitHub: each is live, and neither is "the" one in
/// use (commands name theirs).
#[test]
fn two_control_planes_that_each_serve_github_are_both_listed_live_and_neither_in_use() {
    let served = serve(|_| {});
    served.connect_github();
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    other.aws.put("/superci/testplane0002/GITHUB_APP", &superci_testkit::GitHub::app().to_string());
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    // (Both kept as this workspace's: one moved to and serving GitHub there since.)
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let (code, listed) = json(&home, &["planes", "list"]);
    let planes = listed["control_planes"].as_array().cloned().unwrap_or_default();
    let ours: Vec<&serde_json::Value> = planes.iter().filter(|p| ["testplane0001", "testplane0002"].contains(&p["id"].as_str().unwrap_or_default())).collect();
    assert!(code == 0 && ours.len() == 2 && ours.iter().all(|p| p["live"] == true && p["in_use"] == false), "{listed}");
    let said = String::from_utf8_lossy(&superci(&home, &["planes", "list"]).stdout).into_owned();
    for id in ["testplane0001", "testplane0002"] { assert!(said.lines().any(|l| l.starts_with(id) && l.ends_with("  live")), "{said}") }
    assert!(!said.contains("in use"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// GitLab connected with a token that reaches no project: said so, rather than an empty list.
#[test]
fn a_gitlab_token_that_reaches_no_project_is_said_to_reach_none() {
    let served = serve(|_| {});
    served.connect_gitlab();
    served.gitlab.reach_no_project(true);
    let home = home(&served);
    let o = superci(&home, &["projects", "list"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "No projects this token can reach.", "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    served.gitlab.reach_no_project(false);
    let o = superci(&home, &["projects", "list"]);
    assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("acme/app"), "{}", String::from_utf8_lossy(&o.stdout));
    let _ = std::fs::remove_dir_all(&home);
}

/// `--add` is for one more AWS account: asked of Cloudflare, signed in already, it says so and opens nothing.
#[test]
fn a_cloudflare_sign_in_asked_with_add_while_signed_in_there_says_so_and_opens_nothing() {
    let served = serve(|_| {});
    let home = home(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let o = common::command(&home).args(["login", "cloudflare", "--add", "--no-browser"]).stdin(std::process::Stdio::null()).output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && said.starts_with("SuperCI is signed in on this computer:") && !said.contains("--add"), "{said}{}", String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&home);
}

/// A secret a run from a computer used with that computer's own key (not a run key): listed as used by that
/// computer, by its name.
#[test]
fn a_secret_used_by_a_run_from_a_computers_own_key_is_listed_as_used_by_that_computer() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let before: Vec<String> = served.aws.settings(superci_testkit::PLANE).into_iter().map(|(k, _)| k).collect();
    let o = common::command(&home).args(["secrets", "create", "NPM_TOKEN", "--value-env=THE_TOKEN"]).env("THE_TOKEN", "s3cret").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let setting = served.aws.settings(superci_testkit::PLANE).into_iter().map(|(k, _)| k).find(|k| !before.contains(k)).expect("the secret's setting");
    let now = served.now_ms();
    served.store.0.lock().unwrap().insert(format!("sused:{setting}"), serde_json::json!({ "run_id": "123", "at_ms": now, "repository": "acme/app", "path": ".github/workflows/ci.yml", "ref": "refs/heads/main", "by": "mona-laptop", "run_key": false }).to_string());
    let said = String::from_utf8_lossy(&superci(&home, &["secrets", "list"]).stdout).into_owned();
    assert!(said.lines().any(|l| l.starts_with("NPM_TOKEN") && l.ends_with("used just now by mona-laptop")), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// What is kept of a workspace once its last sign-in is over: nothing, with no control plane and key to read it
/// with (an AWS sign-in AWS will not renew, alone or with a key and no control plane), nor after signing out of it.
#[test]
fn a_workspace_left_with_no_sign_in_and_no_control_plane_to_read_keeps_nothing() {
    let account = superci_testkit::Aws::serve();
    for with_key in [false, true] {
        let home = common::aws_only(&account);
        let file = home.join("workspaces/default/sign-ins.json");
        let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        kept["aws"]["creds"]["expires_at_ms"] = 0.into();
        kept["aws"]["refresh_token"] = "refresh-for-tests".into();
        kept["aws"]["dpop"] = serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap();
        if with_key { kept["key"] = serde_json::json!({ "name": "DEVICE_KEY_4102444800_TEST", "value": "dashboard-key-for-tests-0001", "planes": [] }) }
        std::fs::write(&file, kept.to_string()).unwrap();
        let o = superci(&home, &["accounts", "retrieve", "aws"]);
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(o.status.code() == Some(3) && said.contains("Sign in with AWS first."), "{with_key}: {said}");
        assert!(!file.exists(), "{with_key}: {}", std::fs::read_to_string(&file).unwrap_or_default());
        let _ = std::fs::remove_dir_all(&home);
    }
    let home = common::aws_only(&account);
    let file = home.join("workspaces/default/sign-ins.json");
    let o = superci(&home, &["logout"]);
    assert!(o.status.success() && !file.exists(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let _ = std::fs::remove_dir_all(&home);
}

/// The first workspace, never made on this computer, renamed from the dashboard: made under its new id; and
/// another never made, forgotten: nothing to remove, and the dashboard goes back to the list.
#[test]
fn a_first_workspace_never_made_here_is_made_under_its_new_id_and_one_forgotten_without_a_folder_is_gone_all_the_same() {
    let home = common::temp("no-workspace");
    let (_board, link) = common::dashboard(&home);
    let (status, to, _) = common::post(&link, "/workspaces/rename", &[("w", "default"), ("id", "main")]);
    assert!(status == 303 && to == "/?p=settings&w=main" && home.join("workspaces/main").is_dir() && !home.join("workspaces/default").exists(), "{status} {to}");
    drop(_board);
    let home2 = common::temp("no-workspace");
    let (_board, link) = common::dashboard(&home2);
    let (status, to, _) = common::post(&link, "/workspaces/remove", &[("w", "default")]);
    assert!(status == 303 && to == "/" && !home2.join("workspaces/default").exists(), "{status} {to}");
    drop(_board);
    for h in [home, home2] { let _ = std::fs::remove_dir_all(&h); }
}

/// One of Modal's calls answered as a recording has it (each answer protobuf, in base64): for what a real workspace
/// was not caught answering.
fn modal_answered(method: &str, answers: &[Vec<u8>]) -> String {
    use base64::Engine;
    let answers: Vec<String> = answers.iter().map(|a| base64::engine::general_purpose::STANDARD.encode(a)).collect();
    serde_json::json!({ "method": "POST", "url": format!("https://api.modal.com/modal.client.ModalClient/{method}"), "target": "", "asked": "", "status": 200, "headers": [],
        "answer": serde_json::json!({ "answers": answers }).to_string() }).to_string() + "\n"
}

/// A protobuf message's field: its number, and a number or bytes (a string, another message).
enum Pb { N(u32, u64), B(u32, Vec<u8>) }

fn pb(fields: &[Pb]) -> Vec<u8> {
    let varint = |out: &mut Vec<u8>, mut n: u64| loop { let b = (n & 0x7f) as u8; n >>= 7; if n == 0 { out.push(b); break } out.push(b | 0x80) };
    let mut out = vec![];
    for f in fields {
        match f {
            Pb::N(k, n) => { varint(&mut out, (*k as u64) << 3); varint(&mut out, *n) }
            Pb::B(k, b) => { varint(&mut out, ((*k as u64) << 3) | 2); varint(&mut out, b.len() as u64); out.extend(b) }
        }
    }
    out
}

/// Modal's GenericResult: a status (1 done), what was raised, and the exit code.
fn modal_result(status: u64, exception: &str, exitcode: u64) -> Vec<u8> { pb(&[Pb::N(1, status), Pb::B(2, exception.as_bytes().to_vec()), Pb::N(3, exitcode)]) }

/// A Modal control plane deployed as Modal answered a real one, but where it did not: the setup image's build
/// followed to its end and failed; `modal deploy` ending with an error (its log's last lines said, of the most
/// read), or ending otherwise than done, or never; then deployed, the workspace's web address said by Modal. An id
/// asked for that cannot be one is not taken.
#[test]
fn a_modal_deploy_says_how_modals_sandbox_ended_and_its_image_build_failed_and_goes_on_with_the_address_modal_gives() {
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let modal = superci_testkit::serve_as("mdplane00001", move |c| { let settings = aws.modal_plane_settings(); c.read_settings(&settings); c.own_cloud = "modal".into(); });
    served.aws.workers.lock().unwrap().insert("acmeorg--superci-plane-mdplane00001.modal.run".into(), modal.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["modal"] = serde_json::json!({ "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let deploy = |id: &str, answered: &[String]| {
        served.aws.replaying(include_str!("../fixtures/modal/02-deploy.jsonl"), &[]);
        served.aws.replaying(&answered.concat(), &[]);
        let o = common::command(&home).args(["planes", "create", "modal"]).env("SUPERCI_NEW_PLANE_ID", id).output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let image_built = modal_answered("ImageGetOrCreate", &[pb(&[Pb::B(1, b"im-built".to_vec()), Pb::B(2, modal_result(1, "", 0))])]);
    // The image still building: followed until its build ends, failed.
    let (ok, said) = deploy("short", &[
        modal_answered("ImageGetOrCreate", &[pb(&[Pb::B(1, b"im-building".to_vec())])]),
        modal_answered("ImageJoinStreaming", &[pb(&[Pb::B(3, b"entry-1".to_vec())]), pb(&[Pb::B(1, modal_result(2, "pip: no space left on device", 0))])]),
    ]);
    assert!(!ok && said.contains("Modal could not build the setup image: pip: no space left on device"), "{said}");
    // `modal deploy` failed: the last lines of what it wrote (no more than so much read of it).
    let logs: Vec<Vec<u8>> = (1..=60).map(|n| pb(&[Pb::B(2, pb(&[Pb::B(1, format!("line {n}\n").into_bytes())]))])).collect();
    let (ok, said) = deploy("abcdefghijk-", &[image_built.clone(), modal_answered("SandboxWait", &[pb(&[Pb::B(1, modal_result(1, "", 3))])]), modal_answered("SandboxGetLogs", &logs)]);
    assert!(!ok && said.contains("`modal deploy app.py` did not succeed (status 1, exit 3): line 43 / line 44 / line 45 / line 46 / line 47 / line 48 / line 49 / line 50") && !said.contains("line 51"), "{said}");
    // Ended otherwise than done; and still running at the last wait.
    let (ok, said) = deploy("", &[image_built.clone(), modal_answered("SandboxWait", &[pb(&[Pb::B(1, modal_result(2, "", 0))])])]);
    assert!(!ok && said.contains("did not succeed (status 2, exit 0)"), "{said}");
    let (ok, said) = deploy("", &[image_built.clone(), modal_answered("SandboxWait", &[vec![]])]);
    assert!(!ok && said.contains("did not succeed (still running)"), "{said}");
    assert_eq!(served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/SandboxWait"))).count(), 2 + 18);
    // The ids asked for were none: each deploy made its own.
    let created: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/DictGetOrCreate"))).filter_map(|a| a["asked"].as_str().map(str::to_string)).collect();
    assert!(created.is_empty(), "{created:?}");
    // Deployed, Modal saying the workspace's web addresses end in modal.run.
    let env = modal_answered("EnvironmentGetOrCreate", &[pb(&[Pb::B(1, b"en-main".to_vec()), Pb::B(2, pb(&[Pb::B(1, b"main".to_vec()), Pb::B(2, pb(&[Pb::B(1, b"2025.06".to_vec()), Pb::B(2, b"modal.run".to_vec())]))]))])]);
    let (ok, said) = deploy("mdplane00001", &[env, image_built, modal_answered("SandboxWait", &[pb(&[Pb::B(1, modal_result(1, "", 0))])])]);
    assert!(ok && said.contains("The control plane is running: https://acmeorg--superci-plane-mdplane00001.modal.run (Modal · acmeorg)"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A Modal token given by name that cannot go in a request's header as it is (not plain ASCII): left out of what is
/// asked, and Modal's refusal said as Modal said it.
#[test]
fn a_modal_token_given_by_name_that_no_header_can_carry_is_left_out_and_modals_refusal_said() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/modal/sign-in/ended.jsonl"), &[]);
    let o = common::command(&home).args(["planes", "create", "modal"]).env("SUPERCI_MODAL_TOKEN_ID", "ak-tökén").env("SUPERCI_MODAL_TOKEN_SECRET", "as-superciendedrecording00").output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("Token not found"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The answers a recording has for one call (its method, and its address ending so), in their order.
fn recorded_answers(recording: &str, method: &str, url_end: &str) -> Vec<serde_json::Value> {
    recording.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter(|r| r["method"] == method && r["url"].as_str().is_some_and(|u| u.ends_with(url_end))).collect()
}

/// Cloudflare's runners for a control plane in AWS, deployed as Cloudflare answered a real one but where it did
/// not: a control plane that does not publish its keys yet is refused; Cloudflare refusing to prepare the image is
/// said; then GitHub's registry and Cloudflare's asking for a token first, GitHub's index listing other platforms
/// before the one containers run (at length), the image not visible at once and then still being prepared, the
/// Worker's containers of one size from before (their application gone, one without an id left), and it is deployed.
#[test]
fn cloudflare_runners_are_deployed_through_registry_challenges_an_image_being_prepared_and_containers_from_before() {
    let served = serve(|_| {});
    served.connect_github();
    served.aws.workers.lock().unwrap().insert("superci-runners-testplane0001.acme.workers.dev".into(), served.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    let plain = kept.to_string();
    let recording = include_str!("../fixtures/runners/03-cf-agent.jsonl").replace("awsplane0001", superci_testkit::PLANE).replace("http://new-plane.test", &served.url);
    let run = |overlay: &[serde_json::Value]| {
        served.aws.replaying(&recording, &[]);
        if !overlay.is_empty() { served.aws.replaying(&overlay.iter().map(|v| v.to_string() + "\n").collect::<String>(), &[]) }
        let o = common::command(&home).args(["runners", "create", "cloudflare"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        served.aws.fail_none();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // A control plane from before it published its keys.
    let mut older = kept.clone();
    older["plane"]["Aws"]["url"] = superci_testkit::without_routes(&served.url, &["/.well-known/jwks.json"]).into();
    std::fs::write(&file, older.to_string()).unwrap();
    let (ok, said) = run(&[]);
    assert!(!ok && said.contains("the control plane did not publish its keys"), "{said}");
    std::fs::write(&file, &plain).unwrap();
    // Cloudflare refusing to prepare the image.
    let prepare = recorded_answers(&recording, "POST", "/containers/image-preparations")[0].clone();
    let answered = |r: &serde_json::Value, status: u16, answer: serde_json::Value| { let mut r = r.clone(); r["status"] = status.into(); r["answer"] = answer.to_string().into(); r };
    let (ok, said) = run(&[answered(&prepare, 400, serde_json::json!({ "success": false, "errors": [{ "code": 1000, "message": "image too large for this account" }], "result": null }))]);
    assert!(!ok && said.contains("image too large for this account"), "{said}");
    // Each registry asks for a token first (GitHub's with its service and scope, Cloudflare's with neither).
    served.aws.fail_with_headers("actions-runner/manifests/latest", 401, &[("www-authenticate", "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:actions/actions-runner:pull\"")], "", Some(1));
    served.aws.fail_with_headers("superci-runner/manifests/gh-", 401, &[("www-authenticate", "Bearer realm=\"https://registry.cloudflare.com/token\"")], "", Some(1));
    served.aws.fail("registry.cloudflare.com/token", 200, r#"{"token":"cloudflare-registry-token"}"#, Some(1));
    // GitHub's index: Windows and Arm first, and long.
    let index = recorded_answers(&recording, "GET", "/manifests/latest")[0].clone();
    let mut list: serde_json::Value = serde_json::from_str(index["answer"].as_str().unwrap()).unwrap();
    let amd64 = list["manifests"].as_array().unwrap().clone();
    list["manifests"] = serde_json::json!([{ "digest": "sha256:1111", "platform": { "os": "windows", "architecture": "amd64" } }, { "digest": "sha256:2222", "platform": { "os": "linux", "architecture": "arm64" } }]);
    list["manifests"].as_array_mut().unwrap().extend(amd64);
    let long = list.to_string() + &" ".repeat(1_100_000);
    // Not visible at once, then being prepared twice over; and the Worker's containers of one size from before.
    let scripts = recorded_answers(&recording, "GET", "/workers/scripts");
    let mut before: serde_json::Value = serde_json::from_str(scripts[1]["answer"].as_str().unwrap()).unwrap();
    before["result"].as_array_mut().unwrap().push(serde_json::json!({ "id": format!("superci-runners-{}", superci_testkit::PLANE), "migration_tag": "v2" }));
    let apps = recorded_answers(&recording, "GET", "/containers/applications");
    let (ok, said) = run(&[
        { let mut r = index.clone(); r["answer"] = long.clone().into(); r }, answered(&prepare, 400, serde_json::json!({ "success": false, "errors": [{ "code": 1, "message": "IMAGE_REGISTRY_DOESNT_CONTAIN_IMAGE" }], "result": null })),
        answered(&prepare, 200, serde_json::json!({ "success": true, "result": { "status": "preparing" } })), answered(&prepare, 200, serde_json::json!({ "success": true, "result": { "status": "preparing" } })), prepare.clone(),
        scripts[0].clone(), answered(&scripts[1], 200, before),
        answered(&apps[0], 200, serde_json::json!({ "success": true, "result": [{ "name": format!("superci-runners-{}", superci_testkit::PLANE) }] })), apps[0].clone(),
    ]);
    assert!(ok && said.contains("cloudflare runs jobs now."), "{said}");
    assert!(said.contains("Cloudflare is preparing the runner image for its machines…") && said.matches("Cloudflare is preparing").count() == 1, "{said}");
    let asked: Vec<String> = served.aws.asked.lock().unwrap().iter().map(|a| format!("{} {}", a["method"].as_str().unwrap_or_default(), a["url"].as_str().unwrap_or_default())).collect();
    assert!(asked.iter().any(|a| a == "GET https://registry.cloudflare.com/token") && asked.iter().any(|a| a.starts_with("GET https://ghcr.io/token?service=ghcr.io&scope=")), "{asked:?}");
    // (Its applications looked at for the one from before, which has no id to delete it by: left.)
    assert!(asked.iter().filter(|a| a.starts_with("GET") && a.ends_with("/containers/applications")).count() == 2 && !asked.iter().any(|a| a.starts_with("DELETE") && a.contains("/containers/applications/")), "{asked:?}");
    assert!(asked.iter().any(|a| a.contains("/manifests/sha256:660f7b9d")) && !asked.iter().any(|a| a.contains("/manifests/sha256:1111") || a.contains("/manifests/sha256:2222")), "{asked:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A Cloudflare control plane, as Cloudflare answered a real one but where it did not: its address not given (asked
/// again and again, then said); then deployed, its containers of one size from before going; deleted with
/// Cloudflare failing to delete its Worker (said, nothing forgotten), then saying it is not there (taken as gone).
#[test]
fn a_cloudflare_control_plane_says_an_address_cloudflare_never_gave_drops_containers_from_before_and_is_deleted_once_its_worker_is_gone() {
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let cf = superci_testkit::serve_as("cfplane00001", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        if let Some((_, on)) = settings.iter().find(|(k, _)| k == "CONTAINERS") { c.containers = on == "on" }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00001/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let run = |recording: &str, overlay: &[serde_json::Value], args: &[&str]| {
        served.aws.replaying(recording, &[]);
        if !overlay.is_empty() { served.aws.replaying(&overlay.iter().map(|v| v.to_string() + "\n").collect::<String>(), &[]) }
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", "cfplane00001").env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let answered = |r: &serde_json::Value, method: &str, url_end: &str, status: u16, answer: serde_json::Value| {
        let mut r = r.clone();
        if !method.is_empty() { r["method"] = method.into() }
        if !url_end.is_empty() { r["url"] = format!("https://api.cloudflare.com/client/v4/accounts/1111111111111111111111111111111a{url_end}").into() }
        r["status"] = status.into(); r["answer"] = answer.to_string().into(); r
    };
    let deploy = include_str!("../fixtures/cloudflare/02-deploy.jsonl");
    let subdomain = recorded_answers(deploy, "POST", "/workers/scripts/superci/subdomain")[0].clone();
    let refused = serde_json::json!({ "success": false, "errors": [{ "code": 10013, "message": "workers.dev is not set up for this account yet" }], "result": null });
    let (ok, said) = run(deploy, &[answered(&subdomain, "", "", 400, refused)], &["planes", "create", "cloudflare"]);
    assert!(!ok && said.contains("workers.dev is not set up for this account yet"), "{said}");
    assert_eq!(served.aws.asked.lock().unwrap().iter().filter(|a| a["method"] == "POST" && a["url"].as_str().is_some_and(|u| u.ends_with("/superci/subdomain"))).count(), 10);
    // Deployed, its containers of one size from before (their application, `superci-runners`) gone.
    let scripts = recorded_answers(deploy, "GET", "/workers/scripts");
    let mut before: serde_json::Value = serde_json::from_str(scripts[1]["answer"].as_str().unwrap()).unwrap();
    before["result"].as_array_mut().unwrap().push(serde_json::json!({ "id": "superci", "migration_tag": "v2" }));
    let (ok, said) = run(deploy, &[scripts[0].clone(), answered(&scripts[1], "", "", 200, before),
        answered(&scripts[0], "GET", "/containers/applications", 200, serde_json::json!({ "success": true, "result": [{ "id": "00000000000000000000000000000009", "name": "superci-runners" }, { "id": "0000000000000000000000000000000a", "name": "another-app" }] })),
        answered(&scripts[0], "DELETE", "/containers/applications/00000000000000000000000000000009", 200, serde_json::json!({ "success": true, "result": {} }))], &["planes", "create", "cloudflare"]);
    assert!(ok && said.contains("The control plane is running: https://superci.acme.workers.dev (Cloudflare · Acme)"), "{said}");
    let deleted: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["method"] == "DELETE").filter_map(|a| a["url"].as_str().map(str::to_string)).collect();
    assert!(deleted == ["https://api.cloudflare.com/client/v4/accounts/1111111111111111111111111111111a/containers/applications/00000000000000000000000000000009"], "{deleted:?}");
    // Deleted: Cloudflare failing to delete its Worker, then saying there is none.
    let delete = [include_str!("../fixtures/cloudflare/03-use.jsonl"), include_str!("../fixtures/cloudflare/07-delete.jsonl")].concat();
    let worker = recorded_answers(&delete, "DELETE", "/workers/scripts/superci?force=true")[0].clone();
    let (ok, said) = run(&delete, &[answered(&worker, "", "", 500, serde_json::json!({ "success": false, "errors": [{ "code": 10013, "message": "An unknown error has occurred" }], "result": null }))], &["planes", "delete", "cfplane00001", "--confirm"]);
    assert!(!ok && said.contains("An unknown error has occurred"), "{said}");
    let (ok, said) = run(&delete, &[answered(&worker, "", "", 404, serde_json::json!({ "success": false, "errors": [{ "code": 0, "message": "Worker Not Found" }], "result": null }))], &["planes", "delete", "cfplane00001", "--confirm"]);
    assert!(ok && said.contains("Deleted Cloudflare · Acme."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// An AWS control plane's deploy as AWS answered a real one, but where it did not: Lambda refusing to make the
/// function or its converter for want of a role, until it refuses for another reason (said as it said it) or for
/// as long as it is asked (said, the last refusal); and the function made but failed to update.
#[test]
fn an_aws_deploy_says_what_lambda_refused_of_the_function_and_its_converter_and_a_function_that_failed_to_update() {
    let account = superci_testkit::Aws::serve();
    let home = common::aws_only(&account);
    let recording = include_str!("../fixtures/aws/planes/deploy.jsonl").replace("testplane0002", "deployplane1");
    let deploy = |overlay: &[serde_json::Value]| {
        account.replaying(&recording, &[]);
        if !overlay.is_empty() { account.replaying(&overlay.iter().map(|v| v.to_string() + "\n").collect::<String>(), &[]) }
        let o = common::command(&home).args(["planes", "create", "aws", "--region=us-east-1"]).env("SUPERCI_NEW_PLANE_ID", "deployplane1").env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        account.fail_none();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    // (Lambda's refusal to make a function, as Lambda words it: the control plane's, or its converter's.)
    let refuse = |which: &str, message: &str, times: Option<u32>| account.fail(&format!("2015-03-31/functions|\"Handler\"|{which}"), 400, &serde_json::json!({ "Type": "User", "message": message }).to_string(), times);
    let creates = || account.asked.lock().unwrap().iter().filter(|a| a["method"] == "POST" && a["url"].as_str().is_some_and(|u| u.ends_with("/2015-03-31/functions"))).count();
    let not_yet = "The role is still being made: try again in a few seconds.";
    let other = "Unsupported architecture for provided.al2023 here.";
    let cannot = "The role defined for the function cannot be assumed by Lambda.";
    // A role not ready, then another refusal; then a role never assumed: the function's, then its converter's.
    for (which, made_first) in [("SuperCI control plane", 0), ("layer store converter", 1)] {
        refuse(which, not_yet, Some(1));
        refuse(which, other, Some(1));
        let (ok, said) = deploy(&[]);
        assert!(!ok && said.contains(other), "{which}: {said}");
        refuse(which, cannot, None);
        let before = creates();
        let (ok, said) = deploy(&[]);
        assert!(!ok && said.contains(cannot) && creates() - before == 12 + made_first, "{which}: {} {said}", creates() - before);
    }
    // Made, and failed to update while it was still starting.
    let mut config = recorded_answers(&recording, "GET", "/functions/superci-plane-deployplane1/configuration")[0].clone();
    let mut v: serde_json::Value = serde_json::from_str(config["answer"].as_str().unwrap()).unwrap();
    (v["State"], v["LastUpdateStatus"], v["LastUpdateStatusReason"]) = ("Pending".into(), "Failed".into(), "The code could not be unpacked.".into());
    config["answer"] = v.to_string().into();
    let (ok, said) = deploy(&[config]);
    assert!(!ok && said.contains("the function failed to start:"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Before an AWS control plane is made in a region: the image jobs run on not there is said, and nothing is made;
/// AWS not saying whether it is (an error of its own) is left to the deploy, which goes on.
#[test]
fn an_aws_deploy_into_a_region_without_the_image_is_refused_and_one_aws_did_not_say_goes_on() {
    let account = superci_testkit::Aws::serve();
    let home = common::aws_only(&account);
    let recording = include_str!("../fixtures/aws/planes/deploy.jsonl").replace("testplane0002", "deployplane1");
    let deploy = || {
        account.replaying(&recording, &[]);
        let o = common::command(&home).args(["planes", "create", "aws", "--region=us-east-1"]).env("SUPERCI_NEW_PLANE_ID", "deployplane1").env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        account.fail_none();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    account.fail("DescribeImages", 200, r#"<?xml version="1.0" encoding="UTF-8"?><DescribeImagesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>1</requestId><imagesSet/></DescribeImagesResponse>"#, None);
    let (ok, said) = deploy();
    assert!(!ok && said.contains("The image SuperCI's jobs run on is not in us-east-1 yet, so no job could start there. Choose another region.") && !said.contains("Creating its storage"), "{said}");
    // AWS failing to say (once): the deploy goes on, to where Lambda refuses its function.
    account.fail("DescribeImages", 500, &superci_testkit::ec2_error("InternalError", "An internal error has occurred."), Some(1));
    account.fail("2015-03-31/functions|\"Handler\"", 400, r#"{"Type":"User","message":"Unsupported architecture for provided.al2023 here."}"#, None);
    let (ok, said) = deploy();
    assert!(!ok && said.contains("Uploading it (a Lambda function)…") && said.contains("Unsupported architecture for provided.al2023 here."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// AWS's runners for a control plane elsewhere: IAM refusing the identity provider they start machines through (not
/// because it is there already) is said as IAM said it, and nothing more is asked.
#[test]
fn aws_runners_for_a_control_plane_elsewhere_say_iams_refusal_of_their_identity_provider() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"] = serde_json::json!({ "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci", "url": served.url, "plane_id": superci_testkit::PLANE, "label": "superci" } });
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.fail("CreateOpenIDConnectProvider", 403, &superci_testkit::ec2_error("AccessDenied", "User: arn:aws:sts::111111111111:assumed-role/Admin/someone is not authorized to perform: iam:CreateOpenIDConnectProvider"), None);
    let o = superci(&home, &["runners", "create", "aws", "--region=us-east-1"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("not authorized to perform: iam:CreateOpenIDConnectProvider"), "{said}");
    assert!(!served.aws.asked.lock().unwrap().iter().any(|a| a["asked"].as_str().is_some_and(|s| s.starts_with("Action=CreateRole&"))), "no role made after");
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane with GitLab connected and no GitHub App: jobs can come and run, and the status says so.
#[test]
fn a_control_plane_with_gitlab_alone_and_runners_is_ready_for_jobs() {
    let served = serve(|_| {});
    served.connect_gitlab();
    let home = home(&served);
    let (code, v) = json(&home, &["status"]);
    assert!(code == 0 && v["control_plane"]["ready"] == true, "{v}");
    let said = String::from_utf8_lossy(&superci(&home, &["status"]).stdout).into_owned();
    assert!(said.contains("Jobs can run  yes"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// SuperCI stopped from the CLI with another control plane in the workspace beside the one in use: one that serves
/// nothing and answers goes with it, and so does one that had moved to the one in use (it only passed jobs on).
#[test]
fn leaving_deletes_another_control_plane_that_serves_nothing_or_had_moved_to_the_one_in_use() {
    for moved in [false, true] {
        let served = serve(|_| {});
        served.connect_github();
        let other = superci_testkit::serve_as("testplane0002", |_| {});
        if moved { other.store.0.lock().unwrap().insert("moved_to".into(), serde_json::json!(served.url).to_string()); }
        let home = common::signed_in(&served);
        served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
        for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
        let file = home.join("workspaces/default/sign-ins.json");
        let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } }]);
        kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
        std::fs::write(&file, kept.to_string()).unwrap();
        let o = common::command(&home).args(["leave", "--confirm"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(said.contains("SuperCI is stopped") && !said.contains("testplane0002 --confirm"), "{moved}: {said}");
        // (Its settings gone from SSM, as its deletion removes them.)
        assert!(served.aws.settings("testplane0002").is_empty(), "{moved}: {:?} {said}", served.aws.settings("testplane0002"));
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// What the control plane's AWS role lacks, read before it is given: a region its machines use with no network of
/// its own is said among it.
#[test]
fn a_region_without_the_control_planes_own_network_is_said_among_what_its_role_lacks() {
    let served = serve(|c| {
        (c.aws_own, c.aws_own_creds) = (Some(("111111111111".into(), "us-east-1".into())), Some(superci_core::aws::Credentials { access_key_id: "ASIAX".into(), secret_access_key: "secret".into(), session_token: Some("tok".into()), expires_at_ms: u64::MAX }));
    });
    let home = common::signed_in(&served);
    served.aws.fail("DescribeVpcs", 200, r#"<?xml version="1.0" encoding="UTF-8"?><DescribeVpcsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>1</requestId><vpcSet/></DescribeVpcsResponse>"#, None);
    let o = superci(&home, &["planes", "allow", "--dry-run"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("A network of its own in each region its machines use"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The account's copy of the image of an older build than the newest published: `images update` copies the newer
/// one in, as AWS answered a real copy, and says what happens to the older.
#[test]
fn an_older_build_of_the_accounts_copy_is_updated_with_the_newest_from_the_cli() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[]);
    served.aws.replaying(include_str!("../fixtures/aws/images/05-update-newest.jsonl"), &[("20261006080844", "20250101000000")]);
    let o = superci(&home, &["images", "update", "ca-central-1"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("into ca-central-1, from us-east-1") && said.contains("The older copy is removed once the new one is there"), "{said}");
    assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["asked"].as_str().is_some_and(|s| s.starts_with("Action=CopyImage")) && a["url"].as_str().is_some_and(|u| u.contains("ca-central-1"))));
    let _ = std::fs::remove_dir_all(&home);
}

/// A runner on a machine of a run from a computer deletes the message it took (as GitHub's runner does once it has
/// it): answered, nothing more, and the job goes on to its end.
#[test]
fn a_runner_deleting_the_message_it_took_is_answered_and_its_job_goes_on() {
    use superci_testkit::Runner;
    use std::io::{Read, Write};
    let served = serve(|_| {});
    let dir = common::temp("message-deleted");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make test\n").unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(std::process::Command::new("git").current_dir(&dir).args(args).status().unwrap().success()) }
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(&dir).args(["runs", "create", "--workflow=.github/workflows/ci.yml", "--quiet"])
        .env("SUPERCI_HOME", dir.join(".cli")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &served.run_key).env("SUPERCI_STAND_IN", &served.aws.url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let run = loop { if let Some((k, _)) = served.store.under("run:").into_iter().next() { break k.trim_start_matches("run:").to_string() } std::thread::sleep(std::time::Duration::from_millis(50)) };
    let runner = loop { if let Some(r) = Runner::of_run(&served, &run) { break r } std::thread::sleep(std::time::Duration::from_millis(50)) };
    runner.take();
    // (As the runner asks it: DELETE, on the message's address.)
    let u = runner.at.trim_start_matches("http://");
    let (host, rest) = u.split_once('/').unwrap();
    let mut s = std::net::TcpStream::connect(host).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    s.write_all(format!("DELETE /{rest}/Message/1 HTTP/1.1\r\nhost: {host}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = s.read_to_string(&mut answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let step = "dddddddd-0000-0000-0000-000000000001";
    runner.steps(&[(step, "Run make test", 1, "completed", Some("succeeded"), Some(1))]);
    assert_eq!(runner.finish("succeeded"), 200);
    let _ = cli.wait_with_output();
    let job = served.store.under("job:").into_iter().map(|(_, j)| j).find(|j| j["run_id"].to_string().trim_matches('"') == run).unwrap_or_default();
    assert_eq!(job["conclusion"], "success", "{job}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A job's `timeout-minutes` given by an expression that indexes a list: read as the item it names.
#[test]
fn a_jobs_timeout_given_by_an_expression_indexing_a_list_is_the_item_it_names() {
    let served = serve(|_| {});
    let dir = common::temp("timeout-indexed");
    common::played_steps(&served, &dir, "jobs:\n  tests:\n    runs-on: superci\n    timeout-minutes: ${{ fromJSON('[7, 30]')[1] }}\n    steps:\n      - run: make test\n", &[("Run make test", "succeeded")]);
    let (_, run) = served.store.under("run:").into_iter().next().expect("the run");
    assert_eq!(run["jobs"][0]["timeout_ms"], 1_800_000, "{run}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A move onto another control plane that serves GitHub of its own is refused: it is another CI.
#[test]
fn a_move_onto_a_control_plane_that_serves_github_of_its_own_is_refused() {
    let served = serve(|_| {});
    served.connect_github();
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    other.aws.put("/superci/testplane0002/GITHUB_APP", &superci_testkit::GitHub::app().to_string());
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let o = superci(&home, &["planes", "move", "testplane0002", "--plane=testplane0001"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("That control plane serves a code host of its own") && said.contains("Nothing has changed."), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The GitHub App asked for an organization with a long name: its name cut, with a mark of the whole of it, to fit
/// GitHub's limit.
#[test]
fn the_app_asked_for_an_organization_with_a_long_name_has_it_cut_with_a_mark_of_the_whole() {
    let served = serve(|_| {});
    let login = "a-very-long-organization";
    served.github.answer_with("GET", &format!("/users/{login}"), 200, &[("content-type", "application/json")], &serde_json::json!({ "login": login, "id": 7, "type": "Organization" }).to_string());
    let home = home(&served);
    std::fs::write(home.join("aws-at"), &served.aws.url).unwrap();
    let (_board, link) = common::dashboard(&home);
    let (status, _, page) = common::post(&link, "/github/start", &[("login", login)]);
    let mark = &superci_core::crypto::sha256_hex(login.as_bytes())[..4];
    assert!(status < 400 && page.replace("&quot;", "\"").contains(&format!("\"name\":\"SuperCI a-very-lon-{mark}\"")), "{status} {}", common::words(&page));
    let _ = std::fs::remove_dir_all(&home);
}

/// The CLI with a run key, in `dir`, reaching the control plane at `plane`: whether it succeeded, and all it said;
/// ended after `secs` at most.
fn keyed_within(served: &superci_testkit::Served, plane: &str, dir: &std::path::Path, args: &[&str], envs: &[(&str, &str)], secs: u64) -> (bool, String) {
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(dir).args(args).env("SUPERCI_HOME", dir.join(".home")).env("SUPERCI_PLANE", plane).env("SUPERCI_KEY", &served.run_key)
        .env_remove("SUPERCI_LOGS_WAIT_S").env("SUPERCI_STAND_IN", &served.aws.url).envs(envs.iter().copied()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });
    let out = match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(out) => out.unwrap(),
        Err(_) => { let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status(); panic!("{args:?}: not over in {secs} s") }
    };
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

/// A run followed as its control plane gives it: a step's pages that hold nothing past what was out add nothing; a
/// step never begun in a failed job is no result to wait for; a job running whose start's times say no step of it
/// is said without one; a machine that keeps Docker's images and said nothing of them says nothing.
#[test]
fn a_run_followed_says_nothing_of_what_its_control_plane_gives_without_anything_in_it() {
    let served = serve(|_| {});
    let dir = common::temp("followed-empty");
    let t = "2026-10-15T10:00:00.0000000Z";
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let plane = superci_testkit::front(&served.url, move |method, target, status, body| {
        if method != "GET" { return (status, body) }
        match target.split_once("/runs/1/log?").map(|(_, q)| q) {
            Some("job=0&step=a1") => return (200, format!("{t} l1").into_bytes()),
            Some("job=0&log=1") => return (200, format!("{t} l1\n{t} l2\n{t} l3\n{t} l4").into_bytes()),
            Some(_) => return (404, b"nothing here".to_vec()),
            None => {}
        }
        if !target.starts_with("/runs/1?") { return (status, body) }
        let first = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let posts = |p: serde_json::Value| if first { p } else { serde_json::json!([]) };
        let answer = serde_json::json!({ "over": !first, "result": if first { serde_json::Value::Null } else { "failure".into() }, "jobs": [
            { "name": "build", "job_id": "11", "state": "done", "result": "failure", "start": { "keeps": true, "images": { "at": 1 } },
                "posts": posts(serde_json::json!([{ "step": "a1", "start": 1, "lines": ["l1"] }, { "step": "a1", "start": 4, "lines": ["l4"] }])),
                "steps": [{ "id": "a1", "name": "Run make", "order": 1, "state": "completed", "result": "failed", "log": 1, "start": "2026-10-15T10:00:00Z" },
                    { "id": "a2", "name": "Run make test", "order": 2, "state": "pending", "result": null }] },
            { "name": "later", "state": if first { "running" } else { "done" }, "result": if first { serde_json::Value::Null } else { "success".into() },
                "start": { "at_ms": 1000, "launched_ms": 500, "machine_id": "i-1", "started_ms": 400 }, "posts": [], "steps": [] }] });
        (200, answer.to_string().into_bytes())
    });
    let (ok, said) = keyed_within(&served, &plane, &dir, &["runs", "wait", "1"], &[("SUPERCI_GIVE_UP_S", "1"), ("SUPERCI_LOGS_WAIT_S", "4")], 60);
    assert!(!ok && said.contains("The run ended: failure"), "{said}");
    assert!(said.lines().any(|l| l.trim_end() == "[later] Running…") && !said.contains("Docker images"), "{said}");
    assert!(said.matches("l1").count() == 1 && said.contains("l2"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One call to Cloudflare's API asked with no body, as a change that takes none is asked: sent empty, and what
/// Cloudflare (here, the test kit, which has no answer for it) said is said.
#[test]
fn a_call_to_cloudflares_api_that_changes_something_is_sent_with_no_body_when_given_none() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    for method in ["POST", "PUT"] {
        let o = superci(&home, &["debug", "cloudflare", method, "/accounts/x/workers/scripts/superci/subdomain"]);
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(o.status.code().is_some_and(|c| c <= 1) && !said.contains("Sign in with Cloudflare first"), "{method}: {said}");
        assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["method"] == method && a["url"].as_str().is_some_and(|u| u.ends_with("/superci/subdomain"))), "{method}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A control plane that fails to give a job's log or GitHub's deliveries (not for want of a newer version): said as
/// it said it.
#[test]
fn a_control_plane_failing_to_give_a_jobs_log_or_githubs_deliveries_is_said_as_it_said_it() {
    let served = serve(|_| {});
    served.connect_github();
    let home = home(&served);
    played(&served, &home.join("checkout"));
    let front = superci_testkit::front(&served.url, |method, target, status, body| {
        if method == "GET" && (target.starts_with("/job/log?") || target.starts_with("/github/deliveries")) { return (500, b"the store is down".to_vec()) }
        (status, body)
    });
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["plane"]["Aws"]["url"] = front.into();
    std::fs::write(&file, kept.to_string()).unwrap();
    let (_, jobs) = json(&home, &["jobs", "list"]);
    let id = jobs["jobs"][0]["id"].to_string();
    for args in [&["jobs", "retrieve", &id][..], &["deliveries", "list"]] {
        let o = superci(&home, args);
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success() && said.contains("500") && said.contains("the store is down") && !said.contains("planes update"), "{args:?}: {said}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A setting written to a Modal control plane, as Modal's client keeps a string: one of a few hundred bytes is
/// written and taken; one past what a setting holds there (60 kB) is refused before anything is written.
#[test]
fn a_modal_control_planes_long_setting_is_written_and_one_past_what_it_holds_is_refused() {
    let account = superci_testkit::Aws::serve();
    let acct = account.clone();
    let md = superci_testkit::serve_as("mdplane00005", move |c| { c.read_settings(&acct.modal_plane_settings()); c.own_cloud = "modal".into(); c.containers = true; });
    // (Its key, among the settings it reads.)
    account.modal_settings.lock().unwrap().insert("DEVICE_KEY_4102444800_TEST".into(), md.key.clone());
    let home = common::temp("modal-long");
    std::fs::create_dir_all(home.join("workspaces/default")).unwrap();
    std::fs::write(home.join("workspaces/default/sign-ins.json"), serde_json::json!({
        "modal": { "token_id": "ak-modal-for-tests", "token_secret": "as-modal-for-tests", "workspace": "acmeorg" },
        "key": { "name": "DEVICE_KEY_4102444800_TEST", "value": md.key, "planes": ["mdplane00005"] },
        "plane": { "Modal": { "workspace": "acmeorg", "url": md.url, "plane_id": "mdplane00005", "label": "superci" } },
    }).to_string()).unwrap();
    std::fs::write(home.join("aws-at"), &account.url).unwrap();
    account.replaying(include_str!("../fixtures/modal/03-use.jsonl"), &[]);
    account.replaying(&[modal_answered("DictGetOrCreate", &[pb(&[Pb::B(1, b"di-settings".to_vec())])]), modal_answered("DictUpdate", &[pb(&[Pb::N(1, 1)])])].concat(), &[]);
    let long = format!("acme/{}", "a".repeat(300));
    let o = superci(&home, &["public_repos", "create", &long]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("may run here"), "{said}");
    assert!(account.modal_plane_settings().iter().any(|(k, v)| k == "ROUTING" && v.contains(&long)), "{:?}", account.modal_plane_settings());
    let longest = format!("acme/{}", "b".repeat(60_000));
    let o = superci(&home, &["public_repos", "create", &longest]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("a setting longer than 60 kB"), "{said}");
    assert!(!account.modal_plane_settings().iter().any(|(_, v)| v.contains(&longest)));
    let _ = std::fs::remove_dir_all(&home);
}

/// A runner served a job here asking for its next message while the job runs: answered once the service has held
/// it (as GitHub holds such a request), with none.
#[test]
fn a_runner_asking_for_a_message_while_its_job_runs_here_is_held_then_told_there_is_none() {
    use superci_testkit::Runner;
    let dir = common::temp("debug-runner-held");
    let workflow = dir.join("ci.yml");
    std::fs::write(&workflow, "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make test\n").unwrap();
    let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
    let root = format!("http://127.0.0.1:{port}/runner/rig");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).env("SUPERCI_HOME", dir.join(".home"))
        .args(["debug", "runner-service", workflow.to_str().unwrap(), "tests", &root, &port.to_string(), dir.join("jit").to_str().unwrap(), "--json"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() { assert!(std::time::Instant::now() < until, "it did not listen"); std::thread::sleep(std::time::Duration::from_millis(50)) }
    let runner = Runner { at: format!("{root}/_apis/v1") };
    runner.take();
    let asked = std::time::Instant::now();
    let (status, _) = runner.message();
    assert!(status == 204 && asked.elapsed() >= std::time::Duration::from_secs(9), "{status} after {:?}", asked.elapsed());
    runner.steps(&[("dddddddd-0000-0000-0000-000000000004", "Run make test", 1, "completed", Some("succeeded"), Some(1))]);
    runner.finish("succeeded");
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });
    let out = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the service ended").unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("the job ended: succeeded"), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `SUPERCI_RECORD` with `SUPERCI_RECORD_SIGN_INS=1`: a sign-in's renewal is recorded too (for fixtures of sign-ins).
#[test]
fn a_sign_ins_renewal_is_recorded_only_when_asked_for_too() {
    let served = serve(|_| {});
    served.aws.replaying(include_str!("../fixtures/aws/sign-in/ended.jsonl"), &[]);
    for (asked, kept) in [(None, false), (Some("1"), true)] {
        let home = common::signed_in(&served);
        let file = home.join("workspaces/default/sign-ins.json");
        let mut k: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        (k["aws"]["creds"]["expires_at_ms"], k["aws"]["refresh_token"], k["aws"]["dpop"]) = (0.into(), "superci-ended-refresh-token-for-a-recording".into(), serde_json::to_value(superci_core::crypto::SigningKeyStore::generate()).unwrap());
        std::fs::write(&file, k.to_string()).unwrap();
        let recording = home.join("recording.jsonl");
        let mut c = common::command(&home);
        c.env("SUPERCI_RECORD", &recording).args(["accounts", "retrieve", "aws", "--json"]);
        if let Some(v) = asked { c.env("SUPERCI_RECORD_SIGN_INS", v); }
        let o = c.output().unwrap();
        let text = std::fs::read_to_string(&recording).unwrap_or_default();
        assert_eq!(text.contains("signin.aws.amazon.com"), kept, "{asked:?}: {text} {}", String::from_utf8_lossy(&o.stdout));
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// SuperCI stopped with another control plane beside the one in use that does not answer: left as it is, with the
/// command that deletes it. Or with the machines' network AWS will not let the sign-in look at (its own policy, not
/// the organization's): said as left too.
#[test]
fn leaving_leaves_a_control_plane_that_does_not_answer_and_a_network_the_sign_in_may_not_look_at() {
    for down in [true, false] {
        let served = serve(|_| {});
        served.connect_github();
        let other = superci_testkit::serve_as("testplane0002", |_| {});
        let home = common::signed_in(&served);
        // (Down, the other one is found in AWS as the recording has it; else the one in use is deleted as AWS answered
        // the deletion of one, as it is called here.)
        let replace: &[(&str, &str)] = if down { &[("http://new-plane.test", &other.url)] } else { &[("testplane0002", "testplane0001")] };
        served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), replace);
        for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
        let file = home.join("workspaces/default/sign-ins.json");
        let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        if down {
            kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } }]);
            kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
            other.down.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            served.aws.fail("DescribeInstances", 403, &superci_testkit::ec2_error("UnauthorizedOperation", "You are not authorized to perform this operation. User: arn:aws:sts::111111111111:assumed-role/ReadOnly/someone is not authorized to perform: ec2:DescribeInstances because no identity-based policy allows the ec2:DescribeInstances action"), None);
        }
        std::fs::write(&file, kept.to_string()).unwrap();
        let o = common::command(&home).args(["leave", "--confirm"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        assert!(said.contains("SuperCI is stopped"), "{said}");
        if down { assert!(said.contains("did not answer and was left as it is (`superci planes delete testplane0002 --confirm` deletes it)"), "{said}") }
        else { assert!(said.contains("Its machines' network in AWS (us-east-1: UnauthorizedOperation") && said.contains("no identity-based policy allows the ec2:DescribeInstances action"), "{said}") }
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// A move back onto the control plane this one moved from (it still has its GitHub App): not refused as another
/// CI's; on to what the move needs (here a sign-in for the runners that come along).
#[test]
fn a_move_back_onto_the_control_plane_this_one_moved_from_is_not_refused_as_another_cis() {
    let served = serve(|_| {});
    served.connect_github();
    served.aws.put(&format!("/superci/{}/LAUNCHERS", superci_testkit::PLANE), r#"[{"cloud":"cloudflare","url":"https://superci-runners-testplane0001.acme.workers.dev"}]"#);
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    other.aws.put("/superci/testplane0002/GITHUB_APP", &superci_testkit::GitHub::app().to_string());
    other.store.0.lock().unwrap().insert("moved_to".into(), serde_json::json!(served.url).to_string());
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let o = superci(&home, &["planes", "move", "testplane0002", "--plane=testplane0001"]);
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!said.contains("serves a code host of its own") && said.contains("Sign in with Cloudflare first"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A GitHub App made on GitHub Enterprise Cloud (a `.ghe.com` host), installed nowhere yet: installed from its
/// host's own pages, as on github.com (not under /github-apps, as on a server of one's own).
#[test]
fn an_app_on_a_ghe_com_host_installed_nowhere_yet_is_installed_from_its_hosts_apps_page() {
    let served = serve(|_| {});
    let mut app = superci_testkit::GitHub::app();
    app["host"] = "acme.ghe.com".into();
    served.aws.put(&format!("/superci/{}/GITHUB_APP", superci_testkit::PLANE), &app.to_string());
    served.github.set_installed(false, true);
    let home = common::signed_in(&served);
    let (_board, link) = common::dashboard(&home);
    let want = "https://acme.ghe.com/apps/superci-acme/installations/new";
    let page = (0..40).map(|_| { std::thread::sleep(std::time::Duration::from_millis(250)); common::part(&link, "/?p=repos&fragment=1").1 }).find(|p| p.contains("installations/new")).unwrap_or_default();
    assert!(page.contains(want), "{}", common::words(&page));
    let _ = std::fs::remove_dir_all(&home);
}

/// A secret for workflows whose file a pattern names, the pattern opening a class it never closes (taken as the
/// character itself): given to a run of a workflow so named, not to another starting the same way.
#[test]
fn a_secrets_workflow_pattern_with_an_unclosed_class_is_taken_as_the_character_itself() {
    use superci_testkit::Runner;
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    for (name, pattern, value) in [("GIVEN", "[ci.yml", "given-value-1"), ("NOT_GIVEN", "[cd.yml", "other-value-2")] {
        let o = common::command(&home).args(["secrets", "create", name, "--value-env=THE_VALUE", &format!("--workflow={pattern}")]).env("THE_VALUE", value).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let dir = common::temp("bracket-workflow");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/[ci.yml"), "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make test ${{ secrets.GIVEN }} ${{ secrets.NOT_GIVEN }}\n").unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(std::process::Command::new("git").current_dir(&dir).args(args).status().unwrap().success()) }
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_superci")).current_dir(&dir).args(["runs", "create", "--workflow=.github/workflows/[ci.yml", "--quiet"])
        .env("SUPERCI_HOME", dir.join(".cli")).env("SUPERCI_PLANE", &served.url).env("SUPERCI_KEY", &served.run_key).env("SUPERCI_STAND_IN", &served.aws.url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let run = loop { if let Some((k, _)) = served.store.under("run:").into_iter().next() { break k.trim_start_matches("run:").to_string() } assert!(std::time::Instant::now() < until, "no run"); std::thread::sleep(std::time::Duration::from_millis(50)) };
    let runner = loop { if let Some(r) = Runner::of_run(&served, &run) { break r } assert!(std::time::Instant::now() < until, "no runner"); std::thread::sleep(std::time::Duration::from_millis(50)) };
    runner.take();
    let handed = runner.handed(&served).to_string();
    assert!(handed.contains("given-value-1") && !handed.contains("other-value-2"), "{handed}");
    runner.steps(&[("dddddddd-0000-0000-0000-000000000001", "Run make test", 1, "completed", Some("succeeded"), Some(1))]);
    runner.finish("succeeded");
    let _ = cli.wait_with_output();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// Three control planes in the workspace's clouds, two serving GitHub each and one serving nothing: the two are
/// each live, the third neither live nor in use.
#[test]
fn of_three_control_planes_two_live_the_one_that_serves_nothing_is_listed_neither_live_nor_in_use() {
    let served = serve(|_| {});
    served.connect_github();
    let other = superci_testkit::serve_as("testplane0002", |_| {});
    other.aws.put("/superci/testplane0002/GITHUB_APP", &superci_testkit::GitHub::app().to_string());
    let third = superci_testkit::serve_as("testplane0003", |_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/aws/planes/delete.jsonl"), &[("http://new-plane.test", &other.url)]);
    for (k, v) in other.aws.settings("testplane0002") { served.aws.put(&format!("/superci/testplane0002/{k}"), &v) }
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["planes_more"] = serde_json::json!([{ "Aws": { "account_id": "111111111111", "region": "us-east-1", "url": other.url, "plane_id": "testplane0002", "label": "superci" } },
        { "Cloudflare": { "account_id": "1111111111111111111111111111111a", "account_name": "Acme", "script": "superci", "url": third.url, "plane_id": "testplane0003", "label": "superci" } }]);
    kept["key"]["planes"] = serde_json::json!(["testplane0001", "testplane0002", "testplane0003"]);
    std::fs::write(&file, kept.to_string()).unwrap();
    let said = String::from_utf8_lossy(&superci(&home, &["planes", "list"]).stdout).into_owned();
    for id in ["testplane0001", "testplane0002"] { assert!(said.lines().any(|l| l.starts_with(id) && l.ends_with("  live")), "{said}") }
    assert!(said.lines().any(|l| l.starts_with("testplane0003") && !l.ends_with("live") && !l.ends_with("in use")), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A Cloudflare control plane deleted, Cloudflare saying its Worker is not there by its code alone (10007, in
/// words that do not say "not found"), and IAM that the role its AWS runners had is not there: each taken as gone.
#[test]
fn a_cloudflare_control_plane_is_deleted_though_its_worker_and_its_aws_runners_role_are_gone_already() {
    let served = serve(|_| {});
    served.connect_github();
    let aws = served.aws.clone();
    let cf = superci_testkit::serve_as("cfplane00001", move |c| {
        let settings = aws.worker_settings("superci");
        c.read_settings(&settings);
        if let Some((_, on)) = settings.iter().find(|(k, _)| k == "CONTAINERS") { c.containers = on == "on" }
    });
    cf.aws.params.lock().unwrap().remove("/superci/cfplane00001/CONTAINERS");
    served.aws.workers.lock().unwrap().insert("superci.acme.workers.dev".into(), cf.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let run = |recording: &str, overlay: &[serde_json::Value], args: &[&str]| {
        served.aws.replaying(recording, &[]);
        if !overlay.is_empty() { served.aws.replaying(&overlay.iter().map(|v| v.to_string() + "\n").collect::<String>(), &[]) }
        let o = common::command(&home).args(args).env("SUPERCI_NEW_PLANE_ID", "cfplane00001").env("SUPERCI_PAUSE_MS", "1").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
    };
    let (ok, said) = run(include_str!("../fixtures/cloudflare/02-deploy.jsonl"), &[], &["planes", "create", "cloudflare"]);
    assert!(ok && said.contains("The control plane is running"), "{said}");
    let delete = [include_str!("../fixtures/cloudflare/03-use.jsonl"), include_str!("../fixtures/cloudflare/07-delete.jsonl")].concat();
    let mut worker = recorded_answers(&delete, "DELETE", "/workers/scripts/superci?force=true")[0].clone();
    (worker["status"], worker["answer"]) = (404.into(), serde_json::json!({ "success": false, "errors": [{ "code": 10007, "message": "This Worker does not exist on your account." }], "result": null }).to_string().into());
    // (Its machines in the AWS account signed in to, through a role made for it.)
    cf.store.0.lock().unwrap().insert("aws".into(), serde_json::json!({ "account_id": "111111111111", "region": "us-east-1", "role_arn": "arn:aws:iam::111111111111:role/superci-plane-cfplane00001", "connected": true }).to_string());
    served.aws.fail("DeleteRolePolicy", 404, &superci_testkit::ec2_error("NoSuchEntity", "The role with name superci-plane-cfplane00001 cannot be found."), None);
    let (ok, said) = run(&delete, &[worker], &["planes", "delete", "cfplane00001", "--confirm"]);
    assert!(ok && said.contains("Deleted Cloudflare · Acme."), "{said}");
    assert!(served.aws.asked.lock().unwrap().iter().any(|a| a["asked"].as_str().is_some_and(|s| s.starts_with("Action=DeleteRole&"))), "on past the policy gone already");
    let _ = std::fs::remove_dir_all(&home);
}

/// A Modal token given for one run (SUPERCI_MODAL_TOKEN_ID and _SECRET), a new workspace made to look after a CI
/// it reaches: it is said to, its key handed over, and nothing of it is kept on this machine (what a sign-in given
/// for a run reaches is that run's).
#[test]
fn a_workspace_given_a_ci_with_a_modal_token_for_the_run_alone_keeps_nothing_of_it() {
    let account = superci_testkit::Aws::serve();
    let acct = account.clone();
    let md = superci_testkit::serve_as("mdplane00001", move |c| { c.read_settings(&acct.modal_plane_settings()); c.own_cloud = "modal".into(); });
    account.workers.lock().unwrap().insert("acmeorg--superci-plane-mdplane00001.modal.run".into(), md.url.clone());
    let home = common::temp("modal-given");
    std::fs::write(home.join("aws-at"), &account.url).unwrap();
    account.replaying(include_str!("../fixtures/modal/03-use.jsonl"), &[]);
    account.replaying(&[modal_answered("DictGetOrCreate", &[pb(&[Pb::B(1, b"di-settings".to_vec())])]), modal_answered("DictUpdate", &[pb(&[Pb::N(1, 1)])])].concat(), &[]);
    let given = |args: &[&str]| { let o = common::command(&home).args(args).env("SUPERCI_MODAL_TOKEN_ID", "ak-modal-for-tests").env("SUPERCI_MODAL_TOKEN_SECRET", "as-modal-for-tests").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)) };
    let (ok, said) = given(&["workspaces", "create", "Acme"]);
    assert!(ok, "{said}");
    let (ok, said) = given(&["workspaces", "update", "acme", "--plane=mdplane00001"]);
    assert!(ok && said.contains("This workspace looks after Modal"), "{said}");
    assert!(account.modal_plane_settings().iter().any(|(k, _)| k.starts_with("DEVICE_KEY_")), "{:?}", account.modal_plane_settings());
    assert!(!home.join("workspaces/acme/sign-ins.json").exists(), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A working tree larger packed than a control plane without a bucket takes through itself: said, with how large
/// it is, and no run is made.
#[test]
fn a_working_tree_larger_than_a_control_plane_without_a_bucket_takes_is_said_and_not_run() {
    let served = serve(|_| {});
    let plane = superci_testkit::front(&served.url, |method, target, status, body| {
        // (No bucket: no link to put it in.)
        if method == "POST" && target.starts_with("/runs/tree?") { return (200, b"{}".to_vec()) }
        (status, body)
    });
    let dir = common::temp("large-tree");
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(dir.join(".github/workflows/ci.yml"), "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: make test\n").unwrap();
    // (What does not pack smaller than it is.)
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let noise: Vec<u8> = (0..8_000).flat_map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x.to_le_bytes() }).collect();
    std::fs::write(dir.join("noise.bin"), noise).unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "t"]] { assert!(std::process::Command::new("git").current_dir(&dir).args(args).status().unwrap().success()) }
    let (ok, said) = keyed_within(&served, &plane, &dir, &["runs", "create", "--workflow=.github/workflows/ci.yml"], &[("SUPERCI_MOST_KB", "32")], 60);
    assert!(!ok && said.contains("The working tree is 0 MB packed; a run from a computer takes 0 MB at most."), "{said}");
    assert!(served.store.under("run:").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Modal token given by name with a control character in it, which no header may carry: left out of the call
/// (never sent cut or altered), and Modal's refusal of a call without it said.
#[test]
fn a_modal_token_given_by_name_with_a_control_character_is_left_out_and_modals_refusal_said() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    served.aws.replaying(include_str!("../fixtures/modal/sign-in/ended.jsonl"), &[]);
    let o = common::command(&home).args(["planes", "create", "modal"]).env("SUPERCI_MODAL_TOKEN_ID", "ak-\u{1}token").env("SUPERCI_MODAL_TOKEN_SECRET", "as-superciendedrecording00").output().unwrap();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && said.contains("Token not found"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// GitHub's runner image copied into the account's registry, one layer of it not there yet and larger than a chunk
/// the upload goes in: sent in chunks, each where the registry says to go on, then the layer and the image put.
#[test]
fn a_layer_larger_than_a_chunk_is_copied_into_cloudflares_registry_a_chunk_at_a_time() {
    let served = serve(|_| {});
    served.connect_github();
    served.aws.workers.lock().unwrap().insert("superci-runners-testplane0001.acme.workers.dev".into(), served.url.clone());
    let home = common::signed_in(&served);
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    served.aws.replaying(&include_str!("../fixtures/runners/03-cf-agent.jsonl").replace("awsplane0001", superci_testkit::PLANE).replace("http://new-plane.test", &served.url), &[]);
    // (The layer of 36 MB not there, the rest there; its upload's address moving on as the registry says.)
    let reg = "registry.cloudflare.com/v2/1111111111111111111111111111111a/superci-runner";
    let layer = "sha256:577f3edc6a403caf5e1d36050abf05a22e73f81491c49e95aef70ba7df131075";
    let (at, at_more) = (format!("https://{reg}/blobs/uploads/u1"), format!("https://{reg}/blobs/uploads/u1?state=2"));
    served.aws.fail_with_headers(&format!("{reg}/manifests/gh-"), 404, &[], "", Some(1));
    served.aws.fail_with_headers("digest=sha256", 201, &[], "", None);
    served.aws.fail_with_headers(&format!("{reg}/blobs/uploads/u1"), 202, &[("location", &at_more)], "", None);
    served.aws.fail_with_headers(&format!("{reg}/blobs/uploads/"), 202, &[("location", &at)], "", None);
    served.aws.fail_with_headers(&format!("{reg}/blobs/{layer}"), 404, &[], "", None);
    served.aws.fail_with_headers(&format!("{reg}/blobs/sha256"), 200, &[], "", None);
    // (GitHub sends it from its storage: a server of its own here, so the registry is asked while it is read.)
    let storage = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let blob = format!("http://{}/blob", storage.local_addr().unwrap());
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        let Ok((mut s, _)) = storage.accept() else { return };
        let mut r = std::io::BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        while r.read_line(&mut line).is_ok_and(|n| n > 2) { line.clear() }
        let body = vec![b'x'; 33 * 1024 * 1024 + 1];
        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
        let _ = s.write_all(&body);
    });
    served.aws.fail_with_headers(&format!("actions-runner/blobs/{layer}"), 307, &[("location", &blob)], "", None);
    served.aws.fail_with_headers(&format!("{reg}/manifests/gh-"), 201, &[], "", None);
    let o = common::command(&home).args(["runners", "create", "cloudflare"]).env("SUPERCI_PAUSE_MS", "1").output().unwrap();
    served.aws.fail_none();
    let said = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success() && said.contains("Runner image ready") && said.contains("cloudflare runs jobs now."), "{said}");
    let chunks: Vec<String> = served.aws.asked.lock().unwrap().iter().filter(|a| a["url"].as_str().is_some_and(|u| u.ends_with("/blobs/uploads/u1") || u.ends_with("/blobs/uploads/u1?state=2"))).map(|a| a["url"].as_str().unwrap_or_default().to_string()).collect();
    assert_eq!(chunks, [at, at_more], "two chunks, the second where the first said");
    let _ = std::fs::remove_dir_all(&home);
}

/// Signed in to an AWS account with no control plane in it yet: every command that reads one or changes one of its
/// settings says there is none and how to set one up, and changes nothing.
#[test]
fn every_command_with_no_control_plane_yet_says_to_set_one_up() {
    let account = superci_testkit::Aws::serve();
    let home = common::aws_only(&account);
    let lines: [&[&str]; 39] = [&["jobs", "list"], &["jobs", "retrieve", "7"], &["runs", "list"], &["runs", "retrieve", "7"], &["runs", "cancel", "7"], &["runs", "wait", "7"], &["usage", "retrieve"],
            &["artifacts", "list", "--run=7"], &["caches", "retrieve"], &["caches", "update", "--days=3"], &["caches", "delete", "--repo=acme/app", "--confirm"], &["secrets", "list"],
            &["secrets", "delete", "NPM_TOKEN", "--confirm"], &["keys", "list"], &["keys", "create", "agent"], &["keys", "delete", "agent"], &["accounts", "update", "aws", "--monthly-usd=5"],
            &["deliveries", "list"], &["planes", "allow"], &["planes", "update"], &["runners", "create", "aws"], &["runners", "delete", "aws", "--confirm"], &["runners", "order", "aws"],
            &["runners", "update", "aws", "--max-jobs=3"], &["runners", "list"], &["labels", "list"], &["labels", "create", "gpu"], &["name", "retrieve"], &["name", "update", "Acme"],
            &["machine", "retrieve"], &["limits", "retrieve"], &["limits", "update", "--max-cpu=8"], &["public_repos", "list"], &["public_repos", "create", "acme/site"], &["connections", "list"],
            &["connections", "create", "gitlab"], &["projects", "list"], &["projects", "update", "7", "--enabled=true"], &["leave", "--confirm"]];
    std::thread::scope(|s| {
        let home = &home;
        let ran: Vec<_> = lines.iter().map(|args| s.spawn(move || (args, common::command(&home).env("SUPERCI_GITLAB_TOKEN", "glpat-x").env("NPM_VALUE", "x").args(*args).arg("--json").output().unwrap()))).collect();
        for r in ran {
            let (args, o) = r.join().unwrap();
            let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
            assert!(o.status.code() == Some(1) && v["error"].as_str().is_some_and(|e| e.starts_with("There is no control plane yet")), "{args:?}: {v}{}", String::from_utf8_lossy(&o.stderr));
        }
    });
    // With nothing to set up: none listed, and the dashboard's own words for it.
    let (code, planes) = json(&home, &["planes", "list"]);
    assert!(code == 0 && planes["control_planes"].as_array().is_some_and(|p| p.is_empty()), "{planes}");
    assert!(account.params.lock().unwrap().is_empty(), "nothing was made");
    let _ = std::fs::remove_dir_all(&home);
}

/// `superci planes allow`, signed in to the control plane's AWS account: its role given what this version asks for,
/// and said.
#[test]
fn a_control_planes_aws_role_is_given_what_this_version_asks_for() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // (IAM takes the policy, as it answers PutRolePolicy.)
    served.aws.fail("PutRolePolicy", 200, "<PutRolePolicyResponse xmlns=\"https://iam.amazonaws.com/doc/2010-05-08/\"><ResponseMetadata><RequestId>1</RequestId></ResponseMetadata></PutRolePolicyResponse>", None);
    let (code, v) = json(&home, &["planes", "allow"]);
    assert!(code == 0 && v["said"].as_str().unwrap_or_default().contains("Its AWS role has what this version asks for."), "{code} {v}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_image_of_ones_own_moves_to_a_new_tag_in_one_write_and_an_unknown_name_is_refused() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let ok = |args: &[&str]| { let (code, v) = json(&home, args); assert_eq!(code, 0, "{args:?}: {v}"); v };
    let images = || serde_json::from_str::<serde_json::Value>(&served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == "ROUTING").map(|(_, v)| v).unwrap_or("{}".into())).unwrap()["images"].clone();
    let writes = || served.aws.asked.lock().unwrap().len();

    ok(&["runner_images", "create", "rust-ci", "--reference=ghcr.io/acme/ci:1", "--modal-secret=ghcr-read"]);
    assert_eq!(images(), serde_json::json!({ "rust-ci": { "reference": "ghcr.io/acme/ci:1", "modal_secret": "ghcr-read" } }));
    // A new tag: one write of the routing, the Secret kept as it was.
    let before = writes();
    let said = ok(&["runner_images", "update", "rust-ci", "--reference=ghcr.io/acme/ci:2"]);
    assert_eq!(said["said"], "Jobs on Modal start from ghcr.io/acme/ci:2 (Modal Secret ghcr-read) when their label says image=rust-ci (runs-on: superci/image=rust-ci).");
    assert_eq!(writes() - before, 1, "{:?}", &served.aws.asked.lock().unwrap()[before..]);
    assert_eq!(images(), serde_json::json!({ "rust-ci": { "reference": "ghcr.io/acme/ci:2", "modal_secret": "ghcr-read" } }));
    // The Secret taken off (a public image now); the address kept.
    ok(&["runner_images", "update", "rust-ci", "--modal-secret="]);
    assert_eq!(images(), serde_json::json!({ "rust-ci": { "reference": "ghcr.io/acme/ci:2" } }));
    assert_eq!(ok(&["runner_images", "list"])["runner_images"], images());
    // With --dry-run, what it would do, and nothing done.
    let before = writes();
    let would = ok(&["runner_images", "update", "rust-ci", "--reference=ghcr.io/acme/ci:3", "--dry-run"]);
    assert_eq!((would["dry_run"].as_bool(), would["would"].as_str(), would["form"]["fields"]["action"].as_str(), would["form"]["fields"].get("modal_secret")),
        (Some(true), Some("let jobs on Modal whose label says image=rust-ci start from ghcr.io/acme/ci:3, in place of ghcr.io/acme/ci:2"), Some("image_update"), None), "{would}");
    assert_eq!((writes(), images()["rust-ci"]["reference"].as_str()), (before, Some("ghcr.io/acme/ci:2")));
    // A name the control plane does not have: refused, with or without --dry-run, and nothing written.
    for dry in [&[][..], &["--dry-run"]] {
        let (code, refused) = json(&home, &[&["runner_images", "update", "gone", "--reference=ghcr.io/acme/ci:3"][..], dry].concat());
        assert_eq!((code, refused["error"].as_str()), (1, Some("There is no image gone here (`superci runner_images list`). `superci runner_images create gone --reference=REGISTRY/IMAGE:TAG` adds it.")), "{refused}");
    }
    assert_eq!((writes(), images()), (before, serde_json::json!({ "rust-ci": { "reference": "ghcr.io/acme/ci:2" } })));
    assert!(served.aws.unanswered.lock().unwrap().is_empty(), "{:?}", served.aws.unanswered.lock().unwrap());
    let _ = std::fs::remove_dir_all(&home);
}
