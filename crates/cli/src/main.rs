//! `superci`: your dashboard, on this machine, and the same things as commands. You sign in with your clouds in the
//! browser; it finds or sets up the control plane there, creates its GitHub App, connects the clouds that run jobs,
//! and shows whether it all works. SuperCI's own sign-ins are kept in its folder on this machine (`~/.superci`), so
//! the dashboard opens signed in and commands run without a browser; nothing of it stays running: the control plane in
//! your cloud does the always-on part.
//!
//! The control planes (Rust compiled to WebAssembly for Cloudflare, a Lambda package, Modal's program) are inside
//! this binary.
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
mod store;
mod view;

pub type Result<T> = std::result::Result<T, String>;

const HELP: &str = "SuperCI — GitHub Actions and GitLab CI jobs on your own clouds

Usage:
  superci           Opens your dashboard (on this machine only, while this runs): set up or find SuperCI in your
                    cloud, connect repositories and runners, see your jobs.
  superci login     Signs in with your cloud in the browser, and ends. The dashboard's sign-in does the same.
  superci status    The control plane in use and what it says: its version, repositories, runners, jobs.
  superci logout    Removes SuperCI's sign-ins from this machine.

  --json            (status, logout) Prints JSON. What a person must do first is said in \"needs\".
  --no-browser      (dashboard, login) Prints the link without opening it.

SuperCI keeps its own sign-ins in ~/.superci (SUPERCI_HOME to put it elsewhere) and reads no other tool's.
On a machine with no browser, a sign-in can be given by name instead: SUPERCI_CLOUDFLARE_TOKEN,
SUPERCI_MODAL_TOKEN_ID and SUPERCI_MODAL_TOKEN_SECRET.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") { println!("superci {}", env!("CARGO_PKG_VERSION")); return }
    let flag = |f: &str| args.iter().any(|a| a == f);
    let words: Vec<&str> = args.iter().map(String::as_str).filter(|a| !a.starts_with('-')).collect();
    let known_flags = args.iter().filter(|a| a.starts_with('-')).all(|a| ["--no-browser", "--json"].contains(&a.as_str()));
    if flag("-h") || flag("--help") { println!("{HELP}"); return }
    let command = match words.as_slice() { [] | ["open"] | ["init"] => "open", [one @ ("login" | "status" | "logout")] => one, _ => "" };
    if command.is_empty() || !known_flags { eprintln!("{HELP}"); std::process::exit(2) }
    let json = flag("--json");
    let signed_in = || dashboard::Dashboard::signed_in(store::Store::new());
    // A command's failure: said (as JSON too, with what a person must do first), and a status other than 0.
    let fail = |e: String| -> ! {
        let needs = if e == dashboard::NOT_SIGNED_IN || e == dashboard::AWS_ENDED { Some("superci login") } else { None };
        if json { println!("{}", serde_json::json!({ "error": e, "needs": needs })) } else { eprintln!("superci: {e}") }
        std::process::exit(if needs.is_some() { 3 } else { 1 })
    };
    match command {
        "status" => match signed_in().status() {
            Ok(v) if json => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
            Ok(v) => print!("{}", status_text(&v)),
            Err(e) => fail(e),
        },
        "logout" => match signed_in().logout() {
            Ok(said) if json => println!("{}", serde_json::json!({ "done": said })),
            Ok(said) => for line in said { println!("{line}") },
            Err(e) => fail(e),
        },
        "login" => {
            let d = signed_in();
            if let Some(at) = d.signed_in_as() { println!("SuperCI is signed in on this computer: {at}. `superci logout` removes it; the dashboard (`superci`) signs in to more clouds."); return }
            if let Err(e) = d.for_login().serve(!flag("--no-browser")) { fail(e) }
        }
        _ => if let Err(e) = signed_in().serve(!flag("--no-browser")) { fail(e) },
    }
}

/// `superci status` for a person: a few lines.
fn status_text(v: &serde_json::Value) -> String {
    let mut out = format!("Signed in     {}\n", v["signed_in"].as_str().unwrap_or_default());
    let p = &v["control_plane"];
    if p.is_null() { return out + "Control plane none found in the clouds signed in to. Open the dashboard (`superci`) to set one up.\n" }
    let s = |x: &serde_json::Value| x.as_str().unwrap_or_default().to_string();
    out += &format!("Control plane {} · {}{}\n", s(&p["where"]), match p["version"].as_str() { Some(ver) => ver.to_string(), None => "not answering".into() },
        p["update_to"].as_str().map(|to| format!(" (update to {to}: Control plane → Update in the dashboard)")).unwrap_or_default());
    out += &format!("Address       {}\n", s(&p["url"]));
    out += &format!("Jobs can run  {}\n", if p["ready"] == true { "yes" } else { "not yet: repositories and a runner provider are both needed (see the dashboard's Set up)" });
    let st = &v["status"];
    if st.is_null() { return out + "Its jobs could not be read just now (it did not answer with this machine's key yet). Try again in a moment.\n" }
    let order: Vec<String> = st["routing"]["order"].as_array().into_iter().flatten().filter(|o| o["off"] != true).filter_map(|o| o["cloud"].as_str().map(str::to_string)).collect();
    if !order.is_empty() { out += &format!("Runner order  {}\n", order.join(", ")) }
    let jobs = st["jobs"].as_array().cloned().unwrap_or_default();
    let count = |states: &[&str]| jobs.iter().filter(|j| states.contains(&j["state"].as_str().unwrap_or_default())).count();
    out += &format!("Jobs          {} listed · {} running · {} starting · {} waiting\n", jobs.len(), count(&["running"]), count(&["launching", "launched"]), count(&["waiting"]));
    out
}
