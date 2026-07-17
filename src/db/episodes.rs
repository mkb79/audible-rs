//! Podcast parents and their episodes: episode application and the
//! podcast/episode listings.

use rusqlite::OptionalExtension as _;

use super::{ChangeRecording, Db, DbError, in_placeholders, now_iso_utc};
use crate::db::changes::{ChangeClass, classify_change};

/// One podcast episode to upsert.
#[derive(Debug, Clone)]
pub struct UpsertEpisode {
    /// Audible ASIN (primary key together with marketplace).
    pub asin: String,
    /// Full episode document as JSON text.
    pub doc: String,
    /// Episode title.
    pub title: String,
    /// Optional subtitle.
    pub subtitle: Option<String>,
    /// `Title: Subtitle` (or just the title).
    pub full_title: String,
}

/// One podcast subscription (an active parent item).
#[derive(Debug, Clone)]
pub struct PodcastRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// ASIN of the parent.
    pub asin: String,
    /// Podcast title.
    pub full_title: String,
    /// `episode_count` announced in the parent document, if any.
    pub announced_episodes: Option<String>,
    /// Episodes currently stored (active) in the database.
    pub stored_episodes: u64,
}

/// One stored podcast episode.
#[derive(Debug, Clone)]
pub struct EpisodeRow {
    /// Episode ASIN.
    pub asin: String,
    /// Parent podcast ASIN.
    pub parent_asin: String,
    /// Episode title.
    pub full_title: String,
    /// Release date, if present in the document.
    pub release_date: Option<String>,
    /// Runtime in minutes, if present.
    pub runtime_min: Option<String>,
}

/// A title-search hit in the `episodes` table (AUD-174) — carries the
/// show's title so pickers can label it distinguishably.
#[derive(Debug, Clone)]
pub struct EpisodeHit {
    /// Episode ASIN.
    pub asin: String,
    /// Episode title (incl. subtitle).
    pub full_title: String,
    /// Title of the followed show (falls back to the parent ASIN when the
    /// parent item is gone).
    pub parent_title: String,
}

impl Db {
    /// Replaces the episode set of one podcast parent: upserts the given
    /// episodes and soft-deletes episodes that vanished from the feed.
    /// Per `recording`, added/changed/removed episodes are written to the
    /// `change_log` (item_kind `episode`, AUD-173) with the same
    /// classification as items — unchanged/reordered docs are not
    /// recorded, volatile-only diffs are recorded but hidden by default.
    /// Returns (upserted, soft-deleted).
    pub async fn apply_episodes(
        &self,
        marketplace: String,
        parent_asin: String,
        episodes: Vec<UpsertEpisode>,
        recording: ChangeRecording,
    ) -> Result<(usize, usize), DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            let tx = conn.transaction()?;

            // change_log rows: (change, asin, full_title, diff).
            let mut change_rows: Vec<(&'static str, String, String, Option<String>)> = Vec::new();
            {
                let mut prior = tx.prepare_cached(
                    "SELECT doc, is_deleted FROM episodes WHERE asin = ? AND marketplace = ?",
                )?;
                let mut statement = tx.prepare_cached(
                    "INSERT INTO episodes(asin, marketplace, parent_asin, doc, title, subtitle, full_title, updated_utc, is_deleted, deleted_utc)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, NULL)
                     ON CONFLICT(asin, marketplace) DO UPDATE SET
                       parent_asin = excluded.parent_asin,
                       doc         = excluded.doc,
                       title       = excluded.title,
                       subtitle    = excluded.subtitle,
                       full_title  = excluded.full_title,
                       updated_utc = excluded.updated_utc,
                       is_deleted  = 0,
                       deleted_utc = NULL",
                )?;
                for episode in &episodes {
                    let prior_state: Option<(String, i64)> = prior
                        .query_row(rusqlite::params![episode.asin, marketplace], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })
                        .optional()?;
                    statement.execute(rusqlite::params![
                        episode.asin,
                        marketplace,
                        parent_asin,
                        episode.doc,
                        episode.title,
                        episode.subtitle,
                        episode.full_title,
                        now,
                    ])?;
                    if recording.record {
                        let classified = match &prior_state {
                            Some((old_doc, 0)) => match classify_change(old_doc, &episode.doc) {
                                (ChangeClass::Unchanged, _) => None,
                                (_, diff) => Some(("changed", diff)),
                            },
                            _ => Some(("added", None)),
                        };
                        if let Some((change, diff)) = classified {
                            change_rows.push((
                                change,
                                episode.asin.clone(),
                                episode.full_title.clone(),
                                diff,
                            ));
                        }
                    }
                }
            }

