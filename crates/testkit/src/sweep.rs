//! Faults, one at a time, as SQLite tests itself: a scenario run once clean, counting every call the stand-ins answer
//! and every operation of the control plane's store, then again for each of those, that one failing (a call answered
//! with its service's own error, or its connection dropped; a store operation not answered). Each faulted run is
//! held to what SuperCI must do whatever fails: nothing panics, every answer of the control plane's that is an error
//! says what failed, and the state it leaves is one the scenario's own checks take (a job that runs, waits saying
//! why, or failed saying why).
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::Served;

/// Which fault a run of a scenario is given, armed once the scenario's setup is done (`Served::arm`). Counted from
/// there, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// None: the clean run that counts.
    None,
    /// The k-th call answered with its service's error (AWS's InternalError, a 500 of GitHub's, …).
    Call(usize),
    /// The k-th call's connection dropped before any answer.
    Dropped(usize),
    /// The k-th store operation not answered.
    Store(usize),
    /// The k-th call and every one after it answered with its service's error, until the scenario says the outage is
    /// over (`Faults::calm`): what a client that tries a call again does not get past.
    Outage(usize),
    /// The k-th store operation and every one after it not answered, until then.
    StoreDown(usize),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::None => write!(f, "no fault"),
            Fault::Call(k) => write!(f, "call {k} failing"),
            Fault::Dropped(k) => write!(f, "call {k} dropped"),
            Fault::Store(k) => write!(f, "store operation {k} failing"),
            Fault::Outage(k) => write!(f, "calls failing from call {k} on"),
            Fault::StoreDown(k) => write!(f, "the store failing from operation {k} on"),
        }
    }
}

/// What the stand-ins and the store counted, the fault armed, and what the control plane answered and where it
/// panicked: shared by a `Served`'s store, machines and stand-ins.
#[derive(Clone, Default)]
pub struct Faults(Arc<Mutex<Faulting>>);

#[derive(Default)]
struct Faulting {
    /// Each call answered and each store operation, in their order (what was asked, as one line).
    calls: Vec<String>,
    ops: Vec<String>,
    /// Where counting started (`arm`): the calls, operations and answers before it.
    marks: (usize, usize, usize),
    /// Where it stopped (`stop`), when it did: the calls and operations before it.
    ends: Option<(usize, usize)>,
    /// The call (its place among `calls`) to fail, and whether by dropping it; the operation to fail.
    call: Option<(usize, bool)>,
    op: Option<usize>,
    /// The first call, and the first store operation, of an outage: each from it on fails (see `Fault::Outage`).
    outage: Option<usize>,
    down: Option<usize>,
    /// What was failed.
    fired: Vec<String>,
    /// The control plane's panics: what it was answering, and what the panic said.
    panics: Vec<String>,
    /// The request the control plane is answering now, and each answer it gave: request, status, body.
    asking: String,
    answers: Vec<(String, u16, String)>,
}

impl Faults {
    /// Counting starts now, with `fault` armed.
    pub fn arm(&self, fault: Fault) {
        let mut f = self.0.lock().unwrap();
        f.marks = (f.calls.len(), f.ops.len(), f.answers.len());
        f.ends = None;
        let (calls, ops) = (f.calls.len(), f.ops.len());
        (f.call, f.op, f.outage, f.down) = match fault {
            Fault::None => (None, None, None, None),
            Fault::Call(k) => (Some((calls + k - 1, false)), None, None, None),
            Fault::Dropped(k) => (Some((calls + k - 1, true)), None, None, None),
            Fault::Store(k) => (None, Some(ops + k - 1), None, None),
            Fault::Outage(k) => (None, None, Some(calls + k - 1), None),
            Fault::StoreDown(k) => (None, None, None, Some(ops + k - 1)),
        };
    }

    /// Nothing fails any more (an outage over), counting going on as before.
    pub fn calm(&self) { let mut f = self.0.lock().unwrap(); (f.call, f.op, f.outage, f.down) = (None, None, None, None) }

