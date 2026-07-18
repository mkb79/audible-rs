//! Shared fixtures for the db domain tests.

use super::{Db, SyncLogEntry, UpsertEpisode, UpsertItem, now_iso_utc};

pub(crate) const MP: &str = "de";

pub(crate) fn item(asin: &str, title: &str) -> UpsertItem {
    UpsertItem {
        asin: asin.into(),
        doc: serde_json::json!({
            "asin": asin,
            "title": title,
            "purchase_date": "2024-01-01",
            "runtime_length_min": 123,
            "language": "german",
            // Real library docs always carry customer_rights (pinned
            // response groups); the missing-download queries filter on it
            // (AUD-104), so the default fixture is a consumable title.
            "customer_rights": {"is_consumable": true},
        })
        .to_string(),
        title: title.into(),
        subtitle: None,
        full_title: title.into(),
        series: Vec::new(),
    }
}

pub(crate) fn episode(asin: &str, title: &str) -> UpsertEpisode {
    UpsertEpisode {
        asin: asin.into(),
        doc: serde_json::json!({
            "asin": asin,
            "title": title,
            "release_date": "2026-06-01",
            "runtime_length_min": 30,
        })
        .to_string(),
        title: title.into(),
        subtitle: None,
        full_title: title.into(),
    }
}

pub(crate) fn default_log() -> SyncLogEntry {
    SyncLogEntry {
        request_time_utc: now_iso_utc(),
        response_time_utc: now_iso_utc(),
        http_status: Some(200),
        ..Default::default()
    }
}

pub(crate) async fn open_temp() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    (dir, db)
}

pub(crate) fn upsert(asin: &str, doc: serde_json::Value) -> UpsertItem {
    UpsertItem {
        asin: asin.into(),
        doc: doc.to_string(),
        title: "T".into(),
        subtitle: None,
        full_title: format!("{asin} title"),
        series: vec![],
    }
}
