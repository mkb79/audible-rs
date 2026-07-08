//! The per-item change log (AUD-64/65/66): change classification
//! (order-insensitive, volatile-aware), recording, listing and pruning.

use super::{Db, DbError};

/// Whether/how a page's added/changed/removed items are written to the
/// `change_log` (AUD-64). `record` is `record_changes && !initial-sync`.
#[derive(Debug, Clone, Copy)]
pub struct ChangeRecording {
    /// Write `change_log` rows for this page.
    pub record: bool,
    /// `"full"` or `"delta"`, tagged on each row.
    pub mode: &'static str,
}

/// One `change_log` entry, for `library changes`.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    /// When the change was recorded (a sync's apply time).
    pub recorded_utc: String,
    /// Marketplace the item belongs to.
    pub marketplace: String,
    /// ASIN of the item.
    pub asin: String,
    /// Title (incl. subtitle) for display.
    pub full_title: String,
    /// `"full"` or `"delta"`.
    pub mode: String,
    /// `"added"`, `"changed"` or `"removed"`.
    pub kind: String,
    /// Field diff `[{key, old, new}]` (kind `changed` only).
    pub changed: Option<String>,
}

/// Filters for [`Db::list_changes`]. Empty fields match everything.
#[derive(Debug, Default, Clone)]
pub struct ChangeFilter {
    /// Restrict to these marketplaces (empty = all).
    pub marketplaces: Vec<String>,
    /// Restrict to one ASIN.
    pub asin: Option<String>,
    /// Only changes recorded at/after this ISO timestamp.
    pub since: Option<String>,
    /// Restrict to a mode (`full`/`delta`).
    pub mode: Option<String>,
    /// Restrict to a kind (`added`/`changed`/`removed`).
    pub kind: Option<String>,
    /// Include volatile-only `changed` entries (default `false` hides them).
    pub show_volatile: bool,
    /// Max rows (most recent first); 0 = no limit.
    pub limit: u32,
}

/// Top-level document keys treated as *volatile*: they shift without a real
/// library change — the user's own playback state, plus the community `rating`
/// (its aggregate average drifts on essentially every full sync of a popular
/// title). Verified live: these do not even move the delta state token, so they
/// only ever surface on a full sync.
///
/// Volatile changes are still **recorded** in the `change_log` (so a history is
/// available), but a change confined to these keys is not counted as a
/// significant change and is hidden from `library sync` / `library changes` by
/// default (revealed with `--show-volatile`). Kept here in code, intentionally
/// easy to extend; a slice so adding a key is a one-line change.
const VOLATILE_KEYS: &[&str] = &[
    "percent_complete",
    "listening_status",
    "is_finished",
    "is_downloaded",
    "rating",
];

/// Whether a top-level document key is [volatile](VOLATILE_KEYS).
fn is_volatile(key: &str) -> bool {
    VOLATILE_KEYS.contains(&key)
}

/// A canonical, order-insensitive string form of a JSON value: arrays are
/// sorted by their canonical elements and object entries by key, so two values
/// that differ only in ordering canonicalize identically. Audible returns some
/// arrays (`relationships`, `series`, …) in a non-stable order, which would
/// otherwise look like a change on every full sync.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(items) => {
            let mut parts: Vec<String> = items.iter().map(canonical).collect();
            parts.sort();
            format!("[{}]", parts.join(","))
        }
        serde_json::Value::Object(map) => {
            let mut parts: Vec<String> = map
                .iter()
                .map(|(key, val)| format!("{key:?}:{}", canonical(val)))
                .collect();
            parts.sort();
            format!("{{{}}}", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// Order-insensitive equality of two optional JSON values (see [`canonical`]).
/// Only worth calling once a cheap `!=` has already flagged a difference.
fn json_eq_unordered(a: Option<&serde_json::Value>, b: Option<&serde_json::Value>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => canonical(a) == canonical(b),
        (None, None) => true,
        _ => false,
    }
}

/// How an upserted document relates to the one already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeClass {
    /// Identical, or differing only in array/key ordering — not a change.
    Unchanged,
    /// At least one *non-volatile* top-level key differs.
    Significant,
    /// Only [volatile](VOLATILE_KEYS) top-level keys differ.
    VolatileOnly,
}

