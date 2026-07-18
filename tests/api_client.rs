//! Integration tests for the API client: auth modes, signing parity with
//! the golden fixtures, access-token refresh with write-back, all against
//! wiremock servers. Synthetic throwaway material only.

use audible_rs::api::client::{ApiError, AuthMode, Client};
use audible_rs::auth::Authenticator;
use audible_rs::auth::signing::RequestSigner;
use reqwest::{Method, Url};
use serde::Deserialize;
use wiremock::matchers::{body_string_contains, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Deserialize)]
struct SigningFixture {
    adp_token: String,
    device_private_key: String,
}

fn signing_fixture() -> SigningFixture {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/signing.json"
    ))
    .expect("missing signing fixture — run scripts/gen_fixtures.py once");
    serde_json::from_str(&raw).unwrap()
}

const FAR_FUTURE: f64 = 9999999999.0;
const PAST: f64 = 1.0;

/// Synthetic auth data; `signing` controls whether adp_token + key are
/// present, `expires` the access token expiry.
fn make_auth(signing: bool, expires: f64) -> Authenticator {
    let fixture = signing_fixture();
    let data = serde_json::json!({
        "country_code": "de",
        "device": { "name": "Alices Test iPhone" },
        "signing": {
            "adp_token": signing.then(|| fixture.adp_token.clone()),
            "device_private_key": signing.then(|| fixture.device_private_key.clone()),
        },
        "bearer": {
            "access_token": "Atna|current-token",
            "refresh_token": "Atnr|refresh-token",
            "expires": expires,
        },
        "website_cookies": {
            // A different domain's cookies must NOT be sent to the mock host
            // (127.0.0.1); only the matching bucket below is attached.
            ".amazon.de": [
                {"name": "should-not-leak", "value": "secret"}
            ],
            "127.0.0.1": [
                {"name": "session-id", "value": "000-0000000-0000000"},
                {"name": "x-acbde", "value": "\"quoted\""}
            ]
        },
        // A future TTL keeps the mock host's bucket fresh, so the cookies-mode
        // request sends them instead of attempting a lazy exchange.
        "cookie_ttls": { "127.0.0.1": FAR_FUTURE },
    });
    Authenticator::from_value(data).unwrap()
}

async fn client_for(server: &MockServer, auth: Authenticator) -> Client {
    let url = Url::parse(&server.uri()).unwrap();
    Client::builder(auth)
        .api_base_override(url.clone())
        .auth_base_override(url)
        .build()
        .unwrap()
}

#[tokio::test]
async fn signing_headers_match_reference_signer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/account/information"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(true, FAR_FUTURE)).await;
    let response = client
        .request(Method::GET, "/1.0/account/information")
        .auth(AuthMode::Signing)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let request = &server.received_requests().await.unwrap()[0];
    let fixture = signing_fixture();
    assert_eq!(
        request
            .headers
            .get("x-adp-token")
            .unwrap()
            .to_str()
            .unwrap(),
        fixture.adp_token
    );
    assert_eq!(
        request.headers.get("x-adp-alg").unwrap().to_str().unwrap(),
        "SHA256withRSA:1.0"
    );

    // Re-sign with the timestamp embedded in the header: the signature
    // must match exactly (PKCS#1 v1.5 is deterministic).
    let signature_header = request
        .headers
        .get("x-adp-signature")
        .unwrap()
        .to_str()
        .unwrap();
    let timestamp = signature_header
        .split_once(':')
        .expect("signature has <sig>:<timestamp> form")
        .1;
    let signer = RequestSigner::new(&fixture.device_private_key, fixture.adp_token).unwrap();
    let expected = signer.sign_request_at("GET", "/1.0/account/information", b"", timestamp);
    assert_eq!(signature_header, expected.signature);
}

