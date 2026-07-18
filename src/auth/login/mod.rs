//! The `account login` OAuth/PKCE + device-registration flow (UI-free core).
//!
//! Phase 1 (external browser, AUD-58): generate the PKCE pair and `frc`,
//! build the authorize URL the user opens in their browser, take the pasted
//! redirect, and register the device into an [`Authenticator`]. Captured from
//! live iOS app traffic; secrets are never logged.
//!
//! Phase 2 (internal scripted login, AUD-59): [`metadata1`] + a form-driven
//! sign-in with challenge handlers (added in sibling submodules).

mod challenge;
mod internal;
mod metadata1;
mod page;
mod server;

pub use challenge::{ChallengePrompt, MfaDevice};
pub use internal::login_internal;
pub use server::{LoginDefaults, LoginServer};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::locale::Locale;

use super::device::Device;
use super::{AuthError, Authenticator};

/// Errors raised by the login / device-registration flow.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The HTTP layer failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A local I/O error (e.g. binding the login server's socket).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The pasted redirect URL could not be parsed.
    #[error("the pasted URL is not a valid URL")]
    InvalidRedirect,
    /// The pasted redirect URL has no `openid.oa2.authorization_code`.
    #[error("the pasted URL has no authorization code (sign-in not completed?)")]
    NoAuthorizationCode,
    /// The register endpoint answered with an error status.
    #[error("device registration failed with HTTP status {0}")]
    Register(reqwest::StatusCode),
    /// The register response did not have the expected shape.
    #[error("device registration returned an unexpected response")]
    RegisterResponse,
    /// The device private key from the registration could not be parsed.
    #[error("could not parse the device private key from the registration")]
    PrivateKey,
    /// Mapping the registration onto the auth envelope failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The scripted login could not be completed (the page's message, if any).
    #[error("login failed: {0}")]
    LoginFailed(String),
    /// Amazon served its JavaScript-based anti-automation verification page
    /// (AUD-178). The scripted login cannot pass it — there is nothing to
    /// prompt for (no code is ever sent); the user must sign in through a
    /// real browser.
    #[error(
        "Amazon served an anti-automation verification page that requires JavaScript — \
         the scripted login cannot pass it (no verification code is sent). \
         Sign in through a browser instead:\n  \
         audible account login --external   (paste the redirect URL back here)\n  \
         audible account login server       (opens a local sign-in page; headless-friendly)"
    )]
    AntiAutomation,
    /// The challenge loop did not terminate.
    #[error("login did not complete after several challenge steps")]
    TooManyChallenges,
    /// A challenge prompt was cancelled or could not be answered.
    #[error("challenge cancelled")]
    Cancelled,
}

/// A PKCE (RFC 7636, S256) verifier + challenge pair for the OAuth flow.
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Generates a fresh S256 pair: `verifier` = base64url(32 random bytes),
    /// `challenge` = base64url(SHA-256(verifier)) — both unpadded, matching the
    /// app's `openid.oa2.code_verifier` / `code_challenge`.
    pub fn generate() -> Self {
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let verifier = URL_SAFE_NO_PAD.encode(raw);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }

    /// The `code_verifier`, sent to `/auth/register` to prove the OAuth code.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The `code_challenge`, sent in the authorize URL.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// Generates the `frc` device-context cookie for the register payload.
///
/// Per the Python `audible` reference, this is simply `base64(313 random
/// bytes)` without padding — the server accepts an opaque value here, so no
/// fingerprint encryption is needed.
pub fn generate_frc() -> String {
    let mut raw = [0u8; 313];
    OsRng.fill_bytes(&mut raw);
    STANDARD.encode(raw).trim_end_matches('=').to_owned()
}

/// The IETF language tag for a marketplace (`de` → `de-DE`), used for the
/// authorize URL's `language` and the register `Accept-Language`.
fn locale_language(country_code: &str) -> &'static str {
    match country_code {
        "de" => "de-DE",
        "uk" => "en-GB",
        "fr" => "fr-FR",
        "ca" => "en-CA",
        "it" => "it-IT",
        "au" => "en-AU",
        "in" => "en-IN",
        "jp" => "ja-JP",
        "es" => "es-ES",
        "br" => "pt-BR",
        _ => "en-US",
    }
}

