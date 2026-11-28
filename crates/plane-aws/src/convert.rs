//! The layer store's converter: the same program as a second function (`SUPERCI_CONVERTER`), with a role that may
//! only read and write the layer store's part of the buckets. Given a layer (the blob a machine put in the bucket's
//! staging after its job), it streams the blob in once: hashed (it must be the digest it was asked for), unpacked
//! (gzip, zstd or a plain tar, as its first bytes say) and hashed again (the layer's diff ID), and the tar handed to
//! `mkfs.erofs` as it comes. The EROFS file and
//! the blob go to the bucket, and what was made (or why nothing was) is written beside them for the control plane,
//! which the sweep reads (`layers/done/`).
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use superci_core::aws::{self, Credentials};
use superci_core::crypto::hex;
use superci_core::io::Http;

/// The options every EROFS file is made with (as the layer store's machines expect: lz4hc in 128 KiB clusters, no
/// inline data, every time and the UUID fixed, so the same layer always makes the same file).
pub const MKFS: [&str; 9] = ["--tar=f", "--aufs", "-zlz4hc", "-C131072", "-Enoinline_data", "-T0", "--mkfs-time", "-U", "39bb3f52-5f38-4d6c-a25a-5ea4cc1d8b11"];

/// A blob larger than this is not converted (the function's disk keeps the blob and the file at once).
pub use superci_core::layers::MOST_BYTES;

/// An upload's part (S3 takes 10,000 at most: 640 GB).
const PART: usize = 64 << 20;

/// What to convert, as the control plane asks.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Job {
    pub digest: String,
    pub size: u64,
    pub region: String,
    pub bucket: String,
    /// Where the blob is: `{"key"}`, in the bucket's staging (put there by the machine that pulled it).
    pub source: Value,
    pub erofs_key: String,
    pub blob_key: String,
    pub done_key: String,
}

/// The blob as it streams in: hashed, and kept on disk when asked.
struct Taken<R> { inner: R, hash: Sha256, bytes: u64, keep: Option<std::fs::File> }

impl<R: Read> Read for Taken<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        if let Some(f) = &mut self.keep { f.write_all(&buf[..n])? }
        Ok(n)
    }
}

/// The tar as it is unpacked: hashed (the diff ID).
struct Hashed<R> { inner: R, hash: Sha256 }

impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

/// How a blob is packed, by its first bytes.
#[derive(Debug, PartialEq)]
pub enum Packing { Gzip, Zstd, Tar }

pub fn packing(first: &[u8]) -> Packing {
    if first.starts_with(&[0x1f, 0x8b]) { Packing::Gzip } else if first.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) { Packing::Zstd } else { Packing::Tar }
}

/// What came of a blob streamed through `mkfs`: its digest and size, the diff ID.
#[derive(Debug, PartialEq)]
pub struct Made { pub digest: String, pub size: u64, pub diff_id: String }

