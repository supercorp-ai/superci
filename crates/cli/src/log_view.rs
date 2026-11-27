//! A job's log as the dashboard shows it: in steps, as GitHub shows a job's, each with how it went, how long it took
//! and its lines (their colours kept, a group's heading marked, errors and warnings told apart). From what the control
//! plane gives (`/job/log` with `steps`): GitHub's steps with their times and its log once the job has ended (each
//! line goes to the step it was written in, by its time); a run from a computer's steps, each with its log; GitLab's
//! log, whose sections are its steps.
use serde_json::{json, Value};

/// One line as the page draws it: its time (hh:mm:ss, when the log says), its kind (`g` a group's heading, `e` an
/// error, `w` a warning, `n` a notice, `c` a command, else nothing) and its text as HTML.
type Line = (String, &'static str, String);

struct Step { name: String, state: String, ms: Option<u64>, lines: Vec<Line> }

/// The page's view of a job's log: whether it is still going, its steps, and how many errors and warnings it has.
pub fn view(job: &Value, got: &Value, going: bool) -> Value {
    let log = got["log"].as_str().unwrap_or_default();
    let steps = match job["provider"].as_str().unwrap_or_default() {
        "local" => local(got),
        "gitlab" => gitlab(log, going, job["conclusion"].as_str().unwrap_or_default()),
        _ => github(got, log),
    };
    let count = |k: &str| steps.iter().map(|s| s.lines.iter().filter(|l| l.1 == k).count()).sum::<usize>();
    let (errors, warnings) = (count("e"), count("w"));
    let shaped = shaped(steps, got["shape"].as_array().map(Vec::as_slice).unwrap_or_default());
    json!({ "going": going, "truncated": got["truncated"], "error": got["error"], "errors": errors, "warnings": warnings,
        "steps": shaped.iter().map(|(s, shape)| {
            let mut v = json!({ "name": s.name, "state": s.state, "ms": s.ms, "lines": s.lines.iter().map(|(t, k, h)| json!([t, k, h])).collect::<Vec<_>>() });
            if let Some(o) = shape.as_object() { for (k, x) in o { v[k] = x.clone() } }
            v
        }).collect::<Vec<_>>() })
}

/// The steps with their shape (`bg`: runs in the background; `child`: a step of the parallel group just before it;
/// `group`: a parallel group's own row, with how many steps it has; `control`: `wait`, `wait-all` or `cancel`, with
/// the names of the steps it names), as GitHub shows them: a parallel group's row first, its steps under it. By their
/// order, from the steps the runner was handed (`Set up job` comes before them; post steps after).
fn shaped(steps: Vec<Step>, shape: &[Value]) -> Vec<(Step, Value)> {
    let from = usize::from(steps.first().is_some_and(|s| s.name == "Set up job"));
    if shape.is_empty() || steps.len() < from + shape.len() { return steps.into_iter().map(|s| (s, Value::Null)).collect() }
    let names: std::collections::HashMap<String, String> = shape.iter().zip(&steps[from..]).filter_map(|(sh, st)| Some((sh["id"].as_str()?.to_string(), st.name.clone()))).collect();
    let mut out: Vec<(Step, Value)> = vec![];
    let mut held: Vec<(Step, Value)> = vec![];
    // (There are steps here: at least as many as the shape has.)
    steps.into_iter().enumerate().for_each(|(i, step)| {
        if !(from..from + shape.len()).contains(&i) { out.push((step, Value::Null)); return }
        let sh = &shape[i - from];
        let mut v = json!({});
        if sh["bg"] == true { v["bg"] = true.into() }
        match (sh["control"].as_str(), sh["group"].as_str()) {
            // A parallel group's steps: held until its own row, which goes before them.
            (None, Some(_)) => { v["child"] = true.into(); held.push((step, v)); return }
            (Some(_), Some(_)) => {
                let kids: Vec<(Step, Value)> = std::mem::take(&mut held);
                let mut row = json!({ "group": kids.len() });
                if kids.iter().all(|(_, k)| k["bg"] == true) { row["bg"] = true.into() }
                out.push((step, row));
                out.extend(kids);
                return
            }
            (Some(c), None) => {
                v["control"] = c.into();
                v["targets"] = sh["targets"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(|t| names.get(t).cloned().unwrap_or_else(|| t.to_string())).collect::<Vec<_>>().into();
            }
            (None, None) => {}
        }
        // (A group's steps whose own row never came: as they are.)
        out.append(&mut held);
        out.push((step, v));
    });
    out.append(&mut held);
    out
}

/// A run from a computer: its steps as its runner said them, each with its log.
fn local(got: &Value) -> Vec<Step> {
    got["steps"].as_array().into_iter().flatten().map(|s| {
        let state = match (s["state"].as_str(), s["result"].as_str()) {
            (_, Some("succeeded")) => "success", (_, Some("failed")) => "failure", (_, Some("skipped")) => "skipped", (_, Some("cancelled" | "canceled")) => "cancelled",
            (Some("inProgress"), _) => "running", (Some("completed"), _) => "success", _ => "pending",
        };
        let ms = match (s["start"].as_str().and_then(seconds), s["finish"].as_str().and_then(seconds)) { (Some(a), Some(b)) => Some(b.saturating_sub(a) * 1000), _ => None };
        Step { name: s["name"].as_str().unwrap_or("Step").to_string(), state: state.into(), ms, lines: s["text"].as_str().unwrap_or_default().lines().map(line).collect() }
    }).collect()
}

/// Whether a job of GitHub's ran steps side by side (`background`, `parallel`): one began before an earlier one ended.
/// Its log then has their lines mixed, and only its run's logs, a file for each step, tell them apart.
pub fn side_by_side(steps: &Value) -> bool {
    let mut ended = 0;
    steps.as_array().into_iter().flatten().any(|s| {
        let (began, end) = (s["started_at"].as_str().and_then(seconds), s["completed_at"].as_str().and_then(seconds));
        let earlier = began.is_some_and(|b| b < ended);
        ended = ended.max(end.unwrap_or_default());
        earlier
    })
}

/// Each step's own log, by its number, from its run's logs (GitHub's zip: a folder for each job, named as the job is
/// with a slash made `_` and what else a file name cannot hold left out, as `build / x: "y"` is `build _ x y`; in it
/// `3_Run make.txt` for its third step). Nothing when the job's folder is not there.
pub fn step_logs(zip: &[u8], job: &str) -> Option<Value> {
    let folder: String = job.chars().filter(|c| !":*?\"<>|".contains(*c)).map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
    let mut out = serde_json::Map::new();
    for (name, data) in zip_files(zip).ok()? {
        let Some(file) = name.strip_prefix(&format!("{folder}/")) else { continue };
        let Some((n, _)) = file.split_once('_').filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) else { continue };
        out.insert(n.to_string(), String::from_utf8_lossy(&data).trim_start_matches('\u{feff}').to_string().into());
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// A zip's files, read in memory: each one's name and what it holds.
fn zip_files(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let u16at = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or("cut short");
    let u32at = |i: usize| data.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("cut short");
    // Its end record: in the last 64 KB (and 22 bytes), the last that says so.
    let end = (data.len().saturating_sub(65_557)..data.len().saturating_sub(21)).rev().find(|&i| data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]).ok_or("not a zip")?;
    // (The end record found is whole: 22 bytes from `end`.)
    let (count, mut at) = (u16::from_le_bytes([data[end + 10], data[end + 11]]) as usize, u32::from_le_bytes([data[end + 16], data[end + 17], data[end + 18], data[end + 19]]) as usize);
    let mut out = vec![];
    for _ in 0..count {
        if data.get(at..at + 4) != Some(&[0x50, 0x4b, 0x01, 0x02][..]) { return Err("a damaged directory".into()) }
        let (method, packed) = (u16at(at + 10)?, u32at(at + 20)?);
        let (n, e, c, local) = (u16at(at + 28)?, u16at(at + 30)?, u16at(at + 32)?, u32at(at + 42)?);
        let name = String::from_utf8_lossy(data.get(at + 46..at + 46 + n).ok_or("cut short")?).into_owned();
        at += 46 + n + e + c;
        let from = local + 30 + u16at(local + 26)? + u16at(local + 28)?;
        let raw = data.get(from..from + packed).ok_or("cut short")?;
        out.push((name, match method { 0 => raw.to_vec(), 8 => miniz_oxide::inflate::decompress_to_vec(raw).map_err(|e| format!("{e:?}"))?, m => return Err(format!("packed as {m}")) }));
    }
    Ok(out)
}

/// A job of GitHub's: its steps as GitHub has them, and each line of its log (once it has ended) in the step that was
/// going when it was written. A step begins at its time: a line of that very second goes to it only from the step's
/// heading on (GitHub's times are whole seconds). With each step's own log (`step_logs`, by its number): its lines.
fn github(got: &Value, log: &str) -> Vec<Step> {
    let given = got["steps"].as_array().cloned().unwrap_or_default();
    let secs = |v: &Value| v.as_str().and_then(seconds);
    let mut steps: Vec<(Step, Option<u64>)> = given.iter().map(|s| {
        let state = match (s["status"].as_str(), s["conclusion"].as_str()) {
            (Some("completed"), Some(c)) => match c { "success" => "success", "skipped" => "skipped", "cancelled" => "cancelled", _ => "failure" },
            (Some("in_progress"), _) => "running", _ => "pending",
        };
        let ms = match (secs(&s["started_at"]), secs(&s["completed_at"])) { (Some(a), Some(b)) => Some(b.saturating_sub(a) * 1000), _ => None };
        (Step { name: s["name"].as_str().unwrap_or("Step").to_string(), state: state.into(), ms, lines: vec![] }, secs(&s["started_at"]))
    }).collect();
    if steps.is_empty() {
        return if log.trim().is_empty() { vec![] } else { vec![Step { name: "Log".into(), state: "success".into(), ms: None, lines: log.lines().map(line).collect() }] }
    }
    if let Some(own) = got["step_logs"].as_object() {
        // (There are steps here: none were seen above.)
        steps.iter_mut().zip(&given).for_each(|(s, g)| {
            let text = g["number"].as_u64().and_then(|n| own.get(&n.to_string())).and_then(Value::as_str).unwrap_or_default();
            s.0.lines = text.lines().map(line).collect();
        });
        return steps.into_iter().map(|(s, _)| s).collect()
    }
    let mut at = 0;
    for raw in log.lines() {
        let when = raw.trim_start_matches('\u{feff}').get(..19).and_then(|t| seconds(&format!("{t}Z")));
        let l = line(raw);
        let plain = strip_controls(split_time(raw).1);
        // On to a later step: its second has passed, or it has come and this line begins that very step (its heading
        // names it; GitHub's own steps begin with lines of their own). (Steps of the same second are not skipped.)
        let begins = |name: &str| plain.strip_prefix("##[group]").is_some_and(|h| h == name)
            || (name == "Complete job" && plain.starts_with("Cleaning up orphan processes")) || (name.starts_with("Post ") && plain.starts_with("Post job cleanup."));
        while let (Some(t), Some((step, Some(next)))) = (when, steps.get(at + 1)) {
            if t > *next || (t == *next && begins(&step.name)) { at += 1 } else { break }
        }
        steps[at].0.lines.push(l);
    }
    steps.into_iter().map(|(s, _)| s).collect()
}

/// A job of GitLab's: its sections are its steps (what comes before the first, and after the last, are steps of their
/// own); a section within one is a group in it.
fn gitlab(log: &str, going: bool, conclusion: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = vec![Step { name: "Started".into(), state: "success".into(), ms: None, lines: vec![] }];
    let (mut depth, mut began, mut untitled) = (0usize, 0u64, false);
    for raw in log.lines() {
        let text = gitlab_text(raw);
        // A section's heading on the line after its start: its name.
        if untitled && !text.contains("section_") {
            let heading = strip_controls(text);
            if !heading.trim().is_empty() { steps.last_mut().unwrap().name = heading.trim().to_string(); untitled = false; continue }
        }
        if let Some(i) = text.find("section_start:") {
            let rest = &text[i + "section_start:".len()..];
            let (ts, rest) = rest.split_once(':').unwrap_or(("0", rest));
            let header = rest.split_once('\r').map(|(_, h)| h).unwrap_or("");
            let header = strip_controls(header);
            if depth == 0 {
                began = ts.parse().unwrap_or(0);
                untitled = header.trim().is_empty();
                let name = if untitled { rest.split(['[', '\r']).next().unwrap_or("Section").replace('_', " ") } else { header.trim().to_string() };
                steps.push(Step { name, state: if going { "running".into() } else { "success".into() }, ms: None, lines: vec![] });
            } else {
                steps.last_mut().unwrap().lines.push((String::new(), "g", ansi_html(&header)));
            }
            depth += 1;
            continue;
        }
        if let Some(i) = text.find("section_end:") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                let ended: u64 = text[i + "section_end:".len()..].split(':').next().and_then(|t| t.parse().ok()).unwrap_or(began);
                let s = steps.last_mut().unwrap();
                (s.ms, s.state) = (Some(ended.saturating_sub(began) * 1000), "success".into());
                steps.push(Step { name: "Ended".into(), state: "success".into(), ms: None, lines: vec![] });
            }
            continue;
        }
        steps.last_mut().unwrap().lines.push(line(raw));
    }
    // A step that never ended: still going, or where the job failed.
    let failed = !going && conclusion == "failure";
    if failed {
        if let Some(s) = steps.iter_mut().rev().find(|s| s.ms.is_some()) { s.state = "failure".into() }
        if let Some(last) = steps.iter_mut().rev().find(|s| !s.lines.is_empty()) { last.state = "failure".into() }
    }
    steps.retain(|s| !s.lines.is_empty() || s.ms.is_some());
    steps
}

