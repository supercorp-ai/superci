//! What SuperCI keeps on this computer: its own sign-ins to your clouds, in a folder of its own (`~/.superci`, or
//! `SUPERCI_HOME`), so that the dashboard opens signed in and commands run without a browser. They are kept by
//! workspace (`workspaces/<id>/sign-ins.json`): each workspace is a world of its own (its sign-ins, and through
//! them its control plane and everything of it), and nothing of one is seen from another. Beside them, what the
//! workspace is called and looks like in a list (`workspace.json`: nothing secret). Nothing of another
//! tool's is read (no AWS profile, no wrangler or Modal login) and nothing is written anywhere else. `superci logout`
//! removes it.
//!
//! A sign-in here is as strong as the sign-in itself: the file is the owner's alone (0600 in a 0700 folder).
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::plane::Plane;
use crate::{aws, cloudflare, modal};

/// What is kept: each cloud's sign-in, the key the control planes were handed (with the name it is stored under
/// there, which says when it expires) and which of them have it, and the control plane in use.
#[derive(Serialize, Deserialize, Default)]
pub struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<cloudflare::Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws: Option<aws::Session>,
    /// Further AWS accounts signed in to (a second sign-in to AWS adds an account; it replaces none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aws_more: Vec<aws::Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal: Option<modal::Session>,
    /// Further Modal workspaces signed in to (`superci login modal --add`): each runs jobs through a launcher of its
    /// own. `modal` stays the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modal_more: Vec<modal::Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Key>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<Plane>,
    /// The other control planes known, so that each is listed at once, also one whose account's sign-in has ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planes_more: Vec<Plane>,
    /// The AWS sign-in kept here was ended by AWS (it ends one after twelve hours at most) and has not been made
    /// again: said where it matters, and the key and the control plane stay, so looking goes on working.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub aws_ended: bool,
    /// The same of Cloudflare's sign-in (Cloudflare no longer renews it) and of Modal's (Modal no longer takes its
    /// token): what the other sign-ins reach goes on working, and what needs this one says to sign in again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cloudflare_ended: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub modal_ended: bool,
}

/// What a workspace is called and looks like: what a list of workspaces says of each without asking its control
/// plane (and without using its sign-ins from another workspace's page). Kept beside the sign-ins, and not one.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct About {
    /// Its name: given when it was made, and from then on what its CI is called (AcmeCI). None: it has none yet.
    #[serde(default)]
    pub name: String,
    /// Each code host connected: its kind (`github`, `gitlab`) and its name (an organization, a GitLab's address).
    #[serde(default)]
    pub hosts: Vec<(String, String)>,
    /// The first GitHub organization's picture, as GitHub serves it (a `data:` address), and whose it is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub picture: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub picture_of: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Key { pub name: String, pub value: String, pub planes: Vec<String> }

impl Kept {
    pub fn signed_in(&self) -> bool { self.cloudflare.is_some() || self.aws.is_some() || !self.aws_more.is_empty() || self.modal.is_some() || !self.modal_more.is_empty() }
    /// Worth keeping: a sign-in, or what is left of one that ended (the control plane, and the key it is read with:
    /// what the dashboard keeps always has one).
    pub fn worth_keeping(&self) -> bool { self.signed_in() || ((self.aws_ended || self.cloudflare_ended || self.modal_ended) && self.plane.is_some()) }
}

#[derive(Clone)]
pub struct Store { dir: PathBuf }

/// What the first workspace is called until it is given a name.
pub const FIRST: &str = "default";

/// SuperCI's folder: `SUPERCI_HOME`, or `.superci` in the home folder. None when there is no home folder.
pub fn root() -> Option<PathBuf> { root_in(|n| std::env::var_os(n)) }

/// The same, from these settings (the environment's, or a test's).
fn root_in(var_os: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let var = |n: &str| var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("SUPERCI_HOME").or_else(|| var("HOME").or_else(|| var("USERPROFILE")).map(|h| h.join(".superci")))
}

/// Whether this can be a workspace's id: 2 to 24 lowercase letters, digits and dashes, starting with a letter (it
/// is a folder's name, and what `--workspace` takes).
pub fn valid_id(id: &str) -> crate::Result<()> {
    let ok = (2..=24).contains(&id.len()) && id.starts_with(|c: char| c.is_ascii_lowercase()) && !id.ends_with('-') && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err("A workspace's id is 2 to 24 lowercase letters, digits and dashes, starting with a letter (acme, acme-staging).".into()) }
}