/// Streams `blob` through `mkfs` (a command taking a tar on its input and writing `out`), keeping the blob at `keep`
/// when said. Nothing of a failed run is left at `out`.
pub fn make(blob: impl Read, mkfs: &str, out: &std::path::Path, keep: Option<&std::path::Path>) -> Result<Made, String> {
    let _ = std::fs::remove_file(out);
    let keep = match keep { Some(p) => Some(std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?), None => None };
    let taken = Taken { inner: blob, hash: Sha256::new(), bytes: 0, keep };
    let mut buffered = BufReader::with_capacity(1 << 20, taken);
    let first = buffered.fill_buf().map_err(|e| format!("reading the blob: {e}"))?.to_vec();
    let mut child = Command::new(mkfs).args(MKFS).arg(out).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{mkfs}: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let said = std::thread::spawn(move || { let mut s = String::new(); let _ = BufReader::new(stderr).take(64 << 10).read_to_string(&mut s); s });
    // (Unpacked, then what is left of the blob read too: its digest is of all of it.)
    let streamed = (|| -> Result<(String, BufReader<Taken<_>>), String> {
        let copy = |r: &mut dyn Read, w: &mut dyn Write| std::io::copy(r, w).map_err(|e| format!("unpacking the blob into mkfs.erofs: {e}"));
        Ok(match packing(&first) {
            Packing::Gzip => {
                let mut tar = Hashed { inner: flate2::bufread::MultiGzDecoder::new(buffered), hash: Sha256::new() };
                copy(&mut tar, &mut stdin)?;
                (hex(&tar.hash.finalize()), tar.inner.into_inner())
            }
            Packing::Zstd => {
                // (Its decoder is only memory set aside: making it does not fail.)
                let mut tar = Hashed { inner: zstd::stream::read::Decoder::with_buffer(buffered).expect("a zstd decoder"), hash: Sha256::new() };
                copy(&mut tar, &mut stdin)?;
                (hex(&tar.hash.finalize()), tar.inner.finish())
            }
            Packing::Tar => {
                let mut tar = Hashed { inner: buffered, hash: Sha256::new() };
                copy(&mut tar, &mut stdin)?;
                (hex(&tar.hash.finalize()), tar.inner)
            }
        })
    })();
    drop(stdin);
    // (Its own child, its input closed: waited for, it ends.)
    let status = child.wait().expect("mkfs.erofs, a child of this program");
    let said = said.join().unwrap_or_default();
    let fail = |e: String| { let _ = std::fs::remove_file(out); Err(e) };
    let (diff_id, mut rest) = match streamed { Ok(s) => s, Err(e) => return fail(e) };
    // (Unpacking reads a blob to its end; were anything left that could not be read, its digest would not be the one
    // asked for, and nothing is made of it.)
    let _ = std::io::copy(&mut rest, &mut std::io::sink());
    if !status.success() { return fail(format!("mkfs.erofs refused the layer ({status}): {}", said.trim().lines().last().unwrap_or_default())) }
    // (What was kept was written as it came: a file has nothing more to flush.)
    let taken = rest.into_inner();
    Ok(Made { digest: format!("sha256:{}", hex(&taken.hash.finalize())), size: taken.bytes, diff_id: format!("sha256:{diff_id}") })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).max_redirects(10).timeout_connect(Some(Duration::from_secs(20))).timeout_recv_body(Some(Duration::from_secs(600))).build().into()
}

/// A file's SHA-256, read from disk.
fn file_sha256(path: &std::path::Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
    Ok(format!("sha256:{}", hex(&h.finalize())))
}

fn said(r: superci_core::io::Response, what: &str) -> Result<superci_core::io::Response, String> {
    if superci_core::io::done(r.status) { Ok(r) } else { Err(format!("{what}: S3 answered {} {}", r.status, r.body_text().chars().take(300).collect::<String>())) }
}