/// A GitLab log line's text: without the time and stream GitLab's runner puts before it ("2026-…Z 00O+ ").
fn gitlab_text(raw: &str) -> &str {
    let (_, rest) = split_time(raw);
    let b = rest.as_bytes();
    if b.len() >= 4 && b[..2].iter().all(u8::is_ascii_digit) && (b[2] == b'O' || b[2] == b'E') {
        let skip = if b.get(3) == Some(&b'+') { 4 } else { 3 };
        return rest[skip..].strip_prefix(' ').unwrap_or(&rest[skip..])
    }
    rest
}

/// A line's time (when it begins with one, as GitHub's and GitLab's runners write) and the rest.
pub(crate) fn split_time(raw: &str) -> (Option<&str>, &str) {
    let r = raw.trim_start_matches('\u{feff}');
    let b = r.as_bytes();
    if b.len() > 20 && b[4] == b'-' && b[10] == b'T' && b[..4].iter().all(u8::is_ascii_digit) {
        if let Some(at) = r.find("Z ").filter(|at| *at < 40) { return (r.get(11..19), &r[at + 2..]) }
    }
    (None, r)
}

/// One line for the page.
fn line(raw: &str) -> Line {
    let (time, rest) = split_time(raw);
    let rest = gitlab_text(rest);
    let time = time.unwrap_or_default().to_string();
    const MARKS: [(&str, &str); 5] = [("##[group]", "g"), ("##[error]", "e"), ("##[warning]", "w"), ("##[notice]", "n"), ("[command]", "c")];
    if let Some((kind, text)) = MARKS.iter().find_map(|(mark, kind)| rest.strip_prefix(mark).map(|text| (*kind, text))) { return (time, kind, ansi_html(text)) }
    if rest.starts_with("##[endgroup]") { return (time, "x", String::new()) }
    let kind = if rest.starts_with("ERROR: ") { "e" } else if rest.starts_with("WARNING: ") { "w" } else { "" };
    (time, kind, ansi_html(rest))
}