#[tokio::test]
async fn token_mode_uses_current_token_without_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    // No mock for /auth/token: a refresh attempt would 404 and fail.

    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    client
        .request(Method::GET, "/1.0/library")
        .auth(AuthMode::Token)
        .send()
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        request
            .headers
            .get("x-amz-access-token")
            .unwrap()
            .to_str()
            .unwrap(),
        "Atna|current-token"
    );
    // The live API rejects the Python-style Authorization/client-id pair.
    assert!(!request.headers.contains_key("authorization"));
    assert!(!request.headers.contains_key("client-id"));
}

#[tokio::test]
async fn expired_token_is_refreshed_once_for_concurrent_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_string_contains("source_token_type=refresh_token"))
        .and(body_string_contains("app_name=Audible"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "Atna|fresh-token",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(false, PAST)).await;
    let (a, b) = tokio::join!(
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Token)
            .send(),
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Token)
            .send(),
    );
    a.unwrap();
    b.unwrap();

    for request in server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/1.0/library")
    {
        assert_eq!(
            request
                .headers
                .get("x-amz-access-token")
                .unwrap()
                .to_str()
                .unwrap(),
            "Atna|fresh-token"
        );
    }
}

#[tokio::test]
async fn deregister_uses_access_token_and_posts_flag() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/deregister"))
        .and(body_string_contains("deregister_all_existing_accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "response": { "success": {} }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    // The locally stored device name is returned so the caller can report it.
    let device_name = client.deregister(false).await.unwrap();
    assert_eq!(device_name.as_deref(), Some("Alices Test iPhone"));

    let request = &server.received_requests().await.unwrap()[0];
    // Authenticated with x-amz-access-token, never an RFC-6750 Bearer.
    assert_eq!(
        request
            .headers
            .get("x-amz-access-token")
            .unwrap()
            .to_str()
            .unwrap(),
        "Atna|current-token"
    );
    assert!(!request.headers.contains_key("authorization"));
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("\"deregister_all_existing_accounts\":false"));
}

#[tokio::test]
async fn deregister_surfaces_error_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/deregister"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    let error = client.deregister(true).await.unwrap_err();
    assert!(matches!(error, ApiError::Deregister(status) if status.as_u16() == 403));
}

#[tokio::test]
async fn auto_is_token_first_off_the_sidecar() {
    // AUD-195: `Auto` sends the access token for every host audible-rs calls
    // except the CDE-Sidecar — including accounts that *have* signing
    // material (this used to be signing-first). The one signed host,
    // `cde-ta-g7g.*`, cannot be reached through the mock server (it resolves
    // by name); its selection is pinned by the `host_requires_signing` unit
    // test in the client module.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;

    let with_signing = client_for(&server, make_auth(true, FAR_FUTURE)).await;
    with_signing
        .request(Method::GET, "/1.0/library")
        .send()
        .await
        .unwrap();

    let token_only = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    token_only
        .request(Method::GET, "/1.0/library")
        .send()
        .await
        .unwrap();

    // Both carry the token and neither is signed — signing material or not.
    for request in server.received_requests().await.unwrap() {
        assert!(
            request.headers.contains_key("x-amz-access-token"),
            "Auto must send the access token off the sidecar"
        );
        assert!(
            !request.headers.contains_key("x-adp-signature"),
            "Auto must not sign off the sidecar"
        );
    }
}

#[tokio::test]
async fn cookies_mode_attaches_stored_cookies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/web/endpoint"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    client
        .request(Method::GET, "/web/endpoint")
        .auth(AuthMode::Cookies)
        .send()
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    let cookie = request.headers.get("cookie").unwrap().to_str().unwrap();
    // Host-scoped: only the matching host's cookies, never another domain's.
    assert_eq!(cookie, "session-id=000-0000000-0000000; x-acbde=\"quoted\"");
    assert!(!cookie.contains("should-not-leak"));
}

