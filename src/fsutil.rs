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

/// True if `path` is a regular file that can be executed: the owner has an
/// exec bit on Unix, any regular file on Windows (executability there is
/// decided by extension, applied during the PATH search — see [`which`]).
///
/// One home (audit 2026-07-20, AUD-281): `commands/download/decrypt` and
/// `plugins` each carried a copy, and the `plugins` one omitted the
/// `is_file` guard, so a directory with an exec bit read as executable.
pub(crate) fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The first executable named `name` on `PATH`, or `None`. On Windows each
/// `PATH` entry is expanded across `PATHEXT`, so a bare `ffmpeg` finds
/// `ffmpeg.exe`; a `name` that already carries an extension is used
/// verbatim. Shared by the decrypt-tool and plugin-interpreter lookups
/// (audit 2026-07-20, AUD-281).
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| executable_candidates(&dir, name))
        .find(|candidate| is_executable(candidate))
}

/// Resolves an executable via an optional override value. When
/// `override_value` is present it must be executable, else `None` — a
/// set-but-broken override never silently falls through to a different tool
/// (fail-closed). When absent, `fallback` runs (typically a `PATH` lookup).
/// One home for the decrypt-tool (`AUDIBLE_FFMPEG`/`AUDIBLE_AAXCLEAN_CLI`) and
/// plugin-interpreter (`AUDIBLE_PYTHON`) overrides (audit 2026-07-20, AUD-284).
pub(crate) fn resolve_with_override(
    override_value: Option<std::ffi::OsString>,
    fallback: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(value) = override_value {
        let path = PathBuf::from(value);
        return is_executable(&path).then_some(path);
    }
    fallback()
}

/// The filenames to try for `name` in one `PATH` directory. On Unix a
/// program is its bare name; on Windows an executable carries an extension,
/// so a bare `ffmpeg` never matches `ffmpeg.exe` — try each `PATHEXT`
/// extension (unless the caller already gave one).
#[cfg(not(windows))]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    if Path::new(name).extension().is_some() {
        return vec![dir.join(name)];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    pathext
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| dir.join(format!("{name}{ext}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A present, executable override wins and the fallback never runs.
    #[test]
    fn override_uses_an_executable_value() {
        let exe = std::env::current_exe().unwrap();
        let got = resolve_with_override(Some(exe.clone().into_os_string()), || {
            panic!("fallback must not run when the override is usable")
        });
        assert_eq!(got, Some(exe));
    }

    /// A present but non-executable override yields `None` — it never
    /// silently falls through to a different tool (fail-closed).
    #[test]
    fn override_broken_value_does_not_fall_through() {
        let bogus = std::ffi::OsString::from("definitely-not-an-executable-xyz");
        let mut fallback_ran = false;
        let got = resolve_with_override(Some(bogus), || {
            fallback_ran = true;
            Some(PathBuf::from("fallback"))
        });
        assert_eq!(got, None);
        assert!(
            !fallback_ran,
            "a present override must not run the fallback"
        );
    }

    /// An absent override runs the fallback.
    #[test]
    fn override_absent_runs_the_fallback() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(resolve_with_override(None, || Some(exe.clone())), Some(exe));
    }

    /// On Unix a tool is looked up by its bare name.
    #[cfg(not(windows))]
    #[test]
    fn candidates_are_the_bare_name_on_unix() {
        assert_eq!(
            executable_candidates(Path::new("/usr/bin"), "ffmpeg"),
            vec![PathBuf::from("/usr/bin/ffmpeg")]
        );
    }

    /// On Windows the PATH search must try the executable extensions, so a
    /// bare `ffmpeg` finds `ffmpeg.exe`. A name that already has an extension
    /// is used verbatim.
    #[cfg(windows)]
    #[test]
    fn candidates_apply_pathext_on_windows() {
        let cands = executable_candidates(Path::new(r"C:\bin"), "ffmpeg");
        assert!(
            cands
                .iter()
                .any(|c| c.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))),
            "{cands:?} should include an .exe candidate"
        );
        assert!(
            cands.iter().all(|c| c.starts_with(r"C:\bin")),
            "candidates stay in the given dir"
        );
        assert_eq!(
            executable_candidates(Path::new(r"C:\bin"), "ffmpeg.exe"),
            vec![PathBuf::from(r"C:\bin\ffmpeg.exe")],
            "an explicit extension is used as-is"
        );
    }

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
