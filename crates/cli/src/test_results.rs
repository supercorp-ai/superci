//! A job's tests, read from its log: what each common test runner says of the tests that failed (their names, files
//! and errors) and of how many passed, failed and were skipped. For `superci runs retrieve --tests` and `superci jobs
//! retrieve --tests`, so that a coding agent learns what failed without reading megabytes of log.
//!
//! Each runner is known by its own lines: Vitest (` FAIL  file > suite > name`, ` Tests  2 failed | 9 passed`), Jest
//! (`● suite › name`, `Tests:  2 failed, 9 passed, 11 total`), Playwright (`1) [chromium] › file:12:5 › name`,
//! `2 failed`), pytest (`FAILED file::name - error`, `== 2 failed, 9 passed in 1.2s ==`), cargo
//! (`test name ... FAILED`, `test result: FAILED. 9 passed; 2 failed; 1 ignored`) and Go (`--- FAIL: TestName`).
use serde::Serialize;

#[derive(Serialize, Debug, Default, Clone, PartialEq)]
pub struct Tests {
    /// The test runners found in the log.
    pub runners: Vec<&'static str>,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Of those passed: tests marked to fail that failed (`test.fail()`), and tests that passed when run again.
    #[serde(skip_serializing_if = "is_zero")]
    pub failed_as_expected: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub flaky: u64,
    pub failures: Vec<Failure>,
    /// What a runner said of the tests as a whole that a count does not: that none matched what was asked, or that
    /// it ran none (a `file:line` that no longer points at a test, with no tests found allowed).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Each test that ran and how it came out, where the runner lists them (Playwright's list): the first `MOST`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ran: Vec<Ran>,
    /// The tests that failed as they were marked to (the first `MOST`, however many ran): their runner passes them
    /// and prints no error for them, so each is named.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_as_expected_tests: Vec<Ran>,
    /// Each process's own counts, where lines are marked by the process they came from (`[slot 2] …`) and more than
    /// one ran tests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_process: Vec<Process>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Process {
    pub mark: String,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub failed_as_expected: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub flaky: u64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Ran {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// "passed", "failed", "skipped", "flaky" (failed, then passed when run again), or "failed as expected" (a test
    /// marked to fail, as `test.fail()`, that failed: a pass, though its runner marks it ✘).
    pub outcome: &'static str,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Failure {
    pub runner: &'static str,
    /// The test, as its runner names it (suite and name).
    pub name: String,
    /// Its file (and line), when the runner says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// What it failed with: the first lines its runner printed of it.
    pub error: String,
}

fn is_zero(n: &u64) -> bool { *n == 0 }

/// At most this many failures are kept (their error at most `ERROR_LINES` lines, `ERROR_CHARS` characters).
const MOST: usize = 200;
const ERROR_LINES: usize = 40;
const ERROR_CHARS: usize = 6000;

/// A log line as text: without its time (GitHub's), GitLab's stream mark, colour codes and carriage returns.
fn plain(raw: &str) -> String {
    let r = crate::log_view::split_time(raw).1;
    let b = r.as_bytes();
    let r = if b.len() >= 4 && b[..2].iter().all(u8::is_ascii_digit) && (b[2] == b'O' || b[2] == b'E') && (b[3] == b' ' || b[3] == b'+') { r[3..].trim_start_matches('+').trim_start_matches(' ') } else { r };
    let mut out = String::with_capacity(r.len());
    let mut chars = r.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') { chars.next(); for n in chars.by_ref() { if n.is_ascii_alphabetic() { break } } continue }
        if c != '\r' { out.push(c) }
    }
    out.trim_end().to_string()
}

/// The number before `word` in a summary (`2 failed`), or 0.
fn count(text: &str, word: &str) -> u64 {
    let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || c == ',' || c == '|' || c == ';' || c == '(' || c == ')').filter(|w| !w.is_empty()).collect();
    words.windows(2).filter(|w| w[1].trim_end_matches('.') == word).filter_map(|w| w[0].parse::<u64>().ok()).sum()
}

/// The lines after `from` that tell of a failure, up to where the next one (or the end of the list) begins.
fn error_after(lines: &[String], from: usize, stop: impl Fn(&str) -> bool) -> String {
    let mut out: Vec<&str> = vec![];
    for l in lines.iter().skip(from) {
        if stop(l) { break }
        let t = l.trim();
        if t.is_empty() && out.is_empty() { continue }
        if t.is_empty() && out.last().is_some_and(|p| p.trim().is_empty()) { break }
        out.push(l.as_str());
        if out.len() == ERROR_LINES { break }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) { out.pop(); }
    let indent = out.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let text = out.iter().map(|l| l.get(indent..).unwrap_or(l.trim_start())).collect::<Vec<_>>().join("\n");
    text.chars().take(ERROR_CHARS).collect()
}

/// A line's mark of the process it came from, when several run at once and each line says which (`[slot 2] …`,
/// `[web] …`), and the line without it. (Not Playwright's own `[chromium] › file`.)
pub fn marked(line: &str) -> (&str, &str) {
    let s = line.trim_start();
    if let Some(end) = s.strip_prefix('[').and_then(|r| r.find("] ")).map(|at| at + 1) {
        let (mark, rest) = (&s[..=end], &s[end + 2..]);
        if !rest.starts_with('›') && mark.len() <= 40 && !mark[1..end].contains(['[', ']']) { return (mark, rest) }
    }
    ("", line)
}

