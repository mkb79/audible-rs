//! Integration test for the chapter metadata request against wiremock.
//! Mirrors audible-cli's `get_content_metadata`: drm_type=Adrm and
//! chapter_titles_type only — no acr/file_version/tenant_id (those pin
//! the request to the AAX file's generic segment markers). Synthetic
//! auth material only.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::downloader::request_chapters;
use reqwest::Url;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(server: &MockServer) -> Client {
    let auth = Authenticator::from_value(serde_json::json!({
        "country_code": "de",
        "bearer": { "access_token": "Atna|synthetic", "expires": 9999999999.0 }
    }))
    .unwrap();
    let url = Url::parse(&server.uri()).unwrap();
    Client::builder(auth)
        .api_base_override(url.clone())
        .auth_base_override(url)
        .build()
        .unwrap()
}

#[tokio::test]
async fn chapter_request_matches_audible_cli() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/1.0/content/B0D186SQWV/metadata"))
        .and(query_param("drm_type", "Adrm"))
        .and(query_param("quality", "High"))
        .and(query_param("chapter_titles_type", "Tree"))
        .and(query_param(
            "response_groups",
            "last_position_heard,content_reference,chapter_info",
        ))
        // These must NOT be sent — they switch the endpoint to the AAX
        // file's generic "Kapitel N" segment markers.
        .and(query_param_is_missing("acr"))
        .and(query_param_is_missing("file_version"))
        .and(query_param_is_missing("tenant_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_metadata": {
                "chapter_info": {
                    "brandIntroDurationMs": 2043,
                    "chapters": [{"title": "Vorspann", "length_ms": 60000, "start_offset_ms": 0}]
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let chapters = request_chapters(&client, "de", "B0D186SQWV", "High", "Tree")
        .await
        .unwrap();

    assert_eq!(chapters["chapters"][0]["title"], "Vorspann");
    assert_eq!(chapters["brandIntroDurationMs"], 2043);
}

#[tokio::test]
async fn flat_passes_the_quality_and_layout() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/1.0/content/B0D186SQWV/metadata"))
        .and(query_param("quality", "Normal"))
        .and(query_param("chapter_titles_type", "Flat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_metadata": { "chapter_info": {"chapters": []} }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let chapters = request_chapters(&client, "de", "B0D186SQWV", "Normal", "Flat")
        .await
        .unwrap();
    assert!(chapters["chapters"].is_array());
}
