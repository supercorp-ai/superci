//! containerd's local content store (`plugins/content/local`), file for file: blobs at `blobs/<algorithm>/<hex>`,
//! each write in progress at `ingest/<sha256 of its ref>/` (`data`, `ref`, `startedat`, `updatedat`, `total`), one
//! writer per ref at a time, resumable. The same folder containerd's own store keeps, read the same way by it: with
//! SuperCI's store turned off, containerd's finds every blob where it left it. Labels are kept beside (containerd's
//! keeps them in its own database above this store; its tests ask a store for them too).

use crate::error::{db, Error, Result};
use crate::filters::Filters;
use crate::meta::Nanos;
use redb::{ReadableDatabase, TableDefinition};
use sha2::Digest as _;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const LABELS: TableDefinition<&str, &[u8]> = TableDefinition::new("labels");

/// A content digest, `algorithm:hex`, as go-digest validates it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest { pub algorithm: Algorithm, pub hex: String }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Algorithm { Sha256, Sha384, Sha512 }

impl Algorithm {
    fn name(self) -> &'static str { match self { Algorithm::Sha256 => "sha256", Algorithm::Sha384 => "sha384", Algorithm::Sha512 => "sha512" } }
    fn from_name(s: &str) -> Option<Algorithm> { match s { "sha256" => Some(Algorithm::Sha256), "sha384" => Some(Algorithm::Sha384), "sha512" => Some(Algorithm::Sha512), _ => None } }
    fn hex_len(self) -> usize { match self { Algorithm::Sha256 => 64, Algorithm::Sha384 => 96, Algorithm::Sha512 => 128 } }
    pub fn hasher(self) -> Hasher { match self { Algorithm::Sha256 => Hasher::Sha256(Default::default()), Algorithm::Sha384 => Hasher::Sha384(Default::default()), Algorithm::Sha512 => Hasher::Sha512(Default::default()) } }
}

impl Digest {
    /// go-digest's `Parse`, with its errors.
    pub fn parse(s: &str) -> std::result::Result<Digest, &'static str> {
        let i = s.find(':').unwrap_or(0);
        if i == 0 || i + 1 == s.len() { return Err("invalid checksum digest format") }
        let (alg, hex) = (&s[..i], &s[i + 1..]);
        let Some(algorithm) = Algorithm::from_name(alg) else {
            // (go-digest's own pattern for a digest it has no algorithm for.)
            let ok = alg.split(['+', '.', '_', '-']).all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
                && hex.len() >= 32 && hex.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'=' || b == b'_' || b == b'-');
            return Err(if ok { "unsupported digest algorithm" } else { "invalid checksum digest format" });
        };
        if hex.len() != algorithm.hex_len() { return Err("invalid checksum digest length") }
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) { return Err("invalid checksum digest format") }
        Ok(Digest { algorithm, hex: hex.to_string() })
    }

    pub fn of(algorithm: Algorithm, bytes: &[u8]) -> Digest { let mut h = algorithm.hasher(); h.update(bytes); h.digest() }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "{}:{}", self.algorithm.name(), self.hex) }
}

#[derive(Clone)]
pub enum Hasher { Sha256(sha2::Sha256), Sha384(sha2::Sha384), Sha512(sha2::Sha512) }

impl Hasher {
    pub fn update(&mut self, b: &[u8]) { match self { Hasher::Sha256(h) => h.update(b), Hasher::Sha384(h) => h.update(b), Hasher::Sha512(h) => h.update(b) } }
    fn algorithm(&self) -> Algorithm { match self { Hasher::Sha256(_) => Algorithm::Sha256, Hasher::Sha384(_) => Algorithm::Sha384, Hasher::Sha512(_) => Algorithm::Sha512 } }
    /// The digest of what was given so far (the hasher goes on).
    pub fn digest(&self) -> Digest {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let h = match self { Hasher::Sha256(h) => hex(&h.clone().finalize()), Hasher::Sha384(h) => hex(&h.clone().finalize()), Hasher::Sha512(h) => hex(&h.clone().finalize()) };
        Digest { algorithm: self.algorithm(), hex: h }
    }
}

/// A blob as containerd's API says it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Info { pub digest: String, pub size: i64, pub created: Nanos, pub updated: Nanos, pub labels: BTreeMap<String, String> }

/// A write in progress.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status { pub ref_: String, pub offset: i64, pub total: i64, pub started: Nanos, pub updated: Nanos, pub expected: String }

pub struct Store {
    pub root: PathBuf,
    labels: redb::Database,
    /// The refs being written, since when.
    locks: Mutex<HashMap<String, std::time::SystemTime>>,
}

