//! The `passwords` file — an `authorized_keys`-style, multi-account store
//! for auth-file passphrases (`password_source = "file"`).
//!
//! One entry per line, `<account> <passphrase>`; the account name is the
//! first whitespace-delimited token (names are `[a-z0-9-_]+`, so the split
//! is unambiguous) and the passphrase is the rest of the line (internal
//! spaces kept, the line terminator stripped). Blank lines and lines
//! starting with `#` are ignored; the first matching entry wins.
//!
//! The passphrase itself is plaintext, so the file is written `0600` and a
//! group/other-readable file draws a warning. [`upsert`] and [`remove`]
//! preserve comments and every other account's line.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};

use super::schema::Account;

/// Default passwords file (`<config_dir>/passwords`).
pub fn default_path(config_dir: &Path) -> PathBuf {
    config_dir.join("passwords")
}

/// The passwords file for an account: its `password_file` (with `~`
/// expanded) or the default under the config dir.
pub fn resolve_path(config_dir: &Path, account: &Account) -> PathBuf {
    match &account.password_file {
        Some(path) => crate::naming::expand_tilde(path),
        None => default_path(config_dir),
    }
}

/// Account name of an entry line, or `None` for blanks/comments/malformed.
fn entry_account(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    line.split_whitespace().next()
}

/// Looks up an account's passphrase. `Ok(None)` if the file or the entry is
/// absent. Warns (once) if the existing file is group/other-readable.
pub fn lookup(path: &Path, account: &str) -> Result<Option<SecretString>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("could not read {}", path.display()));
        }
    };
    warn_if_readable(path);

    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((acct, rest)) = trimmed.split_once(char::is_whitespace)
            && acct == account
        {
            return Ok(Some(SecretString::from(rest.trim_start().to_owned())));
        }
    }
    Ok(None)
}

/// Inserts or replaces an account's entry, preserving every other line.
/// Creates the file `0600` if missing.
pub fn upsert(path: &Path, account: &str, passphrase: &SecretString) -> Result<()> {
    let pass = passphrase.expose_secret();
    if pass.contains(['\n', '\r']) {
        bail!(
            "passphrase contains a newline and cannot be stored in the passwords \
             file; use password_source = \"command\" instead"
        );
    }

    let existing = read_or_empty(path)?;
    let mut out = String::new();
    let mut replaced = false;
    for line in existing.lines() {
        if entry_account(line) == Some(account) {
            if !replaced {
                out.push_str(account);
                out.push(' ');
                out.push_str(pass);
                out.push('\n');
                replaced = true;
            }
            // Drop any old/duplicate line for this account.
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(account);
        out.push(' ');
        out.push_str(pass);
        out.push('\n');
    }
    write_0600(path, &out)
}

/// Removes an account's entry (and any duplicates). No-op if the file or the
/// entry is absent; every other line is preserved.
pub fn remove(path: &Path, account: &str) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("could not read {}", path.display()));
        }
    };

    let mut out = String::new();
    let mut changed = false;
    for line in existing.lines() {
        if entry_account(line) == Some(account) {
            changed = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if changed {
        write_0600(path, &out)?;
    }
    Ok(())
}

fn read_or_empty(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Writes `content` to `path` with owner-only (`0o600`) permissions (parent
/// dirs created). On Windows the mode is a no-op — the `passwords` file rests on
/// user-profile isolation, not an ACL (AUD-198).
fn write_0600(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not set 0600 on {}", path.display()))?;
    }
    Ok(())
}

/// Warns if the file is readable by group or other (Unix only).
fn warn_if_readable(path: &Path) {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            eprintln!(
                "warning: {} is readable by group/other — it holds plaintext \
                 passphrases; run `chmod 600 {}`",
                path.display(),
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn upsert_then_lookup_round_trips_including_spaces() {
        let dir = tmp();
        let path = dir.path().join("passwords");
        upsert(&path, "alice", &SecretString::from("a pass with spaces")).unwrap();
        let got = lookup(&path, "alice").unwrap().unwrap();
        assert_eq!(got.expose_secret(), "a pass with spaces");
    }

    #[test]
    fn upsert_replaces_only_the_target_and_keeps_comments_and_others() {
        let dir = tmp();
        let path = dir.path().join("passwords");
        std::fs::write(&path, "# header\nalice old\nwork secret\n").unwrap();
        upsert(&path, "alice", &SecretString::from("new")).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# header"), "comment kept: {raw:?}");
        assert!(raw.contains("work secret"), "other account kept: {raw:?}");
        assert_eq!(
            lookup(&path, "alice").unwrap().unwrap().expose_secret(),
            "new"
        );
        assert_eq!(
            lookup(&path, "work").unwrap().unwrap().expose_secret(),
            "secret"
        );
    }

    #[test]
    fn lookup_misses_are_none() {
        let dir = tmp();
        let path = dir.path().join("passwords");
        assert!(lookup(&path, "nobody").unwrap().is_none()); // no file
        upsert(&path, "alice", &SecretString::from("x")).unwrap();
        assert!(lookup(&path, "work").unwrap().is_none()); // file, no entry
    }

    #[test]
    fn remove_targets_one_account() {
        let dir = tmp();
        let path = dir.path().join("passwords");
        upsert(&path, "alice", &SecretString::from("a")).unwrap();
        upsert(&path, "work", &SecretString::from("b")).unwrap();
        remove(&path, "alice").unwrap();
        assert!(lookup(&path, "alice").unwrap().is_none());
        assert_eq!(lookup(&path, "work").unwrap().unwrap().expose_secret(), "b");
        remove(&path, "absent").unwrap(); // no-op
    }

    #[test]
    fn upsert_rejects_a_newline_passphrase() {
        let dir = tmp();
        let path = dir.path().join("passwords");
        let err = upsert(&path, "alice", &SecretString::from("a\nb")).unwrap_err();
        assert!(err.to_string().contains("newline"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tmp();
        let path = dir.path().join("passwords");
        upsert(&path, "alice", &SecretString::from("x")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }
}
