//! App tokens for the agent (AUD-117): scoped, optionally account-bound,
//! optionally expiring bearer tokens for external consumers (web
//! backends). Persisted **hashed** so they survive agent restarts —
//! `agent-tokens.json` (0600) holds only the SHA-256 of each token, never
//! the token itself; the plaintext is shown once at `create` time. The
//! agent looks tokens up by hash on every request, reloading the file
//! when it changes (so `create`/`revoke` take effect without a restart).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One persisted token record — the token itself is never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Human label (also the revoke key); unique within the store.
    pub label: String,
    /// SHA-256 of the token, hex.
    pub hash: String,
    /// Granted scopes (subset of [`crate::plugins::VALID_SCOPES`]).
    pub scopes: Vec<String>,
    /// Optional account binding — requests are pinned to this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Expiry (ISO-8601 UTC); `None` never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Creation time (ISO-8601 UTC).
    pub created: String,
}

impl TokenRecord {
    /// Whether the record is past its expiry at `now`. An `expires` value
    /// that does not parse counts as expired (fail-closed): a corrupt or
    /// hand-edited timestamp must never yield an eternal token.
    fn is_expired(&self, now: time::OffsetDateTime) -> bool {
        match self.expires.as_deref() {
            None => false,
            Some(text) => parse_iso(text).is_none_or(|expiry| now > expiry),
        }
    }
}

/// The token file plus a small mtime-keyed in-memory cache. Cheap to
/// clone-construct; safe to share.
pub struct TokenStore {
    path: PathBuf,
    cache: Mutex<Option<(std::time::SystemTime, Vec<TokenRecord>)>>,
}

impl TokenStore {
    /// Store at `<config_dir>/agent-tokens.json`.
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join("agent-tokens.json"),
            cache: Mutex::new(None),
        }
    }

    /// The token file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads all records (missing file → empty).
    pub fn load(&self) -> Result<Vec<TokenRecord>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("could not parse {}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => {
                Err(error).with_context(|| format!("could not read {}", self.path.display()))
            }
        }
    }

    /// Writes all records back (0600).
    fn save(&self, records: &[TokenRecord]) -> Result<()> {
        let json = serde_json::to_vec_pretty(records)?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        // Created private from the first byte — no umask window.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .with_context(|| format!("could not write {}", self.path.display()))?;
        std::io::Write::write_all(&mut file, &json)
            .with_context(|| format!("could not write {}", self.path.display()))?;
        // `mode` only applies on create; repair a pre-existing file too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        *self.cache.lock().expect("token cache") = None; // force a reload
        Ok(())
    }

    /// Looks a presented token up by hash, honoring expiry; returns its
    /// `(scopes, account binding, label)`. Reloads the file when its
    /// mtime moved, so `create`/`revoke` from the CLI take effect without
    /// a restart.
    pub fn lookup(&self, token: &str) -> Option<(Vec<String>, Option<String>, String)> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        let mut cache = self.cache.lock().expect("token cache");
        let stale = match (&*cache, mtime) {
            (Some((cached_mtime, _)), Some(now)) => *cached_mtime != now,
            (Some(_), None) => true, // file vanished
            (None, _) => true,
        };
        if stale {
            let records = self.load().unwrap_or_default();
            *cache = mtime.map(|mtime| (mtime, records));
        }
        let records = cache.as_ref().map(|(_, records)| records)?;

        let hash = hash_token(token);
        let now = time::OffsetDateTime::now_utc();
        records
            .iter()
            .find(|record| record.hash == hash && !record.is_expired(now))
            .map(|record| {
                (
                    record.scopes.clone(),
                    record.account.clone(),
                    record.label.clone(),
                )
            })
    }

    /// Creates a token: generates it, stores its hash + metadata, returns
    /// the **plaintext** (shown once). Fails on a duplicate label or an
    /// unknown scope.
    pub fn create(
        &self,
        label: &str,
        scopes: Vec<String>,
        account: Option<String>,
        ttl: Option<std::time::Duration>,
    ) -> Result<String> {
        for scope in &scopes {
            if !crate::plugins::VALID_SCOPES.contains(&scope.as_str()) {
                bail!(
                    "unknown scope {scope:?} (valid: {})",
                    crate::plugins::VALID_SCOPES.join(", ")
                );
            }
        }
        let mut records = self.load()?;
        if records.iter().any(|record| record.label == label) {
            bail!("a token named {label:?} already exists (revoke it first)");
        }
        let token = hex::encode(rand::random::<[u8; 32]>());
        let now = time::OffsetDateTime::now_utc();
        let expires = ttl.map(|ttl| iso(now + ttl));
        records.push(TokenRecord {
            label: label.to_owned(),
            hash: hash_token(&token),
            scopes,
            account,
            expires,
            created: iso(now),
        });
        self.save(&records)?;
        Ok(token)
    }

    /// Removes a token by label; returns whether one was removed.
    pub fn revoke(&self, label: &str) -> Result<bool> {
        let mut records = self.load()?;
        let before = records.len();
        records.retain(|record| record.label != label);
        let removed = records.len() != before;
        if removed {
            self.save(&records)?;
        }
        Ok(removed)
    }
}

/// SHA-256 of a token, hex — the only form persisted.
fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// `2026-07-06T12:34:56Z` (second precision; one home: [`crate::timefmt`]).
fn iso(time: time::OffsetDateTime) -> String {
    crate::timefmt::format_iso(time).expect("iso format is valid")
}

/// Parses an ISO-8601 UTC timestamp back (second precision).
fn parse_iso(text: &str) -> Option<time::OffsetDateTime> {
    crate::timefmt::parse_iso(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn create_lookup_revoke_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path());

        let token = store
            .create(
                "web",
                vec!["api".into(), "config".into()],
                Some("alice".into()),
                None,
            )
            .unwrap();
        // The plaintext is never on disk.
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains(&token));
        assert!(raw.contains(&hash_token(&token)));

        let (scopes, account, label) = store.lookup(&token).unwrap();
        assert_eq!(scopes, ["api", "config"]);
        assert_eq!(account.as_deref(), Some("alice"));
        assert_eq!(label, "web");
        assert!(store.lookup("wrong").is_none());

        assert!(store.revoke("web").unwrap());
        assert!(store.lookup(&token).is_none());
        assert!(!store.revoke("web").unwrap());
    }

    #[test]
    fn rejects_bad_scope_and_duplicate_label() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path());
        assert!(store.create("x", vec!["root".into()], None, None).is_err());
        store.create("dup", vec!["api".into()], None, None).unwrap();
        assert!(store.create("dup", vec!["api".into()], None, None).is_err());
    }

    #[test]
    fn expired_tokens_do_not_authenticate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path());
        // A token that expired in the past (negative TTL via a manual
        // record edit).
        let token = store
            .create(
                "temp",
                vec!["api".into()],
                None,
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        assert!(store.lookup(&token).is_some());

        // Rewrite its expiry into the past and force a cache reload.
        let mut records = store.load().unwrap();
        records[0].expires = Some("2000-01-01T00:00:00Z".to_owned());
        store.save(&records).unwrap();
        assert!(store.lookup(&token).is_none());
    }

    #[test]
    fn unparseable_expiry_counts_as_expired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path());
        let token = store
            .create("garbled", vec!["api".into()], None, None)
            .unwrap();
        assert!(store.lookup(&token).is_some());

        // A corrupt expiry must fail closed, not grant an eternal token.
        let mut records = store.load().unwrap();
        records[0].expires = Some("not-a-timestamp".to_owned());
        store.save(&records).unwrap();
        assert!(store.lookup(&token).is_none());
    }
}