            // Episodes of this parent that are no longer in the feed.
            let existing: Vec<(String, String)> = tx
                .prepare_cached(
                    "SELECT asin, full_title FROM episodes
                     WHERE parent_asin = ? AND marketplace = ? AND is_deleted = 0",
                )?
                .query_map(rusqlite::params![parent_asin, marketplace], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<_, _>>()?;
            let fresh: std::collections::BTreeSet<&str> =
                episodes.iter().map(|e| e.asin.as_str()).collect();
            let mut removed = 0usize;
            {
                let mut soft_delete = tx.prepare_cached(
                    "UPDATE episodes SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                     WHERE asin = ? AND marketplace = ?",
                )?;
                for (asin, full_title) in existing {
                    if !fresh.contains(asin.as_str()) {
                        soft_delete.execute(rusqlite::params![now, now, asin, marketplace])?;
                        removed += 1;
                        if recording.record {
                            change_rows.push(("removed", asin, full_title, None));
                        }
                    }
                }
            }

            // Episode change history (AUD-173). No sync_id: episode
            // resolution runs after (and independent of) the page applies.
            if !change_rows.is_empty() {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO change_log(sync_id, recorded_utc, marketplace, asin, full_title, mode, kind, item_kind, changed)
                     VALUES (NULL, ?, ?, ?, ?, ?, ?, 'episode', ?)",
                )?;
                for (change, asin, full_title, diff) in &change_rows {
                    insert.execute(rusqlite::params![
                        now,
                        marketplace,
                        asin,
                        full_title,
                        recording.mode,
                        change,
                        diff,
                    ])?;
                }
            }

