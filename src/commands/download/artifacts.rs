//! The individual artifact downloads (audio, chapters, PDF, covers)
//! and their sidecar/record handling.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::config::ctx::Ctx;
use crate::models::content::{DownloadLicense, Voucher};
use crate::naming::join_relative;

use super::item::{record_download, variant_recorded};
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn download_audio(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    license: &DownloadLicense,
    dir: &Path,
    base: &str,
    force: bool,
    no_db_write: bool,
    request_kind: &str,
    mp: Option<&MultiProgress>,
) -> Result<String> {
    let url = license
        .offline_url
        .as_deref()
        .context("license has no download URL")?;

    // The content format is part of the file name so different qualities
    // of the same title do not overwrite each other on disk.
    let format = license.content_format.as_deref().filter(|f| !f.is_empty());
    let planned = match format {
        Some(format) => join_relative(dir, &format!("{base}.{format}.aaxc")),
        None => join_relative(dir, &format!("{base}.aaxc")),
    };

    // The on-disk extension follows the response Content-Type: the plain,
    // DRM-free variants (`audio/mpeg` → .mp3, AAC-in-MP4 → .m4a) get their
    // real extension; every aax variant stays .aaxc. Some podcast episodes
    // are served as `audio/mp4` (AUD-159). The actual path is returned (the
    // `.part` is renamed to it).
    let (outcome, dest) = download_to_file(
        client,
        url,
        &planned,
        license.content_size,
        force,
        mp,
        &[
            "audio/aax",
            "audio/vnd.audible.aax",
            "audio/mpeg",
            "audio/mp3",
            "audio/mp4",
            "audio/x-m4a",
            "audio/audible",
        ],
        &[
            ("audio/mpeg", "mp3"),
            ("audio/mp3", "mp3"),
            ("audio/mp4", "m4a"),
            ("audio/x-m4a", "m4a"),
        ],
    )
    .await?;
    match outcome {
        DownloadOutcome::AlreadyComplete => eprintln!("{} already complete", dest.display()),
        DownloadOutcome::Downloaded => {}
    }

    // Key/iv sidecar next to the file (`<name>.voucher`), so the same quality
    // keeps its own key. Regenerable from the stored license. Only the
    // encrypted `.aaxc` downloads need one — plain media (mp3/m4a podcast
    // episodes) is DRM-free and its license carries only an empty voucher, so
    // write the sidecar only when the file actually stayed encrypted.
    if dest.extension().and_then(|ext| ext.to_str()) == Some("aaxc") {
        write_keyfile(client, license, &dest.with_extension("voucher"));
    }

    let size = std::fs::metadata(&dest).ok().map(|m| m.len());
    record_download(
        ctx,
        marketplace,
        &license.asin,
        Some(license),
        "audio",
        format.unwrap_or_default(),
        "original",
        request_kind,
        &dest,
        size,
        no_db_write,
    )
    .await;

    Ok(dest.display().to_string())
}

