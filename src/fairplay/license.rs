//! The FairPlay license protocol: the content license (`licenserequest` with
//! `drm_types: ["FairPlay"]`), the provider certificate, and the SPC → CKC
//! exchange (`drmlicense`).
//!
//! The request/response handling is a copy of `downloader`'s on purpose (see
//! the module docs). The shapes are the ones the spike confirmed live and the
//! iOS app sends (AUD-321).

use base64::Engine as _;
use reqwest::Method;
use serde_json::{Value, json};

use super::FairPlayError;
use crate::api::client::{AuthMode, Client, echoed_request_id};

const DRM_TYPE: &str = "FairPlay";
const TENANT: &str = "Audible";

/// Audio quality of a download license (`quality`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Highest available.
    #[default]
    High,
    /// Standard.
    Normal,
}

impl Quality {
    /// The API value (`High`/`Normal`).
    pub fn api_value(self) -> &'static str {
        match self {
            Quality::High => "High",
            Quality::Normal => "Normal",
        }
    }
}

/// What a license is for (`consumption_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Consumption {
    /// Offline playback: persistable keys and a downloaded package.
    #[default]
    Download,
    /// Online playback: a key per request, nothing stored.
    Streaming,
}

impl Consumption {
    /// The API value (`Download`/`Streaming`).
    pub fn api_value(self) -> &'static str {
        match self {
            Consumption::Download => "Download",
            Consumption::Streaming => "Streaming",
        }
    }
}

/// What a FairPlay licenserequest asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LicenseOptions {
    /// Download or streaming.
    pub consumption: Consumption,
    /// Download quality. A streaming license asks for the adaptive bitrate
    /// ladder instead, so it ignores this.
    pub quality: Quality,
    /// Also offer xHE-AAC (`mp4a.40.42`).
    pub xhe: bool,
    /// Ask for Dolby Atmos: offers E-AC-3 JOC (`ec+3`) and sets `spatial`.
    pub spatial: bool,
}

/// The codecs a request offers: AAC-LC (`mp4a.40.2`) always — universally
/// playable, and the fallback when a title has nothing better — plus xHE-AAC
/// and E-AC-3 JOC on request, the three the iOS app offers with FairPlay.
/// There is no security-level gate as on the Widevine path; whether a title's
/// Atmos rendition is granted is up to the server.
pub fn codecs(options: &LicenseOptions) -> Vec<&'static str> {
    let mut codecs = vec!["mp4a.40.2"];
    if options.xhe {
        codecs.push("mp4a.40.42");
    }
    if options.spatial {
        codecs.push("ec+3");
    }
    codecs
}

/// The licenserequest body. A download asks for a fixed `quality`; a stream
/// asks for the adaptive bitrate ladder instead, as the iOS app does. The
/// `certificate` response group returns the provider certificate inline,
/// which saves the separate [`request_certificate`] call.
pub fn license_body(options: &LicenseOptions) -> Value {
    let mut body = json!({
        "consumption_type": options.consumption.api_value(),
        "spatial": options.spatial,
        "tenant_id": TENANT,
        "response_groups": "content_reference,certificate",
        "supported_media_features": {
            "drm_types": [DRM_TYPE],
            "codecs": codecs(options),
            "previews": false,
            "catalog_samples": false,
        },
    });
    match options.consumption {
        Consumption::Download => body["quality"] = json!(options.quality.api_value()),
        Consumption::Streaming => body["use_adaptive_bit_rate"] = json!(true),
    }
    body
}

/// A granted FairPlay content license.
#[derive(Clone, PartialEq, Eq)]
pub struct FairPlayGrant {
    /// The licensed item.
    pub asin: String,
    /// Identifies this grant in the later `drmlicense` call.
    pub license_id: Option<String>,
    /// The signed HLS master playlist URL (`license_response`). A credential
    /// while it is valid: never log it (the `Debug` output leaves it out).
    pub hls_url: String,
    /// The provider certificate, when the server sent it inline.
    pub certificate: Option<Vec<u8>>,
    /// The right the grant rests on (e.g. `Purchase`, `AYCL`).
    pub granted_right: Option<String>,
    /// When access ends (`access_expiry_date`, RFC 3339); far in the future
    /// for a purchase.
    pub access_expiry: Option<String>,
    /// Audible Content Reference.
    pub acr: Option<String>,
    /// Content version.
    pub version: Option<String>,
    /// Stock-keeping unit.
    pub sku: Option<String>,
    /// Codec of the granted content (e.g. `mp4a.40.42`).
    pub codec: Option<String>,
    /// Content format (e.g. `M4A_XHE`).
    pub content_format: Option<String>,
    /// Content size in bytes, if the server reported it.
    pub content_size: Option<u64>,
}