/// Builds the OAuth/PKCE authorize URL the user opens in their browser. On
/// success the browser lands on `…/ap/maplanding?…openid.oa2.authorization_code=…`.
pub fn authorize_url(device: &Device, pkce: &Pkce, locale: &Locale, with_username: bool) -> String {
    let cc = locale.country_code;
    let domain = locale.domain;
    // Pre-merger Audible accounts (username login) sign in on the Audible
    // domain with the `lap`/`privatepool` OAuth handles; everyone else on the
    // Amazon domain with the device's normal handles.
    let host = if with_username { "audible" } else { "amazon" };
    let assoc_handle = if with_username {
        device.username_assoc_handle(cc)
    } else {
        device.oauth_assoc_handle(cc)
    };
    let page_id = if with_username {
        device.username_page_id()
    } else {
        device.oauth_page_id(cc)
    };
    let identifier_select = "http://specs.openid.net/auth/2.0/identifier_select";
    let params: Vec<(&str, String)> = vec![
        ("openid.oa2.response_type", "code".to_owned()),
        (
            "openid.return_to",
            format!("https://www.{host}.{domain}/ap/maplanding"),
        ),
        ("openid.oa2.code_challenge_method", "S256".to_owned()),
        ("openid.assoc_handle", assoc_handle),
        ("openid.identity", identifier_select.to_owned()),
        ("pageId", page_id),
        ("accountStatusPolicy", "P1".to_owned()),
        ("openid.claimed_id", identifier_select.to_owned()),
        ("openid.mode", "checkid_setup".to_owned()),
        (
            "openid.ns.oa2",
            "http://www.amazon.com/ap/ext/oauth/2".to_owned(),
        ),
        (
            "openid.oa2.client_id",
            format!("device:{}", device.client_id()),
        ),
        ("language", locale_language(cc).replace('-', "_")),
        (
            "openid.ns.pape",
            "http://specs.openid.net/extensions/pape/1.0".to_owned(),
        ),
        ("openid.oa2.code_challenge", pkce.challenge().to_owned()),
        ("marketPlaceId", locale.market_place_id.to_owned()),
        ("forceMobileLayout", "true".to_owned()),
        ("openid.ns", "http://specs.openid.net/auth/2.0".to_owned()),
        ("openid.pape.max_auth_age", "0".to_owned()),
        ("openid.oa2.scope", "device_auth_access".to_owned()),
    ];
    let base = format!("https://www.{host}.{domain}/ap/signin");
    reqwest::Url::parse_with_params(&base, &params)
        .expect("a valid authorize URL")
        .to_string()
}

/// Whether a marketplace supports username (pre-merger Audible) login. Mirrors
/// the reference's `SUPPORTED_USERNAME_DOMAINS` (`de`, `com`, `co.uk`) → DE,
/// US and UK.
pub fn username_login_supported(locale: &Locale) -> bool {
    matches!(locale.country_code, "de" | "us" | "uk")
}

/// The OAuth authorization-code query parameter the maplanding redirect
/// carries. One home for the three login paths (audit 2026-07-17, D6):
/// the pasted-URL path here, the internal follow path, and the reverse
/// proxy — each had its own copy of the name + find/non-empty rule.
pub(crate) const AUTH_CODE_PARAM: &str = "openid.oa2.authorization_code";

/// The non-empty `AUTH_CODE_PARAM` value among decoded query pairs, if any.
/// The shared core behind all three login extractors.
pub(crate) fn auth_code_from_pairs<'a>(
    pairs: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)>,
) -> Option<String> {
    pairs
        .filter(|(key, _)| key == AUTH_CODE_PARAM)
        .map(|(_, value)| value.into_owned())
        .find(|code| !code.is_empty())
}

/// Extracts the `openid.oa2.authorization_code` from the pasted redirect URL.
pub fn extract_authorization_code(redirect: &str) -> Result<String, LoginError> {
    let url = reqwest::Url::parse(redirect.trim()).map_err(|_| LoginError::InvalidRedirect)?;
    auth_code_from_pairs(url.query_pairs()).ok_or(LoginError::NoAuthorizationCode)
}

