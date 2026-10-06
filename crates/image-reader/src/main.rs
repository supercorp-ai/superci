//! A big read-only file (a machine image, squashfs) kept at a public address as small pieces, shown here as one local
//! file. A piece is fetched whole the first time something in it is read, several at once, and kept on local disk; the
//! pieces most jobs need (the "hot" list beside the image) are fetched as soon as it starts. Linux mounts the file as
//! it would any image (`mount -o loop`), so only what a job touches is ever downloaded.
//!
//! `image-reader <address> <mount point> <cache directory> [--parallel N] [--ahead N] [--keep-free-mb N]
//! [--max-cache-mb N] [--pin HEX] [--record FILE]` (Linux)
//!
//! `image-reader pack <image file> <output directory> [--piece BYTES] [--hot FILE] [--reader FILE]` (anywhere)
//!
//! At the address: `index.json` (see `Index`), the pieces as `p/000000`, `p/000001`, … and, optionally, `hot.json`
//! (piece numbers, in the order to fetch them) and `reader-x86_64` (this program, for machines that have none).
//!
//! What is read is what was published: the index carries each piece's SHA-256, and a piece that does not match is
//! not used. `pack` writes everything into a directory named by the index's own SHA-256, so an address ending in that
//! name pins the index (the reader checks it), and with it every piece, the hot list and the reader program. Such an
//! address never changes what it holds: a new image is a new address.
//!
//! Publishing an image:
//! 1. One squashfs file of it: `crane export <image> - | sqfstar -comp zstd -b 256K image.sqsh` (a container image's
//!    files as one read-only file system).
//! 2. `image-reader pack image.sqsh out --reader <image-reader built for Linux x86_64>`.
//! 3. Upload the directory it names, as it is, anywhere that serves files over https (an R2 or S3 bucket, any web
//!    server); the address of that directory is the image's.
//! 4. For a fast start: run a usual job once with `--record used.json`, then pack again with `--hot used.json` (a new
//!    directory, a new address: the pieces are the same files, only the index and the list are new).
//!
//! The cache gives way to whatever else uses its disk: when free space runs low, the pieces fetched longest ago are
//! dropped (and fetched again if read again). `--record` writes the pieces that were read, in the order they were
//! first read (what a hot list is made from). `stats.json` in the cache directory says what was fetched.
use sha2::{Digest, Sha256};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("pack") {
        match pack(&args[1..]) { Ok(said) => println!("{said}"), Err(e) => { eprintln!("{e}"); std::process::exit(1) } }
        return;
    }
    #[cfg(target_os = "linux")]
    linux::main(args);
    #[cfg(not(target_os = "linux"))]
    { eprintln!("image-reader shows an image on Linux only (here: image-reader pack <image file> <output directory>)"); std::process::exit(2) }
}

pub fn sha256_hex(data: &[u8]) -> String { Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect() }