impl std::fmt::Debug for FairPlayGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FairPlayGrant")
            .field("asin", &self.asin)
            .field("license_id", &self.license_id.as_ref().map(|_| "<present>"))
            .field("hls_url", &"<signed URL>")
            .field(
                "certificate_bytes",
                &self.certificate.as_ref().map(Vec::len),
            )
            .field("granted_right", &self.granted_right)
            .field("access_expiry", &self.access_expiry)
            .field("acr", &self.acr)
            .field("version", &self.version)
            .field("sku", &self.sku)
            .field("codec", &self.codec)
            .field("content_format", &self.content_format)
            .field("content_size", &self.content_size)
            .finish()
    }
}

/// Parses a licenserequest response into a FairPlay grant. Pure (no I/O) so
/// the shape handling stays unit-testable.
pub fn parse_grant(payload: &Value, asin: &str) -> Result<FairPlayGrant, FairPlayError> {
    let unexpected = |reason| FairPlayError::Unexpected {
        target: format!("licenserequest for {asin}"),
        reason,
    };
    let license = payload
        .get("content_license")
        .filter(|license| license.is_object())
        .ok_or_else(|| unexpected("no content_license"))?;
    // An explicit denial (e.g. a lapsed subscription right) comes back as
    // HTTP 200 with `status_code: "Denied"` and no drm_type.
    if license["status_code"].as_str() == Some("Denied") {
        return Err(FairPlayError::Denied {
            asin: asin.to_owned(),
            message: license["message"]
                .as_str()
                .unwrap_or("no reason given")
                .to_owned(),
        });
    }
    match license["drm_type"].as_str() {
        Some(DRM_TYPE) => {}
        other => {
            return Err(FairPlayError::NotFairPlay {
                asin: asin.to_owned(),
                drm_type: other.unwrap_or("none").to_owned(),
            });
        }
    }
    let hls_url = license["license_response"]
        .as_str()
        .filter(|url| url.starts_with("https://"))
        .ok_or_else(|| unexpected("no HTTPS playlist URL in license_response"))?;
    let certificate = match license.get("certificate").and_then(Value::as_str) {
        Some(text) => {
            Some(decode_base64(text).ok_or_else(|| unexpected("certificate is not Base64"))?)
        }
        None => None,
    };
    let reference = &license["content_metadata"]["content_reference"];
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(FairPlayGrant {
        asin: asin.to_owned(),
        license_id: text(license, "license_id"),
        hls_url: hls_url.to_owned(),
        certificate,
        granted_right: text(license, "granted_right"),
        access_expiry: text(license, "access_expiry_date"),
        // The server has put the acr in both places.
        acr: text(reference, "acr").or_else(|| text(license, "acr")),
        version: text(reference, "version"),
        sku: text(reference, "sku"),
        codec: text(reference, "codec"),
        content_format: text(reference, "content_format"),
        content_size: reference
            .get("content_size_in_bytes")
            .and_then(Value::as_u64),
    })
}

/// Requests a FairPlay content license for `asin`. Only FairPlay is offered,
/// so a title without a FairPlay asset fails ([`FairPlayError::NotFairPlay`]
/// or a rejection) instead of falling back to another format.
pub async fn request_license(
    client: &Client,
    country_code: &str,
    asin: &str,
    options: &LicenseOptions,
) -> Result<FairPlayGrant, FairPlayError> {
    let payload = post(
        client,
        country_code,
        asin,
        "licenserequest",
        license_body(options),
    )
    .await?;
    parse_grant(&payload, asin)
}

/// Fetches the provider certificate (`GET /1.0/content/FairPlay/certificate`)
/// — the fallback when a grant did not carry it inline. It is public and the
/// same for every customer.
pub async fn request_certificate(
    client: &Client,
    country_code: &str,
) -> Result<Vec<u8>, FairPlayError> {
    let target = "FairPlay certificate request".to_owned();
    let response = client
        .request(Method::GET, "/1.0/content/FairPlay/certificate")
        .country_code(country_code)
        .auth(AuthMode::Auto)
        .send()
        .await?;
    let payload = read_json(response, target.clone()).await?;
    payload
        .get("certificate")
        .and_then(Value::as_str)
        .and_then(decode_base64)
        .ok_or(FairPlayError::Unexpected {
            target,
            reason: "no Base64 certificate",
        })
}

/// The `drmlicense` body: the SPC as `licenseChallenge` (camelCase, unlike
/// the rest of the body) plus the grant's `license_id`.
pub fn ckc_body(
    asin: &str,
    spc: &[u8],
    license_id: Option<&str>,
    consumption: Consumption,
) -> Value {
    let mut body = json!({
        "asin": asin,
        "consumption_type": consumption.api_value(),
        "drm_type": DRM_TYPE,
        "licenseChallenge": base64::engine::general_purpose::STANDARD.encode(spc),
        "tenant_id": TENANT,
    });
    if let Some(id) = license_id {
        body["license_id"] = json!(id);
    }
    body
}

