//! Downloads (archived architecture §10, M3). The download path requests a
//! content license for the `Adrm` DRM type — a single aaxc/aax file
//! plus an encrypted voucher — mirroring mkb79/audible-cli (the modern
//! app uses FairPlay HLS, which we deliberately do not target).
//!
//! Parallel downloads via `buffer_unordered`, streaming to disk without
//! buffering whole files; voucher decrypt and the `download` command
//! land in later M3 commits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use futures::StreamExt as _;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, RANGE};
use tokio::io::AsyncWriteExt as _;

use crate::api::client::{ApiError, AuthMode, Client, echoed_request_id};
use crate::models::content::DownloadLicense;

/// Fallback device type (the Audible iOS app) when the auth file
/// predates the typed `device.device_type`.
const DEFAULT_DEVICE_TYPE: &str = "A2CZJZGLK2JJVM";

/// `drm_type` for the chapter metadata request (audible-cli sends `Adrm`
/// here; combined with no `acr`/`file_version` it returns the curated
/// chapter titles).
const CHAPTER_DRM_TYPE: &str = "Adrm";

/// Errors raised while downloading a file.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The API/license request failed.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The HTTP transfer failed.
    #[error("download transfer failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Filesystem access failed.
    #[error("download IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// The server answered with a non-success status.
    #[error("download failed with HTTP status {0}")]
    Status(reqwest::StatusCode),
    /// The response Content-Type did not match the expected type(s). `message`
    /// is empty or a `: <body snippet>` for a text-like body (an HTML/JSON error
    /// page); a binary/media body is not echoed.
    #[error(
        "unexpected content type {got:?} (expected one of: {expected}){message}. \
         If this content type is legitimate, please report it so it can be added to the allow-list."
    )]
    ContentType {
        expected: String,
        got: String,
        message: String,
    },
    /// A multi-part title: the server returns a text body asking to download
    /// the parts individually instead of the audio stream.
    #[error("this title must be downloaded in parts, not as one file: {0}")]
    MultipartTitle(String),
    /// The transfer ended below the expected byte count without a stream
    /// error (e.g. the server closed early). The partial is kept — the
    /// next run resumes from it.
    #[error("transfer ended at {written} of {expected} bytes — the partial is kept for resume")]
    ShortTransfer { written: u64, expected: u64 },
    /// The transfer produced more bytes than expected: the remote file
    /// differs from the expectation (e.g. a corrected release), so a
    /// resumed partial would be head-of-old + tail-of-new. The partial is
    /// discarded; the next run starts clean.
    #[error(
        "transfer produced {written} bytes but {expected} were expected — the remote \
         file seems to have changed; the partial was discarded, retry to start clean"
    )]
    SizeMismatch { written: u64, expected: u64 },
}

/// Outcome of a resumable download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// The file was downloaded (fully or resumed to completion).
    Downloaded,
    /// The file already existed complete; nothing transferred.
    AlreadyComplete,
}

/// Downloads `url` to `dest` with resume support, authenticated through
/// the account's `Client` — content delivery URLs require the auth flow
/// (a plain GET is rejected with 403).
///
/// Progress is staged in a sibling `<dest>.part` file. If a partial
/// exists, a `Range` request continues from its size (HTTP 206); a
/// `200` means the server ignored the range, so the partial is
/// restarted; a `416` discards the partial and restarts cleanly (see
/// [`stream_to_file`] for the resume-validation rules). On success the
/// part file is renamed onto `dest`. Multi-GB transfers therefore
/// survive an abort and resume on the next run.
///
/// With `progress` (a `MultiProgress` to attach to), a single-line progress
/// bar is drawn while transferring (resume-aware; a spinner when the total
/// size is unknown). The caller decides visibility (TTY and not `--quiet`)
/// and which transfers get a bar at all (only the heavy ones); the bar is
/// only created once a transfer actually starts, so a skipped/complete file
/// shows nothing.
#[allow(clippy::too_many_arguments)]
pub async fn download_to_file(
    client: &Client,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
    force: bool,
    progress: Option<&MultiProgress>,
    expected_content_type: &[&str],
    ext_overrides: &[(&str, &str)],
    version_tag: Option<&str>,
) -> Result<(DownloadOutcome, PathBuf), DownloadError> {
    stream_to_file(
        dest,
        expected_size,
        force,
        progress,
        expected_content_type,
        ext_overrides,
        url,
        version_tag,
        // Auth flow required (a plain GET 403s); a Range header only when
        // actually resuming.
        |offset| async move {
            let mut request = client.authed_get(url).await?;
            if offset > 0 {
                request = request.header(RANGE, format!("bytes={offset}-"));
            }
            Ok(request)
        },
    )
    .await
}

/// Player User-Agent for the CENC content host (the Android app's media stack).
pub const CENC_USER_AGENT: &str =
    "com.audible.playersdk.player/3.79.0 (Linux;Android 11) AndroidXMedia3/1.3.0";

/// A plain, **uncompressed** HTTP client (no auth) with the same connect/read
/// timeouts as the API client (AUD-98). Used for the signed CloudFront URLs —
/// the CENC content download and the MPD/text fetch — which 403 on the API
/// client's `Accept-Encoding: gzip, br`. Without the timeouts a stalled
/// connection (worst case a multi-GB CENC transfer) would hang forever.
pub fn plain_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .read_timeout(crate::api::client::READ_TIMEOUT)
        .build()
}

/// Downloads a signed CENC/DASH content URL to `dest`, with resume. Unlike
/// [`download_to_file`] this uses a plain, **uncompressed** client with a player
/// User-Agent and no auth: the signed CloudFront URL 403s on the API client's
/// `Accept-Encoding: gzip, br` and requires a ranged request (`bytes=0-`).
pub async fn download_cenc_to_file(
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
    force: bool,
    progress: Option<&MultiProgress>,
    expected_content_type: &[&str],
    version_tag: Option<&str>,
) -> Result<(DownloadOutcome, PathBuf), DownloadError> {
    stream_to_file(
        dest,
        expected_size,
        force,
        progress,
        expected_content_type,
        &[],
        url,
        version_tag,
        // The CENC host requires a ranged request even from zero
        // (`bytes=0-`) and 403s on the API client's Accept-Encoding, so
        // this path uses the plain client with the player User-Agent.
        |offset| async move {
            Ok(plain_http_client()?
                .get(url)
                .header(reqwest::header::USER_AGENT, CENC_USER_AGENT)
                .header(RANGE, format!("bytes={offset}-")))
        },
    )
    .await
}