fn is_hex(s: &str, len: std::ops::RangeInclusive<usize>) -> bool { len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

/// What `index.json` says: the image's name (the file's, under the mount point), its size, the size of a piece, and,
/// from `pack`, each piece's SHA-256, the hot list's, and the reader programs' by architecture.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub struct Index { pub name: String, pub size: u64, pub piece: u64, pub sums: Option<Vec<String>>, pub hot: Option<String>, pub readers: Vec<(String, String)> }

impl Index {
    pub fn count(&self) -> u64 { self.size.div_ceil(self.piece) }

    /// An index as published, or why it cannot be one: nothing in it is taken on trust (its name becomes a file's,
    /// its sizes are allocated).
    pub fn parse(bytes: &[u8]) -> Result<Index, String> {
        let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| format!("index.json: {e}"))?;
        let name = v["name"].as_str().unwrap_or("image").to_string();
        if name.is_empty() || name.len() > 100 || name.starts_with('.') || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) { return Err(format!("index.json: {name:?} is not a plain file name")) }
        let (size, piece) = (v["size"].as_u64().ok_or("index.json: no size")?, v["piece"].as_u64().ok_or("index.json: no piece size")?);
        if !(64 * 1024..=64 * 1024 * 1024).contains(&piece) { return Err(format!("index.json: pieces of {piece} bytes (64 KiB to 64 MiB)")) }
        if size == 0 || size > 1 << 40 { return Err(format!("index.json: an image of {size} bytes (up to 1 TiB)")) }
        let index = Index { name, size, piece, sums: None, hot: v["hot"].as_str().map(str::to_string),
            readers: v["readers"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect() };
        if index.hot.as_deref().is_some_and(|h| !is_hex(h, 64..=64)) || index.readers.iter().any(|(_, h)| !is_hex(h, 64..=64)) { return Err("index.json: a checksum that is not one".into()) }
        let sums = match v["sha256"].as_array() {
            Some(list) => {
                let sums: Vec<String> = list.iter().filter_map(|s| s.as_str().filter(|s| is_hex(s, 64..=64)).map(str::to_string)).collect();
                if sums.len() as u64 != index.count() || sums.len() != list.len() { return Err(format!("index.json: {} checksums for {} pieces", list.len(), index.count())) }
                Some(sums)
            }
            None => None,
        };
        Ok(Index { sums, ..index })
    }
}

/// What an address pins its index to: its last part, when that is (the start of) a SHA-256, as `pack` names its
/// output; or `--pin`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pin_of(address: &str) -> Option<&str> {
    address.trim_end_matches('/').rsplit('/').next().filter(|last| is_hex(last, 16..=64))
}

