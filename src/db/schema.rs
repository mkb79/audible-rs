//! Library database schema, ported from the Python reference branch
//! `feature/db-library` (D9). Versioned via `PRAGMA user_version`;
//! there are no pre-existing databases, so v1 is created in one step
//! (the reference's column-probing migrations are dropped on purpose).

use rusqlite::Connection;

/// Current schema version. Stays at 1 until release: pre-release schema
/// changes recreate the (reproducible, re-syncable) database instead of
/// growing a migration ladder here.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = r#"
-- One database per account (one user_id). Every row carries its
-- `marketplace`; the key is (asin, marketplace) because the same asin can
-- in principle exist in more than one marketplace's library.
CREATE TABLE items (
  asin        TEXT NOT NULL,
  marketplace TEXT NOT NULL,
  doc         TEXT NOT NULL,
  title       TEXT NOT NULL,
  subtitle    TEXT,
  full_title  TEXT NOT NULL,
  updated_utc TEXT NOT NULL,
  is_deleted  INTEGER NOT NULL DEFAULT 0,
  deleted_utc TEXT,
  PRIMARY KEY (asin, marketplace)
);

-- Per-marketplace sync state (response_groups pinned + continuation token).
CREATE TABLE sync_state (
  marketplace          TEXT PRIMARY KEY,
  response_groups      TEXT NOT NULL,
  last_state_token_utc TEXT,
  last_state_token_raw TEXT,
  created_utc          TEXT NOT NULL
);

CREATE TABLE sync_log (
  id                        INTEGER PRIMARY KEY AUTOINCREMENT,
  marketplace               TEXT NOT NULL,
  request_time_utc          TEXT NOT NULL,
  request_state_token_utc   TEXT,
  response_time_utc         TEXT NOT NULL,
  response_state_token_utc  TEXT,
  http_status               INTEGER,
  -- "upserted" is split into newly added vs. changed (an existing item whose
  -- document differs beyond the volatile keys); soft-deleted are removals.
  num_added                 INTEGER DEFAULT 0,
  num_changed               INTEGER DEFAULT 0,
  num_soft_deleted          INTEGER DEFAULT 0,
  note                      TEXT,
  added_asins               TEXT,
  changed_asins             TEXT,
  soft_deleted_asins        TEXT
);

CREATE INDEX idx_sync_log_marketplace ON sync_log (marketplace);

-- Per-item change history (AUD-64): one row per added/changed/removed item per
-- non-initial sync, so changes can be reviewed later via `library changes`.
-- `changed` holds the top-level field diff WITH values for kind='changed'
-- (JSON [{key, old, new}], VOLATILE_KEYS filtered); NULL for added/removed.
-- `sync_id` correlates to sync_log (no FK: the log has its own retention).
CREATE TABLE change_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  sync_id      INTEGER,
  recorded_utc TEXT NOT NULL,
  marketplace  TEXT NOT NULL,
  asin         TEXT NOT NULL,
  full_title   TEXT NOT NULL,
  mode         TEXT NOT NULL,   -- 'full' | 'delta'
  kind         TEXT NOT NULL,   -- 'added' | 'changed' | 'removed'
  item_kind    TEXT NOT NULL DEFAULT 'book', -- 'book' | 'podcast' | 'episode' (AUD-173)
  changed      TEXT             -- JSON [{key, old, new}] (kind='changed' only)
);

CREATE INDEX idx_change_log_recorded ON change_log (recorded_utc);
CREATE INDEX idx_change_log_asin     ON change_log (asin, marketplace);

