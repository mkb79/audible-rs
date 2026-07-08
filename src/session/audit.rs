//! Agent audit log (AUD-118): an append-only JSONL record of every `/v1`
//! request the agent serves — timestamp, caller (token label or `admin`),
//! method, path, and HTTP status. **Never** request bodies or tokens.
//! Written only by the long-lived agent, where remote callers exist (the
//! ephemeral plugin broker's `audit` is a no-op — plugin runs are the
//! invoking user's own CLI). Retention: one size-based rotation at ~10
//! MiB to `agent-audit.jsonl.1`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Rotate the log once it passes this size.
const ROTATE_AT_BYTES: u64 = 10 * 1024 * 1024;

/// One audit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// ISO-8601 UTC timestamp.
    pub time: String,
    /// Who made the request: token label, or `admin`.
    pub caller: String,
    pub method: String,
    pub path: String,
    /// HTTP status the agent replied with.
    pub status: u16,
    /// Extra context for security-relevant calls (AUD-122) — currently
    /// `external:<host>` when an api request targeted an external host
    /// (attempted or served). Still never bodies or tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Append-only audit writer (serialized across connections).
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    /// Log at `<data_dir>/agent-audit.jsonl`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("agent-audit.jsonl"),
            lock: Mutex::new(()),
        }
    }

    /// The log path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry; errors are swallowed to a warning (auditing
    /// must never break serving).
    pub fn append(&self, entry: &AuditEntry) {
        if let Err(error) = self.try_append(entry) {
            tracing::warn!(%error, "could not write the audit log");
        }
    }

    fn try_append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        let _guard = self.lock.lock().expect("audit lock");
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|meta| meta.len() >= ROTATE_AT_BYTES) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("jsonl.1"));
        }
        let mut line = serde_json::to_vec(entry).expect("audit entry serializes");
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(&line)
    }

    /// Reads the recorded entries (oldest first). Malformed lines are
    /// skipped. Missing file → empty.
    pub fn read(&self) -> Vec<AuditEntry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(caller: &str, status: u16) -> AuditEntry {
        AuditEntry {
            time: "2026-07-06T00:00:00Z".into(),
            caller: caller.into(),
            method: "GET".into(),
            path: "/v1/accounts".into(),
            status,
            detail: None,
        }
    }

    #[test]
    fn appends_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AuditLog::new(tmp.path());
        log.append(&entry("admin", 200));
        log.append(&AuditEntry {
            detail: Some("external:cde-ta-g7g.amazon.com".into()),
            ..entry("web", 403)
        });
        let entries = log.read();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].caller, "admin");
        // The detail field is absent for normal calls, kept for external
        // ones (AUD-122).
        assert_eq!(entries[0].detail, None);
        assert_eq!(entries[1].status, 403);
        assert_eq!(
            entries[1].detail.as_deref(),
            Some("external:cde-ta-g7g.amazon.com")
        );
        // 0600, tokens never appear.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(log.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