/// A file put in the bucket: in one request when small, else in parts (of `part` bytes), four at once.
fn upload(http: &(dyn Http + Sync), creds: &Credentials, region: &str, url: &str, path: &std::path::Path, part: usize) -> Result<(), String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let now = super::now_ms;
    if size <= part as u64 {
        let body = std::fs::read(path).map_err(|e| e.to_string())?;
        said(block_on(aws::s3(http, "PUT", url, region, creds, body, &[], now()))?, "putting the file")?;
        return Ok(())
    }
    let made = said(block_on(aws::s3(http, "POST", &format!("{url}?uploads"), region, creds, vec![], &[], now()))?, "beginning the upload")?;
    let text = made.body_text();
    let id = text.split("<UploadId>").nth(1).and_then(|s| s.split("</UploadId>").next()).ok_or("S3 gave no upload id")?.to_string();
    let parts = size.div_ceil(part as u64) as usize;
    let tags: std::sync::Mutex<Vec<(usize, String)>> = std::sync::Mutex::new(vec![]);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        (0..4).for_each(|_| { s.spawn(|| loop {
                let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n >= parts || failed.lock().unwrap().is_some() { break }
                let one = (|| -> Result<String, String> {
                    use std::os::unix::fs::FileExt;
                    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                    let mut body = vec![0; part.min((size - (n * part) as u64) as usize)];
                    f.read_exact_at(&mut body, (n * part) as u64).map_err(|e| e.to_string())?;
                    // (Asked three times.)
                    let mut tries = 0;
                    loop {
                        tries += 1;
                        let last = match block_on(aws::s3(http, "PUT", &format!("{url}?partNumber={}&uploadId={}", n + 1, aws::rfc3986(&id)), region, creds, body.clone(), &[], now())) {
                            Ok(r) if superci_core::io::done(r.status) => return r.header("etag").map(str::to_string).ok_or_else(|| "S3 gave no ETag".to_string()),
                            Ok(r) => format!("S3 answered {}", r.status),
                            Err(e) => e,
                        };
                        if tries == 3 { return Err(format!("part {}: {last}", n + 1)) }
                    }
                })();
                match one { Ok(tag) => tags.lock().unwrap().push((n + 1, tag)), Err(e) => { *failed.lock().unwrap() = Some(e); break } }
            }); });
    });
    if let Some(e) = failed.into_inner().unwrap() {
        let _ = block_on(aws::s3(http, "DELETE", &format!("{url}?uploadId={}", aws::rfc3986(&id)), region, creds, vec![], &[], now()));
        return Err(e)
    }
    let mut tags = tags.into_inner().unwrap();
    tags.sort();
    let xml = format!("<CompleteMultipartUpload>{}</CompleteMultipartUpload>", tags.iter().map(|(n, t)| format!("<Part><PartNumber>{n}</PartNumber><ETag>{t}</ETag></Part>")).collect::<String>());
    let done = said(block_on(aws::s3(http, "POST", &format!("{url}?uploadId={}", aws::rfc3986(&id)), region, creds, xml.into_bytes(), &[], now()))?, "finishing the upload")?;
    // (S3 may answer 200 and say in the body that it failed.)
    if done.body_text().contains("<Error>") { return Err(format!("finishing the upload: {}", done.body_text().chars().take(300).collect::<String>())) }
    Ok(())
}

