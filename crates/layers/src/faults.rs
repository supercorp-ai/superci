//! One of the program's steps that can fail, failing in a test, as SQLite tests itself: every `?` in the program
//! passes through `pass`, and with `SUPERCI_LAYERS_FAULT=N` the N-th to pass (counted from the program's start) fails
//! as the step's own error would; with `N+`, the N-th and every one after it (what is put right after a failure
//! failing too). With `SUPERCI_LAYERS_FAULTS=FILE`, how many have passed so far is written to FILE after each, so that
//! a test knows how many there are to fail (`setup-finish`, which `setup` leaves running, to FILE-finish: each its
//! own count, and each file whole whenever it is read). In a debug build only: a release build passes all.

/// What passed, passed on; or, in a test, the step asked to fail, failing.
#[cfg(debug_assertions)]
pub fn pass<T, E: From<std::io::Error>>(v: T) -> Result<T, E> {
    static PASSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = PASSED.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    if let Some(file) = std::env::var_os("SUPERCI_LAYERS_FAULTS") {
        let file = std::path::PathBuf::from(file);
        let file = if std::env::args().nth(1).as_deref() == Some("setup-finish") { file.with_file_name(format!("{}-finish", file.file_name().unwrap_or_default().to_string_lossy())) } else { file };
        let being_written = file.with_file_name(format!(".{}-{n}", std::process::id()));
        let _ = std::fs::write(&being_written, n.to_string()).and_then(|_| std::fs::rename(&being_written, &file));
    }
    let asked = std::env::var("SUPERCI_LAYERS_FAULT").unwrap_or_default();
    let (k, after) = asked.strip_suffix('+').map_or((asked.as_str(), false), |k| (k, true));
    if k.parse::<usize>().is_ok_and(|k| n == k || (after && n > k)) { return Err(std::io::Error::other(format!("step {n} failed, as the test asked")).into()) }
    Ok(v)
}

/// Whether steps that may run at once do: not while a test fails one, so that each step is the same one in every run.
#[cfg(debug_assertions)]
pub fn together() -> bool { std::env::var_os("SUPERCI_LAYERS_FAULT").is_none() }

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn pass<T, E>(v: T) -> Result<T, E> { Ok(v) }

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn together() -> bool { true }

/// The same for a step whose error is said in words.
pub fn said<T>(v: T) -> Result<T, String> { pass::<T, std::io::Error>(v).map_err(|e| e.to_string()) }

/// A step's own outcome, or (in a test) the step asked to fail, failing: for an outcome looked at, not passed up.
pub fn check<T, E: From<std::io::Error>>(r: Result<T, E>) -> Result<T, E> { r.and_then(pass) }
