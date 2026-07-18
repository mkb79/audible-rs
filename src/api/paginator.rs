//! Pagination of API list endpoints as async streams.
//!
//! `/1.0/library` paginates via headers: the `Continuation-Token`
//! response header feeds the `continuation_token` query parameter of
//! the next request until the header is absent. The `State-Token`
//! response header carries the delta-sync token (a literal `"0"` means
//! none). Mirrors `iter_library_pages` from the Python reference
//! branch `feature/db-library`.
//!
//! Transient failures (429, 5xx, network errors) are retried with
//! exponential backoff, honoring `Retry-After`.

use std::time::Duration;

use futures::Stream;

use super::client::{ApiError, RequestBuilder};

/// Response header announcing the next page.
pub const HEADER_CONTINUATION_TOKEN: &str = "Continuation-Token";
/// Response header carrying the delta-sync state token.
pub const HEADER_STATE_TOKEN: &str = "State-Token";
/// Query parameter taking the continuation token.
pub const PARAM_CONTINUATION_TOKEN: &str = "continuation_token";

const RETRY_STATUS: [u16; 5] = [429, 500, 502, 503, 504];
const MAX_RETRIES: u32 = 6;
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// One page of a paginated endpoint.
#[derive(Debug)]
pub struct Page {
    /// The JSON response body.
    pub body: serde_json::Value,
    /// Delta-sync token from the `State-Token` header, if any.
    pub state_token: Option<String>,
}

/// Streams all pages of a continuation-token paginated endpoint.
///
/// `make_request` builds the request for one page; the paginator passes
/// the continuation token of the previous page (`None` for the first)
/// and the callback must NOT add [`PARAM_CONTINUATION_TOKEN`] itself.
pub fn pages<'c, F>(make_request: F) -> impl Stream<Item = Result<Page, ApiError>> + 'c
where
    F: FnMut(Option<&str>) -> RequestBuilder<'c> + 'c,
{
    // State: the callback plus the pending continuation —
    // Some(None) = first page, Some(Some(token)) = next page, None = done.
    futures::stream::try_unfold(
        (make_request, Some(None::<String>)),
        |(mut make_request, pending)| async move {
            let Some(continuation) = pending else {
                return Ok(None);
            };

            let response = fetch_with_retry(&mut make_request, continuation.as_deref()).await?;
            let response = response.error_for_status()?;

            let state_token = header(&response, HEADER_STATE_TOKEN).filter(|token| token != "0");
            let next = header(&response, HEADER_CONTINUATION_TOKEN);
            let body = response.json().await?;

            Ok(Some((
                Page { body, state_token },
                (make_request, next.map(Some)),
            )))
        },
    )
}

async fn fetch_with_retry<'c, F>(
    make_request: &mut F,
    continuation: Option<&str>,
) -> Result<reqwest::Response, ApiError>
where
    F: FnMut(Option<&str>) -> RequestBuilder<'c>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut request = make_request(continuation);
        if let Some(token) = continuation {
            request = request.query(PARAM_CONTINUATION_TOKEN, token);
        }

        let backoff = BASE_BACKOFF * 2u32.pow(attempt.saturating_sub(1).min(8));
        match request.send().await {
            Ok(response)
                if RETRY_STATUS.contains(&response.status().as_u16()) && attempt <= MAX_RETRIES =>
            {
                let delay = retry_after(&response).unwrap_or(backoff);
                tracing::debug!(
                    status = %response.status(),
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying page request"
                );
                tokio::time::sleep(delay).await;
            }
            Ok(response) => return Ok(response),
            Err(ApiError::Http(error)) if attempt <= MAX_RETRIES && is_transient(&error) => {
                tracing::debug!(attempt, "retrying page request after network error");
                tokio::time::sleep(backoff).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_transient(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Upper bound on a server-suggested `Retry-After` delay. Also bounds the
/// `from_secs_f64` conversion, which panics on huge values.
const MAX_RETRY_AFTER_SECS: f64 = 60.0;

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    parse_retry_after(&header(response, "Retry-After")?)
}

/// Parses a numeric `Retry-After` value. The header is server/proxy input:
/// negative, NaN, infinite or absurdly large values must fall back to the
/// computed backoff (`None`) or be capped — never reach the panicking
/// `Duration::from_secs_f64` unchecked. (HTTP-date values fail the parse
/// and use the backoff, as before.)
fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER_SECS)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every value a proxy/CDN can send must yield a bounded delay or
    /// `None` (→ computed backoff) — `"-1"`, `"NaN"`, `"inf"` and
    /// `"1e300"` all panicked in `Duration::from_secs_f64` before.
    #[test]
    fn retry_after_survives_adversarial_values() {
        for hostile in ["-1", "NaN", "inf", "-inf", "", "soon", "Fri, 31 Dec"] {
            assert_eq!(parse_retry_after(hostile), None, "{hostile:?}");
        }
        // Finite but absurd: capped, never slept (and never a panic).
        assert_eq!(parse_retry_after("1e300"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_after_honors_and_caps_valid_values() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(
            parse_retry_after(" 2.5 "),
            Some(Duration::from_secs_f64(2.5))
        );
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        // A well-formed but absurd delay (10 days) is capped, not slept.
        assert_eq!(parse_retry_after("864000"), Some(Duration::from_secs(60)));
    }
}
