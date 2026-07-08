//! `audible series` — series overview from the local database, plus the
//! catalog diff that answers "which volumes am I missing?".

use anyhow::{Result, bail};
use clap::Arg;
use futures::{StreamExt as _, TryStreamExt as _};
use reqwest::Method;

use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::db::Db;
use crate::models::library as model;
use crate::output::Output;

use super::library::maybe_auto_sync;

/// `audible series`.
pub struct SeriesCommand;

#[async_trait::async_trait]
impl super::Command for SeriesCommand {
    fn name(&self) -> &'static str {
        "series"
    }

    fn clap(&self) -> clap::Command {
        let series_arg = || {
            Arg::new("series")
                .value_name("ASIN|TITLE")
                .help("Series, by ASIN or title substring")
        };
        clap::Command::new(self.name())
            .about("Series in your library (from the local database)")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(clap::Command::new("list").about("List series with owned volumes"))
            .subcommand(
                clap::Command::new("show")
                    .about("Show the owned volumes of a series")
                    .arg(series_arg().required(true)),
            )
            .subcommand(
                clap::Command::new("missing")
                    .about("Find volumes you do not own (asks the catalog)")
                    .arg(series_arg())
                    .arg(
                        clap::Arg::new("include_unreleased")
                            .long("include-unreleased")
                            .action(clap::ArgAction::SetTrue)
                            .help("Also list volumes that are not released yet"),
                    ),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let needle = |m: &clap::ArgMatches| m.get_one::<String>("series").cloned();
        match matches.subcommand() {
            Some(("list", _)) => list(ctx).await,
            Some(("show", sub)) => show(ctx, needle(sub).expect("required")).await,
            Some(("missing", sub)) => {
                missing(ctx, needle(sub), sub.get_flag("include_unreleased")).await
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

async fn list(ctx: &Ctx) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let rows: Vec<Vec<String>> = db
        .series_overview(marketplaces)
        .await?
        .into_iter()
        .map(|series| {
            vec![
                series.marketplace,
                series.series_asin,
                series.series_title,
                series.owned.to_string(),
                series.sequences.unwrap_or_default(),
            ]
        })
        .collect();
    if rows.is_empty() {
        eprintln!("no series in the library — run `audible library sync` first");
    }
    ctx.print(&Output::table(
        vec!["mp", "asin", "title", "owned", "sequences"],
        rows,
    ));
    Ok(())
}

async fn show(ctx: &Ctx, needle: String) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let rows: Vec<Vec<String>> = db
        .series_items(marketplaces, needle.clone())
        .await?
        .into_iter()
        .map(|item| {
            vec![
                item.marketplace,
                item.series_title,
                item.sequence,
                item.asin,
                item.full_title,
            ]
        })
        .collect();
    if rows.is_empty() {
        bail!("no series in the library matches {needle:?}");
    }
    ctx.print(&Output::table(
        vec!["mp", "series", "sequence", "asin", "title"],
        rows,
    ));
    Ok(())
}

/// A volume of a series that is not covered by the owned items.
#[derive(Debug)]
pub struct MissingVolume {
    /// Series title.
    pub series_title: String,
    /// Position within the series (`"?"` when the catalog has none).
    pub sequence: String,
    /// ASIN of the missing volume.
    pub asin: String,
    /// Title of the missing volume (resolved from the catalog).
    pub title: String,
    /// Release date, if the catalog provides one.
    pub release_date: Option<String>,
    /// Whether the volume is already released.
    pub released: bool,
}

/// Whether a release date (`YYYY-MM-DD`, optionally with a time suffix)
/// lies in the past. Missing or unparsable dates count as released.
fn is_released(release_date: Option<&str>, today: time::Date) -> bool {
    let Some(date) = release_date else {
        return true;
    };
    let format = time::macros::format_description!("[year]-[month]-[day]");
    match time::Date::parse(date.get(..10).unwrap_or(date), format) {
        Ok(parsed) => parsed <= today,
        Err(_) => true,
    }
}

/// How many catalog batch requests run concurrently during series
/// resolution. The ASIN list is chunked to 50 per request (the catalog
/// limit) and the chunks fetch in parallel over the multiplexed HTTP/2
/// connection. A small internal cap — no `--jobs` flag (uniform requests).
const SERIES_CATALOG_CONCURRENCY: usize = 8;

/// `series_asin -> [(sequence, child_asin)]`, from the catalog relationships.
type SeriesChildren = std::collections::BTreeMap<String, Vec<(Option<String>, String)>>;

/// Computes the missing volumes for the matched series: owned coverage
/// from the database (ranges like "1-6" count as volumes 1–6) against the
/// authoritative volume list from each series product's relationships.
///
/// The catalog work is batched: every series' child relationships are
/// fetched in `⌈series/50⌉` requests (CSV `asins=`, run concurrently)
/// instead of one request per series, and the missing volumes' titles and
/// release dates resolve in a single further batch across all series.
pub async fn missing_volumes(
    client: &Client,
    db: &Db,
    marketplace: &str,
    needle: Option<String>,
) -> Result<Vec<MissingVolume>> {
    let mut series = db.series_overview(vec![marketplace.to_owned()]).await?;
    if let Some(needle) = &needle {
        let lowered = needle.to_lowercase();
        series.retain(|s| {
            s.series_asin == *needle || s.series_title.to_lowercase().contains(&lowered)
        });
        if series.is_empty() {
            bail!("no series in the library matches {needle:?}");
        }
    }

    // Authoritative volume list for every matched series, fetched in one
    // batch of catalog requests instead of one request per series.
    let series_asins: Vec<String> = series.iter().map(|s| s.series_asin.clone()).collect();
    let children_by_series = fetch_series_children(client, marketplace, series_asins).await?;

    // Per series, compute the absent volumes (database-only work) and
    // collect every absent ASIN so their details resolve in one batch.
    struct Pending {
        series_title: String,
        absent: Vec<(Option<String>, String)>,
    }
    let mut pending: Vec<Pending> = Vec::new();
    let mut all_absent: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in series {
        let children = match children_by_series.get(&entry.series_asin) {
            Some(children) if !children.is_empty() => children,
            _ => {
                tracing::warn!(
                    series = entry.series_title,
                    "catalog returned no child volumes for this series"
                );
                continue;
            }
        };

        // Coverage from owned volumes (sequence numbers, ranges expanded).
        let owned = db
            .series_items(vec![marketplace.to_owned()], entry.series_asin.clone())
            .await?;
        let covered: Vec<f64> = owned
            .iter()
            .flat_map(|item| model::sequence_numbers(&item.sequence))
            .collect();
        let owned_asins: std::collections::BTreeSet<&str> =
            owned.iter().map(|item| item.asin.as_str()).collect();

        // A child is missing when neither its ASIN is owned nor its
        // sequence numbers are fully covered (an owned omnibus "1-6"
        // covers the single volumes 1..6 — and vice versa).
        let mut absent: Vec<(Option<String>, String)> = children
            .iter()
            .filter(|(sequence, asin)| {
                if owned_asins.contains(asin.as_str()) {
                    return false;
                }
                let numbers = sequence
                    .as_deref()
                    .map(model::sequence_numbers)
                    .unwrap_or_default();
                numbers.is_empty() || !numbers.iter().all(|n| covered.contains(n))
            })
            .cloned()
            .collect();
        absent.sort_by(|(a, _), (b, _)| {
            let key = |sequence: &Option<String>| {
                sequence
                    .as_deref()
                    .map(model::sequence_numbers)
                    .unwrap_or_default()
                    .first()
                    .copied()
                    .unwrap_or(f64::MAX)
            };
            key(a).total_cmp(&key(b))
        });

        all_absent.extend(absent.iter().map(|(_, asin)| asin.clone()));
        pending.push(Pending {
            series_title: entry.series_title,
            absent,
        });
    }

    // Resolve title and release date of every missing volume across all
    // series in one batch.
    let details = volume_details(client, marketplace, all_absent.into_iter().collect()).await?;
    let today = time::OffsetDateTime::now_utc().date();
    let mut missing = Vec::new();
    for entry in pending {
        for (sequence, asin) in entry.absent {
            let (title, release_date) = details.get(&asin).cloned().unwrap_or_default();
            missing.push(MissingVolume {
                series_title: entry.series_title.clone(),
                sequence: sequence.unwrap_or_else(|| "?".into()),
                title,
                released: is_released(release_date.as_deref(), today),
                release_date,
                asin,
            });
        }
    }
    Ok(missing)
}

/// Child volumes for many series, fetched in batches of 50 ASINs per
/// catalog request (CSV `asins=`, `relationships_v2` group) run
/// concurrently. Returns `series_asin -> [(sequence, child_asin)]`.
///
/// `relationships_v2` is what the official app requests and what the rest
/// of the codebase already uses. For the `child`/`series` entries this
/// command reads it is identical to the legacy `relationships` group (same
/// `asin`/`sequence`, verified live); v2 only drops the per-component
/// `sku`/`sku_lite` fields we never look at, so it is a touch leaner. The
/// batched response carries each product's `relationships` array just like
/// the single-product endpoint.
async fn fetch_series_children(
    client: &Client,
    marketplace: &str,
    series_asins: Vec<String>,
) -> Result<SeriesChildren> {
    let chunks: Vec<Vec<String>> = series_asins
        .chunks(50)
        .map(|chunk| chunk.to_vec())
        .collect();
    let parts: Vec<SeriesChildren> = futures::stream::iter(chunks)
        .map(|chunk| async move {
            let joined = chunk.join(",");
            let response = client
                .request(Method::GET, "/1.0/catalog/products")
                .country_code(marketplace)
                .query("asins", &joined)
                .query("response_groups", "relationships_v2")
                .send()
                .await?;
            let body: serde_json::Value = response.error_for_status()?.json().await?;
            let mut part = std::collections::BTreeMap::new();
            if let Some(products) = body.get("products").and_then(serde_json::Value::as_array) {
                for product in products {
                    if let Some(asin) = product.get("asin").and_then(serde_json::Value::as_str) {
                        part.insert(asin.to_owned(), model::extract_series_children(product));
                    }
                }
            }
            Ok::<_, anyhow::Error>(part)
        })
        .buffer_unordered(SERIES_CATALOG_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(parts.into_iter().flatten().collect())
}

/// Title and release date for a set of catalog ASINs (batched, 50 per
/// request, run concurrently).
async fn volume_details(
    client: &Client,
    marketplace: &str,
    asins: Vec<String>,
) -> Result<std::collections::BTreeMap<String, (String, Option<String>)>> {
    let chunks: Vec<Vec<String>> = asins.chunks(50).map(|chunk| chunk.to_vec()).collect();
    let parts: Vec<std::collections::BTreeMap<String, (String, Option<String>)>> =
        futures::stream::iter(chunks)
            .map(|chunk| async move {
                let joined = chunk.join(",");
                let response = client
                    .request(Method::GET, "/1.0/catalog/products")
                    .country_code(marketplace)
                    .query("asins", &joined)
                    .query("response_groups", "product_desc,product_attrs")
                    .send()
                    .await?;
                let body: serde_json::Value = response.error_for_status()?.json().await?;
                let mut part = std::collections::BTreeMap::new();
                if let Some(products) = body.get("products").and_then(serde_json::Value::as_array) {
                    for product in products {
                        let Some(asin) = product.get("asin").and_then(serde_json::Value::as_str)
                        else {
                            continue;
                        };
                        let title = product
                            .get("title")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let release_date = product
                            .get("release_date")
                            .or_else(|| product.get("publication_datetime"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        part.insert(asin.to_owned(), (title, release_date));
                    }
                }
                Ok::<_, anyhow::Error>(part)
            })
            .buffer_unordered(SERIES_CATALOG_CONCURRENCY)
            .try_collect()
            .await?;
    Ok(parts.into_iter().flatten().collect())
}

async fn missing(ctx: &Ctx, needle: Option<String>, include_unreleased: bool) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let client = ctx.client().await?;
    let marketplaces = ctx.marketplaces()?;

    // Each marketplace's series are resolved against that marketplace's
    // catalog host; rows are tagged with the marketplace.
    let mut tagged: Vec<(String, MissingVolume)> = Vec::new();
    let mut unreleased = 0usize;
    for cc in &marketplaces {
        let all = missing_volumes(client, &db, cc, needle.clone()).await?;
        unreleased += all.iter().filter(|volume| !volume.released).count();
        for volume in all {
            if include_unreleased || volume.released {
                tagged.push((cc.clone(), volume));
            }
        }
    }

    if tagged.is_empty() {
        eprintln!("no missing volumes detected");
    }
    if unreleased > 0 && !include_unreleased {
        eprintln!("{unreleased} not yet released volume(s) hidden (--include-unreleased)");
    }
    let rows = tagged
        .into_iter()
        .map(|(mp, volume)| {
            vec![
                mp,
                volume.series_title,
                volume.sequence,
                volume.asin,
                volume.title,
                volume.release_date.unwrap_or_default(),
            ]
        })
        .collect();
    ctx.print(&Output::table(
        vec!["mp", "series", "sequence", "asin", "title", "release_date"],
        rows,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_decision() {
        let today = time::macros::date!(2026 - 06 - 11);
        assert!(is_released(Some("2026-06-11"), today));
        assert!(is_released(Some("2020-01-01T00:00:00Z"), today));
        assert!(!is_released(Some("2027-03-01"), today));
        // Unknown or unparsable dates count as released.
        assert!(is_released(None, today));
        assert!(is_released(Some("soon"), today));
    }
}