/// Registers the device with Amazon (`POST /auth/register`) using the OAuth
/// `authorization_code`, and maps the response onto an [`Authenticator`] —
/// the same envelope `account import` produces. No secrets are logged.
///
/// `requested_token_type` deliberately omits `store_authentication_cookie`
/// (see the NOTE in `auth/mod.rs`). Stored website cookies are left empty;
/// they are lazily exchanged on first cookie use (AUD-25).
pub async fn register(
    http: &reqwest::Client,
    locale: &Locale,
    device: &Device,
    pkce: &Pkce,
    authorization_code: &str,
    with_username: bool,
) -> Result<Authenticator, LoginError> {
    let domain = locale.domain;
    // Pre-merger Audible accounts register on the Audible host; everyone else
    // on the Amazon host. (The website-cookie domain in the body stays
    // `.amazon.<tld>` either way, matching the reference.)
    let host = if with_username { "audible" } else { "amazon" };
    let url = format!("https://api.{host}.{domain}/auth/register");
    let body = serde_json::json!({
        "age_info": {},
        "auth_data": {
            "client_id": device.client_id(),
            "authorization_code": authorization_code,
            "code_verifier": pkce.verifier(),
            "code_algorithm": "SHA-256",
            "client_domain": "DeviceLegacy",
        },
        "user_context_map": { "frc": generate_frc() },
        "cookies": {
            "website_cookies": [
                { "Name": "amzn-app-id", "Value": "MAPiOSLib/6.0/ToHideRetailLink" },
                { "Name": "id_pk", "Value": "eyJuIjoiMSJ9" },
            ],
            "domain": format!(".amazon.{domain}"),
        },
        "registration_data": device.registration_data(),
        "requested_extensions": ["device_info", "customer_info"],
        "requested_token_type": ["bearer", "mac_dms", "website_cookies"],
    });

    let response = http
        .post(&url)
        .header(
            "x-amzn-identity-auth-domain",
            format!("api.{host}.{domain}"),
        )
        .header(reqwest::header::USER_AGENT, device.register_user_agent())
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::ACCEPT_LANGUAGE,
            locale_language(locale.country_code),
        )
        .header(reqwest::header::CACHE_CONTROL, "no-store")
        .header(
            reqwest::header::COOKIE,
            "amzn-app-id=MAPiOSLib/6.0/ToHideRetailLink",
        )
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(LoginError::Register(response.status()));
    }
    let payload: Value = response.json().await?;
    let success = payload
        .get("response")
        .and_then(|response| response.get("success"))
        .ok_or(LoginError::RegisterResponse)?;
    build_authenticator(locale, success).await
}

/// Maps the register `response.success` onto the grouped auth data and builds
/// an [`Authenticator`]. The device private key is normalized to PKCS#1 PEM.
async fn build_authenticator(
    locale: &Locale,
    success: &Value,
) -> Result<Authenticator, LoginError> {
    let extensions = success
        .get("extensions")
        .ok_or(LoginError::RegisterResponse)?;
    let device_info = extensions
        .get("device_info")
        .ok_or(LoginError::RegisterResponse)?;
    let customer_info = extensions
        .get("customer_info")
        .ok_or(LoginError::RegisterResponse)?;
    let tokens = success.get("tokens").ok_or(LoginError::RegisterResponse)?;
    let mac_dms = tokens.get("mac_dms").ok_or(LoginError::RegisterResponse)?;
    let bearer = tokens.get("bearer").ok_or(LoginError::RegisterResponse)?;

    let adp_token = str_field(mac_dms, "adp_token")?.to_owned();
    let raw_key = str_field(mac_dms, "device_private_key")?.to_owned();
    let pem = normalize_private_key(raw_key).await?;

    let expires_in = match bearer.get("expires_in") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
    .ok_or(LoginError::RegisterResponse)?;
    let now = crate::timefmt::now_unix();

    // identity = customer_info with user_id renamed to customer_id; unknown
    // fields (e.g. given_name) round-trip through the Identity `extra` map.
    let mut identity = customer_info
        .as_object()
        .cloned()
        .ok_or(LoginError::RegisterResponse)?;
    if let Some(user_id) = identity.remove("user_id") {
        identity.insert("customer_id".to_owned(), user_id);
    }

    let grouped = serde_json::json!({
        "country_code": locale.country_code,
        "identity": Value::Object(identity),
        "device": {
            "device_type": str_field(device_info, "device_type")?,
            "serial": str_field(device_info, "device_serial_number")?,
            "name": device_info.get("device_name"),
        },
        "signing": { "adp_token": adp_token, "device_private_key": pem },
        "bearer": {
            "access_token": str_field(bearer, "access_token")?,
            "refresh_token": str_field(bearer, "refresh_token")?,
            "expires": now + expires_in,
        },
    });
    Ok(Authenticator::from_value(grouped)?)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, LoginError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(LoginError::RegisterResponse)
}

