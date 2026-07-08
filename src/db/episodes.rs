//! Podcast parents and their episodes: episode application and the
//! podcast/episode listings.

use super::{Db, DbError, in_placeholders, now_iso_utc};

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

impl Db {
    /// Replaces the episode set of one podcast parent: upserts the given
    /// episodes and soft-deletes episodes that vanished from the feed.
    /// Returns (upserted, soft-deleted).
    pub async fn apply_episodes(
        &self,
        marketplace: String,
        parent_asin: String,
        episodes: Vec<UpsertEpisode>,
    ) -> Result<(usize, usize), DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            let tx = conn.transaction()?;

            {
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
                }
            }

            // Episodes of this parent that are no longer in the feed.
            let existing: Vec<String> = tx
                .prepare_cached(
                    "SELECT asin FROM episodes
                     WHERE parent_asin = ? AND marketplace = ? AND is_deleted = 0",
                )?
                .query_map(rusqlite::params![parent_asin, marketplace], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            let fresh: std::collections::BTreeSet<&str> =
                episodes.iter().map(|e| e.asin.as_str()).collect();
            let mut removed = 0usize;
            {
                let mut soft_delete = tx.prepare_cached(
                    "UPDATE episodes SET is_deleted = 1, deleted_utc = ?, updated_utc = ?
                     WHERE asin = ? AND marketplace = ?",
                )?;
                for asin in existing {
                    if !fresh.contains(asin.as_str()) {
                        soft_delete.execute(rusqlite::params![now, now, asin, marketplace])?;
                        removed += 1;
                    }
                }
            }

            tx.commit()?;
            Ok((episodes.len(), removed))
        })
        .await
    }

    /// Active podcast parents across the marketplace set, with their
    /// stored episode counts.
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
                   AND (json_extract(i.doc, '$.content_delivery_type')
                          IN ('PodcastParent', 'Periodical')
                        OR json_extract(i.doc, '$.content_type') = 'Podcast')
                 ORDER BY i.full_title, i.marketplace",
                in_placeholders(marketplaces.len())
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
}

#[cfg(test)]
mod tests {

    use crate::db::test_util::{MP, default_log, episode, item, open_temp};
    #[allow(unused_imports)]
    use crate::db::*;

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
}
