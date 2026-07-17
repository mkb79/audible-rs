//! Agent audit log (AUD-118): an append-only JSONL record of every `/v1`
//! request the agent serves — timestamp, caller (token label or `admin`),
//! method, path, and HTTP status. **Never** request bodies or tokens.
//! Written only by the long-lived agent, where remote callers exist (the
//! ephemeral plugin broker's `audit` is a no-op — plugin runs are the
//! invoking user's own CLI). Retention: one size-based rotation at ~10
//! MiB to `agent-audit.jsonl.1`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

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

/// A queued writer job: an entry, or a flush handshake.
enum Job {
    Write(AuditEntry),
    Flush(std::sync::mpsc::Sender<()>),
}

/// Append-only audit writer. The file IO runs on a dedicated blocking
/// writer thread (audit 2026-07-17, E4 — it used to run synchronously
/// under a mutex on the daemon's hottest async path, per request); the
/// async side only enqueues. Ordering is the channel's FIFO.
pub struct AuditLog {
    path: PathBuf,
    sender: std::sync::mpsc::Sender<Job>,
}

fn write_entry(path: &Path, entry: &AuditEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= ROTATE_AT_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
    }
    let mut line = serde_json::to_vec(entry).expect("audit entry serializes");
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(&line)
}

impl AuditLog {
    /// Log at `<data_dir>/agent-audit.jsonl`.
    pub fn new(data_dir: &Path) -> Self {
        let path = data_dir.join("agent-audit.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel::<Job>();
        let writer_path = path.clone();
        std::thread::Builder::new()
            .name("audit-writer".into())
            .spawn(move || {
                for job in receiver {
                    match job {
                        Job::Write(entry) => {
                            if let Err(error) = write_entry(&writer_path, &entry) {
                                tracing::warn!(%error, "could not write the audit log");
                            }
                        }
                        Job::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("the audit writer thread spawns");
        Self { path, sender }
    }

    /// The log path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Enqueues one entry for the writer thread; failures on the write
    /// side are swallowed to a warning there (auditing must never break
    /// serving), and a dead writer drops entries silently the same way.
    pub fn append(&self, entry: &AuditEntry) {
        let _ = self.sender.send(Job::Write(entry.clone()));
    }

    /// Waits until every entry enqueued so far has hit the disk (tests,
    /// orderly shutdown).
    pub fn flush(&self) {
        let (done, wait) = std::sync::mpsc::channel();
        if self.sender.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
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
        log.flush();
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