/// The id a workspace gets from its name: its letters and digits in lowercase, a dash for whatever is between them
/// (Acme CI: acme-ci).
pub fn id_from(name: &str) -> crate::Result<String> {
    let mut id = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() { id.push(c.to_ascii_lowercase()) } else if !id.ends_with('-') { id.push('-') }
    }
    let id: String = id.trim_start_matches(|c: char| !c.is_ascii_lowercase()).chars().take(24).collect();
    let id = id.trim_end_matches('-').to_string();
    valid_id(&id).map_err(|_| "A workspace's name needs two letters or more (AcmeCI, Build Farm).".to_string())?;
    Ok(id)
}

/// The workspaces kept under a folder, by name, in order.
pub fn workspaces_in(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join("workspaces")).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| valid_id(n).is_ok()).collect();
    names.sort();
    names
}

/// A workspace in a line of a list: read from what it keeps, nothing asked of any cloud.
pub struct Listed { pub id: String, pub name: String, pub hosts: Vec<(String, String)>, pub picture: String, pub place: String, pub signed_in: bool,
    /// The control plane it looks after (none yet: empty).
    pub plane_id: String }

impl Listed {
    /// What it is called where people read it: its name, or its id until it has one.
    pub fn title(&self) -> &str { if self.name.is_empty() { &self.id } else { &self.name } }
}

/// The workspaces kept under a folder, with `also` among them (the one a page is of, which has no folder until it
/// keeps something).
pub fn listed(root: &std::path::Path, also: Option<&str>) -> Vec<Listed> {
    let mut names = workspaces_in(root);
    if let Some(a) = also.filter(|a| !names.iter().any(|n| n == a)) { names.push(a.to_string()); names.sort() }
    names.into_iter().map(|id| {
        let store = Store::under(root, &id);
        let (kept, about) = (store.read(), store.about());
        Listed { name: about.name, hosts: about.hosts, picture: about.picture, place: kept.plane.as_ref().map(|p| p.place()).unwrap_or_default(), signed_in: kept.signed_in(),
            plane_id: kept.plane.as_ref().map(|p| p.plane_id().to_string()).unwrap_or_default(), id }
    }).collect()
}

/// A workspace under a new id: its folder is renamed. The new id must be free.
pub fn rename(root: &std::path::Path, from: &str, to: &str) -> crate::Result<()> {
    valid_id(to)?;
    if from == to { return Ok(()) }
    let (old, new) = (root.join("workspaces").join(from), root.join("workspaces").join(to));
    if new.exists() { return Err(format!("There is a workspace with the id {to} already.")) }
    if !old.is_dir() { return Store::under(root, to).make() }
    std::fs::rename(&old, &new).map_err(|e| format!("could not rename {}: {e}", old.display()))
}

/// Which workspace is meant, of those kept under `root`: the one named; with none named, the only one, or the first
/// one when there is none yet. With several and none named, nothing picks: the names are given back, for whoever
/// asked to say which.
pub fn which(root: &std::path::Path, named: Option<&str>) -> std::result::Result<String, Vec<String>> {
    let all = workspaces_in(root);
    match named.map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()) {
        Some(n) => Ok(n),
        None => match all.as_slice() { [] => Ok(FIRST.to_string()), [only] => Ok(only.clone()), _ => Err(all) },
    }
}

/// The workspace the dashboard showed last (a note in SuperCI's folder), if it is still kept: where the dashboard
/// opens when it is started with several and none named. It only says which pages open first: no command reads it,
/// and nothing is ever done in a workspace because of it.
pub fn last_opened(root: &std::path::Path) -> Option<String> {
    let name = std::fs::read_to_string(root.join("last-workspace")).ok()?.trim().to_string();
    workspaces_in(root).into_iter().find(|w| *w == name)
}

/// Notes the workspace the dashboard shows now (see `last_opened`): one kept here, or the one it was started in, so
/// its name is a workspace's id.
pub fn note_opened(root: &std::path::Path, name: &str) {
    let _ = write_private(&root.join("last-workspace"), name.as_bytes());
}

/// Whether a workspace is kept under `root` (the first one is there before it keeps anything, when no other is).
pub fn kept_here(root: &std::path::Path, name: &str) -> bool {
    let all = workspaces_in(root);
    all.iter().any(|w| w == name) || (all.is_empty() && name == FIRST)
}

impl Store {
    /// Where one workspace's sign-ins are kept under SuperCI's folder. None when there is no home folder.
    pub fn of(workspace: &str) -> Option<Store> { root().map(|r| Store::under(&r, workspace)) }

    pub fn under(root: &std::path::Path, workspace: &str) -> Store { Store { dir: root.join("workspaces").join(workspace) } }

