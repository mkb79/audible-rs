//! SQLite layer (archived architecture reference §8): `rusqlite` (bundled,
//! FTS5) behind a dedicated writer thread, WAL, migrations via
//! `user_version`. The DB is the engine of the `library` commands.
//!
//! One [`Db`] handle owns one database file. All operations run as
//! closures on the dedicated connection thread (rusqlite connections
//! are not `Sync`); async callers await the result through a oneshot
//! channel, so nothing blocks the executor.
//!
//! The domain methods live in submodules — one `impl Db` block each —
//! re-exported here so callers keep using `db::` paths: [`items`]
//! (sync/list/search/export), [`episodes`], [`series`], [`downloads`]
//! (+ licenses), [`annotations`], [`changes`] (the change log) and
//! [`stats`] (maintenance).

mod annotations;
mod changes;
mod downloads;
mod episodes;
mod items;
mod series;
mod stats;
#[cfg(test)]
mod test_util;

pub use annotations::{AnnotationDoc, AnnotationStatus};
pub use changes::{ChangeFilter, ChangeRecord, ChangeRecording};
pub use downloads::{
    DOWNLOAD_KINDS, DOWNLOAD_VARIANTS, DownloadEntry, DownloadRecord, LicenseGrant, ReorgDownload,
};
pub use episodes::{EpisodeHit, EpisodeRow, MissingEpisodeRow, PodcastRow, UpsertEpisode};
pub use items::{
    ApplyOutcome, BookRow, BorrowedRow, ChangedItem, ExportBookRow, ItemRemoval,
    MissingDownloadsRow, SeriesRef, Settings, SyncLogEntry, UpsertItem, prepare_fts_query,
    state_token_iso,
};
pub use series::{SeriesItemRow, SeriesRow};
pub use stats::DbStats;

pub mod schema;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Errors raised by the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem access around the database failed.
    #[error("database IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// The connection thread is gone.
    #[error("database connection thread terminated")]
    Closed,
    /// The database was initialized with different response groups.
    #[error(
        "this database was created with response_groups {existing:?} but \
         {requested:?} was requested — keep the value stable or start a \
         new database"
    )]
    ResponseGroupsMismatch {
        /// Groups stored in the database.
        existing: String,
        /// Groups requested now.
        requested: String,
    },
}

