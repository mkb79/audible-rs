//! `download info` — read-only companion to the download action
//! (AUD-108, moved under `download` in AUD-113): what is fetchable for an
//! item (audio formats/qualities, cover sizes, chapters, pdf) and what is
//! already on disk. DB-first: the pinned library response groups already
//! carry `available_codecs`, `customer_rights`, `is_pdf_url_available`
//! and the cover sizes, so owned items need no API call for the static
//! facts. The catalog is the fallback for ASINs outside the library
//! (e.g. wishlist titles), and one licence-free
//! `/1.0/content/{asin}/metadata` request per item adds the dynamic bits
//! (chapter count, exact audio size). The same probe detects
//! widevine-only titles (AUD-113): `available_codecs` lists aax entries
//! even when no aax asset exists — the Adrm metadata request then fails
//! (`000307`/`acr:null`) and a Widevine retry succeeds. Never requests a
//! license and never writes.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::Value;

use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::db::DownloadEntry;
use crate::output::Output;

/// `download info` — clap.
pub(super) fn info_command() -> clap::Command {
    crate::commands::items::item_source_args(
        clap::Command::new("info")
            .about("Show what download could fetch for an item, and what is already on disk"),
    )
    .group(
        clap::ArgGroup::new("source")
            .args(["asin", "title"])
            .multiple(true)
            .required(true),
    )
}

/// `download info` — one table row per artifact kind and audio format.
pub(super) async fn info(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    use crate::commands::strings;
    show(ctx, strings(matches, "asin"), strings(matches, "title")).await
}

async fn show(ctx: &Ctx, asins: Vec<String>, titles: Vec<String>) -> Result<()> {
    let marketplace = ctx.marketplace_single()?;
    let db = ctx.open_library_db().await?;
    crate::library_sync::maybe_auto_sync(ctx, &db).await?;
    let asins = crate::commands::items::resolve_asins(
        &db,
        &marketplace,
        asins,
        titles,
        crate::commands::items::PodcastMode::Episodes,
    )
    .await?;
    if asins.is_empty() {
        bail!("nothing selected");
    }
    let client = ctx.client().await?;

    // Local download state, grouped per ASIN.
    let mut downloads: BTreeMap<String, Vec<DownloadEntry>> = BTreeMap::new();
    for entry in db.download_entries().await? {
        if entry.marketplace == marketplace {
            downloads.entry(entry.asin.clone()).or_default().push(entry);
        }
    }

    // Static facts: library doc first, catalog fallback for the rest.
    let mut facts: BTreeMap<String, AssetFacts> = BTreeMap::new();
    let mut not_owned: Vec<String> = Vec::new();
    for asin in &asins {
        let doc = match db.item_doc(asin.clone(), marketplace.clone()).await? {
            Some(doc) => Some(doc),
            None => db
                .episode_doc(asin.clone(), marketplace.clone())
                .await?
                .map(|(doc, _parent)| doc),
        };
        match doc.as_deref().and_then(parse_doc) {
            Some(parsed) => {
                facts.insert(asin.clone(), parsed);
            }
            None => not_owned.push(asin.clone()),
        }
    }
    if !not_owned.is_empty() {
        for asin in &not_owned {
            eprintln!("{asin} is not in the library — showing catalog data (no ownership info)");
        }
        catalog_facts(client, &marketplace, &not_owned, &mut facts).await?;
    }

    // Dynamic facts (chapter count, exact audio size, widevine-only
    // detection) — licence-free, one request per item (two for
    // widevine-only titles); a failure degrades the affected cells to `?`.
    let mut metadata: BTreeMap<String, (Value, MetadataDrm)> = BTreeMap::new();
    for asin in &asins {
        let wants_audio = facts
            .get(asin)
            .is_some_and(|f| f.delivery_type.as_deref() != Some("Periodical"));
        if !wants_audio {
            continue;
        }
        match content_metadata(client, &marketplace, asin).await {
            Ok(pair) => {
                metadata.insert(asin.clone(), pair);
            }
            Err(error) => {
                tracing::debug!(asin, %error, "content metadata unavailable");
            }
        }
    }

    let mut rows = Vec::new();
    for asin in &asins {
        let Some(fact) = facts.get(asin) else {
            eprintln!("{asin}: no data (not in the library or the catalog); skipping");
            continue;
        };
        let empty = Vec::new();
        let entries = downloads.get(asin).unwrap_or(&empty);
        for row in item_rows(fact, metadata.get(asin), entries) {
            let mut full = vec![marketplace.clone(), asin.clone()];
            full.extend(row);
            rows.push(full);
        }
    }
    if rows.is_empty() {
        eprintln!("no assets to show");
        return Ok(());
    }
    ctx.print(&Output::table(
        vec!["mp", "asin", "kind", "detail", "available", "downloaded"],
        rows,
    ));
    Ok(())
}

