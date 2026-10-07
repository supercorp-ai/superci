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
mod commands;
mod dashboard;
mod image;
mod live;
mod logos;
mod modal;
mod plane;
mod store;
mod view;

pub type Result<T> = std::result::Result<T, String>;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let json = raw.iter().any(|a| a == "--json");
    // A failure: said (as JSON too, with the command a person runs first when that is what it needs), and its status.
    let fail = |e: String, usage: bool| -> ! {
        let needs = commands::needs(&e);
        if json { println!("{}", serde_json::json!({ "ok": false, "error": e, "needs": needs })) } else { eprintln!("superci: {e}") }
        std::process::exit(if needs.is_some() { 3 } else if usage { 2 } else { 1 })
    };
    let args = match commands::Args::parse(&raw) { Ok(a) => a, Err(e) => fail(e, true) };
    if args.has("version") { println!("superci {}", env!("CARGO_PKG_VERSION")); return }
    if args.has("help") || args.words.first().is_some_and(|w| w == "help") { println!("{}", commands::HELP); return }
    // The dashboard itself.
    if matches!(args.words.as_slice(), [] | [_]) && ["", "open", "init"].contains(&args.words.first().map(String::as_str).unwrap_or("")) {
        if let Err(e) = dashboard::Dashboard::signed_in(store::Store::new()).serve(!args.has("no-browser")) { fail(e, false) }
        return
    }
    // What a person does in the browser: the dashboard, opened for that alone.
    if let Some(done) = commands::in_browser(&args) {
        if let Err(e) = done { fail(e, false) }
        return
    }
    // A long command says each step as it begins (to a person; a program gets them with the result).
    let mut steps = vec![];
    let result = commands::run(&args, &mut |step: &str| { if !json { println!("{step}…") } steps.push(step.to_string()) });
    match result {
        Ok(done) if json => {
            let mut data = done.data;
            if !steps.is_empty() { if let Some(o) = data.as_object_mut() { o.insert("steps".into(), steps.into()); } }
            println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default())
        }
        Ok(done) => for line in done.said { println!("{line}") },
        Err(e) => { let usage = e.starts_with("not a command"); fail(if usage && !json { format!("{e}\n\n{}", commands::HELP) } else { e }, usage) }
    }
}