/// The one resumable transfer engine behind [`download_to_file`] and
/// [`download_cenc_to_file`] (audit 2026-07-17, D1 — the resume logic had
/// been patched into both copies once already). `build_request` turns a
/// resume offset into a ready-to-send request; auth, User-Agent and
/// whether a zero-offset `Range` is sent differ per caller, everything
/// else is shared. It may be called twice: once for the resume attempt
/// and once more when a `416` forces a clean restart.
///
/// Progress is staged in a sibling `<dest>.part`. A partial resumes via
/// `Range` (206); a `200` means the server ignored the range and the
/// partial restarts; a `416` no longer trusts the partial (see below).
/// On success the part file is renamed onto the final destination.
///
/// Resume validation (audit 2026-07-17, A9). The CDN gives no HTTP
/// validator on content responses — no `ETag`, no `Last-Modified`,
/// `x-amz-version-id: null` (confirmed against live captures) — so
/// `If-Range` is impossible; the version identity comes from the license
/// instead, via `version_tag` (`acr:version:file_version`, which the
/// signed URL also encodes in its path). The three guards:
/// - **version marker**: a resumable `.part` is trusted only when its
///   sibling `<part>.ver` matches `version_tag`. A corrected re-release
///   (new acr/version, even at the *same* byte length — the one case the
///   size check below cannot catch) mismatches, so the partial is
///   discarded and the transfer restarts clean.
/// - **416**: proves only that the partial is at/past the remote EOF — a
///   shrunk replacement file (PDF/cover re-issue, `expected_size`
///   unknown) would otherwise strand truncated old bytes as "complete".
///   The partial is discarded and the transfer restarts from zero.
/// - **post-transfer byte count** must equal `expected_size` (when
///   known): short = kept partial + error, long = the remote file
///   changed → partial discarded + error. Never a silent rename.
///
/// `version_tag` is `None` for artifacts with no license version (covers,
/// PDFs, chapters): those keep only the 416 + size guards.
#[allow(clippy::too_many_arguments)]
async fn stream_to_file<Fut>(
    dest: &Path,
    expected_size: Option<u64>,
    force: bool,
    progress: Option<&MultiProgress>,
    expected_content_type: &[&str],
    ext_overrides: &[(&str, &str)],
    url: &str,
    version_tag: Option<&str>,
    build_request: impl Fn(u64) -> Fut,
) -> Result<(DownloadOutcome, PathBuf), DownloadError>
where
    Fut: std::future::Future<Output = Result<reqwest::RequestBuilder, DownloadError>>,
{
    // `force` re-downloads from scratch, ignoring an existing complete
    // file and any partial — used by `--force`/`--relicense`. The check
    // covers the planned path plus every extension-corrected candidate:
    // an audio served as `audio/mpeg` landed as `X.mp3`, and probing only
    // the planned `X.aaxc` re-transferred it on every record-less run.
    if !force && let Some(size) = expected_size {
        let candidates = std::iter::once(dest.to_path_buf()).chain(
            ext_overrides
                .iter()
                .map(|(_, ext)| dest.with_extension(ext)),
        );
        for candidate in candidates {
            if let Ok(meta) = tokio::fs::metadata(&candidate).await
                && meta.len() == size
            {
                return Ok((DownloadOutcome::AlreadyComplete, candidate));
            }
        }
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let part = part_path(dest);
    let marker = version_marker_path(&part);
    let mut offset = if force {
        0
    } else {
        match tokio::fs::metadata(&part).await {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        }
    };
    // Version-gate a resumable partial (A9): trust it only when its
    // `<part>.ver` marker matches the license version we are about to
    // download with. A corrected re-release mismatches (or a
    // marker-less partial from before this check exists) → discard and
    // restart clean, so no head-of-old + tail-of-new can survive.
    if offset > 0
        && let Some(tag) = version_tag
    {
        let recorded = tokio::fs::read_to_string(&marker).await.ok();
        if recorded.as_deref() != Some(tag) {
            tracing::warn!(
                partial = %part.display(),
                "the partial belongs to a different content version — discarding and \
                 restarting from scratch"
            );
            tokio::fs::remove_file(&part).await?;
            let _ = tokio::fs::remove_file(&marker).await;
            offset = 0;
        }
    }
    // Verify a resumable partial against the expected size before asking
    // the server: an oversized partial is corrupt (restart from scratch),
    // a partial exactly at the expected size needs no request at all.
    if let Some(size) = expected_size {
        if offset > size {
            tracing::warn!(
                partial = %part.display(),
                partial_len = offset,
                expected = size,
                "partial is larger than the expected file — restarting"
            );
            tokio::fs::remove_file(&part).await?;
            offset = 0;
        } else if offset == size && offset > 0 {
            tokio::fs::rename(&part, dest).await?;
            let _ = tokio::fs::remove_file(&marker).await;
            return Ok((DownloadOutcome::Downloaded, dest.to_path_buf()));
        }
    }

    let mut response = build_request(offset).await?.send().await?;
    let mut status = response.status();

    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        // A 416 only proves the partial is at/past the remote EOF, not
        // that its bytes are the file's. With the expected size known
        // this point is only reached on an inconsistency (a complete
        // partial finishes above without a request); without one nothing
        // can vouch for the partial at all — renaming it here stranded a
        // shrunk replacement's truncated old bytes as "complete".
        // Restart cleanly instead.
        tracing::warn!(
            partial = %part.display(),
            partial_len = offset,
            "server says the resume offset is past EOF — discarding the partial \
             and restarting from scratch"
        );
        tokio::fs::remove_file(&part).await?;
        offset = 0;
        response = build_request(0).await?.send().await?;
        status = response.status();
    }
    let content_length = response.content_length();

    let mut append = offset > 0;
    if status == reqwest::StatusCode::OK {
        // Server ignored the range: restart from scratch.
        append = false;
        offset = 0;
    } else if status != reqwest::StatusCode::PARTIAL_CONTENT && !status.is_success() {
        return Err(DownloadError::Status(status));
    }

    // The response Content-Type drives both verification and the final
    // on-disk extension.
    let got_content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // Verify the type (only when the server sent one) so a wrong or error
    // response (an HTML/JSON error page, an expired URL, or a multi-part
    // title's "download the parts" text) is not streamed onto disk.
    if !expected_content_type.is_empty()
        && let Some(got) = &got_content_type
        && !content_type_matches(got, expected_content_type)
    {
        return Err(content_type_error(response, got, expected_content_type).await);
    }

    // Final destination: correct the extension from the Content-Type when an
    // override matches (e.g. `audio/mpeg` → `mp3`); else keep `dest`'s.
    let final_dest = got_content_type
        .as_deref()
        .and_then(|ct| extension_override(ct, ext_overrides))
        .map(|ext| dest.with_extension(ext))
        .unwrap_or_else(|| dest.to_path_buf());

    tracing::info!(
        url = redact_url(url),
        resumed_from = offset,
        partial = %part.display(),
        "downloading"
    );

    // Full size: the caller's expectation, else the response length plus
    // what we already have (Content-Length is the remaining bytes on 206).
    let total = expected_size.or_else(|| content_length.map(|len| offset + len));
    let bar = progress.map(|multi| {
        let name = final_dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bar = multi.add(progress_bar(total, &name));
        bar.set_position(offset);
        bar
    });

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part)
        .await?;

    // Stamp the partial's content version so a later resume can trust
    // (or reject) it. Written for a fresh start and a validated resume
    // alike; best-effort — a missing marker just forces a clean restart
    // next time, never a wrong resume.
    if let Some(tag) = version_tag {
        let _ = tokio::fs::write(&marker, tag).await;
    }

    let mut stream = response.bytes_stream();
    let mut written = offset;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if let Some(bar) = &bar {
            bar.inc(chunk.len() as u64);
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }

    // A transfer that ends at the wrong byte count is never renamed into
    // place as if it were the file.
    if let Some(size) = expected_size
        && written != size
    {
        if written > size {
            // More data than expected: the remote file changed — an
            // appended-to partial would be head-of-old + tail-of-new.
            tokio::fs::remove_file(&part).await?;
            let _ = tokio::fs::remove_file(&marker).await;
            return Err(DownloadError::SizeMismatch {
                written,
                expected: size,
            });
        }
        // Short: the stream ended early without an error. Keep the
        // partial (and its version marker) — the next run resumes there.
        return Err(DownloadError::ShortTransfer {
            written,
            expected: size,
        });
    }

    tokio::fs::rename(&part, &final_dest).await?;
    // The completed file needs no version marker.
    let _ = tokio::fs::remove_file(&marker).await;
    Ok((DownloadOutcome::Downloaded, final_dest))
}

