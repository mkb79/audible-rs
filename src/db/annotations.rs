//! Per-item annotations: upserts, saved-file paths and inventories.

use rusqlite::OptionalExtension;

use super::{Db, DbError, now_iso_utc};

/// A stored annotation document and when it was last fetched.
#[derive(Debug, Clone)]
pub struct AnnotationDoc {
    /// The last annotation response (None when the title has none).
    pub doc: Option<String>,
    /// `ok` (has annotations) or `none` (a 404 — none, recorded as synced).
    pub status: String,
    /// When it was last fetched (ISO UTC).
    pub fetched_utc: String,
}

/// One item's annotation state for the `annotations list` inventory.
#[derive(Debug, Clone)]
pub struct AnnotationStatus {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// Item ASIN.
    pub asin: String,
    /// `Title: Subtitle`.
    pub full_title: String,
    /// `ok` | `none`, or `None` when never synced.
    pub status: Option<String>,
    /// When last synced, if ever (ISO UTC).
    pub fetched_utc: Option<String>,
}

impl Db {
    /// File paths of all recorded downloads for an item, marketplace and
    /// kind (any content_format). Used to decide whether an artifact
    /// already exists before re-fetching it.
    /// Inserts or refreshes an item's annotation: `doc` is the response
    /// payload (None when the title has none), `status` is `ok` or `none`.
    pub async fn upsert_annotation(
        &self,
        marketplace: String,
        asin: String,
        doc: Option<String>,
        status: String,
    ) -> Result<(), DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            conn.execute(
                "INSERT INTO annotations(asin, marketplace, doc, status, fetched_utc)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(asin, marketplace) DO UPDATE SET
                   doc = excluded.doc, status = excluded.status, fetched_utc = excluded.fetched_utc",
                rusqlite::params![asin, marketplace, doc, status, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Records (or updates) the `.annot` file path for an item — set by
    /// `annotations --save` and by `download reorganize` after a move. The row
    /// already exists (the annotation was upserted first).
    pub async fn set_annotation_path(
        &self,
        marketplace: String,
        asin: String,
        file_path: String,
    ) -> Result<(), DbError> {
        self.call(move |conn| {
            conn.execute(
                "UPDATE annotations SET file_path = ? WHERE asin = ? AND marketplace = ?",
                rusqlite::params![file_path, asin, marketplace],
            )?;
            Ok(())
        })
        .await
    }

    /// Recorded `.annot` files in a marketplace (asin, path) for `download
    /// reorganize` — only rows that were actually saved (`file_path` set).
    pub async fn reorg_annotations(
        &self,
        marketplace: String,
    ) -> Result<Vec<(String, String)>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT asin, file_path FROM annotations
                 WHERE marketplace = ? AND file_path IS NOT NULL ORDER BY asin",
            )?;
            let rows = statement
                .query_map(rusqlite::params![marketplace], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// The stored annotation for an item, taken from the first of
    /// `marketplaces` that has one (with the marketplace it was found in).
    pub async fn annotation_doc(
        &self,
        asin: String,
        marketplaces: Vec<String>,
    ) -> Result<Option<(String, AnnotationDoc)>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT doc, status, fetched_utc FROM annotations
                 WHERE asin = ? AND marketplace = ?",
            )?;
            for marketplace in &marketplaces {
                let found = statement
                    .query_row(rusqlite::params![asin, marketplace], |row| {
                        Ok(AnnotationDoc {
                            doc: row.get(0)?,
                            status: row.get(1)?,
                            fetched_utc: row.get(2)?,
                        })
                    })
                    .optional()?;
                if let Some(doc) = found {
                    return Ok(Some((marketplace.clone(), doc)));
                }
            }
            Ok(None)
        })
        .await
    }

    /// ASINs to sync annotations for in one marketplace: all active items, or
    /// (when `only_missing`) only those without an annotations row yet — a
    /// `none` row counts as synced and is skipped.
    pub async fn annotation_target_asins(
        &self,
        marketplace: String,
        only_missing: bool,
    ) -> Result<Vec<String>, DbError> {
        self.call(move |conn| {
            let sql = if only_missing {
                "SELECT i.asin FROM items i
                 LEFT JOIN annotations a ON a.asin = i.asin AND a.marketplace = i.marketplace
                 WHERE i.marketplace = ? AND i.is_deleted = 0 AND a.asin IS NULL
                 ORDER BY i.full_title COLLATE NOCASE, i.asin"
            } else {
                "SELECT asin FROM items
                 WHERE marketplace = ? AND is_deleted = 0
                 ORDER BY full_title COLLATE NOCASE, asin"
            };
            let mut statement = conn.prepare_cached(sql)?;
            let rows = statement
                .query_map(rusqlite::params![marketplace], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Inventory for `annotations list`: every active item with its annotation
    /// status (`None` = never synced), over the given marketplaces.
    pub async fn annotation_inventory(
        &self,
        marketplaces: Vec<String>,
    ) -> Result<Vec<AnnotationStatus>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT i.asin, i.full_title, a.status, a.fetched_utc
                 FROM items i
                 LEFT JOIN annotations a ON a.asin = i.asin AND a.marketplace = i.marketplace
                 WHERE i.marketplace = ? AND i.is_deleted = 0
                 ORDER BY i.full_title COLLATE NOCASE, i.asin",
            )?;
            let mut out = Vec::new();
            for marketplace in &marketplaces {
                let rows = statement.query_map(rusqlite::params![marketplace], |row| {
                    Ok(AnnotationStatus {
                        marketplace: marketplace.clone(),
                        asin: row.get(0)?,
                        full_title: row.get(1)?,
                        status: row.get(2)?,
                        fetched_utc: row.get(3)?,
                    })
                })?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
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
    async fn annotations_roundtrip_and_missing() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![
                item("A1", "Book One"),
                item("A2", "Book Two"),
                item("A3", "Book Three"),
            ],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // Nothing synced yet: all three are "missing".
        assert_eq!(
            db.annotation_target_asins(MP.into(), true)
                .await
                .unwrap()
                .len(),
            3
        );

        // A1 has annotations, A2 has none (a 404 — recorded as synced).
        db.upsert_annotation(
            MP.into(),
            "A1".into(),
            Some(r#"{"x":1}"#.into()),
            "ok".into(),
        )
        .await
        .unwrap();
        db.upsert_annotation(MP.into(), "A2".into(), None, "none".into())
            .await
            .unwrap();

        // --missing now only A3 ("none" counts as synced); --all stays 3.
        assert_eq!(
            db.annotation_target_asins(MP.into(), true).await.unwrap(),
            vec!["A3".to_owned()]
        );
        assert_eq!(
            db.annotation_target_asins(MP.into(), false)
                .await
                .unwrap()
                .len(),
            3
        );

        // Doc lookup: ok carries the doc, none is empty, unknown is None.
        let (mp, ok) = db
            .annotation_doc("A1".into(), vec![MP.to_owned()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mp, MP);
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.doc.as_deref(), Some(r#"{"x":1}"#));
        let (_, none) = db
            .annotation_doc("A2".into(), vec![MP.to_owned()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(none.status, "none");
        assert!(none.doc.is_none());
        assert!(
            db.annotation_doc("A3".into(), vec![MP.to_owned()])
                .await
                .unwrap()
                .is_none()
        );

        // Inventory covers every item; A3 is "never synced" (None).
        let inventory = db.annotation_inventory(vec![MP.to_owned()]).await.unwrap();
        assert_eq!(inventory.len(), 3);
        let status = |asin: &str| {
            inventory
                .iter()
                .find(|row| row.asin == asin)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(status("A1").as_deref(), Some("ok"));
        assert_eq!(status("A2").as_deref(), Some("none"));
        assert_eq!(status("A3"), None);

        // A re-sync overwrites (always fresh).
        db.upsert_annotation(
            MP.into(),
            "A1".into(),
            Some(r#"{"x":2}"#.into()),
            "ok".into(),
        )
        .await
        .unwrap();
        let (_, updated) = db
            .annotation_doc("A1".into(), vec![MP.to_owned()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.doc.as_deref(), Some(r#"{"x":2}"#));
    }
}
