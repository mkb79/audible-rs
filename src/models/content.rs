//! Content license / download models, parsed from
//! `POST /1.0/content/<asin>/licenserequest` (the Adrm download path).
//! Only the fields we need are typed; the rest stays in the raw value.

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Outcome of a download license request.
#[derive(Debug, Clone)]
pub struct DownloadLicense {
    /// `Granted` or `Denied`.
    pub status_code: String,
    /// The licensed item.
    pub asin: String,
    /// DRM type the server granted (`Adrm` for a downloadable aaxc).
    pub drm_type: Option<String>,
    /// Direct download URL of the aaxc/aax file, if granted.
    pub offline_url: Option<String>,
    /// Content format / codec (`AAX_22_64`, `MPEG`, …).
    pub content_format: Option<String>,
    /// Audible Content Reference — changes when a corrected release is
    /// published under the same ASIN.
    pub acr: Option<String>,
    /// Content version.
    pub version: Option<String>,
    /// File revision of the content (`content_reference.file_version`),
    /// echoed back to the chapter metadata endpoint.
    pub file_version: Option<String>,
    /// SKU.
    pub sku: Option<String>,
    /// File size in bytes, if reported.
    pub content_size: Option<u64>,
    /// Companion PDF URL, if any.
    pub pdf_url: Option<String>,
    /// Whether an encrypted voucher (`license_response`) is present.
    pub has_voucher: bool,
    /// The raw encrypted voucher string (decrypted by `crypto`, M3
    /// commit 2).
    pub voucher_raw: Option<String>,
    /// Denial message, when `status_code` is `Denied`.
    pub denial_message: Option<String>,
    /// License expiry (`content_license.expiration_date`); used as the
    /// stored grant's `valid_until`. `None`/far-future = never expires.
    pub expiration_date: Option<String>,
    /// Full `content_metadata.chapter_info`, if requested.
    pub chapter_info: Option<Value>,
    /// The whole response, for fields we did not model.
    pub raw: Value,
}