    /// What the workspace is called and looks like in a list (nothing, when it has not been said yet).
    pub fn about(&self) -> About {
        std::fs::read(self.dir.join("workspace.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn set_about(&self, about: &About) -> crate::Result<()> {
        self.make()?;
        write_private(&self.dir.join("workspace.json"), &pretty(about)).map_err(|e| format!("could not write in {}: {e}", self.dir.display()))
    }

    /// The workspace's id: its folder's name.
    pub fn workspace(&self) -> String { self.dir.file_name().and_then(|n| n.to_str()).unwrap_or(FIRST).to_string() }

    /// Makes the workspace's folder (a new workspace is listed before anything is signed in to).
    pub fn make(&self) -> crate::Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("could not make {}: {e}", self.dir.display()))?;
        self.owners_alone();
        Ok(())
    }

    /// The workspace's folder and the two above it (SuperCI's, and the one that holds the workspaces) are the
    /// owner's alone.
    fn owners_alone(&self) {
        only_the_owner(&self.dir, 0o700);
        // (A folder named `workspaces` is in one: SuperCI's.)
        let Some(all) = self.dir.parent().filter(|p| p.file_name().is_some_and(|n| n == "workspaces")) else { return };
        only_the_owner(all, 0o700);
        // (A folder with a name always has one above it.)
        only_the_owner(all.parent().unwrap_or(all), 0o700);
    }

    /// Forgets the workspace on this computer: its sign-ins and its folder. Nothing in any cloud is touched.
    pub fn forget(&self) -> crate::Result<()> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not remove {}: {e}", self.dir.display())),
        }
    }

    #[cfg(test)]
    pub fn at(dir: impl Into<PathBuf>) -> Store { Store { dir: dir.into() } }

    /// SuperCI's folder, which this workspace's is in.
    pub fn root(&self) -> Option<PathBuf> { Some(self.dir.parent()?.parent()?.to_path_buf()) }

    pub fn path(&self) -> PathBuf { self.dir.join("sign-ins.json") }

    /// Held while a sign-in is renewed (see `Dashboard::cf_client`): Cloudflare takes what renews a sign-in once, and
    /// two SuperCIs renewing the same one at once would end it. Let go when dropped.
    pub fn renewing(&self) -> Option<std::fs::File> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join(".renewing")).ok()?;
        // (A lock refused leaves it unheld; whoever asked goes on either way, as without the file.)
        let _ = f.lock();
        Some(f)
    }

    /// What is kept (nothing, when there is no file or it cannot be read as this version writes it).
    pub fn read(&self) -> Kept {
        let mut kept: Kept = std::fs::read(self.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // A key a SuperCI from before 0.19.13 wrote (it names them DASHBOARD_KEY_…): the same key under the name control
        // planes read now, handed to each again under it. (Taken for one that has expired, it would be replaced, and
        // the two versions would replace each other's keys at each command.)
        if let Some(k) = kept.key.as_mut().filter(|k| k.name.starts_with("DASHBOARD_KEY_")) { k.name = k.name.replacen("DASHBOARD_KEY_", "DEVICE_KEY_", 1); k.planes.clear() }
        kept
    }

    /// Writes what is kept: to a file beside it first, then moved over, so a stop midway leaves the last one whole.
    pub fn write(&self, kept: &Kept) -> crate::Result<()> {
        let say = |e: std::io::Error| format!("could not keep the sign-in in {}: {e}", self.dir.display());
        std::fs::create_dir_all(&self.dir).map_err(say)?;
        self.owners_alone();
        let tmp = self.dir.join(format!("sign-ins.json.{}", std::process::id()));
        write_private(&tmp, &pretty(kept)).map_err(say)?;
        std::fs::rename(&tmp, self.path()).map_err(say)
    }

    /// Removes what is kept. Whether there was anything.
    pub fn remove(&self) -> crate::Result<bool> {
        match std::fs::remove_file(self.path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("could not remove {}: {e}", self.path().display())),
        }
    }
}

/// What is kept here as JSON for people to read: strings, numbers, flags and lists, which serde_json always writes.
fn pretty<T: Serialize>(value: &T) -> Vec<u8> { serde_json::to_vec_pretty(value).expect("what is kept here is always JSON") }

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

/// On Windows a file in the user's own folder is the user's (and the administrators') by what it inherits.
#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> { std::fs::write(path, bytes) }

