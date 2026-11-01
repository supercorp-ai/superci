//! The dashboard's pages and forms with each of their calls and store operations failing in turn
//! (`superci_testkit::sweep_with_outages`: each failing once, and each failing from then on): the calls the dashboard makes itself (AWS's, GitHub's, through the stand-in) and the
//! control plane's. Whatever fails, the dashboard keeps answering (nothing in it panics), a page says what it could
//! not reach instead of breaking, a refused change says why, and the next look, with nothing failing, shows things as
//! they are.
mod common;
use std::sync::{Arc, Mutex};
use superci_testkit::{serve, sweep_with_outages, Fault, Outcome, Served};

/// A dashboard signed in to the control plane's AWS account, its GitHub connected, reading afresh on every request
/// (no views kept), and each line it wrote to its errors.
struct Board { home: std::path::PathBuf, _child: common::Dashboard, link: String, heard: Arc<Mutex<Vec<String>>> }

impl Drop for Board { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.home); } }

/// The control plane, set up as `setup` says before the dashboard starts, and the dashboard, its first look taken
/// (what it reads once, at the start, is not swept).
fn board(setup: impl Fn(&Served, &std::path::Path)) -> (Served, Board) {
    let served = serve(|_| {});
    served.connect_github();
    let home = common::signed_in(&served);
    setup(&served, &home);
    let (child, link, heard) = common::dashboard_heard(&home, &[("SUPERCI_VIEWS_S", "0"), ("SUPERCI_PAUSE_MS", "20")]);
    let b = Board { home, _child: child, link, heard };
    let _ = ask(&b, "GET", "/?p=overview&fragment=1", &[]);
    (served, b)
}

/// The dashboard's answer to a request as its own pages make it: status and body (0 and what failed, when it dropped
/// the connection).
fn ask(b: &Board, method: &str, path: &str, fields: &[(&str, &str)]) -> (u16, String) {
    let (base, key) = b.link.split_once("/?k=").unwrap();
    let port = base.rsplit(':').next().unwrap();
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(std::time::Duration::from_secs(60))).build().into();
    let cookie = format!("superci_local_{port}={key}");
    let answered = if method == "POST" {
        let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
        agent.post(&format!("{base}{path}")).header("cookie", &cookie).header("sec-fetch-site", "same-origin").header("content-type", "application/x-www-form-urlencoded").send(body)
    } else {
        agent.get(&format!("{base}{path}")).header("cookie", &cookie).header("sec-fetch-site", "same-origin").call()
    };
    match answered {
        Ok(mut r) => (r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default()),
        Err(e) => (0, e.to_string()),
    }
}

/// What must hold of the dashboard after a faulted request: it did not panic, and it still answers.
fn kept_answering(out: &Outcome, b: &Board) {
    let heard = b.heard.lock().unwrap().clone();
    out.check(!heard.iter().any(|l| l.contains("panicked")), || format!("the dashboard panicked: {}", heard.join("\n")));
    let (status, _) = ask(b, "GET", "/?p=overview&fragment=1", &[]);
    out.check(status == 200, || format!("the dashboard no longer answers a page ({status})"));
}

/// A page's live part opened with one call or store operation failing: answered, with words; then, nothing failing,
/// it shows `real` again.
fn page(fault: Fault, setup: impl Fn(&Served, &std::path::Path), path: &str, real: &str) -> Outcome {
    let (served, b) = board(setup);
    served.arm(fault);
    let (status, body) = ask(&b, "GET", path, &[]);
    // (An outage over: what follows looks at what it left.)
    served.faults().calm();
    let out = Outcome::new(served);
    out.check(status == 200 && !common::words(&body).is_empty(), || format!("{path} answered {status}: {}", common::words(&body)));
    kept_answering(&out, &b);
    let (_, again) = ask(&b, "GET", path, &[]);
    out.check(common::words(&again).contains(real), || format!("{path}, nothing failing, does not show {real:?}: {}", common::words(&again)));
    out
}

fn none(_: &Served, _: &std::path::Path) {}

