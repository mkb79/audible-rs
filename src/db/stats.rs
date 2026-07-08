//! Maintenance and introspection: stats, vacuum, integrity check, backup.

use super::{Db, DbError};

/// Database status, for `db info`.
#[derive(Debug, Clone)]
pub struct DbStats {
    /// Schema version (`PRAGMA user_version`).
    pub schema_version: i64,
    /// Active (not soft-deleted) items.
    pub items_active: u64,
    /// Soft-deleted items.
    pub items_deleted: u64,
    /// Active podcast episodes.
    pub episodes_active: u64,
    /// Distinct series with at least one membership.
    pub series: u64,
    /// Tracked downloads.
    pub downloads: u64,
    /// Stored license grants.
    pub licenses: u64,
    /// Last successful sync timestamp, if any.
    pub last_sync_utc: Option<String>,
}

impl Db {
    /// Writes a consistent, compacted single-file snapshot to `dest`
    /// (`VACUUM INTO`). The connection sees the live state including the
    /// WAL, so no separate checkpoint is needed. `dest` must not exist.
    pub async fn backup_into(&self, dest: String) -> Result<(), DbError> {
        self.call(move |conn| {
            conn.execute("VACUUM INTO ?1", rusqlite::params![dest])?;
            Ok(())
        })
        .await
    }

    /// Compacts the database: checkpoints and truncates the WAL, then
    /// `VACUUM` rebuilds the main file (`db vacuum`).
    pub async fn vacuum(&self) -> Result<(), DbError> {
        self.call(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
            Ok(())
        })
        .await
    }

    /// Runs `PRAGMA integrity_check`. Returns `["ok"]` when healthy, else
    /// the reported problems (`db check`).
    pub async fn integrity_check(&self) -> Result<Vec<String>, DbError> {
        self.call(|conn| {
            let mut statement = conn.prepare("PRAGMA integrity_check")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Collects database status in one round-trip (`db info`).
    pub async fn stats(&self) -> Result<DbStats, DbError> {
        self.call(|conn| {
            let count = |sql: &str| -> rusqlite::Result<u64> {
                conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                    .map(|n| n as u64)
            };
            Ok(DbStats {
                schema_version: conn.pragma_query_value(None, "user_version", |row| row.get(0))?,
                items_active: count("SELECT COUNT(*) FROM items WHERE is_deleted = 0")?,
                items_deleted: count("SELECT COUNT(*) FROM items WHERE is_deleted = 1")?,
                episodes_active: count("SELECT COUNT(*) FROM episodes WHERE is_deleted = 0")?,
                series: count("SELECT COUNT(DISTINCT series_asin) FROM item_series")?,
                downloads: count("SELECT COUNT(*) FROM downloads")?,
                licenses: count("SELECT COUNT(*) FROM licenses")?,
                last_sync_utc: conn.query_row(
                    "SELECT MAX(response_time_utc) FROM sync_log WHERE http_status = 200",
                    [],
                    |row| row.get(0),
                )?,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {

    use crate::db::test_util::{MP, default_log, item, open_temp};
    #[allow(unused_imports)]
    use crate::db::*;

    #[tokio::test]
    async fn stats_reports_counts_and_schema() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("A1", "x"), item("A2", "y")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.record_download(
            MP.into(),
            DownloadRecord {
                asin: "A1".into(),
                kind: "audio".into(),
                acr: None,
                content_format: "AAX".into(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: "/x".into(),
                file_size: Some(10),
            },
        )
        .await
        .unwrap();

        let stats = db.stats().await.unwrap();
        assert_eq!(stats.schema_version, 1);
        assert_eq!(stats.items_active, 2);
        assert_eq!(stats.items_deleted, 0);
        assert_eq!(stats.downloads, 1);
        assert_eq!(stats.licenses, 0);
        assert!(stats.last_sync_utc.is_some());
    }

    #[tokio::test]
    async fn vacuum_and_integrity_check() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.vacuum().await.unwrap();
        assert_eq!(db.integrity_check().await.unwrap(), vec!["ok".to_owned()]);
    }

    #[tokio::test]
    async fn backup_into_produces_a_usable_copy() {
        let (dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("A1", "x")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        let dest = dir.path().join("backup.sqlite");
        db.backup_into(dest.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert!(dest.exists());

        // The snapshot opens and carries the data.
        let restored = Db::open(dest, 5000).await.unwrap();
        assert_eq!(restored.count_active(vec![MP.to_owned()]).await.unwrap(), 1);
    }
}