#[cfg(unix)]
fn only_the_owner(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn only_the_owner(_: &std::path::Path, _: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_and_removes() {
        let dir = std::env::temp_dir().join(format!("superci-store-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        let store = Store::at(&dir);
        assert!(!store.read().signed_in() && !store.remove().unwrap());
        let kept = Kept { modal: Some(modal::Session { token_id: "ak-test".into(), token_secret: "as-test".into(), workspace: "acme".into() }),
            aws: Some(aws::Session::for_test("123456789012")),
            key: Some(Key { name: "DEVICE_KEY_1_AB".into(), value: "k".into(), planes: vec!["p1".into()] }),
            plane: Some(Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "p1".into(), label: "superci".into() }), ..Default::default() };
        store.write(&kept).unwrap();
        let back = store.read();
        assert!(back.signed_in() && back.cloudflare.is_none());
        assert_eq!(back.modal.unwrap().workspace, "acme");
        assert!(back.modal_more.is_empty() && !std::fs::read_to_string(store.path()).unwrap().contains("modal_more"), "none further: not written");
        // Further Modal workspaces are kept beside the first, in order; with them alone, it is still a sign-in.
        let session = |w: &str| modal::Session { token_id: format!("ak-{w}"), token_secret: format!("as-{w}"), workspace: w.into() };
        store.write(&Kept { modal_more: vec![session("acme-two"), session("acme-three")], ..Default::default() }).unwrap();
        let more = store.read();
        assert!(more.signed_in() && more.modal.is_none());
        assert_eq!(more.modal_more.iter().map(|m| m.workspace.as_str()).collect::<Vec<_>>(), ["acme-two", "acme-three"]);
        store.write(&kept).unwrap();
        assert_eq!(back.aws.unwrap().account_id, "123456789012");
        assert!(back.key == kept.key && back.plane == kept.plane);
        // One a SuperCI from before 0.19.13 wrote: the same key under its new name, to be handed again under it.
        store.write(&Kept { key: Some(Key { name: "DASHBOARD_KEY_1794848867_4HXLAV".into(), value: "k".into(), planes: vec!["p1".into()] }), ..Default::default() }).unwrap();
        assert!(store.read().key == Some(Key { name: "DEVICE_KEY_1794848867_4HXLAV".into(), value: "k".into(), planes: vec![] }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // A file this version cannot read is nothing kept, not an error.
        std::fs::write(store.path(), b"{ not json").unwrap();
        assert!(!store.read().signed_in() && !store.read().worth_keeping());
        assert!(store.remove().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspaces_are_folders_and_none_is_picked_among_several() {
        let root = std::env::temp_dir().join(format!("superci-workspaces-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        // None yet: the first one, which is there before it keeps anything.
        assert!(workspaces_in(&root).is_empty() && which(&root, None) == Ok("default".into()) && kept_here(&root, "default") && !kept_here(&root, "acme"));
        // One: it, whatever it is called. A name is taken as typed (in lowercase), and is not looked up here.
        Store::under(&root, "acme").make().unwrap();
        assert_eq!(which(&root, None), Ok("acme".into()));
        assert!(kept_here(&root, "acme") && !kept_here(&root, "default"));
        assert_eq!(which(&root, Some(" Globex ")), Ok("globex".into()));
        // Two: nothing picks.
        let globex = Store::under(&root, "globex");
        globex.write(&Kept { modal: Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "globex-team".into() }), ..Default::default() }).unwrap();
        globex.set_about(&About { name: "GlobexCI".into(), hosts: vec![("github".into(), "globex".into())], ..Default::default() }).unwrap();
        assert_eq!(which(&root, None), Err(vec!["acme".to_string(), "globex".to_string()]));
        assert_eq!(globex.workspace(), "globex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [root.clone(), root.join("workspaces"), root.join("workspaces/globex"), root.join("workspaces/acme")] { assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700, "{}", dir.display()) }
            assert_eq!(std::fs::metadata(globex.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(globex.root().unwrap(), root);
        // Each one's sign-ins are its own: nothing of one is in another.
        assert!(!Store::under(&root, "acme").read().signed_in() && globex.read().signed_in());
        let all = listed(&root, Some("default"));
        assert_eq!(all.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["acme", "default", "globex"]);
        assert!(all[2].signed_in && all[2].name == "GlobexCI" && all[2].hosts == [("github".to_string(), "globex".to_string())] && !all[0].signed_in);
        assert_eq!((all[2].title(), all[0].title()), ("GlobexCI", "acme"));
        // Signing out removes the sign-ins and leaves what it is called.
        assert!(globex.remove().unwrap() && globex.about().name == "GlobexCI");
        globex.write(&Kept { modal: Some(modal::Session { token_id: "ak".into(), token_secret: "as".into(), workspace: "globex-team".into() }), ..Default::default() }).unwrap();
        // A new name: the folder moves, with what it keeps; a taken name and one that is no name are refused.
        assert_eq!(rename(&root, "globex", "acme").unwrap_err(), "There is a workspace with the id acme already.");
        assert!(rename(&root, "globex", "Globex Inc").unwrap_err().starts_with("A workspace's id is 2 to 24 lowercase letters"));
        rename(&root, "globex", "initech").unwrap();
        assert!(Store::under(&root, "initech").read().signed_in() && Store::under(&root, "initech").about().name == "GlobexCI" && workspaces_in(&root) == ["acme", "initech"]);
        // Forgotten: its folder goes; the other stays.
        Store::under(&root, "initech").forget().unwrap();
        assert_eq!(workspaces_in(&root), ["acme"]);
        for no in ["a", "Acme", "9lives", "acme corp", "acme_corp", "../acme", "acme-", "a-name-that-is-much-too-long"] { assert!(valid_id(no).is_err(), "{no}") }
        // An id from a name.
        for (name, id) in [("MyCI", "myci"), ("Acme CI", "acme-ci"), (" Build  Farm 2 ", "build-farm-2"), ("9 Lives", "lives"), ("A name that is much too long for it", "a-name-that-is-much-too")] { assert_eq!(id_from(name).unwrap(), id, "{name}") }
        assert_eq!(id_from("7").unwrap_err(), "A workspace's name needs two letters or more (AcmeCI, Build Farm).");
        assert!(id_from("   ").is_err() && id_from("").is_err());
        // SuperCI's folder: SUPERCI_HOME, else .superci in the home folder (HOME, else USERPROFILE); one set empty is none.
        let given = |set: &'static [(&'static str, &'static str)]| move |n: &str| set.iter().find(|(k, _)| *k == n).map(|(_, v)| std::ffi::OsString::from(v));
        assert_eq!(root_in(given(&[("SUPERCI_HOME", "/s"), ("HOME", "/h")])), Some(PathBuf::from("/s")));
        assert_eq!(root_in(given(&[("SUPERCI_HOME", ""), ("HOME", "/h")])), Some(PathBuf::from("/h/.superci")));
        assert_eq!(root_in(given(&[("USERPROFILE", "/u")])), Some(PathBuf::from("/u/.superci")));
        assert_eq!(root_in(given(&[])), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder that cannot be made or written in is said, with where; a lock asked where none can be made is none;
    /// a store not under SuperCI's folder has no root.
    #[test]
    fn what_cannot_be_written_is_said_and_a_lock_that_cannot_be_had_is_none() {
        let base = std::env::temp_dir().join(format!("superci-store-refused-{}-{}", std::process::id(), superci_core::crypto::random_id(6)));
        std::fs::create_dir_all(&base).unwrap();
        // (A file where a folder would be.)
        std::fs::write(base.join("file"), b"").unwrap();
        let under_file = Store::at(base.join("file/workspaces/x"));
        assert!(under_file.make().unwrap_err().starts_with("could not make"));
        assert!(under_file.set_about(&About::default()).unwrap_err().starts_with("could not make"));
        assert!(under_file.write(&Kept::default()).unwrap_err().starts_with("could not keep the sign-in in"));
        assert!(under_file.renewing().is_none());
        // Its file beside the sign-ins taken by a folder: the sign-ins cannot be written, and nothing is kept.
        let store = Store::at(base.join("ws"));
        std::fs::create_dir_all(base.join(format!("ws/sign-ins.json.{}", std::process::id()))).unwrap();
        assert!(store.write(&Kept::default()).unwrap_err().starts_with("could not keep the sign-in in"));
        assert!(!store.path().exists());
        // Its lock's file taken by a folder: no lock; otherwise one.
        std::fs::create_dir_all(base.join("ws/.renewing")).unwrap();
        assert!(store.renewing().is_none());
        assert!(Store::at(base.join("other")).renewing().is_some());
        assert!(Store::at("").root().is_none() && Store::at("x").root().is_none());
        // A workspace that cannot be moved (its folder's folder not writable) is said.
        let root = base.join("root");
        Store::under(&root, "acme").make().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("workspaces"), std::fs::Permissions::from_mode(0o500)).unwrap();
            // (Whoever is root may write anywhere: then it moves.)
            if let Err(e) = rename(&root, "acme", "globex") { assert!(e.starts_with("could not rename"), "{e}") }
            std::fs::set_permissions(root.join("workspaces"), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