            tx.commit()?;
            Ok((episodes.len(), removed))
        })
        .await
    }

    /// Active podcast parents across the marketplace set, with their
    /// stored episode counts. Filters on the shared kind expression, so
    /// this is by construction the same set as `library list --kind
    /// podcast` — an individually-subscribed `PodcastEpisode` item never
    /// shows up as a show (AUD-173).
    pub async fn podcasts(&self, marketplaces: Vec<String>) -> Result<Vec<PodcastRow>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT i.marketplace, i.asin, i.full_title,
                        CAST(json_extract(i.doc, '$.episode_count') AS TEXT),
                        (SELECT COUNT(*) FROM episodes e
                          WHERE e.parent_asin = i.asin AND e.marketplace = i.marketplace
                            AND e.is_deleted = 0)
                 FROM items i
                 WHERE i.marketplace IN ({})
                   AND i.is_deleted = 0
                   AND {} = 'podcast'
                 ORDER BY i.full_title, i.marketplace",
                in_placeholders(marketplaces.len()),
                super::item_kind_sql("i.doc")
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(PodcastRow {
                        marketplace: row.get(0)?,
                        asin: row.get(1)?,
                        full_title: row.get(2)?,
                        announced_episodes: row.get(3)?,
                        stored_episodes: row.get::<_, i64>(4)? as u64,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Active episodes for a marketplace, newest first; optionally only
    /// one parent's.
    pub async fn episodes(
        &self,
        marketplace: String,
        parent_asin: Option<String>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<EpisodeRow>, DbError> {
        let offset = offset.min(i64::MAX as u64) as i64;
        self.call(move |conn| {
            let sql = "SELECT asin, parent_asin, full_title,
                              CAST(release_date AS TEXT), CAST(runtime_min AS TEXT)
                       FROM v_episodes
                       WHERE marketplace = ?1
                         AND (?2 IS NULL OR parent_asin = ?2)
                       ORDER BY release_date DESC, asin
                       LIMIT ?3 OFFSET ?4";
            let mut statement = conn.prepare_cached(sql)?;
            let rows = statement
                .query_map(
                    rusqlite::params![marketplace, parent_asin, limit, offset],
                    |row| {
                        Ok(EpisodeRow {
                            asin: row.get(0)?,
                            parent_asin: row.get(1)?,
                            full_title: row.get(2)?,
                            release_date: row.get(3)?,
                            runtime_min: row.get(4)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Title search over active episodes (AUD-174): case-insensitive
    /// substring match on `full_title`, joined to the parent item for the
    /// show's title. LIKE instead of FTS — episode titles are long and
    /// distinctive, and `episodes` has no FTS table by design.
    pub async fn search_episodes(
        &self,
        marketplace: String,
        query: String,
        limit: u32,
    ) -> Result<Vec<EpisodeHit>, DbError> {
        self.call(move |conn| {
            let sql = "SELECT e.asin, e.full_title,
                              COALESCE(i.full_title, e.parent_asin)
                       FROM episodes e
                       LEFT JOIN items i
                         ON i.asin = e.parent_asin AND i.marketplace = e.marketplace
                       WHERE e.marketplace = ?1 AND e.is_deleted = 0
                         AND lower(e.full_title) LIKE '%' || lower(?2) || '%' ESCAPE '\\'
                       ORDER BY e.full_title, e.asin
                       LIMIT ?3";
            let mut statement = conn.prepare_cached(sql)?;
            let rows = statement
                .query_map(
                    rusqlite::params![marketplace, super::escape_like(&query), limit],
                    |row| {
                        Ok(EpisodeHit {
                            asin: row.get(0)?,
                            full_title: row.get(1)?,
                            parent_title: row.get(2)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Counts active episodes for a marketplace, optionally only one
    /// parent's (for the `--page` end-of-pages message).
    pub async fn count_episodes(
        &self,
        marketplace: String,
        parent_asin: Option<String>,
    ) -> Result<u64, DbError> {
        self.call(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM v_episodes
                 WHERE marketplace = ?1 AND (?2 IS NULL OR parent_asin = ?2)",
                rusqlite::params![marketplace, parent_asin],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
        .await
    }

    /// Episodes of one show that have no download record of at least one of
    /// the given kinds (`library episodes <SHOW> --missing`), newest first —
    /// the drill-down of the show roll-up that `library list --missing`
    /// displays (AUD-205). Each row names the kinds it actually lacks.
    ///
    /// An episode is a **leaf**: unlike the show in
    /// [`Db::books_missing_downloads`], which owns no record and rolls its
    /// episodes up, the test here is simply "no record for this ASIN".
    pub async fn episodes_missing_downloads(
        &self,
        marketplace: String,
        parent_asin: String,
        kinds: Vec<String>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<MissingEpisodeRow>, DbError> {
        let kinds_json = serde_json::to_string(&kinds).expect("strings serialize");
        let offset = offset.min(i64::MAX as u64) as i64;
        self.call(move |conn| {
            // A kind that cannot exist for the episode is never missing — a
            // podcast episode has no PDF, so it must not be reported (AUD-206).
            let possible = super::kind_possible_sql(&super::pdf_available_lookup_sql(
                "episodes",
                "e.asin",
                "e.marketplace",
            ));
            let sql = format!(
                "SELECT asin, full_title, release_date, missing FROM (
                     SELECT e.asin AS asin, e.full_title AS full_title,
                            CAST(e.release_date AS TEXT) AS release_date,
                            (SELECT group_concat(k.value)
                             FROM json_each(?1) k
                             WHERE NOT EXISTS (
                                 SELECT 1 FROM downloads d
                                 WHERE d.asin = e.asin AND d.marketplace = e.marketplace
                                   AND d.kind = k.value
                             )
                             AND {possible}) AS missing
                     FROM v_episodes e
                     WHERE e.marketplace = ?2 AND e.parent_asin = ?3
                 )
                 WHERE missing IS NOT NULL
                 ORDER BY release_date DESC, asin
                 LIMIT ?4 OFFSET ?5"
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let rows = statement
                .query_map(
                    rusqlite::params![kinds_json, marketplace, parent_asin, limit, offset],
                    |row| {
                        Ok(MissingEpisodeRow {
                            asin: row.get(0)?,
                            full_title: row.get(1)?,
                            release_date: row.get(2)?,
                            missing: row.get(3)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Counts the rows [`Self::episodes_missing_downloads`] would return
    /// without limit (for the `--page` end-of-pages message).
    pub async fn count_episodes_missing_downloads(
        &self,
        marketplace: String,
        parent_asin: String,
        kinds: Vec<String>,
    ) -> Result<u64, DbError> {
        let kinds_json = serde_json::to_string(&kinds).expect("strings serialize");
        self.call(move |conn| {
            let possible = super::kind_possible_sql(&super::pdf_available_lookup_sql(
                "episodes",
                "e.asin",
                "e.marketplace",
            ));
            let sql = format!(
                "SELECT COUNT(*) FROM v_episodes e
                 WHERE e.marketplace = ?2 AND e.parent_asin = ?3
                   AND EXISTS (
                       SELECT 1 FROM json_each(?1) k
                       WHERE NOT EXISTS (
                           SELECT 1 FROM downloads d
                           WHERE d.asin = e.asin AND d.marketplace = e.marketplace
                             AND d.kind = k.value
                       )
                       AND {possible}
                   )"
            );
            let count: i64 = conn.query_row(
                &sql,
                rusqlite::params![kinds_json, marketplace, parent_asin],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
        .await
    }
}

/// One episode of a show that still lacks a download
/// ([`Db::episodes_missing_downloads`]).
#[derive(Debug, Clone)]
pub struct MissingEpisodeRow {
    /// Episode ASIN.
    pub asin: String,
    /// Episode title.
    pub full_title: String,
    /// Release date, if present in the document.
    pub release_date: Option<String>,
    /// The requested kinds this episode has no download record for.
    pub missing: String,
}

#[cfg(test)]
mod tests {

    use crate::db::test_util::{MP, default_log, episode, item, open_temp};

    /// Episode application without change recording (the common test case).
    fn no_recording() -> ChangeRecording {
        ChangeRecording {
            record: false,
            mode: "delta",
        }
    }
    #[allow(unused_imports)]
    use crate::db::*;

    /// The episode drill-down (AUD-205): only the episodes actually lacking a
    /// download are returned, each naming what it lacks — the leaf view of the
    /// show roll-up that `library list --missing` displays.
    #[tokio::test]
    async fn episodes_missing_downloads_lists_only_undownloaded_episodes() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("P1", "A Show")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![
                episode("E1", "Eins"),
                episode("E2", "Zwei"),
                episode("E3", "Drei"),
            ],
            no_recording(),
        )
        .await
        .unwrap();
        // Only E2 has its audio.
        db.record_download(
            MP.into(),
            DownloadRecord {
                asin: "E2".into(),
                kind: "audio".into(),
                acr: None,
                content_format: String::new(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: "/dl/E2.aaxc".into(),
                file_size: None,
            },
        )
        .await
        .unwrap();

        let mut rows = db
            .episodes_missing_downloads(MP.into(), "P1".into(), vec!["audio".into()], u32::MAX, 0)
            .await
            .unwrap();
        rows.sort_by(|a, b| a.asin.cmp(&b.asin));
        let asins: Vec<&str> = rows.iter().map(|row| row.asin.as_str()).collect();
        assert_eq!(asins, vec!["E1", "E3"], "E2 is downloaded");
        assert!(
            rows.iter().all(|row| row.missing == "audio"),
            "each row names what it lacks"
        );
        assert_eq!(
            db.count_episodes_missing_downloads(MP.into(), "P1".into(), vec!["audio".into()])
                .await
                .unwrap(),
            2,
            "count matches the rows"
        );
        // Nothing was recorded for covers, so every episode lacks that kind.
        assert_eq!(
            db.count_episodes_missing_downloads(MP.into(), "P1".into(), vec!["cover".into()])
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn episodes_lifecycle_with_parent_soft_delete() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        // A podcast parent plus two episodes.
        let mut parent = item("P1", "Mein Podcast");
        parent.doc = serde_json::json!({
            "asin": "P1",
            "title": "Mein Podcast",
            "content_delivery_type": "PodcastParent",
            "episode_count": 3,
        })
        .to_string();
        db.apply_page(MP.into(), vec![parent], vec![], default_log(), None)
            .await
            .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E1", "Folge 1"), episode("E2", "Folge 2")],
            no_recording(),
        )
        .await
        .unwrap();

        let podcasts = db.podcasts(vec![MP.to_owned()]).await.unwrap();
        assert_eq!(podcasts.len(), 1);
        assert_eq!(podcasts[0].stored_episodes, 2);
        assert_eq!(podcasts[0].announced_episodes.as_deref(), Some("3"));

        let episodes = db
            .episodes(MP.into(), Some("P1".into()), 10, 0)
            .await
            .unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].release_date.as_deref(), Some("2026-06-01"));

        // Paging and counting.
        assert_eq!(
            db.episodes(MP.into(), Some("P1".into()), 1, 1)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.episodes(MP.into(), Some("P1".into()), 10, 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.count_episodes(MP.into(), Some("P1".into()))
                .await
                .unwrap(),
            2
        );
        assert_eq!(db.count_episodes(MP.into(), None).await.unwrap(), 2);

        // Refresh without E2: it vanishes (soft delete), E3 appears.
        let (upserted, removed) = db
            .apply_episodes(
                MP.into(),
                "P1".into(),
                vec![episode("E1", "Folge 1"), episode("E3", "Folge 3")],
                no_recording(),
            )
            .await
            .unwrap();
        assert_eq!((upserted, removed), (2, 1));
        assert_eq!(
            db.episodes(MP.into(), Some("P1".into()), 10, 0)
                .await
                .unwrap()
                .len(),
            2
        );

        // Soft-deleting the parent takes the episodes with it.
        db.apply_page(MP.into(), vec![], vec!["P1".into()], default_log(), None)
            .await
            .unwrap();
        assert!(db.podcasts(vec![MP.to_owned()]).await.unwrap().is_empty());
        assert!(
            db.episodes(MP.into(), Some("P1".into()), 10, 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// An individually-subscribed `PodcastEpisode` living in `items` must
    /// never show up as a show in `podcasts list` (AUD-173) — the listing
    /// filters on the shared kind expression, not on `content_type` alone.
    #[tokio::test]
    async fn podcasts_listing_excludes_episode_items() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        let mut parent = item("P1", "Show");
        parent.doc = serde_json::json!({
            "asin": "P1", "title": "Show",
            "content_delivery_type": "PodcastParent", "content_type": "Podcast",
        })
        .to_string();
        let mut standalone = item("E9", "Einzelfolge");
        standalone.doc = serde_json::json!({
            "asin": "E9", "title": "Einzelfolge",
            "content_delivery_type": "PodcastEpisode", "content_type": "Podcast",
        })
        .to_string();
        db.apply_page(
            MP.into(),
            vec![parent, standalone],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        let podcasts = db.podcasts(vec![MP.to_owned()]).await.unwrap();
        let asins: Vec<&str> = podcasts.iter().map(|p| p.asin.as_str()).collect();
        assert_eq!(asins, ["P1"], "the standalone episode is not a show");
    }

    /// `search_episodes` (AUD-174): LIKE over full_title, parent title in
    /// the hit, soft-deleted episodes invisible.
    #[tokio::test]
    async fn search_episodes_matches_and_labels() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("P1", "Mein Podcast")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![
                episode("E1", "Folge Eins: Anfang"),
                episode("E2", "Folge Zwei: Ende"),
            ],
            no_recording(),
        )
        .await
        .unwrap();

        let hits = db
            .search_episodes(MP.into(), "anfang".into(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].asin, "E1");
        assert_eq!(hits[0].parent_title, "Mein Podcast");

        // A vanished (soft-deleted) episode no longer matches.
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E2", "Folge Zwei: Ende")],
            no_recording(),
        )
        .await
        .unwrap();
        assert!(
            db.search_episodes(MP.into(), "anfang".into(), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Episode resolution records added/changed/removed to the change_log
    /// (item_kind `episode`) when recording is on — and nothing when off
    /// (the initial scan).
    #[tokio::test]
    async fn apply_episodes_records_changes_when_enabled() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("P1", "Show")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // Initial fill without recording (the init scan): no log rows.
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E1", "Folge 1"), episode("E2", "Folge 2")],
            no_recording(),
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
        assert!(changes.is_empty(), "init scan records nothing: {changes:?}");

        // Recorded refresh: E2 vanishes, E3 appears, E1 changes its doc.
        let mut e1 = episode("E1", "Folge 1");
        e1.doc = serde_json::json!({
            "asin": "E1", "title": "Folge 1",
            "release_date": "2026-06-01", "runtime_length_min": 31,
        })
        .to_string();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![e1, episode("E3", "Folge 3")],
            ChangeRecording {
                record: true,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        let changes = db
            .list_changes(ChangeFilter {
                item_kinds: vec!["episode".into()],
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap();
        let mut summary: Vec<(String, String)> = changes
            .iter()
            .map(|c| (c.change.clone(), c.asin.clone()))
            .collect();
        summary.sort();
        assert_eq!(
            summary,
            [
                ("added".to_owned(), "E3".to_owned()),
                ("changed".to_owned(), "E1".to_owned()),
                ("removed".to_owned(), "E2".to_owned()),
            ]
        );
        assert!(changes.iter().all(|c| c.item_kind == "episode"));
        // The --kind filter separates episodes from item changes.
        assert!(
            db.list_changes(ChangeFilter {
                item_kinds: vec!["book".into()],
                limit: 0,
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty()
        );
    }
}