/// What a job's log says of its tests (nothing found: no runners, no counts). Lines marked by the process they came
/// from (`[slot 2] …`) are read as that process's own, apart from the others' they are mixed with, and added up.
pub fn read(log: &str) -> Tests {
    let mut streams: Vec<(String, Vec<String>)> = vec![];
    for l in log.lines() {
        let line = plain(l);
        let (mark, rest) = marked(&line);
        match streams.iter_mut().find(|(m, _)| m == mark) {
            Some((_, lines)) => lines.push(rest.to_string()),
            None => streams.push((mark.to_string(), vec![rest.to_string()])),
        }
    }
    let mut t = Tests::default();
    for (mark, lines) in streams {
        let mut one = read_lines(&lines);
        // A runner that ran nothing, and did not say why: said (the run may have passed without its tests).
        if !one.runners.is_empty() && one.notes.is_empty() && one.passed + one.failed + one.skipped == 0 {
            one.notes.push(format!("{} ran no tests.", one.runners.iter().map(|r| runner_name(r)).collect::<Vec<_>>().join(", ")));
        }
        for r in one.runners.clone() { if !t.runners.contains(&r) { t.runners.push(r) } }
        (t.passed, t.failed, t.skipped) = (t.passed + one.passed, t.failed + one.failed, t.skipped + one.skipped);
        for f in one.failures { if t.failures.len() < MOST { t.failures.push(f) } }
        for r in one.ran { if t.ran.len() < MOST { t.ran.push(r) } }
        for r in one.failed_as_expected_tests { if t.failed_as_expected_tests.len() < MOST { t.failed_as_expected_tests.push(r) } }
        (t.failed_as_expected, t.flaky) = (t.failed_as_expected + one.failed_as_expected, t.flaky + one.flaky);
        if !mark.is_empty() && !one.runners.is_empty() {
            t.by_process.push(Process { mark: mark.clone(), passed: one.passed, failed: one.failed, skipped: one.skipped, failed_as_expected: one.failed_as_expected, flaky: one.flaky });
        }
        // (Which process said it, where lines are marked so.)
        for n in one.notes { let n = if mark.is_empty() { n } else { format!("{mark} {n}") }; t.notes.push(n) }
    }
    // (One process alone: its counts are the whole's.)
    if t.by_process.len() < 2 { t.by_process.clear() }
    t
}

/// Each test runner read here, as people call it.
const RUNNER_NAMES: [(&str, &str); 6] = [("vitest", "Vitest"), ("jest", "Jest"), ("playwright", "Playwright"), ("pytest", "pytest"), ("cargo", "cargo test"), ("go", "go test")];

fn runner_name(r: &str) -> &'static str { RUNNER_NAMES.iter().find(|(id, _)| *id == r).map_or("A test runner", |(_, name)| *name) }

/// A line of Playwright's list of tests as they end (`  ✓  3 [chromium] › e2e/a.spec.ts:4:3 › logs in (2.1s)`): its
/// mark, the test's name (with its project) and its file.
fn listed(s: &str) -> Option<(char, String, String, bool)> {
    let mark = s.chars().next().filter(|c| ['✓', '✘', '-'].contains(c))?;
    let rest = s[mark.len_utf8()..].trim_start();
    let rest = rest.split_once(' ').filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit())).map(|(_, r)| r.trim_start()).unwrap_or(rest);
    let parts: Vec<&str> = rest.split(" › ").collect();
    let (project, file, title) = match parts.as_slice() {
        [p, f, t @ ..] if p.starts_with('[') && p.ends_with(']') && !t.is_empty() => (Some(&p[1..p.len() - 1]), *f, t.join(" › ")),
        [f, t @ ..] if !t.is_empty() && f.contains(':') => (None, *f, t.join(" › ")),
        _ => return None,
    };
    if !file.chars().last().is_some_and(|c| c.is_ascii_digit()) { return None }
    // (Without its time and a retry's mark, as Playwright adds them: "(2.1s)", "(345ms)", "(1.2m)", "(retry #1)".
    // Nothing else in brackets: "(old layout)" is the test's own title.)
    let mut title = title.trim().to_string();
    let mut retry = false;
    let added = |inner: &str| {
        let num = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.') && s.bytes().next().is_some_and(|b| b.is_ascii_digit());
        inner.strip_prefix("retry #").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            || ["ms", "s", "m", "h"].iter().any(|u| inner.strip_suffix(u).is_some_and(num))
    };
    // One taken off the end; whether there was one.
    let mut take_one = |title: &mut String| {
        let Some(open) = title.rfind(" (").filter(|_| title.ends_with(')')) else { return false };
        let inner = &title[open + 2..title.len() - 1];
        if !added(inner) { return false }
        retry |= inner.starts_with("retry #");
        title.truncate(open);
        true
    };
    // (Two at most: a time, and a retry's mark before it.)
    if take_one(&mut title) { take_one(&mut title); }
    Some((mark, match project { Some(p) => format!("[{p}] {title}"), None => title }, file.trim().to_string(), retry))
}

/// A runner's word that it ran no test because none matched (its runner, and the note), from one of its lines.
fn none_matched(s: &str) -> Option<(&'static str, &'static str)> {
    if s.starts_with("Error: No tests found") { return Some(("playwright", "Playwright found no test to run: a `file:line` must be the first line of a test (or of a describe), and a name given must match one.")) }
    if s.starts_with("No test files found, exiting with code") || s.starts_with("No test files found.") { return Some(("vitest", "Vitest found no test file matching what was asked.")) }
    if s.starts_with("No test suite found in file") { return Some(("vitest", "Vitest found a file with no tests in it.")) }
    if s.starts_with("No tests found, exiting with code") { return Some(("jest", "Jest found no test matching what was asked.")) }
    if s.starts_with("ERROR: not found:") || (s.starts_with('=') && s.contains(" no tests ran in ")) { return Some(("pytest", "pytest found no test matching what was asked.")) }
    None
}

