//! Small filesystem primitives shared by the auth and config writers
//! (audit 2026-07-17, B5): atomic persist with a per-writer temp name and
//! fsync, a cross-process write lock, and the owner-only writers for
//! secret-bearing files and directories (audit 2026-07-18, B1+C5).

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Writes `content` atomically next to `path`.
///
/// The temp file gets a **unique** name and is opened with `O_EXCL` — two
/// concurrent writers (the CLI and the agent both write auth files back
/// after a token refresh, and both edit the config) previously shared one
/// `<name>.tmp` with `O_TRUNC` and interleaved into a single inode: a torn
/// auth file loses the refresh token, a torn config every setting. The
/// content is fsynced before the rename (no zero-length file on power
/// loss), and the parent directory is synced best-effort after it (the
/// rename itself survives a crash). `mode` sets Unix permissions from the
/// first byte (no-op elsewhere).
pub(crate) fn persist_atomically(
    path: &Path,
    content: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    let tmp_path = unique_tmp_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&tmp_path)?;
    let written = file.write_all(content).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    sync_parent_dir(path);
    Ok(())
}

/// A temp name no other writer can be using: pid + random suffix. Stale
/// files (a crash between create and rename) are orphaned, not reused —
/// `create_new` would refuse them, hence the random component.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        hex::encode(rand::random::<[u8; 4]>())
    ))
}

/// Best-effort fsync of `path`'s parent directory, so the rename itself
/// is durable. Errors are ignored: not every filesystem supports it, and
/// the file content is already synced.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    // No directory fsync on non-unix targets; `path` is otherwise unused.
    #[cfg(not(unix))]
    let _ = path;
}

/// Writes `bytes` to `path` owner-only (`0o600`) from the first byte: the
/// mode rides on the create itself, so a secret-bearing file (passphrase
/// store, key sidecar, token file) is never observable world-readable —
/// not even between create and a later chmod. A pre-existing file keeps
/// its inode mode on open and is re-tightened afterwards (it may predate
/// the rule). On Windows the mode is a no-op — secret files rest on
/// user-profile isolation, not an ACL (AUD-198).
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private_opts(path, bytes, true)
}

/// [`write_private`] that refuses to overwrite: the create fails when the
/// file already exists (`create_new` — no TOCTOU window between an
/// exists-check and the write).
pub(crate) fn write_private_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private_opts(path, bytes, false)
}

fn write_private_opts(path: &Path, bytes: &[u8], overwrite: bool) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Creates `dir` (and missing parents) and sets it owner-only (`0o700`),
/// idempotently. An existing `dir` is re-tightened too — installs
/// predating the rule pick it up on the next write. Only `dir` itself is
/// tightened; created parents keep the umask. Mode is a no-op on
/// non-Unix (AUD-198).
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The cross-process write lock guarding a file's read-modify-write
/// cycle: `<file>.lock` next to the target. Callers hold the guard from
/// read to rename — without it, concurrent CLI + agent edits silently
/// dropped one side's change. Lock usage: `let mut lock = write_lock(p)?;
/// let _guard = lock.write()?;`.
pub(crate) fn write_lock(path: &Path) -> std::io::Result<fd_lock::RwLock<std::fs::File>> {
    let lock_path = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    Ok(fd_lock::RwLock::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_is_atomic_unique_and_syncs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        persist_atomically(&path, b"version = 1\n", None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"version = 1\n");
        // Overwrite keeps the file consistent and leaves no temp litter.
        persist_atomically(&path, b"version = 1\n# edited\n", None).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp files may remain");
    }

    #[cfg(unix)]
    #[test]
    fn persist_honors_the_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alice.auth");
        persist_atomically(&path, b"secret", Some(0o600)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn two_writers_never_share_a_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_ne!(unique_tmp_path(&path), unique_tmp_path(&path));
    }

    #[cfg(unix)]
    #[test]
    fn write_private_creates_owner_only_files() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.voucher");
        write_private(&path, b"{\"key\":\"k\",\"iv\":\"i\"}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must never be world-readable");
        // Overwriting an existing (say, pre-fix 0644) file tightens it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrite must restore owner-only permissions");
    }

    #[test]
    fn write_private_new_refuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alice.auth");
        write_private_new(&path, b"first").unwrap();
        let error = write_private_new(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_tightens_new_and_existing_dirs() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("audible");
        create_private_dir(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a fresh config dir must be owner-only");
        // A pre-existing world-traversable dir (install predating the
        // rule) is re-tightened on the next call.
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "an existing dir must be re-tightened");
    }
}
