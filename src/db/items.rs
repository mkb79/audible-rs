//! Library items: sync-state, page application (upserts/soft-deletes,
//! sync log), listing, search (FTS5), export, counts and item removal.

use rusqlite::OptionalExtension;

use super::changes::{ChangeClass, ChangeRecording, classify_change};
use super::{Db, DbError, in_placeholders, not_archived_clause, now_iso_utc};

/// Sync state for a specific marketplace row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Response groups every request must use (fixed per database).
    pub response_groups: String,
    /// Raw state token of the last successful sync, if any.
    pub last_state_token: Option<String>,
}

/// One item to upsert during a sync.
#[derive(Debug, Clone)]
pub struct UpsertItem {
    /// Audible ASIN (primary key together with marketplace).
    pub asin: String,
    /// Full item document as JSON text.
    pub doc: String,
    /// Item title.
    pub title: String,
    /// Optional subtitle.
    pub subtitle: Option<String>,
    /// `Title: Subtitle` (or just the title).
    pub full_title: String,
    /// Series memberships extracted from the document.
    pub series: Vec<SeriesRef>,
}

/// One series membership of an item.
#[derive(Debug, Clone)]
pub struct SeriesRef {
    /// ASIN of the series.
    pub series_asin: String,
    /// Title of the series.
    pub series_title: String,
    /// Position within the series — may be empty or a range (`"1-6"`).
    pub sequence: Option<String>,
}

/// Audit-log entry for one sync request (page).
#[derive(Debug, Clone, Default)]
pub struct SyncLogEntry {
    /// When the request was sent.
    pub request_time_utc: String,
    /// State token sent with the request (delta sync), ISO form.
    pub request_state_token_utc: Option<String>,
    /// When the response arrived.
    pub response_time_utc: String,
    /// State token received, ISO form.
    pub response_state_token_utc: Option<String>,
    /// HTTP status of the response.
    pub http_status: Option<i64>,
    /// Free-form note (`sync-full-page-1`, …).
    pub note: Option<String>,
}

/// One item touched by a sync, for the change summary: ASIN + display title.
#[derive(Debug, Clone)]
pub struct ChangedItem {
    /// ASIN of the item.
    pub asin: String,
    /// Title (incl. subtitle) for display.
    pub full_title: String,
}

/// What applying a sync page changed, split by kind. `added` are items new to
/// the library (or returning from a soft-delete), `changed` existing items with
/// a significant (non-volatile) change, `changed_volatile` existing items whose
/// change is confined to [volatile](VOLATILE_KEYS) keys (recorded, but hidden
/// from the summary unless `--show-volatile`), `removed` soft-deletes.
#[derive(Debug, Default)]
pub struct ApplyOutcome {
    /// Newly added items.
    pub added: Vec<ChangedItem>,
    /// Existing items with a significant (non-volatile) change.
    pub changed: Vec<ChangedItem>,
    /// Existing items whose change is confined to volatile keys.
    pub changed_volatile: Vec<ChangedItem>,
    /// Items soft-deleted in this page.
    pub removed: Vec<ChangedItem>,
}

impl ApplyOutcome {
    /// Merges another page's outcome into this one.
    pub fn extend(&mut self, other: ApplyOutcome) {
        self.added.extend(other.added);
        self.changed.extend(other.changed);
        self.changed_volatile.extend(other.changed_volatile);
        self.removed.extend(other.removed);
    }
}

/// Result of [`Db::remove_items`] (`db library remove`).
#[derive(Debug, Clone, Default)]
pub struct ItemRemoval {
    /// Requested ASINs that existed and were removed.
    pub removed_asins: Vec<String>,
    /// Requested ASINs not present in `items`.
    pub missing_asins: Vec<String>,
    /// Episodes removed via the parent cascade.
    pub episodes_removed: usize,
    /// Download records removed (item and its episodes).
    pub downloads_removed: usize,
    /// License grants removed (item and its episodes).
    pub licenses_removed: usize,
    /// File paths of the removed download records (for `--with-files`).
    pub file_paths: Vec<String>,
}

/// A flat export row (`library export --format csv`).
#[derive(Debug, Clone)]
pub struct ExportBookRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// Audible ASIN.
    pub asin: String,
    /// Item title.
    pub title: String,
    /// Optional subtitle.
    pub subtitle: Option<String>,
    /// `Title: Subtitle`.
    pub full_title: String,
    /// Purchase/added date, if present.
    pub purchase_date: Option<String>,
    /// Runtime in minutes, if present.
    pub runtime_min: Option<String>,
    /// Language, if present.
    pub language: Option<String>,
    /// Content kind: `book`, `podcast` or `episode` (AUD-173).
    pub kind: String,
}

/// An active item lacking download records
/// (`library list --missing`).
#[derive(Debug, Clone)]
pub struct MissingDownloadsRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// Audible ASIN.
    pub asin: String,
    /// `Title: Subtitle`.
    pub full_title: String,
    /// Comma-separated kinds the item has no download record of.
    pub missing: String,
}

/// A row of `library list --borrowed` (AUD-153): a title the user did not
/// purchase (access via a subscription/grant plan).
#[derive(Debug, Clone)]
pub struct BorrowedRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// Audible ASIN.
    pub asin: String,
    /// `Title: Subtitle`.
    pub full_title: String,
    /// Comma-separated names of the plans the user is eligible for
    /// (e.g. `Audible-AYCL`, `US Minerva`, `Free Tier`) — the ones through
    /// which the title is currently playable. Empty when none.
    pub eligible: String,
    /// Comma-separated names of the other plans the title belongs to that the
    /// user is NOT eligible for (e.g. `SpecialBenefit`, or the membership plan
    /// once a subscription lapses). The generic `AccessViaMusic` route is
    /// omitted as noise. Empty when there is none.
    pub not_eligible: String,
}

/// A row of the `v_books` view.
#[derive(Debug, Clone)]
pub struct BookRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// Audible ASIN.
    pub asin: String,
    /// `Title: Subtitle`.
    pub full_title: String,
    /// Purchase/added date, if present in the document.
    pub purchase_date: Option<String>,
    /// Runtime in minutes, if present.
    pub runtime_min: Option<String>,
    /// Language, if present.
    pub language: Option<String>,
    /// Content kind: `book`, `podcast` or `episode` (AUD-173).
    pub kind: String,
}

fn book_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        marketplace: row.get(0)?,
        asin: row.get(1)?,
        full_title: row.get(2)?,
        purchase_date: row.get(3)?,
        runtime_min: row.get(4)?,
        language: row.get(5)?,
        kind: row.get(6)?,
    })
}

/// The `v_books` column expressions, selected straight from `items` so an
/// FTS join can map a matched `rowid` to its exact `(asin, marketplace)`
/// row without a cross product on the (non-unique) asin.
const ITEMS_BOOK_COLUMNS: &str = "i.marketplace, i.asin, i.full_title,
     CAST(COALESCE(json_extract(i.doc, '$.purchase_date'),
                   json_extract(i.doc, '$.library_status.date_added')) AS TEXT),
     CAST(COALESCE(json_extract(i.doc, '$.runtime_length_min'),
                   json_extract(i.doc, '$.duration_min')) AS TEXT),
     CAST(COALESCE(json_extract(i.doc, '$.language'),
                   json_extract(i.doc, '$.metadata.language')) AS TEXT)";

/// Converts an epoch state token (seconds or milliseconds) to ISO form,
/// like the reference's `epoch_ms_to_iso`.
pub fn state_token_iso(raw: &str) -> Option<String> {
    let value: i64 = raw.trim().parse().ok()?;
    let seconds = if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    };
    let timestamp = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    timestamp.format(format).ok()
}

/// Prepares a user query for an FTS5 `MATCH`. Plain words become
/// quoted prefix tokens joined by an implicit AND, so `jed` finds
/// "Jedi" and punctuation can no longer break the query syntax
/// (`c++` would otherwise be a syntax error). If the input already
/// uses FTS5 syntax (`"`, `*`, `(`, `)`, `:`, `^`, or a bare
/// `AND`/`OR`/`NOT`/`NEAR` token), it is passed through unchanged so
/// power users keep full control.
pub fn prepare_fts_query(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Passthrough: FTS5 special chars or explicit operators present.
    if trimmed.contains('"')
        || trimmed.contains('*')
        || trimmed.contains('(')
        || trimmed.contains(')')
        || trimmed.contains(':')
        || trimmed.contains('^')
    {
        return trimmed.to_owned();
    }
    for token in trimmed.split_whitespace() {
        if matches!(token, "AND" | "OR" | "NOT" | "NEAR") {
            return trimmed.to_owned();
        }
    }
    // Build `"<tok>"*` for each whitespace-separated token, doubling
    // any embedded `"` characters (defensive; real `"` hits passthrough
    // above, but escape anyway for correctness).
    let parts: Vec<String> = trimmed
        .split_whitespace()
        .map(|tok| format!("\"{}\"*", tok.replace('"', "\"\"")))
        .collect();
    parts.join(" ")
}