fn read_lines(lines: &[String]) -> Tests {
    let mut t = Tests::default();
    let mut listed_order: Vec<(String, String, &'static str)> = vec![];
    let runner = |t: &mut Tests, name: &'static str| if !t.runners.contains(&name) { t.runners.push(name) };
    let push = |t: &mut Tests, f: Failure| if t.failures.len() < MOST && !t.failures.iter().any(|g| g.runner == f.runner && g.name == f.name) { t.failures.push(f) };
    let playwright = lines.iter().any(|l| l.trim_start().starts_with("Running ") && l.contains(" using ") && l.contains("worker"));
    for (i, l) in lines.iter().enumerate() {
        let s = l.trim_start();
        if let Some((name, note)) = none_matched(s) {
            runner(&mut t, name);
            if !t.notes.iter().any(|n| n == note) { t.notes.push(note.to_string()) }
            continue;
        }
        // Vitest: its list of failures (` FAIL  file > suite > name`), then its summary.
        if let Some(rest) = s.strip_prefix("FAIL ").map(str::trim_start).filter(|r| r.contains(" > ")) {
            let (file, name) = rest.split_once(" > ").unwrap();
            if file.contains(".test.") || file.contains(".spec.") || file.contains('/') {
                runner(&mut t, "vitest");
                let error = error_after(lines, i + 1, |n| { let n = n.trim_start(); (n.starts_with("FAIL ") && n.contains(" > ")) || n.starts_with("⎯⎯") || n.starts_with("Test Files") });
                push(&mut t, Failure { runner: "vitest", name: name.trim().to_string(), file: Some(file.trim().to_string()), error });
                continue;
            }
        }
        if let Some(rest) = s.strip_prefix("Tests ").filter(|r| !r.starts_with(':') && (r.contains(" passed") || r.contains(" failed")) && r.contains('(')) {
            runner(&mut t, "vitest");
            (t.passed, t.failed, t.skipped) = (t.passed + count(rest, "passed"), t.failed + count(rest, "failed"), t.skipped + count(rest, "skipped") + count(rest, "todo"));
            continue;
        }
        // Jest: `● suite › name` and its error, then `Tests:  2 failed, 9 passed, 11 total`.
        if let Some(name) = s.strip_prefix("● ").filter(|n| !n.starts_with("Console") && !n.starts_with("Test suite failed to run")) {
            runner(&mut t, "jest");
            let error = error_after(lines, i + 1, |n| n.trim_start().starts_with("● ") || n.trim_start().starts_with("Test Suites:"));
            push(&mut t, Failure { runner: "jest", name: name.trim().to_string(), file: None, error });
            continue;
        }
        if let Some(rest) = s.strip_prefix("Tests:") {
            runner(&mut t, "jest");
            (t.passed, t.failed, t.skipped) = (t.passed + count(rest, "passed"), t.failed + count(rest, "failed"), t.skipped + count(rest, "skipped") + count(rest, "todo"));
            continue;
        }
        // Playwright: each test as it ends (its list), `1) [chromium] › tests/a.spec.ts:12:5 › name ───` for each
        // failure, then its summary lines.
        if playwright {
            // (Its first line: Playwright ran here, though it may say nothing more.)
            if s.starts_with("Running ") && s.contains(" using ") && s.contains("worker") { runner(&mut t, "playwright"); continue }
            if let Some((mark, name, file, retry)) = listed(s) {
                runner(&mut t, "playwright");
                let outcome = match mark { '✓' => "passed", '✘' => "failed", _ => "skipped" };
                // A retry is the same run of the test again (failed before and passed now is flaky); the same title
                // with no retry's mark is a run of its own (`--repeat-each`), counted as Playwright counts it.
                match listed_order.iter().rposition(|(n, f, _)| *n == name && *f == file).filter(|_| retry) {
                    Some(at) => { let was = listed_order[at].2; listed_order[at].2 = if was == "failed" && outcome == "passed" { "flaky" } else { outcome } }
                    None => listed_order.push((name, file, outcome)),
                }
                continue;
            }
            if let Some((n, rest)) = s.split_once(") [").filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
                let _ = n;
                let parts: Vec<&str> = rest.split(" › ").collect();
                if parts.len() >= 3 {
                    runner(&mut t, "playwright");
                    let project = parts[0].trim_end_matches(']');
                    let file = parts[1].trim();
                    let name = parts[2..].join(" › ").trim_end_matches(|c: char| c == '─' || c.is_whitespace()).to_string();
                    let summary = |n: &str| { let w: Vec<&str> = n.split_whitespace().collect(); w.len() >= 2 && w[0].parse::<u64>().is_ok() && ["passed", "failed", "flaky", "skipped", "interrupted", "did"].contains(&w[1]) };
                    let error = error_after(lines, i + 1, |n| { let n = n.trim_start(); summary(n) || n.split_once(") [").is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_digit())) });
                    push(&mut t, Failure { runner: "playwright", name: format!("[{project}] {name}"), file: Some(file.to_string()), error });
                    continue;
                }
            }
            let words: Vec<&str> = s.split_whitespace().collect();
            if words.len() >= 2 && words[0].parse::<u64>().is_ok() && ["passed", "failed", "flaky", "skipped", "interrupted", "did"].contains(&words[1]) && words.len() <= 4 {
                runner(&mut t, "playwright");
                let n = words[0].parse::<u64>().unwrap_or(0);
                match words[1] { "passed" | "flaky" => t.passed += n, "failed" | "interrupted" => t.failed += n, "skipped" => t.skipped += n, _ => {} }
                continue;
            }
        }
        // pytest: `FAILED file::name - error`, and its `== 2 failed, 9 passed in 1.2s ==`.
        if let Some(rest) = s.strip_prefix("FAILED ").or_else(|| s.strip_prefix("ERROR ")).filter(|r| r.contains("::")) {
            runner(&mut t, "pytest");
            let (test, error) = rest.split_once(" - ").unwrap_or((rest, ""));
            let (file, name) = test.split_once("::").unwrap();
            push(&mut t, Failure { runner: "pytest", name: name.trim().to_string(), file: Some(file.trim().to_string()), error: error.trim().chars().take(ERROR_CHARS).collect() });
            continue;
        }
        if s.starts_with('=') && s.ends_with('=') && s.contains(" in ") && (s.contains(" passed") || s.contains(" failed") || s.contains(" error")) && !s.contains("short test summary") {
            runner(&mut t, "pytest");
            (t.passed, t.failed, t.skipped) = (t.passed + count(s, "passed"), t.failed + count(s, "failed") + count(s, "error") + count(s, "errors"), t.skipped + count(s, "skipped"));
            continue;
        }
        // cargo: `test name ... FAILED`, its `---- name stdout ----` block, and `test result: …`.
        if let Some(name) = s.strip_prefix("test ").and_then(|r| r.strip_suffix(" ... FAILED")) {
            runner(&mut t, "cargo");
            let block = lines.iter().position(|b| b.trim() == format!("---- {name} stdout ----"));
            let error = block.map(|b| error_after(lines, b + 1, |n| n.trim_start().starts_with("---- ") || n.trim() == "failures:")).unwrap_or_default();
            push(&mut t, Failure { runner: "cargo", name: name.to_string(), file: None, error });
            continue;
        }
        if let Some(rest) = s.strip_prefix("test result: ") {
            runner(&mut t, "cargo");
            (t.passed, t.failed, t.skipped) = (t.passed + count(rest, "passed"), t.failed + count(rest, "failed"), t.skipped + count(rest, "ignored"));
            continue;
        }
        // Go: `--- FAIL: TestName (0.01s)` and the lines indented under it; `--- PASS:`, `--- SKIP:` counted.
        if let Some(rest) = s.strip_prefix("--- FAIL: ") {
            runner(&mut t, "go");
            t.failed += 1;
            let name = rest.split(" (").next().unwrap_or(rest).trim().to_string();
            let depth = l.len() - s.len();
            let error = error_after(lines, i + 1, |n| { let d = n.len() - n.trim_start().len(); !n.trim().is_empty() && d <= depth });
            push(&mut t, Failure { runner: "go", name, file: None, error });
            continue;
        }
        if s.starts_with("--- PASS: ") { runner(&mut t, "go"); t.passed += 1; continue }
        if s.starts_with("--- SKIP: ") { runner(&mut t, "go"); t.skipped += 1; continue }
    }
    // (Failures listed but no summary seen: counted from the list.)
    if t.failed < t.failures.len() as u64 { t.failed = t.failures.len() as u64 }
    // A test listed as failed that is not among the failures failed as it was marked to (`test.fail()`). Counted
    // over every test listed (only the first `MOST` are kept to show).
    for (name, file, outcome) in listed_order {
        let failure = t.failures.iter().any(|f| f.name == name && f.file.as_deref() == Some(file.as_str()));
        let outcome = if outcome == "failed" && !failure { "failed as expected" } else { outcome };
        match outcome { "failed as expected" => t.failed_as_expected += 1, "flaky" => t.flaky += 1, _ => {} }
        if outcome == "failed as expected" && t.failed_as_expected_tests.len() < MOST { t.failed_as_expected_tests.push(Ran { name: name.clone(), file: Some(file.clone()), outcome }) }
        if t.ran.len() < MOST { t.ran.push(Ran { name, file: Some(file), outcome }) }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log of no lines says no runner and no test; a title's added marks are taken off two at most.
    #[test]
    fn no_lines_are_no_tests_and_at_most_two_marks_come_off_a_title() {
        let t = read_lines(&[]);
        assert!(t.runners.is_empty() && t.failures.is_empty() && (t.passed, t.failed, t.skipped) == (0, 0, 0));
        assert_eq!(listed("✓  1 a.spec.ts:4:3 › logs in (retry #1) (2.1s)"), Some(('✓', "logs in".into(), "a.spec.ts:4:3".into(), true)));
        assert_eq!(listed("✓  1 a.spec.ts:4:3 › logs in (1s) (retry #1) (2.1s)"), Some(('✓', "logs in (1s)".into(), "a.spec.ts:4:3".into(), true)));
        assert_eq!(listed("✓  1 a.spec.ts:4:3 › logs in (old layout) (2.1s)"), Some(('✓', "logs in (old layout)".into(), "a.spec.ts:4:3".into(), false)));
    }

    #[test]
    fn vitest_s_failures_and_counts() {
        let log = "2026-10-15T10:00:01.0000000Z  ❯ src/sum.test.ts (3 tests | 1 failed) 12ms\n\
                   2026-10-15T10:00:01.0000000Z    × sums > adds two numbers 5ms\n\
                   2026-10-15T10:00:02.0000000Z ⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯\n\
                   2026-10-15T10:00:02.0000000Z \n\
                   2026-10-15T10:00:02.0000000Z \u{1b}[31m FAIL \u{1b}[39m src/sum.test.ts > sums > adds two numbers\n\
                   2026-10-15T10:00:02.0000000Z AssertionError: expected 3 to be 4 // Object.is equality\n\
                   2026-10-15T10:00:02.0000000Z \n\
                   2026-10-15T10:00:02.0000000Z - Expected\n\
                   2026-10-15T10:00:02.0000000Z + Received\n\
                   2026-10-15T10:00:02.0000000Z ⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯\n\
                   2026-10-15T10:00:02.0000000Z  Test Files  1 failed | 4 passed (5)\n\
                   2026-10-15T10:00:02.0000000Z       Tests  1 failed | 41 passed | 2 skipped (44)";
        let t = read(log);
        assert_eq!((t.runners.clone(), t.passed, t.failed, t.skipped), (vec!["vitest"], 41, 1, 2));
        assert_eq!((t.failures[0].name.as_str(), t.failures[0].file.as_deref()), ("sums > adds two numbers", Some("src/sum.test.ts")));
        assert!(t.failures[0].error.starts_with("AssertionError: expected 3 to be 4") && t.failures[0].error.contains("+ Received"), "{}", t.failures[0].error);
    }

    #[test]
    fn each_run_of_a_repeated_test_is_counted_as_playwright_counts_it_and_a_retry_is_the_same_run() {
        // `--repeat-each=3`: the same title three times, no retry's mark; a test failed then passed on its retry.
        let log = "Running 4 tests using 1 worker\n  ✘  1 [chromium] › e2e/a.spec.ts:3:1 › known bug (1.0s)\n  ✘  2 [chromium] › e2e/a.spec.ts:3:1 › known bug (1.0s)\n  ✘  3 [chromium] › e2e/a.spec.ts:3:1 › known bug (1.1s)\n  ✘  4 [chromium] › e2e/b.spec.ts:5:1 › sometimes (2.0s)\n  ✓  5 [chromium] › e2e/b.spec.ts:5:1 › sometimes (retry #1) (1.9s)\n\n  1 flaky\n  3 passed (8.0s)";
        let t = read(log);
        assert_eq!((t.failed_as_expected, t.flaky, t.failed_as_expected_tests.len()), (3, 1, 3));
    }

    #[test]
    fn tests_that_failed_as_expected_are_counted_however_many_tests_ran() {
        // 250 tests, the last 18 marked to fail (✘, not among the failures): all 18 counted and named, past the 200 kept.
        let mut log = String::from("Running 250 tests using 4 workers\n");
        for i in 0..250 { log.push_str(&format!("  {}  {} [chromium] › e2e/t.spec.ts:{}:1 › test {i} (1.0s)\n", if i >= 232 { '✘' } else { '✓' }, i + 1, i + 1)) }
        log.push_str("  18 failed\n  232 passed (1.0m)\n");
        let t = read(&log);
        assert_eq!((t.ran.len(), t.failed_as_expected, t.failed_as_expected_tests.len()), (200, 18, 18));
        assert_eq!(t.failed_as_expected_tests[0].name, "[chromium] test 232");
    }

    #[test]
    fn jest_s_failures_and_counts() {
        let log = "  ● Cart › totals the items\n\n    expect(received).toBe(expected) // Object.is equality\n\n    Expected: 30\n    Received: 20\n\n      at Object.<anonymous> (src/cart.test.js:12:20)\n\n  ● Console\n\n    console.log\n      hi\n\nTest Suites: 1 failed, 2 passed, 3 total\nTests:       1 failed, 1 skipped, 17 passed, 19 total";
        let t = read(log);
        assert_eq!((t.runners.clone(), t.passed, t.failed, t.skipped), (vec!["jest"], 17, 1, 1));
        assert_eq!(t.failures.len(), 1, "the console is no failure");
        assert_eq!(t.failures[0].name, "Cart › totals the items");
        assert!(t.failures[0].error.starts_with("expect(received).toBe(expected)") && t.failures[0].error.contains("Received: 20"), "{}", t.failures[0].error);
    }

    #[test]
    fn playwright_s_failures_and_counts() {
        let log = "Running 12 tests using 4 workers\n\n  1) [chromium] › tests/login.spec.ts:14:5 › login › shows an error for a wrong password ─────────\n\n    Error: expect(locator).toBeVisible() failed\n\n    Locator: getByText('Wrong password')\n    Expected: visible\n\n  2) [webkit] › tests/cart.spec.ts:8:3 › adds to cart ──────\n\n    TimeoutError: page.click: Timeout 5000ms exceeded.\n\n  2 failed\n    [chromium] › tests/login.spec.ts:14:5 › login › shows an error for a wrong password\n  1 flaky\n  9 passed (41.2s)";
        let t = read(log);
        assert_eq!((t.runners.clone(), t.passed, t.failed), (vec!["playwright"], 10, 2));
        assert_eq!((t.failures[0].name.as_str(), t.failures[0].file.as_deref()), ("[chromium] login › shows an error for a wrong password", Some("tests/login.spec.ts:14:5")));
        assert!(t.failures[0].error.starts_with("Error: expect(locator).toBeVisible() failed") && !t.failures[0].error.contains("TimeoutError"), "{}", t.failures[0].error);
        assert_eq!(t.failures[1].error, "TimeoutError: page.click: Timeout 5000ms exceeded.");
    }

    #[test]
    fn pytest_cargo_and_go_failures_and_counts() {
        let py = read("=========================== short test summary info ============================\nFAILED tests/test_api.py::test_login - AssertionError: assert 401 == 200\nERROR tests/test_db.py::test_migrate - ConnectionRefusedError\n=================== 1 failed, 30 passed, 2 skipped, 1 error in 4.21s ===================");
        assert_eq!((py.runners.clone(), py.passed, py.failed, py.skipped), (vec!["pytest"], 30, 2, 2));
        assert_eq!((py.failures[0].name.as_str(), py.failures[0].file.as_deref(), py.failures[0].error.as_str()), ("test_login", Some("tests/test_api.py"), "AssertionError: assert 401 == 200"));

        let cargo = read("test store::reads_back ... ok\ntest store::writes ... FAILED\n\nfailures:\n\n---- store::writes stdout ----\n\nthread 'store::writes' panicked at src/store.rs:40:9:\nassertion `left == right` failed\n  left: 1\n right: 2\n\nfailures:\n    store::writes\n\ntest result: FAILED. 12 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out");
        assert_eq!((cargo.runners.clone(), cargo.passed, cargo.failed, cargo.skipped), (vec!["cargo"], 12, 1, 2));
        assert!(cargo.failures[0].name == "store::writes" && cargo.failures[0].error.starts_with("thread 'store::writes' panicked at src/store.rs:40:9:") && cargo.failures[0].error.contains("right: 2"), "{:?}", cargo.failures[0]);

        let go = read("=== RUN   TestSum\n--- PASS: TestSum (0.00s)\n=== RUN   TestDiv\n    div_test.go:12: expected 2, got 0\n--- FAIL: TestDiv (0.00s)\n    div_test.go:14: division by zero\n--- SKIP: TestSlow (0.00s)\nFAIL\nFAIL\texample.com/calc\t0.012s");
        assert_eq!((go.runners.clone(), go.passed, go.failed, go.skipped), (vec!["go"], 1, 1, 1));
        assert_eq!((go.failures[0].name.as_str(), go.failures[0].error.as_str()), ("TestDiv", "div_test.go:14: division by zero"));
    }

    #[test]
    fn a_runner_that_found_no_test_matching_says_so() {
        let t = read("[slot 1] Running 0 tests using 0 workers\n[slot 1] Error: No tests found.\n[slot 1] Make sure that arguments are regular expressions matching test files.");
        assert_eq!((t.runners.clone(), t.failed, t.notes.len()), (vec!["playwright"], 0, 1));
        assert!(t.notes[0].starts_with("[slot 1] Playwright found no test to run: a `file:line` must be the first line of a test"), "{:?}", t.notes);
        assert_eq!(read("No test files found, exiting with code 1").notes, ["Vitest found no test file matching what was asked."]);
        assert_eq!(read("No tests found, exiting with code 1\nRun with `--passWithNoTests` to exit with code 0").runners, ["jest"]);
        assert_eq!(read("============================ no tests ran in 0.01s =============================").runners, ["pytest"]);
    }

    #[test]
    fn the_tests_that_ran_are_listed_a_test_marked_to_fail_passes_and_a_process_that_ran_none_is_said() {
        let log = "[slot 1] Running 4 tests using 2 workers\n\
                   [slot 1]   ✓  1 [chromium] › e2e/a.spec.ts:4:3 › logs in (2.1s)\n\
                   [slot 1]   ✘  2 [chromium] › e2e/a.spec.ts:9:3 › known bug (1.0s)\n\
                   [slot 1]   ✘  3 [chromium] › e2e/b.spec.ts:2:1 › sometimes (3.0s)\n\
                   [slot 1]   ✓  4 [chromium] › e2e/b.spec.ts:2:1 › sometimes (retry #1) (2.0s)\n\
                   [slot 1]   -  5 [chromium] › e2e/c.spec.ts:1:1 › later\n\
                   [slot 1]   1 skipped\n\
                   [slot 1]   1 flaky\n\
                   [slot 1]   2 passed (6.0s)\n\
                   [slot 2] Running 0 tests using 0 workers\n\
                   [slot 3] Running 1 test using 1 worker\n\
                   [slot 3]   ✓  1 e2e/d.spec.ts:3:1 › alone (1s)\n\
                   [slot 3]   1 passed (1.0s)";
        let t = read(log);
        assert_eq!((t.failed, t.passed, t.skipped), (0, 4, 1));
        let ran: Vec<(&str, &str)> = t.ran.iter().map(|r| (r.name.as_str(), r.outcome)).collect();
        assert_eq!(ran, [("[chromium] logs in", "passed"), ("[chromium] known bug", "failed as expected"), ("[chromium] sometimes", "flaky"), ("[chromium] later", "skipped"), ("alone", "passed")]);
        assert_eq!(t.ran[0].file.as_deref(), Some("e2e/a.spec.ts:4:3"));
        assert_eq!((t.failed_as_expected, t.flaky), (1, 1));
        assert_eq!(t.notes, ["[slot 2] Playwright ran no tests."]);
        // Each process's own counts (the one that ran nothing, none); each test that failed as expected, named.
        assert_eq!(t.by_process.iter().map(|p| (p.mark.as_str(), p.passed, p.failed_as_expected, p.flaky)).collect::<Vec<_>>(), [("[slot 1]", 3, 1, 1), ("[slot 2]", 0, 0, 0), ("[slot 3]", 1, 0, 0)]);
        assert_eq!(t.failed_as_expected_tests.iter().map(|r| (r.name.as_str(), r.file.as_deref())).collect::<Vec<_>>(), [("[chromium] known bug", Some("e2e/a.spec.ts:9:3"))]);
    }

    #[test]
    fn lines_are_read_whatever_wraps_them_and_odd_ones_are_not_taken_for_tests() {
        // A line's time (GitHub's), GitLab's stream mark, colour codes and carriage returns go; what only looks like
        // them stays.
        assert_eq!(plain("2026-10-16T10:00:00.1234567Z \u{1b}[31mok\u{1b}[0m\r"), "ok");
        assert_eq!(plain("00O+passed line"), "passed line");
        assert_eq!(plain("12E text"), "text");
        for kept in ["00X text", "0aO text", "00Otext", "abc", "2026x10-16T10:00:00Z short one", "2026-10-16x10:00:00.0000000Z not a time", "abcd-10-16T10:00:00.0000000Z not a year", "\u{1b}x"] {
            assert_eq!(plain(kept), kept.trim_end(), "{kept:?}");
        }
        // pytest's summary with errors only; a line of = that is no summary.
        let t = read("========================= 2 errors in 0.31s =========================\n===== not a summary =====");
        assert_eq!((t.runners.clone(), t.failed), (vec!["pytest"], 2));
        assert_eq!(read("=== RESULTS: 3 passed ===").runners, Vec::<&str>::new(), "no time, no summary");
        // Vitest's FAIL with no test file named: not a failure of its.
        assert!(read(" FAIL  build > step").failures.is_empty());
        assert_eq!(read(" FAIL  pkg/x.ts > a > b\nboom").failures.len(), 1);
        assert_eq!(read(" FAIL  a.spec.ts > b\nboom").failures[0].file.as_deref(), Some("a.spec.ts"));
        // An error is cut at a second blank line, and at `ERROR_LINES` lines.
        let long: String = (1..=60).map(|n| format!("  line {n}\n")).collect();
        let t = read(&format!(" FAIL  a.test.ts > b\n{long}"));
        assert_eq!(t.failures[0].error.lines().count(), ERROR_LINES);
        let t = read(" FAIL  a.test.ts > b\nfirst\n\nsecond\n\n\nthird");
        assert_eq!(t.failures[0].error, "first\n\nsecond");
        // The same failure said twice is one; at most `MOST` are kept.
        assert_eq!(read(" FAIL  a.test.ts > b\nx\n FAIL  a.test.ts > b\nx").failures.len(), 1);
        let many: String = (0..MOST + 5).map(|n| format!(" FAIL  a.test.ts > t{n}\nx\n")).collect();
        assert_eq!(read(&many).failures.len(), MOST);
        // Marks: one too long, one with brackets inside, Playwright's own list line: no process's.
        let long_mark = format!("[{}] Running 1 test using 1 worker", "x".repeat(50));
        assert_eq!(marked(&long_mark).0, "");
        assert_eq!(marked("[a[b] text").0, "");
        assert_eq!(marked("[slot 1] › text").0, "");
        // Playwright's list: without a project, with a file but no line, a retry passed (flaky), passed then failed
        // with no retry's mark (two runs of it, as `--repeat-each` makes).
        let t = read("Running 4 tests using 1 worker\n  ✓  1 e2e/a.spec.ts:3:1 › one (1s)\n  ✓  2 e2e/a.spec.ts › no line (1s)\n  ✘  3 [p] › e2e/b.spec.ts:1:1 › two (1s)\n  ✓  4 [p] › e2e/b.spec.ts:1:1 › two (retry #1) (1s)\n  ✓  5 [p] › e2e/c.spec.ts:1:1 › three (1s)\n  ✘  6 [p] › e2e/c.spec.ts:1:1 › three (1s)\n  ✓  [p] › e2e/d.spec.ts:2:2 › four\n  - [p] e2e/x.spec.ts:1:1 no separator\n  3 passed (4s)");
        let ran: Vec<(&str, &str)> = t.ran.iter().map(|r| (r.name.as_str(), r.outcome)).collect();
        assert_eq!(ran, [("one", "passed"), ("[p] two", "flaky"), ("[p] three", "passed"), ("[p] three", "failed as expected"), ("[p] four", "passed")]);
        // Playwright's summary that is not one: too many words, or a word it does not say.
        let t = read("Running 1 test using 1 worker\n  2 passed but slowly today\n  2 tests\n  1 passed (1s)");
        assert_eq!(t.passed, 1);
        // "Running" alone is not Playwright's.
        assert!(read("Running 3 checks\n  1 passed").runners.is_empty());
        // A note said once, however many processes say it.
        let t = read("Error: No tests found\n[a] Error: No tests found\n[a] Error: No tests found");
        assert_eq!(t.notes.len(), 2);
    }

    #[test]
    fn a_titles_own_brackets_are_kept_and_only_a_real_failure_is_one() {
        // Two tests alike but for their bracketed end, one of which really failed: each its own line, and the failed
        // one a failure (not "failed as expected", which only a test marked to fail is).
        let log = "Running 3 tests using 1 worker\n  ✓  1 [chromium] › e2e/feed.spec.ts:4:3 › shows the page (old layout) (1.2s)\n  ✓  2 [chromium] › e2e/feed.spec.ts:9:3 › shows the page (new layout) (845ms)\n  ✘  3 [chromium] › e2e/audit.spec.ts:2:1 › keeps the order (issue #25) (2.0m)\n  ✘  4 [chromium] › e2e/audit.spec.ts:2:1 › keeps the order (issue #25) (retry #1) (1.9m)\n\n  1) [chromium] › e2e/audit.spec.ts:2:1 › keeps the order (issue #25) ───\n\n    Error: expected 1, got 2\n\n  1 failed\n  2 passed (4.1m)";
        let t = read(log);
        let ran: Vec<(&str, &str)> = t.ran.iter().map(|r| (r.name.as_str(), r.outcome)).collect();
        assert_eq!(ran, [("[chromium] shows the page (old layout)", "passed"), ("[chromium] shows the page (new layout)", "passed"), ("[chromium] keeps the order (issue #25)", "failed")]);
        assert_eq!((t.failed_as_expected, t.flaky), (0, 0));
        assert_eq!((t.failed, t.failures[0].name.as_str()), (1, "[chromium] keeps the order (issue #25)"));
    }

    #[test]
    fn a_log_without_tests_says_none() {
        let t = read("Run make build\ncompiling…\nDone in 3s");
        assert_eq!(t, Tests::default());
    }

    #[test]
    fn lines_marked_by_the_process_they_came_from_are_read_apart_and_added_up() {
        // Two Playwright processes at once, and Vitest before them, their lines mixed.
        let log = "2026-10-16T09:00:00.0000000Z  Test Files  40 passed (40)\n\
                   2026-10-16T09:00:00.0000000Z       Tests  1915 passed (1915)\n\
                   2026-10-16T09:00:01.0000000Z [slot 1] Running 30 tests using 2 workers\n\
                   2026-10-16T09:00:01.0000000Z [slot 2] Running 28 tests using 2 workers\n\
                   2026-10-16T09:00:09.0000000Z [slot 2]   1) [chromium] › e2e/billing.spec.ts:21:3 › billing › shows the invoice ───\n\
                   2026-10-16T09:00:09.0000000Z [slot 1]   ✓ [chromium] › e2e/login.spec.ts:4:3 › logs in (2.1s)\n\
                   2026-10-16T09:00:09.0000000Z [slot 2] \n\
                   2026-10-16T09:00:09.0000000Z [slot 2]     Error: expect(locator).toHaveText(expected) failed\n\
                   2026-10-16T09:00:10.0000000Z [slot 1]   30 passed (12.0s)\n\
                   2026-10-16T09:00:10.0000000Z [slot 2]     Expected: \"Invoice\"\n\
                   2026-10-16T09:00:11.0000000Z [slot 2]   1 failed\n\
                   2026-10-16T09:00:11.0000000Z [slot 2]     [chromium] › e2e/billing.spec.ts:21:3 › billing › shows the invoice\n\
                   2026-10-16T09:00:11.0000000Z [slot 2]   27 passed (13.4s)";
        let t = read(log);
        assert_eq!((t.runners.clone(), t.passed, t.failed), (vec!["vitest", "playwright"], 1915 + 30 + 27, 1));
        assert_eq!((t.failures[0].name.as_str(), t.failures[0].file.as_deref()), ("[chromium] billing › shows the invoice", Some("e2e/billing.spec.ts:21:3")));
        assert_eq!(t.failures[0].error, "Error: expect(locator).toHaveText(expected) failed\nExpected: \"Invoice\"");
        // (A test that failed whole: its error is kept whole, not its first lines.)
        let long = read(&format!("Running 1 test using 1 worker\n  1) [chromium] › a.spec.ts:3:1 › x ───\n\n    Error: expect(received).toEqual(expected)\n\n{}\n  1 failed", (1..=30).map(|n| format!("    - line {n}")).collect::<Vec<_>>().join("\n")));
        assert!(long.failures[0].error.ends_with("- line 30"), "{}", long.failures[0].error);
        // (Playwright's own project in front of a line is no process's mark.)
        assert_eq!(marked("[chromium] › a.spec.ts:1:1 › x"), ("", "[chromium] › a.spec.ts:1:1 › x"));
    }

    #[test]
    fn every_runner_is_called_by_its_name_when_it_ran_no_tests() {
        for (id, name) in RUNNER_NAMES { assert_eq!(runner_name(id), name) }
        assert_eq!(runner_name("mocha"), "A test runner");
        for (log, note) in [("Tests:  0 total", "Jest ran no tests."), (" Tests  0 passed (0)", "Vitest ran no tests."), ("== 0 passed in 0.1s ==", "pytest ran no tests."),
            ("test result: ok. 0 passed; 0 failed", "cargo test ran no tests.")] {
            assert_eq!(read(log).notes, [note], "{log}");
        }
    }

    #[test]
    fn an_empty_log_and_lines_cut_short_are_read_as_nothing() {
        assert_eq!(read(""), Tests::default());
        // A colour code cut off at the line's end; GitLab's stream mark with a space.
        assert_eq!((plain("ok\u{1b}["), plain("01O hello")), ("ok".to_string(), "hello".to_string()));
        assert_eq!(error_after(&["a".to_string()], 1, |_| false), "");
        assert_eq!(count("", "passed"), 0);
    }

    #[test]
    fn every_runners_word_that_nothing_matched_is_known() {
        for (line, runner) in [("No test files found.", "vitest"), ("No test suite found in file a.test.ts", "vitest"), ("ERROR: not found: tests/a.py::b", "pytest"), ("No tests found, exiting with code 1", "jest")] {
            assert_eq!(none_matched(line).map(|(r, _)| r), Some(runner), "{line}");
        }
        assert_eq!(none_matched("All good"), None);
    }

    #[test]
    fn a_playwright_list_line_that_is_not_one_is_not_taken_for_a_test() {
        for line in ["✓ [chromium › a.ts:1:1 › t", "✓ [chromium] › a.ts:1:1", "✓ a.ts:x › t", "✓ a.ts › t", "x a.ts:1:1 › t"] {
            assert_eq!(listed(line), None, "{line}");
        }
        // Only a time or a retry's mark is taken off a title; anything else in brackets is the title's.
        for (line, title, retry) in [("✓ a.ts:1:1 › t (retry #x)", "t (retry #x)", false), ("✓ a.ts:1:1 › t (retry #)", "t (retry #)", false), ("✓ a.ts:1:1 › t (.5s)", "t (.5s)", false),
            ("✓ a.ts:1:1 › t (1a s)", "t (1a s)", false), ("✓ a.ts:1:1 › t ()", "t ()", false), ("✓ a.ts:1:1 › t (retry #2) (1.5h)", "t", true)] {
            assert_eq!(listed(line).map(|(_, n, _, r)| (n, r)), Some((title.to_string(), retry)), "{line}");
        }
    }

    #[test]
    fn at_most_so_many_failures_and_tests_are_kept_from_all_processes_together() {
        let many = |mark: &str, from: usize| -> String { (from..from + 150).map(|n| format!("[{mark}] FAILED t.py::t{n} - x\n")).collect() };
        let t = read(&format!("{}{}", many("a", 0), many("b", 150)));
        assert_eq!((t.failures.len(), t.failed, t.by_process.len()), (MOST, 300, 2));
        let listed = |mark: &str, outcome: char| -> String { format!("[{mark}] Running 150 tests using 1 worker\n{}", (0..150).map(|n| format!("[{mark}]   {outcome} {n} e2e/{mark}.spec.ts:{n}:1 › t\n")).collect::<String>()) };
        let t = read(&format!("{}{}", listed("a", '✘'), listed("b", '✘')));
        assert_eq!((t.ran.len(), t.failed_as_expected_tests.len(), t.failed_as_expected), (MOST, MOST, 300));
        // A process that ran no runner is no process of the counts; Playwright's skipped ones counted as skipped.
        let t = read("[web] listening on 3000\n[a] Running 1 test using 1 worker\n[a]   2 skipped\n[b] test result: ok. 1 passed");
        assert_eq!((t.skipped, t.by_process.iter().map(|p| p.mark.as_str()).collect::<Vec<_>>()), (2, vec!["[a]", "[b]"]));
    }
}