/// Exchanges an SPC (the key request AVFoundation builds) for a CKC (the
/// key response it consumes). Both are opaque DRM messages bound to this
/// device; neither carries a plaintext key, but neither is ever logged.
pub async fn request_ckc(
    client: &Client,
    country_code: &str,
    asin: &str,
    spc: &[u8],
    license_id: Option<&str>,
    consumption: Consumption,
) -> Result<Vec<u8>, FairPlayError> {
    let body = ckc_body(asin, spc, license_id, consumption);
    let payload = post(client, country_code, asin, "drmlicense", body).await?;
    payload
        .get("license")
        .and_then(Value::as_str)
        .and_then(decode_base64)
        .ok_or_else(|| FairPlayError::Unexpected {
            target: format!("drmlicense for {asin}"),
            reason: "no Base64 license",
        })
}

/// POSTs `body` to `/1.0/content/{asin}/{endpoint}`. Auth is `Auto`, which
/// the access token serves on these content endpoints.
async fn post(
    client: &Client,
    country_code: &str,
    asin: &str,
    endpoint: &str,
    body: Value,
) -> Result<Value, FairPlayError> {
    let response = client
        .request(Method::POST, format!("/1.0/content/{asin}/{endpoint}"))
        .country_code(country_code)
        .auth(AuthMode::Auto)
        .body(body)
        .send()
        .await?;
    read_json(response, format!("{endpoint} for {asin}")).await
}

/// Reads a response as JSON. The body is read as text first: a rejected
/// request answers with a small JSON error (`error_code`/`message`), passed
/// on verbatim together with the echoed request id.
async fn read_json(response: reqwest::Response, target: String) -> Result<Value, FairPlayError> {
    let status = response.status();
    let request_id = echoed_request_id(&response).unwrap_or_else(|| "-".into());
    let text = response.text().await?;
    if !status.is_success() {
        let (error_code, message) = parse_error(&text);
        return Err(FairPlayError::Rejected {
            target,
            status,
            error_code,
            message,
            request_id,
        });
    }
    serde_json::from_str(&text).map_err(|_| FairPlayError::Unexpected {
        target,
        reason: "not JSON",
    })
}

/// `error_code` and `message` of a failed request; a length-capped copy of
/// the raw body when it is not the expected JSON error shape.
fn parse_error(body: &str) -> (String, String) {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let error_code = value["error_code"].as_str().unwrap_or_default().to_owned();
        let message = value["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| truncate(body));
        return (error_code, message);
    }
    (String::new(), truncate(body))
}

