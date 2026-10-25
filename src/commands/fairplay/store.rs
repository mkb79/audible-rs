//! The FairPlay store: what `audible fairplay` keeps instead of database
//! rows.
//!
//! ```text
//! <store>/index.json                              downloaded titles
//! <store>/packages/<marketplace>-<asin>.movpkg    the AVFoundation packages
//! <store>/keys/<marketplace>-<asin>.fpkey         their persistable keys (0600)
//! ```
//!
//! AVFoundation downloads into the system cache
//! (`~/Library/Caches/com.apple.nsurlsessiond/Downloads/`), where macOS or a
//! cleanup tool may delete it; [`Store::adopt_package`] moves the finished
//! package here. Apple's documentation expects a package to stay where it was
//! downloaded, but a moved package plays fine (spike test D, AUD-321). The
//! store must lie outside the download dir, or `download orphans` would take
//! its files for orphans.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::config::ctx::Ctx;

/// Where the store lives by default, below the home directory: the macOS
/// place for an application's own data.
const DEFAULT_DIR: &str = "Library/Application Support/audible-rs/FairPlay";
const INDEX_FILE: &str = "index.json";
const KEYS_DIR: &str = "keys";
const PACKAGES_DIR: &str = "packages";

/// The downloaded titles, keyed by [`Index::key`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Index {
    #[serde(default)]
    pub(super) entries: BTreeMap<String, Entry>,
}

impl Index {
    /// The index key of a title.
    pub(super) fn key(marketplace: &str, asin: &str) -> String {
        format!("{marketplace}/{asin}")
    }

    pub(super) fn get(&self, marketplace: &str, asin: &str) -> Option<&Entry> {
        self.entries.get(&Self::key(marketplace, asin))
    }
}

/// One downloaded title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Entry {
    pub(super) asin: String,
    pub(super) marketplace: String,
    /// The audible-rs account the license was granted to.
    pub(super) account: String,
    pub(super) title: Option<String>,
    /// The package, in the store's `packages/`.
    pub(super) package: PathBuf,
    /// The key file, relative to the store.
    pub(super) keys: String,
    pub(super) codec: Option<String>,
    pub(super) content_format: Option<String>,
    pub(super) quality: String,
    pub(super) xhe: bool,
    pub(super) spatial: bool,
    pub(super) granted_right: Option<String>,
    pub(super) access_expiry: Option<String>,
    /// RFC 3339.
    pub(super) downloaded_at: String,
}

