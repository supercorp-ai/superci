//! The Cloudflare launcher's Worker as Cloudflare runs it: the module `./build.sh` makes, under `wrangler dev`, its
//! runners' containers answered by the test (see the test kit's `worker`: nothing of the Worker's own is changed). It
//! takes only what its one control plane signed, for it, from that control plane. Where Node is and the Worker is
//! built; skipped elsewhere.
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use superci_core::crypto::SigningKeyStore;
use superci_testkit::worker::{self, Setup, Worker};

const PLANE_URL: &str = "https://superci.acme.workers.dev";
const SUBJECT: &str = "plane:cfplane00001";

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

const BINDINGS: &str = "[[durable_objects.bindings]]\nname = \"RUNNER\"\nclass_name = \"JobRunner\"\n\n[[durable_objects.bindings]]\nname = \"LEGACY\"\nclass_name = \"Runner\"\n\n[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"JobRunner\", \"Runner\"]\n";

/// The launcher as the dashboard sets it up for a control plane (its address, its id, its public keys pinned); with
/// `trust` given, these settings instead (`[vars]` lines: none, for a launcher set up with no control plane).
fn start(keys: &SigningKeyStore, trust: Option<&str>) -> Option<Worker> { start_bound(keys, trust, BINDINGS) }

/// The same, its Durable Objects bound as these lines say.
fn start_bound(keys: &SigningKeyStore, trust: Option<&str>, bindings: &str) -> Option<Worker> {
    let pinned = json!({ "keys": [keys.public_jwk().unwrap()] }).to_string();
    let vars = trust.map(str::to_string).unwrap_or_else(|| format!("TRUSTED_ISSUER = \"{PLANE_URL}\"\nTRUSTED_SUBJECT = \"{SUBJECT}\"\nTRUSTED_KEYS = '{pinned}'"));
    Worker::start(&Setup { built: worker::built(env!("CARGO_MANIFEST_DIR")), bindings, vars: &vars }, &[])
}

/// A token as a control plane signs one: for `aud`, from `sub`, by `iss`, good for a few minutes (or ended).
fn token(keys: &SigningKeyStore, iss: &str, sub: &str, aud: &str, ended: bool) -> String {
    let now = now_secs();
    let exp = if ended { now - 60 } else { now + 300 };
    keys.jwt(&json!({ "iss": iss, "sub": sub, "aud": aud, "iat": now - 1, "exp": exp })).unwrap()
}