impl DownloadLicense {
    /// Parses a `licenserequest` response body.
    pub fn from_response(body: Value) -> Option<Self> {
        let license = body.get("content_license")?;
        let metadata = license.get("content_metadata");
        let str_at = |value: &Value, keys: &[&str]| -> Option<String> {
            let mut cur = value;
            for key in keys {
                cur = cur.get(key)?;
            }
            cur.as_str().map(str::to_owned)
        };

        Some(Self {
            status_code: license
                .get("status_code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            asin: license
                .get("asin")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            drm_type: str_at(license, &["drm_type"]),
            offline_url: metadata.and_then(|m| str_at(m, &["content_url", "offline_url"])),
            content_format: metadata
                .and_then(|m| str_at(m, &["content_reference", "content_format"])),
            acr: str_at(license, &["acr"])
                .or_else(|| metadata.and_then(|m| str_at(m, &["content_reference", "acr"]))),
            version: metadata.and_then(|m| str_at(m, &["content_reference", "version"])),
            file_version: metadata.and_then(|m| str_at(m, &["content_reference", "file_version"])),
            sku: metadata.and_then(|m| str_at(m, &["content_reference", "sku"])),
            content_size: metadata.and_then(|m| {
                m.get("content_reference")
                    .and_then(|r| r.get("content_size_in_bytes"))
                    .and_then(Value::as_u64)
            }),
            pdf_url: pdf_url_from_license(license, metadata),
            has_voucher: license.get("license_response").is_some(),
            voucher_raw: str_at(license, &["license_response"]),
            denial_message: license
                .get("message")
                .filter(|_| license.get("status_code").and_then(Value::as_str) == Some("Denied"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            expiration_date: str_at(license, &["expiration_date"]),
            chapter_info: metadata.and_then(|m| m.get("chapter_info").cloned()),
            raw: body,
        })
    }

    /// Whether the license was granted with a usable download URL.
    pub fn is_granted(&self) -> bool {
        self.status_code == "Granted" && self.offline_url.is_some()
    }

    /// The content-version identity for resume validation (A9):
    /// `acr:version:file_version`. `None` when the license carries none of
    /// them (nothing to gate on). A corrected re-release changes acr and
    /// version, so a stale partial's marker no longer matches.
    pub fn version_tag(&self) -> Option<String> {
        if self.acr.is_none() && self.version.is_none() && self.file_version.is_none() {
            return None;
        }
        Some(format!(
            "{}:{}:{}",
            self.acr.as_deref().unwrap_or(""),
            self.version.as_deref().unwrap_or(""),
            self.file_version.as_deref().unwrap_or("")
        ))
    }

    /// Decrypts the aaxc voucher to obtain the file's `key`/`iv`.
    ///
    /// The voucher key is `sha256(device_type + device_serial +
    /// customer_id + asin)`: first 16 bytes are the AES-128 key, the
    /// next 16 the IV; the base64 voucher is AES-128-CBC decrypted
    /// without padding and the trailing NUL bytes stripped (mirrors
    /// `audible.aescipher`). The result is sensitive — it decrypts the
    /// audiobook.
    pub fn decrypt_voucher(
        &self,
        device_type: &str,
        device_serial: &str,
        customer_id: &str,
    ) -> Result<Voucher, VoucherError> {
        let encrypted = self.voucher_raw.as_deref().ok_or(VoucherError::Missing)?;

        let digest = Sha256::digest(
            format!("{device_type}{device_serial}{customer_id}{}", self.asin).as_bytes(),
        );
        let key: [u8; 16] = digest[0..16].try_into().expect("sha256 is 32 bytes");
        let iv: [u8; 16] = digest[16..32].try_into().expect("sha256 is 32 bytes");

        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(encrypted)
            .map_err(|_| VoucherError::Decrypt)?;
        let plain = crate::crypto::aes128_cbc_decrypt_nopad(&key, &iv, &ciphertext)
            .map_err(|_| VoucherError::Decrypt)?;

        let end = plain
            .iter()
            .rposition(|byte| *byte != 0)
            .map(|i| i + 1)
            .unwrap_or(0);
        let text = std::str::from_utf8(&plain[..end]).map_err(|_| VoucherError::Parse)?;
        let raw: Value = serde_json::from_str(text).map_err(|_| VoucherError::Parse)?;

        let field = |key: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(VoucherError::Parse)
        };
        Ok(Voucher {
            key: field("key")?,
            iv: field("iv")?,
            raw,
        })
    }
}

/// The companion-PDF URL of a licenserequest response (audit 2026-07-17,
/// D6): `content_metadata.pdf_url`, else the `content_license` top-level
/// `pdf_url`. One home for the aaxc [`DownloadLicense::from_response`] and
/// the Widevine grant parser, which each walked it. `license` is the
/// `content_license` object; `metadata` is its `content_metadata`.
pub fn pdf_url_from_license(license: &Value, metadata: Option<&Value>) -> Option<String> {
    metadata
        .and_then(|m| m.get("pdf_url"))
        .or_else(|| license.get("pdf_url"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A decrypted aaxc voucher: the content `key`/`iv` (hex) plus the full
/// document. Sensitive — never log the values.
#[derive(Clone)]
pub struct Voucher {
    /// AES key for the aaxc file (hex).
    pub key: String,
    /// AES IV for the aaxc file (hex).
    pub iv: String,
    /// The full decrypted voucher (rights, dates, …).
    pub raw: Value,
}

impl std::fmt::Debug for Voucher {
    // Holds content decryption keys; never print the values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Voucher").finish_non_exhaustive()
    }
}

/// Errors from [`DownloadLicense::decrypt_voucher`].
#[derive(Debug, thiserror::Error)]
pub enum VoucherError {
    /// The license response carried no voucher.
    #[error("no voucher in the license response")]
    Missing,
    /// Base64 decoding or AES decryption failed.
    #[error("voucher decryption failed")]
    Decrypt,
    /// The decrypted plaintext is not JSON with key/iv.
    #[error("voucher is not valid JSON with key/iv")]
    Parse,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_granted_adrm_license() {
        let body = json!({
            "content_license": {
                "status_code": "Granted",
                "asin": "B0D186SQWV",
                "acr": "CR!ABC",
                "drm_type": "Adrm",
                "license_response": "BASE64VOUCHER==",
                "content_metadata": {
                    "content_url": {"offline_url": "https://cds.audible.de/x.aaxc?Policy=…"},
                    "content_reference": {
                        "content_format": "AAX_44_64",
                        "content_size_in_bytes": 244048442u64,
                        "version": "63671221",
                        "file_version": "1",
                        "sku": "BK_RHDE_004960"
                    },
                    "pdf_url": "https://x/booklet.pdf",
                    "chapter_info": {"chapters": []}
                },
                "expiration_date": "3000-01-01T00:00:00Z"
            }
        });
        let lic = DownloadLicense::from_response(body).unwrap();
        assert!(lic.is_granted());
        assert_eq!(lic.drm_type.as_deref(), Some("Adrm"));
        assert_eq!(lic.expiration_date.as_deref(), Some("3000-01-01T00:00:00Z"));
        assert!(lic.offline_url.unwrap().ends_with(".aaxc?Policy=…"));
        assert_eq!(lic.content_format.as_deref(), Some("AAX_44_64"));
        assert_eq!(lic.acr.as_deref(), Some("CR!ABC"));
        assert_eq!(lic.version.as_deref(), Some("63671221"));
        assert_eq!(lic.file_version.as_deref(), Some("1"));
        assert_eq!(lic.sku.as_deref(), Some("BK_RHDE_004960"));
        assert_eq!(lic.content_size, Some(244048442));
        assert!(lic.has_voucher);
        assert_eq!(lic.voucher_raw.as_deref(), Some("BASE64VOUCHER=="));
        assert_eq!(lic.pdf_url.as_deref(), Some("https://x/booklet.pdf"));
        assert!(lic.chapter_info.is_some());
    }

    #[test]
    fn parses_a_denied_license() {
        let body = json!({
            "content_license": {
                "status_code": "Denied",
                "asin": "B000",
                "message": "Not entitled.",
                "content_metadata": {}
            }
        });
        let lic = DownloadLicense::from_response(body).unwrap();
        assert!(!lic.is_granted());
        assert_eq!(lic.denial_message.as_deref(), Some("Not entitled."));
        assert!(lic.offline_url.is_none());
    }
}