/// Normalizes the registration device private key to PKCS#1 PEM (the format
/// the signer parses). The response may give a PKCS#1/PKCS#8 PEM, or a
/// base64-encoded DER (the Android app) — both are accepted. RSA parsing is
/// CPU-bound, so it runs off the async executor.
async fn normalize_private_key(raw: String) -> Result<String, LoginError> {
    tokio::task::spawn_blocking(move || {
        use rsa::RsaPrivateKey;
        use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey, LineEnding};
        use rsa::pkcs8::DecodePrivateKey;

        let key = if raw.contains("-----BEGIN") {
            RsaPrivateKey::from_pkcs1_pem(&raw)
                .or_else(|_| RsaPrivateKey::from_pkcs8_pem(&raw))
                .map_err(|_| LoginError::PrivateKey)?
        } else {
            let der = STANDARD
                .decode(raw.trim())
                .map_err(|_| LoginError::PrivateKey)?;
            RsaPrivateKey::from_pkcs1_der(&der)
                .or_else(|_| RsaPrivateKey::from_pkcs8_der(&der))
                .map_err(|_| LoginError::PrivateKey)?
        };
        key.to_pkcs1_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|_| LoginError::PrivateKey)
    })
    .await
    .map_err(|_| LoginError::PrivateKey)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::locale;
    use crate::auth::device::DeviceKind;

    #[test]
    fn pkce_pair_is_well_formed_s256() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier().len(), 43);
        assert_eq!(pkce.challenge().len(), 43);
        assert!(!pkce.verifier().contains('='));
        assert!(!pkce.challenge().contains('='));
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier().as_bytes()));
        assert_eq!(pkce.challenge(), expected);
    }

    #[test]
    fn two_pkce_pairs_differ() {
        assert_ne!(Pkce::generate().verifier(), Pkce::generate().verifier());
    }

    #[test]
    fn frc_is_unpadded_base64_of_313_bytes() {
        let frc = generate_frc();
        assert!(!frc.contains('='), "frc must be stripped of padding");
        assert_eq!(frc.len(), 418);
        let decoded = STANDARD.decode(format!("{frc}==")).unwrap();
        assert_eq!(decoded.len(), 313);
    }

    #[test]
    fn authorize_url_has_the_expected_params() {
        let device = Device::generate(DeviceKind::IPhone);
        let pkce = Pkce::generate();
        let de = locale::find("de").unwrap();
        let url = authorize_url(&device, &pkce, &de, false);
        assert!(url.starts_with("https://www.amazon.de/ap/signin?"));
        assert!(url.contains("openid.oa2.scope=device_auth_access"));
        assert!(url.contains("openid.assoc_handle=amzn_audible_ios_de"));
        assert!(url.contains("marketPlaceId=AN7V1F1VY261K"));
        assert!(url.contains(&format!("device%3A{}", device.client_id())));
        // The challenge is URL-encoded into the query.
        assert!(url.contains("openid.oa2.code_challenge="));
        // maplanding return_to (percent-encoded).
        assert!(url.contains("maplanding"));
    }

    #[test]
    fn authorize_url_username_uses_audible_domain_and_lap_handles() {
        let pkce = Pkce::generate();
        let de = locale::find("de").unwrap();

        let iphone = Device::generate(DeviceKind::IPhone);
        let url = authorize_url(&iphone, &pkce, &de, true);
        assert!(url.starts_with("https://www.audible.de/ap/signin?"));
        assert!(url.contains("openid.assoc_handle=amzn_audible_ios_lap_de"));
        assert!(url.contains("pageId=amzn_audible_ios_privatepool"));
        // return_to points at the Audible maplanding (percent-encoded host).
        assert!(url.contains("www.audible.de%2Fap%2Fmaplanding"));

        let android = Device::generate(DeviceKind::Android);
        let url = authorize_url(&android, &pkce, &de, true);
        assert!(url.starts_with("https://www.audible.de/ap/signin?"));
        assert!(url.contains("openid.assoc_handle=amzn_audible_android_experiment_lap_de"));
        assert!(url.contains("pageId=amzn_audible_android_privatepool"));
    }

    #[test]
    fn username_login_supported_only_de_us_uk() {
        assert!(username_login_supported(&locale::find("de").unwrap()));
        assert!(username_login_supported(&locale::find("us").unwrap()));
        assert!(username_login_supported(&locale::find("uk").unwrap()));
        assert!(!username_login_supported(&locale::find("fr").unwrap()));
        assert!(!username_login_supported(&locale::find("jp").unwrap()));
    }

    #[test]
    fn extracts_the_authorization_code() {
        let redirect = "https://www.amazon.de/ap/maplanding?openid.oa2.authorization_code=ABC123&openid.assoc_handle=amzn_audible_ios_de";
        assert_eq!(extract_authorization_code(redirect).unwrap(), "ABC123");
    }

    #[test]
    fn missing_code_is_an_error() {
        assert!(matches!(
            extract_authorization_code("https://www.amazon.de/ap/maplanding?foo=bar"),
            Err(LoginError::NoAuthorizationCode)
        ));
        assert!(matches!(
            extract_authorization_code("not a url"),
            Err(LoginError::InvalidRedirect)
        ));
    }
}
