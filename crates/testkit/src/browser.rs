//! Chrome, headless, driven as a person would use a page: it goes to an address, clicks, types and submits, and the
//! page's own scripts run as they do for anyone. Spoken to by Chrome's DevTools protocol over a pipe
//! (`--remote-debugging-pipe`: its descriptors 3 and 4, a message each ending in a NUL), so nothing more is needed
//! than Chrome itself. `None` where no Chrome is installed.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

pub struct Browser { child: std::process::Child, to: std::fs::File, from: Receiver<Value>, id: u64, session: String, _profile: TempDir }

struct TempDir(std::path::PathBuf);
impl Drop for TempDir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

/// Where Chrome may be.
const CHROMES: [&str; 5] = ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/usr/bin/google-chrome", "/usr/bin/google-chrome-stable", "/usr/bin/chromium", "/usr/bin/chromium-browser"];

fn pipe() -> (i32, i32) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    (fds[0], fds[1])
}

impl Browser {
    /// A fresh Chrome (a profile of its own) with one tab, `width` × `height`.
    pub fn open(width: u32, height: u32) -> Option<Browser> { Browser::open_among(&CHROMES, width, height) }

    /// The same, with the first of these that is there; `None` where none is. (One that is there and does not start
    /// is no test passed: it panics.)
    fn open_among(chromes: &[&str], width: u32, height: u32) -> Option<Browser> {
        let chrome = chromes.iter().find(|p| std::path::Path::new(p).exists())?;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let profile = TempDir(std::env::temp_dir().join(format!("superci-chrome-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst))));
        let ((their_in, our_out), (our_in, their_out)) = (pipe(), pipe());
        // (Ours are not handed on to Chrome; its two become its 3 and 4.)
        unsafe { libc::fcntl(our_out, libc::F_SETFD, libc::FD_CLOEXEC); libc::fcntl(our_in, libc::F_SETFD, libc::FD_CLOEXEC); }
        let mut cmd = std::process::Command::new(*chrome);
        cmd.args(["--headless=new", "--remote-debugging-pipe", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--hide-scrollbars", "--mute-audio",
            &format!("--window-size={width},{height}"), &format!("--user-data-dir={}", profile.0.display()), "about:blank"])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        unsafe { cmd.pre_exec(move || { libc::dup2(their_in, 3); libc::dup2(their_out, 4); Ok(()) }); }
        let child = cmd.spawn().expect("Chrome, which is there, starting");
        unsafe { libc::close(their_in); libc::close(their_out); }
        let (to, from_chrome) = unsafe { (std::fs::File::from_raw_fd(our_out), std::fs::File::from_raw_fd(our_in)) };
        let (tx, from) = channel();
        std::thread::spawn(move || messages(from_chrome, tx));
        let mut b = Browser { child, to, from, id: 0, session: String::new(), _profile: profile };
        let target = b.call("Target.createTarget", json!({ "url": "about:blank" })).unwrap()["targetId"].clone();
        b.session = b.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true })).unwrap()["sessionId"].as_str().unwrap().to_string();
        b.call("Page.enable", json!({})).unwrap();
        b.call("Runtime.enable", json!({})).unwrap();
        Some(b)
    }

