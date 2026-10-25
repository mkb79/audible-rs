//! Integration tests for the experimental FairPlay protocol layer against
//! wiremock: the licenserequest and drmlicense body shapes, the certificate
//! request, the error surfacing, and the playlist walk to the key URIs.
//! Synthetic auth material, ASINs and URLs only.

// The FairPlay module exists on macOS only.
#![cfg(target_os = "macos")]

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::fairplay::{self, Consumption, FairPlayError, LicenseOptions, hls};
use reqwest::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
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
async fn requests_a_fairplay_license_and_parses_the_grant() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B0EXAMPLE1/licenserequest"))
        .and(body_string_contains("\"drm_types\":[\"FairPlay\"]"))
        .and(body_string_contains(
            "\"codecs\":[\"mp4a.40.2\",\"mp4a.40.42\",\"ec+3\"]",
        ))
        .and(body_string_contains("\"consumption_type\":\"Download\""))
        .and(body_string_contains("\"spatial\":true"))
        .and(header("x-amz-access-token", "Atna|synthetic"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_license": {
                "status_code": "Granted",
                "drm_type": "FairPlay",
                "license_id": "lic-1",
                "license_response": "https://cdn.example.invalid/t/title.master.m3u8",
                "certificate": "Y2VydA==",
                "content_metadata": {
                    "content_reference": {"codec": "mp4a.40.42", "content_format": "M4A_XHE"}
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let options = LicenseOptions {
        xhe: true,
        spatial: true,
        ..LicenseOptions::default()
    };
    let grant = fairplay::request_license(&client, "de", "B0EXAMPLE1", &options)
        .await
        .unwrap();
    assert_eq!(grant.license_id.as_deref(), Some("lic-1"));
    assert_eq!(grant.certificate.as_deref(), Some(&b"cert"[..]));
    assert_eq!(grant.content_format.as_deref(), Some("M4A_XHE"));
}

#[tokio::test]
async fn a_rejected_license_surfaces_the_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B0EXAMPLE1/licenserequest"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error_code": "000307",
            "message": "No FairPlay asset."
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let error = fairplay::request_license(&client, "de", "B0EXAMPLE1", &LicenseOptions::default())
        .await
        .unwrap_err();
    match error {
        FairPlayError::Rejected {
            status,
            error_code,
            message,
            ..
        } => {
            assert_eq!(status, 403);
            assert_eq!(error_code, "000307");
            assert_eq!(message, "No FairPlay asset.");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn fetches_the_certificate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/content/FairPlay/certificate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"certificate": "Y2VydA=="})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let certificate = fairplay::request_certificate(&client, "de").await.unwrap();
    assert_eq!(certificate, b"cert");
}

#[tokio::test]
async fn exchanges_an_spc_for_a_ckc() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B0EXAMPLE1/drmlicense"))
        .and(body_string_contains("\"licenseChallenge\":\"c3Bj\""))
        .and(body_string_contains("\"license_id\":\"lic-1\""))
        .and(body_string_contains("\"drm_type\":\"FairPlay\""))
        .and(body_string_contains("\"consumption_type\":\"Streaming\""))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"license": "Y2tj"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let ckc = fairplay::request_ckc(
        &client,
        "de",
        "B0EXAMPLE1",
        b"spc",
        Some("lic-1"),
        Consumption::Streaming,
    )
    .await
    .unwrap();
    assert_eq!(ckc, b"ckc");
}

#[tokio::test]
async fn walks_from_the_master_to_the_key_uris() {
    let server = MockServer::start().await;
    let master = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.42\"\n\
        v64.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.42\"\n\
        /hls/v128.m3u8\n";
    let media = |id: &str| {
        format!(
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://{id}\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:10.0,\nseg.mp4\n"
        )
    };
    Mock::given(method("GET"))
        .and(path("/hls/master.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string(master))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/hls/v64.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string(media("content-a")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/hls/v128.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string(media("content-a")))
        .expect(1)
        .mount(&server)
        .await;

    let http = hls::http_client().unwrap();
    let url = format!("{}/hls/master.m3u8?ss_sec=20", server.uri());
    let found = fairplay::discover(&http, &url).await.unwrap();
    assert_eq!(found.master.variants.len(), 2);
    assert_eq!(found.key_uris, ["skd://content-a"]);
    assert_eq!(fairplay::content_id(&found.key_uris[0]), Some("content-a"));
}

#[tokio::test]
async fn session_keys_in_the_master_need_no_further_fetch() {
    let server = MockServer::start().await;
    let master = "#EXTM3U\n\
        #EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://content-b\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=64000\n\
        v64.m3u8\n";
    Mock::given(method("GET"))
        .and(path("/hls/master.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string(master))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/hls/v64.m3u8"))
        .respond_with(ResponseTemplate::new(200).set_body_string("#EXTM3U\n"))
        .expect(0)
        .mount(&server)
        .await;

    let http = hls::http_client().unwrap();
    let found = fairplay::discover(&http, &format!("{}/hls/master.m3u8", server.uri()))
        .await
        .unwrap();
    assert_eq!(found.key_uris, ["skd://content-b"]);
}

#[tokio::test]
async fn a_playlist_error_does_not_leak_the_signed_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hls/master.m3u8"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let http = hls::http_client().unwrap();
    let url = format!("{}/hls/master.m3u8?token=secret-token", server.uri());
    let error = fairplay::discover(&http, &url).await.unwrap_err();
    let message = format!("{error} {error:?}");
    assert!(!message.contains("secret-token"), "{message}");
    assert!(!message.contains("/hls/master.m3u8"), "{message}");
}