/// The sibling marker of a `.part` that records the content version it was
/// written for (A9). Kept next to the partial and removed on completion.
fn version_marker_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".ver");
    part.with_file_name(name)
}

/// The extension to use for a response `Content-Type`, if an override matches
/// (exact, case-insensitive, parameters like `; charset=…` ignored). `None`
/// keeps the caller's default extension.
fn extension_override<'a>(content_type: &str, overrides: &[(&str, &'a str)]) -> Option<&'a str> {
    let got = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    overrides
        .iter()
        .find(|(ct, _)| got == ct.to_ascii_lowercase())
        .map(|&(_, ext)| ext)
}

/// Whether a non-audio text body is Audible's "download the parts
/// individually" message for a multi-part title.
fn is_multipart_message(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("individual part") || lower.contains("download the parts")
}

/// Case-insensitive membership test for a response `Content-Type` against a
/// kind's expected types, ignoring parameters like `; charset=…`. An empty
/// expected set means "no check" (always matches).
fn content_type_matches(got: &str, expected: &[&str]) -> bool {
    if expected.is_empty() {
        return true;
    }
    let got = got.split(';').next().unwrap_or(got).trim();
    expected.iter().any(|kind| kind.eq_ignore_ascii_case(got))
}

/// Whether a content type carries a human-readable body worth echoing in an
/// error (an HTML/JSON/XML/plain error page), as opposed to binary media.
fn is_text_like(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

/// Builds the error for a content-type mismatch, consuming the response body.
/// Only text-like bodies are read and echoed (a "download the parts" notice
/// becomes a [`DownloadError::MultipartTitle`]; an HTML/JSON error page is
/// quoted in the message); a legitimate-but-mistyped media body is binary and
/// possibly huge, so it is neither read nor echoed — just the type is reported.
async fn content_type_error(
    response: reqwest::Response,
    got: &str,
    expected: &[&str],
) -> DownloadError {
    let message = if is_text_like(got) {
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        if is_multipart_message(&body) {
            return DownloadError::MultipartTitle(body.trim().to_owned());
        }
        format!(": {}", body.trim())
    } else {
        String::new()
    };
    DownloadError::ContentType {
        expected: expected.join(", "),
        got: got.to_owned(),
        message,
    }
}

// Single-line bar templates, widest first. Narrow terminals (phones over
// SSH) drop rate/ETA, then the byte counts, so `{wide_bar}` always keeps
// room instead of collapsing to `[]`.
const BAR_FULL: &str = "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] \
                        {bytes}/{total_bytes} ({binary_bytes_per_sec}, eta {eta})";
const BAR_MID: &str =
    "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {percent:>3}%";
const BAR_NARROW: &str = "{spinner:.green} {msg} [{wide_bar:.cyan/blue}] {percent:>3}%";
const SPINNER_TEMPLATE: &str = "{spinner:.green} {msg} {bytes} ({binary_bytes_per_sec})";

/// Picks the determinate template and the label budget for a terminal
/// width: the wider the terminal, the more fields and label we show.
fn bar_layout(cols: usize) -> (&'static str, usize) {
    if cols >= 96 {
        (BAR_FULL, 28)
    } else if cols >= 66 {
        (BAR_MID, 18)
    } else {
        (BAR_NARROW, 12)
    }
}

/// A single-line download progress bar (resume-aware), adapted to the
/// terminal width: a determinate bar with bytes/rate/ETA when there is
/// room, trimmed down on narrow terminals; a spinner when the total size
/// is unknown. The label is truncated so the bar never collapses.
fn progress_bar(total: Option<u64>, name: &str) -> ProgressBar {
    let cols = console::Term::stderr()
        .size_checked()
        .map(|(_, cols)| cols as usize)
        .unwrap_or(80);

    let (bar, label_budget) = match total {
        Some(total) => {
            let (template, budget) = bar_layout(cols);
            let bar = ProgressBar::new(total);
            bar.set_style(
                ProgressStyle::with_template(template)
                    .expect("valid progress template")
                    .progress_chars("█▉▊▋▌▍▎▏ "),
            );
            (bar, budget)
        }
        None => {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template(SPINNER_TEMPLATE).expect("valid spinner template"),
            );
            (bar, if cols >= 66 { 28 } else { 16 })
        }
    };
    bar.set_message(truncate_label(name, label_budget));
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

/// Truncates a label to at most `max` characters, cutting in the middle
/// with an ellipsis so both the title (start) and the file extension
/// (end) stay visible.
fn truncate_label(label: &str, max: usize) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= max {
        return label.to_owned();
    }
    // One slot for the ellipsis; give the slightly larger half to the
    // tail so the extension survives on tight budgets.
    let keep = max.saturating_sub(1);
    let tail = keep / 2;
    let head = keep - tail;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// Suffix of an in-progress partial download.
const PART_SUFFIX: &str = ".part";
/// Suffix of a partial's version marker (`<dest>.part.ver`, see
/// [`version_marker_path`]).
const VERSION_MARKER_SUFFIX: &str = ".part.ver";

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(PART_SUFFIX);
    dest.with_file_name(name)
}