/// Static asset facts of one item, extracted from a library doc or a
/// catalog product.
struct AssetFacts {
    /// `available_codecs[].name` (e.g. `aax_44_128`), original order.
    codecs: Vec<String>,
    /// `product_images` sizes, ascending.
    image_sizes: Vec<u32>,
    /// [`crate::models::library::pdf_available`] (flag with `pdf_url`
    /// fallback) — `None` when the source cannot know (catalog products
    /// carry no ownership data).
    pdf_available: Option<bool>,
    /// `customer_rights.is_consumable_offline` (library docs only).
    offline_right: Option<bool>,
    /// `content_delivery_type` (`SinglePartBook`, `PodcastEpisode`,
    /// `Periodical` = podcast parent, …).
    delivery_type: Option<String>,
}

/// Parses the static facts from a JSON document string.
fn parse_doc(doc: &str) -> Option<AssetFacts> {
    serde_json::from_str::<Value>(doc).ok().map(|v| facts(&v))
}

/// Extracts [`AssetFacts`] from a parsed library doc / catalog product.
fn facts(value: &Value) -> AssetFacts {
    let codecs = value
        .get("available_codecs")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|codec| codec.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut image_sizes: Vec<u32> = value
        .get("product_images")
        .and_then(Value::as_object)
        .map(|images| images.keys().filter_map(|key| key.parse().ok()).collect())
        .unwrap_or_default();
    image_sizes.sort_unstable();
    AssetFacts {
        codecs,
        image_sizes,
        pdf_available: crate::models::library::pdf_available(value),
        offline_right: value
            .get("customer_rights")
            .and_then(|rights| rights.get("is_consumable_offline"))
            .and_then(Value::as_bool),
        delivery_type: value
            .get("content_delivery_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Fetches catalog facts for ASINs outside the library (batched; catalog
/// works unauthenticated, but the shared client signs anyway).
async fn catalog_facts(
    client: &Client,
    marketplace: &str,
    asins: &[String],
    into: &mut BTreeMap<String, AssetFacts>,
) -> Result<()> {
    let products = crate::catalog::products_batched(
        client,
        marketplace,
        asins,
        "product_attrs,media",
        Some("252,408,500,558,900,1215"),
        1,
    )
    .await?;
    for product in &products {
        if let Some(asin) = product.get("asin").and_then(Value::as_str) {
            into.insert(asin.to_owned(), facts(product));
        }
    }
    Ok(())
}

/// Which DRM flavor the licence-free metadata probe succeeded with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MetadataDrm {
    /// Adrm answered — the aax/aaxc asset exists.
    Adrm,
    /// Adrm failed (`000307`/`acr:null` — no aax asset) but Widevine
    /// answered: the title is streaming-DRM only (Plus/AYCL, AUD-113).
    Widevine,
}

/// One licence-free content-metadata request (chapter count + exact audio
/// size). Mirrors the download path's chapter request (Adrm + quality
/// only — see `downloader::request_chapters` for why nothing else). When
/// Adrm fails, one Widevine retry decides whether the title is
/// widevine-only or metadata is simply unavailable.
async fn content_metadata(
    client: &Client,
    marketplace: &str,
    asin: &str,
) -> Result<(Value, MetadataDrm)> {
    match content_metadata_drm(client, marketplace, asin, "Adrm").await {
        Ok(value) => Ok((value, MetadataDrm::Adrm)),
        // Only a definitive server answer may flip the verdict to
        // Widevine: a transient fault (connect, timeout, transfer) with a
        // succeeding Widevine retry used to mislabel a normal aax title
        // as "widevine only" (A7). The download path gates its fallback
        // on the specific 000307 rejection; the metadata request cannot
        // see the body's error code, so the gate here is "the server
        // rejected Adrm" vs "the network hiccuped".
        Err(adrm_error) if is_transient_fault(&adrm_error) => Err(adrm_error),
        Err(adrm_error) => {
            tracing::debug!(asin, %adrm_error, "Adrm metadata rejected; probing Widevine");
            let value = content_metadata_drm(client, marketplace, asin, "Widevine").await?;
            Ok((value, MetadataDrm::Widevine))
        }
    }
}

/// Whether the error is a transport-level fault (connect, timeout, body
/// transfer/decode) rather than a definitive HTTP-status answer.
fn is_transient_fault(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|error| !error.is_status())
}

async fn content_metadata_drm(
    client: &Client,
    marketplace: &str,
    asin: &str,
    drm_type: &str,
) -> Result<Value> {
    let body: Value = client
        .request(Method::GET, format!("/1.0/content/{asin}/metadata"))
        .country_code(marketplace)
        .query("response_groups", "content_reference,chapter_info")
        .query("quality", "High")
        .query("drm_type", drm_type)
        .query("chapter_titles_type", "Flat")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    body.get("content_metadata")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no content_metadata in the response"))
}

/// Builds the artifact rows (kind, detail, available, downloaded) for one
/// item from its static facts, the optional content metadata (plus which
/// DRM answered it) and local download records.
fn item_rows(
    fact: &AssetFacts,
    metadata: Option<&(Value, MetadataDrm)>,
    downloads: &[DownloadEntry],
) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let downloaded = |kind: &str, format: Option<&str>| -> String {
        let cells: Vec<String> = downloads
            .iter()
            .filter(|entry| {
                entry.kind == kind
                    && format.is_none_or(|format| entry.content_format.eq_ignore_ascii_case(format))
            })
            .map(|entry| {
                let size = entry
                    .file_size
                    .map(|n| format!(" ({})", indicatif::BinaryBytes(n)))
                    .unwrap_or_default();
                if entry.kind == "audio" {
                    format!("{}{size}", entry.variant)
                } else {
                    format!("{}{size}", show_or_dash(&entry.content_format))
                }
            })
            .collect();
        if cells.is_empty() {
            "-".to_owned()
        } else {
            cells.join("; ")
        }
    };

    // Audio: one row per announced codec; the exact size lands on the row
    // whose format the metadata's content_reference names. When only the
    // Widevine probe answered, the announced aax codecs have no asset
    // behind them (AUD-113) — mark them and show the real representation.
    let (reference, drm) = match metadata {
        Some((value, drm)) => (
            value
                .get("content_reference")
                .cloned()
                .unwrap_or(Value::Null),
            Some(*drm),
        ),
        None => (Value::Null, None),
    };
    let reference_format = reference
        .get("content_format")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reference_size = reference
        .get("content_size_in_bytes")
        .and_then(Value::as_u64);
    let widevine_only = drm == Some(MetadataDrm::Widevine);
    let is_parent = fact.delivery_type.as_deref() == Some("Periodical");
    let is_episode = fact.delivery_type.as_deref() == Some("PodcastEpisode");
    let has_aax = fact.codecs.iter().any(|codec| codec.starts_with("aax"));
    // The exact representation the server answered with, sized.
    let reference_row = || {
        let detail = if reference_format.is_empty() {
            "widevine (aac|xhe)".to_owned()
        } else {
            format!("widevine ({})", reference_format.to_lowercase())
        };
        let available = reference_size
            .map(|n| indicatif::BinaryBytes(n).to_string())
            .unwrap_or_else(|| "yes".to_owned());
        (detail, available)
    };
    if is_parent {
        rows.push(vec![
            "audio".into(),
            "podcast parent".into(),
            "episodes only".into(),
            "-".into(),
        ]);
    } else if has_aax && !widevine_only {
        for codec in &fact.codecs {
            let detail = match codec.as_str() {
                "aax_44_128" => format!("{codec} (-q high)"),
                "aax_44_64" => format!("{codec} (-q normal)"),
                other => other.to_owned(),
            };
            let available = if codec.eq_ignore_ascii_case(reference_format) {
                reference_size
                    .map(|n| indicatif::BinaryBytes(n).to_string())
                    .unwrap_or_else(|| "yes".to_owned())
            } else {
                "yes".to_owned()
            };
            rows.push(vec![
                "audio".into(),
                detail,
                available,
                downloaded("audio", Some(codec)),
            ]);
        }
    } else if has_aax && widevine_only {
        // The catalog announces aax codecs, but no aax asset exists —
        // verified by the failed Adrm probe.
        for codec in fact.codecs.iter().filter(|codec| codec.starts_with("aax")) {
            rows.push(vec![
                "audio".into(),
                codec.clone(),
                "no (widevine only)".into(),
                downloaded("audio", Some(codec)),
            ]);
        }
        let (detail, available) = reference_row();
        rows.push(vec![
            "audio".into(),
            detail,
            available,
            downloaded("audio", None),
        ]);
    } else if is_episode {
        rows.push(vec![
            "audio".into(),
            "mpeg (podcast episode)".into(),
            "yes".into(),
            downloaded("audio", None),
        ]);
    } else {
        // No aax codecs announced at all: streaming-only title. The
        // Widevine probe names representation and size; without it the
        // offline right decides how confident the label is.
        let (detail, available) = if widevine_only {
            reference_row()
        } else {
            (
                "widevine (aac|xhe)".to_owned(),
                match fact.offline_right {
                    Some(false) => "streaming only".to_owned(),
                    _ => "streaming only?".to_owned(),
                },
            )
        };
        rows.push(vec![
            "audio".into(),
            detail,
            available,
            downloaded("audio", None),
        ]);
    }
    // Audio formats on disk that no row above claimed (reencodes,
    // Widevine fetches of an aax title, …) still deserve a row.
    let claimed: Vec<&str> = fact.codecs.iter().map(String::as_str).collect();
    let mut extra: Vec<&DownloadEntry> = downloads
        .iter()
        .filter(|entry| {
            entry.kind == "audio"
                && !claimed
                    .iter()
                    .any(|codec| entry.content_format.eq_ignore_ascii_case(codec))
        })
        .collect();
    if is_episode || is_parent || widevine_only || !has_aax {
        // Those branches already show unmatched audio via `downloaded(None)`.
        extra.clear();
    }
    for entry in extra {
        let size = entry
            .file_size
            .map(|n| format!(" ({})", indicatif::BinaryBytes(n)))
            .unwrap_or_default();
        rows.push(vec![
            "audio".into(),
            show_or_dash(&entry.content_format),
            "-".into(),
            format!("{}{size}", entry.variant),
        ]);
    }

    // Cover: one compact row, sizes as CSV.
    if !fact.image_sizes.is_empty() {
        let sizes: Vec<String> = fact
            .image_sizes
            .iter()
            .map(|size| size.to_string())
            .collect();
        rows.push(vec![
            "cover".into(),
            sizes.join(","),
            "yes".into(),
            downloaded("cover", None),
        ]);
    }

    // Chapters: count from the metadata; `?` when the request failed.
    if !is_parent {
        let chapters = metadata
            .and_then(|(value, _)| value.get("chapter_info"))
            .and_then(|info| info.get("chapters"))
            .and_then(Value::as_array)
            .map(Vec::len);
        let available = chapters
            .map(|count| format!("{count} chapters"))
            .unwrap_or_else(|| "?".to_owned());
        rows.push(vec![
            "chapter".into(),
            "flat|tree".into(),
            available,
            downloaded("chapter", None),
        ]);
    }

    // PDF: ownership-aware (`?` for catalog-only items).
    let pdf = match fact.pdf_available {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    };
    rows.push(vec![
        "pdf".into(),
        "companion".into(),
        pdf.into(),
        downloaded("pdf", None),
    ]);
    rows
}

