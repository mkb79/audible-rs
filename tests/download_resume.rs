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
        None,
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
        None,
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
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::AlreadyComplete);
}

/// The already-complete pre-check also covers extension-corrected files
/// (audit A16): an audio served as `audio/mpeg` landed as `X.mp3`, and
/// probing only the planned `X.aaxc` re-transferred it on every
/// record-less run.
#[tokio::test]
async fn an_extension_corrected_file_counts_as_complete() {
    let server = MockServer::start().await;
    // No mock mounted: any request would 404 and fail the download.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    std::fs::write(dir.path().join("Book.mp3"), FULL).unwrap();

    let client = make_client(&server);
    let (outcome, path) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[("audio/mpeg", "mp3"), ("audio/mp4", "m4a")],
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::AlreadyComplete);
    assert!(path.ends_with("Book.mp3"), "{}", path.display());
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
        None,
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
        None,
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
        None,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unexpected content type"), "{msg}");
    assert!(msg.contains("please report it"), "{msg}");
    assert!(!dest.exists());
    assert!(!dest.with_file_name("Book.cenc.part").exists());
}

/// A 416 no longer "completes" an unvalidated partial (audit A9): the
/// partial is discarded and the file re-fetched from scratch — a shrunk
/// replacement PDF/cover would otherwise strand truncated old bytes as
/// the "complete" file.
#[tokio::test]
async fn a_416_discards_the_partial_and_restarts() {
    let server = MockServer::start().await;
    let fresh: &[u8] = b"SHRUNKFILE"; // 10 bytes — smaller than the stale partial
    // The resume attempt (Range) answers 416; the clean retry answers 200.
    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("range", "bytes=16-"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fresh))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.pdf");
    // A stale 16-byte partial of the old, larger file; size unknown (PDF).
    std::fs::write(dest.with_file_name("Book.pdf.part"), b"OLDOLDOLDOLDOLD!").unwrap();

    let client = make_client(&server);
    let (outcome, _) = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        None,
        false,
        None,
        &[],
        &[],
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        fresh,
        "the shrunk replacement is fetched whole — no stale bytes survive"
    );
    assert!(!dest.with_file_name("Book.pdf.part").exists());
}

/// A transfer that ends short of the expected size errors instead of
/// renaming a truncated file into place (audit A9); the partial stays
/// for the next resume.
#[tokio::test]
async fn a_short_transfer_keeps_the_partial_and_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(&FULL[..12]))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    let client = make_client(&server);
    let err = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[],
        None,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("12 of 20 bytes"), "{msg}");
    assert!(!dest.exists(), "no truncated file is renamed into place");
    assert_eq!(
        std::fs::read(dest.with_file_name("Book.aaxc.part")).unwrap(),
        &FULL[..12],
        "the partial survives for the next resume"
    );
}

/// A transfer that produces more bytes than expected errors and discards
/// the partial (audit A9): the remote file changed, so an appended-to
/// partial would be head-of-old + tail-of-new.
#[tokio::test]
async fn an_oversized_transfer_discards_the_partial_and_errors() {
    let server = MockServer::start().await;
    let bigger: &[u8] = b"0123456789ABCDEFGHIJKLMNO"; // 25 bytes, expected 20
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bigger))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    let client = make_client(&server);
    let err = download_to_file(
        &client,
        &format!("{}/file", server.uri()),
        &dest,
        Some(20),
        false,
        None,
        &[],
        &[],
        None,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("25 bytes") && msg.contains("20"), "{msg}");
    assert!(!dest.exists());
    assert!(
        !dest.with_file_name("Book.aaxc.part").exists(),
        "a mixed-content partial must not survive"
    );
}

/// A9: a `.part` from a different content version is discarded and the
/// file re-fetched clean — the CDN gives no HTTP validator, so the guard
/// is the license version stamped in a `<part>.ver` marker. This catches
/// the one case the size check cannot: a corrected re-release at the
/// exact same byte length.
#[tokio::test]
async fn a_partial_of_a_different_version_is_discarded() {
    let server = MockServer::start().await;
    // The whole file is served fresh (no Range honoured) — a clean
    // restart must fetch all 20 bytes, not resume the 8 stale ones.
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(FULL))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    // A stale 8-byte partial stamped with an OLD version marker.
    std::fs::write(dest.with_file_name("Book.aaxc.part"), b"OLDBYTES").unwrap();
    std::fs::write(dest.with_file_name("Book.aaxc.part.ver"), "CR!OLD:1:1").unwrap();

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
        Some("CR!NEW:2:1"), // the current license is a newer version
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        FULL,
        "the new version is fetched whole; no stale bytes survive"
    );
    // Both the partial and its marker are gone after completion.
    assert!(!dest.with_file_name("Book.aaxc.part").exists());
    assert!(!dest.with_file_name("Book.aaxc.part.ver").exists());
}

/// A9: a `.part` with a MATCHING version marker is resumed normally, and
/// the marker is cleaned up on completion.
#[tokio::test]
async fn a_partial_of_the_same_version_resumes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .and(header("range", "bytes=8-"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(&FULL[8..]))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Book.aaxc");
    std::fs::write(dest.with_file_name("Book.aaxc.part"), &FULL[..8]).unwrap();
    std::fs::write(dest.with_file_name("Book.aaxc.part.ver"), "CR!SAME:7:1").unwrap();

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
        Some("CR!SAME:7:1"),
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(std::fs::read(&dest).unwrap(), FULL, "resumed to completion");
    assert!(!dest.with_file_name("Book.aaxc.part.ver").exists());
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
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome, DownloadOutcome::Downloaded);
    assert_eq!(std::fs::read(&dest).unwrap(), FULL);
}