/// Whether `name` is resume data of an in-progress `download`: the `.part`
/// partial or its `.part.ver` version marker. The one home for the
/// resume-suffix knowledge — `download orphans` consumes it, because resume
/// data is reported but never deleted there.
pub(crate) fn is_resume_artifact(name: &str) -> bool {
    name.ends_with(PART_SUFFIX) || name.ends_with(VERSION_MARKER_SUFFIX)
}

/// Drops the query string (CloudFront signature) from a URL for logging.
fn redact_url(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the resume writers to the predicate: whatever `part_path` and
    /// `version_marker_path` produce must count as a resume artifact, and
    /// final files or key sidecars must not.
    #[test]
    fn resume_artifacts_are_recognized() {
        let dest = Path::new("/dl/Book.AAX_44_128.aaxc");
        let part = part_path(dest);
        let marker = version_marker_path(&part);
        for path in [&part, &marker] {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(is_resume_artifact(&name), "{name}");
        }
        for name in [
            "Book.AAX_44_128.aaxc",
            "Book.AAX_44_128.voucher",
            "Note.ver",
        ] {
            assert!(!is_resume_artifact(name), "{name}");
        }
    }

    #[test]
    fn decode_annotations_treats_no_annotations_as_none() {
        use reqwest::StatusCode;
        // 404: the title has no annotations.
        assert!(matches!(
            decode_annotations(StatusCode::NOT_FOUND, b""),
            AnnotationBody::None
        ));
        assert!(matches!(
            decode_annotations(StatusCode::NOT_FOUND, b"Not Found"),
            AnnotationBody::None
        ));
        // Empty / whitespace body on a 2xx.
        assert!(matches!(
            decode_annotations(StatusCode::OK, b""),
            AnnotationBody::None
        ));
        assert!(matches!(
            decode_annotations(StatusCode::OK, b"  \n "),
            AnnotationBody::None
        ));
        // A non-empty non-JSON 2xx is a FAILURE, not "none" — recording it
        // as none would permanently skip the item's real bookmarks (A14).
        assert!(matches!(
            decode_annotations(StatusCode::OK, b"<html>nope</html>"),
            AnnotationBody::Unparseable
        ));
        // Valid JSON is the annotation payload.
        let AnnotationBody::Payload(payload) =
            decode_annotations(StatusCode::OK, br#"{"md5":"x","payload":{}}"#)
        else {
            panic!("valid JSON decodes");
        };
        assert_eq!(payload["md5"], "x");
    }

    #[test]
    fn widevine_grant_extracts_pdf_url() {
        // A Widevine grant carrying a companion PDF under content_metadata.
        let payload = serde_json::json!({
            "content_license": {
                "drm_type": "Widevine",
                "license_response": "https://mpd.example/manifest.mpd",
                "content_metadata": {
                    "pdf_url": "https://cds.example/companion.pdf",
                    "content_reference": {
                        "acr": "CR!ABC", "version": "42", "sku": "SKU1",
                        "content_size_in_bytes": 123,
                    }
                }
            }
        });
        let grant = parse_widevine_grant(&payload, "B01").expect("parses");
        match grant {
            WidevineGrant::Widevine(license) => {
                assert_eq!(license.mpd_url, "https://mpd.example/manifest.mpd");
                assert_eq!(
                    license.pdf_url.as_deref(),
                    Some("https://cds.example/companion.pdf")
                );
                assert_eq!(license.content_size, Some(123));
                // A9: the CENC resume gate depends on this being populated
                // from content_reference (acr:version). The version is the
                // same identifier that appears in the signed CENC URL path
                // (verified live), so a re-release changes it.
                assert_eq!(license.acr.as_deref(), Some("CR!ABC"));
                assert_eq!(license.version.as_deref(), Some("42"));
                assert_eq!(license.version_tag().as_deref(), Some("CR!ABC:42"));
            }
            WidevineGrant::Mpeg(_) => panic!("expected a Widevine grant"),
        }
    }

    #[test]
    fn widevine_grant_without_pdf_is_none() {
        // No pdf_url in the response (the common case) → None, not an error.
        let payload = serde_json::json!({
            "content_license": {
                "drm_type": "Widevine",
                "license_response": "https://mpd.example/manifest.mpd",
                "content_metadata": { "content_reference": { "acr": "CR!ABC" } }
            }
        });
        match parse_widevine_grant(&payload, "B01").expect("parses") {
            WidevineGrant::Widevine(license) => assert!(license.pdf_url.is_none()),
            WidevineGrant::Mpeg(_) => panic!("expected a Widevine grant"),
        }
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(
            part_path(Path::new("/a/Book.aaxc")),
            Path::new("/a/Book.aaxc.part")
        );
    }

    #[test]
    fn redact_strips_query() {
        assert_eq!(
            redact_url("https://cds.audible.de/x.aaxc?Policy=secret&Signature=s"),
            "https://cds.audible.de/x.aaxc"
        );
    }

    #[test]
    fn content_type_matching() {
        let audio = [
            "audio/aax",
            "audio/vnd.audible.aax",
            "audio/mpeg",
            "audio/mp3",
            "audio/mp4",
            "audio/x-m4a",
            "audio/audible",
        ];
        assert!(content_type_matches("audio/mpeg", &audio)); // mp3, not aaxc
        assert!(content_type_matches("audio/mp3", &audio)); // non-standard mp3 alias (AUD-159)
        assert!(content_type_matches("audio/mp4", &audio)); // AAC-in-MP4 episode (AUD-159)
        assert!(content_type_matches("audio/x-m4a", &audio));
        assert!(content_type_matches("AUDIO/AAX", &audio)); // case-insensitive
        assert!(content_type_matches("audio/aax; charset=binary", &audio)); // param stripped
        assert!(!content_type_matches("text/html", &audio));
        let pdf = ["application/octet-stream", "application/pdf"];
        assert!(content_type_matches("application/pdf; charset=utf-8", &pdf));
        // empty expected set → no check
        assert!(content_type_matches("text/html", &[]));
    }

    #[test]
    fn content_type_error_reports_type_and_invites_report() {
        // A legitimate-but-mistyped media body carries no snippet — just the
        // type, plus the hint so a user knows to report it.
        let media = DownloadError::ContentType {
            expected: "audio/mp4, video/mp4".into(),
            got: "audio/mp4a-latm".into(),
            message: String::new(),
        };
        let s = media.to_string();
        assert!(s.contains("audio/mp4a-latm"), "shows the actual type: {s}");
        assert!(s.contains("expected one of: audio/mp4, video/mp4"));
        assert!(s.contains("please report it"), "invites a report: {s}");
        assert!(!s.contains("body"), "no binary snippet: {s}");
        // A text body (HTML error page) is quoted.
        let html = DownloadError::ContentType {
            expected: "application/pdf".into(),
            got: "text/html".into(),
            message: ": <html>nope</html>".into(),
        };
        assert!(html.to_string().contains("<html>nope</html>"));
    }

    #[test]
    fn text_like_vs_binary() {
        // Text-like bodies are worth echoing in a content-type error.
        assert!(is_text_like("text/html"));
        assert!(is_text_like("text/html; charset=UTF-8"));
        assert!(is_text_like("application/json"));
        assert!(is_text_like("application/problem+json"));
        assert!(is_text_like("application/xml"));
        assert!(is_text_like("image/svg+xml"));
        // Binary / media bodies are not.
        assert!(!is_text_like("audio/mp4"));
        assert!(!is_text_like("audio/mpeg"));
        assert!(!is_text_like("application/octet-stream"));
        assert!(!is_text_like("image/jpeg"));
    }

    #[test]
    fn extension_from_content_type() {
        // Plain media gets its real extension; aax variants keep .aaxc.
        let audio = [
            ("audio/mpeg", "mp3"),
            ("audio/mp3", "mp3"),
            ("audio/mp4", "m4a"),
            ("audio/x-m4a", "m4a"),
        ];
        assert_eq!(extension_override("audio/mpeg", &audio), Some("mp3"));
        assert_eq!(extension_override("audio/mp3", &audio), Some("mp3")); // AUD-159
        assert_eq!(extension_override("audio/mp4", &audio), Some("m4a")); // AUD-159
        assert_eq!(
            extension_override("audio/mp4; charset=binary", &audio),
            Some("m4a")
        );
        assert_eq!(extension_override("AUDIO/X-M4A", &audio), Some("m4a"));
        assert_eq!(extension_override("AUDIO/MPEG", &audio), Some("mp3"));
        assert_eq!(extension_override("audio/vnd.audible.aax", &audio), None);
        // No overrides → never changes the extension.
        assert_eq!(extension_override("audio/mpeg", &[]), None);
    }

    #[test]
    fn multipart_message_detection() {
        assert!(is_multipart_message(
            "Please download the individual parts of this title."
        ));
        assert!(is_multipart_message("download the parts separately"));
        assert!(!is_multipart_message("<html>error page</html>"));
        assert!(!is_multipart_message(""));
    }

    #[test]
    fn progress_templates_are_valid_at_every_width() {
        for template in [BAR_FULL, BAR_MID, BAR_NARROW, SPINNER_TEMPLATE] {
            ProgressStyle::with_template(template).expect("template parses");
        }
        // Both constructors must build without panicking too.
        progress_bar(Some(100), "Book.aaxc").finish_and_clear();
        progress_bar(None, "Book.pdf").finish_and_clear();
    }

    #[test]
    fn narrower_terminals_drop_fields_and_shrink_the_label() {
        assert_eq!(bar_layout(120).0, BAR_FULL);
        assert_eq!(bar_layout(80).0, BAR_MID);
        assert_eq!(bar_layout(40).0, BAR_NARROW);
        // Label budget shrinks with width.
        assert!(bar_layout(120).1 > bar_layout(40).1);
    }

    #[test]
    fn annotation_url_adds_guid_and_format_when_known() {
        assert_eq!(
            annotation_url("B0D186SQWV", None, None, None),
            "https://cde-ta-g7g.amazon.com/FionaCDEServiceEngine/sidecar?type=AUDI&key=B0D186SQWV"
        );
        assert_eq!(
            annotation_url(
                "B0D186SQWV",
                Some("CR!ABC"),
                Some("63671221"),
                Some("AAX_44_128")
            ),
            "https://cde-ta-g7g.amazon.com/FionaCDEServiceEngine/sidecar?type=AUDI&key=B0D186SQWV\
             &guid=CR!ABC:63671221&format=AAX_44_128"
        );
        // guid needs both acr and version, or it is omitted.
        assert!(!annotation_url("B0D186SQWV", Some("CR!ABC"), None, None).contains("guid="));
    }

    #[test]
    fn parse_license_error_extracts_code_and_message() {
        let body = r#"{"error_code":"000307","message":"Unable to retrieve asset details"}"#;
        let (code, message) = parse_license_error(body);
        assert_eq!(code, "000307");
        assert_eq!(message, "Unable to retrieve asset details");
    }

    #[test]
    fn parse_license_error_falls_back_for_non_json() {
        let (code, message) = parse_license_error("  <html>nope</html>  ");
        assert!(code.is_empty());
        assert_eq!(message, "<html>nope</html>");
    }

    #[test]
    fn truncate_for_error_caps_on_a_char_boundary() {
        let long = "ä".repeat(600);
        let out = truncate_for_error(&long);
        // 500 multi-byte chars plus the ellipsis, never a split codepoint.
        assert_eq!(out.chars().count(), 501);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_label_keeps_both_ends() {
        assert_eq!(truncate_label("short", 10), "short");
        // Middle cut: keeps the start (title) and the end (extension).
        let out = truncate_label("Book_Title.AAX_44_128.aaxc", 14);
        assert_eq!(out.chars().count(), 14);
        assert!(out.starts_with("Book"), "{out}");
        assert!(out.ends_with(".aaxc"), "{out}");
        assert!(out.contains('…'), "{out}");
    }
}

/// Download audio quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Highest available.
    High,
    /// Standard.
    Normal,
}

impl Quality {
    /// The `quality` query/body value the API expects (`High`/`Normal`).
    pub fn api_value(self) -> &'static str {
        match self {
            Quality::High => "High",
            Quality::Normal => "Normal",
        }
    }
}