/// Why a page that says one thing says it: beneath its heading, or the heading itself when it is not the one that
/// only says something did not work.
fn reason(body: &str) -> String {
    let note = body.split(r#"<p class="note">"#).nth(1).and_then(|r| r.split("</p>").next()).map(common::words).unwrap_or_default();
    let heading = body.split("<h1>").nth(1).and_then(|r| r.split("</h1>").next()).map(common::words).unwrap_or_default();
    if note.is_empty() && heading != "That did not work" { heading } else { note }
}

/// A form sent with one call or store operation failing: taken (sent on to a page, and `took` holds of the control
/// plane), or refused saying why; the dashboard answering after either way.
fn form(fault: Fault, setup: impl Fn(&Served, &std::path::Path), path: &str, fields: &[(&str, &str)], took: impl Fn(&Served) -> bool) -> Outcome {
    let fields: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    send(fault, setup, |_, _| {}, path, |_| fields.clone(), took)
}

/// The same, with `before` done in the dashboard once it is up (not swept), and the form's fields made from what the
/// control plane has then.
fn send(fault: Fault, setup: impl Fn(&Served, &std::path::Path), before: impl Fn(&Served, &Board), path: &str, fields: impl Fn(&Served) -> Vec<(String, String)>, took: impl Fn(&Served) -> bool) -> Outcome {
    let (served, b) = board(setup);
    before(&served, &b);
    let fields = fields(&served);
    let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    served.arm(fault);
    let (status, body) = ask(&b, "POST", path, &fields);
    // (An outage over: what follows looks at what it left.)
    served.faults().calm();
    let out = Outcome::new(served);
    let said = format!("{path} {fields:?} answered {status}: {}", common::words(&body));
    if fault == Fault::None && std::env::var_os("SUPERCI_SWEEP_SAY").is_some() { eprintln!("clean: {said}") }
    out.check(status != 0 && status < 500 || !reason(&body).is_empty(), || said.clone());
    out.check(!(400..600).contains(&status) || !reason(&body).is_empty(), || format!("refused without saying why: {said}"));
    out.check(!(300..400).contains(&status) || took(&out.served), || format!("taken, yet the control plane does not have it: {said}"));
    kept_answering(&out, &b);
    out
}

/// The control plane's setting `name`, as its cloud keeps it.
fn setting(served: &Served, name: &str) -> Option<String> { served.aws.settings(superci_testkit::PLANE).into_iter().find(|(k, _)| k == name).map(|(_, v)| v) }

#[test]
fn the_overview_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("the overview", |f| page(f, none, "/?p=overview&fragment=1", "Overview"));
}

#[test]
fn naming_the_ci_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("naming the CI", |f| form(f, none, "/name", &[("name", "Acme CI")], |s| setting(s, "NAME").as_deref() == Some("Acme CI")));
}

#[test]
fn a_label_added_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("a label added", |f| form(f, none, "/labels", &[("label", "big")], |s| setting(s, "LABELS").is_some_and(|l| l.contains("big"))));
}

#[test]
fn the_default_machine_set_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("the default machine", |f| form(f, none, "/machine", &[("cpu", "8")], |s| setting(s, "MACHINE").is_some_and(|m| m.contains('8'))));
}

#[test]
fn the_largest_machine_set_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("the largest machine", |f| form(f, none, "/routing", &[("action", "max_cpu"), ("max_cpu", "16")], |s| setting(s, "ROUTING").is_some_and(|r| r.contains("16"))));
}