impl Store {
    /// The store at `root` (containerd's content folder), its labels in `labels` (a file of SuperCI's own).
    pub fn open(root: &Path, labels: &Path) -> Result<Store> {
        std::fs::create_dir_all(root).and_then(crate::faults::pass)?;
        let labels = redb::Database::create(labels).and_then(crate::faults::pass).map_err(db)?;
        let tx = labels.begin_write().map_err(db).and_then(crate::faults::pass)?;
        tx.open_table(LABELS).map_err(db).and_then(crate::faults::pass)?;
        tx.commit().map_err(db).and_then(crate::faults::pass)?;
        Ok(Store { root: root.to_path_buf(), labels, locks: Mutex::new(HashMap::new()) })
    }

    pub fn blob_path(&self, d: &Digest) -> PathBuf { self.root.join("blobs").join(d.algorithm.name()).join(&d.hex) }

    fn ingest_root(&self, r: &str) -> PathBuf { self.root.join("ingest").join(Digest::of(Algorithm::Sha256, r.as_bytes()).hex) }

    fn get_labels(&self, d: &Digest) -> Result<BTreeMap<String, String>> {
        let tx = self.labels.begin_read().map_err(db).and_then(crate::faults::pass)?;
        let t = tx.open_table(LABELS).map_err(db).and_then(crate::faults::pass)?;
        let got = redb::ReadableTable::get(&t, d.to_string().as_str()).and_then(crate::faults::pass).map_err(db)?;
        let labels = match got { Some(v) => serde_json::from_slice(v.value()).map_err(db).and_then(crate::faults::pass)?, None => BTreeMap::new() };
        Ok(labels)
    }

    fn set_labels(&self, d: &Digest, labels: &BTreeMap<String, String>) -> Result<()> {
        let tx = self.labels.begin_write().map_err(db).and_then(crate::faults::pass)?;
        {
            let mut t = tx.open_table(LABELS).map_err(db).and_then(crate::faults::pass)?;
            let key = d.to_string();
            if labels.is_empty() { t.remove(key.as_str()).map_err(db).and_then(crate::faults::pass)?; } else { t.insert(key.as_str(), serde_json::to_vec(labels).map_err(db).and_then(crate::faults::pass)?.as_slice()).map_err(db).and_then(crate::faults::pass)?; }
        }
        tx.commit().map_err(db)
    }

