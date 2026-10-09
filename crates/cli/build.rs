//! Finds what the program carries inside it: the control planes and the runner agent, built by `./build.sh`, and
//! the changelog. In this repository they are where the build leaves them; in the crate published to crates.io
//! they come along in `embedded/`, already built, so that `cargo install superci` needs nothing but Rust.
use std::path::Path;

fn main() {
    let here = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let here = Path::new(&here);
    for (name, built, packed) in [
        ("SUPERCI_WORKER_JS", "../plane-cloudflare/build/index.js", "plane-cloudflare.js"),
        ("SUPERCI_WORKER_WASM", "../plane-cloudflare/build/index_bg.wasm", "plane-cloudflare.wasm"),
        ("SUPERCI_RUNNERS_JS", "../runners-cloudflare/build/index.js", "runners-cloudflare.js"),
        ("SUPERCI_RUNNERS_WASM", "../runners-cloudflare/build/index_bg.wasm", "runners-cloudflare.wasm"),
        ("SUPERCI_PLANE_AWS", "../plane-aws/build/bootstrap.zip", "plane-aws.zip"),
        ("SUPERCI_PLANE_MODAL", "../plane-modal/build/superci-plane", "plane-modal"),
        ("SUPERCI_CHANGELOG", "../../CHANGELOG.md", "CHANGELOG.md"),
    ] {
        let (built, packed) = (here.join(built), here.join("embedded").join(packed));
        println!("cargo:rerun-if-changed={}", built.display());
        println!("cargo:rerun-if-changed={}", packed.display());
        let found = if built.exists() { built } else if packed.exists() { packed } else {
            panic!("{} is not built: run ./build.sh in the repository first (it builds what the program carries inside it)", built.display())
        };
        println!("cargo:rustc-env={name}={}", found.display());
    }
}