/// Sends one `/1.0/content/{asin}/<endpoint>` POST and decodes the shared
/// licenserequest protocol (audit 2026-07-17, D5 — this lived three
/// times). The body is read as text first: a rejected request answers
/// with a small JSON error (`error_code`/`message`) surfaced verbatim as
/// [`ApiError::LicenseRejected`] together with the echoed request id;
/// a success body must parse as JSON or is [`ApiError::LicenseResponse`].
/// `app_headers` adds the app-parity block (X-ADP-*, device type/idiom)
/// both licenserequest flavors send; `drmlicense` sends none.
async fn send_licenserequest(
    client: &Client,
    country_code: &str,
    asin: &str,
    endpoint: &str,
    body: serde_json::Value,
    app_headers: bool,
) -> Result<serde_json::Value, ApiError> {
    let mut request = client
        .request(Method::POST, format!("/1.0/content/{asin}/{endpoint}"))
        .country_code(country_code)
        // Auto = the access token here: this content endpoint accepts it,
        // and only the CDE-Sidecar forces signing (AUD-195).
        .auth(AuthMode::Auto);
    if app_headers {
        // X-Device-Type-Id from the registered device (falls back to the
        // iOS app type only if the auth file predates the typed device).
        let device_type = client.device_type().unwrap_or(DEFAULT_DEVICE_TYPE);
        request = request
            .header("X-ADP-Transport", "WIFI")
            .header("X-ADP-LTO", "120")
            .header("X-Device-Type-Id", device_type)
            .header("device_idiom", "phone");
    }
    let response = request.body(body).send().await?;
    let status = response.status();
    let request_id = echoed_request_id(&response);
    let text = response.text().await?;
    if !status.is_success() {
        let (error_code, message) = parse_license_error(&text);
        return Err(ApiError::LicenseRejected {
            asin: asin.to_owned(),
            status,
            error_code,
            message,
            request_id: request_id.unwrap_or_else(|| "-".into()),
        });
    }
    serde_json::from_str(&text).map_err(|_| ApiError::LicenseResponse(asin.to_owned()))
}

