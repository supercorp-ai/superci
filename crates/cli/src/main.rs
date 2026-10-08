//! `superci`: your dashboard, on this machine (`superci dashboard`), and the same things as commands. You sign in with your clouds in the
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
    // A failure: said (as JSON too, with the command a person runs first when that is what it needs), and its status:
    // 1 failed, 2 not a command or not a whole one (its help follows), 3 a person is needed first.
    let fail = |e: commands::Fail, topic: &str| -> ! {
        let (said, half, unconfirmed) = (e.said().to_string(), matches!(e, commands::Fail::Usage(_)), matches!(e, commands::Fail::Unconfirmed(_)));
        let needs = commands::needs(&said);
        if json { println!("{}", serde_json::json!({ "ok": false, "error": said, "needs": needs })) }
        else if half { eprintln!("superci: {said}\n\n{}", commands::help(topic)) }
        else { eprintln!("superci: {said}") }
        std::process::exit(if needs.is_some() { 3 } else if half || unconfirmed { 2 } else { 1 })
    };
    let args = match commands::Args::parse(&raw) { Ok(a) => a, Err(e) => fail(e, "") };
    if args.has("version") { println!("superci {}", env!("CARGO_PKG_VERSION")); return }
    // Help: for everything (also what `superci` alone says), or for one command.
    if args.words.is_empty() { println!("{}", commands::help("")); return }
    if args.word(0) == "help" { println!("{}", commands::help(args.word(1))); return }
    if args.has("help") { println!("{}", commands::help(args.word(0))); return }
    // The dashboard, or the dashboard opened for the one thing a person does in the browser.
    if let Some(done) = commands::in_browser(&args) {
        if let Err(e) = done { fail(e, args.word(0)) }
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
        Err(e) => fail(e, args.word(0)),
    }
}