/// File name for an account's database: `account_{sha256(user_id)[..16]}.sqlite`
/// (one file per account; the marketplace is stored as a column).
pub fn account_file_name(user_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(user_id.as_bytes());
    format!("account_{}.sqlite", hex::encode(&digest[..8]))
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Handle to one database file; cheap to clone.
#[derive(Clone)]
pub struct Db {
    jobs: std::sync::mpsc::Sender<Job>,
    path: PathBuf,
}

impl Db {
    /// Opens (and migrates) the database, creating file and parent
    /// directory on demand.
    pub async fn open(path: PathBuf, busy_timeout_ms: u64) -> Result<Self, DbError> {
        let (jobs, job_receiver) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let thread_path = path.clone();
        std::thread::Builder::new()
            .name("audible-db".into())
            .spawn(move || {
                let opened = open_connection(&thread_path, busy_timeout_ms);
                let mut conn = match opened {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = job_receiver.recv() {
                    job(&mut conn);
                }
            })?;

        ready_rx.await.map_err(|_| DbError::Closed)??;
        Ok(Self { jobs, path })
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs a closure on the connection thread and awaits its result.
    pub async fn call<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, DbError> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .map_err(|_| DbError::Closed)?;
        rx.await.map_err(|_| DbError::Closed)?
    }
}

fn open_connection(path: &Path, busy_timeout_ms: u64) -> Result<Connection, DbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))?;
    schema::migrate(&conn)?;
    Ok(conn)
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ` (the reference format).
pub fn now_iso_utc() -> String {
    let format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    time::OffsetDateTime::now_utc()
        .format(format)
        .expect("formatting a UTC timestamp with a const format never fails")
}

// ------------------------- typed operations -------------------------

/// Builds an `IN (?,?,…)` placeholder list for `n` bound values.
fn in_placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// SQL fragment excluding archived items from a `v_books b` query
/// (AUD-110) — empty when archived items are wanted. The correlated
/// lookup keeps the `v_books` view untouched (no schema change, no DB
/// reset); `is_archived` may be true/false/absent in the doc.
fn not_archived_clause(include_archived: bool) -> &'static str {
    if include_archived {
        ""
    } else {
        "AND COALESCE((SELECT json_extract(i.doc, '$.is_archived')
                       FROM items i
                       WHERE i.asin = b.asin AND i.marketplace = b.marketplace), 0) = 0"
    }
}

/// The archive filter for a query over `v_episodes` aliased `e`: an episode
/// has no archive flag of its own, so it inherits its parent show's
/// (AUD-110/AUD-203). Twin of [`not_archived_clause`], which assumes the `b`
/// alias over `v_books`.
fn episode_not_archived_clause(include_archived: bool) -> &'static str {
    if include_archived {
        ""
    } else {
        "AND COALESCE((SELECT json_extract(i.doc, '$.is_archived')
                       FROM items i
                       WHERE i.asin = e.parent_asin AND i.marketplace = e.marketplace), 0) = 0"
    }
}

/// Whether a document advertises a companion PDF — the SQL twin of
/// `commands::download::artifacts::library_pdf_flag`: `is_pdf_url_available`,
/// with a `pdf_url`-presence fallback. `doc_expr` must name a document column
/// in scope (e.g. `e.doc`).
///
/// Verified against a real library (AUD-206): the flag and an actual `pdf_url`
/// agree 100% (29 titles with both, 139 with neither), and **every** podcast
/// episode reports `false` — so this is what tells a title that can have a PDF
/// from one that cannot.
fn pdf_available_sql(doc_expr: &str) -> String {
    format!(
        "(json_extract({doc_expr}, '$.is_pdf_url_available') = 1
          OR json_extract({doc_expr}, '$.pdf_url') IS NOT NULL)"
    )
}

/// As [`pdf_available_sql`], for a row that carries no `doc` column of its own
/// (`v_books`/`v_episodes`): looks the document up in `table` by the row's
/// `(asin, marketplace)`.
fn pdf_available_lookup_sql(table: &str, asin_expr: &str, marketplace_expr: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM {table} pdfdoc
                 WHERE pdfdoc.asin = {asin_expr}
                   AND pdfdoc.marketplace = {marketplace_expr}
                   AND {})",
        pdf_available_sql("pdfdoc.doc")
    )
}

/// Gates the requested kind `k.value` on the artifact being able to exist at
/// all (AUD-206). Only `pdf` is gated: it is the one kind the library document
/// advertises. There is deliberately **no** chapter gate — no document field
/// says whether a title has chapters (they come from a live
/// `content/{asin}/metadata` request), so the DB cannot know; and a cover is
/// universal.
///
/// `pdf_available` is the row-appropriate [`pdf_available_sql`] /
/// [`pdf_available_lookup_sql`] expression.
fn kind_possible_sql(pdf_available: &str) -> String {
    format!("(k.value != 'pdf' OR {pdf_available})")
}

/// Whether the requested download kind `k.value` is still missing for the
/// `v_books` row aliased `b` — for queries that join `json_each(?) k` over the
/// requested kinds (AUD-203).
///
/// A podcast **parent** owns no download record: the record always sits on the
/// episode. Testing the parent's own ASIN would therefore report it missing
/// forever, even with every episode downloaded. So a show counts as missing a
/// kind while **any** of its episodes still lacks it; a show whose episodes are
/// all downloaded (or that has none) is complete. Books and individually-
/// subscribed episodes keep testing their own ASIN.
///
/// A kind that cannot exist for the row is never missing (AUD-206) — which is
/// also what stops a show whose episodes have no PDF from reporting one.
fn missing_kind_predicate() -> String {
    format!(
        "CASE WHEN b.kind = 'podcast' THEN
         EXISTS (
             SELECT 1 FROM episodes e
             WHERE e.parent_asin = b.asin AND e.marketplace = b.marketplace
               AND e.is_deleted = 0
               AND NOT EXISTS (
                   SELECT 1 FROM downloads d
                   WHERE d.asin = e.asin AND d.marketplace = e.marketplace
                     AND d.kind = k.value
               )
               AND {}
         )
     ELSE
         NOT EXISTS (
             SELECT 1 FROM downloads d
             WHERE d.asin = b.asin AND d.marketplace = b.marketplace
               AND d.kind = k.value
         )
         AND {}
     END",
        // The roll-up reads the episode rows directly, so the doc is in scope.
        kind_possible_sql(&pdf_available_sql("e.doc")),
        kind_possible_sql(&pdf_available_lookup_sql(
            "items",
            "b.asin",
            "b.marketplace"
        )),
    )
}

/// The content-kind expression over an item document column — the SQL
/// twin of [`crate::models::library::item_kind`] (AUD-173), for queries
/// selecting from `items` directly. `v_books` embeds the same CASE as its
/// `kind` column; a functional test keeps all copies in lockstep.
fn item_kind_sql(doc_column: &str) -> String {
    format!(
        "CASE \
           WHEN json_extract({doc_column}, '$.content_delivery_type') = 'PodcastEpisode' \
             THEN 'episode' \
           WHEN json_extract({doc_column}, '$.content_delivery_type') \
                  IN ('PodcastParent', 'Periodical', 'PodcastSeason') \
                OR json_extract({doc_column}, '$.content_type') = 'Podcast' \
             THEN 'podcast' \
           ELSE 'book' \
         END"
    )
}

/// SQL fragment for a `--kind` content filter over a kind column or
/// expression; empty `kinds` = empty fragment (all kinds). The values come
/// from clap's fixed `book|podcast|episode` set, so they are embedded as
/// literals (asserted, defensively).
fn kind_clause(kind_expr: &str, kinds: &[String]) -> String {
    if kinds.is_empty() {
        return String::new();
    }
    debug_assert!(
        kinds
            .iter()
            .all(|kind| crate::models::library::ITEM_KINDS.contains(&kind.as_str())),
        "kind filter values must come from the fixed clap set"
    );
    let quoted: Vec<String> = kinds.iter().map(|kind| format!("'{kind}'")).collect();
    format!(" AND {kind_expr} IN ({})", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::MP;

    #[tokio::test]
    async fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library_test.sqlite");
        {
            let db = Db::open(path.clone(), 5000).await.unwrap();
            db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        }
        let db = Db::open(path, 5000).await.unwrap();
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
    }

    #[test]
    fn account_file_name_is_stable_and_scoped() {
        let a = account_file_name("amzn1.account.X");
        let b = account_file_name("amzn1.account.Y");
        assert!(a.starts_with("account_") && a.ends_with(".sqlite"));
        assert_ne!(a, b);
        assert_eq!(a, account_file_name("amzn1.account.X"));
        // The marketplace no longer affects the file name.
        // (No second argument, one file per account.)
    }
}