/// Converts one layer (see the module): what was made, as written to `done_key`.
pub fn convert(http: &(dyn Http + Sync), creds: &Credentials, job: &Job, mkfs: &str, tmp: &std::path::Path) -> Value {
    let began = Instant::now();
    let bucket_url = |key: &str| aws::s3_url(&job.bucket, &job.region, key);
    let result = (|| -> Result<Value, String> {
        if job.size > MOST_BYTES { return Err(format!("a blob of {} bytes (converted up to {MOST_BYTES})", job.size)) }
        if !job.digest.starts_with("sha256:") || job.digest.len() != 71 || !job.digest[7..].bytes().all(|b| b.is_ascii_hexdigit()) { return Err("not a sha256 digest".into()) }
        let key = job.source["key"].as_str().filter(|k| k.starts_with(superci_core::layers::STAGING)).ok_or("no blob in staging")?;
        let url = aws::presign("GET", &bucket_url(key), creds, &job.region, 3600, super::now_ms())?;
        let response = agent().get(&superci_core::io::stand_in(&url)).call().map_err(|e| format!("fetching the blob: {e}"))?;
        if response.status() != 200 { return Err(format!("fetching the blob: HTTP {}", response.status())) }
        let out = tmp.join("layer.erofs");
        let reader = response.into_body().into_reader().take(job.size + 1);
        let made = make(reader, mkfs, &out, None)?;
        if made.digest != job.digest || made.size != job.size { let _ = std::fs::remove_file(&out); return Err(format!("the blob is {} ({} bytes), not {} ({} bytes)", made.digest, made.size, job.digest, job.size)) }
        let erofs_size = std::fs::metadata(&out).map_err(|e| e.to_string())?.len();
        let erofs_digest = file_sha256(&out)?;
        upload(http, creds, &job.region, &bucket_url(&job.erofs_key), &out, PART)?;
        let _ = std::fs::remove_file(&out);
        // The blob kept beside it (within the bucket: S3 copies it, up to 5 GB, larger than any blob converted).
        let source = format!("/{}/{key}", job.bucket);
        let copied = said(block_on(aws::s3(http, "PUT", &bucket_url(&job.blob_key), &job.region, creds, vec![], &[("x-amz-copy-source", &source)], super::now_ms()))?, "copying the blob")?;
        if copied.body_text().contains("<Error>") { return Err("copying the blob: S3 failed".into()) }
        let _ = block_on(aws::s3(http, "DELETE", &bucket_url(key), &job.region, creds, vec![], &[], super::now_ms()));
        Ok(json!({ "digest": job.digest, "size": job.size, "diff_id": made.diff_id, "erofs": { "key": job.erofs_key, "size": erofs_size, "digest": erofs_digest }, "blob": { "key": job.blob_key, "size": job.size } }))
    })();
    let _ = std::fs::remove_file(tmp.join("layer.erofs"));
    let mut done = result.unwrap_or_else(|e| json!({ "digest": job.digest, "error": e }));
    done["seconds"] = json!((began.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    if let Err(e) = block_on(aws::s3(http, "PUT", &bucket_url(&job.done_key), &job.region, creds, done.to_string().into_bytes(), &[("content-type", "application/json")], super::now_ms())).map_err(|e| e.to_string()).and_then(|r| said(r, "writing what was made").map(|_| ())) {
        done["unsaid"] = json!(e);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `mkfs.erofs`: what it is given, written where it is told (the last argument), and its options
    /// checked; it refuses a tar that says "refuse".
    fn fake_mkfs(dir: &std::path::Path) -> String {
        let p = dir.join("mkfs.erofs");
        std::fs::write(&p, format!("#!/bin/sh\n[ \"$1 $2 $3 $4 $5 $6 $7 $8 $9\" = \"{}\" ] || exit 9\nfor a; do out=$a; done\ncat > \"$out\"\nif grep -q refuse \"$out\"; then echo 'unusual entry' >&2; exit 1; fi\n", MKFS.join(" "))).unwrap();
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap() }
        p.to_string_lossy().into_owned()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("superci-convert-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(b: &[u8]) -> String { format!("sha256:{}", hex(&Sha256::digest(b))) }

    #[test]
    fn a_blob_is_unpacked_as_its_first_bytes_say_and_hashed_before_and_after() {
        let d = tmp("packing");
        let mkfs = fake_mkfs(&d);
        let tar = b"a tar's bytes, as a layer has them".repeat(1000);
        let gz = { let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()); e.write_all(&tar).unwrap(); e.finish().unwrap() };
        // Two gzip members one after the other (as some tools write them), and zstd frames with a skippable one.
        let two = [gz.clone(), { let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()); e.write_all(b"more").unwrap(); e.finish().unwrap() }].concat();
        let zst = [zstd::encode_all(&tar[..5000], 3).unwrap(), vec![0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 1, 2, 3, 4], zstd::encode_all(&tar[5000..], 3).unwrap()].concat();
        for (blob, whole) in [(gz.clone(), tar.clone()), (two.clone(), [tar.clone(), b"more".to_vec()].concat()), (zst.clone(), tar.clone()), (tar.clone(), tar.clone())] {
            let (out, keep) = (d.join("out"), d.join("blob"));
            let made = make(&blob[..], &mkfs, &out, Some(&keep)).unwrap();
            assert_eq!(made, Made { digest: sha(&blob), size: blob.len() as u64, diff_id: sha(&whole) });
            assert_eq!((std::fs::read(&out).unwrap() == whole, std::fs::read(&keep).unwrap() == blob), (true, true));
        }
        assert_eq!((packing(&gz), packing(&zst), packing(&tar)), (Packing::Gzip, Packing::Zstd, Packing::Tar));
    }

    #[test]
    fn a_layer_mkfs_refuses_or_a_broken_blob_leaves_nothing() {
        let d = tmp("refused");
        let mkfs = fake_mkfs(&d);
        let out = d.join("out");
        let e = make(&b"a tar to refuse"[..], &mkfs, &out, None).unwrap_err();
        assert!(e.contains("mkfs.erofs refused the layer") && e.contains("unusual entry") && !out.exists(), "{e}");
        let gz = { let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()); e.write_all(&[7u8; 100_000]).unwrap(); e.finish().unwrap() };
        let e = make(&gz[..gz.len() / 2], &mkfs, &out, None).unwrap_err();
        assert!(e.contains("unpacking") && !out.exists(), "{e}");
        assert!(make(&b"x"[..], "/no/such/mkfs", &out, None).unwrap_err().contains("/no/such/mkfs"));
    }

    /// With the real `mkfs.erofs` when there is one (`SUPERCI_MKFS_EROFS`, as the layers job in CI sets): a tar with a
    /// whiteout and an opaque folder makes a file that `fsck.erofs` takes, the same one every time.
    #[test]
    fn the_real_mkfs_makes_the_same_file_from_the_same_layer() {
        let Ok(mkfs) = std::env::var("SUPERCI_MKFS_EROFS") else { return };
        let d = tmp("real");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("etc/opaque")).unwrap();
        std::fs::write(src.join("etc/hello"), "hello\n").unwrap();
        std::fs::write(src.join("etc/.wh.gone"), "").unwrap();
        std::fs::write(src.join("etc/opaque/.wh..wh..opq"), "").unwrap();
        let tar = Command::new("tar").args(["-C", src.to_str().unwrap(), "--owner=0", "--group=0", "-czf", "-", "etc"]).output().unwrap().stdout;
        let (a, b) = (d.join("a.erofs"), d.join("b.erofs"));
        let first = make(&tar[..], &mkfs, &a, None).unwrap();
        make(&tar[..], &mkfs, &b, None).unwrap();
        assert_eq!(first.digest, sha(&tar));
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), "the same layer makes the same file");
        if let Ok(fsck) = std::env::var("SUPERCI_FSCK_EROFS") { assert!(Command::new(fsck).arg(&a).status().unwrap().success()) }
    }

    /// S3 as a test has it answer: by what is asked (`POST ?uploads`, `PUT ?partNumber=2`, …), each call kept.
    struct S3<F: Fn(&str, &str) -> superci_core::io::Result<superci_core::io::Response> + Sync>(F, std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait(?Send)]
    impl<F: Fn(&str, &str) -> superci_core::io::Result<superci_core::io::Response> + Sync> Http for S3<F> {
        async fn send(&self, r: superci_core::io::Request) -> superci_core::io::Result<superci_core::io::Response> {
            let query = r.url.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
            self.1.lock().unwrap().push(format!("{} {query}", r.method));
            (self.0)(&r.method, &query)
        }
    }

    fn creds() -> Credentials { Credentials { access_key_id: "AKIAEXAMPLE".into(), secret_access_key: "example".into(), session_token: None, expires_at_ms: u64::MAX } }

    fn answer(status: u16, body: &str) -> superci_core::io::Result<superci_core::io::Response> { Ok(superci_core::io::Response::new(status, "application/xml", body)) }

    /// A file in parts (four bytes each here), each part asked again up to three times, the upload finished in order; or,
    /// where S3 does not take it, given up and taken back, saying why.
    #[test]
    fn a_large_file_goes_up_in_parts_asked_again_and_given_up_saying_why() {
        let d = tmp("upload");
        let file = d.join("layer.erofs");
        std::fs::write(&file, b"0123456789").unwrap();
        let url = "https://superci-plane-p-us-east-1.s3.us-east-1.amazonaws.com/layers/a.erofs";
        let up = |http: &(dyn Http + Sync), part: usize| upload(http, &creds(), "us-east-1", url, &file, part);
        let begun = || answer(200, "<InitiateMultipartUploadResult><UploadId>up-1</UploadId></InitiateMultipartUploadResult>");
        let tagged = || Ok(superci_core::io::Response::new(200, "application/xml", "").with_header("etag", "\"e\""));
        // Small: one request; S3 refusing it, or out of reach.
        let s3 = S3(|_, _| answer(200, ""), Default::default());
        up(&s3, 64).unwrap();
        assert_eq!(*s3.1.lock().unwrap(), ["PUT "]);
        assert!(up(&S3(|_, _| answer(403, "<Error>AccessDenied</Error>"), Default::default()), 64).unwrap_err().starts_with("putting the file: S3 answered 403"));
        assert!(up(&S3(|_, _| Err("connection refused".into()), Default::default()), 64).is_err());
        // In parts: the second part refused once, then taken.
        let refused = std::sync::atomic::AtomicBool::new(false);
        let s3 = S3(|m, q| match (m, q) {
            ("POST", "uploads") => begun(),
            ("PUT", q) if q.starts_with("partNumber=2&") && !refused.swap(true, std::sync::atomic::Ordering::SeqCst) => answer(500, "<Error>InternalError</Error>"),
            ("PUT", _) => tagged(),
            _ => answer(200, "<CompleteMultipartUploadResult/>"),
        }, Default::default());
        up(&s3, 4).unwrap();
        let calls = s3.1.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| c.starts_with("PUT partNumber=")).count(), 4, "{calls:?}");
        assert_eq!(calls.last().unwrap(), "POST uploadId=up-1");
        // What S3 may answer amiss: no upload id; a part refused three times (or out of reach), the upload taken back;
        // no ETag; finished with 200 and an error in its body; the beginning refused.
        assert_eq!(up(&S3(|_, _| answer(200, "<nothing/>"), Default::default()), 4).unwrap_err(), "S3 gave no upload id");
        let s3 = S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), ("PUT", _) => answer(503, "<Error>SlowDown</Error>"), _ => answer(204, "") }, Default::default());
        assert!(up(&s3, 4).unwrap_err().ends_with("S3 answered 503"));
        assert!(s3.1.lock().unwrap().iter().any(|c| c == "DELETE uploadId=up-1"), "taken back");
        assert!(up(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), ("PUT", _) => Err("reset".into()), _ => answer(204, "") }, Default::default()), 4).unwrap_err().ends_with("reset"));
        assert_eq!(up(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), _ => answer(200, "") }, Default::default()), 4).unwrap_err().split(": ").last(), Some("S3 gave no ETag"));
        assert!(up(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), ("PUT", _) => tagged(), _ => answer(200, "<Error><Code>InternalError</Code></Error>") }, Default::default()), 4).unwrap_err().starts_with("finishing the upload: <Error>"));
        assert!(up(&S3(|_, _| answer(403, "<Error>AccessDenied</Error>"), Default::default()), 4).unwrap_err().starts_with("beginning the upload"));
        // The file gone before it is read, or not a file (a folder): said.
        let gone = d.join("gone.erofs");
        assert!(upload(&S3(|_, _| answer(200, ""), Default::default()), &creds(), "us-east-1", url, &gone, 4).is_err());
        assert!(upload(&S3(|_, _| answer(200, ""), Default::default()), &creds(), "us-east-1", url, &d, 1 << 20).is_err());
        assert!(upload(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), _ => tagged() }, Default::default()), &creds(), "us-east-1", url, &d, 4).is_err());
        // S3 out of reach as the upload begins, or as it is finished.
        assert!(up(&S3(|_, _| Err("connection refused".into()), Default::default()), 4).unwrap_err().contains("connection refused"));
        assert!(up(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), ("PUT", _) => tagged(), _ => Err("reset".into()) }, Default::default()), 4).unwrap_err().contains("reset"));
        assert!(up(&S3(|m, q| match (m, q) { ("POST", "uploads") => begun(), ("PUT", _) => tagged(), _ => answer(500, "<Error>InternalError</Error>") }, Default::default()), 4).unwrap_err().starts_with("finishing the upload: S3 answered 500"));
        // The file taken away once the upload has begun: its parts cannot be read.
        let moving = d.join("moving.erofs");
        std::fs::write(&moving, b"0123456789").unwrap();
        let s3 = S3(|m, q| match (m, q) { ("POST", "uploads") => { let _ = std::fs::remove_file(&moving); begun() } _ => tagged() }, Default::default());
        assert!(upload(&s3, &creds(), "us-east-1", url, &moving, 4).is_err());
        // One part failing: the parts not begun yet are not asked for.
        let many = d.join("many.erofs");
        std::fs::write(&many, [7u8; 12]).unwrap();
        let s3 = S3(|m, q| match (m, q) {
            ("POST", "uploads") => begun(),
            ("PUT", q) if q.starts_with("partNumber=1&") => Err("reset".into()),
            ("PUT", _) => { std::thread::sleep(Duration::from_millis(100)); tagged() }
            _ => answer(204, ""),
        }, Default::default());
        assert!(upload(&s3, &creds(), "us-east-1", url, &many, 1).is_err());
        assert!(s3.1.lock().unwrap().iter().filter(|c| c.starts_with("PUT partNumber=")).count() < 12 * 3);
    }

    /// The converter with S3 out of reach as the blob is copied beside its file: what was made is not said as made.
    #[test]
    fn a_copy_s3_does_not_answer_is_said() {
        let d = tmp("copy");
        let mkfs = fake_mkfs(&d);
        let blob = b"a tar".repeat(10);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", listener.local_addr().unwrap());
        let served = blob.clone();
        std::thread::spawn(move || for mut c in listener.incoming().flatten() {
            let mut head = [0u8; 4096];
            let _ = c.read(&mut head);
            let _ = c.write_all(&[format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", served.len()).into_bytes(), served.clone()].concat());
        });
        // (The blob fetched from where the stand-in is: only this test fetches one here.)
        std::env::set_var("SUPERCI_STAND_IN", &at);
        // (Its file put; the copy beside it not answered.)
        let puts = std::sync::atomic::AtomicUsize::new(0);
        let s3 = S3(|m, _| match m { "PUT" if puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 => Err("connection reset".into()), _ => answer(200, "") }, Default::default());
        let digest = sha(&blob);
        let job = Job { digest: digest.clone(), size: blob.len() as u64, region: "us-east-1".into(), bucket: "superci-plane-p-us-east-1".into(), source: json!({ "key": format!("{}x", superci_core::layers::STAGING) }),
            erofs_key: "layers/a.erofs".into(), blob_key: "layers/a.blob".into(), done_key: "layers/done/a.json".into() };
        let done = convert(&s3, &creds(), &job, &mkfs, &d);
        std::env::remove_var("SUPERCI_STAND_IN");
        assert!(done["error"].as_str().is_some_and(|e| e.contains("connection reset")), "{done}");
    }

    /// A blob that cannot be read (from its start, or partway), a place to keep it that cannot be written, a file made
    /// that cannot be read back: said, nothing left.
    #[test]
    fn a_blob_or_a_file_that_cannot_be_read_or_kept_is_said() {
        struct Broken(usize);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 { return Err(std::io::Error::other("the connection broke")) }
                let n = self.0.min(buf.len()).min(512);
                buf[..n].fill(b'a');
                self.0 -= n;
                Ok(n)
            }
        }
        let d = tmp("broken");
        let mkfs = fake_mkfs(&d);
        let out = d.join("out");
        assert!(make(Broken(0), &mkfs, &out, None).unwrap_err().starts_with("reading the blob"));
        assert!(make(Broken(2048), &mkfs, &out, None).unwrap_err().starts_with("unpacking the blob"));
        let zst = zstd::encode_all(&[1u8; 1000][..], 3).unwrap();
        assert!(make(&zst[..zst.len() - 3], &mkfs, &out, None).unwrap_err().starts_with("unpacking the blob"));
        // (A gzip member whole, then the rest of the blob broken.)
        let gz = { let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()); e.write_all(b"a tar").unwrap(); e.finish().unwrap() };
        let then_broken = std::io::Read::chain(&gz[..], Broken(0));
        assert!(make(then_broken, &mkfs, &out, None).is_err());
        assert!(make(&b"a tar"[..], &mkfs, &out, Some(&d.join("no/such/folder/blob"))).unwrap_err().contains("no/such/folder"));
        if std::path::Path::new("/dev/full").exists() {
            assert!(make(&[b'a'; 4096][..], &mkfs, &out, Some(std::path::Path::new("/dev/full"))).is_err());
        }
        assert!(file_sha256(&d.join("none")).is_err());
        assert!(file_sha256(&d).is_err(), "a folder is not read as a file");
    }
}