    /// Counting stops now, and nothing more fails: what a scenario does after this (a check that the state it left
    /// still works) is neither counted nor faulted.
    pub fn stop(&self) {
        let mut f = self.0.lock().unwrap();
        f.ends = Some((f.calls.len(), f.ops.len()));
        (f.call, f.op, f.outage, f.down) = (None, None, None, None);
    }

    /// The calls answered since counting started (`arm`), each as one line (`POST https://ec2…/ RunInstances`).
    pub fn calls(&self) -> Vec<String> { let f = self.0.lock().unwrap(); f.calls[f.marks.0..f.ends.map_or(f.calls.len(), |e| e.0)].to_vec() }
    /// The store's operations since then (`put job:90000000001`).
    pub fn store_ops(&self) -> Vec<String> { let f = self.0.lock().unwrap(); f.ops[f.marks.1..f.ends.map_or(f.ops.len(), |e| e.1)].to_vec() }
    /// What was failed.
    pub fn fired(&self) -> Vec<String> { self.0.lock().unwrap().fired.clone() }
    /// Where the control plane panicked.
    pub fn panics(&self) -> Vec<String> { self.0.lock().unwrap().panics.clone() }
    /// The control plane's answers since counting started: request, status, body.
    pub fn answers(&self) -> Vec<(String, u16, String)> { let f = self.0.lock().unwrap(); f.answers[f.marks.2..].to_vec() }

    /// A call counted: whether it is the one to fail (`Some(true)`: dropped).
    pub(crate) fn call(&self, what: String) -> Option<bool> {
        let mut f = self.0.lock().unwrap();
        let at = f.calls.len();
        f.calls.push(what.clone());
        let hit = f.call.filter(|(k, _)| *k == at).map(|(_, dropped)| dropped).or(f.outage.filter(|k| at >= *k).map(|_| false));
        if let Some(dropped) = hit { f.fired.push(format!("{} {what}", if dropped { "dropped" } else { "failed" })) }
        hit
    }

    /// A store operation counted: its refusal, when it is the one to fail.
    pub(crate) fn op(&self, what: String) -> superci_core::io::Result<()> {
        let mut f = self.0.lock().unwrap();
        let at = f.ops.len();
        f.ops.push(what.clone());
        if f.op != Some(at) && !f.down.is_some_and(|k| at >= k) { return Ok(()) }
        f.fired.push(format!("failed {what}"));
        Err(format!("the store did not answer ({what})"))
    }

    pub(crate) fn asking(&self, request: String) { self.0.lock().unwrap().asking = request }
    pub(crate) fn answered(&self, status: u16, body: &[u8]) {
        let mut f = self.0.lock().unwrap();
        let asking = f.asking.clone();
        f.answers.push((asking, status, String::from_utf8_lossy(body).chars().take(400).collect()));
    }
    pub(crate) fn panicked(&self, panic: Box<dyn std::any::Any + Send>) {
        let mut f = self.0.lock().unwrap();
        let asking = f.asking.clone();
        f.panics.push(format!("answering {asking}: {}", panic_said(&*panic)));
    }
}

/// What a panic said.
fn panic_said(panic: &(dyn std::any::Any + Send)) -> String {
    panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
}

/// A failed call's answer, in its service's own words: AWS's (its JSON APIs', its query APIs', S3's), GitHub's,
/// GitLab's, Cloudflare's, Modal's.
pub(crate) fn failed_answer(url: &str, target: &str) -> (u16, Vec<(String, String)>, String) {
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let (status, kind, body) = if host.contains(".s3.") {
        (500, "application/xml", "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>InternalError</Code><Message>We encountered an internal error. Please try again.</Message></Error>".to_string())
    } else if host.ends_with(".amazonaws.com") && !target.is_empty() {
        (500, "application/x-amz-json-1.1", crate::json_error("InternalServerError", "An internal error occurred. Try again."))
    } else if host.starts_with("lambda.") {
        (500, "application/json", json!({ "Type": "Service", "message": "An internal error occurred. Try again." }).to_string())
    } else if host.ends_with(".amazonaws.com") || host.ends_with(".amazon.com") {
        (500, "text/xml", crate::ec2_error("InternalError", "An internal error has occurred"))
    } else if host.contains("gitlab") {
        (500, "application/json", json!({ "message": "500 Internal Server Error" }).to_string())
    } else if host.ends_with("cloudflare.com") || host.ends_with(".workers.dev") {
        (500, "application/json", json!({ "success": false, "errors": [{ "code": 10000, "message": "Internal server error" }], "messages": [], "result": null }).to_string())
    } else if host.ends_with(".modal.run") || host.contains("modal.com") {
        (500, "application/json", json!({ "detail": "Internal Server Error" }).to_string())
    } else {
        (500, "application/json", json!({ "message": "Server Error" }).to_string())
    };
    (status, vec![("content-type".into(), kind.into())], body)
}