/// Requests a download license for `asin` via the Adrm path.
///
/// Headers mirror the reference client; auth is `Auto`, which the access
/// token serves here — the token is what the endpoint validates (AUD-195).
pub async fn request_license(
    client: &Client,
    country_code: &str,
    asin: &str,
    quality: Quality,
) -> Result<DownloadLicense, ApiError> {
    let body = serde_json::json!({
        "supported_drm_types": ["Mpeg", "Adrm"],
        "quality": quality.api_value(),
        "consumption_type": "Download",
        // chapter_info is fetched separately via the metadata endpoint.
        "response_groups": "last_position_heard,pdf_url,content_reference",
    });

    let payload =
        send_licenserequest(client, country_code, asin, "licenserequest", body, true).await?;
    DownloadLicense::from_response(payload)
        .ok_or_else(|| ApiError::LicenseResponse(asin.to_owned()))
}

/// A granted Widevine (DASH/CENC) license: the MPD URL plus the metadata we
/// record. The content keys come later (`drmlicense` + the Widevine client).
#[derive(Debug, Clone)]
pub struct WidevineLicense {
    /// `content_license.license_response` — the DASH MPD URL.
    pub mpd_url: String,
    /// Audible Content Reference.
    pub acr: Option<String>,
    /// Content version.
    pub version: Option<String>,
    /// SKU.
    pub sku: Option<String>,
    /// Encrypted file size in bytes (cross-check for the derived quality).
    pub content_size: Option<u64>,
    /// Companion PDF URL, if the title has one and `pdf_url` was requested.
    pub pdf_url: Option<String>,
}

impl WidevineLicense {
    /// The content-version identity for CENC resume validation (A9):
    /// `acr:version`. `None` when the grant carries neither.
    pub fn version_tag(&self) -> Option<String> {
        if self.acr.is_none() && self.version.is_none() {
            return None;
        }
        Some(format!(
            "{}:{}",
            self.acr.as_deref().unwrap_or(""),
            self.version.as_deref().unwrap_or("")
        ))
    }
}

/// A Mpeg fallback grant from a `[Widevine, Mpeg]` request — a plain, DRM-free
/// MP3 `offline_url` (podcasts / titles with no Widevine or aax asset).
#[derive(Debug, Clone)]
pub struct MpegGrant {
    /// Direct MP3 download URL.
    pub offline_url: String,
    /// Content format (`MPEG`).
    pub content_format: String,
    /// File size in bytes, if known.
    pub content_size: Option<u64>,
}

