//! Integration test for the series catalog diff: owned volumes
//! (including an omnibus covering a range) vs. the catalog's child
//! list. Synthetic data only.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::commands::series::missing_volumes;
use audible_rs::db::{Db, SeriesRef, SyncLogEntry, UpsertItem};
use reqwest::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(server: &MockServer) -> Client {
    let auth = Authenticator::from_value(serde_json::json!({
        "country_code": "de",
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

fn owned_item(asin: &str, title: &str, series_asin: &str, sequence: &str) -> UpsertItem {
    UpsertItem {
        asin: asin.into(),
        doc: serde_json::json!({"asin": asin, "title": title}).to_string(),
        title: title.into(),
        subtitle: None,
        full_title: title.into(),
        series: vec![SeriesRef {
            series_asin: series_asin.into(),
            series_title: "Thrawn".into(),
            sequence: Some(sequence.into()),
        }],
    }
}

#[tokio::test]
async fn missing_volumes_respects_ranges_and_asins() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
        .await
        .unwrap();
    let client = make_client(&server);

    // Owned: an omnibus covering volumes 1-2 and volume 4 directly.
    db.ensure_sync_state("de".into(), "g".into()).await.unwrap();
    db.apply_page(
        "de".into(),
        vec![
            owned_item("OMN", "Thrawn Sammelband 1-2", "SER", "1-2"),
            owned_item("V4", "Thrawn Band 4", "SER", "4"),
        ],
        vec![],
        SyncLogEntry {
            request_time_utc: audible_rs::db::now_iso_utc(),
            response_time_utc: audible_rs::db::now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();

    // Catalog: five volumes plus a foreign omnibus covering 1-2. The
    // volume number lives in `sequence`; `sort` is display order only
    // (offset by the omnibus) and must be ignored.
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products"))
        .and(query_param("asins", "SER"))
        .and(query_param("response_groups", "relationships_v2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "products": [{"asin": "SER", "relationships": [
                {"asin": "OMN2", "relationship_to_product": "child", "relationship_type": "series", "sequence": "1-2", "sort": "1"},
                {"asin": "C1", "relationship_to_product": "child", "relationship_type": "series", "sequence": "1", "sort": "2"},
                {"asin": "C2", "relationship_to_product": "child", "relationship_type": "series", "sequence": "2", "sort": "3"},
                {"asin": "C3", "relationship_to_product": "child", "relationship_type": "series", "sequence": "3", "sort": "4"},
                {"asin": "V4", "relationship_to_product": "child", "relationship_type": "series", "sequence": "4", "sort": "5"},
                {"asin": "C5", "relationship_to_product": "child", "relationship_type": "series", "sequence": "5", "sort": "6"},
                {"asin": "X", "relationship_to_product": "parent"},
            ]}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Detail lookup for the two missing volumes: band 3 is released,
    // band 5 only comes out in 2027.
    Mock::given(method("GET"))
        .and(path("/1.0/catalog/products"))
        .and(query_param("asins", "C3,C5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "products": [
                {"asin": "C3", "title": "Thrawn Band 3", "release_date": "2024-05-01"},
                {"asin": "C5", "title": "Thrawn Band 5", "release_date": "2099-03-01"},
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let missing = missing_volumes(&client, &db, "de", Some("thrawn".into()))
        .await
        .unwrap();

    // Volumes 1+2 are covered by the owned omnibus range (which also
    // covers the catalog's foreign omnibus OMN2), 4 by ASIN — only 3
    // and 5 are missing; 5 is flagged as not yet released.
    assert_eq!(missing.len(), 2);
    assert_eq!(missing[0].sequence, "3");
    assert_eq!(missing[0].title, "Thrawn Band 3");
    assert!(missing[0].released);
    assert_eq!(missing[1].sequence, "5");
    assert_eq!(missing[1].asin, "C5");
    assert!(!missing[1].released);
    assert_eq!(missing[1].release_date.as_deref(), Some("2099-03-01"));

    // Unknown series fails clearly.
    assert!(
        missing_volumes(&client, &db, "de", Some("nope".into()))
            .await
            .is_err()
    );
}