/// `pack <image file> <output directory> [--piece BYTES] [--hot FILE] [--reader FILE]`: the image as pieces, with its
/// index, in a directory of the output directory named by the index's SHA-256. Upload that directory as it is (to any
/// place that serves files over https); its address is what the dashboard takes.
fn pack(args: &[String]) -> Result<String, String> {
    use std::io::Read;
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let (Some(file), Some(out)) = (args.first(), args.get(1)) else { return Err("image-reader pack <image file> <output directory> [--piece BYTES] [--hot FILE] [--reader FILE]".into()) };
    let piece: u64 = match flag("--piece") { Some(p) => p.parse().map_err(|_| "--piece: a number of bytes")?, None => 4 * 1024 * 1024 };
    let name = std::path::Path::new(file).file_name().and_then(|n| n.to_str()).ok_or("the image file has no name")?.to_string();
    let mut input = std::fs::File::open(file).map_err(|e| format!("{file}: {e}"))?;
    let size = input.metadata().map_err(|e| e.to_string())?.len();
    let staging = format!("{out}/.packing");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(format!("{staging}/p")).map_err(|e| format!("{staging}: {e}"))?;
    let mut sums = vec![];
    let mut buf = vec![0u8; piece as usize];
    for i in 0..size.div_ceil(piece.max(1)) {
        let want = piece.min(size - i * piece) as usize;
        input.read_exact(&mut buf[..want]).map_err(|e| format!("{file}: {e}"))?;
        sums.push(sha256_hex(&buf[..want]));
        std::fs::write(format!("{staging}/p/{i:06}"), &buf[..want]).map_err(|e| e.to_string())?;
    }
    let mut index = serde_json::json!({ "name": name, "size": size, "piece": piece, "sha256": sums });
    if let Some(hot) = flag("--hot") {
        // As recorded (`--record`): piece numbers, kept in order, each once, only ones the image has.
        let list: Vec<u64> = serde_json::from_slice(&std::fs::read(&hot).map_err(|e| format!("{hot}: {e}"))?).map_err(|e| format!("{hot}: {e}"))?;
        let mut seen = std::collections::HashSet::new();
        let list: Vec<u64> = list.into_iter().filter(|i| *i < sums.len() as u64 && seen.insert(*i)).collect();
        let body = serde_json::to_vec(&list).map_err(|e| e.to_string())?;
        index["hot"] = sha256_hex(&body).into();
        std::fs::write(format!("{staging}/hot.json"), body).map_err(|e| e.to_string())?;
    }
    if let Some(reader) = flag("--reader") {
        let body = std::fs::read(&reader).map_err(|e| format!("{reader}: {e}"))?;
        index["readers"] = serde_json::json!({ "x86_64": sha256_hex(&body) });
        std::fs::write(format!("{staging}/reader-x86_64"), body).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec(&index).map_err(|e| e.to_string())?;
    Index::parse(&body)?;
    std::fs::write(format!("{staging}/index.json"), &body).map_err(|e| e.to_string())?;
    let pin = &sha256_hex(&body)[..32];
    let to = format!("{out}/{pin}");
    let _ = std::fs::remove_dir_all(&to);
    std::fs::rename(&staging, &to).map_err(|e| format!("{to}: {e}"))?;
    Ok(format!("{to}\n{} pieces of {piece} bytes. Upload this directory as it is; the address to use ends in /{pin}", sums.len()))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::VecDeque;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use fuser::{Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request, SessionACL};

    use super::{pin_of, sha256_hex, Index};

    const TTL: Duration = Duration::from_secs(3600);
    const ROOT: u64 = 1;
    const IMAGE: u64 = 2;

    #[derive(Clone, Copy, PartialEq)]
    enum Piece { Missing, Queued, Present }

    /// What is shared by the file system and the fetchers: each piece's state, what to fetch next (asked-for pieces before
    /// ones fetched ahead), and the cache file the pieces are written into at their own offsets.
    struct Shared {
        base: String,
        index: Index,
        cache: File,
        state: Mutex<State>,
        changed: Condvar,
        fetched_bytes: AtomicU64,
        fetched_pieces: AtomicU64,
        asked_pieces: AtomicU64,
        dropped_pieces: AtomicU64,
        failed_pieces: AtomicU64,
        ahead: u64,
        /// The cache gives way: pieces are dropped while its disk has less than this free, or it holds more than `max`.
        keep_free: u64,
        max: u64,
        /// The index is pinned (by the address or --pin): only what it names is used.
        pinned: bool,
    }

    /// `used`: the pieces something read, in the order they were first read (what a hot list is made from). `held`:
    /// how many reads are using each piece now (it is not dropped meanwhile). `kept`: the pieces here, oldest first.
    struct State { pieces: Vec<Piece>, urgent: VecDeque<u64>, later: VecDeque<u64>, used: Vec<u64>, seen: Vec<bool>, held: Vec<u32>, kept: VecDeque<u64>, kept_bytes: u64 }

    /// Pieces held for one read: released when it is answered.
    struct Hold<'a> { shared: &'a Shared, first: u64, last: u64 }
    impl Drop for Hold<'_> {
        fn drop(&mut self) {
            let mut s = self.shared.state.lock().unwrap();
            for i in self.first..=self.last { s.held[i as usize] = s.held[i as usize].saturating_sub(1) }
        }
    }

    impl Shared {
        fn count(&self) -> u64 { self.index.count() }
        fn len_of(&self, i: u64) -> u64 { self.index.piece.min(self.index.size - i * self.index.piece) }

        /// Asks for pieces `first..=last` now, and a few after them later (a file's data lies in order), waits for
        /// the asked ones, and holds them until the read is answered. A piece that could not be fetched fails this
        /// read only: the next read of it tries again.
        fn hold(&self, first: u64, last: u64) -> Result<Hold<'_>, String> {
            let mut s = self.state.lock().unwrap();
            for i in first..=last { s.held[i as usize] += 1 }
            let hold = Hold { shared: self, first, last };
            let mut missing = false;
            for i in first..=last {
                match s.pieces[i as usize] {
                    Piece::Present => {}
                    Piece::Missing => { s.pieces[i as usize] = Piece::Queued; s.urgent.push_back(i); missing = true }
                    Piece::Queued => {
                        // Queued to be fetched later: now.
                        if let Some(at) = s.later.iter().position(|p| *p == i) { s.later.remove(at); s.urgent.push_back(i) }
                        missing = true
                    }
                }
            }
            if !missing { return Ok(hold) }
            self.asked_pieces.fetch_add(1, Ordering::Relaxed);
            for i in last + 1..(last + 1 + self.ahead).min(self.count()) {
                if s.pieces[i as usize] == Piece::Missing { s.pieces[i as usize] = Piece::Queued; s.later.push_back(i) }
            }
            self.changed.notify_all();
            loop {
                // Held pieces are not dropped, so one that is missing again is one whose fetch failed.
                if let Some(i) = (first..=last).find(|i| s.pieces[*i as usize] == Piece::Missing) { drop(s); return Err(format!("piece {i} could not be fetched")) }
                if (first..=last).all(|i| s.pieces[i as usize] == Piece::Present) { return Ok(hold) }
                s = self.changed.wait(s).unwrap();
            }
        }

        /// One fetcher: takes the next piece (asked-for ones first), fetches it whole, writes it into the cache file.
        fn fetch_loop(self: Arc<Self>) {
            let agent = agent(&self.base, 60);
            loop {
                let i = {
                    let mut s = self.state.lock().unwrap();
                    loop {
                        if let Some(i) = s.urgent.pop_front() { break i }
                        // Nothing is fetched ahead while the disk is short of room (it would be dropped at once).
                        if let Some(i) = s.later.pop_front() { if self.free() >= self.keep_free { break i } s.pieces[i as usize] = Piece::Missing; continue }
                        s = self.changed.wait(s).unwrap();
                    }
                };
                let got = (0..4).find_map(|attempt| {
                    if attempt > 0 { std::thread::sleep(Duration::from_millis(300 * attempt)) }
                    self.fetch(&agent, i).map_err(|e| eprintln!("piece {i}: {e}")).ok()
                });
                let mut s = self.state.lock().unwrap();
                match got {
                    Some(()) => { s.pieces[i as usize] = Piece::Present; s.kept.push_back(i); s.kept_bytes += self.len_of(i) }
                    // Back to missing: whoever waits for it is told, and a later read asks again.
                    None => { s.pieces[i as usize] = Piece::Missing; self.failed_pieces.fetch_add(1, Ordering::Relaxed); }
                }
                self.make_room(&mut s, false);
                self.changed.notify_all();
            }
        }

        fn fetch(&self, agent: &ureq::Agent, i: u64) -> Result<(), String> {
            let want = self.len_of(i) as usize;
            let mut body = Vec::with_capacity(want);
            let res = agent.get(&format!("{}/p/{i:06}", self.base)).header("user-agent", "image-reader").call().map_err(|e| e.to_string())?;
            res.into_body().into_reader().take(want as u64 + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
            if body.len() != want { return Err(format!("{} bytes, expected {want}", body.len())) }
            if let Some(sums) = &self.index.sums { if sha256_hex(&body) != sums[i as usize] { return Err("not what the index says it is (its checksum differs)".into()) } }
            // A full disk: the oldest pieces give way, once.
            if self.cache.write_all_at(&body, i * self.index.piece).is_err() {
                self.make_room(&mut self.state.lock().unwrap(), true);
                self.cache.write_all_at(&body, i * self.index.piece).map_err(|e| format!("kept nowhere: {e}"))?;
            }
            self.fetched_bytes.fetch_add(want as u64, Ordering::Relaxed);
            self.fetched_pieces.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        /// Free space on the cache's disk.
        fn free(&self) -> u64 {
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatvfs(self.cache.as_raw_fd(), &mut st) } != 0 { return u64::MAX }
            st.f_bavail as u64 * st.f_frsize as u64
        }

        /// Drops the pieces fetched longest ago (not ones a read holds) while the disk is short of free space or the
        /// cache holds more than it may; `hard`: a write just failed for want of space, so half of what is kept goes.
        fn make_room(&self, s: &mut State, hard: bool) {
            let target = if hard { s.kept_bytes / 2 } else { u64::MAX };
            let mut passed = 0;
            while passed < s.kept.len() && (s.kept_bytes > self.max || s.kept_bytes > target || (s.kept_bytes > 0 && self.free() < self.keep_free)) {
                let Some(i) = s.kept.pop_front() else { break };
                if s.held[i as usize] > 0 { s.kept.push_back(i); passed += 1; continue }
                let len = self.len_of(i);
                // The piece's blocks go back to the disk; the file keeps its size.
                unsafe { libc::fallocate(self.cache.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, (i * self.index.piece) as libc::off_t, len as libc::off_t) };
                s.pieces[i as usize] = Piece::Missing;
                s.kept_bytes -= len;
                self.dropped_pieces.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct Fs { shared: Arc<Shared>, name: String }

    impl Fs {
        fn attr(&self, ino: u64) -> FileAttr {
            let (kind, size, perm, nlink) = if ino == ROOT { (FileType::Directory, 0, 0o555, 2) } else { (FileType::RegularFile, self.shared.index.size, 0o444, 1) };
            FileAttr { ino: INodeNo(ino), size, blocks: size.div_ceil(512), atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH, crtime: UNIX_EPOCH, kind, perm, nlink, uid: 0, gid: 0, rdev: 0, blksize: 131072, flags: 0 }
        }
    }

    impl Filesystem for Fs {
        fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
            // The kernel reads ahead generously (a piece is several megabytes anyway): as far as it lets us ask.
            if let Err(most) = config.set_max_readahead(4 * 1024 * 1024) { let _ = config.set_max_readahead(most); }
            Ok(())
        }
        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            if parent.0 == ROOT && name.to_str() == Some(self.name.as_str()) { reply.entry(&TTL, &self.attr(IMAGE), Generation(0)) } else { reply.error(Errno::ENOENT) }
        }
        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            if ino.0 == ROOT || ino.0 == IMAGE { reply.attr(&TTL, &self.attr(ino.0)) } else { reply.error(Errno::ENOENT) }
        }
        fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
            if ino.0 != ROOT { return reply.error(Errno::ENOTDIR) }
            let entries = [(ROOT, FileType::Directory, "."), (ROOT, FileType::Directory, ".."), (IMAGE, FileType::RegularFile, self.name.as_str())];
            for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) { if reply.add(INodeNo(*ino), i as u64 + 1, *kind, name) { break } }
            reply.ok()
        }
        fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
            // What was read stays in the kernel's page cache.
            if ino.0 == IMAGE { reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE) } else { reply.error(Errno::EISDIR) }
        }
        fn read(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
            if ino.0 != IMAGE { return reply.error(Errno::EISDIR) }
            let shared = self.shared.clone();
            if offset >= shared.index.size { return reply.data(&[]) }
            let len = (size as u64).min(shared.index.size - offset);
            let (first, last) = (offset / shared.index.piece, (offset + len - 1) / shared.index.piece);
            // Already here: answered at once. Else on a thread of its own, so other reads go on meanwhile.
            let here = {
                let mut s = self.shared.state.lock().unwrap();
                for i in first..=last { if !s.seen[i as usize] { s.seen[i as usize] = true; s.used.push(i) } }
                (first..=last).all(|i| s.pieces[i as usize] == Piece::Present)
            };
            // The reply travels in a cell: a thread that could not be started hands it back, and it is answered here.
            let cell = Arc::new(Mutex::new(Some(reply)));
            let answer = { let cell = cell.clone(); move || {
                let Some(reply) = cell.lock().unwrap().take() else { return };
                let held = match shared.hold(first, last) { Ok(h) => h, Err(e) => { eprintln!("{e}"); return reply.error(Errno::EIO) } };
                let mut buf = vec![0u8; len as usize];
                match shared.cache.read_exact_at(&mut buf, offset) { Ok(()) => reply.data(&buf), Err(_) => reply.error(Errno::EIO) }
                drop(held);
            } };
            if here { return answer() }
            if std::thread::Builder::new().stack_size(256 * 1024).spawn(answer.clone()).is_err() { answer() }
        }
    }

    /// https only, but for an address on this machine (to try things out).
    fn agent(base: &str, seconds: u64) -> ureq::Agent {
        let local = base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost");
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(seconds))).https_only(!local).build().into()
    }

    fn get(base: &str, path: &str, seconds: u64, most: u64) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        agent(base, seconds).get(&format!("{base}/{path}")).header("user-agent", "image-reader").call().map_err(|e| format!("{base}/{path}: {e}"))?
            .into_body().into_reader().take(most + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
        if body.len() as u64 > most { return Err(format!("{base}/{path}: more than {most} bytes")) }
        Ok(body)
    }

    fn stop(why: &str) -> ! { eprintln!("image-reader: {why}"); std::process::exit(1) }

    pub fn main(args: Vec<String>) {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let (Some(base), Some(at), Some(cache_dir)) = (args.first(), args.get(1), args.get(2)) else {
            eprintln!("image-reader <address> <mount point> <cache directory> [--parallel N] [--ahead N] [--keep-free-mb N] [--max-cache-mb N] [--pin HEX] [--record FILE]");
            std::process::exit(2)
        };
        let base = base.trim_end_matches('/').to_string();
        if !base.starts_with("https://") && !base.starts_with("http://127.0.0.1") && !base.starts_with("http://localhost") { stop("the address must start with https://") }
        let parallel: usize = flag("--parallel").and_then(|v| v.parse().ok()).unwrap_or(24).clamp(1, 128);
        let ahead: u64 = flag("--ahead").and_then(|v| v.parse().ok()).unwrap_or(2);
        let keep_free = flag("--keep-free-mb").and_then(|v| v.parse::<u64>().ok()).unwrap_or(2048) << 20;
        let max = flag("--max-cache-mb").and_then(|v| v.parse::<u64>().ok()).map(|m| m << 20).unwrap_or(u64::MAX);
        let record = flag("--record");
        let started = Instant::now();

        // The index: what the address pins it to (its last part, or --pin), then nothing in it taken on trust.
        let body = (0..3).find_map(|n| { if n > 0 { std::thread::sleep(Duration::from_millis(500)) } get(&base, "index.json", 10, 64 << 20).map_err(|e| eprintln!("{e}")).ok() }).unwrap_or_else(|| stop("no index.json at the address"));
        let pin = flag("--pin").or_else(|| pin_of(&base).map(str::to_string));
        match &pin {
            Some(pin) if !sha256_hex(&body).starts_with(pin.as_str()) => stop(&format!("index.json is not the one the address names ({pin})")),
            Some(_) => {}
            None => eprintln!("image-reader: the address pins nothing; the image is whatever is there now"),
        }
        let index = Index::parse(&body).unwrap_or_else(|e| stop(&e));
        if pin.is_some() && index.sums.is_none() { stop("a pinned index without its pieces' checksums") }
        let (size, piece, count, name) = (index.size, index.piece, index.count() as usize, index.name.clone());
        std::fs::create_dir_all(cache_dir).unwrap_or_else(|e| stop(&format!("{cache_dir}: {e}")));
        let cache = File::options().read(true).write(true).create(true).truncate(true).open(format!("{cache_dir}/pieces")).unwrap_or_else(|e| stop(&format!("{cache_dir}/pieces: {e}")));
        cache.set_len(size).unwrap_or_else(|e| stop(&format!("{cache_dir}/pieces: {e}")));
        let shared = Arc::new(Shared { base: base.clone(), index, cache, ahead, keep_free, max, pinned: pin.is_some(),
            state: Mutex::new(State { pieces: vec![Piece::Missing; count], urgent: VecDeque::new(), later: VecDeque::new(), used: vec![], seen: vec![false; count], held: vec![0; count], kept: VecDeque::new(), kept_bytes: 0 }),
            changed: Condvar::new(), fetched_bytes: AtomicU64::new(0), fetched_pieces: AtomicU64::new(0), asked_pieces: AtomicU64::new(0), dropped_pieces: AtomicU64::new(0), failed_pieces: AtomicU64::new(0) });

        let mut fetchers = 0;
        for _ in 0..parallel { let s = shared.clone(); if std::thread::Builder::new().stack_size(512 * 1024).spawn(move || s.fetch_loop()).is_ok() { fetchers += 1 } }
        if fetchers == 0 { stop("no thread could be started") }

        // The pieces most jobs need, fetched from the start (after anything asked for); the image does not wait for
        // the list. One the index names is used only if it is that one.
        { let s = shared.clone();
          let _ = std::thread::Builder::new().spawn(move || {
            let Ok(body) = get(&s.base, "hot.json", 10, 16 << 20) else { return };
            if s.index.hot.as_ref().is_some_and(|h| *h != sha256_hex(&body)) { return eprintln!("hot.json is not the one the index names: not used") }
            if s.pinned && s.index.hot.is_none() { return eprintln!("hot.json is not named by the pinned index: not used") }
            let Ok(hot) = serde_json::from_slice::<Vec<u64>>(&body) else { return };
            let mut st = s.state.lock().unwrap();
            let mut n = 0;
            for i in hot.into_iter().filter(|i| (*i as usize) < count) { if st.pieces[i as usize] == Piece::Missing { st.pieces[i as usize] = Piece::Queued; st.later.push_back(i); n += 1 } }
            eprintln!("hot: {n} pieces");
            s.changed.notify_all();
          }); }

        // What was fetched (and asked for, when recording), written every second while it changes; and room made
        // when something else has filled the disk meanwhile.
        { let (s, dir) = (shared.clone(), cache_dir.clone());
          let _ = std::thread::Builder::new().spawn(move || { let mut last = u64::MAX; loop {
            std::thread::sleep(Duration::from_secs(1));
            s.make_room(&mut s.state.lock().unwrap(), false);
            let (pieces, dropped) = (s.fetched_pieces.load(Ordering::Relaxed), s.dropped_pieces.load(Ordering::Relaxed));
            let mark = pieces + dropped + ((s.state.lock().unwrap().used.len() as u64) << 32);
            if mark == last { continue }
            last = mark;
            let stats = serde_json::json!({ "pieces": pieces, "bytes": s.fetched_bytes.load(Ordering::Relaxed), "reads_that_waited": s.asked_pieces.load(Ordering::Relaxed),
                "dropped": dropped, "failed": s.failed_pieces.load(Ordering::Relaxed), "kept_bytes": s.state.lock().unwrap().kept_bytes, "seconds": started.elapsed().as_secs() });
            let _ = std::fs::write(format!("{dir}/stats.json"), stats.to_string());
            if let Some(file) = &record { let used = s.state.lock().unwrap().used.clone(); let _ = std::fs::write(file, serde_json::to_string(&used).unwrap_or_default()); }
          } }); }

        let mut config = Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("image-reader".into()), MountOption::DefaultPermissions];
        config.acl = SessionACL::All;
        config.n_threads = Some(4);
        config.clone_fd = true;
        eprintln!("{name}: {size} bytes in {count} pieces of {piece}, from {base}{}", if shared.index.sums.is_some() { ", each checked" } else { ", unchecked (an index without checksums)" });
        if let Err(e) = fuser::mount(Fs { shared, name }, at, &config) { stop(&format!("mount: {e}")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_is_taken_only_as_far_as_it_makes_sense() {
        let ok = serde_json::json!({ "name": "full-24.04.sqsh", "size": 10 * 1024 * 1024, "piece": 4 * 1024 * 1024 });
        let parse = |v: &serde_json::Value| Index::parse(v.to_string().as_bytes());
        assert_eq!(parse(&ok).unwrap().count(), 3);
        let with = |k: &str, v: serde_json::Value| { let mut i = ok.clone(); i[k] = v; i };
        for (bad, why) in [(with("name", "../etc/passwd".into()), "plain file name"), (with("name", ".hidden".into()), "plain file name"), (with("piece", 0.into()), "pieces of 0"),
            (with("piece", (1u64 << 30).into()), "pieces of"), (with("size", 0.into()), "0 bytes"), (with("size", (1u64 << 50).into()), "up to 1 TiB"),
            (with("sha256", serde_json::json!(["ab"])), "checksums for 3 pieces"), (with("hot", "nope".into()), "checksum that is not one")] {
            assert!(parse(&bad).unwrap_err().contains(why), "{bad}");
        }
        let sums: Vec<String> = (0..3).map(|i| sha256_hex(&[i])).collect();
        assert_eq!(parse(&with("sha256", serde_json::json!(sums))).unwrap().sums.unwrap().len(), 3);
    }

    #[test]
    fn an_address_ending_in_a_checksum_pins_its_index() {
        assert_eq!(pin_of("https://images.example.com/ubuntu-24.04/0123456789abcdef0123456789abcdef/"), Some("0123456789abcdef0123456789abcdef"));
        for plain in ["https://images.example.com/ubuntu-24.04", "https://images.example.com/full-24.04-4m", "https://images.example.com/DEADBEEFDEADBEEF", "https://x.com/abc123"] { assert_eq!(pin_of(plain), None, "{plain}") }
    }

    #[test]
    fn pack_writes_pieces_an_index_with_their_checksums_and_names_the_directory_by_the_index() {
        let dir = std::env::temp_dir().join(format!("image-reader-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let d = |n: &str| dir.join(n).to_string_lossy().to_string();
        std::fs::write(d("small.sqsh"), &image).unwrap();
        std::fs::write(d("hot.json"), "[2, 0, 2, 99]").unwrap();
        std::fs::write(d("reader"), b"program").unwrap();
        let args: Vec<String> = [d("small.sqsh"), d("out"), "--piece".into(), "65536".into(), "--hot".into(), d("hot.json"), "--reader".into(), d("reader")].to_vec();
        let said = pack(&args).unwrap();
        let to = said.lines().next().unwrap().to_string();
        let body = std::fs::read(format!("{to}/index.json")).unwrap();
        // The directory's name is (the start of) the index's checksum: its address pins it.
        assert_eq!(pin_of(&format!("https://x.example/{}", to.rsplit('/').next().unwrap())), Some(&sha256_hex(&body)[..32]));
        let index = Index::parse(&body).unwrap();
        assert_eq!((index.name.as_str(), index.size, index.piece, index.count()), ("small.sqsh", 200_000, 65_536, 4));
        // Each piece is there, whole, and is what the index says; the last one is the rest.
        let mut back = vec![];
        for (i, sum) in index.sums.as_ref().unwrap().iter().enumerate() { let p = std::fs::read(format!("{to}/p/{i:06}")).unwrap(); assert_eq!(&sha256_hex(&p), sum); back.extend(p) }
        assert_eq!(back, image);
        // The hot list as recorded, each piece once, only ones there are; it and the reader are named by the index.
        let hot = std::fs::read(format!("{to}/hot.json")).unwrap();
        assert_eq!((String::from_utf8_lossy(&hot).to_string(), index.hot.as_deref()), ("[2,0]".to_string(), Some(sha256_hex(&hot).as_str())));
        assert_eq!(index.readers, vec![("x86_64".to_string(), sha256_hex(b"program"))]);
        // Packed again: the same place (nothing in it depends on when).
        assert_eq!(pack(&args).unwrap(), said);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
