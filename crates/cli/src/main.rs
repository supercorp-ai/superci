//! `superci`: your dashboard, on this machine. You sign in with your clouds in the browser; it finds or sets up the
//! control plane there (Cloudflare now; AWS and Modal next), creates its GitHub App, connects the clouds that run jobs,
//! and shows whether it all works. Nothing is kept on this machine and nothing of it stays running: the control plane
//! in your cloud does the always-on part.
//!
//! The control plane's Worker (Rust compiled to WebAssembly, crates/plane-cloudflare) is inside this binary.
mod agents;
mod aws;
mod aws_plane;
mod cloudflare;
mod dashboard;
mod image;
mod live;
mod logos;
mod modal;
mod plane;
mod view;

pub type Result<T> = std::result::Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") { println!("superci {}", env!("CARGO_PKG_VERSION")); return }
    if args.iter().any(|a| a == "-h" || a == "--help") || args.iter().any(|a| a != "init" && a != "open" && a != "--no-browser") {
        eprintln!("SuperCI — GitHub Actions jobs on your own clouds

Usage:
  superci        Opens your dashboard (on this machine only, while this runs): sign in with your clouds, set up or
                 find SuperCI there, connect GitHub and AWS, see whether it all works.
                 For automation, CLOUDFLARE_API_TOKEN replaces the Cloudflare sign-in.
  --no-browser   Prints the dashboard's link without opening it.");
        std::process::exit(2)
    }
    if let Err(e) = dashboard::Dashboard::new().serve(!args.iter().any(|a| a == "--no-browser")) {
        eprintln!("superci: {e}");
        std::process::exit(1)
    }
}
