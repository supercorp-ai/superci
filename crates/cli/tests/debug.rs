//! `superci debug …` (a debug build's own commands, to see and record what real accounts answer), each run against
//! the test kit's stand-ins: AWS answering as it was recorded, a runner played as GitHub's plays one.
#![cfg(debug_assertions)]
mod common;
use std::process::{Command, Stdio};
use superci_testkit::{serve, Runner};

fn answer(home: &std::path::Path, args: &[&str]) -> String {
    let (code, v) = common::json(home, &[&["debug"], args].concat());
    assert_eq!(code, 0, "{args:?}: {v}");
    v["answer"].as_str().unwrap_or_default().to_string()
}

#[test]
fn aws_is_asked_as_this_workspace_signs_in_by_query_json_and_rest() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    // Query APIs (EC2 by region, IAM global) and JSON ones (by target, a version, or a path), as recorded.
    assert!(answer(&home, &["ec2", "eu-north-1", "DescribeVpcs", "MaxResults=5"]).contains("DescribeVpcsResponse"));
    assert!(answer(&home, &["iam", "ListRoles", "MaxItems=1000"]).contains("ListRolesResponse"));
    let params = answer(&home, &["json", "us-east-1", "ssm", "AmazonSSM.GetParametersByPath", &format!(r#"{{"Path":"/superci/{}/"}}"#, superci_testkit::PLANE)]);
    assert!(params.contains("Parameters"), "{params}");
    assert!(!answer(&home, &["json", "us-east-1", "dynamodb", "DynamoDB_20120810.Query@1.0", r#"{"TableName":"nope","KeyConditionExpression":"p = :p","ExpressionAttributeValues":{":p":{"S":"job"}}}"#]).is_empty());
    assert!(answer(&home, &["json", "us-east-1", "account.us-east-1.amazonaws.com", "/listRegions", "{}"]).starts_with("refused or failed"));
    let (code, _) = common::json(&home, &["debug", "json", "us-east-1", "ssm", "AmazonSSM.GetParameter", "not json"]);
    assert_eq!(code, 1);
    // REST (S3, a body from a file, a header signed with the rest).
    let body = common::temp("debug-body").join("body");
    std::fs::write(&body, b"hello").unwrap();
    let said = answer(&home, &["rest", "us-east-1", "s3", "PUT", "https://bucket-for-tests.s3.us-east-1.amazonaws.com/a.txt", body.to_str().unwrap(), "--header=X-Amz-Meta-Who:tests"]);
    assert!(said.split(' ').next().is_some_and(|s| s.parse::<u16>().is_ok()), "{said}");
    let (code, _) = common::json(&home, &["debug", "rest", "us-east-1", "s3", "PUT", "https://bucket-for-tests.s3.us-east-1.amazonaws.com/a.txt", "/no/such/file"]);
    assert_eq!(code, 1);
    // Links an hour long, for trying things on a machine with no sign-in.
    let o = common::superci(&home, &["debug", "presign", "GET", "us-east-1", "bucket-for-tests", "/image/", "2"]);
    let links: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stdout)));
    assert!(links["p/000001"].as_str().is_some_and(|l| l.contains("/image/p/000001?X-Amz-")) && links["index.json"].is_string() && links["p/000002"].is_null(), "{links}");
    assert_eq!(common::superci(&home, &["debug", "presign", "GET", "us-east-1", "b", "p", "many"]).status.code(), Some(1));
    // What a stopped deploy left, removed (nothing here); the account's budget, each step (none answered here).
    let left = String::from_utf8_lossy(&common::superci(&home, &["debug", "abandon", "us-east-1", "gonegone0001"]).stdout).into_owned();
    assert!(left.starts_with("nothing left") || left.starts_with("left: "), "{left}");
    for step in [&["budget"][..], &["budget", "set", "50", "ops@acme.example", "stop"], &["budget", "lift", "again"], &["budget", "remove"]] {
        assert!(!answer(&home, step).is_empty(), "{step:?}");
    }
    assert_eq!(common::json(&home, &["debug", "budget", "set", "fifty"]).0, 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn one_machine_is_started_and_ended_as_the_control_plane_starts_a_jobs() {
    let served = serve(|_| {});
    served.aws.replaying(include_str!("../fixtures/aws/launch/spot.jsonl"), &[]);
    let home = common::signed_in(&served);
    // Whatever AWS answers each step is said (here what the recording has of it).
    for args in [&["launch", "us-east-1"][..], &["launch", "us-east-1", "on-demand", "c8a.large,c7a.large", "--disk=40", "--extra-disk=20:500"]] {
        let said = answer(&home, args);
        assert!(said.starts_with("started ") || said.starts_with("not started: "), "{args:?}: {said}");
    }
    let script = common::temp("debug-user-data").join("user-data.sh");
    std::fs::write(&script, "#!/bin/bash\npoweroff\n").unwrap();
    let said = answer(&home, &["launch", "us-east-1", "spot", "--image=runs-on-v2.2-ubuntu24-full-x64-*", &format!("--user-data={}", script.display())]);
    assert!(said.contains("left to its script") || said.starts_with("not started: "), "{said}");
    assert_eq!(common::json(&home, &["debug", "launch", "us-east-1", "spot", "--user-data=/no/such/file"]).0, 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn cloudflares_api_is_called_with_this_workspaces_sign_in() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let (code, v) = common::json(&home, &["debug", "cloudflare", "GET", "/accounts"]);
    assert!(code == 3 && v["error"].as_str().unwrap_or_default().contains("Sign in with Cloudflare first"), "{v}");
    let file = home.join("workspaces/default/sign-ins.json");
    let mut kept: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept["cloudflare"] = serde_json::json!({ "access": "cloudflare-access-for-tests", "expires_at_ms": 4102444800000u64, "refresh": "cloudflare-refresh-for-tests" });
    std::fs::write(&file, kept.to_string()).unwrap();
    // (What Cloudflare was not recorded answering is said as such.)
    let body = common::temp("debug-cf-body").join("body.json");
    std::fs::write(&body, "{}").unwrap();
    for args in [&["debug", "cloudflare", "GET", "/accounts"][..], &["debug", "cloudflare", "POST", "/accounts/x/workers", body.to_str().unwrap()]] {
        let o = common::superci(&home, args);
        assert!(o.status.code().is_some_and(|c| c <= 1) && !String::from_utf8_lossy(&o.stdout).contains("Sign in with Cloudflare first"), "{args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_job_is_served_to_a_runner_here_as_the_control_plane_serves_it() {
    let dir = common::temp("debug-runner-service");
    std::fs::create_dir_all(&dir).unwrap();
    let workflow = dir.join("ci.yml");
    std::fs::write(&workflow, "jobs:\n  tests:\n    runs-on: superci\n    steps:\n      - run: echo ${{ secrets.TOKEN }} ${{ vars.WHERE }}\n").unwrap();
    let port = superci_testkit::free_port();
    let root = format!("http://127.0.0.1:{port}/runner/rig");
    let jit = dir.join("jit");
    let home = common::temp("debug-runner-home");
    let child = Command::new(env!("CARGO_BIN_EXE_superci")).env("SUPERCI_HOME", &home)
        .args(["debug", "runner-service", workflow.to_str().unwrap(), "tests", &root, &port.to_string(), jit.to_str().unwrap(), "--secret=TOKEN=s3cret", "--var=WHERE=here", "--json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() { assert!(std::time::Instant::now() < until, "it did not listen"); std::thread::sleep(std::time::Duration::from_millis(50)) }
    assert!(std::fs::read_to_string(&jit).unwrap().len() > 20, "the runner's configuration written");
    // A runner takes the job, runs its step and ends it.
    let runner = Runner { at: format!("{root}/_apis/v1") };
    runner.take();
    let s = "dddddddd-0000-0000-0000-000000000004";
    runner.steps(&[(s, "Run echo", 1, "inProgress", None, None)]);
    runner.lines(s, 1, &["s3cret here".to_string()]);
    runner.steps(&[(s, "Run echo", 1, "completed", Some("succeeded"), Some(1))]);
    runner.finish("succeeded");
    // (Asked once more, the service sees the job ended and says so.)
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    let out = child.wait_with_output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)));
    let said = v["answer"].as_str().unwrap_or_default();
    assert!(said.starts_with("the job ended: succeeded") && said.contains("Run echo succeeded"), "{v}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

/// `SUPERCI_RECORD=FILE`: what AWS answered each question, a line each, for the test kit (no header, so no credential).
#[test]
fn what_aws_answers_is_recorded_a_line_each_without_headers() {
    let served = serve(|_| {});
    let home = common::signed_in(&served);
    let file = common::temp("debug-record").join("recording.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let o = common::command(&home).env("SUPERCI_RECORD", &file).args(["debug", "iam", "ListRoles", "MaxItems=1000", "--json"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = std::fs::read_to_string(&file).unwrap();
    let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert!(line["url"].as_str().unwrap().contains("iam.amazonaws.com") && line["asked"].as_str().unwrap().contains("Action=ListRoles") && line["status"] == 200 && line["answer"].as_str().unwrap().contains("ListRolesResponse"), "{line}");
    assert!(!text.contains("ASIAEXAMPLE") && !text.to_lowercase().contains("authorization"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}