    pub fn info(&self, d: &Digest) -> Result<Info> {
        let m = match crate::faults::check(std::fs::metadata(self.blob_path(d))) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound(format!("content {d}"))),
            Err(e) => return Err(e.into()),
        };
        Ok(self.info_of(d, &m, self.get_labels(d).and_then(crate::faults::pass)?))
    }

    fn info_of(&self, d: &Digest, m: &std::fs::Metadata, labels: BTreeMap<String, String>) -> Info {
        use std::os::unix::fs::MetadataExt;
        Info { digest: d.to_string(), size: m.len() as i64, created: m.mtime() * 1_000_000_000 + m.mtime_nsec(), updated: m.atime() * 1_000_000_000 + m.atime_nsec(), labels }
    }

    /// A blob to read, and its size.
    pub fn open_blob(&self, d: &Digest) -> Result<(std::fs::File, u64)> {
        let p = self.blob_path(d);
        let not_found = |e: std::io::Error| if e.kind() == std::io::ErrorKind::NotFound { Error::NotFound("blob not found".into()).context(format!("blob {d} expected at {}", p.display())) } else { e.into() };
        let size = std::fs::metadata(&p).and_then(crate::faults::pass).map_err(not_found)?.len();
        let f = std::fs::File::open(&p).and_then(crate::faults::pass).map_err(not_found)?;
        Ok((f, size))
    }

    /// A blob taken away (none there is no error: containerd's store says nothing either).
    pub fn delete(&self, d: &Digest) -> Result<()> {
        match crate::faults::check(std::fs::remove_file(self.blob_path(d))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// A blob's labels changed: all of them, or the `labels` and `labels.KEY` paths given (a key given no value goes).
    pub fn update(&self, info: &Info, paths: &[String]) -> Result<Info> {
        // (The service checks a digest before it asks.)
        let d = Digest::parse(&info.digest).expect("a digest checked by the service");
        let p = self.blob_path(&d);
        let m = match crate::faults::check(std::fs::metadata(&p)) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound(format!("content {}", info.digest))),
            Err(e) => return Err(e.into()),
        };
        let (mut all, mut changed) = (false, BTreeMap::new());
        if paths.is_empty() { all = true; changed = info.labels.clone() } else {
            paths.iter().try_for_each(|path| {
                if let Some(key) = path.strip_prefix("labels.") {
                    changed.insert(key.to_string(), info.labels.get(key).cloned().unwrap_or_default());
                    return Ok(())
                }
                match path.as_str() {
                    "labels" => { all = true; changed = info.labels.clone(); Ok(()) }
                    _ => Err(Error::InvalidArgument(format!("cannot update {path:?} field on content info {:?}", info.digest))),
                }
            }).and_then(crate::faults::pass)?;
        }
        let labels = if all { changed.retain(|_, v| !v.is_empty()); changed } else {
            let mut l = self.get_labels(&d).and_then(crate::faults::pass)?;
            changed.into_iter().for_each(|(k, v)| { if v.is_empty() { l.remove(&k); } else { l.insert(k, v); } });
            l
        };
        self.set_labels(&d, &labels).and_then(crate::faults::pass)?;
        let mut out = self.info_of(&d, &m, labels);
        out.updated = crate::meta::now();
        // (Its access time is when it was last changed, as containerd's store keeps it.)
        let _ = set_times(&p, out.updated, out.created);
        Ok(out)
    }

    /// Each blob the filters match (by `digest` and `labels.KEY`).
    pub fn walk(&self, filters: &[String], mut f: impl FnMut(Info) -> Result<()>) -> Result<()> {
        let filters = Filters::parse_all(filters).map_err(Error::InvalidArgument).and_then(crate::faults::pass)?;
        let root = self.root.join("blobs");
        let Ok(algs) = std::fs::read_dir(&root) else { return Ok(()) };
        let mut algs: Vec<_> = algs.filter_map(|e| e.ok()).collect();
        algs.sort_by_key(|e| e.file_name());
        for a in algs {
            let Some(alg) = Algorithm::from_name(&a.file_name().to_string_lossy()) else { continue };
            if !a.file_type().and_then(crate::faults::pass)?.is_dir() { continue }
            let mut blobs: Vec<_> = std::fs::read_dir(a.path()).and_then(crate::faults::pass)?.filter_map(|e| e.ok()).collect();
            blobs.sort_by_key(|e| e.file_name());
            for b in blobs {
                let d = Digest { algorithm: alg, hex: b.file_name().to_string_lossy().into_owned() };
                let m = b.metadata().and_then(crate::faults::pass)?;
                let i = self.info_of(&d, &m, self.get_labels(&d).and_then(crate::faults::pass)?);
                let adapt = |path: &[String]| -> Option<String> {
                    match path.first().map(String::as_str) {
                        Some("digest") => Some(i.digest.clone()),
                        Some("labels") => if i.labels.is_empty() { None } else { i.labels.get(&path[1..].join(".")).cloned() },
                        _ => None,
                    }
                };
                if filters.matches(&adapt) { f(i.clone()).and_then(crate::faults::pass)? }
            }
        }
        Ok(())
    }

    /// A folder for files on their way into the store (`superci-fetch`, beside containerd's own folders, on the
    /// same disk so a file is moved in, not copied).
    pub fn fetch_dir(&self) -> Result<PathBuf> {
        let d = self.root.join("superci-fetch");
        std::fs::create_dir_all(&d).and_then(crate::faults::pass)?;
        Ok(d)
    }

    /// A file fetched whole made the blob `d`, if it is: hashed, moved into place, made read-only (as a commit
    /// does).
    pub fn put_file(&self, d: &Digest, file: &Path) -> Result<()> {
        let got = crate::fetch::hash_file(file, d.algorithm).and_then(crate::faults::pass)?;
        if got != *d {
            let _ = std::fs::remove_file(file);
            return Err(Error::FailedPrecondition(format!("unexpected commit digest {got}, expected {d}")));
        }
        let target = self.blob_path(d);
        std::fs::create_dir_all(target.parent().unwrap()).and_then(crate::faults::pass)?;
        // (Fetched once: the service refuses a write of a blob it serves, so none is here before it.)
        std::fs::rename(file, &target).and_then(crate::faults::pass)?;
        std::fs::File::open(target.parent().unwrap()).and_then(|f| f.sync_all()).and_then(crate::faults::pass)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444));
        Ok(())
    }

    pub fn status(&self, r: &str) -> Result<Status> { status_at(&self.ingest_root(r)) }

    /// Each write in progress the filters match (by `ref`).
    pub fn list_statuses(&self, filters: &[String]) -> Result<Vec<Status>> {
        let dir = self.root.join("ingest");
        let entries = match crate::faults::check(std::fs::read_dir(&dir)) { Ok(e) => e, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]), Err(e) => return Err(e.into()) };
        let filters = Filters::parse_all(filters).map_err(Error::InvalidArgument).and_then(crate::faults::pass)?;
        let mut out = vec![];
        for e in entries {
            match status_at(&e.and_then(crate::faults::pass)?.path()) {
                Ok(s) => if filters.matches(&|path: &[String]| (path.first().map(String::as_str) == Some("ref")).then(|| s.ref_.clone())) { out.push(s) },
                // (One finishing while the folder is read is no longer a write in progress.)
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// A write's progress thrown away (none there is no error, as containerd's store has it).
    pub fn abort(&self, r: &str) -> Result<()> {
        match crate::faults::check(std::fs::remove_dir_all(self.ingest_root(r))) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()), _ => Ok(()) }
    }

    fn try_lock(&self, r: &str) -> Result<()> {
        let mut locks = self.locks.lock().unwrap();
        if let Some(since) = locks.get(r) {
            let now = std::time::SystemTime::now();
            let held = now.duration_since(*since).unwrap_or_default();
            return Err(Error::Unavailable(format!("ref {r} locked for {:?} (since {})", held, rfc3339(nanos_of(*since)))));
        }
        locks.insert(r.to_string(), std::time::SystemTime::now());
        Ok(())
    }

    fn unlock(&self, r: &str) { self.locks.lock().unwrap().remove(r); }

    /// A writer for `r`, new or going on from where it was left (one at a time for a ref); `expected` there already
    /// is an error.
    pub fn writer<'s>(&'s self, r: &str, total: i64, expected: Option<&Digest>) -> Result<Writer<'s>> {
        // (A ref is never empty: the service refuses a write without one before it gets here.)
        self.try_lock(r).and_then(crate::faults::pass)?;
        match self.writer_locked(r, total, expected) {
            Ok(w) => Ok(w),
            Err(e) => { self.unlock(r); Err(e) }
        }
    }

    fn writer_locked<'s>(&'s self, r: &str, mut total: i64, expected: Option<&Digest>) -> Result<Writer<'s>> {
        if let Some(d) = expected {
            if self.blob_path(d).exists() { return Err(Error::AlreadyExists(format!("content {d}"))) }
        }
        let path = self.ingest_root(r);
        let mut hasher = expected.map(|d| d.algorithm).unwrap_or(Algorithm::Sha256).hasher();
        let (mut offset, mut started, mut updated) = (0i64, 0, 0);
        let mut found = false;
        std::fs::create_dir_all(self.root.join("ingest")).and_then(crate::faults::pass)?;
        match crate::faults::check(std::fs::DirBuilder::new().create(&path)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match self.resume(&path, r, total, &mut hasher) {
                Ok(s) => { found = true; (started, updated, total, offset) = (s.started, s.updated, s.total, s.offset) }
                Err(e) => eprintln!("failed to resume the status from path {}: {e}. will recreate them", path.display()),
            },
            Err(e) => return Err(e.into()),
        }
        if !found {
            started = crate::meta::now();
            updated = started;
            std::fs::write(path.join("ref"), r).and_then(crate::faults::pass)?;
            write_to_completion(&path.join("startedat"), rfc3339(started).as_bytes()).and_then(crate::faults::pass)?;
            write_to_completion(&path.join("updatedat"), rfc3339(started).as_bytes()).and_then(crate::faults::pass)?;
            if total > 0 { std::fs::write(path.join("total"), total.to_string()).and_then(crate::faults::pass)? }
        }
        let mut fp = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(path.join("data")).and_then(crate::faults::pass).map_err(|e| Error::Other(format!("failed to open data file: {e}")))?;
        fp.seek(std::io::SeekFrom::Start(offset as u64)).and_then(crate::faults::pass).map_err(|e| Error::Other(format!("could not seek to current write offset: {e}")))?;
        Ok(Writer { store: self, fp: Some(fp), path, ref_: r.to_string(), offset, total, hasher, started, updated })
    }

    fn resume(&self, path: &Path, r: &str, total: i64, hasher: &mut Hasher) -> Result<Status> {
        let mut s = status_at(path).and_then(crate::faults::pass).map_err(|e| e.context("failed reading status of resume write"))?;
        if r != s.ref_ { return Err(Error::Other(format!("ref key does not match: {r} != {}", s.ref_))) }
        if total > 0 && s.total > 0 && total != s.total { return Err(Error::Other(format!("provided total differs from status: {total} != {}", s.total))) }
        let mut f = std::fs::File::open(path.join("data")).and_then(crate::faults::pass)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut n: i64 = 0;
        loop { let k = f.read(&mut buf).and_then(crate::faults::pass)?; if k == 0 { break } hasher.update(&buf[..k]); n += k as i64 }
        s.offset = n;
        Ok(s)
    }
}