/// Decrypts the voucher and writes `{key, iv}` to `dest`. Skips (with a
/// warning) when the auth file lacks the device/customer data needed to
/// derive the key — the encrypted voucher stays recoverable from the
/// stored license. The key/iv are sensitive and never logged.
fn write_keyfile(client: &crate::api::client::Client, license: &DownloadLicense, dest: &Path) {
    let (Some(dt), Some(ds), Some(cid)) = (
        client.device_type(),
        client.device_serial(),
        client.customer_id(),
    ) else {
        eprintln!("warning: auth file lacks device/customer data; key/iv file not written");
        return;
    };
    let voucher: Voucher = match license.decrypt_voucher(dt, ds, cid) {
        Ok(voucher) => voucher,
        Err(error) => {
            eprintln!("warning: could not decrypt the aaxc voucher: {error}");
            return;
        }
    };
    let body = serde_json::json!({ "key": voucher.key, "iv": voucher.iv });
    if let Err(error) = std::fs::write(
        dest,
        serde_json::to_vec_pretty(&body).expect("strings serialize"),
    ) {
        eprintln!("warning: could not write {}: {error}", dest.display());
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn write_chapters(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    chapter_type: crate::config::schema::ChapterType,
    quality: Quality,
    dir: &Path,
    base: &str,
    force: bool,
    no_db_write: bool,
) -> Result<Option<String>> {
    // The layout is part of the name and the DB key, so flat and tree
    // chapter files of the same title stay distinct.
    let token = chapter_type.as_str();

    if !force && !no_db_write && variant_recorded(ctx, marketplace, asin, "chapter", token).await {
        eprintln!("skipping chapter ({token}) — already recorded (use --force)");
        return Ok(None);
    }

    // No license metadata: audible-cli's chapter request carries only
    // quality + drm_type + chapter_titles_type (see request_chapters).
    let chapters = request_chapters(
        client,
        marketplace,
        asin,
        quality.api_value(),
        chapter_type.api_value(),
    )
    .await
    .context("could not fetch chapter metadata")?;
    let dest = join_relative(dir, &format!("{base}.chapters_{token}.json"));
    std::fs::write(&dest, serde_json::to_vec_pretty(&chapters)?)
        .with_context(|| format!("could not write {}", dest.display()))?;

    let size = std::fs::metadata(&dest).ok().map(|m| m.len());
    record_download(
        ctx,
        marketplace,
        asin,
        None,
        "chapter",
        token,
        "original",
        "",
        &dest,
        size,
        no_db_write,
    )
    .await;

    Ok(Some(dest.display().to_string()))
}

/// Downloads a title's companion PDF from the web companion-file endpoint
/// (`https://www.audible.<domain>/companion-file/<asin>`), which 302-redirects
/// to the signed CDN PDF. This needs auth but **no licenserequest**, and the
/// bytes are identical to the license `pdf_url` across the library (verified,
/// AUD-103) — so every PDF is reachable this way regardless of the title's DRM.
///
/// `license` is only used to backfill the record's acr/version/sku when this run
/// happened to acquire one for the audio download; PDFs otherwise need no license.
/// Returns `None` when the title has no companion PDF.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_pdf(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    license: Option<&DownloadLicense>,
    dir: &Path,
    base: &str,
    force: bool,
    no_db_write: bool,
) -> Result<Option<String>> {
    // The library doc's `is_pdf_url_available` flag is a reliable pre-check
    // (100% consistent with an actual PDF across the library, AUD-103): skip the
    // request for titles that definitely have none. `None` = unknown (no synced
    // doc / `--no-db-write`), so we attempt the fetch and let the response decide.
    let has_pdf = library_pdf_flag(ctx, marketplace, asin).await;
    if has_pdf == Some(false) {
        return Ok(None);
    }

    let Some(locale) = crate::api::locale::find(marketplace) else {
        bail!("unknown marketplace {marketplace:?}");
    };
    let url = format!(
        "https://www.audible.{}/companion-file/{asin}",
        locale.domain
    );
    let dest = join_relative(dir, &format!("{base}.pdf"));

    // PDFs are small — no byte bar, just counted in the summary.
    let (_, dest) = match download_to_file(
        client,
        &url,
        &dest,
        None,
        force,
        None,
        &["application/octet-stream", "application/pdf"],
        &[],
    )
    .await
    {
        Ok(ok) => ok,
        // A title without a companion PDF answers the companion-file URL with an
        // HTML page (200 text/html), not the PDF. When the library flag is
        // unknown, treat that as "no PDF" instead of surfacing a content-type error.
        Err(DownloadError::ContentType { .. }) if has_pdf.is_none() => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let size = std::fs::metadata(&dest).ok().map(|m| m.len());
    record_download(
        ctx,
        marketplace,
        asin,
        license,
        "pdf",
        "",
        "original",
        "",
        &dest,
        size,
        no_db_write,
    )
    .await;

    Ok(Some(dest.display().to_string()))
}

/// Whether the synced library doc advertises a companion PDF for the title
/// (`is_pdf_url_available`, with a `pdf_url`-presence fallback). `None` when the
/// doc is unavailable (DB closed, item not synced, `--no-db-write`).
async fn library_pdf_flag(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<bool> {
    let db = ctx.open_library_db().await.ok()?;
    let doc = db
        .item_doc(asin.to_owned(), marketplace.to_owned())
        .await
        .ok()??;
    let value: serde_json::Value = serde_json::from_str(&doc).ok()?;
    let flag = value
        .get("is_pdf_url_available")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || value.get("pdf_url").is_some_and(|v| !v.is_null());
    Some(flag)
}

/// Downloads each requested cover size to `<base>.cover_<size>.jpg`. The
/// URL comes from the stored library item's `product_images` when the
/// size was synced, else from a catalog lookup. Per-size skip honours the
/// overwrite policy.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_covers(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    sizes: &[String],
    dir: &Path,
    base: &str,
    force: bool,
    no_db_write: bool,
) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for size in sizes {
        if !force && !no_db_write && variant_recorded(ctx, marketplace, asin, "cover", size).await {
            eprintln!("skipping cover {size} — already recorded (use --force)");
            continue;
        }

        let Some(url) = cover_url(ctx, client, marketplace, asin, size).await? else {
            eprintln!("no cover at size {size} for this title");
            continue;
        };
        let dest = join_relative(dir, &format!("{base}.cover_{size}.jpg"));
        // Covers are small — no byte bar, just counted in the summary.
        download_to_file(client, &url, &dest, None, force, None, &["image/jpeg"], &[]).await?;

        let file_size = std::fs::metadata(&dest).ok().map(|m| m.len());
        record_download(
            ctx,
            marketplace,
            asin,
            None,
            "cover",
            size,
            "original",
            "",
            &dest,
            file_size,
            no_db_write,
        )
        .await;
        written.push(dest.display().to_string());
    }
    Ok(written)
}

/// Resolves a cover URL for `asin` at `size`: the stored library item's
/// `product_images` first (no API call), then a catalog lookup.
async fn cover_url(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    size: &str,
) -> Result<Option<String>> {
    if let Some(url) = cover_url_from_library(ctx, marketplace, asin, size).await {
        return Ok(Some(url));
    }
    Ok(request_cover_url(client, marketplace, asin, size).await?)
}

/// Reads the cover URL for `size` from the stored item's `product_images`.
async fn cover_url_from_library(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    size: &str,
) -> Option<String> {
    let db = ctx.open_library_db().await.ok()?;
    let doc = db
        .item_doc(asin.to_owned(), marketplace.to_owned())
        .await
        .ok()??;
    let value: serde_json::Value = serde_json::from_str(&doc).ok()?;
    value
        .get("product_images")?
        .get(size)?
        .as_str()
        .map(str::to_owned)
}