/// Empty string → `-`.
fn show_or_dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, format: &str, variant: &str, size: Option<u64>) -> DownloadEntry {
        DownloadEntry {
            asin: "B0A".into(),
            marketplace: "de".into(),
            kind: kind.into(),
            content_format: format.into(),
            variant: variant.into(),
            file_path: "/dl/x".into(),
            file_size: size,
        }
    }

    fn book_facts() -> AssetFacts {
        facts(&serde_json::json!({
            "available_codecs": [
                {"name": "aax_44_64"}, {"name": "aax_44_128"}, {"name": "aax"},
            ],
            "product_images": {"500": "u", "1215": "u"},
            "is_pdf_url_available": false,
            "customer_rights": {"is_consumable_offline": true},
            "content_delivery_type": "SinglePartBook",
        }))
    }

    #[test]
    fn facts_extract_codecs_images_pdf_rights() {
        let fact = book_facts();
        assert_eq!(fact.codecs, ["aax_44_64", "aax_44_128", "aax"]);
        assert_eq!(fact.image_sizes, [500, 1215]);
        assert_eq!(fact.pdf_available, Some(false));
        assert_eq!(fact.offline_right, Some(true));
        assert_eq!(fact.delivery_type.as_deref(), Some("SinglePartBook"));
    }

    #[test]
    fn book_rows_tag_quality_size_and_download_state() {
        let metadata = (
            serde_json::json!({
                "content_reference": {
                    "content_format": "AAX_44_128",
                    "content_size_in_bytes": 312_000_000u64,
                },
                "chapter_info": {"chapters": [{}, {}, {}]},
            }),
            MetadataDrm::Adrm,
        );
        let downloads = vec![
            entry("audio", "AAX_44_128", "original", Some(312_000_000)),
            entry("audio", "AAX_44_128", "decrypted", Some(310_000_000)),
            entry("audio", "mp3_320", "reencoded", Some(200_000_000)),
            entry("cover", "500", "original", Some(50_000)),
        ];
        let rows = item_rows(&book_facts(), Some(&metadata), &downloads);
        let by_kind_detail: Vec<(String, String, String, String)> = rows
            .iter()
            .map(|r| (r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone()))
            .collect();

        // High-quality row carries the exact size and both audio variants.
        let high = by_kind_detail
            .iter()
            .find(|(_, d, _, _)| d.starts_with("aax_44_128"))
            .unwrap();
        assert!(high.1.contains("-q high"));
        assert!(high.2.contains("MiB") || high.2.contains("MB"), "{high:?}");
        assert!(high.3.contains("original") && high.3.contains("decrypted"));

        // Normal quality: available, nothing downloaded.
        let normal = by_kind_detail
            .iter()
            .find(|(_, d, _, _)| d.starts_with("aax_44_64"))
            .unwrap();
        assert_eq!(normal.2, "yes");
        assert_eq!(normal.3, "-");

        // The mp3 reencode matches no codec → its own row.
        assert!(
            by_kind_detail
                .iter()
                .any(|(k, d, _, dl)| k == "audio" && d == "mp3_320" && dl.contains("reencoded"))
        );

        // Chapters counted, cover sizes compact, pdf "no".
        assert!(
            by_kind_detail
                .iter()
                .any(|(k, _, a, _)| k == "chapter" && a == "3 chapters")
        );
        assert!(
            by_kind_detail
                .iter()
                .any(|(k, d, _, _)| k == "cover" && d == "500,1215")
        );
        assert!(
            by_kind_detail
                .iter()
                .any(|(k, _, a, _)| k == "pdf" && a == "no")
        );
    }

    #[test]
    fn widevine_only_marks_aax_rows_and_names_the_representation() {
        // Catalog announces aax codecs, but only the Widevine probe
        // answered (AUD-113, live case B08XDZCH78).
        let metadata = (
            serde_json::json!({
                "content_reference": {
                    "content_format": "M4A_XHE",
                    "content_size_in_bytes": 228_699_791u64,
                },
                "chapter_info": {"chapters": [{}, {}]},
            }),
            MetadataDrm::Widevine,
        );
        let rows = item_rows(&book_facts(), Some(&metadata), &[]);
        let aax_rows: Vec<_> = rows
            .iter()
            .filter(|r| r[0] == "audio" && r[1].starts_with("aax"))
            .collect();
        assert!(!aax_rows.is_empty());
        assert!(aax_rows.iter().all(|r| r[2] == "no (widevine only)"));
        let widevine = rows
            .iter()
            .find(|r| r[0] == "audio" && r[1].starts_with("widevine"))
            .unwrap();
        assert_eq!(widevine[1], "widevine (m4a_xhe)");
        assert!(widevine[2].contains("MiB"), "{widevine:?}");
        // Chapters come from the Widevine response — no `?`.
        assert!(
            rows.iter()
                .any(|r| r[0] == "chapter" && r[2] == "2 chapters")
        );
    }

    #[test]
    fn streaming_only_and_podcast_shapes() {
        // No aax codecs + no offline right → widevine row.
        let plus = facts(&serde_json::json!({
            "available_codecs": [{"name": "mp4_44_64"}],
            "customer_rights": {"is_consumable_offline": false},
            "content_delivery_type": "SinglePartBook",
        }));
        let rows = item_rows(&plus, None, &[]);
        let audio = &rows[0];
        assert_eq!(audio[1], "widevine (aac|xhe)");
        assert_eq!(audio[2], "streaming only");

        // With the Widevine probe answered, the representation is named.
        let metadata = (
            serde_json::json!({
                "content_reference": {
                    "content_format": "M4A_AAC",
                    "content_size_in_bytes": 100u64,
                },
            }),
            MetadataDrm::Widevine,
        );
        let rows = item_rows(&plus, Some(&metadata), &[]);
        assert_eq!(rows[0][1], "widevine (m4a_aac)");

        // Podcast episode → mpeg row; parent → episodes-only marker and
        // no chapter row.
        let episode = facts(&serde_json::json!({
            "content_delivery_type": "PodcastEpisode",
        }));
        let rows = item_rows(&episode, None, &[]);
        assert_eq!(rows[0][1], "mpeg (podcast episode)");

        let parent = facts(&serde_json::json!({
            "content_delivery_type": "Periodical",
        }));
        let rows = item_rows(&parent, None, &[]);
        assert_eq!(rows[0][2], "episodes only");
        assert!(!rows.iter().any(|r| r[0] == "chapter"));

        // Unknown metadata → chapters degrade to `?`.
        let rows = item_rows(&book_facts(), None, &[]);
        assert!(rows.iter().any(|r| r[0] == "chapter" && r[2] == "?"));
    }

    #[test]
    fn clap_shape() {
        let parse = |args: &[&str]| info_command().try_get_matches_from(args);
        assert!(parse(&["info", "--asin", "B0A"]).is_ok());
        assert!(parse(&["info", "--title", "waffen"]).is_ok());
        assert!(parse(&["info"]).is_err());
    }
}