/// One run of a scenario: its control plane, and what the scenario's own checks found wrong (none: all held).
pub struct Outcome { pub served: Served, problems: std::cell::RefCell<Vec<String>> }

impl Outcome {
    pub fn new(served: Served) -> Outcome { Outcome { served, problems: Default::default() } }
    /// A check of the state the run left: `what` said when it does not hold.
    pub fn check(&self, holds: bool, what: impl FnOnce() -> String) { if !holds { self.problems.borrow_mut().push(what()) } }
    /// What did not hold.
    pub fn problems(&self) -> Vec<String> { self.problems.borrow().clone() }
}

/// How many faulted runs a sweep did, and how long it took.
#[derive(Debug)]
pub struct Swept { pub calls: usize, pub store_ops: usize, pub runs: usize, pub ms: u128 }

/// `scenario` run clean, then once for each call and each store operation it made, that one failing (a call twice:
/// answered with an error, and dropped). It builds its own control plane each time and arms the fault it is given
/// once set up (`Served::arm`). Panics, naming each fault and what it broke, when any run broke what must hold.
/// (The clean run alone unless `SUPERCI_SWEEP_FAULTS=1`: a sweep is many runs, made where they are asked for (the Test
/// workflow's sweeps step, coverage); everywhere else the scenario is one test among the others.)
pub fn sweep(name: &str, scenario: impl Fn(Fault) -> Outcome + Sync) -> Swept { swept(name, false, scenario) }

/// The same, with outages too: run again for each call, that call and every one after it failing, and for each store
/// operation, the store failing from it on (`Fault::Outage`, `Fault::StoreDown`); the scenario ends the outage
/// (`Faults::calm`) where it looks at what is left.
pub fn sweep_with_outages(name: &str, scenario: impl Fn(Fault) -> Outcome + Sync) -> Swept { swept(name, true, scenario) }

/// Which slice of a sweep's faults this run makes (`SUPERCI_SWEEP_SLICE=k/n`, k from 1): all of them unless said.
pub fn slice() -> (usize, usize) {
    slice_of(&std::env::var("SUPERCI_SWEEP_SLICE").unwrap_or_default())
}

fn slice_of(said: &str) -> (usize, usize) {
    let (k, n) = said.split_once('/').and_then(|(k, n)| Some((k.parse::<usize>().ok()?, n.parse::<usize>().ok()?))).unwrap_or((1, 1));
    if n == 0 || k == 0 || k > n { (1, 1) } else { (k, n) }
}

/// What a sweep does besides its clean run, as `said` (the environment) has it: its faulted runs (`SUPERCI_SWEEP_FAULTS=1`),
/// this run's slice of them, one alone (`SUPERCI_SWEEP_ONLY`), and how many at once.
struct Plan { faults: bool, slice: (usize, usize), only: Option<String>, threads: usize }

impl Plan {
    fn said(said: impl Fn(&str) -> Option<String>) -> Plan {
        // (Fewer with `SUPERCI_SWEEP_THREADS=N`: on macOS, whose closed connections hold their ports half a minute and are
        // not reused on loopback as Linux reuses them, a sweep of the CLI's cloud flows under a coverage tool runs out.)
        Plan { faults: said("SUPERCI_SWEEP_FAULTS").as_deref() == Some("1"), slice: slice_of(&said("SUPERCI_SWEEP_SLICE").unwrap_or_default()), only: said("SUPERCI_SWEEP_ONLY"),
            threads: said("SUPERCI_SWEEP_THREADS").and_then(|n| n.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)).max(1) }
    }
}