/// Classifies an upsert against the stored document and, when there is a real
/// change, returns the **full** top-level value diff as a JSON array
/// `[{"key","old","new"}]` (keys sorted, **including** volatile keys — the
/// `change_log` keeps the complete history). Array/key reordering is not a
/// change. Unparseable / non-object input is treated as a significant change
/// with no recordable diff (safe side).
pub(crate) fn classify_change(old: &str, new: &str) -> (ChangeClass, Option<String>) {
    let (Ok(old), Ok(new)) = (
        serde_json::from_str::<serde_json::Value>(old),
        serde_json::from_str::<serde_json::Value>(new),
    ) else {
        return (ChangeClass::Significant, None);
    };
    let (Some(old), Some(new)) = (old.as_object(), new.as_object()) else {
        return if old == new {
            (ChangeClass::Unchanged, None)
        } else {
            (ChangeClass::Significant, None)
        };
    };
    let mut keys: Vec<&String> = old
        .keys()
        .chain(new.keys())
        .filter(|key| {
            let (o, n) = (old.get(*key), new.get(*key));
            o != n && !json_eq_unordered(o, n)
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.is_empty() {
        return (ChangeClass::Unchanged, None);
    }
    let class = if keys.iter().any(|key| !is_volatile(key)) {
        ChangeClass::Significant
    } else {
        ChangeClass::VolatileOnly
    };
    let diff: Vec<serde_json::Value> = keys
        .iter()
        .map(|key| {
            serde_json::json!({
                "key": key,
                "old": old.get(*key).cloned().unwrap_or(serde_json::Value::Null),
                "new": new.get(*key).cloned().unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    (class, serde_json::to_string(&diff).ok())
}

impl Db {
    /// Prunes `change_log` rows older than `retention_days` (by `recorded_utc`),
    /// returning the number deleted. `retention_days == 0` keeps everything.
    pub async fn prune_change_log(&self, retention_days: u32) -> Result<usize, DbError> {
        self.call(move |conn| {
            if retention_days == 0 {
                return Ok(0);
            }
            let format =
                time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
            let cutoff = (time::OffsetDateTime::now_utc()
                - time::Duration::days(i64::from(retention_days)))
            .format(format)
            .expect("formatting a UTC timestamp with a const format never fails");
            let deleted = conn.execute(
                "DELETE FROM change_log WHERE recorded_utc < ?",
                rusqlite::params![cutoff],
            )?;
            Ok(deleted)
        })
        .await
    }

    /// Lists recorded changes (most recent first) for `library changes`.
    pub async fn list_changes(&self, filter: ChangeFilter) -> Result<Vec<ChangeRecord>, DbError> {
        self.call(move |conn| {
            let mut sql = String::from(
                "SELECT recorded_utc, marketplace, asin, full_title, mode, kind, changed \
                 FROM change_log WHERE 1 = 1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if !filter.marketplaces.is_empty() {
                let placeholders = vec!["?"; filter.marketplaces.len()].join(", ");
                sql.push_str(&format!(" AND marketplace IN ({placeholders})"));
                for marketplace in &filter.marketplaces {
                    params.push(Box::new(marketplace.clone()));
                }
            }
            if let Some(asin) = &filter.asin {
                sql.push_str(" AND asin = ?");
                params.push(Box::new(asin.clone()));
            }
            if let Some(since) = &filter.since {
                sql.push_str(" AND recorded_utc >= ?");
                params.push(Box::new(since.clone()));
            }
            if let Some(mode) = &filter.mode {
                sql.push_str(" AND mode = ?");
                params.push(Box::new(mode.clone()));
            }
            if let Some(kind) = &filter.kind {
                sql.push_str(" AND kind = ?");
                params.push(Box::new(kind.clone()));
            }
            if !filter.show_volatile {
                // Hide volatile-only changes: a 'changed' row whose diff has no
                // non-volatile key. 'added'/'removed' (no diff) and the rare
                // unparseable 'changed' (NULL diff) stay visible. Done in SQL so
                // LIMIT counts the visible rows.
                let placeholders = vec!["?"; VOLATILE_KEYS.len()].join(", ");
                sql.push_str(&format!(
                    " AND (kind != 'changed' OR changed IS NULL \
                       OR EXISTS (SELECT 1 FROM json_each(change_log.changed) \
                                  WHERE json_extract(value, '$.key') NOT IN ({placeholders})))"
                ));
                for key in VOLATILE_KEYS {
                    params.push(Box::new((*key).to_string()));
                }
            }
            sql.push_str(" ORDER BY recorded_utc DESC, id DESC");
            if filter.limit > 0 {
                sql.push_str(&format!(" LIMIT {}", filter.limit));
            }

            let mut statement = conn.prepare(&sql)?;
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
            let rows = statement
                .query_map(refs.as_slice(), |row| {
                    Ok(ChangeRecord {
                        recorded_utc: row.get(0)?,
                        marketplace: row.get(1)?,
                        asin: row.get(2)?,
                        full_title: row.get(3)?,
                        mode: row.get(4)?,
                        kind: row.get(5)?,
                        changed: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::{MP, default_log, open_temp, upsert};
    #[allow(unused_imports)]
    use crate::db::*;

    #[test]
    fn classify_change_full_diff_keeps_volatile() {
        // A significant change (`language`) alongside a volatile one
        // (`percent_complete`): class is Significant, but the recorded diff
        // keeps *all* differing keys, volatile included (full history).
        let old = r#"{"a":1,"language":"de","percent_complete":10,"gone":true}"#;
        let new = r#"{"a":2,"language":"en","percent_complete":90,"added":"x"}"#;
        let (class, diff) = classify_change(old, new);
        assert_eq!(class, ChangeClass::Significant);
        let diff = diff.expect("documents differ");
        for key in ["\"a\"", "language", "percent_complete", "gone", "added"] {
            assert!(diff.contains(key), "missing {key} in {diff}");
        }
        assert!(diff.contains("\"de\"") && diff.contains("\"en\""), "{diff}");
    }

    #[test]
    fn classify_change_volatile_only_is_recorded() {
        // Differing only in volatile keys (incl. the new `rating`): not
        // significant, but still a real change with a recordable diff.
        let (class, diff) = classify_change(
            r#"{"a":1,"percent_complete":1,"rating":{"average":4.81}}"#,
            r#"{"a":1,"percent_complete":9,"rating":{"average":4.82}}"#,
        );
        assert_eq!(class, ChangeClass::VolatileOnly);
        let diff = diff.expect("documents differ");
        assert!(
            diff.contains("percent_complete") && diff.contains("rating"),
            "{diff}"
        );
        assert!(
            !diff.contains("\"a\""),
            "unchanged key must not appear: {diff}"
        );

        // Identical documents are unchanged, no diff.
        assert_eq!(
            classify_change(r#"{"a":1}"#, r#"{"a":1}"#).0,
            ChangeClass::Unchanged
        );
    }

    #[test]
    fn change_detection_ignores_array_reordering() {
        // Same elements in a different order (Audible reorders relationships/
        // series) is NOT a change.
        let a = r#"{"relationships":[{"asin":"X"},{"asin":"Y"}],"series":[1,2,3]}"#;
        let b = r#"{"relationships":[{"asin":"Y"},{"asin":"X"}],"series":[3,1,2]}"#;
        let (class, diff) = classify_change(a, b);
        assert_eq!(
            class,
            ChangeClass::Unchanged,
            "a pure reorder is not a change"
        );
        assert!(diff.is_none());

        // A genuine element change IS detected; the reordered field is not.
        let c = r#"{"relationships":[{"asin":"X"},{"asin":"Z"}],"series":[3,2,1]}"#;
        let (class, diff) = classify_change(a, c);
        assert_eq!(class, ChangeClass::Significant);
        let diff = diff.expect("documents differ");
        assert!(diff.contains("relationships"), "{diff}");
        assert!(
            !diff.contains("series"),
            "reordered series must not appear: {diff}"
        );
    }

    #[tokio::test]
    async fn change_log_records_added_changed_removed() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        // Baseline (no recording): A1 present.
        db.apply_page(
            MP.into(),
            vec![upsert(
                "A1",
                serde_json::json!({"asin":"A1","price":1,"percent_complete":5}),
            )],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // Delta with recording: A1 changes (price + a new key; percent_complete is
        // volatile), A2 is added.
        db.apply_page_recording(
            MP.into(),
            vec![
                upsert(
                    "A1",
                    serde_json::json!({"asin":"A1","price":2,"percent_complete":80,"extra":"x"}),
                ),
                upsert("A2", serde_json::json!({"asin":"A2","price":9})),
            ],
            vec![],
            default_log(),
            None,
            ChangeRecording {
                record: true,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        // Remove A2 (recording on).
        db.apply_page_recording(
            MP.into(),
            vec![],
            vec!["A2".into()],
            default_log(),
            None,
            ChangeRecording {
                record: true,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        let changes = db
            .list_changes(ChangeFilter {
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(changes.len(), 3, "{changes:?}");
        let a1 = changes.iter().find(|c| c.asin == "A1").unwrap();
        assert_eq!(a1.kind, "changed");
        assert_eq!(a1.mode, "delta");
        let diff = a1.changed.as_deref().unwrap();
        assert!(diff.contains("price") && diff.contains("extra"), "{diff}");
        // The full diff now keeps the volatile key too (complete history).
        assert!(diff.contains("percent_complete"), "{diff}");
        let a2_added = changes
            .iter()
            .find(|c| c.asin == "A2" && c.kind == "added")
            .unwrap();
        assert!(a2_added.changed.is_none());
        assert!(
            changes
                .iter()
                .any(|c| c.asin == "A2" && c.kind == "removed")
        );
    }

    #[tokio::test]
    async fn change_log_records_volatile_only_hidden_by_default() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        // Baseline (no recording).
        db.apply_page(
            MP.into(),
            vec![upsert(
                "A1",
                serde_json::json!({"asin":"A1","percent_complete":5}),
            )],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // Only a volatile key (percent_complete) changes.
        let outcome = db
            .apply_page_recording(
                MP.into(),
                vec![upsert(
                    "A1",
                    serde_json::json!({"asin":"A1","percent_complete":90}),
                )],
                vec![],
                default_log(),
                None,
                ChangeRecording {
                    record: true,
                    mode: "full",
                },
            )
            .await
            .unwrap();
        // Not counted as a significant change, but tracked as volatile-only.
        assert!(outcome.changed.is_empty(), "{outcome:?}");
        assert_eq!(outcome.changed_volatile.len(), 1, "{outcome:?}");

        // Recorded in the change_log, but hidden from the default view.
        let hidden = db
            .list_changes(ChangeFilter {
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            hidden.is_empty(),
            "volatile-only is hidden by default: {hidden:?}"
        );

        // Revealed with show_volatile, full diff present.
        let shown = db
            .list_changes(ChangeFilter {
                limit: 0,
                show_volatile: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].kind, "changed");
        assert!(
            shown[0]
                .changed
                .as_deref()
                .unwrap()
                .contains("percent_complete"),
            "{shown:?}"
        );
    }

    #[tokio::test]
    async fn change_log_skipped_when_not_recording() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page_recording(
            MP.into(),
            vec![upsert("A1", serde_json::json!({"asin":"A1"}))],
            vec![],
            default_log(),
            None,
            ChangeRecording {
                record: false,
                mode: "full",
            },
        )
        .await
        .unwrap();
        let changes = db
            .list_changes(ChangeFilter {
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            changes.is_empty(),
            "initial/non-recording sync must log nothing"
        );
    }

    #[tokio::test]
    async fn prune_change_log_removes_old_entries() {
        let (_dir, db) = open_temp().await;
        db.call(|conn| {
            conn.execute(
                "INSERT INTO change_log(recorded_utc, marketplace, asin, full_title, mode, kind)
                 VALUES ('2000-01-01T00:00:00Z','de','OLD','old','delta','added')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        db.call(|conn| {
            conn.execute(
                "INSERT INTO change_log(recorded_utc, marketplace, asin, full_title, mode, kind)
                 VALUES (?,'de','NEW','new','delta','added')",
                rusqlite::params![now_iso_utc()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(db.prune_change_log(365).await.unwrap(), 1);
        let left = db
            .list_changes(ChangeFilter {
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].asin, "NEW");
        // retention 0 keeps everything.
        assert_eq!(db.prune_change_log(0).await.unwrap(), 0);
    }
}
