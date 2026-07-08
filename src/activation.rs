//! Activation bytes for **legacy `.aax`** files. These are an account-global
//! key (the same for all of the account's audiobooks), unlike aaxc, which uses
//! a per-title key/iv from the content license (voucher). audible-rs does not
//! download aax; this is a standalone convenience for users who still have old
//! `.aax` files on disk.
//!
//! Two server flows, both yielding the same bytes (a device property):
//! - **signing**: one signed `GET https://www.audible.com/license/token`.
//! - **cookies**: fetch a player token via the marketplace's
//!   `player-auth-token` (website cookies), then exchange it at
//!   `licenseForCustomerToken`, bracketed by `de-register` to avoid leaving a
//!   phantom player registration.
//!
//! Mirrors `audible.activation_bytes` (Python reference).

use reqwest::header::{COOKIE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::{Client, Url};

use crate::auth::Authenticator;
use crate::auth::signing::{HEADER_ADP_ALG, HEADER_ADP_SIGNATURE, HEADER_ADP_TOKEN};

/// App user-agent for the player-token and signing requests.
const APP_USER_AGENT: &str = "Audible/671 CFNetwork/1240.0.4 Darwin/20.6.0";
/// User-agent the reference uses for the customer-token blob endpoint.
const DOWNLOAD_MANAGER_USER_AGENT: &str = "Audible Download Manager";
/// Software player id: `base64(sha1(b""))` — a fixed value (the reference
/// hashes an empty input, so it never varies).
const PLAYER_ID: &str = "2jmj7l5rSw0yVb/vlWAYkK/YBwk=";

/// Which server flow to use. `Auto` prefers signing (no cookies, no
/// register/deregister), falling back to cookies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivationMethod {
    /// Signing when available, else cookies.
    #[default]
    Auto,
    /// Signed request (needs adp_token + device key).
    Signing,
    /// Player-token exchange (needs valid website cookies).
    Cookies,
}

impl std::str::FromStr for ActivationMethod {
    type Err = String;

    /// Parses an explicit method; `auto` is implicit (the flag is omitted)
    /// and deliberately not accepted here.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "signing" => Ok(Self::Signing),
            "cookies" => Ok(Self::Cookies),
            other => Err(format!(
                "unknown method {other:?} (expected signing or cookies)"
            )),
        }
    }
}

/// Errors raised while fetching activation bytes. No token, cookie or blob
/// value is ever included (only the kind of failure).
#[derive(Debug, thiserror::Error)]
pub enum ActivationError {
    /// Neither signing material nor website cookies are available (auto).
    #[error(
        "no auth material to fetch activation bytes: this account has no signing \
         material and no website cookies (run `audible account cookies refresh`)"
    )]
    NoAuthMethod,
    /// `--method signing` but the account has no signing material.
    #[error("signing is not available for this account (no adp_token / device key)")]
    SigningUnavailable,
    /// `--method cookies` but no website cookies are stored.
    #[error("no website cookies stored (run `audible account cookies refresh`)")]
    CookiesUnavailable,
    /// The player-token response carried no `playerToken`.
    #[error(
        "no player token in the response (website cookies may be expired — run \
         `audible account cookies refresh`)"
    )]
    NoPlayerToken,
    /// The server did not return a valid activation blob.
    #[error(
        "the server did not return a valid activation blob (the account's auth may be invalid)"
    )]
    BadBlob,
    /// The HTTP layer failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// The hosts the fetch talks to. Production uses Audible's real hosts; tests
/// point them at a mock server.
struct Endpoints {
    /// `https://www.audible.com` — signing (`/license/token`) and the
    /// customer-token blob (`/license/licenseForCustomerToken`).
    com: String,
    /// `https://www.audible.<domain>` — the player-auth-token redirect.
    marketplace: String,
}

impl Endpoints {
    fn production(domain: &str) -> Self {
        Self {
            com: "https://www.audible.com".to_owned(),
            marketplace: format!("https://www.audible.{domain}"),
        }
    }
}