/// A write in progress: what is written goes to its `data` file, counted and hashed as it comes.
pub struct Writer<'s> {
    store: &'s Store,
    fp: Option<std::fs::File>,
    path: PathBuf,
    ref_: String,
    offset: i64,
    total: i64,
    hasher: Hasher,
    started: Nanos,
    updated: Nanos,
}

impl Writer<'_> {
    pub fn status(&self) -> Status { Status { ref_: self.ref_.clone(), offset: self.offset, total: self.total, started: self.started, updated: self.updated, expected: String::new() } }

    pub fn digest(&self) -> Digest { self.hasher.digest() }

    pub fn write(&mut self, b: &[u8]) -> Result<usize> {
        let Some(fp) = self.fp.as_mut() else { return Err(Error::FailedPrecondition("cannot write on closed writer".into())) };
        fp.write_all(b).and_then(crate::faults::pass)?;
        self.hasher.update(b);
        self.offset += b.len() as i64;
        self.updated = crate::meta::now();
        Ok(b.len())
    }

    /// Back to nothing written (containerd's store truncates only to nothing).
    pub fn truncate(&mut self) -> Result<()> {
        self.offset = 0;
        self.hasher = self.hasher.algorithm().hasher();
        let Some(fp) = self.fp.as_mut() else { return Err(Error::FailedPrecondition("cannot truncate a closed writer".into())) };
        fp.seek(std::io::SeekFrom::Start(0)).and_then(crate::faults::pass)?;
        fp.set_len(0).and_then(crate::faults::pass)?;
        Ok(())
    }

    /// What was written made a blob: its size and digest checked (when given), moved into place, made read-only.
    /// The writer is done whatever the outcome.
    pub fn commit(&mut self, size: i64, expected: Option<&Digest>, labels: Option<&BTreeMap<String, String>>) -> Result<()> {
        self.commit_str(size, &expected.map(|d| d.to_string()).unwrap_or_default(), labels)
    }

    /// The same, the digest expected as a caller gave it ("" for none; one that is no digest never matches).
    pub fn commit_str(&mut self, size: i64, expected: &str, labels: Option<&BTreeMap<String, String>>) -> Result<()> {
        let r = self.commit_inner(size, expected, labels);
        self.store.unlock(&self.ref_);
        r
    }

    fn commit_inner(&mut self, size: i64, expected: &str, labels: Option<&BTreeMap<String, String>>) -> Result<()> {
        let Some(fp) = self.fp.take() else { return Err(Error::FailedPrecondition("cannot commit on closed writer".into())) };
        fp.sync_all().and_then(crate::faults::pass).map_err(|e| Error::Other(format!("sync failed: {e}")))?;
        let m = fp.metadata().and_then(crate::faults::pass).map_err(|e| Error::Other(format!("stat on ingest file failed: {e}")))?;
        drop(fp);
        if size > 0 && size != m.len() as i64 { return Err(Error::FailedPrecondition(format!("unexpected commit size {}, expected {size}", m.len()))) }
        let mut d = self.hasher.digest();
        if !expected.is_empty() && Digest::parse(expected).is_err() { return Err(Error::FailedPrecondition(format!("unexpected commit digest {d}, expected {expected}"))) }
        if let Ok(e) = Digest::parse(expected) {
            let e = &e;
            if e.algorithm != d.algorithm {
                // (Opened with no digest to say its algorithm: hashed again as the commit asks.)
                let mut h = e.algorithm.hasher();
                let mut f = std::fs::File::open(self.path.join("data")).and_then(crate::faults::pass).map_err(|err| Error::Other(format!("failed to open ingest data for re-hashing to {}: {err}", e.algorithm.name())))?;
                let mut buf = vec![0u8; 1 << 20];
                loop { let k = f.read(&mut buf).and_then(crate::faults::pass)?; if k == 0 { break } h.update(&buf[..k]) }
                self.hasher = h;
                d = self.hasher.digest();
            }
            if *e != d { return Err(Error::FailedPrecondition(format!("unexpected commit digest {d}, expected {e}"))) }
        }
        let ingest = self.path.join("data");
        let target = self.store.blob_path(&d);
        std::fs::create_dir_all(target.parent().unwrap()).and_then(crate::faults::pass)?;
        if target.exists() {
            if let Err(e) = crate::faults::check(std::fs::remove_dir_all(&self.path)) { eprintln!("failed to remove ingest directory {}: {e}", self.path.display()) }
            return Err(Error::AlreadyExists(format!("content {d}")));
        }
        std::fs::rename(&ingest, &target).and_then(crate::faults::pass)?;
        std::fs::File::open(target.parent().unwrap()).and_then(|f| f.sync_all()).and_then(crate::faults::pass).map_err(|e| Error::Other(format!("failed to sync dir {}: {e}", target.parent().unwrap().display()))).and_then(crate::faults::pass)?;
        // From here the blob is in the store: what fails is said, not returned (it cannot be taken back).
        let t = crate::meta::now();
        if let Err(e) = crate::faults::check(set_times(&target, t, t)) { eprintln!("failed to change file time to commit time for {d}: {e}") }
        if let Err(e) = crate::faults::check(std::fs::remove_dir_all(&self.path)) { eprintln!("failed to remove ingest directory {}: {e}", self.path.display()) }
        if let Some(l) = labels { if let Err(e) = crate::faults::check(self.store.set_labels(&d, l)) { eprintln!("failed to set labels for {d}: {e}") } }
        use std::os::unix::fs::PermissionsExt;
        let mode = (m.permissions().mode() & 0o777) & !0o333;
        if let Err(e) = crate::faults::check(std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))) { eprintln!("failed to make {d} read-only: {e}") }
        Ok(())
    }

    /// The writer let go, what was written kept for a writer after it.
    pub fn close(&mut self) {
        if let Some(fp) = self.fp.take() {
            let _ = fp.sync_all();
            drop(fp);
            let _ = write_to_completion(&self.path.join("updatedat"), rfc3339(self.updated).as_bytes());
            self.store.unlock(&self.ref_);
        }
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) { self.close() }
}