/// What a `[Widevine, Mpeg]` licenserequest granted. Widevine titles get a CENC
/// manifest; podcasts / asset-less titles fall back to a plain MP3.
pub enum WidevineGrant {
    /// Widevine/DASH — a CENC MPD.
    Widevine(WidevineLicense),
    /// Mpeg fallback — a direct MP3 URL (no CDM, no decrypt).
    Mpeg(MpegGrant),
}

/// Codecs to offer for a Widevine licenserequest. AAC-LC (`mp4a.40.2`) is always
/// offered — a universally playable file, and the fallback when nothing better
/// exists. `xhe` also offers xHE-AAC (`mp4a.40.42`, higher quality, limited
/// player support) — the server prefers it when a master exists. `spatial` also
/// offers `ec+3` (E-AC-3 JOC — the Atmos codec the real apps request; `ac-4` is
/// deliberately NOT offered, since even L3 devices are denied it at `drmlicense`
/// — and Atmos as a whole needs Widevine L1).
pub fn widevine_codecs(spatial: bool, xhe: bool) -> Vec<&'static str> {
    let mut codecs = vec!["mp4a.40.2"];
    if xhe {
        codecs.push("mp4a.40.42");
    }
    if spatial {
        codecs.push("ec+3");
    }
    codecs
}

/// Requests a Widevine (DASH/CENC) license for a title — the parallel to
/// [`request_license`] for the Widevine path. Errors if the grant is not
/// `Widevine` (the caller decides whether to fall back to aaxc).
pub async fn request_widevine_license(
    client: &Client,
    country_code: &str,
    asin: &str,
    quality: Quality,
    spatial: bool,
    xhe: bool,
) -> Result<WidevineGrant, ApiError> {
    let body = serde_json::json!({
        "supported_media_features": {
            "drm_types": ["Widevine", "Mpeg"],
            "codecs": widevine_codecs(spatial, xhe),
            "chapter_titles_type": "Tree",
            "previews": false,
            "catalog_samples": false,
        },
        "spatial": spatial,
        "consumption_type": "Download",
        "quality": quality.api_value(),
        "tenant_id": "Audible",
        "response_groups": "content_reference,chapter_info,pdf_url",
    });

    let payload =
        send_licenserequest(client, country_code, asin, "licenserequest", body, true).await?;
    parse_widevine_grant(&payload, asin)
}

/// Parses a `[Widevine, Mpeg]` licenserequest response into a grant. Pure (no
/// I/O) so the format branching stays unit-testable.
fn parse_widevine_grant(
    payload: &serde_json::Value,
    asin: &str,
) -> Result<WidevineGrant, ApiError> {
    let license = &payload["content_license"];
    let metadata = &license["content_metadata"];
    // The content_reference fields share the aaxc path's parser (D9), so
    // the Widevine grant gets the same top-level-`acr` fallback and cannot
    // drift from it.
    let reference = crate::models::content::ContentReference::parse(license);
    let content_size = reference.content_size;
    match license["drm_type"].as_str() {
        Some("Widevine") => Ok(WidevineGrant::Widevine(WidevineLicense {
            mpd_url: license["license_response"]
                .as_str()
                .ok_or_else(|| ApiError::LicenseResponse(asin.to_owned()))?
                .to_owned(),
            acr: reference.acr,
            version: reference.version,
            sku: reference.sku,
            content_size,
            // Same `pdf_url` rule as the aaxc path (content_metadata,
            // top-level fallback) — one home, D6.
            pdf_url: crate::models::content::pdf_url_from_license(license, Some(metadata)),
        })),
        // A Mpeg fallback (podcasts / asset-less titles) — a plain MP3 URL.
        Some("Mpeg") => Ok(WidevineGrant::Mpeg(MpegGrant {
            offline_url: metadata["content_url"]["offline_url"]
                .as_str()
                .ok_or_else(|| ApiError::LicenseResponse(asin.to_owned()))?
                .to_owned(),
            content_format: reference
                .content_format
                .unwrap_or_else(|| "MPEG".to_owned()),
            content_size,
        })),
        _ => Err(ApiError::LicenseResponse(asin.to_owned())),
    }
}

/// Requests the Widevine content license (`drmlicense`) for a title, given the
/// base64-on-the-wire license challenge from the Widevine client. Returns the
/// raw license message bytes (parsed by the client into content keys). The
/// challenge and license carry no plaintext key.
pub async fn request_drmlicense(
    client: &Client,
    country_code: &str,
    asin: &str,
    challenge: &[u8],
) -> Result<Vec<u8>, ApiError> {
    let body = serde_json::json!({
        "consumption_type": "Download",
        "drm_type": "Widevine",
        "tenant_id": "Audible",
        "licenseChallenge": base64::engine::general_purpose::STANDARD.encode(challenge),
    });
    let payload =
        send_licenserequest(client, country_code, asin, "drmlicense", body, false).await?;
    let license_b64 = payload["license"]
        .as_str()
        .ok_or_else(|| ApiError::LicenseResponse(asin.to_owned()))?;
    base64::engine::general_purpose::STANDARD
        .decode(license_b64)
        .map_err(|_| ApiError::LicenseResponse(asin.to_owned()))
}

/// Extracts `error_code` and `message` from a failed licenserequest body.
/// Falls back to a length-capped copy of the raw body when it is not the
/// expected JSON error shape. These error bodies carry no credentials.
fn parse_license_error(body: &str) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let error_code = value
            .get("error_code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| truncate_for_error(body));
        return (error_code, message);
    }
    (String::new(), truncate_for_error(body))
}