/// Seconds since the epoch of a time GitHub writes ("2026-10-14T21:09:53Z", fractions allowed).
pub(crate) fn seconds(t: &str) -> Option<u64> {
    let b = t.as_bytes();
    if b.len() < 19 { return None }
    let n = |r: std::ops::Range<usize>| t.get(r).and_then(|s| s.parse::<i64>().ok());
    let (y, mo, d, h, mi, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y2, m2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// Control characters a line's text can carry that are not colours (GitLab's "erase line", carriage returns).
fn strip_controls(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') { chars.next(); for n in chars.by_ref() { if n.is_ascii_alphabetic() { break } } continue }
        if c != '\r' { out.push(c) }
    }
    out
}

/// A line's text as HTML: escaped, its colours (SGR: bold, the 16 colours) as spans, every other control left out.
pub fn ansi_html(s: &str) -> String {
    let (mut out, mut open) = (String::new(), false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut code = String::new();
            let mut end = ' ';
            for n in chars.by_ref() { if n.is_ascii_alphabetic() { end = n; break } code.push(n) }
            if end != 'm' { continue }
            if open { out.push_str("</span>"); open = false }
            let classes: Vec<String> = code.split(';').filter_map(|p| match p.parse::<u32>().ok()? { 1 => Some("ab".to_string()), n @ (30..=37 | 90..=97) => Some(format!("a{n}")), _ => None }).collect();
            if !classes.is_empty() { out.push_str(&format!(r#"<span class="{}">"#, classes.join(" "))); open = true }
            continue;
        }
        match c { '&' => out.push_str("&amp;"), '<' => out.push_str("&lt;"), '>' => out.push_str("&gt;"), '"' => out.push_str("&quot;"), '\r' => {} c if c.is_control() && c != '\t' => {} c => out.push(c) }
    }
    if open { out.push_str("</span>") }
    out
}

#[cfg(test)]
mod tests {

    /// A job with background steps, waits and a parallel group, shaped by the message GitHub handed its runner (as
    /// recorded): the group's row before its steps, each marked, waits naming the steps they wait for by name.
    #[test]
    fn steps_run_side_by_side_are_shown_as_github_shows_them() {
        let message: Value = serde_json::from_str(include_str!("../../core/tests/fixtures/runner/parallel.github.json")).unwrap();
        let shape = superci_core::plane::step_shape(&message);
        let names = ["Set up job", "Start a server", "Run echo two; sleep 2", "Run echo three", "Run echo while they run", "Wait for two", "Wait", "Stop the server", "Wait for all",
            "First", "Run echo second", "Run actions/checkout@v4", "Parallel group", "Run echo after", "What the runner was handed", "Post Run actions/checkout@v4", "Complete job"];
        let steps: Vec<Value> = names.iter().enumerate().map(|(i, n)| json!({ "name": n, "number": i + 1, "status": "completed", "conclusion": "success" })).collect();
        let v = view(&json!({ "provider": "" }), &json!({ "steps": steps, "shape": shape, "log": "" }), false);
        let rows: Vec<String> = v["steps"].as_array().unwrap().iter().map(|s| {
            let mut r = s["name"].as_str().unwrap().to_string();
            if s["bg"] == true { r += " [bg]" }
            if s["child"] == true { r = format!("  {r}") }
            if let Some(n) = s["group"].as_u64() { r += &format!(" [parallel, {n}]") }
            if let Some(c) = s["control"].as_str() { r += &format!(" [{c}: {}]", s["targets"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect::<Vec<_>>().join(", ")) }
            r
        }).collect();
        assert_eq!(rows, ["Set up job", "Start a server [bg]", "Run echo two; sleep 2 [bg]", "Run echo three [bg]", "Run echo while they run", "Wait for two [wait: Run echo two; sleep 2]",
            "Wait [wait: Run echo two; sleep 2]", "Stop the server [cancel: Start a server]", "Wait for all [wait-all: ]", "Parallel group [bg] [parallel, 3]", "  First [bg]", "  Run echo second [bg]",
            "  Run actions/checkout@v4 [bg]", "Run echo after", "What the runner was handed", "Post Run actions/checkout@v4", "Complete job"]);
        // Without a shape (a job from before, a machine that could not say it): as they are.
        let plain = view(&json!({ "provider": "" }), &json!({ "steps": steps, "log": "" }), false);
        assert_eq!(plain["steps"][12]["name"], "Parallel group");
        assert!(plain["steps"][12]["group"].is_null());
    }

    use super::*;

    /// A shape's step with no id is named by none, and a wait naming a step no shape has says the name as given.
    #[test]
    fn a_wait_for_a_step_not_in_the_shape_says_the_name_as_given() {
        let step = |name: &str| Step { name: name.into(), state: "success".into(), ms: None, lines: vec![] };
        let shaped = shaped(vec![step("Set up job"), step("Wait"), step("Run echo")], &[json!({ "control": "wait", "targets": ["nope", "x"] }), json!({ "id": "x" })]);
        assert_eq!(shaped.iter().map(|(s, v)| (s.name.as_str(), v.clone())).collect::<Vec<_>>(), [
            ("Set up job", Value::Null), ("Wait", json!({ "control": "wait", "targets": ["nope", "Run echo"] })), ("Run echo", json!({})),
        ]);
    }

    /// A zip whose directory or file header is cut short at each field read is refused as cut short.
    #[test]
    fn a_zip_cut_short_at_any_field_is_refused() {
        // Its end record (one entry, its directory at 22), then a directory entry: nothing packed, a file header at `local`.
        let zip = |len: usize, local: u32| {
            let mut z = vec![0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 1, 0, 1, 0, 46, 0, 0, 0, 22, 0, 0, 0, 0, 0];
            let mut entry = vec![0u8; 46];
            entry[..4].copy_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            entry[42..46].copy_from_slice(&local.to_le_bytes());
            z.extend(entry);
            z.truncate(len);
            z
        };
        // Cut in its method, size, name's, extra's and comment's lengths, and where its file is; then where that file's
        // name's and extra's lengths would be.
        for len in [33, 45, 51, 53, 55, 67] { assert_eq!(zip_files(&zip(len, 0)), Err("cut short".to_string()), "{len}") }
        assert_eq!(zip_files(&zip(68, 1000)), Err("cut short".to_string()));
        assert_eq!(zip_files(&zip(68, 40)), Err("cut short".to_string()));
    }

    /// A time with its day, hour, minute or second not a number is none.
    #[test]
    fn a_time_with_a_part_not_a_number_is_none() {
        assert_eq!(seconds("2026-10-14T21:09:53Z"), Some(1_792_012_193));
        for t in ["2026-10-xxT21:09:53Z", "2026-10-14Txx:09:53Z", "2026-10-14T21:xx:53Z", "2026-10-14T21:09:xxZ"] { assert_eq!(seconds(t), None, "{t}") }
    }

    #[test]
    fn a_lines_time_and_gitlabs_stream_mark_are_read_only_where_they_are() {
        assert_eq!(split_time("2026-10-16T10:00:01.0000000Z hello"), (Some("10:00:01"), "hello"));
        // Not a time: too short, no dash or T where they go, not digits, or no Z soon after.
        for raw in ["2026-10-16T10:00Z x", "2026x10-16T10:00:01.0000000Z x", "2026-10-16x10:00:01.0000000Z x", "abcd-10-16T10:00:01.0000000Z x", "2026-10-16T10:00:01.0000000000000000000000000000000Z x"] {
            assert_eq!(split_time(raw), (None, raw), "{raw}");
        }
        assert_eq!(gitlab_text("2026-10-16T10:00:01.0000000Z 00O+ text"), "text");
        assert_eq!(gitlab_text("2026-10-16T10:00:01.0000000Z 01E text"), "text");
        for raw in ["00X text", "0aO text", "00"] { assert_eq!(gitlab_text(raw), raw) }
    }

    #[test]
    fn githubs_lines_go_to_the_step_they_were_written_in() {
        let got = json!({ "steps": [
            { "name": "Set up job", "status": "completed", "conclusion": "success", "started_at": "2026-10-14T21:09:50Z", "completed_at": "2026-10-14T21:09:52Z" },
            { "name": "Run npm test", "status": "completed", "conclusion": "failure", "started_at": "2026-10-14T21:09:52Z", "completed_at": "2026-10-14T21:10:30Z" },
            { "name": "Complete job", "status": "completed", "conclusion": "success", "started_at": "2026-10-14T21:10:30Z", "completed_at": "2026-10-14T21:10:31Z" }],
            "log": "\u{feff}2026-10-14T21:09:50.1000000Z Current runner version: '2.337.0'\n2026-10-14T21:09:52.0100000Z Prepare all required actions\n2026-10-14T21:09:52.2000000Z ##[group]Run npm test\n2026-10-14T21:09:59.0000000Z \u{1b}[31mFAIL\u{1b}[0m a <b>.test.js\n2026-10-14T21:10:29.0000000Z ##[error]Process completed with exit code 1.\n2026-10-14T21:10:30.5000000Z Cleaning up orphan processes" });
        let v = view(&json!({ "provider": "" }), &got, false);
        let steps = v["steps"].as_array().unwrap();
        let lines = |i: usize| steps[i]["lines"].as_array().unwrap().iter().map(|l| l[2].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(lines(0), ["Current runner version: '2.337.0'", "Prepare all required actions"], "a line of the next step's second before its heading stays");
        assert_eq!(lines(1), ["Run npm test", r#"<span class="a31">FAIL</span> a &lt;b&gt;.test.js"#, "Process completed with exit code 1."]);
        assert_eq!((steps[1]["state"].as_str(), steps[1]["ms"].as_u64(), steps[1]["lines"][0][1].as_str(), steps[1]["lines"][0][0].as_str()), (Some("failure"), Some(38_000), Some("g"), Some("21:09:52")));
        assert_eq!(lines(2), ["Cleaning up orphan processes"]);
        assert_eq!((v["errors"].as_u64(), v["warnings"].as_u64()), (Some(1), Some(0)));
        // Steps of the same second (a set-up of under a second, a step that ends as the next begins): each line in its own.
        let got = json!({ "steps": [
            { "name": "Set up job", "status": "completed", "conclusion": "success", "started_at": "2026-10-15T06:51:05Z", "completed_at": "2026-10-15T06:51:05Z" },
            { "name": "Run make", "status": "completed", "conclusion": "success", "started_at": "2026-10-15T06:51:05Z", "completed_at": "2026-10-15T06:51:46Z" },
            { "name": "Run echo \"after\"", "status": "completed", "conclusion": "success", "started_at": "2026-10-15T06:51:46Z", "completed_at": "2026-10-15T06:51:46Z" },
            { "name": "Complete job", "status": "completed", "conclusion": "success", "started_at": "2026-10-15T06:51:46Z", "completed_at": "2026-10-15T06:51:46Z" }],
            "log": "2026-10-15T06:51:05.1Z Current runner version: '2.337.0'\n2026-10-15T06:51:05.5Z ##[group]Run make\n2026-10-15T06:51:45.9Z done\n2026-10-15T06:51:46.1Z ##[group]Run echo \"after\"\n2026-10-15T06:51:46.2Z after\n2026-10-15T06:51:46.4Z Cleaning up orphan processes" });
        let v = view(&json!({ "provider": "" }), &got, false);
        let counts: Vec<usize> = v["steps"].as_array().unwrap().iter().map(|s| s["lines"].as_array().unwrap().len()).collect();
        assert_eq!(counts, [1, 2, 2, 1], "{v}");
    }

    #[test]
    fn steps_run_side_by_side_get_their_own_lines_from_their_runs_logs() {
        // GitHub's steps and run logs for a job with background steps, waits and a parallel group (a probe on GitHub;
        // its checkout's lines cut, as they name the repository), and the folders it makes for `build / x: "y" (a)`.
        let steps: Value = serde_json::from_str(include_str!("../fixtures/github/parallel-steps.json")).unwrap();
        let zip = include_bytes!("../fixtures/github/run-logs.zip");
        assert!(side_by_side(&steps));
        assert!(!side_by_side(&json!([{ "started_at": "2026-10-15T22:00:50Z", "completed_at": "2026-10-15T22:00:51Z" }, { "started_at": "2026-10-15T22:00:51Z", "completed_at": "2026-10-15T22:00:53Z" }])), "one after another");
        let own = step_logs(zip, "test").unwrap();
        let v = view(&json!({ "provider": "" }), &json!({ "steps": steps, "step_logs": own, "log": "" }), false);
        let of = |name: &str| -> Vec<String> {
            let s = v["steps"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("{name}: {v}"));
            s["lines"].as_array().unwrap().iter().map(|l| l[2].as_str().unwrap().to_string()).filter(|l| !l.contains("shell:") && !l.contains("36;1m")).collect()
        };
        // In the job's log "two" comes after "Run echo while they run"'s heading, and "first" inside the checkout's.
        assert_eq!(of("Run echo two; sleep 2").last().map(String::as_str), Some("two"));
        assert_eq!(of("Run echo while they run").last().map(String::as_str), Some("while they run"));
        assert_eq!(of("First").last().map(String::as_str), Some("first"));
        assert!(of("Parallel group").iter().any(|l| l.contains("Finished waiting for background step(s).")));
        assert_eq!(v["steps"][0]["lines"].as_array().map(Vec::len), Some(0), "a step whose file is not there has none");
        // A job's folder, named as GitHub names it.
        let b = step_logs(zip, "build / x: \"y\" (b)").unwrap();
        assert_eq!(b.as_object().map(|o| o.len()), Some(3));
        assert!(b["3"].as_str().unwrap().contains("second"));
        assert!(step_logs(zip, "nowhere").is_none() && step_logs(b"not a zip", "test").is_none());
    }

    #[test]
    fn gitlabs_sections_are_its_steps() {
        let log = "2026-10-14T15:22:39.414647Z 00O Running with gitlab-runner 19.4.1\n\
                   2026-10-14T15:22:40.000000Z 00O \u{1b}[0Ksection_start:1760455360:get_sources\r\u{1b}[0K\u{1b}[36;1mGetting source from Git repository\u{1b}[0;m\n\
                   2026-10-14T15:22:41.000000Z 00O Fetching changes...\n\
                   2026-10-14T15:22:42.000000Z 00O \u{1b}[0Ksection_end:1760455363:get_sources\r\u{1b}[0K\n\
                   2026-10-14T15:22:43.000000Z 00O \u{1b}[0Ksection_start:1760455363:step_script\r\u{1b}[0K\n\
                   2026-10-14T15:22:43.100000Z 00O \u{1b}[0K\u{1b}[36;1mExecuting \"step_script\" stage\u{1b}[0;m\n\
                   2026-10-14T15:22:44.000000Z 01E ERROR: something broke\n\
                   2026-10-14T15:22:45.000000Z 00O \u{1b}[0Ksection_end:1760455370:step_script\r\u{1b}[0K\n\
                   2026-10-14T15:22:46.000000Z 00O ERROR: Job failed: exit code 1";
        let v = view(&json!({ "provider": "gitlab", "conclusion": "failure" }), &json!({ "log": log }), false);
        let names: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Started", "Getting source from Git repository", "Executing \"step_script\" stage", "Ended"]);
        assert_eq!((v["steps"][1]["ms"].as_u64(), v["steps"][2]["ms"].as_u64(), v["steps"][1]["lines"][0][2].as_str()), (Some(3000), Some(7000), Some("Fetching changes...")));
        assert_eq!((v["steps"][2]["state"].as_str(), v["steps"][3]["state"].as_str(), v["steps"][1]["state"].as_str()), (Some("failure"), Some("failure"), Some("success")), "where it failed, and its end");
        assert_eq!(v["errors"].as_u64(), Some(2));
    }

    #[test]
    fn a_run_from_a_computers_steps_each_with_its_log() {
        let got = json!({ "steps": [{ "name": "Run make", "state": "completed", "result": "succeeded", "text": "2026-10-14T21:09:53.1Z ok" }, { "name": "Run deploy", "state": "inProgress", "result": null, "text": "going" }] });
        let v = view(&json!({ "provider": "local" }), &got, true);
        assert_eq!((v["steps"][0]["state"].as_str(), v["steps"][0]["lines"][0][2].as_str(), v["steps"][1]["state"].as_str()), (Some("success"), Some("ok"), Some("running")));
    }

    #[test]
    fn every_state_a_step_can_be_in_is_shown_as_github_shows_it() {
        let local: Vec<Value> = [(None, Some("succeeded")), (None, Some("failed")), (None, Some("skipped")), (None, Some("cancelled")), (None, Some("canceled")), (Some("inProgress"), None), (Some("completed"), None), (Some("pending"), None)]
            .iter().map(|(state, result)| json!({ "state": state, "result": result, "start": "2026-10-14T21:09:50Z", "finish": "2026-10-14T21:09:53Z" })).collect();
        let v = view(&json!({ "provider": "local" }), &json!({ "steps": local }), false);
        let states: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap()).collect();
        assert_eq!(states, ["success", "failure", "skipped", "cancelled", "cancelled", "running", "success", "pending"]);
        assert_eq!((v["steps"][0]["ms"].as_u64(), v["steps"][0]["name"].as_str()), (Some(3000), Some("Step")));
        let github: Vec<Value> = [("completed", "success"), ("completed", "skipped"), ("completed", "cancelled"), ("completed", "failure"), ("in_progress", ""), ("queued", "")]
            .iter().map(|(status, conclusion)| json!({ "status": status, "conclusion": if conclusion.is_empty() { Value::Null } else { json!(conclusion) } })).collect();
        let v = view(&json!({ "provider": "" }), &json!({ "steps": github, "log": "" }), true);
        let states: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap()).collect();
        assert_eq!(states, ["success", "skipped", "cancelled", "failure", "running", "pending"]);
    }

    #[test]
    fn a_job_of_githubs_without_steps_shows_its_log_whole_or_nothing() {
        assert_eq!(view(&json!({}), &json!({ "log": " \n" }), false)["steps"], json!([]));
        let v = view(&json!({}), &json!({ "log": "a\n##[warning]b\n##[endgroup]\n[command]c\n##[notice]d\nWARNING: e" }), false);
        assert_eq!(v["steps"][0]["name"], "Log");
        let kinds: Vec<&str> = v["steps"][0]["lines"].as_array().unwrap().iter().map(|l| l[1].as_str().unwrap()).collect();
        assert_eq!((kinds, v["warnings"].as_u64()), (vec!["", "w", "x", "c", "n", "w"], Some(2)));
        // A post step begins at its own cleanup line; a line seconds after the next step began goes to it.
        let got = json!({ "steps": [
            { "name": "Run a", "started_at": "2026-10-14T21:09:50Z" }, { "name": "Post Run a", "started_at": "2026-10-14T21:09:52Z" }, { "name": "Run b", "started_at": "2026-10-14T21:09:54Z" }],
            "log": "2026-10-14T21:09:52.1Z still a\n2026-10-14T21:09:52.2Z Post job cleanup.\nno time\n2026-10-14T21:09:59.0Z long after" });
        let v = view(&json!({}), &got, false);
        let counts: Vec<usize> = v["steps"].as_array().unwrap().iter().map(|s| s["lines"].as_array().unwrap().len()).collect();
        assert_eq!(counts, [1, 2, 1]);
    }

    #[test]
    fn gitlabs_sections_within_sections_and_headings_on_their_own_line_are_read() {
        let log = "before\n\
                   \u{1b}[0Ksection_start:100:outer\r\u{1b}[0KOuter\n\
                   \u{1b}[0Ksection_start:101:inner\r\u{1b}[0KInner\n\
                   inside\n\
                   \u{1b}[0Ksection_end:102:inner\r\u{1b}[0K\n\
                   \u{1b}[0Ksection_end:105:outer\r\u{1b}[0K\n\
                   section_start:200:bare_name\r\n\
                   \u{1b}[0K\n\
                   section_start:201:nested\r\n\
                   section_end:x:nested\r\n\
                   section_end:bad\r\n\
                   section_end:300:extra\r\n\
                   trailing";
        let v = view(&json!({ "provider": "gitlab", "conclusion": "success" }), &json!({ "log": log }), false);
        let names: Vec<(&str, Option<u64>)> = v["steps"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap(), s["ms"].as_u64())).collect();
        // (A section with no heading is named by its id; one whose end has no time ended as it began; an end with no
        // section open ends the step after the last.)
        assert_eq!(names, [("Started", None), ("Outer", Some(5000)), ("bare name", Some(0)), ("Ended", Some(100_000))]);
        assert_eq!(v["steps"][1]["lines"][0], json!(["", "g", "Inner"]));
        // A heading on the line after its section's start names it.
        let v = view(&json!({ "provider": "gitlab" }), &json!({ "log": "section_start:1:x\r\nHeading\nline" }), false);
        assert_eq!((v["steps"][0]["name"].as_str(), v["steps"][0]["lines"][0][2].as_str()), (Some("Heading"), Some("line")));
        // Still going: the section not ended is running, and nothing is a failure.
        let v = view(&json!({ "provider": "gitlab", "conclusion": "failure" }), &json!({ "log": "section_start:1:a\r\u{1b}[0KA\nline" }), true);
        assert_eq!((v["steps"][0]["name"].as_str(), v["steps"][0]["state"].as_str()), (Some("A"), Some("running")));
        // Failed with nothing written: no step to mark.
        assert_eq!(view(&json!({ "provider": "gitlab", "conclusion": "failure" }), &json!({ "log": "" }), false)["steps"], json!([]));
    }

    #[test]
    fn times_and_colours_are_read_only_where_they_are_whole() {
        assert_eq!((seconds("2026-01-15T00:00:00Z"), seconds("2026-02-01T00:00:00Z")), (Some(1_768_435_200), Some(1_769_904_000)));
        for t in ["2026-10-14", "2026-1x-14T21:09:53Z", "x026-10-14T21:09:53Z", "2026-10-14T21:09:5xZ", "1969-12-31T23:59:59Z"] { assert_eq!(seconds(t), None, "{t}") }
        assert_eq!(strip_controls("a\r\u{1b}x\u{1b}[2Kb\u{1b}["), "a\u{1b}xb");
        for (raw, html) in [
            ("\u{1b}[1;31mX\u{1b}[0m", r#"<span class="ab a31">X</span>"#), ("\u{1b}[92mopen", r#"<span class="a92">open</span>"#), ("\u{1b}[2Kmoved\u{1b}[", "moved"),
            ("\u{1b}x&<>\"\r\u{7}\t", "x&amp;&lt;&gt;&quot;\t"), ("\u{1b}[44;;mplain", "plain"), ("", ""),
        ] {
            assert_eq!(ansi_html(raw), html, "{raw:?}");
        }
    }

    /// A zip of stored files, each with its packing method as written.
    fn zip(files: &[(&str, &[u8], u16)]) -> Vec<u8> {
        let (mut data, mut dir) = (vec![], vec![]);
        for (name, body, method) in files {
            let at = data.len() as u32;
            data.extend([0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0]);
            data.extend(method.to_le_bytes());
            data.extend([0; 8]);
            data.extend((body.len() as u32).to_le_bytes());
            data.extend((body.len() as u32).to_le_bytes());
            data.extend((name.len() as u16).to_le_bytes());
            data.extend([0, 0]);
            data.extend(name.as_bytes());
            data.extend(*body);
            dir.extend([0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0]);
            dir.extend(method.to_le_bytes());
            dir.extend([0; 8]);
            dir.extend((body.len() as u32).to_le_bytes());
            dir.extend((body.len() as u32).to_le_bytes());
            dir.extend((name.len() as u16).to_le_bytes());
            dir.extend([0; 12]);
            dir.extend(at.to_le_bytes());
            dir.extend(name.as_bytes());
        }
        let (dir_at, dir_len) = (data.len() as u32, dir.len() as u32);
        data.extend(dir);
        data.extend([0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        data.extend((files.len() as u16).to_le_bytes());
        data.extend((files.len() as u16).to_le_bytes());
        data.extend(dir_len.to_le_bytes());
        data.extend(dir_at.to_le_bytes());
        data.extend([0, 0]);
        data
    }

    #[test]
    fn a_zips_files_are_read_and_a_damaged_one_says_where() {
        let good = zip(&[("job_x/1_Set up.txt", b"\xef\xbb\xbfone", 0), ("job_x/notes.txt", b"n", 0), ("job_x/_x.txt", b"n", 0), ("other/2_x.txt", b"two", 0)]);
        assert_eq!(step_logs(&good, "job\\x"), Some(json!({ "1": "one" })));
        assert_eq!(step_logs(&good, "job/x"), Some(json!({ "1": "one" })), "a slash either way is the folder's _");
        assert_eq!(step_logs(&zip(&[]), "job"), None);
        assert_eq!(zip_files(&zip(&[("a", b"x", 12)])).unwrap_err(), "packed as 12");
        assert!(zip_files(&zip(&[("a", b"\xff\xff", 8)])).is_err());
        let mut damaged = good.clone();
        let at = good.windows(4).position(|w| w == [0x50, 0x4b, 0x01, 0x02]).unwrap();
        damaged[at] = 0;
        assert_eq!(zip_files(&damaged).unwrap_err(), "a damaged directory");
        // Its directory said to be where the data ends, or a file's name or body beyond it: cut short.
        let one = zip(&[("a", b"x", 0)]);
        let (end, dir) = (one.len() - 22, one.windows(4).position(|w| w == [0x50, 0x4b, 0x01, 0x02]).unwrap());
        let mut beyond = one.clone();
        beyond[end + 16..end + 20].copy_from_slice(&(end as u32 - 2).to_le_bytes());
        assert_eq!(zip_files(&beyond).unwrap_err(), "a damaged directory");
        let mut long_name = one.clone();
        long_name[dir + 28..dir + 30].copy_from_slice(&500u16.to_le_bytes());
        assert_eq!(zip_files(&long_name).unwrap_err(), "cut short");
        let mut long_body = one.clone();
        long_body[dir + 20..dir + 24].copy_from_slice(&500u32.to_le_bytes());
        assert_eq!(zip_files(&long_body).unwrap_err(), "cut short");
        assert_eq!(zip_files(b"PK\x05\x06").unwrap_err(), "not a zip");
    }
}
