//! Integration tests for library sync against wiremock: full sync over
//! two pages, then a delta sync applying an update and a revocation.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::db::Db;
use audible_rs::library_sync::{DEFAULT_RESPONSE_GROUPS, SyncOptions, sync_library};
use reqwest::Url;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(server: &MockServer) -> Client {
    let auth = Authenticator::from_value(serde_json::json!({
        "country_code": "de",
        "identity": {"customer_id": "amzn1.account.TEST"},
        "bearer": {
            "access_token": "Atna|synthetic",
            "refresh_token": "Atnr|synthetic",
            "expires": 9999999999.0
        }
    }))
    .unwrap();
    let url = Url::parse(&server.uri()).unwrap();
    Client::builder(auth)
        .api_base_override(url.clone())
        .auth_base_override(url)
        .build()
        .unwrap()
}

fn book(asin: &str, title: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "asin": asin,
        "title": title,
        "status": status,
        "purchase_date": "2024-01-01",
        "runtime_length_min": 100,
    })
}

/// A16 (verified live 2026-07-17): the server sends `State-Token` on the
/// FIRST page — a snapshot marker from before the pages. An abort on a
/// later page must therefore leave the stored token untouched; persisting
/// it with page 1 made the next delta silently skip the never-applied
/// remainder until a manual --full.
#[tokio::test]
async fn aborted_sync_never_advances_the_state_token() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    // Page 1 carries the token (the live wire shape) and a continuation.
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param_is_missing("continuation_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"items": [book("A1", "Erstes Buch", "Active")]}))
                .insert_header("State-Token", "1750000000000")
                .insert_header("Continuation-Token", "page-2"),
        )
        .mount(&server)
        .await;
    // Page 2 fails: the sync aborts mid-stream.
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("continuation_token", "page-2"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 1,
            resolve_podcasts: false,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await;
    assert!(result.is_err(), "the aborted stream must error");

    // The stored token is untouched — the next run is still a full sync
    // that fetches the never-applied remainder.
    let settings = db
        .ensure_sync_state("de".into(), DEFAULT_RESPONSE_GROUPS.to_owned())
        .await
        .unwrap();
    assert_eq!(
        settings.last_state_token, None,
        "an aborted sync must not advance the state token"
    );
}