fn status_at(path: &Path) -> Result<Status> {
    let nf = |e: std::io::Error| if e.kind() == std::io::ErrorKind::NotFound { Error::NotFound(e.to_string()) } else { e.into() };
    let m = std::fs::metadata(path.join("data")).and_then(crate::faults::pass).map_err(nf)?;
    let r = std::fs::read_to_string(path.join("ref")).and_then(crate::faults::pass).map_err(nf)?;
    let read_time = |name: &str| -> Result<Nanos> {
        let t = std::fs::read_to_string(path.join(name)).map_err(nf).and_then(crate::faults::pass).map_err(|e| e.context(format!("could not read {name}")))?;
        parse_rfc3339(&t).ok_or_else(|| Error::Other(format!("could not read {name}: could not parse timestamp file {}", path.join(name).display())))
    };
    let started = read_time("startedat").and_then(crate::faults::pass)?;
    let mut updated = read_time("updatedat").and_then(crate::faults::pass)?;
    use std::os::unix::fs::MetadataExt;
    let mtime = m.mtime() * 1_000_000_000 + m.mtime_nsec();
    // (updatedat is not written on every write: the data file's own time may be later.)
    if mtime > updated { updated = mtime }
    let total = std::fs::read_to_string(path.join("total")).ok().and_then(|t| t.parse().ok()).unwrap_or(0);
    Ok(Status { ref_: r, offset: m.len() as i64, total, started, updated, expected: String::new() })
}