#[tokio::test]
async fn signed_post_body_matches_sent_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/1.0/wishlist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(true, FAR_FUTURE)).await;
    client
        .request(Method::POST, "/1.0/wishlist")
        .auth(AuthMode::Signing)
        .body(serde_json::json!({"asin": "B07RJZJ8L9"}))
        .send()
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    let fixture = signing_fixture();
    let signature_header = request
        .headers
        .get("x-adp-signature")
        .unwrap()
        .to_str()
        .unwrap();
    let timestamp = signature_header.split_once(':').unwrap().1;
    let signer = RequestSigner::new(&fixture.device_private_key, fixture.adp_token).unwrap();
    let expected = signer.sign_request_at("POST", "/1.0/wishlist", &request.body, timestamp);
    assert_eq!(
        signature_header, expected.signature,
        "signature must cover exactly the bytes that were sent"
    );
    assert_eq!(
        request
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/json"
    );
}

#[tokio::test]
async fn rejects_foreign_hosts_and_relative_paths() {
    let server = MockServer::start().await;
    let client = client_for(&server, make_auth(true, FAR_FUTURE)).await;

    for bad in [
        "https://evil.example/x",
        "relative/path",
        "//evil.example/x",
    ] {
        let result = client.request(Method::GET, bad).send().await;
        assert!(
            matches!(result, Err(ApiError::InvalidPath(_))),
            "path {bad:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn force_refresh_replaces_token_even_when_valid() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_string_contains("source_token_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "Atna|fresh-token",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;

    // The current token is NOT expired, yet `force` must refresh anyway.
    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    client.force_refresh_access_token().await.unwrap();

    let status = client.token_status().await;
    assert!(status.has_access_token);
    assert!(status.has_refresh_token);
    let remaining = status.remaining_secs.expect("a fresh token has an expiry");
    assert!(
        (3500..=3600).contains(&remaining),
        "unexpected remaining: {remaining}"
    );

    // A following token-mode request carries the refreshed token.
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    client
        .request(Method::GET, "/1.0/library")
        .auth(AuthMode::Token)
        .send()
        .await
        .unwrap();
    let request = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/1.0/library")
        .unwrap();
    assert_eq!(
        request
            .headers
            .get("x-amz-access-token")
            .unwrap()
            .to_str()
            .unwrap(),
        "Atna|fresh-token"
    );
}

#[tokio::test]
async fn token_status_reports_expiry_and_refresh_token() {
    let server = MockServer::start().await;

    // Expired access token, refresh token present.
    let expired = client_for(&server, make_auth(false, PAST)).await;
    let status = expired.token_status().await;
    assert!(status.has_access_token);
    assert!(status.has_refresh_token);
    assert!(status.remaining_secs.unwrap() < 0, "should read as expired");

    // A token far in the future reads as valid.
    let valid = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    assert!(valid.token_status().await.remaining_secs.unwrap() > 0);
}

#[tokio::test]
async fn clear_access_token_keeps_the_refresh_token() {
    let server = MockServer::start().await;
    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;

    client.clear_access_token().await.unwrap();

    let status = client.token_status().await;
    assert!(!status.has_access_token);
    assert!(status.remaining_secs.is_none());
    // The refresh token is the lifeline and must survive the removal.
    assert!(status.has_refresh_token);
}

#[tokio::test]
async fn signing_unavailable_without_material() {
    let server = MockServer::start().await;
    let client = client_for(&server, make_auth(false, FAR_FUTURE)).await;
    let result = client
        .request(Method::GET, "/1.0/library")
        .auth(AuthMode::Signing)
        .send()
        .await;
    assert!(matches!(
        result,
        Err(ApiError::AuthModeUnavailable(AuthMode::Signing))
    ));
}

/// AUD-29: every API request carries an `x-amzn-requestid` in the official
/// app's format (40 uppercase hex chars), fresh per request.
#[tokio::test]
async fn requests_send_a_fresh_amzn_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(header_regex("x-amzn-requestid", "^[0-9A-F]{40}$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client_for(&server, make_auth(true, FAR_FUTURE)).await;
    for _ in 0..2 {
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Signing)
            .send()
            .await
            .unwrap();
    }

    let ids: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            request.headers["x-amzn-requestid"]
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "the request id must be fresh per request");
}