/// A run from a computer played to its end before the dashboard starts: its run and job pages have something to show.
/// (Played once, on a control plane of its own; each run of a sweep is given its records, as playing it again for each
/// would take a second each time.)
fn with_run(served: &Served, _: &std::path::Path) {
    static PLAYED: std::sync::OnceLock<std::collections::BTreeMap<String, String>> = std::sync::OnceLock::new();
    let records = PLAYED.get_or_init(|| {
        let first = serve(|_| {});
        let dir = common::temp("played");
        common::played(&first, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let kept = first.store.0.lock().unwrap().clone();
        kept.into_iter().map(|(k, v)| (k, v.replace(&first.url, "http://played.test"))).collect()
    });
    let mut store = served.store.0.lock().unwrap();
    for (k, v) in records { store.entry(k.clone()).or_insert_with(|| v.replace("http://played.test", &served.url)); }
}

/// The id of the one run kept, and of its job.
fn run_and_job(served: &Served) -> (String, String) {
    let run = served.store.under("run:").into_iter().next().map(|(k, _)| k.trim_start_matches("run:").to_string()).unwrap_or_default();
    let job = served.store.under("job:").into_iter().next().map(|(k, _)| k.trim_start_matches("job:").to_string()).unwrap_or_default();
    (run, job)
}

macro_rules! pages {
    ($($name:ident: $what:literal, $setup:expr, $path:literal, $real:literal;)*) => {$(
        #[test]
        fn $name() { sweep_with_outages($what, |f| page(f, $setup, $path, $real)); }
    )*};
}

pages! {
    the_repositories_page_holds_with_each_call_and_store_operation_failing: "the repositories page", none, "/?p=repos&fragment=1", "Repositories";
    the_runners_page_holds_with_each_call_and_store_operation_failing: "the runners page", none, "/?p=runners&fragment=1", "Runners";
    the_add_runners_page_holds_with_each_call_and_store_operation_failing: "the add runners page", none, "/?p=add-runners&fragment=1", "Add a provider";
    the_workflows_page_holds_with_each_call_and_store_operation_failing: "the workflows page", none, "/?p=workflows&fragment=1", "Workflows";
    the_runs_page_holds_with_each_call_and_store_operation_failing: "the runs page", with_run, "/?p=runs&fragment=1", "Runs";
    the_jobs_page_holds_with_each_call_and_store_operation_failing: "the jobs page", with_run, "/?p=jobs&fragment=1", "Jobs";
    the_control_planes_page_holds_with_each_call_and_store_operation_failing: "the control planes page", none, "/?p=planes&fragment=1", "Control plane";
    the_settings_page_holds_with_each_call_and_store_operation_failing: "the settings page", none, "/?p=settings&fragment=1", "Settings";
    the_changelog_holds_with_each_call_and_store_operation_failing: "the changelog", none, "/?p=changes&fragment=1", "Changelog";
    the_overview_of_a_week_holds_with_each_call_and_store_operation_failing: "the overview of a week", with_run, "/?p=overview&usage=week&fragment=1", "Overview";
}

/// A page of one of the run's (`at` makes its address from the run's id and its job's) with one call or store
/// operation failing: answered with words, and, nothing failing, showing `real` again.
fn of_the_run(fault: Fault, at: impl Fn(&str, &str) -> String, real: &str) -> Outcome {
    let (served, b) = board(with_run);
    let (run, job) = run_and_job(&served);
    let path = at(&run, &job);
    served.arm(fault);
    let (status, body) = ask(&b, "GET", &path, &[]);
    // (An outage over: what follows looks at what it left.)
    served.faults().calm();
    let out = Outcome::new(served);
    out.check(status != 0 && (status < 400 || !body.trim().is_empty()), || format!("{path} answered {status}: {}", common::words(&body)));
    kept_answering(&out, &b);
    let (_, again) = ask(&b, "GET", &path, &[]);
    out.check(again.contains(real), || format!("{path}, nothing failing, does not show {real:?}: {}", common::words(&again)));
    out
}

#[test]
fn a_runs_page_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("a run's page", |f| of_the_run(f, |run, _| format!("/?p=run&repo=acme/app&id={run}&attempt=1&fragment=1"), "Run"));
}

#[test]
fn a_jobs_page_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("a job's page", |f| of_the_run(f, |_, job| format!("/?p=job&id={job}&fragment=1"), "Job"));
}

#[test]
fn a_jobs_log_holds_with_each_call_and_store_operation_failing() {
    sweep_with_outages("a job's log", |f| of_the_run(f, |_, job| format!("/job/raw?id={job}"), "test 12 passed"));
}

macro_rules! forms {
    ($($name:ident: $what:literal, $setup:expr, $path:literal, $fields:expr, $took:expr;)*) => {$(
        #[test]
        fn $name() { sweep_with_outages($what, |f| form(f, $setup, $path, $fields, $took)); }
    )*};
}

fn anything(_: &Served) -> bool { true }

