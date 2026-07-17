//! Downloaded artifacts and content licenses: recording, lookups,
//! reorganize queries and removal.

use super::{Db, DbError, now_iso_utc};

/// A tracked download (one row of the `downloads` table).
#[derive(Debug, Clone)]
pub struct DownloadRecord {
    /// Item ASIN.
    pub asin: String,
    /// Asset kind (`audio`, `cover`, `chapter`, `pdf`, `annotation`).
    pub kind: String,
    /// Audible Content Reference (audio only).
    pub acr: Option<String>,
    /// Content format / quality (`AAX_44_128`, cover size, reencode target …).
    pub content_format: String,
    /// Audio form: `original` | `decrypted` | `reencoded` (`original` else).
    pub variant: String,
    /// Pre-request intent alias (AUD-93): `adrm-high`, `widevine-aac-normal`,
    /// `mpeg`, or `""` for non-audio artifacts. Keys the format-aware skip.
    pub request_kind: String,
    /// Content version.
    pub version: Option<String>,
    /// SKU.
    pub sku: Option<String>,
    /// Path of the downloaded file.
    pub file_path: String,
    /// File size in bytes.
    pub file_size: Option<u64>,
}

/// A recorded download for `download reorganize`: the fields needed to
/// recompute its new path and move the file.
#[derive(Debug, Clone)]
pub struct ReorgDownload {
    /// Item ASIN.
    pub asin: String,
    /// Asset kind (`audio`, `cover`, `chapter`, `pdf`).
    pub kind: String,
    /// Content format / quality discriminator.
    pub content_format: String,
    /// Audio form (`original` | `decrypted` | `reencoded`).
    pub variant: String,
    /// Current on-disk path (the authoritative old location).
    pub file_path: String,
}

/// One tracked download row, as seen by the `db downloads` commands.
#[derive(Debug, Clone)]
pub struct DownloadEntry {
    /// Item ASIN.
    pub asin: String,
    /// Marketplace this download belongs to.
    pub marketplace: String,
    /// Asset kind (`audio`, `cover`, …).
    pub kind: String,
    /// Content format / quality / size (part of the primary key).
    pub content_format: String,
    /// Audio form (`original` | `decrypted` | `reencoded`; part of the key).
    pub variant: String,
    /// Recorded file path.
    pub file_path: String,
    /// Recorded file size in bytes, if known.
    pub file_size: Option<u64>,
}

/// A stored content-license grant (one row of the `licenses` table).
#[derive(Debug, Clone)]
pub struct LicenseGrant {
    /// Item ASIN.
    pub asin: String,
    /// Content format / quality the grant is for (`AAX_44_128`, …).
    pub content_format: String,
    /// Pre-request intent alias (AUD-93): keys the format-aware reuse.
    pub request_kind: String,
    /// Expiry (`content_license.expiration_date`); `None` never expires.
    pub valid_until: Option<String>,
    /// Full licenserequest response (the voucher inside stays encrypted).
    pub doc: String,
}

/// Artifact kinds tracked in the `downloads` table (canonical order).
pub const DOWNLOAD_KINDS: [&str; 4] = ["audio", "chapter", "pdf", "cover"];

/// Audio forms tracked in the `variant` column (canonical order). Non-audio
/// artifacts always use `original`.
pub const DOWNLOAD_VARIANTS: [&str; 3] = ["original", "decrypted", "reencoded"];