/// As the control plane asks the launcher: `path` with this token.
fn asked(w: &Worker, path: &str, token: &str, body: Value) -> (u16, String) {
    w.call("POST", path, &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")], body.to_string().as_bytes())
}

/// Its health; what it has not (another path, another method): nothing there.
#[test]
fn the_launcher_says_what_it_is_and_has_nothing_else() {
    let keys = SigningKeyStore::generate();
    let Some(w) = start(&keys, None) else { return };
    let (status, health) = w.get("/health");
    assert!(status == 200 && health.contains("\"agent\":\"cloudflare\""), "{status} {health}");
    for (method, path) in [("GET", "/launch"), ("POST", "/nothing"), ("GET", "/")] { assert_eq!(w.call(method, path, &[], b"").0, 404, "{method} {path}") }
}

/// Only its control plane's tokens are taken: none, one not signed by its keys, one for another launcher, from another
/// control plane or of another, one that ended: refused, saying why. A launcher set up with no control plane, or with
/// keys that are none (or a list of none), takes nothing.
#[test]
fn only_its_control_planes_tokens_are_taken() {
    let keys = SigningKeyStore::generate();
    let Some(w) = start(&keys, None) else { return };
    let me = w.url.clone();
    let other = SigningKeyStore::generate();
    let launch = json!({ "name": "r1", "jit": "eyJqaXQiOnRydWV9" });
    assert!(w.call("POST", "/launch", &[], launch.to_string().as_bytes()).1.contains("no token"));
    for (t, why) in [(token(&other, PLANE_URL, SUBJECT, &me, false), "bad token"), (token(&keys, PLANE_URL, SUBJECT, "https://another.acme.workers.dev", false), "for another agent or control plane"),
        (token(&keys, "https://elsewhere.workers.dev", SUBJECT, &me, false), "for another agent or control plane"), (token(&keys, PLANE_URL, "plane:other", &me, false), "for another agent or control plane"),
        (token(&keys, PLANE_URL, SUBJECT, &me, true), "bad token")] {
        let (status, said) = asked(&w, "/launch", &t, launch.clone());
        assert!(status == 401 && said.contains(why), "{why}: {status} {said}");
    }
    assert!(w.containers.ops("start").is_empty());
    drop(w);
    for (trust, why) in [("", "no trusted control plane"), (&format!("TRUSTED_ISSUER = \"{PLANE_URL}\""), "no trusted control plane"),
        (&format!("TRUSTED_ISSUER = \"{PLANE_URL}\"\nTRUSTED_SUBJECT = \"{SUBJECT}\""), "no trusted keys"), (&format!("TRUSTED_ISSUER = \"{PLANE_URL}\"\nTRUSTED_SUBJECT = \"{SUBJECT}\"\nTRUSTED_KEYS = \"none\""), "not allowed"),
        (&format!("TRUSTED_ISSUER = \"{PLANE_URL}\"\nTRUSTED_SUBJECT = \"{SUBJECT}\"\nTRUSTED_KEYS = '{{}}'"), "bad token")] {
        let Some(w) = start(&keys, Some(trust)) else { return };
        let (status, said) = asked(&w, "/launch", &token(&keys, PLANE_URL, SUBJECT, &w.url, false), launch.clone());
        assert!(status == 401 && said.contains(why), "{why}: {status} {said}");
    }
}

/// A runner launched for a job (where the control plane says, or where Cloudflare chooses), known after by its Durable
/// Object's id; stopped by that id or by its name; one the runner refuses to start, said as it said it; one with no
/// name, refused.
#[test]
fn runners_are_launched_and_stopped_as_the_control_plane_asks() {
    let keys = SigningKeyStore::generate();
    let Some(w) = start(&keys, None) else { return };
    let t = token(&keys, PLANE_URL, SUBJECT, &w.url, false);
    let (status, said) = asked(&w, "/launch", &t, json!({ "name": "job-1", "jit": "eyJqaXQiOnRydWV9", "max_minutes": 30, "location": "enam" }));
    let started: Value = serde_json::from_str(&said).unwrap_or_default();
    let id = started["id"].as_str().unwrap_or_default().to_string();
    assert!(status == 200 && id.len() == 64 && started["kind"] == "container", "{status} {said}");
    assert_eq!(w.containers.ops("start").len(), 1);
    let (status, said) = asked(&w, "/stop", &t, json!({ "id": id }));
    assert!(status == 200 && said.contains("stopped") && !said.contains("\"id\""), "a stop's answer is the runner's own: {status} {said}");
    assert_eq!(w.containers.ops("destroy").len(), 1);
    assert_eq!(asked(&w, "/launch", &t, json!({ "name": "job-2", "jit": "eyJqaXQiOnRydWV9", "location": "auto" })).0, 200);
    assert_eq!(asked(&w, "/stop", &t, json!({ "id": "job-2" })).0, 200);
    let (status, said) = asked(&w, "/launch", &t, json!({ "name": "job-3" }));
    assert!(status == 400 && said.contains("no runner configuration"), "{status} {said}");
    assert_ne!(asked(&w, "/launch", &t, json!({ "jit": "eyJqaXQiOnRydWV9" })).0, 200);
}

/// What the launcher cannot do fails, not taken for done: a body that is not JSON; a runner by an id that is not one of
/// its Durable Objects'; one whose Worker has no containers; a Worker without its runners' binding. A name of 64
/// characters that are not an id's is a name. A location Cloudflare has not: refused, said.
#[test]
fn what_the_launcher_cannot_do_fails() {
    let keys = SigningKeyStore::generate();
    let Some(w) = start(&keys, None) else { return };
    let t = token(&keys, PLANE_URL, SUBJECT, &w.url, false);
    let (status, said) = w.call("POST", "/launch", &[("authorization", &format!("Bearer {t}"))], b"not json");
    assert_eq!(status, 500, "{said}");
    let (status, said) = asked(&w, "/stop", &t, json!({ "id": "f".repeat(64) }));
    assert_eq!(status, 500, "{said}");
    let (status, said) = asked(&w, "/stop", &t, json!({ "id": "z".repeat(64) }));
    assert!(status == 200 && said.contains("stopped"), "{status} {said}");
    let (status, said) = asked(&w, "/launch", &t, json!({ "name": "job-9", "jit": "eyJqaXQiOnRydWV9", "location": "nowhere" }));
    assert!(status == 400 && said.contains("no location nowhere"), "{status} {said}");
    w.containers_are(json!({ "noContainer": true }));
    let (status, said) = asked(&w, "/launch", &t, json!({ "name": "job-10", "jit": "eyJqaXQiOnRydWV9" }));
    assert_eq!(status, 500, "{said}");
    assert!(w.containers.ops("start").is_empty());
    drop(w);
    let Some(w) = start_bound(&keys, None, "[[durable_objects.bindings]]\nname = \"LEGACY\"\nclass_name = \"Runner\"\n\n[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"JobRunner\", \"Runner\"]\n") else { return };
    let t = token(&keys, PLANE_URL, SUBJECT, &w.url, false);
    let (status, said) = asked(&w, "/launch", &t, json!({ "name": "job-11", "jit": "eyJqaXQiOnRydWV9" }));
    assert_eq!(status, 500, "{said}");
}

/// A runner's container, each way Cloudflare's may go, as the launcher asks it.
#[test]
fn a_runners_container_goes_each_way_cloudflares_may() {
    let Some(w) = start(&SigningKeyStore::generate(), None) else { return };
    worker::each_way_a_container_goes(&w);
}

/// A runner launched and stopped with each of Cloudflare's container calls failing in turn: nothing panics, and each
/// answer of the launcher's that is an error says what failed.
#[test]
fn a_launched_runner_holds_with_each_call_and_store_operation_failing() {
    let keys = SigningKeyStore::generate();
    worker::sweep("a launched runner", || start(&keys, None), |w| {
        let t = token(&keys, PLANE_URL, SUBJECT, &w.url, false);
        let mut problems = vec![];
        for (path, body) in [("/launch", json!({ "name": "job-1", "jit": "eyJqaXQiOnRydWV9", "max_minutes": 30 })), ("/stop", json!({ "id": "job-1" }))] {
            let (status, said) = asked(w, path, &t, body);
            if status != 200 && said.trim().is_empty() { problems.push(format!("{path}: {status} with nothing said")) }
        }
        problems
    });
}