impl Db {
    /// Creates the `sync_state` row for `marketplace` on first use, or
    /// verifies that the stored `response_groups` match. One row per
    /// marketplace; groups are pinned per row.
    pub async fn ensure_sync_state(
        &self,
        marketplace: String,
        response_groups: String,
    ) -> Result<Settings, DbError> {
        self.call(move |conn| {
            let existing: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT response_groups, last_state_token_raw
                     FROM sync_state WHERE marketplace = ?",
                    rusqlite::params![marketplace],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;

            match existing {
                Some((stored, last_state_token)) => {
                    if stored != response_groups {
                        return Err(DbError::ResponseGroupsMismatch {
                            existing: stored,
                            requested: response_groups,
                        });
                    }
                    Ok(Settings {
                        response_groups: stored,
                        last_state_token,
                    })
                }
                None => {
                    conn.execute(
                        "INSERT INTO sync_state(marketplace, response_groups, created_utc)
                         VALUES (?, ?, ?)",
                        rusqlite::params![marketplace, response_groups, now_iso_utc()],
                    )?;
                    Ok(Settings {
                        response_groups,
                        last_state_token: None,
                    })
                }
            }
        })
        .await
    }

    /// Applies one sync page without recording the change log — the convenience
    /// form used by tests and non-sync callers.
    pub async fn apply_page(
        &self,
        marketplace: String,
        upserts: Vec<UpsertItem>,
        soft_deletes: Vec<String>,
        log: SyncLogEntry,
        state_token_raw: Option<String>,
    ) -> Result<ApplyOutcome, DbError> {
        self.apply_page_recording(
            marketplace,
            upserts,
            soft_deletes,
            log,
            state_token_raw,
            ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
    }

    /// Applies one sync page atomically: upserts, soft-deletes, the sync-log
    /// row, the change log (per `recording`) and (when present) the new state
    /// token. All rows are tagged with `marketplace`.
    pub async fn apply_page_recording(
        &self,
        marketplace: String,
        upserts: Vec<UpsertItem>,
        soft_deletes: Vec<String>,
        log: SyncLogEntry,
        state_token_raw: Option<String>,
        recording: ChangeRecording,
    ) -> Result<ApplyOutcome, DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            let tx = conn.transaction()?;

            let mut added: Vec<ChangedItem> = Vec::new();
            let mut changed: Vec<ChangedItem> = Vec::new();
            let mut changed_volatile: Vec<ChangedItem> = Vec::new();
            // change_log rows for this page, if recording:
            // (change, asin, full_title, item_kind, diff).
            let mut change_rows: Vec<(&'static str, String, String, &'static str, Option<String>)> =
                Vec::new();
            // Content kind of a raw item document, for the change log's
            // item_kind column (AUD-173).
            let kind_of_doc = |doc: &str| {
                crate::models::library::item_kind(
                    &serde_json::from_str(doc).unwrap_or(serde_json::Value::Null),
                )
            };
            {
                // Read the prior state to classify each upsert before it is
                // overwritten: absent / soft-deleted → added; a non-volatile key
                // differs → changed; only volatile keys differ → volatile-only
                // (recorded, hidden by default); identical / reordered →
                // unchanged (still upserted, not reported).
                let mut prior = tx.prepare_cached(
                    "SELECT doc, is_deleted FROM items WHERE asin = ? AND marketplace = ?",
                )?;
                let mut statement = tx.prepare_cached(
                    "INSERT INTO items(asin, marketplace, doc, title, subtitle, full_title, updated_utc, is_deleted, deleted_utc)
                     VALUES (?, ?, ?, ?, ?, ?, ?, 0, NULL)
                     ON CONFLICT(asin, marketplace) DO UPDATE SET
                       doc         = excluded.doc,
                       title       = excluded.title,
                       subtitle    = excluded.subtitle,
                       full_title  = excluded.full_title,
                       updated_utc = excluded.updated_utc,
                       is_deleted  = 0,
                       deleted_utc = NULL",
                )?;
                let mut clear_series = tx
                    .prepare_cached("DELETE FROM item_series WHERE item_asin = ? AND marketplace = ?")?;
                let mut insert_series = tx.prepare_cached(
                    "INSERT INTO item_series(item_asin, marketplace, series_asin, series_title, sequence)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT(item_asin, marketplace, series_asin) DO UPDATE SET
                       series_title = excluded.series_title,
                       sequence     = excluded.sequence",
                )?;
                for item in &upserts {
                    let prior_state: Option<(String, i64)> = prior
                        .query_row(rusqlite::params![item.asin, marketplace], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })
                        .optional()?;
                    statement.execute(rusqlite::params![
                        item.asin,
                        marketplace,
                        item.doc,
                        item.title,
                        item.subtitle,
                        item.full_title,
                        now,
                    ])?;
                    clear_series.execute(rusqlite::params![item.asin, marketplace])?;
                    for series in &item.series {
                        insert_series.execute(rusqlite::params![
                            item.asin,
                            marketplace,
                            series.series_asin,
                            series.series_title,
                            series.sequence,
                        ])?;
                    }
                    // Classify against the prior state read above. `changed`
                    // covers both a significant change and a volatile-only one;
                    // `significant` decides which display bucket it lands in,
                    // while the recorded diff is the full diff either way.
                    let classified: Option<(&'static str, bool, Option<String>)> = match &prior_state
                    {
                        Some((old_doc, 0)) => match classify_change(old_doc, &item.doc) {
                            (ChangeClass::Unchanged, _) => None,
                            (ChangeClass::Significant, diff) => Some(("changed", true, diff)),
                            (ChangeClass::VolatileOnly, diff) => Some(("changed", false, diff)),
                        },
                        _ => Some(("added", true, None)),
                    };
                    if let Some((kind, significant, diff)) = classified {
                        let entry = ChangedItem {
                            asin: item.asin.clone(),
                            full_title: item.full_title.clone(),
                        };
                        match (kind, significant) {
                            ("added", _) => added.push(entry),
                            (_, true) => changed.push(entry),
                            (_, false) => changed_volatile.push(entry),
                        }
                        if recording.record {
                            change_rows.push((
                                kind,
                                item.asin.clone(),
                                item.full_title.clone(),
                                kind_of_doc(&item.doc),
                                diff,
                            ));
                        }
                    }
                }
            }

            let mut removed: Vec<ChangedItem> = Vec::new();
            {
                // Title and doc are read before the soft-delete (the row stays,
                // only its flag flips) so the change summary can show the title
                // and the change log can classify the item's kind.
                let mut title_of = tx.prepare_cached(
                    "SELECT full_title, doc FROM items WHERE asin = ? AND marketplace = ?",
                )?;
                let mut statement = tx.prepare_cached(
                    "UPDATE items SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                     WHERE asin = ? AND marketplace = ?",
                )?;
                // A soft-deleted parent takes its episodes with it. An
                // independently-owned episode keeps its own `items` row — only
                // that row makes it visible (`--kind episode`), so nothing is
                // lost when the parent's child rows go (AUD-173).
                let mut delete_episodes = tx.prepare_cached(
                    "UPDATE episodes SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                     WHERE parent_asin = ? AND marketplace = ? AND is_deleted = 0",
                )?;
                for asin in &soft_deletes {
                    let prior: Option<(String, String)> = title_of
                        .query_row(rusqlite::params![asin, marketplace], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .optional()?;
                    if statement.execute(rusqlite::params![now, now, asin, marketplace])? > 0 {
                        delete_episodes.execute(rusqlite::params![now, now, asin, marketplace])?;
                        let (title, doc) = prior.unwrap_or_default();
                        if recording.record {
                            change_rows.push((
                                "removed",
                                asin.clone(),
                                title.clone(),
                                kind_of_doc(&doc),
                                None,
                            ));
                        }
                        removed.push(ChangedItem {
                            asin: asin.clone(),
                            full_title: title,
                        });
                    }
                }
            }

            let added_asins: Vec<&str> = added.iter().map(|item| item.asin.as_str()).collect();
            let changed_asins: Vec<&str> = changed.iter().map(|item| item.asin.as_str()).collect();
            let removed_asins: Vec<&str> = removed.iter().map(|item| item.asin.as_str()).collect();
            tx.execute(
                "INSERT INTO sync_log(marketplace, request_time_utc, request_state_token_utc,
                                      response_time_utc, response_state_token_utc,
                                      http_status, num_added, num_changed, num_soft_deleted,
                                      note, added_asins, changed_asins, soft_deleted_asins)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    marketplace,
                    log.request_time_utc,
                    log.request_state_token_utc,
                    log.response_time_utc,
                    log.response_state_token_utc,
                    log.http_status,
                    added_asins.len() as i64,
                    changed_asins.len() as i64,
                    removed_asins.len() as i64,
                    log.note,
                    serde_json::to_string(&added_asins).expect("strings serialize"),
                    serde_json::to_string(&changed_asins).expect("strings serialize"),
                    serde_json::to_string(&removed_asins).expect("strings serialize"),
                ],
            )?;

