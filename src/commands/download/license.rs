//! License lifecycle for the aaxc path: reuse of stored, still-valid
//! grants; fresh `licenserequest`s; persistence; and the `--license-only`
//! report.

use anyhow::{Result, bail};

use crate::config::ctx::Ctx;
use crate::models::content::DownloadLicense;

use super::*;

/// The CloudFront canned-policy `Expires=<epoch>` from a signed URL, if present.
fn url_expires_epoch(url: &str) -> Option<i64> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "Expires")
        .and_then(|(_, value)| value.parse::<i64>().ok())
}

/// Whether a stored license's signed `offline_url` has expired (or is within the
/// margin). The content key stays valid indefinitely, but the URL is short-lived
/// (CloudFront `Expires`, ~hours), so a reused audio download over a stale URL
/// would 403 at the CDN — re-license (fresh URL, same key) instead. `false` when
/// there is no `offline_url` or no parseable expiry (nothing to act on).
///
/// **Design rule (do not break):** this expiry gate belongs *only* to the
/// download-URL path (`reuse_license`, which feeds the audio download). Anything
/// that only needs the license's key/iv or metadata — the aaxc voucher, a future
/// standalone `decrypt`'s key regeneration, the AUD-19 backfill — must read the
/// stored license via `Db::find_valid_license` directly, **without** this check:
/// an expired *URL* must never force a fresh licenserequest when the *key* is all
/// that's wanted.
fn license_url_expired(license: &DownloadLicense) -> bool {
    let Some(expires) = license.offline_url.as_deref().and_then(url_expires_epoch) else {
        return false;
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    expires <= now + LICENSE_URL_EXPIRY_MARGIN_SECS
}

/// Loads the most recent still-valid stored license whose `request_kind`
/// matches one of `request_kinds` (format-aware, so a resume reuses the license
/// for the format being requested — not just the newest one).
pub(super) async fn reuse_license(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    request_kinds: Vec<String>,
) -> Option<DownloadLicense> {
    if let Ok(db) = ctx.open_library_db().await
        && let Ok(Some(doc)) = db
            .find_valid_license(
                asin.to_owned(),
                marketplace.to_owned(),
                request_kinds,
                crate::db::now_iso_utc(),
            )
            .await
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&doc)
        && let Some(license) = DownloadLicense::from_response(value)
    {
        // The stored license entitles indefinitely, but its signed `offline_url`
        // expires within hours; a reused audio download over an expired URL 403s
        // at the CDN. Re-license when it's expired — the content key is unchanged
        // (the license reuse only ever feeds the audio download; PDFs use the
        // companion-file URL — AUD-103).
        if license_url_expired(&license) {
            eprintln!("{asin}: stored license URL expired — re-licensing");
            return None;
        }
        return Some(license);
    }
    None
}

/// Whether a license error means there is no downloadable aaxc asset for this
/// entitlement — the aaxc licenserequest returning `error_code 000307`
/// (`acr:null`). Typically an AYCL/Plus title Audible now serves via Widevine
/// only (the migration is per-title and ongoing). The trigger to fall back to
/// the Widevine path.
pub(super) fn is_no_aaxc_asset(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<crate::api::client::ApiError>(),
        Some(crate::api::client::ApiError::LicenseRejected { error_code, .. })
            if error_code == "000307"
    )
}

/// Requests a fresh license, verifies it was granted, and stores it in
/// the `licenses` table (the encrypted voucher stays encrypted) so later
/// runs can re-use it without a new request — unless `no_db_write`
/// suppresses the persistence.
pub(super) async fn acquire_license(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    quality: Quality,
    no_db_write: bool,
) -> Result<DownloadLicense> {
    let license = request_license(client, marketplace, asin, quality).await?;
    if !license.is_granted() {
        bail!(
            "license denied: {}",
            license
                .denial_message
                .as_deref()
                .unwrap_or("no reason given")
        );
    }

    // Intent key for format-aware reuse (AUD-93): a Mpeg grant keys as `mpeg`,
    // else adrm by the requested quality.
    let grant = if license.drm_type.as_deref() == Some("Mpeg") {
        request_kind::Grant::Mpeg
    } else {
        request_kind::Grant::Adrm
    };
    let kind = request_kind::resolved(grant, false, quality);
    persist_license(ctx, marketplace, &license, &kind, no_db_write).await;

    Ok(license)
}

