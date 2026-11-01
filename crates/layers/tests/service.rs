//! superci-layers as containerd reaches it: the real program on a socket, asked through containerd's own snapshot and
//! content APIs (the clients its .proto files make). Its content store (writes, reads, labels, writes under way, what
//! it refuses), its snapshots (made, committed, viewed, listed, counted, removed), then layers from SuperCI's store: an
//! index without links (asked for when a layer is used), a layer's file fetched (and, where it cannot be mounted, the
//! layer pulled as usual), the index read again; and after the job (`keep`), a layer pulled that the store lacks
//! offered and its blob uploaded, with what came from the store said.
//! Mounting needs root: run as root (as on a machine) a served layer is mounted; otherwise it falls back, as it would.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use superci_layers::containerd::services::content::v1 as capi;
use superci_layers::containerd::services::snapshots::v1 as sapi;
use tonic::Code;

fn digest(b: &[u8]) -> String { format!("sha256:{}", Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()) }

struct Service { child: Child, dir: std::path::PathBuf, socket: std::path::PathBuf }
impl Drop for Service {
    // (Stopped as systemd stops it, SIGTERM, so a coverage tool's buffered evidence is written; killed if it stays.)
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM); }
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until && !matches!(self.child.try_wait(), Ok(Some(_))) { std::thread::sleep(Duration::from_millis(20)) }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(name: &str, index: Option<&serde_json::Value>) -> Service {
    let dir = std::env::temp_dir().join(format!("superci-layers-service-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(i) = index { std::fs::write(dir.join("index.json"), i.to_string()).unwrap() }
    let socket = dir.join("layers.sock");
    Service { child: launch(&dir), dir, socket }
}

/// The program on `dir`'s folders, once its socket is there (what it says, to `dir/log`, after what it said before).
fn launch(dir: &std::path::Path) -> Child { launch_with(dir, &[]) }

/// The same with this environment besides.
fn launch_with(dir: &std::path::Path, env: &[(&str, &str)]) -> Child {
    let socket = dir.join("layers.sock");
    let _ = std::fs::remove_file(&socket);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_superci-layers"))
        .args([format!("--root={}", dir.join("root").display()), format!("--content={}", dir.join("content").display()), format!("--socket={}", socket.display()),
            format!("--index={}", dir.join("index.json").display()), format!("--containerd={}", dir.join("no-containerd.sock").display()), "--parallel=4".into(), "--sync-remove".into()])
        .envs(env.iter().copied()).stderr(log).spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while !socket.exists() { assert!(Instant::now() < until, "it did not answer: {}", std::fs::read_to_string(dir.join("log")).unwrap_or_default()); std::thread::sleep(Duration::from_millis(20)) }
    child
}

impl Service {
    fn log(&self) -> String { std::fs::read_to_string(self.dir.join("log")).unwrap_or_default() }
    async fn content(&self) -> capi::content_client::ContentClient<tonic::transport::Channel> { capi::content_client::ContentClient::new(connect(&self.socket).await) }
    async fn snapshots(&self) -> sapi::snapshots_client::SnapshotsClient<tonic::transport::Channel> { sapi::snapshots_client::SnapshotsClient::new(connect(&self.socket).await) }
}

/// A channel to the service's socket (asked again for a moment: a socket just made may not take one yet).
async fn connect(socket: &std::path::Path) -> tonic::transport::Channel {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match superci_layers::client::channel(socket).await {
            Ok(c) => return c,
            Err(e) => { assert!(Instant::now() < until, "{e}"); tokio::time::sleep(Duration::from_millis(50)).await }
        }
    }
}

/// A write session of these messages: the answers, or how it was refused.
async fn write(c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, msgs: Vec<capi::WriteContentRequest>) -> Result<Vec<capi::WriteContentResponse>, tonic::Status> {
    let mut answers = c.write(tokio_stream::iter(msgs)).await?.into_inner();
    let mut out = vec![];
    while let Some(m) = answers.message().await? { out.push(m) }
    Ok(out)
}

fn msg(action: capi::WriteAction, reference: &str, total: i64, expected: &str, offset: i64, data: &[u8]) -> capi::WriteContentRequest {
    capi::WriteContentRequest { action: action as i32, r#ref: reference.into(), total, expected: expected.into(), offset, data: data.to_vec(), labels: HashMap::new() }
}

async fn read_all(c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, d: &str, offset: i64, size: i64) -> Result<Vec<u8>, tonic::Status> {
    let mut s = c.read(capi::ReadContentRequest { digest: d.into(), offset, size }).await?.into_inner();
    let mut out = vec![];
    while let Some(m) = s.message().await? { out.extend(m.data) }
    Ok(out)
}

#[tokio::test(flavor = "multi_thread")]
async fn its_content_store_is_containerds() {
    let s = start("content", None);
    let mut c = s.content().await;
    use capi::WriteAction::*;
    let blob = b"hello, the blob's bytes".repeat(1000);
    let d = digest(&blob);
    // Asked how it stands, written in two parts, committed with labels.
    let mut first = msg(Commit, "ref-1", blob.len() as i64, &d, blob.len() as i64 / 2, &blob[blob.len() / 2..]);
    first.labels.insert("containerd.io/gc.root".into(), "now".into());
    let answers = write(&mut c, vec![msg(Stat, "ref-1", blob.len() as i64, "", 0, b""), msg(Write, "ref-1", 0, "", 0, &blob[..blob.len() / 2]), first]).await.unwrap();
    assert_eq!((answers.first().unwrap().offset, answers.last().unwrap().digest.as_str()), (0, d.as_str()));
    let info = c.info(capi::InfoRequest { digest: d.clone() }).await.unwrap().into_inner().info.unwrap();
    assert_eq!((info.size, info.labels.get("containerd.io/gc.root").map(String::as_str)), (blob.len() as i64, Some("now")));
    assert_eq!(read_all(&mut c, &d, 0, 0).await.unwrap(), blob);
    assert_eq!(read_all(&mut c, &d, 5, 4).await.unwrap(), b", th");
    assert_eq!(read_all(&mut c, &d, blob.len() as i64 + 1, 0).await.unwrap_err().code(), Code::OutOfRange);
    // Its labels changed (the mask's), listed by them.
    let mut changed = info.clone();
    changed.labels = [("superci".to_string(), "yes".to_string())].into_iter().collect();
    let out = c.update(capi::UpdateRequest { info: Some(changed), update_mask: Some(prost_types::FieldMask { paths: vec!["labels.superci".into()] }) }).await.unwrap().into_inner().info.unwrap();
    assert_eq!((out.labels.get("superci").map(String::as_str), out.labels.get("containerd.io/gc.root").map(String::as_str)), (Some("yes"), Some("now")));
    let mut listed = c.list(capi::ListContentRequest { filters: vec!["labels.superci==yes".into()] }).await.unwrap().into_inner();
    let mut found = vec![];
    while let Some(m) = listed.message().await.unwrap() { found.extend(m.info.into_iter().map(|i| i.digest)) }
    assert_eq!(found, [d.clone()]);
    // Written again: it is there already.
    assert_eq!(write(&mut c, vec![msg(Commit, "ref-2", blob.len() as i64, &d, 0, &blob)]).await.unwrap_err().code(), Code::AlreadyExists);
    // A write under way: its status, listed, at the wrong offset refused, given up.
    let other = b"another blob".to_vec();
    write(&mut c, vec![msg(Write, "ref-3", other.len() as i64, &digest(&other), 0, &other[..5])]).await.unwrap();
    let st = c.status(capi::StatusRequest { r#ref: "ref-3".into() }).await.unwrap().into_inner().status.unwrap();
    assert_eq!((st.offset, st.total), (5, other.len() as i64));
    assert_eq!(c.list_statuses(capi::ListStatusesRequest { filters: vec!["ref==ref-3".into()] }).await.unwrap().into_inner().statuses.len(), 1);
    assert_eq!(write(&mut c, vec![msg(Write, "ref-3", 0, "", 9, b"x")]).await.unwrap_err().code(), Code::OutOfRange);
    c.abort(capi::AbortRequest { r#ref: "ref-3".into() }).await.unwrap();
    assert_eq!(c.status(capi::StatusRequest { r#ref: "ref-3".into() }).await.unwrap_err().code(), Code::NotFound);
    // (No write under way now: none listed.)
    assert!(c.list_statuses(capi::ListStatusesRequest { filters: vec![] }).await.unwrap().into_inner().statuses.is_empty());
    // A commit that is not what it said: refused, nothing kept.
    assert_eq!(write(&mut c, vec![msg(Commit, "ref-4", 3, &digest(b"abd"), 0, b"abc")]).await.unwrap_err().code(), Code::FailedPrecondition);
    // What it refuses to begin with.
    assert_eq!(write(&mut c, vec![msg(Stat, "", 0, "", 0, b"")]).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(c.info(capi::InfoRequest { digest: "sha256:nope".into() }).await.unwrap_err().code(), Code::InvalidArgument);
    // Deleted: gone.
    c.delete(capi::DeleteContentRequest { digest: d.clone() }).await.unwrap();
    assert_eq!(c.info(capi::InfoRequest { digest: d.clone() }).await.unwrap_err().code(), Code::NotFound);
    // (Deleting what is gone is no error: containerd's local store removes it with RemoveAll, which says nothing.)
    c.delete(capi::DeleteContentRequest { digest: d }).await.unwrap();
}

/// Digests of each algorithm containerd takes (and those go-digest refuses, as it refuses them); a write hashed again
/// as its commit asks; labels changed whole, by path, or not at all; filters by what is not there; a write left and
/// taken up again (from its own record, or anew when that record is not its own).
#[tokio::test(flavor = "multi_thread")]
async fn its_content_store_takes_every_digest_and_write_as_containerds_does() {
    use capi::WriteAction::*;
    let s = start("digests", None);
    let mut c = s.content().await;
    let info = |c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, d: String| { let mut c = c.clone(); async move { c.info(capi::InfoRequest { digest: d }).await } };
    // What is not a digest (no algorithm, none after it, an algorithm not taken, written amiss, too short, not hex).
    for d in ["sha256", "sha256:", &format!("md5:{}", "a".repeat(32)), &format!("MD5:{}", "a".repeat(32)), "md5:short", &format!("md5:{}", "*".repeat(32)),
        &format!("md5:{}==", "a".repeat(32)), &format!("md5..x:{}", "a".repeat(32)), &format!("md5:{}_-", "a".repeat(32)), &format!("sha256:{}", "a".repeat(63)), &format!("sha256:{}", "A".repeat(64))] {
        assert_eq!(info(&mut c, d.to_string()).await.unwrap_err().code(), Code::InvalidArgument, "{d}");
    }
    // sha512 and sha384: one written knowing its digest, one hashed again at its commit; both read and listed.
    let (big, mid) = (b"a blob kept by its sha512".to_vec(), b"a blob kept by its sha384".to_vec());
    let of = |alg: &str, b: &[u8]| -> String { match alg { "sha512" => format!("sha512:{}", sha2::Sha512::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()), _ => format!("sha384:{}", sha2::Sha384::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>()) } };
    let (d512, d384) = (of("sha512", &big), of("sha384", &mid));
    write(&mut c, vec![msg(Commit, "r512", big.len() as i64, &d512, 0, &big)]).await.unwrap();
    write(&mut c, vec![msg(Write, "r384", 0, "", 0, &mid), msg(Commit, "r384", mid.len() as i64, &d384, mid.len() as i64, b"")]).await.unwrap();
    assert_eq!(read_all(&mut c, &d512, 0, 0).await.unwrap(), big);
    assert_eq!(info(&mut c, d384.clone()).await.unwrap().into_inner().info.unwrap().size, mid.len() as i64);
    let listed = |c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, f: &str| { let (mut c, f) = (c.clone(), f.to_string()); async move {
        let mut l = c.list(capi::ListContentRequest { filters: vec![f] }).await.unwrap().into_inner();
        let mut out = vec![];
        while let Some(m) = l.message().await.unwrap() { out.extend(m.info.into_iter().map(|i| i.digest)) }
        out
    } };
    assert_eq!(listed(&mut c, &format!("digest=={d384}")).await, [d384.clone()]);
    // Filters by what a blob does not have: labels when it has none, a field no blob has.
    assert!(listed(&mut c, "labels.none==x").await.is_empty());
    assert!(listed(&mut c, "size==1").await.is_empty());
    // Labels set whole (no paths), by `labels`, one taken away by a path with no value, a path that is no field,
    // and a blob that is not there.
    let update = |c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, d: &str, labels: &[(&str, &str)], paths: &[&str]| {
        let mut c = c.clone();
        let info = capi::Info { digest: d.to_string(), labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() };
        let mask = prost_types::FieldMask { paths: paths.iter().map(|p| p.to_string()).collect() };
        async move { c.update(capi::UpdateRequest { info: Some(info), update_mask: Some(mask) }).await }
    };
    let set = update(&mut c, &d512, &[("a", "1"), ("b", "2"), ("gone", "")], &[]).await.unwrap().into_inner().info.unwrap();
    assert_eq!(set.labels.len(), 2);
    assert_eq!(update(&mut c, &d512, &[("c", "3")], &["labels"]).await.unwrap().into_inner().info.unwrap().labels.len(), 1);
    assert!(update(&mut c, &d512, &[], &["labels.c"]).await.unwrap().into_inner().info.unwrap().labels.is_empty());
    assert_eq!(update(&mut c, &d512, &[], &["size"]).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(update(&mut c, &digest(b"none"), &[], &[]).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(update(&mut c, "nope", &[], &[]).await.unwrap_err().code(), Code::InvalidArgument);
    // Statuses by a filter that matches none; one asked of no ref.
    write(&mut c, vec![msg(Write, "left", 10, "", 0, b"hello")]).await.unwrap();
    assert!(c.list_statuses(capi::ListStatusesRequest { filters: vec!["ref==other".into()] }).await.unwrap().into_inner().statuses.is_empty());
    assert_eq!(c.status(capi::StatusRequest { r#ref: String::new() }).await.unwrap_err().code(), Code::NotFound);
    // Taken up again where it was left (a new session, the same ref); with another total, or a record not its own:
    // begun anew.
    let w = write(&mut c, vec![msg(Write, "left", 10, "", 5, b"world")]).await.unwrap();
    assert_eq!(w.last().unwrap().offset, 10);
    write(&mut c, vec![msg(Write, "other-total", 4, "", 0, b"ab")]).await.unwrap();
    assert_eq!(write(&mut c, vec![msg(Write, "other-total", 9, "", 0, b"")]).await.unwrap()[0].offset, 0, "begun anew");
    write(&mut c, vec![msg(Write, "tampered", 4, "", 0, b"ab")]).await.unwrap();
    for e in std::fs::read_dir(s.dir.join("content/ingest")).unwrap().flatten() {
        if std::fs::read_to_string(e.path().join("ref")).unwrap_or_default() == "tampered" { std::fs::write(e.path().join("ref"), "someone else's").unwrap() }
    }
    assert_eq!(write(&mut c, vec![msg(Write, "tampered", 4, "", 0, b"")]).await.unwrap()[0].offset, 0, "begun anew");
    // A digest that is no digest: refused as the write begins; at its commit, the commit refused.
    assert_eq!(write(&mut c, vec![msg(Commit, "odd", 3, "nonsense", 0, b"abc")]).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(write(&mut c, vec![msg(Write, "odd", 3, "", 0, b"abc"), msg(Commit, "odd", 3, "nonsense", 3, b"")]).await.unwrap_err().code(), Code::FailedPrecondition);
    // What in its folders is not a blob's: passed over when listing.
    std::fs::create_dir_all(s.dir.join("content/blobs/md5")).unwrap();
    let only_file = start("ff", None);
    let mut oc = only_file.content().await;
    std::fs::create_dir_all(only_file.dir.join("content/blobs")).unwrap();
    std::fs::write(only_file.dir.join("content/blobs/sha256"), b"").unwrap();
    assert!(listed(&mut oc, "digest==x").await.is_empty());
    // A folder of blobs with nothing in it at all: nothing listed.
    std::fs::remove_file(only_file.dir.join("content/blobs/sha256")).unwrap();
    assert!(listed(&mut oc, "").await.is_empty());
    assert_eq!(listed(&mut c, &format!("digest=={d512}")).await, [d512]);
}

/// A write under way's record, as containerd writes it and as it may be found: its times in each way RFC 3339 allows
/// (and ways it does not), its data or its ref gone, a total it did not say.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_under_ways_record_is_read_as_containerd_writes_it() {
    use capi::WriteAction::*;
    let s = start("record", None);
    let mut c = s.content().await;
    // (Begun with no total; taken up again with one.)
    write(&mut c, vec![msg(Write, "rec", 0, "", 0, b"part")]).await.unwrap();
    assert_eq!(write(&mut c, vec![msg(Write, "rec", 8, "", 4, b"more")]).await.unwrap().last().unwrap().offset, 8);
    let folder = std::fs::read_dir(s.dir.join("content/ingest")).unwrap().flatten().map(|e| e.path()).find(|p| std::fs::read_to_string(p.join("ref")).unwrap_or_default() == "rec").unwrap();
    let status = |c: &mut capi::content_client::ContentClient<tonic::transport::Channel>| { let mut c = c.clone(); async move { c.status(capi::StatusRequest { r#ref: "rec".into() }).await } };
    for (t, good) in [("2026-10-25T10:00:00Z", true), ("2026-10-25t10:00:00z\n", true), ("2026-10-25T10:00:00.5Z", true), ("2026-10-25T10:00:00.1234567891Z", true),
        ("2026-10-25T10:00:00+02:00", true), ("2026-10-25T10:00:00-02:30", true), ("2026-10-25T10:00:00.Z", false), ("2026-10-25T10:00:00.5", false),
        ("2026-10-25T10:00:00+0200", false), ("2026-10-25T10:00:00+02-00", false), ("2026-10-25T10:00:00+0x:00", false), ("2026-10-25T10:00:00+02:0x", false),
        ("2026-10-25T10:00:00Q", false), ("2026-10-25T10:00:00ZZ", false), ("2026-10-25", false), ("2026x10-25T10:00:00Z", false), ("2026-10x25T10:00:00Z", false),
        ("2026-10-25X10:00:00Z", false), ("2026-10-25T10x00:00Z", false), ("2026-10-25T10:00x00Z", false), ("abcd-10-25T10:00:00Z", false), ("2026-ab-25T10:00:00Z", false),
        ("2026-10-abT10:00:00Z", false), ("2026-10-25Tab:00:00Z", false), ("2026-10-25T10:ab:00Z", false), ("2026-10-25T10:00:abZ", false), ("2026-13-25T10:00:00Z", false),
        ("2026-00-25T10:00:00Z", false), ("2026-10-32T10:00:00Z", false), ("2026-10-00T10:00:00Z", false), ("2026-10-25T24:00:00Z", false), ("2026-10-25T10:60:00Z", false),
        ("2026-10-25T10:00:61Z", false)] {
        std::fs::write(folder.join("startedat"), t.replace("\\n", "\n")).unwrap();
        assert_eq!(status(&mut c).await.is_ok(), good, "{t}");
    }
    std::fs::write(folder.join("startedat"), "2026-10-25T10:00:00Z").unwrap();
    // Its total not a number: none; its last change not said: refused; its ref gone, then its data: not there.
    std::fs::write(folder.join("total"), "many").unwrap();
    assert_eq!(status(&mut c).await.unwrap().into_inner().status.unwrap().total, 0);
    std::fs::remove_file(folder.join("updatedat")).unwrap();
    assert!(status(&mut c).await.is_err());
    std::fs::remove_file(folder.join("ref")).unwrap();
    assert_eq!(status(&mut c).await.unwrap_err().code(), Code::NotFound);
    std::fs::remove_file(folder.join("data")).unwrap();
    assert_eq!(status(&mut c).await.unwrap_err().code(), Code::NotFound);
    // With no ingest folder at all, nothing under way.
    std::fs::remove_dir_all(s.dir.join("content/ingest")).unwrap();
    assert!(c.list_statuses(capi::ListStatusesRequest { filters: vec![] }).await.unwrap().into_inner().statuses.is_empty());
}

/// Lists chosen by containerd's filters: values quoted with each escape Go takes, regular expressions between `/`
/// and `|`, a quoted field; and each way a filter is written amiss, refused as containerd refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn lists_are_chosen_by_filters_as_containerd_parses_them() {
    let s = start("filters", None);
    let mut c = s.content().await;
    let blob = b"a blob with odd labels".to_vec();
    let mut m = msg(capi::WriteAction::Commit, "f", blob.len() as i64, &digest(&blob), 0, &blob);
    m.labels = [("esc", "\u{7}\u{8}\u{c}\n\r\t\u{b}\\\""), ("hex", "AJ"), ("uni", "é😀"), ("oct", "A"), ("a.b", "dotted"), ("re", "a/b|c"), ("sur", "\u{fffd}")]
        .into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    write(&mut c, vec![m]).await.unwrap();
    let list = |c: &mut capi::content_client::ContentClient<tonic::transport::Channel>, f: &str| { let (mut c, f) = (c.clone(), f.to_string()); async move {
        let mut l = c.list(capi::ListContentRequest { filters: vec![f] }).await?.into_inner();
        let mut n = 0;
        while let Some(m) = l.message().await? { n += m.info.len() }
        Ok::<_, tonic::Status>(n)
    } };
    for (f, n) in [(r#"labels.esc=="\a\b\f\n\r\t\v\\\"""#, 1), (r#"labels.hex=="\x41\x4A""#, 1), (r#"labels.uni=="é\U0001F600""#, 1), (r#"labels.oct=="\101""#, 1),
        (r#"labels."a.b"==dotted"#, 1), (r#"labels.re~=/a\/b/"#, 1), ("labels.re~=|b.c|", 1), ("labels.re~=/x/", 0), (r#"labels.sur=="\uD800""#, 1), ("labels.a_b", 0),
        ("labels.esc!=x", 1), ("labels.re~=(", 0)] {
        assert_eq!(list(&mut c, f).await.unwrap(), n, "{f}");
    }
    for f in [r#"labels.oct=="\777""#, r#"labels.uni=="\U00110000""#, "labels.esc /x/", "/x/==a", "a b", "=x", "a==", "a=b", "a<b", "a\0", "\0", "a==\0", "\"unterminated",
        r#""bad \q""#, r#""\x4""#, r#""\xZ1""#, "a==b,,c", "a\u{fffd}", "a==b\u{1}c", "a.\"b\nc\"", "a.==b"] {
        assert_eq!(list(&mut c, f).await.unwrap_err().code(), Code::InvalidArgument, "{f:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn its_snapshots_are_containerds_overlay() {
    let s = start("snapshots", None);
    let mut c = s.snapshots().await;
    let labels = |k: &str, v: &str| -> HashMap<String, String> { [(k.to_string(), v.to_string())].into_iter().collect() };
    // A first layer: a folder bound as it is; committed; a second on it, mounted over it; a view of the first.
    let m = c.prepare(sapi::PrepareSnapshotRequest { snapshotter: "superci".into(), key: "ns/1/a".into(), parent: String::new(), labels: labels("l", "1") }).await.unwrap().into_inner().mounts;
    assert_eq!(m[0].r#type, "bind");
    std::fs::write(std::path::Path::new(&m[0].source).join("file"), b"in the first layer").unwrap();
    assert_eq!(c.prepare(sapi::PrepareSnapshotRequest { key: "ns/1/a".into(), ..Default::default() }).await.unwrap_err().code(), Code::AlreadyExists);
    assert_eq!(c.mounts(sapi::MountsRequest { key: "ns/1/a".into(), ..Default::default() }).await.unwrap().into_inner().mounts[0].source, m[0].source);
    let usage = c.usage(sapi::UsageRequest { key: "ns/1/a".into(), ..Default::default() }).await.unwrap().into_inner();
    assert!(usage.size > 0 && usage.inodes >= 2, "{usage:?}");
    c.commit(sapi::CommitSnapshotRequest { name: "ns/1/base".into(), key: "ns/1/a".into(), ..Default::default() }).await.unwrap();
    assert_eq!(c.commit(sapi::CommitSnapshotRequest { name: "ns/1/x".into(), key: "ns/1/missing".into(), ..Default::default() }).await.unwrap_err().code(), Code::NotFound);
    let top = c.prepare(sapi::PrepareSnapshotRequest { key: "ns/1/b".into(), parent: "ns/1/base".into(), ..Default::default() }).await.unwrap().into_inner().mounts;
    assert!(top[0].r#type == "overlay" && top[0].options.iter().any(|o| o.starts_with("lowerdir=")), "{top:?}");
    let view = c.view(sapi::ViewSnapshotRequest { key: "ns/1/v".into(), parent: "ns/1/base".into(), ..Default::default() }).await.unwrap().into_inner().mounts;
    assert!(view[0].options.contains(&"ro".to_string()), "{view:?}");
    // Its info, changed, listed by what it says.
    let info = c.stat(sapi::StatSnapshotRequest { key: "ns/1/base".into(), ..Default::default() }).await.unwrap().into_inner().info.unwrap();
    assert_eq!((info.name.as_str(), info.kind), ("ns/1/base", sapi::Kind::Committed as i32));
    let mut changed = info.clone();
    changed.labels = labels("superci", "kept");
    let out = c.update(sapi::UpdateSnapshotRequest { info: Some(changed), update_mask: Some(prost_types::FieldMask { paths: vec!["labels.superci".into()] }), ..Default::default() }).await.unwrap().into_inner().info.unwrap();
    // (A committed snapshot has the labels its commit gave, not those of the active one it was: as containerd's.)
    assert_eq!(out.labels, labels("superci", "kept"));
    let mut listed = c.list(sapi::ListSnapshotsRequest { filters: vec!["parent==ns/1/base".into()], ..Default::default() }).await.unwrap().into_inner();
    let mut names = vec![];
    while let Some(m) = listed.message().await.unwrap() { names.extend(m.info.into_iter().map(|i| i.name)) }
    names.sort();
    assert_eq!(names, ["ns/1/b", "ns/1/v"]);
    // One with children is kept; removed once they are; then what is left behind is cleaned up.
    assert_eq!(c.remove(sapi::RemoveSnapshotRequest { key: "ns/1/base".into(), ..Default::default() }).await.unwrap_err().code(), Code::FailedPrecondition);
    for k in ["ns/1/b", "ns/1/v", "ns/1/base"] { c.remove(sapi::RemoveSnapshotRequest { key: k.into(), ..Default::default() }).await.unwrap(); }
    assert_eq!(c.stat(sapi::StatSnapshotRequest { key: "ns/1/base".into(), ..Default::default() }).await.unwrap_err().code(), Code::NotFound);
    c.cleanup(sapi::CleanupRequest::default()).await.unwrap();
    // A snapshot for a container in a user namespace: its files owned by the namespace's root (as root; otherwise that
    // ownership is refused); mappings written amiss, or without the namespace's root, refused.
    let mapped = |key: &str, uid: &str, gid: Option<&str>| {
        let mut labels: HashMap<String, String> = [("containerd.io/snapshot/uidmapping".to_string(), uid.to_string())].into_iter().collect();
        if let Some(g) = gid { labels.insert("containerd.io/snapshot/gidmapping".into(), g.into()); }
        sapi::PrepareSnapshotRequest { key: key.into(), labels, ..Default::default() }
    };
    let root = unsafe { libc::geteuid() } == 0;
    // (One mapped by its group only: its owner's group.)
    let mut by_group = mapped("ns/2/group", "", Some("0:100000:65536"));
    by_group.labels.remove("containerd.io/snapshot/uidmapping");
    let grouped = c.prepare(by_group).await;
    assert!(grouped.is_ok() || !root, "{grouped:?}");
    match c.prepare(mapped("ns/2/mapped", "0:100000:65536", Some("+0: 100000: 65536"))).await {
        Ok(m) => { use std::os::unix::fs::MetadataExt; assert!(root); assert_eq!(std::fs::metadata(&m.into_inner().mounts[0].source).unwrap().uid(), 100000) }
        Err(e) => assert!(!root && e.message().contains("failed to chown"), "{e:?}"),
    }
    for (i, (uid, gid)) in [("a:b", None), ("0", None), ("0:x:1", None), ("0:1", None), ("0:1:", None), ("0:-1:5", None), ("0:4294967295:1", None), ("1:5:10", None), ("0:5:0", None), ("0:5:1", Some("1:5:10")), ("0:5:1", Some("bad"))].into_iter().enumerate() {
        assert!(c.prepare(mapped(&format!("ns/2/bad{i}"), uid, gid)).await.is_err(), "{uid} {gid:?}");
    }
}

/// What containerd may ask amiss, or leave midway: commits with a parent given (taken, changed, missing, not
/// committed), one onto a name there already, a view committed; lists of more than a page, read and left after the
/// first; a blob left midway, one read from its end; a write with no message, or an action there is none of; every
/// call said in the service's log when asked to (`SUPERCI_LAYERS_DEBUG`).
#[tokio::test(flavor = "multi_thread")]
async fn what_containerd_asks_amiss_or_leaves_midway_is_answered_as_containerd_does() {
    use capi::WriteAction::*;
    let dir = std::env::temp_dir().join(format!("superci-layers-service-{}-amiss", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = Service { child: launch_with(&dir, &[("SUPERCI_LAYERS_DEBUG", "1")]), socket: dir.join("layers.sock"), dir };
    let mut c = s.snapshots().await;
    let prepare = |c: &mut sapi::snapshots_client::SnapshotsClient<tonic::transport::Channel>, key: &str, parent: &str| { let (mut c, key, parent) = (c.clone(), key.to_string(), parent.to_string()); async move { c.prepare(sapi::PrepareSnapshotRequest { key, parent, ..Default::default() }).await } };
    let commit = |c: &mut sapi::snapshots_client::SnapshotsClient<tonic::transport::Channel>, name: &str, key: &str, parent: &str| { let (mut c, name, key, parent) = (c.clone(), name.to_string(), key.to_string(), parent.to_string()); async move { c.commit(sapi::CommitSnapshotRequest { name, key, parent, ..Default::default() }).await } };
    prepare(&mut c, "base-k", "").await.unwrap();
    commit(&mut c, "base", "base-k", "").await.unwrap();
    prepare(&mut c, "other-k", "").await.unwrap();
    commit(&mut c, "other", "other-k", "").await.unwrap();
    // A parent given at the commit: taken by one made with none; refused for one made on another; missing; active.
    prepare(&mut c, "orphan", "").await.unwrap();
    commit(&mut c, "adopted", "orphan", "base").await.unwrap();
    assert_eq!(c.stat(sapi::StatSnapshotRequest { key: "adopted".into(), ..Default::default() }).await.unwrap().into_inner().info.unwrap().parent, "base");
    prepare(&mut c, "on-base", "base").await.unwrap();
    assert_eq!(commit(&mut c, "x", "on-base", "other").await.unwrap_err().code(), Code::InvalidArgument);
    prepare(&mut c, "lost", "").await.unwrap();
    assert_eq!(commit(&mut c, "x", "lost", "nowhere").await.unwrap_err().code(), Code::NotFound);
    prepare(&mut c, "active-parent", "").await.unwrap();
    assert_eq!(commit(&mut c, "x", "lost", "active-parent").await.unwrap_err().code(), Code::FailedPrecondition);
    // Onto a name there already; a view committed; a parent that is not committed for a new one.
    assert_eq!(commit(&mut c, "base", "lost", "").await.unwrap_err().code(), Code::AlreadyExists);
    c.view(sapi::ViewSnapshotRequest { key: "v".into(), parent: "base".into(), ..Default::default() }).await.unwrap();
    assert_eq!(commit(&mut c, "x", "v", "").await.unwrap_err().code(), Code::FailedPrecondition);
    assert_eq!(prepare(&mut c, "y", "lost").await.unwrap_err().code(), Code::InvalidArgument);
    // Labels changed whole; listed by labels when there are none, and by a field there is none of.
    let mut info = c.stat(sapi::StatSnapshotRequest { key: "base".into(), ..Default::default() }).await.unwrap().into_inner().info.unwrap();
    info.labels = [("a".to_string(), "1".to_string())].into_iter().collect();
    c.update(sapi::UpdateSnapshotRequest { info: Some(info.clone()), update_mask: None, ..Default::default() }).await.unwrap();
    info.name = "nowhere".into();
    assert_eq!(c.update(sapi::UpdateSnapshotRequest { info: Some(info), update_mask: None, ..Default::default() }).await.unwrap_err().code(), Code::NotFound);
    for f in ["labels.none==x", "size==1"] {
        let mut l = c.list(sapi::ListSnapshotsRequest { filters: vec![f.into()], ..Default::default() }).await.unwrap().into_inner();
        while l.message().await.unwrap().is_some() {}
    }
    // More than a page of each (a page is a hundred): every one listed; a list left after its first page.
    for i in 0..700 { prepare(&mut c, &format!("many-{i}"), "").await.unwrap(); }
    let mut l = c.list(sapi::ListSnapshotsRequest::default()).await.unwrap().into_inner();
    let mut n = 0;
    while let Some(m) = l.message().await.unwrap() { n += m.info.len() }
    assert!(n >= 700, "{n}");
    let mut l = c.list(sapi::ListSnapshotsRequest::default()).await.unwrap().into_inner();
    let _ = l.message().await;
    drop(l);
    let mut content = s.content().await;
    for i in 0..700 { write(&mut content, vec![msg(Commit, &format!("w{i}"), 0, "", 0, format!("blob {i}").as_bytes())]).await.unwrap(); }
    let mut l = content.list(capi::ListContentRequest::default()).await.unwrap().into_inner();
    let mut n = 0;
    while let Some(m) = l.message().await.unwrap() { n += m.info.len() }
    assert!(n >= 700, "{n}");
    let mut l = content.list(capi::ListContentRequest::default()).await.unwrap().into_inner();
    let _ = l.message().await;
    drop(l);
    // A blob read from its very end (nothing); one left after its first part.
    let big = vec![5u8; 9 << 20];
    let d = digest(&big);
    write(&mut content, vec![msg(Commit, "big", big.len() as i64, &d, 0, &big)]).await.unwrap();
    assert_eq!(content.info(capi::InfoRequest { digest: d.clone() }).await.unwrap().into_inner().info.unwrap().size, big.len() as i64);
    assert!(read_all(&mut content, &d, big.len() as i64, 0).await.unwrap().is_empty());
    let mut r = content.read(capi::ReadContentRequest { digest: d.clone(), offset: 0, size: 0 }).await.unwrap().into_inner();
    let _ = r.message().await;
    drop(r);
    // A write with no message at all; one of an action there is none of; one left after its first answer.
    assert_eq!(content.write(tokio_stream::iter(Vec::<capi::WriteContentRequest>::new())).await.err().map(|e| e.code()).unwrap_or(Code::Cancelled), Code::Cancelled);
    let mut odd = msg(Stat, "odd", 0, "", 0, b"");
    odd.action = 99;
    write(&mut content, vec![odd]).await.unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(msg(Write, "left", 10, "", 0, b"part")).await.unwrap();
    let mut a = content.write(tokio_stream::wrappers::ReceiverStream::new(rx)).await.unwrap().into_inner();
    let _ = a.message().await;
    drop(a);
    // (Its answers no longer read, it goes on writing: the service finds the caller gone.)
    let _ = tx.send(msg(Write, "left", 10, "", 4, b"more")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(tx);
    // A write that says, after it began, it is of a blob there already: given up, as containerd's.
    let there = b"blob 1".to_vec();
    assert_eq!(write(&mut content, vec![msg(Write, "late", 0, "", 0, b"x"), msg(Commit, "late", there.len() as i64, &digest(&there), 1, b"")]).await.unwrap_err().code(), Code::AlreadyExists);
    // Asked of no digest at all; asked of one not there (said in the log).
    assert_eq!(read_all(&mut content, "nope", 0, 0).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(content.delete(capi::DeleteContentRequest { digest: "nope".into() }).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(content.info(capi::InfoRequest { digest: digest(b"not there") }).await.unwrap_err().code(), Code::NotFound);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.log().contains("superci-layers: info") || s.log().contains("superci-layers: write"), "{}", s.log().lines().take(5).collect::<Vec<_>>().join("\n"));
}

/// SuperCI's store and control plane for the test, on one port: files by ranges (`/files/…`), links to a layer's
/// files (`POST /links`), offers (every layer asked to be uploaded, `POST /layers-propose`), uploads (`PUT /up/…`,
/// taken only with the SHA-256 they say), their word (`POST /uploaded`) and what was kept (`POST /layers-kept`).
/// What it was asked, kept.
#[derive(Default)]
struct Plane { files: HashMap<String, Vec<u8>>, asked: Vec<String>, uploaded: HashMap<String, Vec<u8>>, kept: Option<serde_json::Value>, first: Option<serde_json::Value>,
    /// How links are given when asked for again: "" (as they should be), "refused", "nonsense", "elsewhere" (for
    /// other layers only), "expired" (links that are refused too).
    links: String }

fn plane(files: HashMap<String, Vec<u8>>) -> (Arc<Mutex<Plane>>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://{}", listener.local_addr().unwrap());
    let p = Arc::new(Mutex::new(Plane { files, ..Default::default() }));
    let (pp, base) = (p.clone(), at.clone());
    std::thread::spawn(move || for stream in listener.incoming().flatten() {
        let (p, base) = (pp.clone(), base.clone());
        std::thread::spawn(move || {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() { return }
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
            let mut headers = HashMap::new();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h.trim().is_empty() { break }
                if let Some((k, v)) = h.split_once(':') { headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string()); }
            }
            let mut body = vec![0; headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0)];
            let _ = r.read_exact(&mut body);
            p.lock().unwrap().asked.push(format!("{method} {path}"));
            let mut out = stream;
            let send = |out: &mut std::net::TcpStream, status: u16, extra: &str, body: &[u8]| { let _ = out.write_all(format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n", body.len()).as_bytes()); let _ = out.write_all(body); };
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            match (method.as_str(), path.as_str()) {
                // (A link refused as one run out; a connection dropped; a range not taken; a range cut short.)
                ("GET", f) if f.starts_with("/files/expired/") => send(&mut out, 403, "", b"<Error><Code>AccessDenied</Code></Error>"),
                ("GET", f) if f.starts_with("/files/drop/") => {}
                ("GET", f) if f.starts_with("/files/whole/") => { let b = p.lock().unwrap().files.get(&f[13..]).cloned().unwrap_or_default(); send(&mut out, 200, "", &b) }
                ("GET", f) if f.starts_with("/files/short/") => { let _ = out.write_all(b"HTTP/1.1 206 X\r\ncontent-length: 5\r\nconnection: close\r\n\r\nshort"); }
                // (Its first range slow, the others dropped: the slow one ends after the others have failed.)
                ("GET", f) if f.starts_with("/files/slowfirst/") => {
                    if headers.get("range").is_some_and(|r| r.starts_with("bytes=0-")) {
                        std::thread::sleep(Duration::from_millis(3000));
                        let b = p.lock().unwrap().files.get(&f[17..]).cloned().unwrap_or_default();
                        let z = b.len().min(8 << 20) - 1;
                        send(&mut out, 206, &format!("content-range: bytes 0-{z}/{}\r\n", b.len()), &b[..=z])
                    }
                }
                ("GET", f) if f.starts_with("/files/") => {
                    let Some(b) = p.lock().unwrap().files.get(&f[7..]).cloned() else { return send(&mut out, 404, "", b"") };
                    match headers.get("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')) {
                        Some((a, z)) => { let (a, z): (usize, usize) = (a.parse().unwrap(), z.parse::<usize>().unwrap().min(b.len() - 1)); send(&mut out, 206, &format!("content-range: bytes {a}-{z}/{}\r\n", b.len()), &b[a..=z]) }
                        None => send(&mut out, 200, "", &b),
                    }
                }
                ("POST", "/links") => {
                    let mode = p.lock().unwrap().links.clone();
                    match mode.as_str() {
                        "refused" => return send(&mut out, 500, "", b"{}"),
                        "nonsense" => return send(&mut out, 200, "", b"not json"),
                        "truncated" => { let _ = out.write_all(b"HTTP/1.1 200 X\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"lay"); return }
                        "none" => return send(&mut out, 200, "", br#"{"layers": {}}"#),
                        _ => {}
                    }
                    let layers: serde_json::Map<String, serde_json::Value> = json["digests"].as_array().unwrap().iter().map(|d| {
                        let d = d.as_str().unwrap();
                        let size = p.lock().unwrap().files.get(&format!("{d}.erofs")).map(|f| f.len()).unwrap_or(0);
                        let (name, at) = match mode.as_str() { "elsewhere" => (format!("{d}-other"), ""), "expired" => (d.to_string(), "expired/"), _ => (d.to_string(), "") };
                        (name, serde_json::json!({ "size": 1, "erofs": { "url": format!("{base}/files/{at}{d}.erofs"), "size": size }, "blob": { "url": format!("{base}/files/{d}.blob") } }))
                    }).collect();
                    send(&mut out, 200, "", serde_json::json!({ "layers": layers }).to_string().as_bytes())
                }
                ("POST", "/layers-propose") => {
                    let answers: serde_json::Map<String, serde_json::Value> = json["layers"].as_array().unwrap().iter().map(|l| {
                        let d = l["digest"].as_str().unwrap();
                        let sum = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(hex(&d[7..])) };
                        (d.to_string(), serde_json::json!({ "do": "upload", "url": format!("{base}/up/{d}"), "id": d, "headers": { "x-amz-checksum-sha256": sum } }))
                    }).collect();
                    send(&mut out, 200, "", serde_json::json!({ "layers": answers, "uploaded": format!("{base}/uploaded") }).to_string().as_bytes())
                }
                ("PUT", u) if u.starts_with("/up/") => {
                    let ok = headers.get("x-amz-checksum-sha256").is_some_and(|s| { use base64::Engine; base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)) == *s });
                    if ok { p.lock().unwrap().uploaded.insert(u[4..].to_string(), body); send(&mut out, 200, "", b"") } else { send(&mut out, 400, "", b"BadDigest") }
                }
                ("POST", "/uploaded") => send(&mut out, 200, "", br#"{"do": "converting"}"#),
                ("POST", "/layers-kept") => { let mut p = p.lock().unwrap(); if json["keeping"] == true { p.first = Some(json) } else { p.kept = Some(json) } send(&mut out, 200, "", b"{}") }
                _ => send(&mut out, 404, "", b""),
            }
        });
    });
    (p, at)
}

fn hex(s: &str) -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() }

#[tokio::test(flavor = "multi_thread")]
async fn layers_from_the_store_are_fetched_when_used_and_pulled_ones_kept_after_the_job() {
    // A layer the store has (its EROFS file a stand-in: mounting it is not what is tested here), and its index
    // without links.
    let (stored, erofs) = (digest(b"a stored layer's blob"), vec![7u8; 300_000]);
    let (p, at) = plane([(format!("{stored}.erofs"), erofs.clone()), (format!("{stored}.blob"), b"a stored layer's blob".to_vec())].into_iter().collect());
    let index = serde_json::json!({ "layers": { &stored: { "size": 21, "erofs": { "size": erofs.len() }, "blob": {} } }, "refresh": { "url": format!("{at}/links"), "token": "" } });
    let s = start("store", Some(&index));
    let mut snap = s.snapshots().await;
    let layer = |key: &str, d: &str, image: &str| sapi::PrepareSnapshotRequest { key: key.into(), parent: String::new(), labels: [("containerd.io/snapshot.ref".to_string(), format!("chain-{key}")),
        ("containerd.io/snapshot/cri.layer-digest".to_string(), d.to_string()), ("containerd.io/snapshot/cri.image-layers".to_string(), image.to_string())].into_iter().collect(), ..Default::default() };
    // The stored layer: links asked for (with its image's), its file fetched; served (as root) or pulled as usual.
    let served = snap.prepare(layer("moby/1/extract-1", &stored, &stored)).await;
    assert!(p.lock().unwrap().asked.iter().any(|a| a == "POST /links") && p.lock().unwrap().asked.iter().any(|a| a.starts_with(&format!("GET /files/{stored}.erofs"))), "{:?}", p.lock().unwrap().asked);
    match served {
        Err(e) => {
            assert_eq!(e.code(), Code::AlreadyExists, "served: containerd told it exists");
            // Its blob read through the content store (as `docker save` reads it): its size from the index, its bytes
            // fetched whole from the store and kept.
            let mut c = s.content().await;
            assert_eq!(c.info(capi::InfoRequest { digest: stored.clone() }).await.unwrap().into_inner().info.unwrap().size, 21);
            assert_eq!(read_all(&mut c, &stored, 0, 0).await.unwrap(), b"a stored layer's blob");
        }
        Ok(m) => { assert_eq!(m.into_inner().mounts[0].r#type, "bind"); assert!(s.log().contains(&format!("{stored} pulled as usual")), "{}", s.log()) }
    }
    assert!(s.log().contains(&format!("{stored}'s file (0.3 MB) fetched in")), "{}", s.log());
    // A layer the store lacks: pulled as usual (its blob written to the content store as a pull does), with its
    // image's manifest naming both layers, as containerd's records do.
    let blob = b"a layer the store lacks".repeat(200);
    let lacked = digest(&blob);
    let mut content = s.content().await;
    let mut pull = msg(capi::WriteAction::Commit, "pull-1", blob.len() as i64, &lacked, 0, &blob);
    pull.labels.insert("containerd.io/distribution.source.ghcr.io".into(), "acme/app".into());
    write(&mut content, vec![pull]).await.unwrap();
    let manifest = b"{\"layers\": []}".to_vec();
    let mut m = msg(capi::WriteAction::Commit, "manifest-1", manifest.len() as i64, &digest(&manifest), 0, &manifest);
    m.labels.extend([("containerd.io/gc.ref.content.l.0".to_string(), stored.clone()), ("containerd.io/gc.ref.content.l.1".to_string(), lacked.clone())]);
    write(&mut content, vec![m]).await.unwrap();
    let mounts = snap.prepare(layer("moby/2/extract-2", &lacked, &lacked)).await.unwrap().into_inner().mounts;
    assert_eq!(mounts[0].r#type, "bind");
    assert!(!p.lock().unwrap().asked.iter().any(|a| a.contains("layers-propose")), "nothing offered while the job runs");
    // After the job: offered and uploaded at the link the control plane gives, with the SHA-256 it said; what came
    // from the store and what was pulled, said.
    // (Switched in, as its setup says on a machine.)
    let state = s.dir.join("state");
    std::fs::write(&state, "on: Docker answered on it 0.5 s after the switch").unwrap();
    let kept = Command::new(env!("CARGO_BIN_EXE_superci-layers")).args(["keep".to_string(), format!("--link={at}"), format!("--socket={}", s.socket.display()),
        format!("--content={}", s.dir.join("content").display()), format!("--index={}", s.dir.join("index.json").display()), "--most=20".into(), format!("--state={}", state.display())]).output().unwrap();
    assert!(kept.status.success(), "{}", String::from_utf8_lossy(&kept.stderr));
    assert_eq!(p.lock().unwrap().uploaded.get(&lacked), Some(&blob), "{}", String::from_utf8_lossy(&kept.stderr));
    assert!(p.lock().unwrap().asked.iter().any(|a| a == "POST /uploaded"));
    let said = p.lock().unwrap().kept.clone().unwrap();
    assert_eq!((said["served"].as_u64(), said["pulled"].as_u64(), said["pulled_bytes"].as_u64(), said["uploaded"].as_u64()), (Some(1), Some(1), Some(blob.len() as u64), Some(1)), "{said}");
    // (And before the uploads, what it had and pulled: said first, for the run's summary.)
    let first = p.lock().unwrap().first.take().expect("said before the uploads");
    assert_eq!((first["served"].as_u64(), first["pulled"].as_u64(), first["uploaded"].as_u64()), (Some(1), Some(1), None), "{first}");
    // The image's own blobs (there at the machine's start) are not offered; containerd out of reach is said as such.
    std::fs::write(s.dir.join("boot"), format!("{}\n", &lacked[7..])).unwrap();
    p.lock().unwrap().uploaded.clear();
    let again = Command::new(env!("CARGO_BIN_EXE_superci-layers")).args(["keep".to_string(), format!("--link={at}"), format!("--socket={}", s.socket.display()),
        format!("--content={}", s.dir.join("content").display()), format!("--index={}", s.dir.join("index.json").display()), format!("--boot={}", s.dir.join("boot").display())]).output().unwrap();
    assert!(again.status.success());
    let said = p.lock().unwrap().kept.clone().unwrap();
    assert_eq!((said["pulled"].as_u64(), said["uploaded"].as_u64()), (Some(0), Some(0)), "{said}");
    assert!(p.lock().unwrap().first.is_none(), "nothing to upload: said once");
    // Not switched in on its machine (its setup said why): nothing came from the store, and why is said.
    std::fs::write(&state, "off: the kernel does not mount an EROFS file: Block device required (os error 15)").unwrap();
    let off = Command::new(env!("CARGO_BIN_EXE_superci-layers")).args(["keep".to_string(), format!("--link={at}"), format!("--socket={}", s.socket.display()),
        format!("--content={}", s.dir.join("content").display()), format!("--index={}", s.dir.join("index.json").display()), format!("--state={}", state.display())]).output().unwrap();
    assert!(off.status.success());
    let said = p.lock().unwrap().kept.clone().unwrap();
    assert_eq!((said["served"].as_u64(), said["served_bytes"].as_u64()), (Some(0), Some(0)), "{said}");
    assert!(said["off"].as_str().is_some_and(|w| w.starts_with("the kernel does not mount an EROFS file")), "{said}");
    let gone = Command::new(env!("CARGO_BIN_EXE_superci-layers")).args(["keep".to_string(), format!("--link={at}"), format!("--socket={}", s.dir.join("none.sock").display())]).output().unwrap();
    assert!(gone.status.success(), "a keep never fails the machine's end");
    assert!(p.lock().unwrap().kept.clone().unwrap()["error"].as_str().is_some_and(|e| e.starts_with("containerd: ")), "{:?}", p.lock().unwrap().kept);
    let usage = Command::new(env!("CARGO_BIN_EXE_superci-layers")).args(["keep", "--most=soon"]).output().unwrap();
    assert_eq!(usage.status.code(), Some(2));
    // The index written again and SIGHUP: read again.
    std::fs::write(s.dir.join("index.json"), serde_json::json!({ "layers": {} }).to_string()).unwrap();
    unsafe { libc::kill(s.child.id() as i32, libc::SIGHUP); }
    let until = Instant::now() + Duration::from_secs(10);
    while !s.log().contains("0 layers in the store's index") { assert!(Instant::now() < until, "{}", s.log()); tokio::time::sleep(Duration::from_millis(50)).await }
}

/// A layer the job's last run used (`ahead` in the index, its link signed in it): its file fetched as the store
/// starts, before anything asks for it, with no link asked for.
#[tokio::test(flavor = "multi_thread")]
async fn layers_the_last_run_used_are_fetched_as_the_store_starts() {
    let (used, erofs) = (digest(b"a layer the last run used"), vec![9u8; 200_000]);
    let (p, at) = plane([(format!("{used}.erofs"), erofs.clone())].into_iter().collect());
    let index = serde_json::json!({ "layers": { &used: { "size": 25, "erofs": { "url": format!("{at}/files/{used}.erofs"), "size": erofs.len() }, "blob": {}, "ahead": true } }, "refresh": { "url": format!("{at}/links"), "token": "" } });
    let s = start("ahead", Some(&index));
    let until = Instant::now() + Duration::from_secs(10);
    while !s.log().contains("1 layers' files (0 MB) fetched ahead in") { assert!(Instant::now() < until, "{}", s.log()); tokio::time::sleep(Duration::from_millis(50)).await }
    let asked = p.lock().unwrap().asked.clone();
    assert!(asked.iter().any(|a| a.starts_with(&format!("GET /files/{used}.erofs"))) && !asked.iter().any(|a| a == "POST /links"), "{asked:?}");
}

/// A layer's EROFS file made as SuperCI's converter makes it (mkfs.erofs from its tar), where there is one.
fn erofs_of(tar: &[u8]) -> Option<Vec<u8>> {
    let mkfs = std::env::var("SUPERCI_MKFS_EROFS").unwrap_or_else(|_| "mkfs.erofs".into());
    let out = std::env::temp_dir().join(format!("superci-layers-erofs-{}-{}", std::process::id(), digest(tar)[7..19].to_string()));
    let mut c = Command::new(mkfs).args(["--tar=f", "--aufs", "-zlz4hc", "-Enoinline_data", "-T0", "--mkfs-time"]).arg(&out).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok()?;
    c.stdin.take()?.write_all(tar).ok()?;
    if !c.wait().ok()?.success() { return None }
    let b = std::fs::read(&out).ok();
    let _ = std::fs::remove_file(&out);
    b
}

/// A tar of one file, as a layer's blob is.
fn tar_of(name: &str, body: &[u8]) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("superci-layers-tar-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
    let out = Command::new("tar").args(["-C", dir.to_str().unwrap(), "--owner=0", "--group=0", "-cf", "-", name]).output().unwrap().stdout;
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// How what needs root runs: as it is, when this is root; through `sudo -n` where there is one without a password; not
/// at all elsewhere (Linux only).
fn as_root() -> Option<Vec<&'static str>> {
    if !cfg!(target_os = "linux") { return None }
    if unsafe { libc::geteuid() } == 0 { return Some(vec![]) }
    Command::new("sudo").args(["-n", "true"]).status().ok().filter(|s| s.success()).map(|_| vec!["sudo", "-n"])
}

/// `program` as root: through sudo, a coverage tool's settings given on its command line (sudo keeps only some of the
/// environment, whatever it is asked).
fn rooted(prefix: &[&str], program: &str) -> Command {
    let mut c = Command::new(prefix.first().copied().unwrap_or(program));
    if !prefix.is_empty() { c.args(&prefix[1..]).arg("env").args(std::env::vars().filter(|(k, _)| k.starts_with("SUPERCOV_")).map(|(k, v)| format!("{k}={v}"))).arg(program); }
    c
}

/// A socket made by root opened to the test.
fn open_socket(prefix: &[&str], socket: &std::path::Path) {
    let until = Instant::now() + Duration::from_secs(20);
    while !socket.exists() { assert!(Instant::now() < until, "{} not made", socket.display()); std::thread::sleep(Duration::from_millis(20)) }
    assert!(rooted(prefix, "chmod").arg("666").arg(socket).status().unwrap().success());
}

/// The program as root on `dir`'s folders (containerd's socket at `containerd`), its socket opened to the test.
fn launch_rooted(prefix: &[&str], dir: &std::path::Path, containerd: &std::path::Path) -> Child {
    let socket = dir.join("layers.sock");
    let _ = rooted(prefix, "rm").arg("-f").arg(&socket).status();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log")).unwrap();
    let child = rooted(prefix, env!("CARGO_BIN_EXE_superci-layers"))
        .args([format!("--root={}", dir.join("root").display()), format!("--content={}", dir.join("content").display()), format!("--socket={}", socket.display()),
            format!("--index={}", dir.join("index.json").display()), format!("--containerd={}", containerd.display()), "--parallel=4".into(), "--sync-remove".into()])
        .stderr(log).spawn().unwrap();
    open_socket(prefix, &socket);
    child
}

/// A containerd of the test's own, as root on `dir`'s folders, where there is one, with the program as its content
/// store (as `setup` makes a machine's): its socket.
fn own_containerd(prefix: &[&str], dir: &std::path::Path) -> Option<(Child, std::path::PathBuf)> {
    if !Command::new("containerd").arg("--version").output().is_ok_and(|o| o.status.success()) { return None }
    let socket = dir.join("c.sock");
    // (Without CRI, which this containerd has no use for and whose fixed port one left running would hold.)
    let config = superci_layers::setup::containerd_config("version = 2\ndisabled_plugins = [\"io.containerd.grpc.v1.cri\"]\n").unwrap().replace("/run/superci-layers.sock", dir.join("layers.sock").to_str().unwrap());
    std::fs::write(dir.join("containerd.toml"), config).unwrap();
    let child = rooted(prefix, "containerd").args(["--config".as_ref(), dir.join("containerd.toml").as_os_str(), "--address".as_ref(), socket.as_os_str(),
        "--root".as_ref(), dir.join("containerd-root").as_os_str(), "--state".as_ref(), dir.join("containerd-state").as_os_str()])
        .stdout(std::process::Stdio::null()).stderr(std::fs::File::create(dir.join("containerd.log")).unwrap()).spawn().ok()?;
    open_socket(prefix, &socket);
    Some((child, socket))
}

fn stop(prefix: &[&str], child: &mut Child) {
    // (Through sudo, the signal is relayed to the program.)
    let _ = rooted(prefix, "kill").args(["-TERM", &child.id().to_string()]).status();
    let _ = child.wait();
}

/// A layer from the store served as root on Linux: its file mounted, the snapshot committed, containerd told it exists
/// in the namespace that pulled it (a containerd of the test's own, where there is one); its blob read whole from the
/// store when asked for; the service restarted, the snapshot mounted again (its file fetched again when gone; let go
/// when the index no longer has the layer either); removed, its file taken away.
#[tokio::test(flavor = "multi_thread")]
async fn a_layer_from_the_store_is_served_and_mounted_again_after_a_restart() {
    let Some(prefix) = as_root() else { eprintln!("not root on Linux (and no sudo): skipped"); return };
    let tar = tar_of("hello", b"from the store");
    let Some(erofs) = erofs_of(&tar) else { eprintln!("no mkfs.erofs here: skipped"); return };
    let d = digest(&tar);
    // (A second layer of the image, fetched ahead as the last run used it, and a third the store has, fetched ahead too.)
    let (tar2, tar3, tar4) = (tar_of("second", b"the image's other layer"), tar_of("third", b"another"), tar_of("fourth", b"fetched ahead, never used"));
    let (erofs2, erofs3, erofs4) = (erofs_of(&tar2).unwrap(), erofs_of(&tar3).unwrap(), erofs_of(&tar4).unwrap());
    let (d2, d3, d4) = (digest(&tar2), digest(&tar3), digest(&tar4));
    let (p, at) = plane([(format!("{d}.erofs"), erofs.clone()), (format!("{d}.blob"), tar.clone()), (format!("{d2}.erofs"), erofs2.clone()), (format!("{d3}.erofs"), erofs3.clone()), (format!("{d4}.erofs"), erofs4.clone())].into_iter().collect());
    let entry = |d: &str, size: usize, erofs: &[u8], ahead: bool| serde_json::json!({ "size": size, "erofs": { "url": format!("{at}/files/{d}.erofs"), "size": erofs.len() }, "blob": { "url": format!("{at}/files/{d}.blob") }, "ahead": ahead, "usage": { "size": 4096, "inodes": 2 } });
    // (The third says nothing of what its files take: measured on its folder once mounted.)
    let mut third = entry(&d3, tar3.len(), &erofs3, true);
    third.as_object_mut().unwrap().remove("usage");
    let index = serde_json::json!({ "layers": { &d: entry(&d, tar.len(), &erofs, false), &d2: entry(&d2, tar2.len(), &erofs2, true), &d3: third, &d4: entry(&d4, tar4.len(), &erofs4, true) }, "refresh": { "url": format!("{at}/links"), "token": "" } });
    // (On disk: the kernel mounts an EROFS file from a filesystem that reads pages itself, which tmpfs is not. Short
    // names: the folder may be deep, as a coverage tool's is, and containerd takes no socket path over 104 bytes.)
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sl-{}", std::process::id()));
    let _ = rooted(&prefix, "rm").arg("-rf").arg(&dir).status();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
    let containerd = own_containerd(&prefix, &dir);
    let csock = containerd.as_ref().map(|(_, s)| s.clone()).unwrap_or(dir.join("no-containerd.sock"));
    let mut child = launch_rooted(&prefix, &dir, &csock);
    let log = || std::fs::read_to_string(dir.join("log")).unwrap_or_default();
    let ch = connect(&dir.join("layers.sock")).await;
    let mut snap = sapi::snapshots_client::SnapshotsClient::new(ch.clone());
    let layer = sapi::PrepareSnapshotRequest { key: "moby/1/extract-1".into(), labels: [("containerd.io/snapshot.ref".to_string(), "chain-1".to_string()),
        ("containerd.io/snapshot/cri.layer-digest".to_string(), d.clone()), ("containerd.io/snapshot/cri.image-layers".to_string(), format!("{d},,{d2},sha256:{}", "9".repeat(64)))].into_iter().collect(), ..Default::default() };
    assert_eq!(snap.prepare(layer.clone()).await.unwrap_err().code(), Code::AlreadyExists, "{}", log());
    assert!(log().contains(&format!("{d} from the store in")), "{}", log());
    // Made known in its namespace, in containerd.
    if let Some((_, csock)) = &containerd {
        let mut c = capi::content_client::ContentClient::new(connect(csock).await);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let mut r = tonic::Request::new(capi::InfoRequest { digest: d.clone() });
            r.metadata_mut().insert("containerd-namespace", "moby".parse().unwrap());
            if c.info(r).await.is_ok() { break }
            assert!(Instant::now() < until, "{}\n{}", log(), std::fs::read_to_string(dir.join("containerd.log")).unwrap_or_default());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // One of an image whose layers are not named, under a key of no namespace: served, nothing made known.
    let mut bare = layer.clone();
    bare.key = "/extract-3".into();
    bare.labels.remove("containerd.io/snapshot/cri.image-layers");
    bare.labels.insert("containerd.io/snapshot.ref".into(), "chain-3".into());
    bare.labels.insert("containerd.io/snapshot/cri.layer-digest".into(), d3.clone());
    assert_eq!(snap.prepare(bare).await.unwrap_err().code(), Code::AlreadyExists, "{}", log());
    // The second layer in the same namespace (made known there too, under a lease of its own), and in a namespace
    // whose name no request can carry (not made known, said); a layer with no digest said: made as containerd makes it.
    let mut second = layer.clone();
    second.key = "moby/1/extract-4".into();
    second.labels.insert("containerd.io/snapshot.ref".into(), "chain-4".into());
    second.labels.insert("containerd.io/snapshot/cri.layer-digest".into(), d2.clone());
    assert_eq!(snap.prepare(second.clone()).await.unwrap_err().code(), Code::AlreadyExists, "{}", log());
    second.key = "bad\u{1}ns/extract-5".into();
    second.labels.insert("containerd.io/snapshot.ref".into(), "chain-5".into());
    assert_eq!(snap.prepare(second).await.unwrap_err().code(), Code::AlreadyExists, "{}", log());
    let mut plain = layer.clone();
    plain.key = "moby/1/plain".into();
    plain.labels.remove("containerd.io/snapshot/cri.layer-digest");
    assert!(snap.prepare(plain).await.is_ok());
    // Asked again (another pull of it): found served.
    let mut again = layer.clone();
    again.key = "moby/1/extract-2".into();
    assert_eq!(snap.prepare(again).await.unwrap_err().code(), Code::AlreadyExists);
    // A container's layer on it: the served file's folder below it.
    let top = snap.prepare(sapi::PrepareSnapshotRequest { key: "moby/1/top".into(), parent: "moby/1/extract-1/superci-layers".into(), ..Default::default() }).await.unwrap().into_inner().mounts;
    assert!(top[0].options.iter().any(|o| o.starts_with("lowerdir=")), "{top:?}");
    // Its blob, asked for (as `docker save` asks): its size from the index, its bytes from the store.
    let mut c = capi::content_client::ContentClient::new(ch);
    assert_eq!(c.info(capi::InfoRequest { digest: d.clone() }).await.unwrap().into_inner().info.unwrap().size, tar.len() as i64);
    assert_eq!(read_all(&mut c, &d, 0, 0).await.unwrap(), tar);
    assert!(p.lock().unwrap().asked.iter().any(|a| a.starts_with(&format!("GET /files/{d}.blob"))));
    // A blob neither kept nor served: not found.
    assert_eq!(c.info(capi::InfoRequest { digest: digest(b"nobody's") }).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(read_all(&mut c, &digest(b"nobody's"), 0, 0).await.unwrap_err().code(), Code::NotFound);
    // Restarted: mounted again from its file; with its file gone, fetched again; with the index not having it either,
    // let go, said why.
    // (Named by its layer: the folder is root's own.)
    let file = dir.join("root/files").join(format!("{}.erofs", &d[7..]));
    // (What is left over cleaned up while a layer fetched ahead is kept.)
    let mut snap = sapi::snapshots_client::SnapshotsClient::new(connect(&dir.join("layers.sock")).await);
    snap.cleanup(sapi::CleanupRequest::default()).await.unwrap();
    // (Restarted mounted as it was; then unmounted, from its file; its file gone (fetched again); its file not an EROFS
    // file (not mounted, said); gone and the store not giving it (said); gone, and the index not having it.)
    stop(&prefix, &mut child);
    child = launch_rooted(&prefix, &dir, &csock);
    for (step, index) in [("", index.clone()), ("gone", index.clone()), ("garbage", index.clone()), ("unfetchable", index.clone()), ("gone", serde_json::json!({ "layers": {} }))] {
        stop(&prefix, &mut child);
        let mounted = std::fs::read_to_string("/proc/self/mounts").unwrap().lines().filter(|l| l.contains(dir.to_str().unwrap()) && l.contains(" erofs ")).map(|l| l.split(' ').nth(1).unwrap().to_string()).collect::<Vec<_>>();
        for m in &mounted { let _ = rooted(&prefix, "umount").args(["-l", m]).status(); }
        match step {
            "gone" => { let _ = rooted(&prefix, "rm").arg("-f").arg(&file).status(); }
            "garbage" => { let _ = rooted(&prefix, "sh").args(["-c", &format!("echo garbage > {}", file.display())]).status(); }
            "unfetchable" => { let _ = rooted(&prefix, "rm").arg("-f").arg(&file).status(); p.lock().unwrap().files.remove(&format!("{d}.erofs")); }
            _ => {}
        }
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
        child = launch_rooted(&prefix, &dir, &csock);
    }
    assert!(log().contains("could not be mounted again: its file is gone and the index has no such layer"), "{}", log());
    // Removed: let go, its file taken away.
    let mut snap = sapi::snapshots_client::SnapshotsClient::new(connect(&dir.join("layers.sock")).await);
    snap.remove(sapi::RemoveSnapshotRequest { key: "moby/1/top".into(), ..Default::default() }).await.unwrap();
    snap.remove(sapi::RemoveSnapshotRequest { key: "moby/1/extract-1/superci-layers".into(), ..Default::default() }).await.unwrap();
    snap.cleanup(sapi::CleanupRequest::default()).await.unwrap();
    stop(&prefix, &mut child);
    if let Some((mut c, _)) = containerd { stop(&prefix, &mut c) }
    // (Each mount under the folder: one there is not unmounted with the folder, which is no mount itself.)
    let _ = rooted(&prefix, "sh").args(["-c", &format!("grep -o ' {0}[^ ]*' /proc/self/mounts | sort -r | xargs -r umount -l; rm -rf {0}", dir.display())]).status();
}

/// A layer's file fetched as the store's bucket and the control plane answer it amiss: a link run out (signed again),
/// none yet (asked for), a new one refused, not said, or given for other layers only, run out again; a connection
/// dropped; a range not taken; one cut short; a file not what its digest says. Each said, and the layer pulled as usual.
#[tokio::test(flavor = "multi_thread")]
async fn a_layers_file_is_fetched_whatever_the_bucket_and_the_control_plane_answer() {
    let erofs = vec![3u8; (17 << 20) + 5];
    let digests: Vec<String> = (0..14).map(|i| digest(format!("layer {i}").as_bytes())).collect();
    let files: HashMap<String, Vec<u8>> = digests.iter().map(|d| (format!("{d}.erofs"), erofs.clone())).collect();
    let (p, at) = plane(files);
    let link = |i: usize, at_path: &str| format!("{at}/files/{at_path}{}.erofs", digests[i]);
    // (In turn: run out, none, run out with links refused, given for others, not said, run out again; dropped, not
    // ranged, cut short; not its digest.)
    // (Then, for later: run out twice more, its first range slow, and a digest that is none.)
    let urls = [link(0, "expired/"), String::new(), link(2, "expired/"), link(3, "expired/"), link(4, "expired/"), link(5, "expired/"), link(6, "drop/"), link(7, "whole/"), link(8, "short/"), link(9, ""),
        link(10, "expired/"), link(11, "expired/"), link(12, "slowfirst/"), link(13, "")];
    let layers: serde_json::Map<String, serde_json::Value> = digests.iter().zip(urls.iter()).enumerate().map(|(i, (d, u))| {
        let mut erofs_entry = serde_json::json!({ "url": u, "size": erofs.len() });
        if i == 9 { erofs_entry["digest"] = digest(b"something else").into() }
        if i == 13 { erofs_entry["digest"] = "nonsense".into() }
        (d.clone(), serde_json::json!({ "size": 1, "erofs": erofs_entry, "blob": {} }))
    }).collect();
    let s = start("fetch", Some(&serde_json::json!({ "layers": layers, "refresh": { "url": format!("{at}/links"), "token": "t" } })));
    let mut snap = s.snapshots().await;
    let modes = ["", "", "refused", "elsewhere", "nonsense", "expired", "", "", "", ""];
    for (i, d) in digests[..10].iter().enumerate() {
        p.lock().unwrap().links = modes[i].to_string();
        let req = sapi::PrepareSnapshotRequest { key: format!("moby/f/{i}"), labels: [("containerd.io/snapshot.ref".to_string(), format!("chain-f{i}")), ("containerd.io/snapshot/cri.layer-digest".to_string(), d.clone())].into_iter().collect(), ..Default::default() };
        let _ = snap.prepare(req).await;
    }
    let log = s.log();
    for (i, d) in digests[..10].iter().enumerate() {
        let fetched = log.contains(&format!("{d}'s file (17.8 MB) fetched in"));
        assert_eq!(fetched, i < 2, "{i}: {}", log.lines().filter(|l| l.contains(d.as_str())).collect::<Vec<_>>().join("\n"));
    }
    assert!(log.contains("refused its link") && log.contains("not what its digest says") && log.contains("the store answered 200") && log.contains("sent 5 of"), "{log}");
    // Links asked for and answered cut short, or with none; one with no link and none given; an index saying nowhere
    // to ask, and one whose place to ask is out of reach.
    for (i, mode) in ["truncated", "none", "", ""].into_iter().enumerate() {
        p.lock().unwrap().links = mode.to_string();
        let req = sapi::PrepareSnapshotRequest { key: format!("moby/g/{i}"), labels: [("containerd.io/snapshot.ref".to_string(), format!("chain-g{i}")), ("containerd.io/snapshot/cri.layer-digest".to_string(), digests[i + 10].clone())].into_iter().collect(), ..Default::default() };
        let _ = snap.prepare(req).await;
    }
    assert!(s.log().contains("the store's digest"), "{}", s.log());
    // A layer the last run used, with no link yet: asked for as the store starts.
    p.lock().unwrap().links = String::new();
    let ahead = start("aheadnolink", Some(&serde_json::json!({ "layers": { &digests[1]: { "size": 1, "erofs": { "url": "", "size": erofs.len() }, "blob": {}, "ahead": true } }, "refresh": { "url": format!("{at}/links"), "token": "" } })));
    let until = Instant::now() + Duration::from_secs(20);
    while !ahead.log().contains("fetched ahead in") { assert!(Instant::now() < until, "{}", ahead.log()); tokio::time::sleep(Duration::from_millis(100)).await }
    let gone = 1u16;
    for (name, refresh) in [("norefresh", serde_json::Value::Null), ("faraway", serde_json::json!({ "url": format!("http://127.0.0.1:{gone}/links"), "token": "" }))] {
        let index = serde_json::json!({ "layers": { &digests[0]: { "size": 1, "erofs": { "url": link(0, "expired/"), "size": erofs.len() }, "blob": {} }, &digests[1]: { "size": 1, "erofs": { "url": "", "size": erofs.len() }, "blob": {} } }, "refresh": refresh });
        let s = start(name, Some(&index));
        let mut snap = s.snapshots().await;
        for (i, d) in digests[..2].iter().enumerate() {
            let req = sapi::PrepareSnapshotRequest { key: format!("moby/{name}/{i}"), labels: [("containerd.io/snapshot.ref".to_string(), format!("chain-{name}{i}")), ("containerd.io/snapshot/cri.layer-digest".to_string(), d.clone())].into_iter().collect(), ..Default::default() };
            let _ = snap.prepare(req).await;
        }
        assert!(s.log().matches("pulled as usual").count() >= 2, "{}", s.log());
    }
}