/// The store directory.
pub(super) struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens (and creates, owner-only) the store: `dir` when given, else the
    /// default below the home directory. Refuses a location inside the
    /// download dir.
    pub(super) fn open(ctx: &Ctx, dir: Option<&String>) -> Result<Self> {
        let dir = match dir {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(DEFAULT_DIR)
            }
        };
        ensure_outside(&dir, &crate::naming::download_dir(ctx)?)?;
        let store = Self { dir };
        crate::fsutil::create_private_dir(&store.dir.join(KEYS_DIR))
            .and_then(|()| crate::fsutil::create_private_dir(&store.dir.join(PACKAGES_DIR)))
            .and_then(|()| crate::fsutil::create_private_dir(&store.dir))
            .with_context(|| format!("could not create {}", store.dir.display()))?;
        Ok(store)
    }

    #[cfg(test)]
    fn at(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The index; empty when there is none yet.
    pub(super) fn load(&self) -> Result<Index> {
        let path = self.dir.join(INDEX_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a FairPlay index", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Writes the index atomically.
    pub(super) fn save(&self, index: &Index) -> Result<()> {
        let path = self.dir.join(INDEX_FILE);
        let json = serde_json::to_vec_pretty(index).expect("the index serializes");
        crate::fsutil::persist_atomically(&path, &json, Some(0o600))
            .with_context(|| format!("could not write {}", path.display()))
    }

    /// Where the package of a title belongs.
    pub(super) fn package_path(&self, marketplace: &str, asin: &str) -> PathBuf {
        self.dir
            .join(PACKAGES_DIR)
            .join(format!("{marketplace}-{asin}.movpkg"))
    }

    /// Moves a freshly downloaded package from the system cache to
    /// [`Store::package_path`], replacing an earlier package of the title. A
    /// rename where both lie on one volume; otherwise a copy (into a partial
    /// name first, so a broken copy never looks complete) and the source is
    /// removed.
    pub(super) fn adopt_package(
        &self,
        marketplace: &str,
        asin: &str,
        downloaded: &Path,
    ) -> Result<PathBuf> {
        let target = self.package_path(marketplace, asin);
        remove_package(&target)?;
        if std::fs::rename(downloaded, &target).is_ok() {
            return Ok(target);
        }
        let partial = target.with_extension("movpkg.partial");
        remove_package(&partial)?;
        copy_tree(downloaded, &partial)
            .and_then(|()| std::fs::rename(&partial, &target))
            .with_context(|| {
                format!(
                    "could not move the package from {} to {}",
                    downloaded.display(),
                    target.display()
                )
            })?;
        std::fs::remove_dir_all(downloaded)
            .with_context(|| format!("could not remove {}", downloaded.display()))?;
        Ok(target)
    }

    /// The key file name of a title, relative to the store.
    pub(super) fn keys_name(marketplace: &str, asin: &str) -> String {
        format!("{KEYS_DIR}/{marketplace}-{asin}.fpkey")
    }

    /// Writes persistable keys (`{content id: base64}`), owner-only.
    pub(super) fn write_keys(&self, name: &str, keys: &HashMap<String, Vec<u8>>) -> Result<()> {
        let encoded: BTreeMap<&str, String> = keys
            .iter()
            .map(|(id, key)| {
                (
                    id.as_str(),
                    base64::engine::general_purpose::STANDARD.encode(key),
                )
            })
            .collect();
        let path = self.dir.join(name);
        let json = serde_json::to_vec(&encoded).expect("keys serialize");
        crate::fsutil::persist_atomically(&path, &json, Some(0o600))
            .with_context(|| format!("could not write {}", path.display()))
    }

    /// Reads persistable keys written by [`Store::write_keys`].
    pub(super) fn read_keys(&self, name: &str) -> Result<HashMap<String, Vec<u8>>> {
        let path = self.dir.join(name);
        let bytes =
            std::fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
        let encoded: BTreeMap<String, String> = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a FairPlay key file", path.display()))?;
        encoded
            .into_iter()
            .map(|(id, key)| {
                base64::engine::general_purpose::STANDARD
                    .decode(key)
                    .map(|key| (id, key))
                    .with_context(|| format!("{} holds a damaged key", path.display()))
            })
            .collect()
    }
}

/// Deletes a package directory; a missing one is fine. Only ever called on
/// `.movpkg` paths the store owns or AVFoundation just reported.
pub(super) fn remove_package(package: &Path) -> Result<()> {
    match std::fs::remove_dir_all(package) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not delete {}", package.display())),
    }
}

/// Copies a directory tree (an AVFoundation package: directories and files).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Refuses a store inside the download dir, where `download orphans` would
/// list (and could delete) its files. A download dir that does not resolve
/// cannot collide.
fn ensure_outside(dir: &Path, download_dir: &Path) -> Result<()> {
    let Ok(managed) = download_dir.canonicalize() else {
        return Ok(());
    };
    if resolve_for_containment(dir)?.starts_with(&managed) {
        bail!(
            "the FairPlay store must lie outside the download dir ({})",
            managed.display()
        );
    }
    Ok(())
}

/// Canonicalizes `path` without requiring it to exist: the longest existing
/// ancestor resolves symlinks, the remainder is appended verbatim.
fn resolve_for_containment(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("could not resolve {}", path.display()))?;
    let mut existing = absolute.as_path();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(base) = existing.canonicalize() {
            return Ok(rest.iter().rev().fold(base, |path, part| path.join(part)));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return Ok(absolute),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(package: &Path) -> Entry {
        Entry {
            asin: "B0EXAMPLE1".into(),
            marketplace: "de".into(),
            account: "alice".into(),
            title: Some("Example".into()),
            package: package.to_path_buf(),
            keys: Store::keys_name("de", "B0EXAMPLE1"),
            codec: Some("mp4a.40.2".into()),
            content_format: None,
            quality: "High".into(),
            xhe: false,
            spatial: false,
            granted_right: None,
            access_expiry: None,
            downloaded_at: "2026-10-25T12:00:00Z".into(),
        }
    }

    #[test]
    fn index_round_trips_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        assert!(store.load().unwrap().entries.is_empty());
        let mut index = Index::default();
        index.entries.insert(
            Index::key("de", "B0EXAMPLE1"),
            entry(Path::new("/tmp/x.movpkg")),
        );
        store.save(&index).unwrap();
        let back = store.load().unwrap();
        let found = back.get("de", "B0EXAMPLE1").unwrap();
        assert_eq!(found.package, Path::new("/tmp/x.movpkg"));
        assert!(back.get("us", "B0EXAMPLE1").is_none());
    }

    #[test]
    fn keys_round_trip_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(KEYS_DIR)).unwrap();
        let store = Store::at(dir.path());
        let name = Store::keys_name("de", "B0EXAMPLE1");
        let keys = HashMap::from([("content-a".to_owned(), vec![1u8, 2, 3])]);
        store.write_keys(&name, &keys).unwrap();
        assert_eq!(store.read_keys(&name).unwrap(), keys);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(&name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    fn package_in(dir: &Path) -> PathBuf {
        let package = dir.join("download.movpkg");
        std::fs::create_dir_all(package.join("Data")).unwrap();
        std::fs::write(package.join("Data").join("segment"), b"audio").unwrap();
        std::fs::write(package.join("boot.xml"), b"<xml/>").unwrap();
        package
    }

    #[test]
    fn adopting_moves_the_package_and_replaces_an_old_one() {
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(PACKAGES_DIR)).unwrap();
        let store = Store::at(root.path());
        let old = store.package_path("de", "B0EXAMPLE1");
        std::fs::create_dir_all(old.join("stale")).unwrap();

        let downloaded = package_in(cache.path());
        let target = store
            .adopt_package("de", "B0EXAMPLE1", &downloaded)
            .unwrap();
        assert_eq!(target, old);
        assert!(!downloaded.exists());
        assert!(!target.join("stale").exists());
        assert_eq!(
            std::fs::read(target.join("Data/segment")).unwrap(),
            b"audio"
        );
    }

    #[test]
    fn copying_a_package_keeps_its_tree() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        let package = package_in(from.path());
        let copy = to.path().join("copy.movpkg");
        copy_tree(&package, &copy).unwrap();
        assert_eq!(std::fs::read(copy.join("boot.xml")).unwrap(), b"<xml/>");
        assert_eq!(std::fs::read(copy.join("Data/segment")).unwrap(), b"audio");
        remove_package(&copy).unwrap();
        remove_package(&copy).unwrap();
        assert!(!copy.exists());
    }

    #[test]
    fn the_store_stays_out_of_the_download_dir() {
        let root = tempfile::tempdir().unwrap();
        let downloads = root.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        assert!(ensure_outside(&downloads.join("fairplay"), &downloads).is_err());
        assert!(ensure_outside(&downloads, &downloads).is_err());
        assert!(ensure_outside(&root.path().join("fairplay"), &downloads).is_ok());
    }
}