/// What a fault was to fail, as the clean run counted it.
fn planned(fault: Fault, calls: &[String], ops: &[String]) -> String {
    match fault { Fault::Call(k) | Fault::Dropped(k) | Fault::Outage(k) => calls.get(k - 1).cloned(), Fault::Store(k) | Fault::StoreDown(k) => ops.get(k - 1).cloned(), Fault::None => None }.unwrap_or_default()
}

fn swept(name: &str, outages: bool, scenario: impl Fn(Fault) -> Outcome + Sync) -> Swept { swept_as(name, outages, Plan::said(|name| std::env::var(name).ok()), scenario) }

fn swept_as(name: &str, outages: bool, plan: Plan, scenario: impl Fn(Fault) -> Outcome + Sync) -> Swept {
    let started = std::time::Instant::now();
    let clean = run(&scenario, Fault::None);
    let (calls, ops) = match clean {
        Ok((ref o, ref problems)) if problems.is_empty() => (o.served.faults().calls(), o.served.faults().store_ops()),
        Ok((_, problems)) => panic!("{name}, clean: {}", problems.join("\n  ")),
        Err(e) => panic!("{name}, clean: {e}"),
    };
    let mut faults: Vec<Fault> = vec![];
    if plan.faults {
        faults.extend((1..=calls.len()).flat_map(|k| [Fault::Call(k), Fault::Dropped(k)]));
        faults.extend((1..=ops.len()).map(Fault::Store));
        if outages {
            faults.extend((1..=calls.len()).map(Fault::Outage));
            faults.extend((1..=ops.len()).map(Fault::StoreDown));
        }
    }
    // (A slice of them, `SUPERCI_SWEEP_SLICE=k/n`: every n-th from the k-th, so n machines share every sweep evenly.)
    let (k, n) = plan.slice;
    let mut faults: Vec<Fault> = faults.into_iter().enumerate().filter(|(i, _)| i % n == k - 1).map(|(_, f)| f).collect();
    // (One of them alone, said in full, to look into it: `SUPERCI_SWEEP_ONLY=store:109`, `call:41`, `dropped:41`,
    // `outage:41`, `down:109`.)
    let only = plan.only;
    if let Some((kind, k)) = only.as_deref().and_then(|o| o.split_once(':')) {
        let k: usize = k.parse().unwrap_or(1).max(1);
        faults = vec![match kind { "call" => Fault::Call(k), "dropped" => Fault::Dropped(k), "outage" => Fault::Outage(k), "down" => Fault::StoreDown(k), _ => Fault::Store(k) }];
    }
    // (Each run on a control plane of its own: side by side.)
    let next = Mutex::new(0usize);
    let found: Mutex<Vec<String>> = Mutex::default();
    std::thread::scope(|s| (0..plan.threads).for_each(|_| {
        s.spawn(|| loop {
            let i = { let mut n = next.lock().unwrap(); *n += 1; *n - 1 };
            let Some(fault) = faults.get(i).copied() else { break };
            let planned = planned(fault, &calls, &ops);
            let problems = match run(&scenario, fault) {
                Ok((o, p)) => {
                    let f = o.served.faults();
                    if only.is_some() { eprintln!("{fault}: {:?}\ncalls: {:#?}\nanswers: {:#?}", f.fired(), f.calls(), f.answers()) }
                    p
                }
                Err(e) => vec![e],
            };
            if !problems.is_empty() { found.lock().unwrap().push(format!("{fault} ({planned}):\n    {}", problems.join("\n    "))) }
        });
    }));
    let swept = Swept { calls: calls.len(), store_ops: ops.len(), runs: faults.len(), ms: started.elapsed().as_millis() };
    eprintln!("{name}: {} calls, {} store operations, {} faulted runs in {} ms", swept.calls, swept.store_ops, swept.runs, swept.ms);
    let mut found = found.into_inner().unwrap();
    found.sort();
    assert!(found.is_empty(), "{name}: {} of {} faulted runs broke what must hold:\n  {}", found.len(), swept.runs, found.join("\n  "));
    swept
}

