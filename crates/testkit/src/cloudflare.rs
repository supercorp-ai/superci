//! The Cloudflare control plane as Cloudflare runs it, on this computer: its Worker and Durable Object, built
//! (`crates/plane-cloudflare/build`, by `./build.sh`), under `wrangler dev` as the Worker's own tests run it (see
//! `worker`), with storage of its own for the test. `None` where there is no Node or the Worker is not built.
use crate::worker::{self, Setup, Worker};

pub struct CloudflarePlane { pub url: String, pub plane: String, pub key: String, pub run_key: String, _worker: Worker }

/// Its Durable Objects, as a deploy binds them.
const BINDINGS: &str = "[[durable_objects.bindings]]\nname = \"PLANE\"\nclass_name = \"PlaneObject\"\n\n[[durable_objects.bindings]]\nname = \"RUNNER\"\nclass_name = \"JobRunner\"\n\n[[durable_objects.bindings]]\nname = \"LEGACY\"\nclass_name = \"Runner\"\n\n[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"PlaneObject\", \"JobRunner\", \"Runner\"]\n";

pub fn serve_cloudflare() -> Option<CloudflarePlane> { serve_built(worker::built(&format!("{}/../plane-cloudflare", env!("CARGO_MANIFEST_DIR")))) }

fn serve_built(built: std::path::PathBuf) -> Option<CloudflarePlane> {
    let (plane, key, run_key) = ("cftest000001".to_string(), "dashboard-key-for-tests-0001".to_string(), "superci_run_0123456789abcdef0123456789abcdef".to_string());
    let read = format!("{}:runs", superci_core::crypto::sha256_hex(run_key.as_bytes()));
    let w = Worker::start(&Setup { built, bindings: BINDINGS, vars: &format!("PLANE_ID = \"{plane}\"") },
        &[("DEVICE_KEY_4102444800_TEST".into(), key.clone()), ("READ_KEY_4102444800_AGENT".into(), read)])?;
    Some(CloudflarePlane { url: w.url.clone(), plane, key, run_key, _worker: w })
}

#[cfg(test)]
mod tests {
    #[test]
    fn where_the_worker_is_not_built_none_is_served() { assert!(super::serve_built("/no/such/index.js".into()).is_none()) }
}