            // Per-item change history, correlated to the sync_log row just
            // inserted. Skipped on the initial sync (recording.record is false).
            if recording.record && !change_rows.is_empty() {
                let sync_id = tx.last_insert_rowid();
                let mut insert = tx.prepare_cached(
                    "INSERT INTO change_log(sync_id, recorded_utc, marketplace, asin, full_title, mode, kind, item_kind, changed)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )?;
                for (kind, asin, full_title, item_kind, diff) in &change_rows {
                    insert.execute(rusqlite::params![
                        sync_id,
                        now,
                        marketplace,
                        asin,
                        full_title,
                        recording.mode,
                        kind,
                        item_kind,
                        diff,
                    ])?;
                }
            }

            if let Some(raw) = &state_token_raw {
                tx.execute(
                    "UPDATE sync_state SET last_state_token_raw = ?, last_state_token_utc = ?
                     WHERE marketplace = ?",
                    rusqlite::params![raw, state_token_iso(raw), marketplace],
                )?;
            }

            tx.commit()?;
            Ok(ApplyOutcome {
                added,
                changed,
                changed_volatile,
                removed,
            })
        })
        .await
    }

    /// The full stored document of an active item, if present (used to
    /// read `product_images` for covers without an extra API call).
    pub async fn item_doc(
        &self,
        asin: String,
        marketplace: String,
    ) -> Result<Option<String>, DbError> {
        self.call(move |conn| {
            conn.query_row(
                "SELECT doc FROM items WHERE asin = ? AND marketplace = ? AND is_deleted = 0",
                rusqlite::params![asin, marketplace],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
        })
        .await
    }

    /// The raw document and parent (podcast) ASIN of a podcast **episode**,
    /// if present (AUD-100). The naming engine falls back to this when an ASIN
    /// is not an `items` row, so downloaded episodes are named/reorganized like
    /// books instead of by bare ASIN.
    pub async fn episode_doc(
        &self,
        asin: String,
        marketplace: String,
    ) -> Result<Option<(String, String)>, DbError> {
        self.call(move |conn| {
            conn.query_row(
                "SELECT doc, parent_asin FROM episodes
                 WHERE asin = ? AND marketplace = ? AND is_deleted = 0",
                rusqlite::params![asin, marketplace],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
        })
        .await
    }

    /// The `full_title` of an active item, if present.
    pub async fn find_title(
        &self,
        asin: String,
        marketplace: String,
    ) -> Result<Option<String>, DbError> {
        self.call(move |conn| {
            conn.query_row(
                "SELECT full_title FROM items WHERE asin = ? AND marketplace = ? AND is_deleted = 0",
                rusqlite::params![asin, marketplace],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
        })
        .await
    }

    /// Lists books from `v_books` across the marketplace set, newest
    /// purchases first (marketplace as the final, deterministic tiebreaker).
    pub async fn list_books(
        &self,
        marketplaces: Vec<String>,
        kinds: Vec<String>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<BookRow>, DbError> {
        let offset = offset.min(i64::MAX as u64) as i64;
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT marketplace, asin, full_title, CAST(purchase_date AS TEXT),
                        CAST(runtime_min AS TEXT), CAST(language AS TEXT), kind
                 FROM v_books
                 WHERE marketplace IN ({}) {}
                 ORDER BY purchase_date DESC, asin, marketplace
                 LIMIT ? OFFSET ?",
                in_placeholders(marketplaces.len()),
                super::kind_clause("kind", &kinds)
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len() + 2);
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            params.push(&limit);
            params.push(&offset);
            let rows = statement
                .query_map(params.as_slice(), book_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Counts active items across the marketplace set, honoring a
    /// `--kind` content filter (for `library list`'s empty/page-end
    /// paths; [`Self::count_active`] stays unfiltered for the sync).
    pub async fn count_books(
        &self,
        marketplaces: Vec<String>,
        kinds: Vec<String>,
    ) -> Result<u64, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(0);
            }
            let sql = format!(
                "SELECT COUNT(*) FROM v_books WHERE marketplace IN ({}) {}",
                in_placeholders(marketplaces.len()),
                super::kind_clause("kind", &kinds)
            );
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let count: i64 = conn.query_row(&sql, params.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    /// Active items that have no download record of at least one of the
    /// given kinds (`library list --missing`), newest purchases first.
    /// Each row names the kinds that are actually missing and its
    /// marketplace. `kinds` are matched regardless of content format.
    /// Archived items (`is_archived` in the doc, AUD-110) are excluded
    /// unless `include_archived`.
    pub async fn books_missing_downloads(
        &self,
        marketplaces: Vec<String>,
        kinds: Vec<String>,
        item_kinds: Vec<String>,
        limit: u32,
        offset: u64,
        include_archived: bool,
    ) -> Result<Vec<MissingDownloadsRow>, DbError> {
        let kinds_json = serde_json::to_string(&kinds).expect("strings serialize");
        let offset = offset.min(i64::MAX as u64) as i64;
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT marketplace, asin, full_title, missing FROM (
                     SELECT b.marketplace, b.asin, b.full_title, b.purchase_date,
                            (SELECT group_concat(k.value)
                             FROM json_each(?) k
                             WHERE NOT EXISTS (
                                 SELECT 1 FROM downloads d
                                 WHERE d.asin = b.asin AND d.marketplace = b.marketplace
                                   AND d.kind = k.value
                             )) AS missing
                     FROM v_books b
                     WHERE b.marketplace IN ({}) {} {}
                 )
                 WHERE missing IS NOT NULL
                 ORDER BY purchase_date DESC, asin, marketplace
                 LIMIT ? OFFSET ?",
                in_placeholders(marketplaces.len()),
                not_archived_clause(include_archived),
                super::kind_clause("b.kind", &item_kinds)
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len() + 3);
            params.push(&kinds_json);
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            params.push(&limit);
            params.push(&offset);
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(MissingDownloadsRow {
                        marketplace: row.get(0)?,
                        asin: row.get(1)?,
                        full_title: row.get(2)?,
                        missing: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Counts the rows [`Self::books_missing_downloads`] would return
    /// without limit (for the `--page` end-of-pages message).
    pub async fn count_books_missing_downloads(
        &self,
        marketplaces: Vec<String>,
        kinds: Vec<String>,
        item_kinds: Vec<String>,
        include_archived: bool,
    ) -> Result<u64, DbError> {
        let kinds_json = serde_json::to_string(&kinds).expect("strings serialize");
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(0);
            }
            let sql = format!(
                "SELECT COUNT(*) FROM v_books b
                 WHERE b.marketplace IN ({})
                   AND EXISTS (
                     SELECT 1 FROM json_each(?) k
                     WHERE NOT EXISTS (
                         SELECT 1 FROM downloads d
                         WHERE d.asin = b.asin AND d.marketplace = b.marketplace
                           AND d.kind = k.value
                     )
                 ) {} {}",
                in_placeholders(marketplaces.len()),
                not_archived_clause(include_archived),
                super::kind_clause("b.kind", &item_kinds)
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len() + 1);
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            params.push(&kinds_json);
            let count: i64 = conn.query_row(&sql, params.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    /// Titles the user did NOT purchase — access comes from a subscription or
    /// grant, so they can leave the library (`library list --borrowed`,
    /// AUD-153). Gated on `origin_type != 'Purchase'` (null-safe SQL `IS NOT`,
    /// so subscription titles with an absent `origin_type` are kept).
    /// `origin_type` is the reliable ownership marker; NOT `is_ayce`, which
    /// reflects current Plus-catalog membership (not acquisition), is `true`
    /// even for purchased titles also in Plus, and flips between API calls.
    ///
    /// Splits the item's `plans[]` (each `{plan_name, customer_eligible}`) by
    /// eligibility, sorted and comma-joined:
    /// - `eligible`: plans with `customer_eligible = true` — those you can
    ///   currently play through. Empty when none.
    /// - `not_eligible`: the remaining plans (`customer_eligible` false/null),
    ///   minus the generic `AccessViaMusic` route (near-universal noise) —
    ///   e.g. a promo `SpecialBenefit`, or the membership plan you'd need once
    ///   a subscription lapses. Empty when none.
    ///
    /// Ordered by title, then by the `(asin, marketplace)` key so pagination
    /// (`--limit`/`--page`) is stable.
    pub async fn books_borrowed(
        &self,
        marketplaces: Vec<String>,
        item_kinds: Vec<String>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<BorrowedRow>, DbError> {
        let offset = offset.min(i64::MAX as u64) as i64;
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT marketplace, asin, full_title,
                        COALESCE((SELECT group_concat(name, ', ') FROM (
                                    SELECT DISTINCT json_extract(p.value, '$.plan_name') AS name
                                    FROM json_each(items.doc, '$.plans') p
                                    WHERE json_extract(p.value, '$.customer_eligible') = 1
                                    ORDER BY name)), '') AS eligible,
                        COALESCE((SELECT group_concat(name, ', ') FROM (
                                    SELECT DISTINCT json_extract(p.value, '$.plan_name') AS name
                                    FROM json_each(items.doc, '$.plans') p
                                    WHERE json_extract(p.value, '$.customer_eligible') IS NOT 1
                                      AND json_extract(p.value, '$.plan_name') <> 'AccessViaMusic'
                                    ORDER BY name)), '') AS not_eligible
                 FROM items
                 WHERE is_deleted = 0
                   AND marketplace IN ({}) {}
                   AND json_extract(doc, '$.origin_type') IS NOT 'Purchase'
                 ORDER BY full_title, asin, marketplace
                 LIMIT ? OFFSET ?",
                in_placeholders(marketplaces.len()),
                super::kind_clause(&super::item_kind_sql("doc"), &item_kinds)
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len() + 2);
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            params.push(&limit);
            params.push(&offset);
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(BorrowedRow {
                        marketplace: row.get(0)?,
                        asin: row.get(1)?,
                        full_title: row.get(2)?,
                        eligible: row.get(3)?,
                        not_eligible: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Count for [`Self::books_borrowed`] (page-end detection).
    pub async fn count_books_borrowed(
        &self,
        marketplaces: Vec<String>,
        item_kinds: Vec<String>,
    ) -> Result<u64, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(0);
            }
            let sql = format!(
                "SELECT COUNT(*) FROM items
                 WHERE is_deleted = 0
                   AND marketplace IN ({}) {}
                   AND json_extract(doc, '$.origin_type') IS NOT 'Purchase'",
                in_placeholders(marketplaces.len()),
                super::kind_clause(&super::item_kind_sql("doc"), &item_kinds)
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len());
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            let count: i64 = conn.query_row(&sql, params.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    /// ASINs of active items missing a download record of at least one of
    /// the given kinds, newest purchases first (`download --missing`).
    /// Audio is format-aware (AUD-96): it counts as present only when a
    /// downloaded row's `request_kind` is one of `audio_request_kinds` (the
    /// run's candidate set, as in the per-item skip); other kinds stay
    /// kind-level. Archived items are excluded unless `include_archived`
    /// (AUD-110). Lean variant of [`Self::books_missing_downloads`]: only
    /// the ASINs, no pagination — built to scale to whole-library downloads.
    pub async fn missing_download_asins(
        &self,
        marketplaces: Vec<String>,
        kinds: Vec<String>,
        audio_request_kinds: Vec<String>,
        include_archived: bool,
    ) -> Result<Vec<String>, DbError> {
        let audio = kinds.iter().any(|kind| kind == "audio");
        let other: Vec<String> = kinds.into_iter().filter(|kind| kind != "audio").collect();
        let has_other = !other.is_empty();
        let other_json = serde_json::to_string(&other).expect("strings serialize");
        self.call(move |conn| {
            if marketplaces.is_empty() || (!audio && !has_other) {
                return Ok(Vec::new());
            }
            let mut missing: Vec<String> = Vec::new();
            if has_other {
                missing.push(
                    "EXISTS (
                         SELECT 1 FROM json_each(?) k
                         WHERE NOT EXISTS (
                             SELECT 1 FROM downloads d
                             WHERE d.asin = b.asin AND d.marketplace = b.marketplace
                               AND d.kind = k.value
                         )
                     )"
                    .to_owned(),
                );
            }
            if audio {
                // With no candidate set the audio check degrades to
                // kind-level (defensive; the caller always passes ≥ `mpeg`).
                if audio_request_kinds.is_empty() {
                    missing.push(
                        "NOT EXISTS (
                             SELECT 1 FROM downloads d
                             WHERE d.asin = b.asin AND d.marketplace = b.marketplace
                               AND d.kind = 'audio'
                         )"
                        .to_owned(),
                    );
                } else {
                    let placeholders = vec!["?"; audio_request_kinds.len()].join(",");
                    missing.push(format!(
                        "NOT EXISTS (
                             SELECT 1 FROM downloads d
                             WHERE d.asin = b.asin AND d.marketplace = b.marketplace
                               AND d.kind = 'audio' AND d.status = 'downloaded'
                               AND d.request_kind IN ({placeholders})
                         )"
                    ));
                }
            }
            let sql = format!(
                "SELECT b.asin FROM v_books b
                 WHERE b.marketplace IN ({})
                   AND ({}) {}
                 ORDER BY b.purchase_date DESC, b.asin, b.marketplace",
                in_placeholders(marketplaces.len()),
                missing.join(" OR "),
                not_archived_clause(include_archived)
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::new();
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            if has_other {
                params.push(&other_json);
            }
            if audio {
                for kind in &audio_request_kinds {
                    params.push(kind);
                }
            }
            let asins = statement
                .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(asins)
        })
        .await
    }

    /// Searches active books across the marketplace set — FTS5 `MATCH`
    /// (one global BM25 ranking) or case-insensitive LIKE. The FTS join
    /// maps each matched `rowid` to its exact `(asin, marketplace)` item,
    /// so the (non-unique) asin never causes a cross product.
    pub async fn search(
        &self,
        marketplaces: Vec<String>,
        item_kinds: Vec<String>,
        query: String,
        limit: u32,
        fts: bool,
    ) -> Result<Vec<BookRow>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = in_placeholders(marketplaces.len());
            if fts {
                let fts_query = prepare_fts_query(&query);
                let sql = format!(
                    "SELECT {ITEMS_BOOK_COLUMNS}, {}
                     FROM items_fts f JOIN items i ON i.rowid = f.rowid
                     WHERE items_fts MATCH ?
                       AND i.is_deleted = 0
                       AND i.marketplace IN ({placeholders}) {}
                     ORDER BY rank
                     LIMIT ?",
                    super::item_kind_sql("i.doc"),
                    super::kind_clause(&super::item_kind_sql("i.doc"), &item_kinds)
                );
                let mut statement = conn.prepare_cached(&sql)?;
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(marketplaces.len() + 2);
                params.push(&fts_query);
                for marketplace in &marketplaces {
                    params.push(marketplace);
                }
                params.push(&limit);
                let rows = statement
                    .query_map(params.as_slice(), book_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            } else {
                let sql = format!(
                    "SELECT marketplace, asin, full_title, CAST(purchase_date AS TEXT),
                            CAST(runtime_min AS TEXT), CAST(language AS TEXT), kind
                     FROM v_books
                     WHERE marketplace IN ({placeholders}) {}
                       AND lower(full_title) LIKE '%' || lower(?) || '%'
                     ORDER BY full_title, marketplace
                     LIMIT ?",
                    super::kind_clause("kind", &item_kinds)
                );
                let mut statement = conn.prepare_cached(&sql)?;
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(marketplaces.len() + 2);
                for marketplace in &marketplaces {
                    params.push(marketplace);
                }
                params.push(&query);
                params.push(&limit);
                let rows = statement
                    .query_map(params.as_slice(), book_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
        })
        .await
    }

    /// Flat book rows of all active items across the marketplace set (for
    /// `library export --format csv`): the `v_books` columns plus title
    /// and subtitle.
    pub async fn export_books(
        &self,
        marketplaces: Vec<String>,
        item_kinds: Vec<String>,
    ) -> Result<Vec<ExportBookRow>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT marketplace, asin, title, CAST(subtitle AS TEXT), full_title,
                        CAST(purchase_date AS TEXT), CAST(runtime_min AS TEXT),
                        CAST(language AS TEXT), kind
                 FROM v_books
                 WHERE marketplace IN ({}) {}
                 ORDER BY purchase_date DESC, asin, marketplace",
                in_placeholders(marketplaces.len()),
                super::kind_clause("kind", &item_kinds)
            );
            let mut statement = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(ExportBookRow {
                        marketplace: row.get(0)?,
                        asin: row.get(1)?,
                        title: row.get(2)?,
                        subtitle: row.get(3)?,
                        full_title: row.get(4)?,
                        purchase_date: row.get(5)?,
                        runtime_min: row.get(6)?,
                        language: row.get(7)?,
                        kind: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Full documents of all active items across the marketplace set (for
    /// `library export`).
    pub async fn export_docs(
        &self,
        marketplaces: Vec<String>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT doc FROM items WHERE marketplace IN ({}) AND is_deleted = 0
                 ORDER BY marketplace, asin",
                in_placeholders(marketplaces.len())
            );
            let mut statement = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let docs = statement
                .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|doc| serde_json::from_str(&doc).unwrap_or(serde_json::Value::Null))
                .collect();
            Ok(docs)
        })
        .await
    }

    /// Number of active (not soft-deleted) items across the marketplace set.
    pub async fn count_active(&self, marketplaces: Vec<String>) -> Result<u64, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(0);
            }
            let sql = format!(
                "SELECT COUNT(*) FROM items WHERE marketplace IN ({}) AND is_deleted = 0",
                in_placeholders(marketplaces.len())
            );
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let count: i64 = conn.query_row(&sql, params.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    /// Hard-deletes library items by ASIN in a given marketplace, in one
    /// transaction (`db library remove`). Deleting an item cascades its
    /// `episodes` and `item_series` rows; `downloads` and `licenses` carry
    /// no foreign key, so the rows of the item *and its episodes* are
    /// cleared by hand. Unknown ASINs are reported, not an error.
    pub async fn remove_items(
        &self,
        marketplace: String,
        asins: Vec<String>,
    ) -> Result<ItemRemoval, DbError> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let mut removal = ItemRemoval::default();
            {
                let mut find_item =
                    tx.prepare_cached("SELECT 1 FROM items WHERE asin = ? AND marketplace = ?")?;
                let mut find_episodes = tx.prepare_cached(
                    "SELECT asin FROM episodes WHERE parent_asin = ? AND marketplace = ?",
                )?;
                let mut find_files = tx.prepare_cached(
                    "SELECT file_path FROM downloads WHERE asin = ? AND marketplace = ?",
                )?;
                let mut delete_downloads =
                    tx.prepare_cached("DELETE FROM downloads WHERE asin = ? AND marketplace = ?")?;
                let mut delete_licenses =
                    tx.prepare_cached("DELETE FROM licenses WHERE asin = ? AND marketplace = ?")?;
                let mut delete_item =
                    tx.prepare_cached("DELETE FROM items WHERE asin = ? AND marketplace = ?")?;
                for asin in asins {
                    if !find_item.exists(rusqlite::params![asin, marketplace])? {
                        removal.missing_asins.push(asin);
                        continue;
                    }
                    let episode_asins: Vec<String> = find_episodes
                        .query_map(rusqlite::params![asin, marketplace], |row| row.get(0))?
                        .collect::<Result<_, _>>()?;
                    for owner in std::iter::once(&asin).chain(episode_asins.iter()) {
                        let files = find_files
                            .query_map(rusqlite::params![owner, marketplace], |row| {
                                row.get::<_, String>(0)
                            })?
                            .collect::<Result<Vec<_>, _>>()?;
                        removal.file_paths.extend(files);
                        removal.downloads_removed +=
                            delete_downloads.execute(rusqlite::params![owner, marketplace])?;
                        removal.licenses_removed +=
                            delete_licenses.execute(rusqlite::params![owner, marketplace])?;
                    }
                    removal.episodes_removed += episode_asins.len();
                    delete_item.execute(rusqlite::params![asin, marketplace])?;
                    removal.removed_asins.push(asin);
                }
            }
            tx.commit()?;
            Ok(removal)
        })
        .await
    }

    /// Soft-deletes a single library item (and its episodes) by flipping
    /// `is_deleted`, exactly as a sync does when the item turns `Revoked`.
    /// Used by `library return`: the loan is gone the moment the server
    /// accepts the DELETE, so the title leaves the library view now instead
    /// of lingering until the delta change-feed catches up (which lags
    /// minutes for a return, unlike a borrow). Downloaded files, `downloads`
    /// and `licenses` rows are kept. Returns true if a row flipped.
    pub async fn soft_delete_item(
        &self,
        marketplace: String,
        asin: String,
    ) -> Result<bool, DbError> {
        self.call(move |conn| {
            let now = crate::db::now_iso_utc();
            let flipped = conn.execute(
                "UPDATE items SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                 WHERE asin = ? AND marketplace = ? AND is_deleted = 0",
                rusqlite::params![now, now, asin, marketplace],
            )?;
            if flipped > 0 {
                // A soft-deleted parent takes its episodes with it.
                conn.execute(
                    "UPDATE episodes SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                     WHERE parent_asin = ? AND marketplace = ? AND is_deleted = 0",
                    rusqlite::params![now, now, asin, marketplace],
                )?;
            }
            Ok(flipped > 0)
        })
        .await
    }

    /// Timestamp of the last successful sync request, if any.
    pub async fn last_sync_utc(&self) -> Result<Option<String>, DbError> {
        self.call(|conn| {
            Ok(conn.query_row(
                "SELECT MAX(response_time_utc) FROM sync_log WHERE http_status = 200",
                [],
                |row| row.get(0),
            )?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::{MP, default_log, episode, item, open_temp};
    #[allow(unused_imports)]
    use crate::db::*;

    /// The SQL kind expression (`v_books.kind`, the FTS twin) must
    /// classify exactly like `models::library::item_kind` — one truth,
    /// three copies, verified functionally over the whole taxonomy.
    #[tokio::test]
    async fn kind_sql_matches_rust_classifier() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        let docs = [
            serde_json::json!({"asin":"K1","title":"episode",
                "content_delivery_type":"PodcastEpisode","content_type":"Podcast"}),
            serde_json::json!({"asin":"K2","title":"parent",
                "content_delivery_type":"PodcastParent","content_type":"Podcast"}),
            serde_json::json!({"asin":"K3","title":"periodical",
                "content_delivery_type":"Periodical","content_type":"Show"}),
            serde_json::json!({"asin":"K4","title":"season",
                "content_delivery_type":"PodcastSeason","content_type":"Podcast"}),
            serde_json::json!({"asin":"K5","title":"ct fallback","content_type":"Podcast"}),
            serde_json::json!({"asin":"K6","title":"book",
                "content_delivery_type":"SinglePartBook","content_type":"Product"}),
            serde_json::json!({"asin":"K7","title":"multipart",
                "content_delivery_type":"MultiPartBook","content_type":"Product"}),
            serde_json::json!({"asin":"K8","title":"bare"}),
        ];
        let upserts: Vec<UpsertItem> = docs
            .iter()
            .map(|doc| crate::db::test_util::upsert(doc["asin"].as_str().unwrap(), doc.clone()))
            .collect();
        db.apply_page(MP.into(), upserts, vec![], default_log(), None)
            .await
            .unwrap();

        // v_books.kind (list_books) against the Rust classifier.
        let books = db
            .list_books(vec![MP.to_owned()], vec![], u32::MAX, 0)
            .await
            .unwrap();
        assert_eq!(books.len(), docs.len());
        for doc in &docs {
            let asin = doc["asin"].as_str().unwrap();
            let row = books.iter().find(|b| b.asin == asin).unwrap();
            assert_eq!(
                row.kind,
                crate::models::library::item_kind(doc),
                "v_books.kind diverges from item_kind for {asin}: {doc}"
            );
        }
        // The FTS branch (item_kind_sql over i.doc) agrees too — the
        // asin is FTS-indexed, so it addresses one exact row.
        let hits = db
            .search(vec![MP.to_owned()], vec![], "K1".into(), 10, true)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "episode");
    }

    #[tokio::test]
    async fn kind_filter_narrows_list_search_export_and_counts() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        let upserts = vec![
            crate::db::test_util::upsert(
                "B1",
                serde_json::json!({"asin":"B1","title":"Buch",
                    "content_delivery_type":"SinglePartBook"}),
            ),
            crate::db::test_util::upsert(
                "P1",
                serde_json::json!({"asin":"P1","title":"Show",
                    "content_delivery_type":"PodcastParent"}),
            ),
            crate::db::test_util::upsert(
                "E1",
                serde_json::json!({"asin":"E1","title":"Folge",
                    "content_delivery_type":"PodcastEpisode","content_type":"Podcast"}),
            ),
        ];
        db.apply_page(MP.into(), upserts, vec![], default_log(), None)
            .await
            .unwrap();

        let asins = |rows: Vec<BookRow>| {
            let mut asins: Vec<String> = rows.into_iter().map(|r| r.asin).collect();
            asins.sort();
            asins
        };
        // Empty filter = everything.
        assert_eq!(
            asins(
                db.list_books(vec![MP.to_owned()], vec![], u32::MAX, 0)
                    .await
                    .unwrap()
            ),
            ["B1", "E1", "P1"]
        );
        // Single and multi-kind filters.
        assert_eq!(
            asins(
                db.list_books(vec![MP.to_owned()], vec!["episode".into()], u32::MAX, 0)
                    .await
                    .unwrap()
            ),
            ["E1"]
        );
        assert_eq!(
            asins(
                db.list_books(
                    vec![MP.to_owned()],
                    vec!["book".into(), "podcast".into()],
                    u32::MAX,
                    0,
                )
                .await
                .unwrap()
            ),
            ["B1", "P1"]
        );
        // Counts follow the same filter.
        assert_eq!(
            db.count_books(vec![MP.to_owned()], vec!["podcast".into()])
                .await
                .unwrap(),
            1
        );
        // Export carries the kind column and honors the filter.
        let export = db
            .export_books(vec![MP.to_owned()], vec!["episode".into()])
            .await
            .unwrap();
        assert_eq!(export.len(), 1);
        assert_eq!(
            (export[0].asin.as_str(), export[0].kind.as_str()),
            ("E1", "episode")
        );
        // Search (LIKE branch) honors it too: "title" matches every
        // full_title, the filter narrows to the book.
        let hits = db
            .search(
                vec![MP.to_owned()],
                vec!["book".into()],
                "title".into(),
                10,
                false,
            )
            .await
            .unwrap();
        assert_eq!(asins(hits), ["B1"]);
    }

    /// An item for the `books_borrowed` tests. `purchased` sets
    /// `origin_type = "Purchase"` (owned); otherwise the field is absent, as
    /// on subscription titles. `plans` is a list of `(plan_name, eligible)`,
    /// where `eligible` is `Some(true|false)` or `None` (JSON null).
    fn borrowed_item(asin: &str, purchased: bool, plans: &[(&str, Option<bool>)]) -> UpsertItem {
        let plans: Vec<serde_json::Value> = plans
            .iter()
            .map(|(name, eligible)| {
                serde_json::json!({
                    "plan_name": name,
                    "customer_eligible": match eligible {
                        Some(value) => serde_json::Value::Bool(*value),
                        None => serde_json::Value::Null,
                    },
                })
            })
            .collect();
        let mut doc = serde_json::json!({
            "asin": asin,
            "title": asin,
            "plans": plans,
        });
        if purchased {
            doc["origin_type"] = "Purchase".into();
        }
        UpsertItem {
            asin: asin.into(),
            doc: doc.to_string(),
            title: asin.into(),
            subtitle: None,
            full_title: asin.into(),
            series: Vec::new(),
        }
    }

    #[tokio::test]
    async fn books_borrowed_splits_plans_by_eligibility() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![
                // Eligible plan + the generic AccessViaMusic route (ignored).
                borrowed_item(
                    "A_ELIG",
                    false,
                    &[("Audible-AYCL", Some(true)), ("AccessViaMusic", None)],
                ),
                // Eligible, plus a promo the user is NOT on (kept as not_eligible).
                borrowed_item(
                    "B_PROMO",
                    false,
                    &[
                        ("US Minerva", Some(true)),
                        ("AccessViaMusic", None),
                        ("SpecialBenefit", None),
                    ],
                ),
                // No eligible plan (false + null) → membership plan surfaces as
                // not_eligible; eligible is empty.
                borrowed_item(
                    "C_LAPSED",
                    false,
                    &[("US Minerva", Some(false)), ("AccessViaMusic", None)],
                ),
                // Two eligible plans → sorted and comma-joined.
                borrowed_item(
                    "D_MULTI",
                    false,
                    &[("Plan B", Some(true)), ("Plan A", Some(true))],
                ),
                // Purchased → excluded entirely.
                borrowed_item("E_OWNED", true, &[("Audible-AYCL", Some(true))]),
            ],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        let rows = db
            .books_borrowed(vec![MP.to_owned()], vec![], u32::MAX, 0)
            .await
            .unwrap();
        let seen: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row.asin.as_str(),
                    row.eligible.as_str(),
                    row.not_eligible.as_str(),
                )
            })
            .collect();
        // Ordered by title; the purchased title is excluded. AccessViaMusic is
        // dropped from not_eligible; false and null both count as not eligible;
        // multiple eligible plans are sorted and joined.
        assert_eq!(
            seen,
            vec![
                ("A_ELIG", "Audible-AYCL", ""),
                ("B_PROMO", "US Minerva", "SpecialBenefit"),
                ("C_LAPSED", "", "US Minerva"),
                ("D_MULTI", "Plan A, Plan B", ""),
            ]
        );
        assert_eq!(
            db.count_books_borrowed(vec![MP.to_owned()], vec![])
                .await
                .unwrap(),
            4
        );
    }

    #[tokio::test]
    async fn sync_state_is_created_once_and_groups_are_pinned() {
        let (_dir, db) = open_temp().await;
        let settings = db.ensure_sync_state(MP.into(), "a,b".into()).await.unwrap();
        assert_eq!(settings.last_state_token, None);
        // Same groups: fine. Different groups: rejected.
        db.ensure_sync_state(MP.into(), "a,b".into()).await.unwrap();
        assert!(matches!(
            db.ensure_sync_state(MP.into(), "other".into()).await,
            Err(DbError::ResponseGroupsMismatch { .. })
        ));
        // A second marketplace is independent.
        db.ensure_sync_state("us".into(), "other".into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn apply_page_roundtrip() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            note: Some("test-page-1".into()),
            ..Default::default()
        };
        let outcome = db
            .apply_page(
                MP.into(),
                vec![item("A1", "Erstes Buch"), item("A2", "Zweites Buch")],
                vec![],
                log,
                Some("1750000000000".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                outcome.added.len(),
                outcome.changed.len(),
                outcome.removed.len()
            ),
            (2, 0, 0),
            "two new items on an empty DB are added"
        );
        assert_eq!(db.count_active(vec![MP.to_owned()]).await.unwrap(), 2);

        // State token persisted (and converted to ISO) for this marketplace.
        let settings = db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        assert_eq!(settings.last_state_token.as_deref(), Some("1750000000000"));
        assert!(db.last_sync_utc().await.unwrap().is_some());

        // Soft delete via a second page.
        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        };
        let outcome = db
            .apply_page(
                MP.into(),
                vec![],
                vec!["A2".into(), "GHOST".into()],
                log,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome.removed.len(), 1, "unknown asins are not counted");
        assert_eq!(db.count_active(vec![MP.to_owned()]).await.unwrap(), 1);

        // Upserting a deleted item revives it.
        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        };
        let outcome = db
            .apply_page(
                MP.into(),
                vec![item("A2", "Zweites Buch")],
                vec![],
                log,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.added.len(),
            1,
            "an item returning from a soft-delete counts as added"
        );
        assert_eq!(db.count_active(vec![MP.to_owned()]).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn marketplaces_are_isolated() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state("de".into(), "g".into()).await.unwrap();
        db.ensure_sync_state("us".into(), "g".into()).await.unwrap();

        // Insert the same ASIN into two different marketplaces.
        db.apply_page(
            "de".into(),
            vec![item("A1", "Buch DE")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_page(
            "us".into(),
            vec![item("A1", "Book US")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // Each marketplace sees only its own row.
        assert_eq!(db.count_active(vec!["de".to_owned()]).await.unwrap(), 1);
        assert_eq!(db.count_active(vec!["us".to_owned()]).await.unwrap(), 1);

        let de_books = db
            .list_books(vec!["de".to_owned()], vec![], 10, 0)
            .await
            .unwrap();
        let us_books = db
            .list_books(vec!["us".to_owned()], vec![], 10, 0)
            .await
            .unwrap();
        assert_eq!(de_books[0].full_title, "Buch DE");
        assert_eq!(us_books[0].full_title, "Book US");

        // The combined set (WHERE marketplace IN (…)) returns both rows,
        // each tagged with its marketplace.
        let both = db
            .list_books(vec!["de".to_owned(), "us".to_owned()], vec![], 10, 0)
            .await
            .unwrap();
        assert_eq!(both.len(), 2);
        let mut mps: Vec<&str> = both.iter().map(|b| b.marketplace.as_str()).collect();
        mps.sort_unstable();
        assert_eq!(mps, vec!["de", "us"]);
        assert_eq!(
            db.count_active(vec!["de".to_owned(), "us".to_owned()])
                .await
                .unwrap(),
            2
        );

        // FTS search across the set finds the same ASIN in both, one row
        // per (asin, marketplace) — no cross-product from the shared asin.
        let hits = db
            .search(
                vec!["de".to_owned(), "us".to_owned()],
                vec![],
                "buch OR book".into(),
                10,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "one ranked row per marketplace, no duplicates"
        );

        // Soft-deleting in DE does not affect US.
        db.apply_page("de".into(), vec![], vec!["A1".into()], default_log(), None)
            .await
            .unwrap();
        assert_eq!(db.count_active(vec!["de".to_owned()]).await.unwrap(), 0);
        assert_eq!(db.count_active(vec!["us".to_owned()]).await.unwrap(), 1);
        // The combined search now sees only the surviving US row.
        let hits = db
            .search(
                vec!["de".to_owned(), "us".to_owned()],
                vec![],
                "buch OR book".into(),
                10,
                true,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].marketplace, "us");
    }

    #[tokio::test]
    async fn cascade_works_across_asin_marketplace() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        let mut podcast = item("P1", "Podcast");
        podcast.series.push(SeriesRef {
            series_asin: "S1".into(),
            series_title: "Serie".into(),
            sequence: None,
        });
        db.apply_page(MP.into(), vec![podcast], vec![], default_log(), None)
            .await
            .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E1", "Folge 1"), episode("E2", "Folge 2")],
            ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        // Verify episodes exist.
        assert_eq!(
            db.episodes(MP.into(), Some("P1".into()), 10, 0)
                .await
                .unwrap()
                .len(),
            2
        );

        // Hard-delete the item: episodes and item_series cascade.
        let removal = db.remove_items(MP.into(), vec!["P1".into()]).await.unwrap();
        assert_eq!(removal.removed_asins, vec!["P1".to_owned()]);
        assert_eq!(removal.episodes_removed, 2);

        // No episodes or series left.
        assert!(
            db.episodes(MP.into(), Some("P1".into()), 10, 0)
                .await
                .unwrap()
                .is_empty()
        );
        let overview = db.series_overview(vec![MP.to_owned()]).await.unwrap();
        assert!(overview.is_empty());
    }

    #[tokio::test]
    async fn episode_doc_returns_doc_and_parent() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("P1", "Podcast")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E1", "Folge 1")],
            ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        let (doc, parent) = db
            .episode_doc("E1".into(), MP.into())
            .await
            .unwrap()
            .expect("the episode exists");
        assert_eq!(parent, "P1");
        let parsed: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(parsed["title"], "Folge 1");

        // A book ASIN is not an episode; an unknown ASIN is None.
        assert!(
            db.episode_doc("P1".into(), MP.into())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.episode_doc("X9".into(), MP.into())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_search_and_export() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        };
        db.apply_page(
            MP.into(),
            vec![
                item("A1", "Der Hobbit"),
                item("A2", "Die Känguru-Chroniken"),
            ],
            vec![],
            log,
            None,
        )
        .await
        .unwrap();

        let books = db
            .list_books(vec![MP.to_owned()], vec![], 10, 0)
            .await
            .unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].purchase_date.as_deref(), Some("2024-01-01"));
        assert_eq!(books[0].runtime_min.as_deref(), Some("123"));

        let like = db
            .search(vec![MP.to_owned()], vec![], "hobbit".into(), 10, false)
            .await
            .unwrap();
        assert_eq!(like.len(), 1);
        assert_eq!(like[0].asin, "A1");

        let fts = db
            .search(vec![MP.to_owned()], vec![], "känguru".into(), 10, true)
            .await
            .unwrap();
        assert_eq!(fts.len(), 1);
        assert_eq!(fts[0].asin, "A2");

        let docs = db.export_docs(vec![MP.to_owned()]).await.unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["asin"], "A1");
    }

    #[tokio::test]
    async fn lists_items_missing_download_kinds() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("A1", "Mit Audio"), item("A2", "Ohne alles")],
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
                content_format: "AAX_44_128".into(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: "/dl/A1.aaxc".into(),
                file_size: Some(1),
            },
        )
        .await
        .unwrap();

        // audio + cover: A1 lacks only the cover, A2 lacks both.
        let rows = db
            .books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into(), "cover".into()],
                vec![],
                u32::MAX,
                0,
                true,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].asin.as_str(), rows[0].missing.as_str()),
            ("A1", "cover")
        );
        assert_eq!(
            (rows[1].asin.as_str(), rows[1].missing.as_str()),
            ("A2", "audio,cover")
        );

        // audio only: A1 is complete and drops out.
        let rows = db
            .books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec![],
                u32::MAX,
                0,
                true,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].asin, "A2");

        // limit/offset page through the result.
        let rows = db
            .books_missing_downloads(
                vec![MP.to_owned()],
                vec!["cover".into()],
                vec![],
                1,
                1,
                true,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].asin, "A2");

        // The count matches the unlimited result per kind set.
        assert_eq!(
            db.count_books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into(), "cover".into()],
                vec![],
                true
            )
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            db.count_books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec![],
                true
            )
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn remove_items_cascades_episodes_series_downloads_and_licenses() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        // A1: podcast with two episodes and a series membership.
        // A2: unrelated book that must survive.
        let mut podcast = item("A1", "Podcast");
        podcast.series.push(SeriesRef {
            series_asin: "S1".into(),
            series_title: "Serie".into(),
            sequence: None,
        });
        db.apply_page(
            MP.into(),
            vec![podcast, item("A2", "Buch")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "A1".into(),
            vec![episode("E1", "Eins"), episode("E2", "Zwei")],
            ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        let rec = |asin: &str, kind: &str, path: &str| DownloadRecord {
            asin: asin.into(),
            kind: kind.into(),
            acr: None,
            content_format: String::new(),
            variant: "original".into(),
            request_kind: String::new(),
            version: None,
            sku: None,
            file_path: path.into(),
            file_size: None,
        };
        db.record_download(MP.into(), rec("A1", "cover", "/dl/A1.jpg"))
            .await
            .unwrap();
        db.record_download(MP.into(), rec("E1", "audio", "/dl/E1.aaxc"))
            .await
            .unwrap();
        db.record_download(MP.into(), rec("A2", "cover", "/dl/A2.jpg"))
            .await
            .unwrap();
        db.upsert_license(
            MP.into(),
            LicenseGrant {
                asin: "E1".into(),
                content_format: "AAX_44_128".into(),
                request_kind: "adrm-high".into(),
                valid_until: None,
                doc: "{}".into(),
            },
        )
        .await
        .unwrap();

        let removal = db
            .remove_items(MP.into(), vec!["A1".into(), "GHOST".into()])
            .await
            .unwrap();
        assert_eq!(removal.removed_asins, vec!["A1".to_owned()]);
        assert_eq!(removal.missing_asins, vec!["GHOST".to_owned()]);
        assert_eq!(removal.episodes_removed, 2);
        assert_eq!(removal.downloads_removed, 2, "item + episode records");
        assert_eq!(removal.licenses_removed, 1, "episode license");
        let mut paths = removal.file_paths.clone();
        paths.sort();
        assert_eq!(
            paths,
            vec!["/dl/A1.jpg".to_owned(), "/dl/E1.aaxc".to_owned()]
        );

        // A2 survives untouched; A1's episodes and series rows are gone.
        let stats = db.stats().await.unwrap();
        assert_eq!(stats.items_active, 1);
        assert_eq!(stats.episodes_active, 0);
        assert_eq!(stats.series, 0);
        assert_eq!(stats.downloads, 1);
        assert_eq!(stats.licenses, 0);
        let rest = db.download_entries().await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].asin, "A2");
    }

    #[tokio::test]
    async fn missing_download_asins_lists_items_lacking_a_kind() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        };
        db.apply_page(
            MP.into(),
            vec![item("A1", "Eins"), item("A2", "Zwei")],
            vec![],
            log,
            None,
        )
        .await
        .unwrap();

        let rec = |asin: &str, kind: &str| DownloadRecord {
            asin: asin.into(),
            kind: kind.into(),
            acr: None,
            content_format: String::new(),
            variant: "original".into(),
            request_kind: String::new(),
            version: None,
            sku: None,
            file_path: format!("/dl/{asin}.{kind}"),
            file_size: None,
        };
        // A1 has adrm-high audio recorded; A2 has nothing.
        let mut a1_audio = rec("A1", "audio");
        a1_audio.request_kind = "adrm-high".into();
        db.record_download(MP.into(), a1_audio).await.unwrap();

        let default_high = || {
            vec![
                "adrm-high".to_owned(),
                "widevine-aac-high".to_owned(),
                "mpeg".to_owned(),
            ]
        };

        // Missing audio (default-high run) → only A2; A1's row matches a
        // candidate.
        let mut audio = db
            .missing_download_asins(
                vec![MP.to_owned()],
                vec!["audio".into()],
                default_high(),
                true,
            )
            .await
            .unwrap();
        audio.sort();
        assert_eq!(audio, vec!["A2".to_owned()]);

        // Format-aware (AUD-96): a forced-xhe run does not accept A1's
        // adrm-high row → both items are missing.
        let mut xhe = db
            .missing_download_asins(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec!["widevine-xhe-high".to_owned(), "mpeg".to_owned()],
                true,
            )
            .await
            .unwrap();
        xhe.sort();
        assert_eq!(xhe, vec!["A1".to_owned(), "A2".to_owned()]);

        // Missing audio OR cover → A1 (no cover) and A2 (neither).
        let mut both = db
            .missing_download_asins(
                vec![MP.to_owned()],
                vec!["audio".into(), "cover".into()],
                default_high(),
                true,
            )
            .await
            .unwrap();
        both.sort();
        assert_eq!(both, vec!["A1".to_owned(), "A2".to_owned()]);

        // A different marketplace has none of these items.
        assert!(
            db.missing_download_asins(
                vec!["us".to_owned()],
                vec!["audio".into()],
                default_high(),
                true
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    /// Archived items are excluded from every missing-downloads query
    /// unless `include_archived` (AUD-110).
    #[tokio::test]
    async fn missing_queries_skip_archived_items_unless_included() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        let mut archived = item("A1", "Archived");
        let mut doc: serde_json::Value = serde_json::from_str(&archived.doc).unwrap();
        doc["is_archived"] = serde_json::Value::Bool(true);
        archived.doc = doc.to_string();
        let log = SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        };
        // A1 archived, A2 active — neither has any download.
        db.apply_page(
            MP.into(),
            vec![archived, item("A2", "Active")],
            vec![],
            log,
            None,
        )
        .await
        .unwrap();

        let asins = db
            .missing_download_asins(vec![MP.to_owned()], vec!["audio".into()], vec![], false)
            .await
            .unwrap();
        assert_eq!(asins, vec!["A2".to_owned()]);
        let mut all = db
            .missing_download_asins(vec![MP.to_owned()], vec!["audio".into()], vec![], true)
            .await
            .unwrap();
        all.sort();
        assert_eq!(all, vec!["A1".to_owned(), "A2".to_owned()]);

        let rows = db
            .books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec![],
                u32::MAX,
                0,
                false,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].asin, "A2");
        assert_eq!(
            db.count_books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec![],
                false
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            db.count_books_missing_downloads(
                vec![MP.to_owned()],
                vec!["audio".into()],
                vec![],
                true
            )
            .await
            .unwrap(),
            2
        );
    }

    #[test]
    fn state_token_iso_handles_seconds_and_millis() {
        assert_eq!(
            state_token_iso("1750000000000").as_deref(),
            Some("2025-06-15T15:06:40Z")
        );
        assert_eq!(
            state_token_iso("1750000000").as_deref(),
            Some("2025-06-15T15:06:40Z")
        );
        assert_eq!(state_token_iso("abc"), None);
    }

    #[test]
    fn prepare_fts_query_quotes_and_prefixes() {
        // Multi-word plain query: each token becomes a quoted prefix.
        assert_eq!(prepare_fts_query("star wars"), "\"star\"* \"wars\"*");
        // Single plain word: becomes a quoted prefix.
        assert_eq!(prepare_fts_query("jed"), "\"jed\"*");
        // Punctuation that would otherwise crash FTS5: still quoted.
        assert_eq!(prepare_fts_query("c++"), "\"c++\"*");
        // Empty string: returns empty string.
        assert_eq!(prepare_fts_query(""), "");
        // Whitespace-only: returns empty string.
        assert_eq!(prepare_fts_query("   "), "");
        // Input containing `*`: passthrough (user is using FTS5 syntax).
        assert_eq!(prepare_fts_query("jedi*"), "jedi*");
        // Input with a leading `"`: passthrough.
        assert_eq!(prepare_fts_query("\"star wars\""), "\"star wars\"");
        // Explicit AND operator: passthrough.
        assert_eq!(
            prepare_fts_query("hobbit AND tolkien"),
            "hobbit AND tolkien"
        );
        // Explicit OR operator: passthrough.
        assert_eq!(prepare_fts_query("star OR wars"), "star OR wars");
        // Explicit NOT operator: passthrough.
        assert_eq!(prepare_fts_query("jedi NOT sith"), "jedi NOT sith");
        // NEAR is an operator too: passthrough.
        assert_eq!(prepare_fts_query("NEAR(jedi sith)"), "NEAR(jedi sith)");
    }

    #[tokio::test]
    async fn fts_prefix_search_finds_partial_title() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("JQ1", "Jedi Quest"), item("SW1", "Star Wars")],
            vec![],
            SyncLogEntry {
                request_time_utc: now_iso_utc(),
                response_time_utc: now_iso_utc(),
                http_status: Some(200),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        // "jed" does not appear literally in any title; without prefix
        // expansion the FTS MATCH would return nothing. With prepare_fts_query
        // it becomes `"jed"*` and matches "Jedi Quest".
        let results = db
            .search(vec![MP.to_owned()], vec![], "jed".into(), 10, true)
            .await
            .unwrap();
        assert_eq!(results.len(), 1, "prefix search must find 'Jedi Quest'");
        assert_eq!(results[0].asin, "JQ1");

        // Punctuation that previously caused a syntax error is now safe.
        // "c++" has no match in the DB, but the query must not return an error.
        let punct = db
            .search(vec![MP.to_owned()], vec![], "c++".into(), 10, true)
            .await;
        assert!(punct.is_ok(), "punctuation query must not raise FTS5 error");
        assert!(punct.unwrap().is_empty());

        // Sanity: exact-token FTS still works (känguru → "känguru"*).
        let exact = db
            .search(vec![MP.to_owned()], vec![], "star".into(), 10, true)
            .await
            .unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].asin, "SW1");
    }
}