CREATE VIEW v_books AS
SELECT
  asin,
  marketplace,
  title,
  subtitle,
  full_title,
  COALESCE(
    json_extract(doc, '$.purchase_date'),
    json_extract(doc, '$.library_status.date_added')
  ) AS purchase_date,
  COALESCE(
    json_extract(doc, '$.language'),
    json_extract(doc, '$.metadata.language')
  ) AS language,
  COALESCE(
    json_extract(doc, '$.runtime_length_min'),
    json_extract(doc, '$.duration_min')
  ) AS runtime_min,
  json_extract(doc, '$.is_ayce') AS is_ayce,
  -- Content kind for the shared --kind filter (AUD-173). SQL twin of
  -- models::library::item_kind — kept in lockstep by a functional test.
  CASE
    WHEN json_extract(doc, '$.content_delivery_type') = 'PodcastEpisode'
      THEN 'episode'
    WHEN json_extract(doc, '$.content_delivery_type')
           IN ('PodcastParent', 'Periodical', 'PodcastSeason')
         OR json_extract(doc, '$.content_type') = 'Podcast'
      THEN 'podcast'
    ELSE 'book'
  END AS kind
FROM items
WHERE is_deleted = 0;

CREATE INDEX idx_items_marketplace ON items (marketplace);
CREATE INDEX idx_items_title       ON items (lower(title));
CREATE INDEX idx_items_subtitle    ON items (lower(subtitle));
CREATE INDEX idx_items_full_title  ON items (lower(full_title));
CREATE INDEX idx_items_is_deleted  ON items (is_deleted);

CREATE INDEX idx_items_purchase    ON items (
  COALESCE(json_extract(doc,'$.purchase_date'),
           json_extract(doc,'$.library_status.date_added'))
);
CREATE INDEX idx_items_language    ON items (
  COALESCE(json_extract(doc,'$.language'),
           json_extract(doc,'$.metadata.language'))
);

CREATE VIRTUAL TABLE items_fts USING fts5(
  full_title,
  title,
  subtitle,
  asin UNINDEXED,
  content='items',
  content_rowid='rowid'
);

CREATE TRIGGER trg_items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, full_title, title, subtitle, asin)
  VALUES (new.rowid, new.full_title, new.title, new.subtitle, new.asin);
END;

CREATE TRIGGER trg_items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, full_title, title, subtitle, asin)
  VALUES('delete', old.rowid, old.full_title, old.title, old.subtitle, old.asin);
END;

CREATE TRIGGER trg_items_au AFTER UPDATE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, full_title, title, subtitle, asin)
  VALUES('delete', old.rowid, old.full_title, old.title, old.subtitle, old.asin);
  INSERT INTO items_fts(rowid, full_title, title, subtitle, asin)
  VALUES (new.rowid, new.full_title, new.title, new.subtitle, new.asin);
END;

-- Podcast episodes live outside items: own lifecycle (coupled to their
-- parent), own volume profile. Keyed by (asin, marketplace); cascades from
-- the parent item in the same marketplace.
CREATE TABLE episodes (
  asin        TEXT NOT NULL,
  marketplace TEXT NOT NULL,
  parent_asin TEXT NOT NULL,
  doc         TEXT NOT NULL,
  title       TEXT NOT NULL,
  subtitle    TEXT,
  full_title  TEXT NOT NULL,
  updated_utc TEXT NOT NULL,
  is_deleted  INTEGER NOT NULL DEFAULT 0,
  deleted_utc TEXT,
  PRIMARY KEY (asin, marketplace),
  FOREIGN KEY (parent_asin, marketplace)
    REFERENCES items(asin, marketplace) ON DELETE CASCADE
);

CREATE INDEX idx_episodes_parent     ON episodes (parent_asin, marketplace);
CREATE INDEX idx_episodes_is_deleted ON episodes (is_deleted);

CREATE VIEW v_episodes AS
SELECT
  asin,
  marketplace,
  parent_asin,
  title,
  subtitle,
  full_title,
  COALESCE(
    json_extract(doc, '$.release_date'),
    json_extract(doc, '$.issue_date')
  ) AS release_date,
  COALESCE(
    json_extract(doc, '$.runtime_length_min'),
    json_extract(doc, '$.duration_min')
  ) AS runtime_min
FROM episodes
WHERE is_deleted = 0;

-- Series memberships, extracted at upsert time (an item can belong to
-- several series; sequence may be empty or a range like "1-6").
CREATE TABLE item_series (
  item_asin    TEXT NOT NULL,
  marketplace  TEXT NOT NULL,
  series_asin  TEXT NOT NULL,
  series_title TEXT NOT NULL,
  sequence     TEXT,
  PRIMARY KEY (item_asin, marketplace, series_asin),
  FOREIGN KEY (item_asin, marketplace)
    REFERENCES items(asin, marketplace) ON DELETE CASCADE
);

