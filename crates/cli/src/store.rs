//! What SuperCI keeps on this computer: its own sign-ins to your clouds, in a folder of its own (`~/.superci`, or
//! `SUPERCI_HOME`), so that the dashboard opens signed in and commands run without a browser. Nothing of another
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal: Option<modal::Session>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Key>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<Plane>,
    /// The AWS sign-in kept here was ended by AWS (it ends one after twelve hours at most) and has not been made
    /// again: said where it matters, and the key and the control plane stay, so looking goes on working.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub aws_ended: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Key { pub name: String, pub value: String, pub planes: Vec<String> }

impl Kept {
    pub fn signed_in(&self) -> bool { self.cloudflare.is_some() || self.aws.is_some() || self.modal.is_some() }
    /// Worth keeping: a sign-in, or what is left of one AWS ended (the control plane and the key it is read with).
    pub fn worth_keeping(&self) -> bool { self.signed_in() || (self.aws_ended && self.key.is_some() && self.plane.is_some()) }
}

#[derive(Clone)]
pub struct Store { dir: PathBuf }

impl Store {
    /// SuperCI's folder: `SUPERCI_HOME`, or `.superci` in the home folder. None when there is no home folder.
    pub fn new() -> Option<Store> {
        let var = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
        let dir = var("SUPERCI_HOME").or_else(|| var("HOME").or_else(|| var("USERPROFILE")).map(|h| h.join(".superci")))?;
        Some(Store { dir })
    }

    #[cfg(test)]
    pub fn at(dir: impl Into<PathBuf>) -> Store { Store { dir: dir.into() } }

    pub fn path(&self) -> PathBuf { self.dir.join("sign-ins.json") }

    /// What is kept (nothing, when there is no file or it cannot be read as this version writes it).
    pub fn read(&self) -> Kept {
        std::fs::read(self.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Writes what is kept: to a file beside it first, then moved over, so a stop midway leaves the last one whole.
    pub fn write(&self, kept: &Kept) -> crate::Result<()> {
        let say = |e: std::io::Error| format!("could not keep the sign-in in {}: {e}", self.dir.display());
        std::fs::create_dir_all(&self.dir).map_err(say)?;
        only_the_owner(&self.dir, 0o700);
        let tmp = self.dir.join(format!("sign-ins.json.{}", std::process::id()));
        let text = serde_json::to_vec_pretty(kept).map_err(|e| e.to_string())?;
        write_private(&tmp, &text).map_err(say)?;
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
            key: Some(Key { name: "DASHBOARD_KEY_1_AB".into(), value: "k".into(), planes: vec!["p1".into()] }),
            plane: Some(Plane::Aws { account_id: "123456789012".into(), region: "us-east-1".into(), url: "https://x.lambda-url.us-east-1.on.aws".into(), plane_id: "p1".into(), label: "superci".into() }), ..Default::default() };
        store.write(&kept).unwrap();
        let back = store.read();
        assert!(back.signed_in() && back.cloudflare.is_none());
        assert_eq!(back.modal.unwrap().workspace, "acme");
        assert_eq!(back.aws.unwrap().account_id, "123456789012");
        assert!(back.key == kept.key && back.plane == kept.plane);
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
}
