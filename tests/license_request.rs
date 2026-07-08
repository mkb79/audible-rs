//! Integration test for the download license request against wiremock:
//! the Adrm body shape, the app headers, and parsing the granted
//! license. Synthetic auth material only.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::downloader::{Quality, request_license};
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
async fn requests_adrm_license_and_parses_grant() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/1.0/content/B0D186SQWV/licenserequest"))
        .and(body_string_contains("\"consumption_type\":\"Download\""))
        .and(body_string_contains("Adrm"))
        .and(header("x-amz-access-token", "Atna|synthetic"))
        .and(header("X-Device-Type-Id", "A2CZJZGLK2JJVM"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_license": {
                "status_code": "Granted",
                "asin": "B0D186SQWV",
                "drm_type": "Adrm",
                "license_response": "BASE64VOUCHER==",
                "content_metadata": {
                    "content_url": {"offline_url": "https://cds.audible.de/x.aaxc?Policy=abc"},
                    "content_reference": {
                        "content_format": "AAX_44_64",
                        "content_size_in_bytes": 123456789u64
                    },
                    "pdf_url": "https://x/booklet.pdf"
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let license = request_license(&client, "de", "B0D186SQWV", Quality::High)
        .await
        .unwrap();

    assert!(license.is_granted());
    assert_eq!(license.drm_type.as_deref(), Some("Adrm"));
    assert_eq!(license.content_format.as_deref(), Some("AAX_44_64"));
    assert_eq!(license.content_size, Some(123456789));
    assert!(license.has_voucher);
    assert_eq!(license.voucher_raw.as_deref(), Some("BASE64VOUCHER=="));
    assert!(license.pdf_url.is_some());
    assert!(
        license.offline_url.unwrap().contains(".aaxc?Policy="),
        "download url present"
    );
}

#[tokio::test]
async fn denied_license_surfaces_the_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/content/B000/licenserequest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_license": {
                "status_code": "Denied",
                "asin": "B000",
                "message": "Customer does not own this content.",
                "content_metadata": {}
            }
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let license = request_license(&client, "de", "B000", Quality::Normal)
        .await
        .unwrap();
    assert!(!license.is_granted());
    assert_eq!(
        license.denial_message.as_deref(),
        Some("Customer does not own this content.")
    );
}