/// Stores a granted license in the database for later re-use. A no-op
/// under `--no-db-write`.
pub(super) async fn persist_license(
    ctx: &Ctx,
    marketplace: &str,
    license: &DownloadLicense,
    request_kind: &str,
    no_db_write: bool,
) {
    if no_db_write {
        return;
    }
    let Ok(db) = ctx.open_library_db().await else {
        return;
    };
    let grant = crate::db::LicenseGrant {
        asin: license.asin.clone(),
        content_format: license.content_format.clone().unwrap_or_default(),
        request_kind: request_kind.to_owned(),
        valid_until: license.expiration_date.clone(),
        doc: license.raw.to_string(),
    };
    if let Err(error) = db.upsert_license(marketplace.to_owned(), grant).await {
        tracing::warn!(%error, "could not store the license in the database");
    }
}

/// `--license-only`: print the grant without downloading; also probes
/// the voucher decrypt without ever printing the content key.
pub(super) fn print_license(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    license: &DownloadLicense,
) -> Result<()> {
    if !license.is_granted() {
        eprintln!(
            "license denied: {}",
            license
                .denial_message
                .as_deref()
                .unwrap_or("no reason given")
        );
    }
    let voucher_state = if !license.has_voucher {
        "-".to_owned()
    } else {
        match (
            client.device_type(),
            client.device_serial(),
            client.customer_id(),
        ) {
            (Some(dt), Some(ds), Some(cid)) => match license.decrypt_voucher(dt, ds, cid) {
                Ok(_voucher) => "decrypted".to_owned(),
                Err(error) => format!("failed ({error})"),
            },
            _ => "missing device/customer data in auth file".to_owned(),
        }
    };
    ctx.print(&Output::KeyValue(vec![
        ("asin".into(), license.asin.clone()),
        ("status".into(), license.status_code.clone()),
        (
            "drm_type".into(),
            license.drm_type.clone().unwrap_or_else(|| "-".into()),
        ),
        (
            "content_format".into(),
            license.content_format.clone().unwrap_or_else(|| "-".into()),
        ),
        (
            "size_bytes".into(),
            license
                .content_size
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        ("has_voucher".into(), license.has_voucher.to_string()),
        ("voucher".into(), voucher_state),
        (
            "pdf_url".into(),
            if license.pdf_url.is_some() {
                "yes"
            } else {
                "-"
            }
            .into(),
        ),
        (
            "download_url".into(),
            if license.offline_url.is_some() {
                "yes"
            } else {
                "-"
            }
            .into(),
        ),
    ]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cloudfront_expires() {
        // The aaxc offline_url is a CloudFront canned-policy URL: Expires=<epoch>.
        assert_eq!(
            url_expires_epoch(
                "https://cf.net/x.aax?id=1&Expires=1783270514&Signature=s&Key-Pair-Id=k"
            ),
            Some(1783270514)
        );
        // No Expires param, and a non-URL: no expiry to act on.
        assert_eq!(url_expires_epoch("https://cf.net/x.aax?Signature=s"), None);
        assert_eq!(url_expires_epoch("not a url"), None);
    }

    #[test]
    fn license_expiry_uses_offline_url() {
        use crate::models::content::DownloadLicense;
        let with_url = |url: Option<&str>| DownloadLicense {
            status_code: "Granted".into(),
            asin: "B0".into(),
            drm_type: Some("Adrm".into()),
            offline_url: url.map(str::to_owned),
            content_format: None,
            acr: None,
            version: None,
            file_version: None,
            sku: None,
            content_size: None,
            pdf_url: None,
            has_voucher: false,
            voucher_raw: None,
            denial_message: None,
            expiration_date: None,
            chapter_info: None,
            raw: serde_json::Value::Null,
        };
        // A far-future Expires is not expired; a past one is; no URL / no Expires
        // is treated as "nothing to act on" (not expired).
        assert!(!license_url_expired(&with_url(Some(
            "https://cf.net/x?Expires=4102444800" // year 2100
        ))));
        assert!(license_url_expired(&with_url(Some(
            "https://cf.net/x?Expires=1000000000" // year 2001
        ))));
        assert!(!license_url_expired(&with_url(Some(
            "https://cf.net/x?Signature=s"
        ))));
        assert!(!license_url_expired(&with_url(None)));
    }
}
