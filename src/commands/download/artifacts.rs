//! The individual artifact downloads (audio, chapters, PDF, covers)
//! and their sidecar/record handling.

use std::path::Path;

use anyhow::{Context as _, Result};

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
    let planned = join_relative(
        dir,
        &format!(
            "{base}{}",
            crate::naming::artifact_suffix("audio", format.unwrap_or(""), "aaxc")
        ),
    );

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
        // Gate a resumed partial on the license's content version (A9):
        // a corrected re-release must not be resumed over old bytes.
        license.version_tag().as_deref(),
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
    if let Some(sidecar) = crate::naming::sidecar_path(&dest) {
        write_keyfile(client, license, &sidecar);
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
    .await?;

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
    // Owner-only from the first byte: the sidecar holds the decrypted
    // content key, the same secrecy class as the auth file.
    if let Err(error) = crate::fsutil::write_private(
        dest,
        &serde_json::to_vec_pretty(&body).expect("strings serialize"),
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

    if !force && !no_db_write && variant_recorded(ctx, marketplace, asin, "chapter", token).await? {
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
    let dest = join_relative(
        dir,
        &format!(
            "{base}{}",
            crate::naming::artifact_suffix("chapter", token, "json")
        ),
    );
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
    .await?;

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

    let locale =
        crate::api::locale::require(marketplace).map_err(|error| anyhow::anyhow!(error))?;
    let url = format!(
        "https://www.audible.{}/companion-file/{asin}",
        locale.domain
    );
    let dest = join_relative(
        dir,
        &format!("{base}{}", crate::naming::artifact_suffix("pdf", "", "pdf")),
    );

    // PDFs are small — no byte bar, just counted in the summary. No
    // version tag: a companion PDF carries no license version, and its
    // unknown size already routes re-issues through the 416 guard.
    let (_, dest) = match download_to_file(
        client,
        &url,
        &dest,
        None,
        force,
        None,
        &["application/octet-stream", "application/pdf"],
        &[],
        None,
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
    .await?;

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
    crate::models::library::pdf_available(&value)
}

/// Downloads each requested cover size to `<base>.cover_<size>.jpg`. Every size
/// derives from the same source images, so those are resolved once per item
/// rather than once per size: the stored document if there is one, and the
/// catalog at most once, only for what the stored one cannot answer (AUD-209).
/// Per-size skip honours the overwrite policy.
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
    let stored = stored_images(ctx, marketplace, asin).await;
    // `None` = the catalog has not been asked; `Some(None)` = asked, no images.
    let mut fetched: Option<Option<ImageMap>> = None;

    for size in sizes {
        if !force && !no_db_write && variant_recorded(ctx, marketplace, asin, "cover", size).await?
        {
            eprintln!("skipping cover {size} — already recorded (use --force)");
            continue;
        }

        let mut url = stored
            .as_ref()
            .and_then(|images| derive_cover_url(images, size));
        if url.is_none() {
            if fetched.is_none() {
                fetched = Some(
                    request_cover_images(client, marketplace, asin, COVER_ANCHOR_SIZES).await?,
                );
            }
            url = fetched
                .as_ref()
                .and_then(Option::as_ref)
                .and_then(|images| derive_cover_url(images, size));
        }

        let Some(url) = url else {
            // Two different facts, and only the second is about the title: say
            // which one it is instead of blaming the title for both (AUD-209).
            if has_images(stored.as_ref()) || has_images(fetched.as_ref().and_then(Option::as_ref))
            {
                eprintln!("cover {size}: no URL could be derived for this title");
            } else {
                eprintln!("no cover images for this title");
            }
            continue;
        };
        let dest = join_relative(
            dir,
            &format!(
                "{base}{}",
                crate::naming::artifact_suffix("cover", size, "jpg")
            ),
        );
        // Covers are small — no byte bar, just counted in the summary.
        download_to_file(
            client,
            &url,
            &dest,
            None,
            force,
            None,
            &["image/jpeg"],
            &[],
            None,
        )
        .await?;

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
        .await?;
        written.push(dest.display().to_string());
    }
    Ok(written)
}

/// Sizes at or below this resolve to a different, smaller source image than
/// larger ones do; a URL for such a size cannot serve a bigger one — it would
/// silently stay small (AUD-208).
const SMALL_MASTER_MAX: u32 = 500;

/// What the catalog is asked for when there is no stored document to derive
/// from: one size per source image — the same two [`anchor_url`] picks out of
/// the set a sync stores, so both routes yield the same URL. Asking for these
/// instead of the size actually wanted is what lets one request answer every
/// size, including `native` — which is our word, not an API value (AUD-209).
const COVER_ANCHOR_SIZES: &str = "500,1215";

/// A `product_images` map: size key → URL.
type ImageMap = serde_json::Map<String, serde_json::Value>;

/// The stored `product_images` for an asin. `None` for anything the library has
/// no document for, and for documents carrying no images at all.
async fn stored_images(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<ImageMap> {
    let doc = stored_doc(ctx, marketplace, asin).await?;
    doc.get("product_images")?.as_object().cloned()
}

/// The cover URL for `size` out of a set of source images, without any further
/// request: the size is part of the URL, so restating it is enough (AUD-208).
fn derive_cover_url(images: &ImageMap, size: &str) -> Option<String> {
    // An exact hit is the API's own URL for that size — take it as-is.
    if let Some(url) = images.get(size).and_then(serde_json::Value::as_str) {
        return Some(url.to_owned());
    }
    rewrite_size_marker(anchor_url(images, wants_large_master(size)?)?, size)
}

/// Whether any source image is on hand — what separates "this size could not be
/// derived" from "the title has no cover at all" (AUD-209).
fn has_images(images: Option<&ImageMap>) -> bool {
    images.is_some_and(|images| !images.is_empty())
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
fn anchor_url(images: &ImageMap, wants_large: bool) -> Option<&str> {
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

    /// What the catalog returns for [`COVER_ANCHOR_SIZES`] — built from the
    /// constant itself, so narrowing it breaks these tests instead of quietly
    /// leaving a class of sizes without an anchor.
    fn anchor_images() -> ImageMap {
        let entries: Vec<(&str, &str)> = COVER_ANCHOR_SIZES
            .split(',')
            .map(|size| {
                let large = size.parse::<u32>().expect("numeric anchor") > SMALL_MASTER_MAX;
                (size, if large { "LARGESRC01" } else { "SMALLSRC01" })
            })
            .collect();
        images(&entries)
    }

    /// The fallback is only as good as its anchors: one per source image, or a
    /// whole class of sizes has none to derive from (AUD-209).
    #[test]
    fn the_anchor_set_covers_both_sources() {
        let anchors = anchor_images();
        assert!(
            anchor_url(&anchors, true).is_some(),
            "no large-source anchor"
        );
        assert!(
            anchor_url(&anchors, false).is_some(),
            "no small-source anchor"
        );
    }

    /// Asking the catalog for the anchors instead of the size wanted is what
    /// lets one request answer every size — `native` included, which the API has
    /// no word for and which therefore had no fallback at all (AUD-209).
    #[test]
    fn derives_every_size_from_the_catalog_anchors() {
        let anchors = anchor_images();

        // The case that started this: `native` for an episode, which has no
        // stored document to derive from.
        assert_eq!(
            derive_cover_url(&anchors, "native").unwrap(),
            "https://m.media-amazon.com/images/I/LARGESRC01.jpg"
        );
        // A size neither synced nor asked of the catalog.
        assert_eq!(
            derive_cover_url(&anchors, "700").unwrap(),
            "https://m.media-amazon.com/images/I/LARGESRC01._SL700_.jpg"
        );
        // A small size still comes off the small source, as the API does it.
        assert_eq!(
            derive_cover_url(&anchors, "252").unwrap(),
            "https://m.media-amazon.com/images/I/SMALLSRC01._SL252_.jpg"
        );
        // An exact hit is the API's own URL — untouched.
        assert_eq!(
            derive_cover_url(&anchors, "500").unwrap(),
            "https://m.media-amazon.com/images/I/SMALLSRC01._SL500_.jpg"
        );
    }

    /// The anchors are what the stored set resolves to anyway, so both routes
    /// answer identically: an episode gets the same URL as an item (AUD-209).
    #[test]
    fn the_catalog_and_stored_routes_agree() {
        let synced = images(&[
            ("252", "SMALLSRC01"),
            ("408", "SMALLSRC01"),
            ("500", "SMALLSRC01"),
            ("558", "LARGESRC01"),
            ("900", "LARGESRC01"),
            ("1215", "LARGESRC01"),
        ]);
        let anchors = anchor_images();

        for size in ["native", "252", "500", "700", "2000"] {
            assert_eq!(
                derive_cover_url(&synced, size),
                derive_cover_url(&anchors, size),
                "size {size} derives differently depending on the route"
            );
        }
    }

    /// "This size could not be derived" and "the title has no cover" are
    /// different facts, and only the second is about the title (AUD-209).
    #[test]
    fn having_images_is_not_the_same_as_resolving_a_size() {
        assert!(!has_images(None));
        assert!(!has_images(Some(&images(&[]))));

        // Images on hand, but this size cannot be derived from them — a
        // derivation failure, not an absent cover.
        let small_only = images(&[("252", "SMALLSRC01"), ("500", "SMALLSRC01")]);
        assert!(has_images(Some(&small_only)));
        assert!(derive_cover_url(&small_only, "native").is_none());
    }
}