#[tokio::test]
async fn full_then_delta_sync() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    // --- Full sync: two pages, no state_token sent, status=Active. ---
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("status", "Active"))
        .and(query_param_is_missing("state_token"))
        .and(query_param_is_missing("continuation_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"items": [book("A1", "Erstes Buch", "Active")]}))
                .insert_header("Continuation-Token", "page-2"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("continuation_token", "page-2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"items": [book("A2", "Zweites Buch", "Active")]}))
                .insert_header("State-Token", "1750000000000"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 1,
            resolve_podcasts: false,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();

    assert_eq!(summary.mode, "full");
    assert_eq!(summary.pages, 2);
    assert!(
        summary.initial,
        "empty DB before the run is an initial sync"
    );
    assert_eq!(summary.changes.added.len(), 2);
    assert_eq!(summary.changes.changed.len(), 0);
    assert_eq!(summary.changes.removed.len(), 0);
    assert_eq!(summary.state_token.as_deref(), Some("1750000000000"));
    assert_eq!(db.count_active(vec!["de".to_owned()]).await.unwrap(), 2);

    // --- Delta sync: state_token sent, status=Active,Revoked. ---
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("status", "Active,Revoked"))
        .and(query_param("state_token", "1750000000000"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"items": [
                    book("A1", "Erstes Buch (neu)", "Active"),
                    book("A2", "Zweites Buch", "Revoked"),
                ]}))
                .insert_header("State-Token", "1750000099000"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 10,
            resolve_podcasts: false,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();

    assert_eq!(summary.mode, "delta");
    assert!(!summary.initial, "DB was populated, so not an initial sync");
    // A1's title changed → changed; A2 was revoked → removed; nothing new.
    assert_eq!(summary.changes.added.len(), 0);
    assert_eq!(summary.changes.changed.len(), 1);
    assert_eq!(summary.changes.changed[0].asin, "A1");
    assert_eq!(summary.changes.removed.len(), 1);
    assert_eq!(summary.changes.removed[0].asin, "A2");
    assert_eq!(summary.state_token.as_deref(), Some("1750000099000"));
    assert_eq!(db.count_active(vec!["de".to_owned()]).await.unwrap(), 1);

    let books = db
        .list_books(vec!["de".to_owned()], vec![], 10, 0)
        .await
        .unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].full_title, "Erstes Buch (neu)");

    // --full forces a full resync even with a stored token.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("status", "Active"))
        .and(query_param_is_missing("state_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"items": [book("A1", "Erstes Buch (neu)", "Active")]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: true,
            page_size: 10,
            resolve_podcasts: false,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();
    assert_eq!(summary.mode, "full");
}

#[tokio::test]
async fn sync_resolves_podcast_episodes() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    // Library page: one book plus one podcast parent.
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param_is_missing("parent_asin"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": [
                book("B1", "Ein Buch", "Active"),
                {
                    "asin": "P1",
                    "title": "Mein Podcast",
                    "status": "Active",
                    "content_delivery_type": "PodcastParent",
                    "episode_count": 2,
                },
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Episode listing of the parent (parent included in its own list).
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("parent_asin", "P1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": [
                {"asin": "P1", "title": "Mein Podcast", "content_delivery_type": "PodcastParent"},
                {"asin": "E1", "title": "Folge 1", "release_date": "2026-06-01"},
                {"asin": "E2", "title": "Folge 2", "release_date": "2026-06-08"},
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 10,
            resolve_podcasts: true,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();

    assert_eq!(summary.changes.added.len(), 2);
    assert_eq!(summary.podcasts_resolved, 1);
    assert_eq!(summary.episodes_upserted, 2);

    let podcasts = db.podcasts(vec!["de".to_owned()]).await.unwrap();
    assert_eq!(podcasts.len(), 1);
    assert_eq!(podcasts[0].stored_episodes, 2);

    let episodes = db
        .episodes("de".into(), Some("P1".into()), 10, 0)
        .await
        .unwrap();
    assert_eq!(episodes.len(), 2);
    // Newest first.
    assert_eq!(episodes[0].asin, "E2");

    // Books are unaffected by episode storage.
    assert_eq!(db.count_active(vec!["de".to_owned()]).await.unwrap(), 2);
    assert_eq!(
        db.list_books(vec!["de".to_owned()], vec![], 10, 0)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn incomplete_library_listing_falls_back_to_catalog() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    // Parent announces 3 episodes…
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param_is_missing("parent_asin"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": [
                {
                    "asin": "P1",
                    "title": "Mein Podcast",
                    "status": "Active",
                    "content_delivery_type": "PodcastParent",
                    "episode_count": 3,
                },
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // …but the library child listing only returns the newest one
    // (observed live: capped at the most recent episodes).
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("parent_asin", "P1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": [
                {"asin": "E3", "title": "Folge 3 (library doc)", "release_date": "2026-06-10"},
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    // The parent product's relationships list every episode in one request.
    // A PodcastSeason container also appears as a child (relationship_type
    // "season") and must be filtered out.
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products/P1"))
        .and(query_param("response_groups", "relationships_v2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "product": {"asin": "P1", "relationships": [
                {"asin": "E1", "relationship_to_product": "child", "relationship_type": "episode"},
                {"asin": "E2", "relationship_to_product": "child", "relationship_type": "episode"},
                {"asin": "E3", "relationship_to_product": "child", "relationship_type": "episode"},
                {"asin": "SEASON0", "relationship_to_product": "child", "relationship_type": "season"},
            ]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Details are batched by ASIN for the episodes not already covered by the
    // library doc (E3): only E1 and E2 are requested.
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products"))
        .and(query_param("asins", "E1,E2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "products": [
                {"asin": "E1", "title": "Folge 1", "release_date": "2026-06-01"},
                {"asin": "E2", "title": "Folge 2", "release_date": "2026-06-05"},
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 10,
            resolve_podcasts: true,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();
    assert_eq!(summary.episodes_upserted, 3);

    let episodes = db
        .episodes("de".into(), Some("P1".into()), 10, 0)
        .await
        .unwrap();
    assert_eq!(episodes.len(), 3);
    // The library document wins for episodes present in both sources.
    let e3 = episodes.iter().find(|e| e.asin == "E3").unwrap();
    assert_eq!(e3.full_title, "Folge 3 (library doc)");
}

#[tokio::test]
async fn full_episode_list_from_catalog_relationships_batches_by_asin() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param_is_missing("parent_asin"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": [
                {
                    "asin": "P1",
                    "title": "Mein Podcast",
                    "status": "Active",
                    "content_delivery_type": "PodcastParent",
                    "episode_count": 120,
                },
            ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("parent_asin", "P1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"items": []})))
        .expect(1)
        .mount(&server)
        .await;

    // The parent product's relationships list all 120 episodes in one request.
    let relationships: Vec<serde_json::Value> = (0..120)
        .map(|i| {
            serde_json::json!({
                "asin": format!("E{i}"),
                "relationship_to_product": "child",
                "relationship_type": "episode",
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products/P1"))
        .and(query_param("response_groups", "relationships_v2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "product": {"asin": "P1", "relationships": relationships},
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Details are batched 50 ASINs per request: three batches (50/50/20).
    for start in [0usize, 50, 100] {
        let end = (start + 50).min(120);
        let csv = (start..end)
            .map(|i| format!("E{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let products: Vec<serde_json::Value> = (start..end)
            .map(|i| serde_json::json!({"asin": format!("E{i}"), "title": format!("Folge {i}")}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/1.0/catalog/products"))
            .and(query_param("asins", csv))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"products": products})),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let summary = sync_library(
        &client,
        &db,
        "de",
        SyncOptions {
            full: false,
            page_size: 10,
            resolve_podcasts: true,
            record_changes: false,
            change_retention_days: 0,
        },
        &tokio::sync::Semaphore::new(10),
    )
    .await
    .unwrap();
    assert_eq!(summary.episodes_upserted, 120);
    assert_eq!(
        db.episodes("de".into(), Some("P1".into()), 0, 0)
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.episodes("de".into(), Some("P1".into()), 200, 0)
            .await
            .unwrap()
            .len(),
        120
    );
}