impl Db {
    /// Records (or updates) a downloaded asset. The primary key is
    /// (asin, marketplace, kind, content_format, variant), so re-downloading
    /// the same quality updates the row (new acr/version → corrected release).
    pub async fn record_download(
        &self,
        marketplace: String,
        record: DownloadRecord,
    ) -> Result<(), DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            conn.execute(
                "INSERT INTO downloads(asin, marketplace, kind, acr, content_format, variant,
                                       request_kind, version, sku, file_path, file_size, status,
                                       downloaded_utc, updated_utc)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'downloaded', ?, ?)
                 ON CONFLICT(asin, marketplace, kind, content_format, variant) DO UPDATE SET
                   acr            = excluded.acr,
                   request_kind   = excluded.request_kind,
                   version        = excluded.version,
                   sku            = excluded.sku,
                   file_path      = excluded.file_path,
                   file_size      = excluded.file_size,
                   status         = 'downloaded',
                   updated_utc    = excluded.updated_utc",
                rusqlite::params![
                    record.asin,
                    marketplace,
                    record.kind,
                    record.acr,
                    record.content_format,
                    record.variant,
                    record.request_kind,
                    record.version,
                    record.sku,
                    record.file_path,
                    record.file_size.map(|n| n as i64),
                    now,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Updates a recorded download's on-disk path (used by `download
    /// reorganize` after moving the file).
    pub async fn update_download_path(
        &self,
        asin: String,
        marketplace: String,
        kind: String,
        content_format: String,
        variant: String,
        file_path: String,
    ) -> Result<(), DbError> {
        self.call(move |conn| {
            conn.execute(
                "UPDATE downloads SET file_path = ?, updated_utc = ?
                 WHERE asin = ? AND marketplace = ? AND kind = ? AND content_format = ?
                   AND variant = ?",
                rusqlite::params![
                    file_path,
                    now_iso_utc(),
                    asin,
                    marketplace,
                    kind,
                    content_format,
                    variant
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// All recorded downloads in a marketplace (for `download reorganize`).
    pub async fn reorg_downloads(
        &self,
        marketplace: String,
    ) -> Result<Vec<ReorgDownload>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT asin, kind, content_format, variant, file_path FROM downloads
                 WHERE marketplace = ? ORDER BY asin, kind, content_format, variant",
            )?;
            let rows = statement
                .query_map(rusqlite::params![marketplace], |row| {
                    Ok(ReorgDownload {
                        asin: row.get(0)?,
                        kind: row.get(1)?,
                        content_format: row.get(2)?,
                        variant: row.get(3)?,
                        file_path: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Stores (or refreshes) a granted content license.
    pub async fn upsert_license(
        &self,
        marketplace: String,
        grant: LicenseGrant,
    ) -> Result<(), DbError> {
        self.call(move |conn| {
            let now = now_iso_utc();
            conn.execute(
                "INSERT INTO licenses(asin, marketplace, content_format, request_kind, valid_until,
                                      doc, created_utc, updated_utc)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(asin, marketplace, content_format) DO UPDATE SET
                   request_kind = excluded.request_kind,
                   valid_until = excluded.valid_until,
                   doc         = excluded.doc,
                   updated_utc = excluded.updated_utc",
                rusqlite::params![
                    grant.asin,
                    marketplace,
                    grant.content_format,
                    grant.request_kind,
                    grant.valid_until,
                    grant.doc,
                    now,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// The most recently stored, still-valid license `doc` for an item
    /// in a marketplace (any content_format), if any. A license without
    /// an expiry never expires; `now` is an ISO-8601 UTC timestamp.
    /// The most recently stored, still-valid license `doc` for an item whose
    /// `request_kind` is one of `request_kinds` (AUD-93: format-aware reuse, so
    /// a resume picks the license for the format being requested, not just the
    /// newest). `None` if there is no matching, unexpired license.
    pub async fn find_valid_license(
        &self,
        asin: String,
        marketplace: String,
        request_kinds: Vec<String>,
        now: String,
    ) -> Result<Option<String>, DbError> {
        self.call(move |conn| {
            if request_kinds.is_empty() {
                return Ok(None);
            }
            let placeholders = vec!["?"; request_kinds.len()].join(",");
            let sql = format!(
                "SELECT doc FROM licenses
                 WHERE asin = ? AND marketplace = ?
                   AND request_kind IN ({placeholders})
                   AND (valid_until IS NULL OR valid_until > ?)
                 ORDER BY updated_utc DESC
                 LIMIT 1"
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&asin, &marketplace];
            for kind in &request_kinds {
                params.push(kind);
            }
            params.push(&now);
            conn.query_row(&sql, params.as_slice(), |row| row.get(0))
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other.into()),
                })
        })
        .await
    }

    /// The `request_kind`s already fully downloaded for `asin` (audio only) —
    /// the format-aware skip intersects these with a request's candidates.
    pub async fn downloaded_request_kinds(
        &self,
        asin: String,
        marketplace: String,
    ) -> Result<Vec<String>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT DISTINCT request_kind FROM downloads
                 WHERE asin = ? AND marketplace = ? AND kind = 'audio'
                   AND status = 'downloaded'",
            )?;
            let rows = statement
                .query_map(rusqlite::params![asin, marketplace], |row| {
                    row.get::<_, String>(0)
                })?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
        .await
    }

    pub async fn download_files(
        &self,
        asin: String,
        marketplace: String,
        kind: String,
    ) -> Result<Vec<String>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT file_path FROM downloads
                 WHERE asin = ? AND marketplace = ? AND kind = ? AND status = 'downloaded'",
            )?;
            let rows = statement
                .query_map(rusqlite::params![asin, marketplace, kind], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// The recorded download for an item, marketplace, kind and quality,
    /// if any.
    pub async fn download_record(
        &self,
        asin: String,
        marketplace: String,
        kind: String,
        content_format: String,
        variant: String,
    ) -> Result<Option<DownloadRecord>, DbError> {
        self.call(move |conn| {
            conn.query_row(
                "SELECT asin, kind, acr, content_format, variant, version, sku, file_path,
                        CAST(file_size AS INTEGER), request_kind
                 FROM downloads
                 WHERE asin = ? AND marketplace = ? AND kind = ? AND content_format = ? AND variant = ?",
                rusqlite::params![asin, marketplace, kind, content_format, variant],
                record_from_row,
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
        })
        .await
    }

    /// All tracked downloads, for the `db downloads` commands.
    pub async fn download_entries(&self) -> Result<Vec<DownloadEntry>, DbError> {
        self.call(|conn| {
            let mut statement = conn.prepare(
                "SELECT asin, marketplace, kind, content_format, variant, file_path,
                        CAST(file_size AS INTEGER)
                 FROM downloads ORDER BY asin, marketplace, kind, content_format, variant",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(DownloadEntry {
                        asin: row.get(0)?,
                        marketplace: row.get(1)?,
                        kind: row.get(2)?,
                        content_format: row.get(3)?,
                        variant: row.get(4)?,
                        file_path: row.get(5)?,
                        file_size: row.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Deletes the given download rows by their (asin, marketplace, kind,
    /// content_format, variant) key, in one transaction. Returns the count.
    ///
    /// A license belongs to the **original** audio it was granted for, and goes
    /// with it (AUD-217). The tables meet at `content_format`: `licenses` is
    /// keyed `(asin, marketplace, content_format)`, so each quality and DRM path
    /// has its own grant.
    ///
    /// Only the `original` carries one. A decrypted m4b is DRM-free and never
    /// needs it, and the grant's one lasting use is regenerating the aaxc's
    /// key sidecar should it be lost — its other use, reuse for a download,
    /// dies with the signed URL inside it after hours. So once the aaxc is
    /// gone, the grant serves nothing, whatever else was decrypted from it.
    ///
    /// **This also fires on `--decrypt --remove-source`**, which drops the
    /// original's record by this same path: the aaxc it insured no longer
    /// exists, so the grant goes with it rather than lingering as dead weight.
    pub async fn delete_downloads(
        &self,
        keys: Vec<(String, String, String, String, String)>,
    ) -> Result<usize, DbError> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let mut removed = 0;
            {
                let mut statement = tx.prepare_cached(
                    "DELETE FROM downloads
                     WHERE asin = ? AND marketplace = ? AND kind = ? AND content_format = ?
                       AND variant = ?",
                )?;
                let mut delete_license = tx.prepare_cached(
                    "DELETE FROM licenses
                     WHERE asin = ? AND marketplace = ? AND content_format = ?",
                )?;
                for (asin, marketplace, kind, content_format, variant) in &keys {
                    removed += statement.execute(rusqlite::params![
                        asin,
                        marketplace,
                        kind,
                        content_format,
                        variant
                    ])?;
                    if kind == "audio" && variant == "original" {
                        delete_license.execute(rusqlite::params![
                            asin,
                            marketplace,
                            content_format
                        ])?;
                    }
                }
            }
            tx.commit()?;
            Ok(removed)
        })
        .await
    }

    /// File paths recorded for an item's `kind` **and** `variant` (still on
    /// disk or not), e.g. the `original` aaxc to decrypt or the `decrypted`
    /// presence check.
    pub async fn download_files_variant(
        &self,
        asin: String,
        marketplace: String,
        kind: String,
        variant: String,
    ) -> Result<Vec<String>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT file_path FROM downloads
                 WHERE asin = ? AND marketplace = ? AND kind = ? AND variant = ?
                   AND status = 'downloaded'",
            )?;
            let rows = statement
                .query_map(rusqlite::params![asin, marketplace, kind, variant], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Full records for an item's `kind` **and** `variant` (still on disk or
    /// not) — the later-run decrypt reads the original audio's path together
    /// with its authoritative `content_format`/`request_kind` here (AUD-94).
    pub async fn download_records_variant(
        &self,
        asin: String,
        marketplace: String,
        kind: String,
        variant: String,
    ) -> Result<Vec<DownloadRecord>, DbError> {
        self.call(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT asin, kind, acr, content_format, variant, version, sku, file_path,
                        CAST(file_size AS INTEGER), request_kind
                 FROM downloads
                 WHERE asin = ? AND marketplace = ? AND kind = ? AND variant = ?
                   AND status = 'downloaded'",
            )?;
            let rows = statement
                .query_map(
                    rusqlite::params![asin, marketplace, kind, variant],
                    record_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}

/// Maps one `SELECT asin, kind, acr, content_format, variant, version, sku,
/// file_path, file_size, request_kind` row to a [`DownloadRecord`].
fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DownloadRecord> {
    Ok(DownloadRecord {
        asin: row.get(0)?,
        kind: row.get(1)?,
        acr: row.get(2)?,
        content_format: row.get(3)?,
        variant: row.get(4)?,
        request_kind: row.get(9)?,
        version: row.get(5)?,
        sku: row.get(6)?,
        file_path: row.get(7)?,
        file_size: row.get::<_, Option<i64>>(8)?.map(|n| n as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::{MP, open_temp};
    #[allow(unused_imports)]
    use crate::db::*;

    /// A license belongs to the aaxc it was granted for, and to nothing else: a
    /// decrypted m4b is DRM-free and never needs it. So it goes with the
    /// original, whatever was decrypted from it (AUD-217).
    #[tokio::test]
    async fn a_license_goes_with_the_original_it_belongs_to() {
        let (_dir, db) = open_temp().await;
        let audio = |format: &str, variant: &str| DownloadRecord {
            asin: "A1".into(),
            kind: "audio".into(),
            acr: None,
            content_format: format.into(),
            variant: variant.into(),
            request_kind: "adrm-high".into(),
            version: None,
            sku: None,
            file_path: format!("/dl/A1.{format}.{variant}"),
            file_size: None,
        };
        let grant = |format: &str| LicenseGrant {
            asin: "A1".into(),
            content_format: format.into(),
            request_kind: "adrm-high".into(),
            valid_until: None,
            doc: "{}".into(),
        };
        // One title, two qualities; the high one also decrypted.
        for record in [
            audio("AAX_44_128", "original"),
            audio("AAX_44_128", "decrypted"),
            audio("AAX_22_32", "original"),
        ] {
            db.record_download(MP.into(), record).await.unwrap();
        }
        for format in ["AAX_44_128", "AAX_22_32"] {
            db.upsert_license(MP.into(), grant(format)).await.unwrap();
        }

        let key = |format: &str, variant: &str| {
            (
                "A1".to_owned(),
                MP.to_owned(),
                "audio".to_owned(),
                format.to_owned(),
                variant.to_owned(),
            )
        };

        // Dropping the m4b touches no grant — it never carried one.
        db.delete_downloads(vec![key("AAX_44_128", "decrypted")])
            .await
            .unwrap();
        assert_eq!(db.stats().await.unwrap().licenses, 2);

        // Dropping the aaxc takes that format's grant with it — and only that
        // one: the other quality is a separate grant, untouched.
        db.delete_downloads(vec![key("AAX_44_128", "original")])
            .await
            .unwrap();
        assert_eq!(db.stats().await.unwrap().licenses, 1);

        // Even with an m4b of that format still around, the grant is gone: it
        // insured the aaxc, and the aaxc is what left. This is also what
        // `--decrypt --remove-source` does, by this same path.
        db.record_download(MP.into(), audio("AAX_44_128", "decrypted"))
            .await
            .unwrap();
        db.upsert_license(MP.into(), grant("AAX_44_128"))
            .await
            .unwrap();
        db.delete_downloads(vec![key("AAX_44_128", "original")])
            .await
            .unwrap();
        assert_eq!(db.stats().await.unwrap().licenses, 1, "only AAX_22_32 left");

        // A non-audio artifact never carries a grant away.
        db.record_download(
            MP.into(),
            DownloadRecord {
                kind: "cover".into(),
                content_format: "500".into(),
                file_path: "/dl/A1.jpg".into(),
                ..audio("AAX_22_32", "original")
            },
        )
        .await
        .unwrap();
        db.delete_downloads(vec![(
            "A1".to_owned(),
            MP.to_owned(),
            "cover".to_owned(),
            "500".to_owned(),
            "original".to_owned(),
        )])
        .await
        .unwrap();
        assert_eq!(db.stats().await.unwrap().licenses, 1);

        // …and the survivor is the other quality's: dropping its audio clears
        // the last one, so the grant that stayed was never the wrong one.
        db.delete_downloads(vec![key("AAX_22_32", "original")])
            .await
            .unwrap();
        assert_eq!(db.stats().await.unwrap().licenses, 0);
    }

    #[tokio::test]
    async fn reorg_paths_roundtrip() {
        let (_dir, db) = open_temp().await;
        db.record_download(
            MP.into(),
            DownloadRecord {
                asin: "A1".into(),
                kind: "cover".into(),
                acr: None,
                content_format: "500".into(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: "/old/A1.cover_500.jpg".into(),
                file_size: Some(10),
            },
        )
        .await
        .unwrap();

        let rows = db.reorg_downloads(MP.into()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "/old/A1.cover_500.jpg");
        assert_eq!(rows[0].content_format, "500");

        db.update_download_path(
            "A1".into(),
            MP.into(),
            "cover".into(),
            "500".into(),
            "original".into(),
            "/new/A1.cover_500.jpg".into(),
        )
        .await
        .unwrap();
        let rows = db.reorg_downloads(MP.into()).await.unwrap();
        assert_eq!(rows[0].file_path, "/new/A1.cover_500.jpg");

        // Annotation path: none until saved, then round-trips.
        db.upsert_annotation(MP.into(), "A1".into(), Some("{}".into()), "ok".into())
            .await
            .unwrap();
        assert!(db.reorg_annotations(MP.into()).await.unwrap().is_empty());
        db.set_annotation_path(MP.into(), "A1".into(), "/old/A1.annot".into())
            .await
            .unwrap();
        assert_eq!(
            db.reorg_annotations(MP.into()).await.unwrap(),
            vec![("A1".to_owned(), "/old/A1.annot".to_owned())]
        );
    }

    #[tokio::test]
    async fn records_and_finds_downloads_per_kind() {
        let (_dir, db) = open_temp().await;

        let audio = DownloadRecord {
            asin: "A1".into(),
            kind: "audio".into(),
            acr: Some("CR!X".into()),
            content_format: "AAX_44_128".into(),
            variant: "original".into(),
            request_kind: String::new(),
            version: Some("63671221".into()),
            sku: Some("BK_X".into()),
            file_path: "/dl/A1.aaxc".into(),
            file_size: Some(123),
        };
        let chapter = DownloadRecord {
            asin: "A1".into(),
            kind: "chapter".into(),
            acr: None,
            content_format: String::new(),
            variant: "original".into(),
            request_kind: String::new(),
            version: None,
            sku: None,
            file_path: "/dl/A1.chapters.json".into(),
            file_size: Some(7),
        };
        db.record_download(MP.into(), audio.clone()).await.unwrap();
        db.record_download(MP.into(), chapter).await.unwrap();

        assert_eq!(
            db.download_files("A1".into(), MP.into(), "audio".into())
                .await
                .unwrap(),
            vec!["/dl/A1.aaxc".to_owned()]
        );
        assert_eq!(
            db.download_files("A1".into(), MP.into(), "chapter".into())
                .await
                .unwrap(),
            vec!["/dl/A1.chapters.json".to_owned()]
        );
        // No PDF recorded for this item.
        assert!(
            db.download_files("A1".into(), MP.into(), "pdf".into())
                .await
                .unwrap()
                .is_empty()
        );

        // Re-recording the same (asin, marketplace, kind, content_format) updates
        // in place instead of duplicating the row.
        let mut moved = audio;
        moved.file_path = "/dl/new/A1.aaxc".into();
        db.record_download(MP.into(), moved).await.unwrap();
        assert_eq!(
            db.download_files("A1".into(), MP.into(), "audio".into())
                .await
                .unwrap(),
            vec!["/dl/new/A1.aaxc".to_owned()]
        );
    }

    #[tokio::test]
    async fn download_records_variant_carries_the_request_kind() {
        let (_dir, db) = open_temp().await;

        let record = |format: &str, variant: &str, request_kind: &str, path: &str| DownloadRecord {
            asin: "A1".into(),
            kind: "audio".into(),
            acr: None,
            content_format: format.into(),
            variant: variant.into(),
            request_kind: request_kind.into(),
            version: None,
            sku: None,
            file_path: path.into(),
            file_size: None,
        };
        // Two coexisting original formats (AUD-93) and their two decrypted
        // variants — one per content_format (AUD-95).
        for row in [
            record(
                "AAX_44_128",
                "original",
                "adrm-high",
                "/dl/A1.AAX_44_128.aaxc",
            ),
            record(
                "AAC_44_131",
                "original",
                "widevine-aac-normal",
                "/dl/A1.AAC_44_131.cenc",
            ),
            record(
                "AAX_44_128",
                "decrypted",
                "adrm-high",
                "/dl/A1.AAX_44_128.m4b",
            ),
            record(
                "AAC_44_131",
                "decrypted",
                "widevine-aac-normal",
                "/dl/A1.AAC_44_131.m4a",
            ),
        ] {
            db.record_download(MP.into(), row).await.unwrap();
        }

        let mut originals = db
            .download_records_variant("A1".into(), MP.into(), "audio".into(), "original".into())
            .await
            .unwrap();
        originals.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        assert_eq!(originals.len(), 2);
        assert_eq!(originals[0].request_kind, "widevine-aac-normal");
        assert_eq!(originals[0].content_format, "AAC_44_131");
        assert_eq!(originals[1].request_kind, "adrm-high");
        assert_eq!(originals[1].file_path, "/dl/A1.AAX_44_128.aaxc");

        let mut decrypted = db
            .download_records_variant("A1".into(), MP.into(), "audio".into(), "decrypted".into())
            .await
            .unwrap();
        decrypted.sort_by(|a, b| a.content_format.cmp(&b.content_format));
        assert_eq!(decrypted.len(), 2);
        assert_eq!(decrypted[0].content_format, "AAC_44_131");
        assert_eq!(decrypted[1].content_format, "AAX_44_128");
        assert_eq!(decrypted[1].request_kind, "adrm-high");

        // Other items stay empty.
        assert!(
            db.download_records_variant("B2".into(), MP.into(), "audio".into(), "original".into())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stores_and_reuses_valid_licenses() {
        let (_dir, db) = open_temp().await;

        let grant = |fmt: &str, valid_until: Option<&str>, marker: &str| LicenseGrant {
            asin: "A1".into(),
            content_format: fmt.into(),
            request_kind: fmt.into(),
            valid_until: valid_until.map(str::to_owned),
            doc: serde_json::json!({ "marker": marker }).to_string(),
        };
        let both = || vec!["AAX_44_128".to_owned(), "AAX_22_64".to_owned()];

        // A far-future license and an already-expired one.
        db.upsert_license(
            MP.into(),
            grant("AAX_44_128", Some("3000-01-01T00:00:00Z"), "good"),
        )
        .await
        .unwrap();
        db.upsert_license(
            MP.into(),
            grant("AAX_22_64", Some("2000-01-01T00:00:00Z"), "stale"),
        )
        .await
        .unwrap();

        let now = "2026-06-11T00:00:00Z".to_owned();
        let doc = db
            .find_valid_license("A1".into(), MP.into(), both(), now.clone())
            .await
            .unwrap()
            .expect("a valid license exists");
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(value["marker"], "good", "the expired one is not returned");

        // No license for another item.
        assert!(
            db.find_valid_license("B2".into(), MP.into(), both(), now.clone())
                .await
                .unwrap()
                .is_none()
        );

        // Re-upserting the same (asin, marketplace, content_format) updates in place.
        db.upsert_license(
            MP.into(),
            grant("AAX_44_128", Some("3000-01-01T00:00:00Z"), "good2"),
        )
        .await
        .unwrap();
        let doc = db
            .find_valid_license("A1".into(), MP.into(), both(), now)
            .await
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(value["marker"], "good2");
    }

    #[tokio::test]
    async fn lists_and_deletes_download_entries() {
        let (_dir, db) = open_temp().await;
        let rec =
            |asin: &str, kind: &str, fmt: &str, path: &str, size: Option<u64>| DownloadRecord {
                asin: asin.into(),
                kind: kind.into(),
                acr: None,
                content_format: fmt.into(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: path.into(),
                file_size: size,
            };
        db.record_download(
            MP.into(),
            rec("A1", "audio", "AAX_44_128", "/dl/A1.aaxc", Some(123)),
        )
        .await
        .unwrap();
        db.record_download(
            MP.into(),
            rec("A1", "chapter", "tree", "/dl/A1.chap.json", Some(7)),
        )
        .await
        .unwrap();
        db.record_download(
            MP.into(),
            rec("A2", "cover", "500", "/dl/A2.cover.jpg", None),
        )
        .await
        .unwrap();

        assert_eq!(db.download_entries().await.unwrap().len(), 3);

        let removed = db
            .delete_downloads(vec![
                (
                    "A1".into(),
                    MP.into(),
                    "audio".into(),
                    "AAX_44_128".into(),
                    "original".into(),
                ),
                (
                    "A2".into(),
                    MP.into(),
                    "cover".into(),
                    "500".into(),
                    "original".into(),
                ),
                // unknown: ignored
                (
                    "ZZ".into(),
                    MP.into(),
                    "audio".into(),
                    "".into(),
                    "original".into(),
                ),
            ])
            .await
            .unwrap();
        assert_eq!(removed, 2);

        let rest = db.download_entries().await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].kind, "chapter");
        assert_eq!(rest[0].file_size, Some(7));
    }
}