    /// One command, to the tab (once attached), and its result.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let mut msg = json!({ "id": self.id, "method": method, "params": params });
        if !self.session.is_empty() { msg["sessionId"] = json!(self.session) }
        self.to.write_all(format!("{msg}\0").as_bytes()).map_err(|e| e.to_string())?;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let v = self.from.recv_timeout(until.saturating_duration_since(Instant::now())).map_err(|_| format!("Chrome did not answer {method}"))?;
            if v["id"] == self.id {
                if let Some(e) = v.get("error") { return Err(format!("{method}: {e}")) }
                return Ok(v["result"].clone());
            }
        }
    }

    /// Goes to `url` and waits for its page to have loaded.
    pub fn goto(&mut self, url: &str) {
        self.call("Page.navigate", json!({ "url": url })).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        self.wait("document.readyState === 'complete'");
    }

    /// What `js` comes to in the page (a promise waited for).
    pub fn eval(&mut self, js: &str) -> Value {
        let r = self.call("Runtime.evaluate", json!({ "expression": js, "returnByValue": true, "awaitPromise": true })).unwrap();
        if let Some(e) = r.get("exceptionDetails") { panic!("{js}: {}", e["exception"]["description"].as_str().unwrap_or(&e.to_string())) }
        r["result"]["value"].clone()
    }

    /// Waits, up to 30 seconds (a page among many tests at once, on a busy machine; `SUPERCI_BROWSER_WAIT_S` for
    /// longer, as under a coverage run), for `js` to hold in the page.
    pub fn wait(&mut self, js: &str) {
        self.wait_within(js, Duration::from_secs(std::env::var("SUPERCI_BROWSER_WAIT_S").ok().and_then(|s| s.parse().ok()).unwrap_or(30)))
    }

    fn wait_within(&mut self, js: &str, time: Duration) {
        if crate::within(time, || self.call("Runtime.evaluate", json!({ "expression": format!("!!({js})"), "returnByValue": true })).is_ok_and(|r| r["result"]["value"] == true)) { return }
        let shown = self.text("main");
        panic!("waited for {js}; the page shows: {}", shown.chars().take(2000).collect::<String>());
    }

    /// The words an element shows (the first that `selector` names; nothing when there is none).
    pub fn text(&mut self, selector: &str) -> String {
        self.eval(&format!("(document.querySelector({0}) || {{innerText: ''}}).innerText", json!(selector))).as_str().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Clicks the first element `selector` names whose words contain `words` (any, when empty), as a person does: at
    /// its middle, with the mouse.
    /// (Once it is there, holds still and is what is at that point: a page still laying itself out, a card coming in
    /// above, would take the click somewhere else.)
    pub fn click(&mut self, selector: &str, words: &str) { self.click_within(selector, words, Duration::from_secs(30)) }

    fn click_within(&mut self, selector: &str, words: &str, time: Duration) {
        let find = format!("(() => {{ const e = [...document.querySelectorAll({})].find(e => e.innerText.includes({}) || e.value === {1}); if (!e) return null;
            e.scrollIntoView({{block: 'center'}}); const r = e.getBoundingClientRect(); const x = r.x + r.width / 2, y = r.y + r.height / 2; const at = document.elementFromPoint(x, y);
            return [x, y, !!at && (at === e || e.contains(at) || at.contains(e))]; }})()", json!(selector), json!(words));
        let mut last: Option<[f64; 2]> = None;
        // (Up to 15 seconds, as `wait`: a page loading under a slow, measured build comes late.)
        let until = Instant::now() + time;
        let (x, y) = loop {
            let at = self.eval(&find);
            let Some(a) = at.as_array() else {
                if Instant::now() > until { panic!("nothing to click: {selector} {words:?} on {}", self.text("main")) }
                std::thread::sleep(Duration::from_millis(100));
                continue
            };
            let here = [a[0].as_f64().unwrap(), a[1].as_f64().unwrap()];
            if last == Some(here) && a[2] == true { break (here[0], here[1]) }
            if Instant::now() > until { panic!("not clickable (moving, or under something else): {selector} {words:?} on {}", self.text("main")) }
            last = Some(here);
            std::thread::sleep(Duration::from_millis(100));
        };
        ["mousePressed", "mouseReleased"].into_iter().for_each(|kind| { self.call("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 })).unwrap(); });
    }

    /// Types `text` into the field `selector` names, what was there before taken out first.
    pub fn fill(&mut self, selector: &str, text: &str) {
        self.eval(&format!("(() => {{ const e = document.querySelector({}); e.focus(); e.select && e.select(); }})()", json!(selector)));
        self.call("Input.dispatchKeyEvent", json!({ "type": "keyDown", "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8 })).unwrap();
        self.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8 })).unwrap();
        self.call("Input.insertText", json!({ "text": text })).unwrap();
    }

    /// The page as a PNG, at full size (for a person to look at).
    pub fn screenshot(&mut self, path: &std::path::Path) {
        let shot = self.call("Page.captureScreenshot", json!({ "format": "png", "captureBeyondViewport": false })).unwrap();
        std::fs::write(path, base64::Engine::decode(&base64::engine::general_purpose::STANDARD, shot["data"].as_str().unwrap()).unwrap()).unwrap();
    }
}

impl Drop for Browser {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

/// What Chrome says on its pipe, a message each ending in a NUL, passed on until it ends or no one listens.
fn messages(mut from: impl Read, tx: Sender<Value>) {
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 65536]);
    while let Ok(n) = from.read(&mut chunk) {
        if n == 0 { break }
        buf.extend_from_slice(&chunk[..n]);
        while let Some(end) = buf.iter().position(|b| *b == 0) {
            let msg: Vec<u8> = buf.drain(..=end).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&msg[..msg.len() - 1]) { if tx.send(v).is_err() { return } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// A Browser with no Chrome behind it: `to` where its commands go, `from` where its answers come from.
    fn standing(to: std::fs::File, from: Receiver<Value>) -> Browser {
        let child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        Browser { child, to, from, id: 0, session: "s".into(), _profile: TempDir(std::env::temp_dir().join(format!("superci-no-chrome-{}", std::process::id()))) }
    }

    fn sink() -> std::fs::File { std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap() }

    #[test]
    fn where_no_chrome_is_it_opens_none() {
        assert!(Browser::open_among(&[], 800, 600).is_none());
        assert!(Browser::open_among(&["/no/such/chrome"], 800, 600).is_none());
    }

    #[test]
    fn a_command_not_sent_or_not_answered_or_refused_is_said() {
        let (tx, from) = channel();
        drop(tx);
        // (Its pipe open for reading only: nothing can be written.)
        let mut b = standing(std::fs::File::open("/dev/null").unwrap(), from);
        assert!(b.call("Page.enable", json!({})).is_err());
        let (tx, from) = channel();
        drop(tx);
        let mut b = standing(sink(), from);
        assert_eq!(b.call("Page.enable", json!({})), Err("Chrome did not answer Page.enable".to_string()));
        let (tx, from) = channel();
        tx.send(json!({ "method": "an event, no answer" })).unwrap();
        tx.send(json!({ "id": 1, "error": { "message": "no" } })).unwrap();
        let mut b = standing(sink(), from);
        assert!(b.call("Page.enable", json!({})).is_err_and(|e| e.contains("no")));
    }

    #[test]
    fn what_comes_down_the_pipe_is_passed_on_until_it_ends() {
        struct Broken;
        impl Read for Broken { fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> { Err(std::io::Error::other("broken")) } }
        let (tx, rx) = channel();
        messages(Broken, tx);
        assert!(rx.recv().is_err());
        let (tx, rx) = channel();
        messages(&b""[..], tx);
        assert!(rx.recv().is_err());
        let (tx, rx) = channel();
        messages(&b"not json\0{\"a\":1}\0"[..], tx);
        assert_eq!(rx.recv().unwrap(), json!({ "a": 1 }));
        assert!(rx.recv().is_err());
        // (No one listening: it ends at the first message.)
        let (tx, rx) = channel();
        drop(rx);
        messages(&b"{}\0{}\0"[..], tx);
    }

    #[test]
    fn a_page_that_does_not_do_what_is_asked_says_so() {
        let Some(mut b) = Browser::open(800, 600) else { return };
        b.goto("data:text/html,<main>the words</main><div id=under style='position:fixed;left:0;top:50px;width:90px;height:90px'>under</div><div style='position:fixed;left:0;top:50px;width:90px;height:90px;background:red'>over</div>");
        let said = |p: Box<dyn std::any::Any + Send>| p.downcast_ref::<String>().cloned().unwrap_or_default();
        let e = catch_unwind(AssertUnwindSafe(|| b.wait_within("false", Duration::from_millis(1)))).unwrap_err();
        assert!(said(e).contains("the page shows: the words"));
        let e = catch_unwind(AssertUnwindSafe(|| b.eval("throw new Error('thrown')"))).unwrap_err();
        assert!(said(e).contains("thrown"));
        let e = catch_unwind(AssertUnwindSafe(|| b.click_within("nav", "", Duration::ZERO))).unwrap_err();
        assert!(said(e).contains("nothing to click"));
        let e = catch_unwind(AssertUnwindSafe(|| b.click_within("#under", "", Duration::from_millis(300)))).unwrap_err();
        assert!(said(e).contains("not clickable"));
        let shot = std::env::temp_dir().join(format!("superci-shot-{}.png", std::process::id()));
        b.screenshot(&shot);
        assert!(std::fs::read(&shot).unwrap().starts_with(b"\x89PNG"));
        let _ = std::fs::remove_file(&shot);
    }
}
