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
///
/// Falls back to the `episodes` table (AUD-206): a child episode has no `items`
/// row, so an items-only lookup returned `None` = "unknown" and the caller then
/// probed the companion-file URL for **every** episode. Episode docs do carry
/// the flag — verified across a real library, all of them report `false` — so
/// the probe is now skipped.
async fn library_pdf_flag(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<bool> {
    let value = stored_doc(ctx, marketplace, asin).await?;
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

/// Resolves a cover URL for `asin` at `size`: derived from the stored
/// `product_images` (no API call), else a catalog lookup.
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

/// Sizes at or below this resolve to a different, smaller source image than
/// larger ones do; a URL for such a size cannot serve a bigger one — it would
/// silently stay small (AUD-208).
const SMALL_MASTER_MAX: u32 = 500;

/// The cover URL for `size`, derived from the stored `product_images` without
/// any request: the size is part of the URL, so restating it is enough
/// (AUD-208). `None` falls through to the catalog.
async fn cover_url_from_library(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    size: &str,
) -> Option<String> {
    let doc = stored_doc(ctx, marketplace, asin).await?;
    let images = doc.get("product_images")?.as_object()?;

    // An exact hit is the API's own URL for that size — take it as-is.
    if let Some(url) = images.get(size).and_then(serde_json::Value::as_str) {
        return Some(url.to_owned());
    }

    rewrite_size_marker(anchor_url(images, wants_large_master(size)?)?, size)
}

/// Whether a size needs the larger source image — `native` and anything above
/// [`SMALL_MASTER_MAX`] do. `None` for a value that is neither a number nor
/// `native` (rejected long before this by `schema::validate_cover_size`).
fn wants_large_master(size: &str) -> Option<bool> {
    if size.eq_ignore_ascii_case(crate::config::schema::COVER_SIZE_NATIVE) {
        return Some(true);
    }
    Some(size.parse::<u32>().ok()? > SMALL_MASTER_MAX)
}

/// The stored URL to derive a size from — the one **the API itself would use**,
/// so the result is its answer rather than ours. (The small and large sources
/// are separate images; whether they hold identical artwork is unverified, and
/// following the same rule makes that question moot.)
///
/// A large size needs a large source: with only small ones stored, `None`
/// (→ catalog) beats silently handing back a small image.
fn anchor_url(
    images: &serde_json::Map<String, serde_json::Value>,
    wants_large: bool,
) -> Option<&str> {
    let sizes = || {
        images
            .iter()
            .filter_map(|(key, value)| Some((key.parse::<u32>().ok()?, value.as_str()?)))
    };
    let picked = if wants_large {
        sizes()
            .filter(|(px, _)| *px > SMALL_MASTER_MAX)
            .max_by_key(|(px, _)| *px)?
    } else {
        // Largest small-source URL; failing that, any stored one still renders
        // the size correctly.
        sizes()
            .filter(|(px, _)| *px <= SMALL_MASTER_MAX)
            .max_by_key(|(px, _)| *px)
            .or_else(|| sizes().max_by_key(|(px, _)| *px))?
    };
    Some(picked.1)
}

/// Restates an image URL at `size`, or at the largest available for
/// [`COVER_SIZE_NATIVE`].
///
/// The URLs are `{image_id}[.{transform}].{ext}`; the id is opaque and always
/// comes from the API, never derived. Only the one transform shape we actually
/// see is understood — anything else returns `None`, so the caller falls back
/// rather than rewriting a URL whose other parts it would drop. A URL without a
/// transform is already the largest, and a valid base to render from (AUD-208).
fn rewrite_size_marker(url: &str, size: &str) -> Option<String> {
    let (stem, extension) = url.rsplit_once('.')?;
    // Split off a transform block if there is one; `base` is the image id
    // (with its path), which is all we rebuild from.
    let base = match stem.rsplit_once('.') {
        Some((base, block)) if block.starts_with('_') && block.ends_with('_') => {
            let inner = &block[1..block.len() - 1];
            let digits = inner.strip_prefix("SL")?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None; // a block we do not understand
            }
            base
        }
        // No block, or a stem whose last dot is not a transform: render from it
        // as-is.
        _ => stem,
    };
    Some(
        if size.eq_ignore_ascii_case(crate::config::schema::COVER_SIZE_NATIVE) {
            format!("{base}.{extension}")
        } else {
            format!("{base}._SL{size}_.{extension}")
        },
    )
}

/// The stored library document for an asin: an `items` row, else the `episodes`
/// row. A child episode has no `items` row, so an items-only lookup would treat
/// every episode as unknown (AUD-206).
async fn stored_doc(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<serde_json::Value> {
    let db = ctx.open_library_db().await.ok()?;
    let doc = match db.item_doc(asin.to_owned(), marketplace.to_owned()).await {
        Ok(Some(doc)) => doc,
        Ok(None) => {
            db.episode_doc(asin.to_owned(), marketplace.to_owned())
                .await
                .ok()??
                .0
        }
        Err(_) => return None,
    };
    serde_json::from_str(&doc).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://m.media-amazon.com/images/I/EXAMPLEID01._SL1215_.jpg";

    /// The URL carries the size, so restating it is how any size is obtained
    /// without a request; dropping it yields the largest available (AUD-208).
    #[test]
    fn rewrites_or_strips_the_size_marker() {
        assert_eq!(
            rewrite_size_marker(URL, "700").unwrap(),
            "https://m.media-amazon.com/images/I/EXAMPLEID01._SL700_.jpg"
        );
        // Beyond the size it was fetched as: a request above what exists
        // simply yields the largest available.
        assert_eq!(
            rewrite_size_marker(URL, "2000").unwrap(),
            "https://m.media-amazon.com/images/I/EXAMPLEID01._SL2000_.jpg"
        );
        // `native` = the largest available, whatever that turns out to be.
        assert_eq!(
            rewrite_size_marker(URL, crate::config::schema::COVER_SIZE_NATIVE).unwrap(),
            "https://m.media-amazon.com/images/I/EXAMPLEID01.jpg"
        );
        assert_eq!(
            rewrite_size_marker(URL, "NATIVE").as_deref(),
            Some("https://m.media-amazon.com/images/I/EXAMPLEID01.jpg")
        );

        // A URL that already is the largest renders any size just as well.
        assert_eq!(
            rewrite_size_marker("https://x/images/I/EXAMPLEID01.jpg", "500").unwrap(),
            "https://x/images/I/EXAMPLEID01._SL500_.jpg"
        );

        // Only the shape we actually see is understood — any other falls
        // through to the catalog rather than being rewritten with its other
        // parts silently dropped.
        assert!(rewrite_size_marker("https://x/I/abc._AC_SL1500_.jpg", "500").is_none());
        assert!(rewrite_size_marker("https://x/I/abc._SL1215_QL85_.jpg", "500").is_none());
        assert!(rewrite_size_marker("https://x/I/abc._CR0,0,500,500_.jpg", "500").is_none());
        assert!(rewrite_size_marker("https://x/I/abc._SL_.jpg", "500").is_none());
        assert!(rewrite_size_marker("https://x/I/abc._SLxx_.jpg", "500").is_none());
    }

    fn images(entries: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        entries
            .iter()
            .map(|(size, id)| {
                (
                    (*size).to_owned(),
                    serde_json::Value::String(format!(
                        "https://m.media-amazon.com/images/I/{id}._SL{size}_.jpg"
                    )),
                )
            })
            .collect()
    }

    /// Small and large sizes resolve to different source images, and the API
    /// picks by the requested size. The anchor must follow the *same* rule, so a
    /// derived URL is the API's answer and not our own guess (AUD-208).
    #[test]
    fn anchors_on_the_master_audible_would_use() {
        // What a sync stores: three small-source sizes, three large-source ones.
        let synced = images(&[
            ("252", "SMALLSRC01"),
            ("408", "SMALLSRC01"),
            ("500", "SMALLSRC01"),
            ("558", "LARGESRC01"),
            ("900", "LARGESRC01"),
            ("1215", "LARGESRC01"),
        ]);

        // A large size anchors on the biggest large-source URL.
        assert!(anchor_url(&synced, true).unwrap().contains("LARGESRC01"));
        assert!(anchor_url(&synced, true).unwrap().contains("_SL1215_"));
        // A small size anchors on the small source — as the API does — not on
        // the large one just because it is bigger.
        assert!(anchor_url(&synced, false).unwrap().contains("SMALLSRC01"));
        assert!(anchor_url(&synced, false).unwrap().contains("_SL500_"));

        // Only small ones stored: a large size must NOT be faked from them
        // (it would silently stay small) — fall through to the catalog instead.
        let small_only = images(&[("252", "SMALLSRC01"), ("500", "SMALLSRC01")]);
        assert!(anchor_url(&small_only, true).is_none());
        assert!(anchor_url(&small_only, false).unwrap().contains("_SL500_"));

        // Only large ones stored: a small size still renders correctly off it.
        let large_only = images(&[("1215", "LARGESRC01")]);
        assert!(
            anchor_url(&large_only, false)
                .unwrap()
                .contains("LARGESRC01")
        );

        assert!(anchor_url(&images(&[]), true).is_none());
        assert!(anchor_url(&images(&[]), false).is_none());
    }

    /// `native` needs the large source; the boundary itself is inclusive.
    #[test]
    fn size_picks_the_right_master() {
        assert_eq!(wants_large_master("native"), Some(true));
        assert_eq!(wants_large_master("NATIVE"), Some(true));
        assert_eq!(wants_large_master("2400"), Some(true));
        assert_eq!(wants_large_master("558"), Some(true));
        assert_eq!(wants_large_master("501"), Some(true));
        assert_eq!(wants_large_master("500"), Some(false));
        assert_eq!(wants_large_master("252"), Some(false));
        assert_eq!(wants_large_master("nonsense"), None);
    }
}