/// Caps an error body (on a char boundary) so a large HTML error page cannot
/// flood the message.
fn truncate(body: &str) -> String {
    const MAX_CHARS: usize = 500;
    let trimmed = body.trim();
    match trimmed.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted() -> Value {
        json!({
            "content_license": {
                "status_code": "Granted",
                "asin": "B0EXAMPLE1",
                "drm_type": "FairPlay",
                "granted_right": "AYCL",
                "access_expiry_date": "2027-01-31T23:59:59Z",
                "license_id": "00000000-0000-4000-8000-00000000000a",
                "license_response": "https://cdn.example.invalid/token/title.master.m3u8?ss_sec=20",
                "certificate": "Y2VydA==",
                "content_metadata": {
                    "content_reference": {
                        "acr": "CR!EXAMPLE",
                        "codec": "mp4a.40.42",
                        "content_format": "M4A_XHE",
                        "content_size_in_bytes": 4096,
                        "sku": "BK_EXAMPLE_000001",
                        "version": "1"
                    }
                }
            }
        })
    }

    #[test]
    fn download_body_asks_for_a_fixed_quality() {
        let body = license_body(&LicenseOptions::default());
        assert_eq!(body["consumption_type"], "Download");
        assert_eq!(body["quality"], "High");
        assert!(body.get("use_adaptive_bit_rate").is_none());
        assert_eq!(body["spatial"], false);
        assert_eq!(
            body["supported_media_features"]["drm_types"],
            json!(["FairPlay"])
        );
        assert_eq!(
            body["supported_media_features"]["codecs"],
            json!(["mp4a.40.2"])
        );
        assert_eq!(body["response_groups"], "content_reference,certificate");
    }

    #[test]
    fn streaming_body_asks_for_the_bitrate_ladder() {
        let body = license_body(&LicenseOptions {
            consumption: Consumption::Streaming,
            ..LicenseOptions::default()
        });
        assert_eq!(body["consumption_type"], "Streaming");
        assert_eq!(body["use_adaptive_bit_rate"], true);
        assert!(body.get("quality").is_none());
    }

    #[test]
    fn xhe_and_spatial_widen_the_codecs() {
        let options = LicenseOptions {
            xhe: true,
            spatial: true,
            quality: Quality::Normal,
            ..LicenseOptions::default()
        };
        assert_eq!(codecs(&options), ["mp4a.40.2", "mp4a.40.42", "ec+3"]);
        let body = license_body(&options);
        assert_eq!(body["spatial"], true);
        assert_eq!(body["quality"], "Normal");
    }

    #[test]
    fn parses_a_granted_license() {
        let grant = parse_grant(&granted(), "B0EXAMPLE1").unwrap();
        assert_eq!(
            grant.license_id.as_deref(),
            Some("00000000-0000-4000-8000-00000000000a")
        );
        assert!(grant.hls_url.ends_with(".master.m3u8?ss_sec=20"));
        assert_eq!(grant.certificate.as_deref(), Some(&b"cert"[..]));
        assert_eq!(grant.granted_right.as_deref(), Some("AYCL"));
        assert_eq!(grant.access_expiry.as_deref(), Some("2027-01-31T23:59:59Z"));
        assert_eq!(grant.codec.as_deref(), Some("mp4a.40.42"));
        assert_eq!(grant.content_format.as_deref(), Some("M4A_XHE"));
        assert_eq!(grant.content_size, Some(4096));
        assert_eq!(grant.acr.as_deref(), Some("CR!EXAMPLE"));
    }

    #[test]
    fn debug_output_leaves_out_the_signed_url() {
        let grant = parse_grant(&granted(), "B0EXAMPLE1").unwrap();
        let debug = format!("{grant:?}");
        assert!(!debug.contains("cdn.example.invalid"), "{debug}");
        assert!(
            !debug.contains("00000000-0000-4000-8000-00000000000a"),
            "{debug}"
        );
    }

    #[test]
    fn a_missing_certificate_is_not_an_error() {
        let mut payload = granted();
        payload["content_license"]
            .as_object_mut()
            .unwrap()
            .remove("certificate");
        assert_eq!(
            parse_grant(&payload, "B0EXAMPLE1").unwrap().certificate,
            None
        );
    }

    #[test]
    fn a_denial_surfaces_the_reason() {
        let payload = json!({"content_license": {
            "status_code": "Denied",
            "message": "No rights for this title."
        }});
        match parse_grant(&payload, "B0EXAMPLE1") {
            Err(FairPlayError::Denied { message, .. }) => {
                assert_eq!(message, "No rights for this title.");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn another_drm_type_is_not_fairplay() {
        let mut payload = granted();
        payload["content_license"]["drm_type"] = json!("Adrm");
        match parse_grant(&payload, "B0EXAMPLE1") {
            Err(FairPlayError::NotFairPlay { drm_type, .. }) => assert_eq!(drm_type, "Adrm"),
            other => panic!("expected NotFairPlay, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_grants() {
        let mut plain_http = granted();
        plain_http["content_license"]["license_response"] =
            json!("http://cdn.example.invalid/x.m3u8");
        let mut bad_certificate = granted();
        bad_certificate["content_license"]["certificate"] = json!("not base64!");
        for payload in [json!({}), plain_http, bad_certificate] {
            assert!(
                matches!(
                    parse_grant(&payload, "B0EXAMPLE1"),
                    Err(FairPlayError::Unexpected { .. })
                ),
                "{payload}"
            );
        }
    }

    #[test]
    fn ckc_body_uses_the_camel_case_challenge() {
        let body = ckc_body("B0EXAMPLE1", b"spc", Some("lic-1"), Consumption::Download);
        assert_eq!(body["licenseChallenge"], "c3Bj");
        assert_eq!(body["license_id"], "lic-1");
        assert_eq!(body["drm_type"], "FairPlay");
        assert_eq!(body["consumption_type"], "Download");
        let streaming = ckc_body("B0EXAMPLE1", b"spc", None, Consumption::Streaming);
        assert!(streaming.get("license_id").is_none());
        assert_eq!(streaming["consumption_type"], "Streaming");
    }

    #[test]
    fn error_bodies_are_passed_on_or_capped() {
        assert_eq!(
            parse_error(r#"{"error_code":"000307","message":"No asset."}"#),
            ("000307".to_owned(), "No asset.".to_owned())
        );
        let (code, message) = parse_error(&"x".repeat(600));
        assert!(code.is_empty());
        assert_eq!(message.chars().count(), 501);
    }
}
