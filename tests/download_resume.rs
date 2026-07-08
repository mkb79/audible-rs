//! Integration tests for the resumable file downloader against
//! wiremock: a fresh download, a resume from a partial file, and an
//! already-complete file.

use audible_rs::api::client::Client;
use audible_rs::auth::Authenticator;
use audible_rs::downloader::{DownloadOutcome, download_cenc_to_file, download_to_file};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const FULL: &[u8] = b"0123456789ABCDEFGHIJ"; // 20 bytes

/// A client whose default locale points at the mock server, so
/// authed_get builds requests against it (auth headers are ignored by
/// the mock).
fn make_client(server: &MockServer) -> Client {
    let auth = Authenticator::from_value(serde_json::json!({
        "country_code": "de",
        "bearer": { "access_token": "Atna|synthetic", "expires": 9999999999.0 }
    }))
    .unwrap();
    let url = reqwest::Url::parse(&server.uri()).unwrap();
    Client::builder(auth)
        .api_base_override(url.clone())
        .auth_base_override(url)
        .build()
        .unwrap()
}

#[tokio::test]
async fn fresh_download_writes_the_whole_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(FULL))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(std::fs::read(&dest).unwrap(), FULL);
    // The partial file is gone after the rename.
    assert!(!dest.with_file_name("Book.aaxc.part").exists());
}

#[tokio::test]
async fn resumes_from_a_partial_file() {
    let server = MockServer::start().await;
    // A range request from byte 8 returns the remaining bytes as 206.
    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("range", "bytes=8-"))
        .respond_with(|req: &Request| {
            // Echo the tail starting at the requested offset.
            let range = req
                .headers
                .get("range")
                .unwrap()
                .to_str()
                .unwrap()
                .trim_start_matches("bytes=")
                .trim_end_matches('-');
            let start: usize = range.parse().unwrap();
            ResponseTemplate::new(206).set_body_bytes(&FULL[start..])
        })
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    // Seed a partial with the first 8 bytes.
    std::fs::write(dest.with_file_name("Book.aaxc.part"), &FULL[..8]).unwrap();

    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        FULL,
        "resumed file is complete"
    );
}

#[tokio::test]
async fn already_complete_file_is_not_refetched() {
    let server = MockServer::start().await;
    // No mock mounted: any request would 404 and fail the download.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    std::fs::write(&dest, FULL).unwrap();

    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::AlreadyComplete);
}

#[tokio::test]
async fn force_refetches_an_already_complete_file() {
    let server = MockServer::start().await;
    // A fresh 200 with new content; force must request it despite the
    // existing complete file and overwrite it.
    let fresh: &[u8] = b"NEWNEWNEWNEWNEWNEWNE"; // 20 bytes
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fresh))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    std::fs::write(&dest, FULL).unwrap();

    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        true,
        None,
        &[],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        fresh,
        "force overwrote the file"
    );
}

#[tokio::test]
async fn rejects_wrong_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(
            // wiremock serves a string body as text/plain — a type not in the
            // audio set, so the download must be rejected.
            ResponseTemplate::new(200).set_body_string("<html>error</html>"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    let client = make_client(&server);
    let err = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        None,
        false,
        None,
        &["audio/aax", "audio/vnd.audible.aax", "audio/mpeg"],
        &[],
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unexpected content type"), "{msg}");
    assert!(msg.contains("error"), "{msg}"); // the text body is surfaced
    assert!(msg.contains("please report it"), "{msg}"); // report hint (AUD-105)
    // A rejected response is not written to disk (no file, no partial).
    assert!(!dest.exists());
    assert!(!dest.with_file_name("Book.aaxc.part").exists());
}

/// The Widevine/CENC path (`download_cenc_to_file`) verifies the content-type
/// too (AUD-105): a `200 text/plain` is rejected, not written as a broken file.
#[tokio::test]
async fn cenc_rejects_wrong_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cenc"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>error</html>"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.cenc");
    let err = download_cenc_to_file(
        &format!("{}/cenc", server.uri()),
        &dest,
        None,
        false,
        None,
        &["audio/mp4", "video/mp4"],
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unexpected content type"), "{msg}");
    assert!(msg.contains("please report it"), "{msg}");
    assert!(!dest.exists());
    assert!(!dest.with_file_name("Book.cenc.part").exists());
}

#[tokio::test]
async fn accepts_matching_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/aax")
                .set_body_bytes(FULL),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &["audio/aax"],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(std::fs::read(&dest).unwrap(), FULL);
}
