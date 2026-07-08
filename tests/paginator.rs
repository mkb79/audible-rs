//! Integration tests for the continuation-token paginator against
//! wiremock. Synthetic auth material only.

use audible_rs::api::client::{AuthMode, Client};
use audible_rs::api::paginator;
use audible_rs::auth::Authenticator;
use futures::TryStreamExt as _;
use reqwest::{Method, Url};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
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

fn page_body(n: u32) -> serde_json::Value {
    serde_json::json!({"items": [{"asin": format!("ASIN{n}"), "title": format!("Book {n}")}]})
}

#[tokio::test]
async fn follows_continuation_tokens_and_captures_state_token() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param_is_missing("continuation_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page_body(1))
                .insert_header("Continuation-Token", "page-2")
                .insert_header("State-Token", "0"), // "0" means none
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("continuation_token", "page-2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page_body(2))
                .insert_header("Continuation-Token", "page-3"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .and(query_param("continuation_token", "page-3"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(page_body(3))
                .insert_header("State-Token", "1750000000000"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let pages: Vec<paginator::Page> = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Token)
            .query("num_results", "1")
    })
    .try_collect()
    .await
    .unwrap();

    assert_eq!(pages.len(), 3);
    assert_eq!(pages[0].body["items"][0]["asin"], "ASIN1");
    assert_eq!(pages[2].body["items"][0]["asin"], "ASIN3");
    // "0" is filtered, real tokens are surfaced.
    assert_eq!(pages[0].state_token, None);
    assert_eq!(pages[1].state_token, None);
    assert_eq!(pages[2].state_token.as_deref(), Some("1750000000000"));
}

#[tokio::test]
async fn retries_transient_errors_with_retry_after() {
    let server = MockServer::start().await;

    // First two attempts fail with 503, the third succeeds.
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "0"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_body(1)))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let pages: Vec<paginator::Page> = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Token)
    })
    .try_collect()
    .await
    .unwrap();
    assert_eq!(pages.len(), 1);
}

#[tokio::test]
async fn non_retryable_status_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/1.0/library"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let result: Result<Vec<paginator::Page>, _> = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .auth(AuthMode::Token)
    })
    .try_collect()
    .await;
    assert!(result.is_err());
}