/// Fetches and extracts the account's activation bytes. `method` selects the
/// flow (`Auto` resolves to signing, else cookies).
pub async fn fetch(
    auth: &Authenticator,
    method: ActivationMethod,
) -> Result<String, ActivationError> {
    let endpoints = Endpoints::production(auth.locale().domain);
    fetch_with_endpoints(auth, method, &endpoints).await
}

async fn fetch_with_endpoints(
    auth: &Authenticator,
    method: ActivationMethod,
    endpoints: &Endpoints,
) -> Result<String, ActivationError> {
    let blob = match resolve(auth, method)? {
        ActivationMethod::Signing => fetch_signing(auth, endpoints).await?,
        ActivationMethod::Cookies => fetch_cookies(auth, endpoints).await?,
        ActivationMethod::Auto => unreachable!("resolve() never returns Auto"),
    };
    extract_activation_bytes(&blob)
}

/// Resolves `Auto` and validates that the requested method is usable.
fn resolve(
    auth: &Authenticator,
    method: ActivationMethod,
) -> Result<ActivationMethod, ActivationError> {
    let has_signing = auth.signer().is_some();
    let has_cookies = !auth.website_cookies().is_empty();
    match method {
        ActivationMethod::Signing if !has_signing => Err(ActivationError::SigningUnavailable),
        ActivationMethod::Signing => Ok(ActivationMethod::Signing),
        ActivationMethod::Cookies if !has_cookies => Err(ActivationError::CookiesUnavailable),
        ActivationMethod::Cookies => Ok(ActivationMethod::Cookies),
        ActivationMethod::Auto if has_signing => Ok(ActivationMethod::Signing),
        ActivationMethod::Auto if has_cookies => Ok(ActivationMethod::Cookies),
        ActivationMethod::Auto => Err(ActivationError::NoAuthMethod),
    }
}

async fn fetch_signing(
    auth: &Authenticator,
    endpoints: &Endpoints,
) -> Result<Vec<u8>, ActivationError> {
    let signer = auth
        .signer()
        .cloned()
        .ok_or(ActivationError::SigningUnavailable)?;

    // Build the URL, then sign exactly the path+query that is sent (a literal
    // comma in `player_manuf`, matching the reference; the signature must
    // cover the bytes on the wire).
    let mut url = Url::parse(&format!("{}/license/token", endpoints.com))
        .map_err(|_| ActivationError::BadBlob)?;
    url.set_query(Some(
        "player_manuf=Audible,iPhone&action=register&player_model=iPhone",
    ));
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };

    // RSA signing is CPU-bound; keep it off the executor.
    let signed =
        tokio::task::spawn_blocking(move || signer.sign_request("GET", &path_and_query, b""))
            .await
            .expect("signing task must not panic");

    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
    let mut adp_token =
        HeaderValue::from_str(&signed.adp_token).map_err(|_| ActivationError::BadBlob)?;
    adp_token.set_sensitive(true);
    headers.insert(HeaderName::from_static(HEADER_ADP_TOKEN), adp_token);
    headers.insert(
        HeaderName::from_static(HEADER_ADP_ALG),
        HeaderValue::from_static(signed.alg),
    );
    let mut signature =
        HeaderValue::from_str(&signed.signature).map_err(|_| ActivationError::BadBlob)?;
    signature.set_sensitive(true);
    headers.insert(HeaderName::from_static(HEADER_ADP_SIGNATURE), signature);

    let client = Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .read_timeout(crate::api::client::READ_TIMEOUT)
        .build()?;
    let response = client.get(url).headers(headers).send().await?;
    Ok(response.bytes().await?.to_vec())
}