forms! {
    the_longest_job_set_holds_with_each_call_and_store_operation_failing: "the longest job", none, "/routing", &[("action", "max_hours"), ("max_hours", "3")], |s| setting(s, "ROUTING").is_some_and(|r| r.contains("180"));
    a_monthly_limit_set_holds_with_each_call_and_store_operation_failing: "a monthly limit", none, "/routing", &[("action", "monthly_usd"), ("usd", "5")], |s| setting(s, "ROUTING").is_some_and(|r| r.contains("monthly_usd"));
    a_provider_taken_out_holds_with_each_call_and_store_operation_failing: "a provider taken out", none, "/routing", &[("action", "take_out"), ("cloud", "aws"), ("current", "aws,aws-on-demand,cloudflare")], anything;
    a_public_repository_allowed_holds_with_each_call_and_store_operation_failing: "a public repository", none, "/routing", &[("action", "public_add"), ("repo", "acme/site")], anything;
    a_secret_added_holds_with_each_call_and_store_operation_failing: "a secret added", none, "/secrets/add", &[("name", "NPM_TOKEN"), ("value", "abc")], anything;
    a_device_key_taken_out_holds_with_each_call_and_store_operation_failing: "a device key taken out", none, "/keys/remove", &[("name", "DEVICE_KEY_4102444800_OTHER")], anything;
    a_read_key_taken_out_holds_with_each_call_and_store_operation_failing: "a read key taken out", none, "/keys/remove", &[("read", "agent")], anything;
    the_aws_permissions_given_hold_with_each_call_and_store_operation_failing: "the AWS permissions", deployed, "/permissions/aws", &[("plane", superci_testkit::PLANE)], anything;
    a_look_at_aws_holds_with_each_call_and_store_operation_failing: "a look at AWS", none, "/aws/look", &[("next", "?p=planes")], anything;
    a_cache_deleted_holds_with_each_call_and_store_operation_failing: "a cache deleted", none, "/caches/delete", &[("repo", "acme/app"), ("key", "node-modules"), ("scope", "refs/heads/main")], anything;
    an_app_removed_holds_with_each_call_and_store_operation_failing: "an App removed", second_app, "/github/remove", &[("app", "2002")], |s| setting(s, "GITHUB_APP_2002").is_none();
    aws_runners_taken_out_hold_with_each_call_and_store_operation_failing: "AWS runners taken out", none, "/runners/remove", &[("cloud", "aws")], anything;
    the_image_copied_holds_with_each_call_and_store_operation_failing: "the image copied", |s: &Served, _: &std::path::Path| s.aws.replaying(include_str!("../fixtures/aws/images/01-create.jsonl"), &[]), "/aws/image", &[("account", "111111111111"), ("region", "ca-central-1"), ("next", "?p=planes")], anything;
    gitlab_disconnected_holds_with_each_call_and_store_operation_failing: "GitLab disconnected", |s: &Served, _: &std::path::Path| s.connect_gitlab(), "/gitlab/disconnect", &[("gitlab", "")], |s| setting(s, "GITLAB").is_none_or(|g| g == "off");
}

/// globex's App beside acme's.
fn second_app(served: &Served, _: &std::path::Path) {
    let mut second = superci_testkit::github::GitHub::app();
    second["id"] = 2002.into(); second["owner"] = "globex".into(); second["slug"] = "superci-globex".into();
    served.aws.put(&format!("/superci/{}/GITHUB_APP_2002", superci_testkit::PLANE), &second.to_string());
}

/// AWS answering as it answered a control plane's deploy (its runner role among it).
fn deployed(served: &Served, _: &std::path::Path) {
    served.aws.replaying(include_str!("../fixtures/aws/planes/deploy.jsonl"), &[("testplane0002", superci_testkit::PLANE), ("http://new-plane.test", &served.url)]);
}

#[test]
fn a_secret_taken_out_holds_with_each_call_and_store_operation_failing() {
    let add = |_: &Served, b: &Board| { ask(b, "POST", "/secrets/add", &[("name", "NPM_TOKEN"), ("value", "abc")]); };
    sweep_with_outages("a secret taken out", |f| send(f, none, add, "/secrets/remove", |_| vec![("name".into(), "NPM_TOKEN".into())], anything));
}

#[test]
fn a_github_run_cancelled_holds_with_each_call_and_store_operation_failing() {
    // A run of GitHub's under way.
    let running = |s: &Served, _: &std::path::Path| {
        let (id, queued) = s.github.queue("test", "superci", s.now_ms());
        s.webhook(queued);
        s.webhook(s.github.start(id));
    };
    let run = |s: &Served| s.github.job(s.store.under("job:").into_iter().next().map(|(k, _)| k.trim_start_matches("job:").parse::<u64>().unwrap_or_default()).unwrap_or_default())["run_id"].as_u64().unwrap_or_default().to_string();
    sweep_with_outages("a GitHub run cancelled", |f| send(f, running, |_, _| {}, "/run/cancel", |s| vec![("id".into(), run(s)), ("repo".into(), "acme/app".into()), ("attempt".into(), "1".into())], anything));
}