/// Written whole or not at all: to a file beside it, synced, then moved over it.
fn write_to_completion(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&tmp).and_then(crate::faults::pass).map_err(|e| Error::Other(format!("create tmp file: {e}")))?;
    let w = f.write_all(data).and_then(|_| f.sync_all());
    drop(f);
    w.and_then(crate::faults::pass).map_err(|e| Error::Other(format!("write tmp file: {e}")))?;
    std::fs::rename(&tmp, path).and_then(crate::faults::pass).map_err(|e| Error::Other(format!("rename tmp file: {e}")))
}

/// A file's access and change times set by its path (Go's `os.Chtimes`), read-only as a blob is.
fn set_times(p: &Path, atime: Nanos, mtime: Nanos) -> std::io::Result<()> {
    let at = |n: Nanos| std::time::UNIX_EPOCH + std::time::Duration::from_nanos(n.max(0) as u64);
    std::fs::File::open(p).and_then(crate::faults::pass)?.set_times(std::fs::FileTimes::new().set_accessed(at(atime)).set_modified(at(mtime)))
}

fn nanos_of(t: std::time::SystemTime) -> Nanos { t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0) }

/// A time as Go's `Time.MarshalText` writes it (RFC 3339, nanoseconds without trailing zeros), in UTC.
pub fn rfc3339(n: Nanos) -> String {
    let (secs, nanos) = (n.div_euclid(1_000_000_000), n.rem_euclid(1_000_000_000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let frac = if nanos == 0 { String::new() } else { format!(".{:09}", nanos).trim_end_matches('0').to_string() };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// RFC 3339 as Go's `Time.UnmarshalText` reads it: a fraction of a second and an offset (`Z` or `±hh:mm`).
pub fn parse_rfc3339(s: &str) -> Option<Nanos> {
    let s = s.trim_end_matches('\n');
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't') || b[13] != b':' || b[16] != b':' { return None }
    let num = |r: std::ops::Range<usize>| s.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut nanos = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() { j += 1 }
        if j == start { return None }
        let digits = &s[start..j.min(start + 9)];
        // (Nine digits at most: a number.)
        nanos = digits.parse::<i64>().unwrap_or(0) * 10i64.pow(9 - digits.len() as u32);
        i = j;
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        c @ (b'+' | b'-') if b.len() == i + 6 && b[i + 3] == b':' => { let v = num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60; if *c == b'-' { -v } else { v } }
        _ => return None,
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 { return None }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset;
    Some(secs * 1_000_000_000 + nanos)
}

// (Howard Hinnant's day-count algorithms.)
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days and dates both ways across every leap rule (Python's calendar's numbers), and a time's parts at their
    /// edges: the 23rd hour and a leap second taken, the 24th hour and a 61st second not.
    #[test]
    fn days_and_times_hold_across_every_leap_rule_and_at_their_edges() {
        for (days, date) in [(0, (1970, 1, 1)), (11_016, (2000, 2, 29)), (11_017, (2000, 3, 1)), (47_540, (2100, 2, 28)), (47_541, (2100, 3, 1)), (84_005, (2199, 12, 31)),
            (84_065, (2200, 3, 1)), (157_112, (2400, 2, 28)), (157_113, (2400, 2, 29)), (157_114, (2400, 3, 1)), (157_419, (2400, 12, 31)), (-1, (1969, 12, 31)), (31, (1970, 2, 1)), (59, (1970, 3, 1))] {
            assert_eq!(civil_from_days(days), date, "{days}");
            assert_eq!(days_from_civil(date.0, date.1, date.2), days, "{date:?}");
        }
        assert_eq!(parse_rfc3339("2100-02-28T23:59:60Z"), Some((47_540 * 86_400 + 86_400) * 1_000_000_000));
        assert_eq!(parse_rfc3339("2100-03-01T00:00:00Z"), Some(47_541 * 86_400 * 1_000_000_000));
        assert_eq!((parse_rfc3339("2100-02-28T24:00:00Z"), parse_rfc3339("2100-02-28T23:59:61Z")), (None, None));
    }

    fn store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("superci-layers-cs-{}-{}", std::process::id(), crate::meta::now()));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("content"), &dir.join("labels.db")).unwrap(), dir)
    }

    #[test]
    fn digests_parse_as_go_digest_says() {
        let h = "a".repeat(64);
        assert_eq!(Digest::parse(&format!("sha256:{h}")).unwrap().to_string(), format!("sha256:{h}"));
        assert_eq!(Digest::parse("sha256"), Err("invalid checksum digest format"));
        assert_eq!(Digest::parse("sha256:"), Err("invalid checksum digest format"));
        assert_eq!(Digest::parse("sha256:abc"), Err("invalid checksum digest length"));
        assert_eq!(Digest::parse(&format!("sha256:{}", "A".repeat(64))), Err("invalid checksum digest format"));
        assert_eq!(Digest::parse(&format!("md5:{}", "a".repeat(32))), Err("unsupported digest algorithm"));
        assert!(Digest::parse(&format!("sha512:{}", "0".repeat(128))).is_ok());
        assert_eq!(Digest::of(Algorithm::Sha256, b"").hex, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn times_are_written_and_read_as_go_does() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_046_400_123_000_000), "2026-11-19T00:00:00.123Z");
        assert_eq!(parse_rfc3339("2026-11-19T00:00:00.123Z"), Some(1_795_046_400_123_000_000));
        assert_eq!(parse_rfc3339("2026-11-19T03:00:00.123456789+03:00\n"), Some(1_795_046_400_123_456_789), "containerd writes local time with its offset");
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00-01:30"), Some(days_from_civil(2024, 2, 29) * 86_400_000_000_000 + (13 * 3600 + 1800) * 1_000_000_000));
        assert!(parse_rfc3339("yesterday").is_none() && parse_rfc3339("2026-11-19T00:00:00").is_none());
    }

    #[test]
    fn a_write_resumes_commits_once_and_is_locked_while_open() {
        let (s, dir) = store();
        let data = b"hello world, a blob";
        let d = Digest::of(Algorithm::Sha256, data);
        let mut w = s.writer("r1", data.len() as i64, Some(&d)).unwrap();
        assert!(matches!(s.writer("r1", 0, None).err(), Some(Error::Unavailable(m)) if m.starts_with("ref r1 locked for")));
        w.write(&data[..5]).unwrap();
        w.close();
        // Resumed where it was left, with its hash; the total kept.
        let mut w = s.writer("r1", 0, Some(&d)).unwrap();
        assert_eq!((w.status().offset, w.status().total), (5, data.len() as i64));
        assert_eq!(s.status("r1").unwrap().ref_, "r1");
        assert_eq!(s.list_statuses(&["ref==r1".to_string()]).unwrap().len(), 1);
        w.write(&data[5..]).unwrap();
        assert!(matches!(w.commit(3, Some(&d), None), Err(Error::FailedPrecondition(m)) if m == format!("unexpected commit size {}, expected 3", data.len())));
        // The lock is let go and the ingest kept after a failed commit; a new writer finishes it.
        let mut w = s.writer("r1", 0, Some(&d)).unwrap();
        assert_eq!(w.status().offset, data.len() as i64);
        w.commit(data.len() as i64, Some(&d), Some(&[("a".to_string(), "1".to_string())].into_iter().collect())).unwrap();
        let info = s.info(&d).unwrap();
        assert_eq!((info.size, info.labels.get("a").map(String::as_str)), (data.len() as i64, Some("1")));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(s.blob_path(&d)).unwrap().permissions().mode() & 0o222, 0, "read-only once in the store");
        assert!(s.status("r1").unwrap_err().is_not_found(), "its ingest gone");
        assert!(s.writer("r2", 0, Some(&d)).err().unwrap().to_string().ends_with("already exists"));
        // Another write of the same bytes, with no digest said: already there at its commit.
        let mut w = s.writer("r3", 0, None).unwrap();
        w.write(data).unwrap();
        assert!(w.commit(0, None, None).unwrap_err().to_string().ends_with("already exists"));
        let mut w = s.writer("r4", 0, None).unwrap();
        w.write(b"other").unwrap();
        assert!(matches!(w.commit(0, Some(&d), None), Err(Error::FailedPrecondition(m)) if m.starts_with("unexpected commit digest")));
        s.abort("r4").unwrap();
        s.abort("never").unwrap();
        // Labels by path, filters on them, a blob taken away (twice is no error).
        let mut i = info.clone();
        i.labels = [("b".to_string(), "2".to_string())].into_iter().collect();
        assert_eq!(s.update(&i, &["labels.b".into(), "labels.a".into()]).unwrap().labels, i.labels);
        let found = |f: &str| { let mut n = 0; s.walk(&[f.to_string()], |_| { n += 1; Ok(()) }).unwrap(); n };
        assert_eq!((found("labels.b==2"), found(&format!("digest=={d}")), found("labels.a")), (1, 1, 0));
        // All of them at once (`labels`), and a field that is not one to change.
        i.labels = [("c".to_string(), "3".to_string()), ("gone".to_string(), String::new())].into_iter().collect();
        assert_eq!(s.update(&i, &["labels".into()]).unwrap().labels, [("c".to_string(), "3".to_string())].into_iter().collect());
        assert!(matches!(s.update(&i, &["size".into()]), Err(Error::InvalidArgument(m)) if m.starts_with("cannot update \"size\" field")));
        s.delete(&d).unwrap();
        s.delete(&d).unwrap();
        assert!(s.info(&d).unwrap_err().is_not_found());
        assert!(s.update(&i, &[]).unwrap_err().is_not_found());
        // A write started again from nothing (truncated), then committed as the new bytes; one closed is not truncated.
        let fresh = b"written again from the start";
        let f = Digest::of(Algorithm::Sha256, fresh);
        let mut w = s.writer("r5", 0, None).unwrap();
        w.write(b"some bytes that go").unwrap();
        w.truncate().unwrap();
        assert_eq!(w.status().offset, 0);
        w.write(fresh).unwrap();
        w.commit(fresh.len() as i64, Some(&f), None).unwrap();
        assert_eq!(s.info(&f).unwrap().size, fresh.len() as i64);
        let mut w = s.writer("r6", 0, None).unwrap();
        w.close();
        assert!(matches!(w.truncate(), Err(Error::FailedPrecondition(m)) if m == "cannot truncate a closed writer"));
        // (The service ends a write session at its commit: what is done with a writer after, only here.)
        assert!(matches!(w.write(b"x"), Err(Error::FailedPrecondition(m)) if m == "cannot write on closed writer"));
        assert!(matches!(w.commit(0, None, None), Err(Error::FailedPrecondition(m)) if m == "cannot commit on closed writer"));
        // A file fetched whole that is not the blob it was fetched as: thrown away, and refused.
        let file = s.fetch_dir().unwrap().join("not-it");
        std::fs::write(&file, b"something else").unwrap();
        let other = Digest::of(Algorithm::Sha256, b"the blob asked for");
        assert!(matches!(s.put_file(&other, &file), Err(Error::FailedPrecondition(m)) if m.starts_with("unexpected commit digest")));
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