/// Caps an error body at a sane length (on a char boundary) so an
/// unexpected large/HTML response cannot flood the error message or logs.
fn truncate_for_error(body: &str) -> String {
    const MAX_CHARS: usize = 500;
    let trimmed = body.trim();
    match trimmed.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

/// Fetches `chapter_info` for `asin` from the content metadata endpoint,
/// needing no license. Mirrors audible-cli's `get_content_metadata`
/// exactly: `drm_type=Adrm` plus `chapter_titles_type` (Flat/Tree) and
/// nothing else. Adding `acr`/`file_version` pins the request to the
/// AAX file's raw segment markers (generic "Kapitel N") instead of the
/// curated chapter titles, so they are deliberately omitted.
pub async fn request_chapters(
    client: &Client,
    country_code: &str,
    asin: &str,
    quality: &str,
    chapter_titles_type: &str,
) -> Result<serde_json::Value, ApiError> {
    let metadata = request_content_metadata(
        client,
        country_code,
        asin,
        CHAPTER_DRM_TYPE,
        quality,
        chapter_titles_type,
    )
    .await?;
    metadata
        .get("chapter_info")
        .cloned()
        .ok_or_else(|| ApiError::ChapterInfo(asin.to_owned()))
}

/// Fetches the license-free `content_metadata` object for `asin` from the
/// `/1.0/content/{asin}/metadata` endpoint — one wire home (audit
/// 2026-07-18, D6): `request_chapters` and `download info`'s DRM probe
/// each assembled this request separately. The `drm_type` is the knob:
/// `Adrm` yields the curated chapter titles (see [`request_chapters`]),
/// a `Widevine` probe reveals a title with no aax asset. A non-success
/// status is surfaced as [`ApiError::Http`] (so a caller can tell a
/// server rejection from a transport fault); the response groups are a
/// superset both callers read from.
pub async fn request_content_metadata(
    client: &Client,
    country_code: &str,
    asin: &str,
    drm_type: &str,
    quality: &str,
    chapter_titles_type: &str,
) -> Result<serde_json::Value, ApiError> {
    let payload: serde_json::Value = client
        .request(Method::GET, format!("/1.0/content/{asin}/metadata"))
        .country_code(country_code)
        .auth(AuthMode::Auto)
        .query(
            "response_groups",
            "last_position_heard,content_reference,chapter_info",
        )
        .query("quality", quality)
        .query("drm_type", drm_type)
        .query("chapter_titles_type", chapter_titles_type)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    payload
        .get("content_metadata")
        .cloned()
        .ok_or_else(|| ApiError::ChapterInfo(asin.to_owned()))
}

/// Endpoint serving a title's annotations (bookmarks, notes, last
/// position) — the "sidecar". A single global host across marketplaces.
const ANNOTATION_BASE: &str = "https://cde-ta-g7g.amazon.com/FionaCDEServiceEngine/sidecar";

/// Builds the annotation sidecar URL. `guid` (`acr:version`) and `format`
/// are added when known (from a license); the endpoint also answers with
/// just `key`/`type`.
fn annotation_url(
    asin: &str,
    acr: Option<&str>,
    version: Option<&str>,
    content_format: Option<&str>,
) -> String {
    let mut url = format!("{ANNOTATION_BASE}?type=AUDI&key={asin}");
    if let (Some(acr), Some(version)) = (acr, version) {
        url.push_str(&format!("&guid={acr}:{version}"));
    }
    if let Some(content_format) = content_format {
        url.push_str(&format!("&format={content_format}"));
    }
    url
}

/// Fetches a title's annotations (the sidecar) as JSON. The CDE-Sidecar
/// host rejects the access token, so `Auto` signs this request like the app
/// does (AUD-195); it needs no license, though `acr`/`version` refine the
/// query when available.
///
/// Returns `None` when the title simply has no annotations: the endpoint
/// answers `404` (often with an empty/non-JSON body) in that case, which is
/// not an error. Other non-2xx statuses are surfaced.
pub async fn request_annotations(
    client: &Client,
    asin: &str,
    acr: Option<&str>,
    version: Option<&str>,
    content_format: Option<&str>,
) -> Result<Option<serde_json::Value>, ApiError> {
    let url = annotation_url(asin, acr, version, content_format);
    let response = client.authed_get(&url).await?.send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response.error_for_status()?;
    let status = response.status();
    let bytes = response.bytes().await?;
    match decode_annotations(status, &bytes) {
        AnnotationBody::Payload(doc) => Ok(Some(doc)),
        AnnotationBody::None => Ok(None),
        AnnotationBody::Unparseable => Err(ApiError::AnnotationResponse(asin.to_owned())),
    }
}

/// What an annotation response body means (free of IO for unit tests).
#[derive(Debug)]
enum AnnotationBody {
    /// `404` or an empty/whitespace body: genuinely no annotations.
    None,
    /// Valid JSON: the annotation payload.
    Payload(serde_json::Value),
    /// 2xx with a non-empty body that is not JSON (a proxy/HTML error
    /// page, a transient fault). A **failure**, never "no annotations" —
    /// recording it as `none` made `annotations sync --missing` skip the
    /// item's real bookmarks forever (audit 2026-07-17, A14).
    Unparseable,
}

fn decode_annotations(status: reqwest::StatusCode, body: &[u8]) -> AnnotationBody {
    if status == reqwest::StatusCode::NOT_FOUND {
        return AnnotationBody::None;
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return AnnotationBody::None;
    }
    match serde_json::from_slice(body) {
        Ok(doc) => AnnotationBody::Payload(doc),
        Err(_) => AnnotationBody::Unparseable,
    }
}

/// Fetches `asin`'s cover source images from the catalog at the given
/// `image_sizes`. Used when there is no stored library document to derive from:
/// child episodes, and asins outside the library.
///
/// The caller asks for the sizes it can derive from, not the size it wants, so
/// one request answers every size (AUD-209) — which sizes those are is the
/// caller's rule to know. Returns `None` when the catalog has no images at all.
pub async fn request_cover_images(
    client: &Client,
    country_code: &str,
    asin: &str,
    image_sizes: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, ApiError> {
    let response = client
        .request(Method::GET, format!("/1.0/catalog/products/{asin}"))
        .country_code(country_code)
        .auth(AuthMode::Auto)
        .query("response_groups", "media")
        .query("image_sizes", image_sizes)
        .send()
        .await?;
    let payload: serde_json::Value = response.json().await?;
    Ok(payload
        .get("product")
        .and_then(|product| product.get("product_images"))
        .and_then(serde_json::Value::as_object)
        .cloned())
}