async fn fetch_cookies(
    auth: &Authenticator,
    endpoints: &Endpoints,
) -> Result<Vec<u8>, ActivationError> {
    // Follows redirects by default; timeouts match the API client (AUD-98).
    let client = Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .read_timeout(crate::api::client::READ_TIMEOUT)
        .build()?;

    // 1) Player token: GET player-auth-token with the marketplace's cookies and
    //    read `playerToken` from the final (redirected) URL.
    let mut url = Url::parse(&format!("{}/player-auth-token", endpoints.marketplace))
        .map_err(|_| ActivationError::BadBlob)?;
    url.query_pairs_mut()
        .append_pair("ipRedirectOverride", "true")
        .append_pair("playerType", "software")
        .append_pair("bp_ua", "y")
        .append_pair("playerModel", "Desktop")
        .append_pair("playerId", PLAYER_ID)
        .append_pair("playerManufacturer", "Audible")
        .append_pair("serial", "");

    let host = url.host_str().unwrap_or_default().to_owned();
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(APP_USER_AGENT));
    if let Some(cookie) = auth.cookie_header(&host) {
        let mut value =
            HeaderValue::from_str(&cookie).map_err(|_| ActivationError::CookiesUnavailable)?;
        value.set_sensitive(true);
        headers.insert(COOKIE, value);
    }

    let response = client.get(url).headers(headers).send().await?;
    let final_url = response.url().clone();
    let player_token = final_url
        .query_pairs()
        .find(|(key, _)| key == "playerToken")
        .map(|(_, value)| value.into_owned())
        .ok_or(ActivationError::NoPlayerToken)?;

    // 2) Exchange the player token for the activation blob, bracketed by
    //    `de-register` (before and after) so no phantom player registration is
    //    left behind. Deregister is best-effort; only the register matters.
    let endpoint = format!("{}/license/licenseForCustomerToken", endpoints.com);
    deregister(&client, &endpoint, &player_token).await;
    let mut register_url = Url::parse(&endpoint).map_err(|_| ActivationError::BadBlob)?;
    register_url
        .query_pairs_mut()
        .append_pair("customer_token", &player_token);
    let register = client
        .get(register_url)
        .header(USER_AGENT, DOWNLOAD_MANAGER_USER_AGENT)
        .send()
        .await;
    deregister(&client, &endpoint, &player_token).await;

    Ok(register?.bytes().await?.to_vec())
}

/// Best-effort `de-register` call (errors are ignored — it is cleanup).
async fn deregister(client: &Client, endpoint: &str, player_token: &str) {
    let Ok(mut url) = Url::parse(endpoint) else {
        return;
    };
    url.query_pairs_mut()
        .append_pair("customer_token", player_token)
        .append_pair("action", "de-register");
    let _ = client
        .get(url)
        .header(USER_AGENT, DOWNLOAD_MANAGER_USER_AGENT)
        .send()
        .await;
}