CREATE INDEX idx_item_series_series ON item_series (series_asin);

-- Downloaded assets, tracked per item, kind and quality so a corrected
-- release (same asin, new acr/version) is detectable and re-downloads
-- stay registered. No FK: `asin` may be an item OR an episode, so the
-- cascade is done manually in remove_items.
CREATE TABLE downloads (
  asin           TEXT NOT NULL,
  marketplace    TEXT NOT NULL,
  kind           TEXT NOT NULL,   -- 'audio' | 'cover' | 'chapter' | 'pdf'
  acr            TEXT,            -- Audible Content Reference (audio)
  content_format TEXT NOT NULL DEFAULT '',  -- codec/quality (AAX_44_128), cover size, chapter type, or reencode target (mp3_320)
  variant        TEXT NOT NULL DEFAULT 'original',  -- audio form: 'original' | 'decrypted' | 'reencoded' (else 'original')
  request_kind   TEXT NOT NULL DEFAULT '',  -- audio: pre-request intent alias (adrm-high | widevine-aac-normal | mpeg); '' for non-audio
  version        TEXT,
  sku            TEXT,
  file_path      TEXT NOT NULL,
  file_size      INTEGER,
  status         TEXT NOT NULL,   -- 'downloaded'
  downloaded_utc TEXT NOT NULL,
  updated_utc    TEXT NOT NULL,
  PRIMARY KEY (asin, marketplace, kind, content_format, variant)
);

CREATE INDEX idx_downloads_asin ON downloads (asin, marketplace);
CREATE INDEX idx_downloads_request_kind ON downloads (asin, marketplace, request_kind);

-- Granted content licenses, kept so a later run can re-use the (stable)
-- download URL and the encrypted voucher without a fresh licenserequest.
-- `doc` is the full content_license response; the voucher inside it
-- stays encrypted — the content key/iv is never stored in the database.
-- No FK (asin may be an item OR an episode).
CREATE TABLE licenses (
  asin           TEXT NOT NULL,
  marketplace    TEXT NOT NULL,
  content_format TEXT NOT NULL DEFAULT '',  -- e.g. AAX_44_128
  request_kind   TEXT NOT NULL DEFAULT '',  -- pre-request intent alias (adrm-high | widevine-aac-normal | mpeg)
  valid_until    TEXT,                       -- content_license.expiration_date
  doc            TEXT NOT NULL,              -- full licenserequest response
  created_utc    TEXT NOT NULL,
  updated_utc    TEXT NOT NULL,
  PRIMARY KEY (asin, marketplace, content_format)
);

CREATE INDEX idx_licenses_asin ON licenses (asin, marketplace);
CREATE INDEX idx_licenses_request_kind ON licenses (asin, marketplace, request_kind);

-- Per-item annotations (last_heard, bookmarks, notes, clips): mutable user
-- data fetched fresh on each `annotations sync`. `doc` is the last response
-- payload (NULL when the title has none); `status` is 'ok' or 'none' (a 404
-- meaning the title has no annotations — recorded so it counts as synced and
-- is skipped by `--missing`). No FK (asin may be an item OR an episode).
CREATE TABLE annotations (
  asin        TEXT NOT NULL,
  marketplace TEXT NOT NULL,
  doc         TEXT,            -- last annotation response (NULL when status='none')
  status      TEXT NOT NULL,   -- 'ok' | 'none'
  fetched_utc TEXT NOT NULL,
  file_path   TEXT,            -- last saved `.annot` path (NULL until `annotations --save`); moved by `download reorganize`
  PRIMARY KEY (asin, marketplace)
);
"#;

/// Creates the schema on a fresh database. Pre-release there is a single
/// version (1); schema changes are made in place and the developer deletes the
/// database to pick them up — the version is not bumped and no migration
/// ladder is grown until the first release.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_SQL)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}
