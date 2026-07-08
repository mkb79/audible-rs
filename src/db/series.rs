//! Series memberships: the overview and per-series item listings.

use super::{Db, DbError, in_placeholders};

/// One series with owned volumes.
#[derive(Debug, Clone)]
pub struct SeriesRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// ASIN of the series.
    pub series_asin: String,
    /// Series title.
    pub series_title: String,
    /// Number of owned volumes.
    pub owned: u64,
    /// Owned sequences, sorted (`"1,1-6,?"`).
    pub sequences: Option<String>,
}

/// One owned volume of a series.
#[derive(Debug, Clone)]
pub struct SeriesItemRow {
    /// Marketplace the row belongs to.
    pub marketplace: String,
    /// ASIN of the series.
    pub series_asin: String,
    /// Series title.
    pub series_title: String,
    /// Position within the series (`"?"` when unknown).
    pub sequence: String,
    /// ASIN of the owned item.
    pub asin: String,
    /// Title of the owned item.
    pub full_title: String,
}

impl Db {
    /// All series with owned-volume counts and their sequences across the
    /// marketplace set. A series present in several marketplaces yields
    /// one row per marketplace.
    pub async fn series_overview(
        &self,
        marketplaces: Vec<String>,
    ) -> Result<Vec<SeriesRow>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT s.marketplace, s.series_asin, s.series_title, COUNT(*),
                        GROUP_CONCAT(COALESCE(NULLIF(s.sequence, ''), '?')
                                     ORDER BY CAST(s.sequence AS REAL), s.sequence)
                 FROM item_series s
                 JOIN items i ON i.asin = s.item_asin AND i.marketplace = s.marketplace
                              AND i.is_deleted = 0
                 WHERE s.marketplace IN ({})
                 GROUP BY s.marketplace, s.series_asin, s.series_title
                 ORDER BY s.series_title, s.marketplace",
                in_placeholders(marketplaces.len())
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = marketplaces
                .iter()
                .map(|m| m as &dyn rusqlite::ToSql)
                .collect();
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(SeriesRow {
                        marketplace: row.get(0)?,
                        series_asin: row.get(1)?,
                        series_title: row.get(2)?,
                        owned: row.get::<_, i64>(3)? as u64,
                        sequences: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Owned volumes of one series across the marketplace set (matched by
    /// ASIN or title substring).
    pub async fn series_items(
        &self,
        marketplaces: Vec<String>,
        needle: String,
    ) -> Result<Vec<SeriesItemRow>, DbError> {
        self.call(move |conn| {
            if marketplaces.is_empty() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT s.marketplace, s.series_asin, s.series_title,
                        COALESCE(NULLIF(s.sequence, ''), '?'), i.asin, i.full_title
                 FROM item_series s
                 JOIN items i ON i.asin = s.item_asin AND i.marketplace = s.marketplace
                              AND i.is_deleted = 0
                 WHERE s.marketplace IN ({})
                   AND (s.series_asin = ?
                        OR lower(s.series_title) LIKE '%' || lower(?) || '%')
                 ORDER BY s.series_title, s.marketplace,
                          CAST(s.sequence AS REAL), s.sequence, i.full_title",
                in_placeholders(marketplaces.len())
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(marketplaces.len() + 2);
            for marketplace in &marketplaces {
                params.push(marketplace);
            }
            params.push(&needle);
            params.push(&needle);
            let rows = statement
                .query_map(params.as_slice(), |row| {
                    Ok(SeriesItemRow {
                        marketplace: row.get(0)?,
                        series_asin: row.get(1)?,
                        series_title: row.get(2)?,
                        sequence: row.get(3)?,
                        asin: row.get(4)?,
                        full_title: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
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
    async fn series_membership_roundtrip() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();

        let mut omnibus = item("A1", "Andromeda Komplett");
        omnibus.series = vec![
            SeriesRef {
                series_asin: "S1".into(),
                series_title: "Taschenbuch".into(),
                sequence: Some("1".into()),
            },
            SeriesRef {
                series_asin: "S2".into(),
                series_title: "Andromeda".into(),
                sequence: Some("1-6".into()),
            },
        ];
        let mut volume3 = item("A2", "Taschenbuch Band 3");
        volume3.series = vec![SeriesRef {
            series_asin: "S1".into(),
            series_title: "Taschenbuch".into(),
            sequence: Some("3".into()),
        }];
        let mut unnumbered = item("A3", "Ohne Nummer");
        unnumbered.series = vec![SeriesRef {
            series_asin: "S1".into(),
            series_title: "Taschenbuch".into(),
            sequence: None,
        }];
        db.apply_page(
            MP.into(),
            vec![omnibus, volume3, unnumbered],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        let overview = db.series_overview(vec![MP.to_owned()]).await.unwrap();
        assert_eq!(overview.len(), 2);
        let taschenbuch = overview.iter().find(|s| s.series_asin == "S1").unwrap();
        assert_eq!(taschenbuch.owned, 3);
        assert_eq!(taschenbuch.sequences.as_deref(), Some("?,1,3"));

        // Lookup by title substring; multi-series item appears in both.
        let items = db
            .series_items(vec![MP.to_owned()], "andromeda".into())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sequence, "1-6");
        let items = db
            .series_items(vec![MP.to_owned()], "S1".into())
            .await
            .unwrap();
        assert_eq!(items.len(), 3);

        // Soft-deleting an item removes it from the series view.
        db.apply_page(MP.into(), vec![], vec!["A2".into()], default_log(), None)
            .await
            .unwrap();
        let overview = db.series_overview(vec![MP.to_owned()]).await.unwrap();
        let taschenbuch = overview.iter().find(|s| s.series_asin == "S1").unwrap();
        assert_eq!(taschenbuch.owned, 2);
    }
}