/// Extracts the 8-hex-digit activation bytes from an activation blob. Errors
/// without echoing the blob (it carries account material).
fn extract_activation_bytes(data: &[u8]) -> Result<String, ActivationError> {
    /// Bytes of metadata at the end that hold the activation body.
    const META_LEN: usize = 0x238; // 568 = 8 * (70 + 1)
    /// Body bytes per line, plus one trailing separator byte that is dropped.
    const LINE: usize = 71;
    const KEEP: usize = 70;
    const LINES: usize = 8;

    if contains(data, b"BAD_LOGIN") || contains(data, b"Whoops") || !contains(data, b"group_id") {
        return Err(ActivationError::BadBlob);
    }
    if data.len() < META_LEN {
        return Err(ActivationError::BadBlob);
    }

    // Take the trailing metadata and strip the per-line separator bytes.
    let tail = &data[data.len() - META_LEN..];
    let mut body = Vec::with_capacity(KEEP * LINES);
    for line in 0..LINES {
        let start = line * LINE;
        body.extend_from_slice(&tail[start..start + KEEP]);
    }

    // The activation bytes are the first 4 body bytes as a little-endian u32,
    // rendered as 8 hex digits (zero-padded).
    let value = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    Ok(format!("{value:08x}"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Builds a 568-byte-tail activation blob whose first body bytes are
    /// `first4`, with a `group_id` marker so the validity check passes.
    fn synthetic_blob(first4: [u8; 4]) -> Vec<u8> {
        let mut tail = vec![0u8; 0x238];
        tail[0..4].copy_from_slice(&first4);
        let mut blob = b"prefix group_id metadata ".to_vec();
        blob.extend_from_slice(&tail);
        blob
    }

    #[test]
    fn extract_reads_first_le_u32_as_hex() {
        let blob = synthetic_blob([0x78, 0x56, 0x34, 0x12]);
        assert_eq!(extract_activation_bytes(&blob).unwrap(), "12345678");
    }

    #[test]
    fn extract_zero_pads_to_eight_digits() {
        let blob = synthetic_blob([0x01, 0x00, 0x00, 0x00]);
        assert_eq!(extract_activation_bytes(&blob).unwrap(), "00000001");
    }

    #[test]
    fn extract_rejects_bad_blobs() {
        // BAD_LOGIN marker.
        let mut bad = b"BAD_LOGIN group_id".to_vec();
        bad.extend_from_slice(&vec![0u8; 0x238]);
        assert!(matches!(
            extract_activation_bytes(&bad),
            Err(ActivationError::BadBlob)
        ));
        // No group_id marker.
        assert!(extract_activation_bytes(&vec![0u8; 0x238]).is_err());
        // Has the marker but is too short.
        assert!(extract_activation_bytes(b"group_id").is_err());
    }

    fn signing_auth() -> Authenticator {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/signing.json"
        ))
        .expect("missing signing fixture — run scripts/gen_fixtures.py once");
        let fixture: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let data = serde_json::json!({
            "country_code": "us",
            "signing": {
                "adp_token": fixture["adp_token"],
                "device_private_key": fixture["device_private_key"],
            },
        });
        Authenticator::from_value(data).unwrap()
    }

    fn cookies_auth() -> Authenticator {
        let data = serde_json::json!({
            "country_code": "us",
            "website_cookies": {
                "127.0.0.1": [{"name": "session-id", "value": "000-0000000-0000000"}]
            },
        });
        Authenticator::from_value(data).unwrap()
    }

    #[tokio::test]
    async fn signing_fetch_signs_and_extracts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/license/token"))
            .and(header_exists("x-adp-signature"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(synthetic_blob([0x78, 0x56, 0x34, 0x12])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let endpoints = Endpoints {
            com: server.uri(),
            marketplace: server.uri(),
        };
        let ab = fetch_with_endpoints(&signing_auth(), ActivationMethod::Signing, &endpoints)
            .await
            .unwrap();
        assert_eq!(ab, "12345678");
    }

    #[tokio::test]
    async fn cookies_fetch_follows_redirect_and_brackets_deregister() {
        let server = MockServer::start().await;
        // The player-auth-token redirects to a URL carrying the player token.
        Mock::given(method("GET"))
            .and(path("/player-auth-token"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "/redirected?playerToken=TESTTOKEN"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // One handler for all customer-token calls; the order is asserted below.
        Mock::given(method("GET"))
            .and(path("/license/licenseForCustomerToken"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(synthetic_blob([0x21, 0x43, 0x65, 0x87])),
            )
            .mount(&server)
            .await;

        let endpoints = Endpoints {
            com: server.uri(),
            marketplace: server.uri(),
        };
        let ab = fetch_with_endpoints(&cookies_auth(), ActivationMethod::Cookies, &endpoints)
            .await
            .unwrap();
        assert_eq!(ab, "87654321");

        // de-register, register, de-register — all with the player token.
        let calls: Vec<Option<String>> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/license/licenseForCustomerToken")
            .map(|r| {
                assert!(
                    r.url
                        .query_pairs()
                        .any(|(k, v)| k == "customer_token" && v == "TESTTOKEN")
                );
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "action")
                    .map(|(_, v)| v.into_owned())
            })
            .collect();
        assert_eq!(
            calls,
            vec![
                Some("de-register".to_owned()),
                None,
                Some("de-register".to_owned())
            ]
        );
    }

    #[test]
    fn method_from_str_rejects_auto() {
        assert_eq!("signing".parse(), Ok(ActivationMethod::Signing));
        assert_eq!("cookies".parse(), Ok(ActivationMethod::Cookies));
        assert!("auto".parse::<ActivationMethod>().is_err());
    }
}