/// One run, held to what must hold whatever fails; what did not.
fn run(scenario: &(impl Fn(Fault) -> Outcome + Sync), fault: Fault) -> Result<(Outcome, Vec<String>), String> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scenario(fault))).map_err(|p| format!("the scenario panicked: {}", panic_said(&*p)))?;
    let mut problems = outcome.problems();
    let faults = outcome.served.faults();
    problems.extend(faults.panics().into_iter().map(|p| format!("the control plane panicked {p}")));
    problems.extend(faults.answers().into_iter().filter(|(_, status, body)| *status >= 500 && body.trim().is_empty()).map(|(asked, status, _)| format!("{asked} was answered {status} saying nothing")));
    Ok((outcome, problems))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn plan(only: Option<&str>) -> Plan { Plan { faults: true, slice: (1, 1), only: only.map(str::to_string), threads: 2 } }

    /// A scenario of one call and one store operation, each as it was asked; holding unless `check` says otherwise.
    fn small(fault: Fault, check: impl Fn(Fault) -> bool) -> Outcome {
        let o = Outcome::new(serve(|_| {}));
        o.served.arm(fault);
        let _ = o.served.faults().call("a call".into());
        let _ = o.served.faults().op("an operation".into());
        o.check(check(fault), || format!("{fault} broke it"));
        o
    }

    #[test]
    fn each_fault_says_what_it_is_and_what_it_was_to_fail() {
        let (calls, ops) = (vec!["c".to_string()], vec!["o".to_string()]);
        for (fault, said, to_fail) in [(Fault::None, "no fault", ""), (Fault::Call(1), "call 1 failing", "c"), (Fault::Dropped(1), "call 1 dropped", "c"), (Fault::Store(1), "store operation 1 failing", "o"),
            (Fault::Outage(1), "calls failing from call 1 on", "c"), (Fault::StoreDown(1), "the store failing from operation 1 on", "o"), (Fault::Call(2), "call 2 failing", "")] {
            assert_eq!(fault.to_string(), said);
            assert_eq!(planned(fault, &calls, &ops), to_fail);
        }
    }

    #[test]
    fn a_sweep_makes_what_it_is_told_to() {
        assert_eq!(slice_of("2/3"), (2, 3));
        for said in ["", "x/3", "2/x", "0/3", "2/0", "4/3"] { assert_eq!(slice_of(said), (1, 1), "{said}") }
        let p = Plan::said(|name| match name { "SUPERCI_SWEEP_FAULTS" => Some("1".into()), "SUPERCI_SWEEP_SLICE" => Some("2/3".into()), "SUPERCI_SWEEP_ONLY" => Some("store:1".into()), "SUPERCI_SWEEP_THREADS" => Some("0".into()), _ => None });
        assert!(p.faults && p.slice == (2, 3) && p.only.as_deref() == Some("store:1") && p.threads == 1);
        let p = Plan::said(|_| None);
        assert!(!p.faults && p.slice == (1, 1) && p.only.is_none() && (1..=8).contains(&p.threads));
        assert!(!Plan::said(|name| (name == "SUPERCI_SWEEP_FAULTS").then(|| "0".to_string())).faults);
    }

    #[test]
    fn every_fault_is_run_and_one_alone_when_said() {
        let holds = |_: Fault| true;
        let swept = swept_as("all", true, plan(None), |f| small(f, holds));
        assert_eq!((swept.calls, swept.store_ops, swept.runs), (1, 1, 5));
        assert_eq!(swept_as("without outages", false, plan(None), |f| small(f, holds)).runs, 3);
        assert_eq!(swept_as("clean only", true, Plan { faults: false, ..plan(None) }, |f| small(f, holds)).runs, 0);
        for only in ["call:1", "dropped:1", "outage:1", "down:1", "store:1", "store:x"] {
            assert_eq!(swept_as(only, false, plan(Some(only)), |f| small(f, holds)).runs, 1, "{only}");
        }
        assert_eq!(swept_as("nothing said after", false, plan(Some("call")), |f| small(f, holds)).runs, 3);
        // (A scenario that makes no call and no store operation: nothing to fail.)
        let none = swept_as("none", true, plan(None), |f| { let o = Outcome::new(serve(|_| {})); o.served.arm(f); o });
        assert_eq!(none.runs, 0);
    }

    #[test]
    fn what_broke_is_said_with_its_fault() {
        let said = |p: Box<dyn std::any::Any + Send>| panic_said(&*p);
        // Clean: its checks not holding, and the scenario panicking.
        let e = catch_unwind(AssertUnwindSafe(|| swept_as("checks", false, plan(None), |f| small(f, |_| false)))).unwrap_err();
        assert!(said(e).contains("checks, clean: no fault broke it"));
        let e = catch_unwind(AssertUnwindSafe(|| swept_as("panics", false, plan(None), |_| panic!("at once")))).unwrap_err();
        assert!(said(e).contains("panics, clean: the scenario panicked: at once"));
        // Faulted: the same, by a String's words and by none.
        let e = catch_unwind(AssertUnwindSafe(|| swept_as("faulted", false, plan(None), |f| small(f, |f| f == Fault::None)))).unwrap_err();
        assert!(said(e).contains("faulted: 3 of 3 faulted runs broke what must hold") );
        let e = catch_unwind(AssertUnwindSafe(|| swept_as("faulted panics", false, plan(None), |f| { if f != Fault::None { panic!("{f}") } small(f, |_| true) }))).unwrap_err();
        assert!(said(e).contains("the scenario panicked: call 1 failing"));
        assert_eq!(run(&|_| std::panic::panic_any(5), Fault::None).err().unwrap(), "the scenario panicked: ");
    }

    #[test]
    fn a_control_plane_panicking_or_answering_nothing_of_a_failure_is_held_against_it() {
        let o = Outcome::new(serve(|_| {}));
        let f = o.served.faults().clone();
        f.asking("GET /a".into());
        f.answered(200, b"");
        f.answered(500, b"it said");
        f.answered(500, b" ");
        f.panicked(Box::new("in words"));
        f.panicked(Box::new("in a String".to_string()));
        f.panicked(Box::new(5));
        let (_, problems) = run(&|_| Outcome { served: serve(|_| {}), problems: Default::default() }, Fault::None).unwrap();
        assert!(problems.is_empty());
        assert_eq!(f.panics(), ["answering GET /a: in words", "answering GET /a: in a String", "answering GET /a: "]);
        let (_, problems) = run(&|_| Outcome { served: { let s = serve(|_| {}); s.faults().asking("GET /a".into()); s.faults().answered(200, b""); s.faults().answered(500, b"it said"); s.faults().answered(500, b" "); s.faults().panicked(Box::new("x")); s }, problems: Default::default() }, Fault::None).unwrap();
        assert_eq!(problems, ["the control plane panicked answering GET /a: x", "GET /a was answered 500 saying nothing"]);
    }

    #[test]
    fn a_failed_call_is_answered_in_its_services_own_words() {
        for (url, target, kind, words) in [("https://b.s3.us-east-1.amazonaws.com/k", "", "application/xml", "InternalError"), ("https://ssm.us-east-1.amazonaws.com/", "AmazonSSM.GetParameter", "application/x-amz-json-1.1", "InternalServerError"),
            ("https://lambda.us-east-1.amazonaws.com/f", "", "application/json", "Service"), ("https://ec2.us-east-1.amazonaws.com/", "", "text/xml", "InternalError"), ("https://signin.aws.amazon.com/", "", "text/xml", "InternalError"),
            ("https://gitlab.com/api", "", "application/json", "500 Internal"), ("https://api.cloudflare.com/x", "", "application/json", "Internal server error"), ("https://a.workers.dev/x", "", "application/json", "Internal server error"),
            ("https://a.modal.run/x", "", "application/json", "Internal Server Error"), ("https://api.modal.com/x", "", "application/json", "Internal Server Error"), ("https://api.github.com/x", "", "application/json", "Server Error"), ("not an address", "", "application/json", "Server Error")] {
            let (status, headers, body) = failed_answer(url, target);
            assert_eq!((status, headers[0].1.as_str()), (500, kind), "{url}");
            assert!(body.contains(words), "{url}: {body}");
        }
    }
}
