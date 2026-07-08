//! `library sync` — full/delta sync into the per-account database,
//! podcast-episode resolution, the staleness-driven auto-sync used by
//! the read commands, and the sync summaries.

use std::sync::Arc;

use anyhow::Result;
use futures::{StreamExt as _, TryStreamExt as _};
use reqwest::Method;
use tokio::sync::Semaphore;

use crate::api::client::Client;
use crate::api::paginator;
use crate::config::ctx::Ctx;
use crate::db::{self, Db, SyncLogEntry, UpsertItem};
use crate::models::library as model;

use super::changes::print_changes;
use super::*;

/// Options for [`sync_library`].
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Force a full resync even when a state token exists.
    pub full: bool,
    /// Items per page.
    pub page_size: u32,
    /// Resolve podcast episodes for parents touched in this run.
    pub resolve_podcasts: bool,
    /// Record per-item changes to the change_log (skipped on the initial sync).
    pub record_changes: bool,
    /// Days to keep change_log entries (pruned at sync end; 0 = forever).
    pub change_retention_days: u32,
}

/// Result of a sync run.
#[derive(Debug, Default)]
pub struct SyncSummary {
    /// `full` or `delta`.
    pub mode: &'static str,
    /// Number of pages fetched.
    pub pages: u32,
    /// Added / changed / removed items of this run (asin + title).
    pub changes: db::ApplyOutcome,
    /// Whether the database was empty before this run (initial sync — the
    /// change listing is suppressed, everything would just be "added").
    pub initial: bool,
    /// Podcast parents whose episodes were refreshed.
    pub podcasts_resolved: usize,
    /// Episodes inserted or updated.
    pub episodes_upserted: usize,
    /// Episodes soft-deleted (vanished from their feed).
    pub episodes_deleted: usize,
    /// State token after the run, if the API issued one.
    pub state_token: Option<String>,
}

/// Syncs the library into the database: full (status=Active) without a
/// state token, delta (status=Active,Revoked plus `state_token`) with
/// one. Each page is applied atomically together with its audit-log
/// row; the freshest state token wins.
pub async fn sync_library(
    client: &Client,
    db: &Db,
    marketplace: &str,
    options: SyncOptions,
    sem: &Semaphore,
) -> Result<SyncSummary> {
    let settings = db
        .ensure_sync_state(marketplace.to_owned(), DEFAULT_RESPONSE_GROUPS.to_owned())
        .await?;
    let request_token = if options.full {
        None
    } else {
        settings.last_state_token.clone()
    };
    let mode = if request_token.is_some() {
        "delta"
    } else {
        "full"
    };
    tracing::info!(mode, "library sync started");

    let response_groups = settings.response_groups.clone();
    let page_size = options.page_size.to_string();
    let stream = paginator::pages(|_continuation| {
        let mut request = client
            .request(Method::GET, "/1.0/library")
            .country_code(marketplace)
            .query("response_groups", &response_groups)
            .query("num_results", &page_size)
            .query("image_sizes", DEFAULT_IMAGE_SIZES)
            .query("include_pending", "true")
            .query(
                "status",
                if request_token.is_some() {
                    "Active,Revoked"
                } else {
                    "Active"
                },
            );
        if let Some(token) = &request_token {
            request = request.query("state_token", token);
        }
        request
    });
    futures::pin_mut!(stream);

    // Empty before this run → initial sync (the change listing is suppressed).
    let initial = db.count_active(vec![marketplace.to_owned()]).await? == 0;
    let mut summary = SyncSummary {
        mode,
        state_token: request_token.clone(),
        initial,
        ..Default::default()
    };
    // Podcast parents touched in this run: (asin, announced episode count).
    let mut podcast_parents: Vec<(String, Option<u64>)> = Vec::new();

    while let Some(page) = {
        let request_time = db::now_iso_utc();
        stream.try_next().await?.map(|page| (page, request_time))
    } {
        let (page, request_time) = page;
        summary.pages += 1;

        let mut upserts = Vec::new();
        let mut deletes: Vec<String> = model::extract_removed_asins(&page.body)
            .into_iter()
            .collect();
        for item in model::normalize_items(&page.body) {
            let Some(asin) = item.get("asin").and_then(serde_json::Value::as_str) else {
                tracing::warn!("skipping library item without asin");
                continue;
            };
            if model::should_soft_delete(&item) {
                deletes.push(asin.to_owned());
                continue;
            }
            if options.resolve_podcasts && model::is_parent_podcast(&item) {
                let announced = item
                    .get("episode_count")
                    .and_then(serde_json::Value::as_u64);
                podcast_parents.push((asin.to_owned(), announced));
            }
            let Some(full_title) = model::build_full_title(&item) else {
                tracing::warn!(asin, "skipping library item without title");
                continue;
            };
            let series = model::extract_series(&item)
                .into_iter()
                .map(|entry| crate::db::SeriesRef {
                    series_asin: entry.asin,
                    series_title: entry.title,
                    sequence: entry.sequence,
                })
                .collect();
            upserts.push(UpsertItem {
                asin: asin.to_owned(),
                title: item
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                subtitle: item
                    .get("subtitle")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                full_title,
                doc: item.to_string(),
                series,
            });
        }

        let log = SyncLogEntry {
            request_time_utc: request_time,
            request_state_token_utc: request_token.as_deref().and_then(db::state_token_iso),
            response_time_utc: db::now_iso_utc(),
            response_state_token_utc: page.state_token.as_deref().and_then(db::state_token_iso),
            http_status: Some(200),
            note: Some(format!("sync-{mode}-page-{}", summary.pages)),
        };

        let recording = db::ChangeRecording {
            // The initial sync would log the whole library as "added" — skip it.
            record: options.record_changes && !initial,
            mode,
        };
        let outcome = db
            .apply_page_recording(
                marketplace.to_owned(),
                upserts,
                deletes,
                log,
                page.state_token.clone(),
                recording,
            )
            .await?;
        tracing::debug!(
            page = summary.pages,
            added = outcome.added.len(),
            changed = outcome.changed.len(),
            removed = outcome.removed.len(),
            "library page applied"
        );
        if page.state_token.is_some() {
            summary.state_token = page.state_token;
        }
        summary.changes.extend(outcome);
    }

    // Resolve podcasts concurrently, bounded by the shared semaphore (each
    // resolution holds one permit for its whole — sequential — run). A
    // failing podcast is non-fatal: warn and skip, don't abort the sync.
    let response_groups = &response_groups;
    let page_size = &page_size;
    let episode_results: Vec<Result<(usize, usize)>> = futures::stream::iter(podcast_parents)
        .map(|(parent_asin, announced)| async move {
            let _permit = sem.acquire().await.expect("sync semaphore is never closed");
            resolve_episodes(
                client,
                db,
                marketplace,
                response_groups,
                page_size,
                &parent_asin,
                announced,
            )
            .await
        })
        .buffer_unordered(SYNC_RESOLUTION_CONCURRENCY)
        .collect()
        .await;
    for result in episode_results {
        match result {
            Ok((upserted, deleted)) => {
                summary.podcasts_resolved += 1;
                summary.episodes_upserted += upserted;
                summary.episodes_deleted += deleted;
            }
            Err(error) => tracing::warn!(%error, "podcast episode resolution failed; skipping"),
        }
    }

    // Keep the change log bounded (by recorded time). Non-fatal.
    if options.change_retention_days > 0
        && let Err(error) = db.prune_change_log(options.change_retention_days).await
    {
        tracing::warn!(%error, "could not prune the change log");
    }

    tracing::info!(
        mode,
        pages = summary.pages,
        added = summary.changes.added.len(),
        changed = summary.changes.changed.len(),
        removed = summary.changes.removed.len(),
        podcasts = summary.podcasts_resolved,
        episodes = summary.episodes_upserted,
        "library sync finished"
    );
    Ok(summary)
}

/// Fetches all episodes of one podcast parent and replaces the stored
/// episode set. The library listing (`/1.0/library?parent_asin=…`) is
/// capped at the newest ~10; when it falls short of the announced
/// `episode_count`, the complete list comes from the parent product's
/// catalog relationships (`relationships_v2`, one request), with episode
/// metadata batched by ASIN. Library documents win for episodes present
/// in both.
async fn resolve_episodes(
    client: &Client,
    db: &Db,
    marketplace: &str,
    response_groups: &str,
    page_size: &str,
    parent_asin: &str,
    announced: Option<u64>,
) -> Result<(usize, usize)> {
    tracing::debug!(parent_asin, "resolving podcast episodes");

    fn push(
        episodes: &mut std::collections::BTreeMap<String, crate::db::UpsertEpisode>,
        parent_asin: &str,
        item: serde_json::Value,
    ) {
        let Some(asin) = item.get("asin").and_then(serde_json::Value::as_str) else {
            return;
        };
        // The parent itself can appear in its own child listing; the
        // first (library) document wins over a later catalog one.
        if asin == parent_asin || episodes.contains_key(asin) {
            return;
        }
        let Some(full_title) = model::build_full_title(&item) else {
            tracing::warn!(asin, "skipping episode without title");
            return;
        };
        episodes.insert(
            asin.to_owned(),
            crate::db::UpsertEpisode {
                asin: asin.to_owned(),
                title: item
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                subtitle: item
                    .get("subtitle")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                full_title,
                doc: item.to_string(),
            },
        );
    }
    let mut episodes: std::collections::BTreeMap<String, crate::db::UpsertEpisode> =
        std::collections::BTreeMap::new();

    let stream = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .country_code(marketplace)
            .query("parent_asin", parent_asin)
            .query("response_groups", response_groups)
            .query("num_results", page_size)
            .query("status", "Active")
    });
    futures::pin_mut!(stream);
    while let Some(page) = stream.try_next().await? {
        for item in model::normalize_items(&page.body) {
            push(&mut episodes, parent_asin, item);
        }
    }

    // The library listing is capped (10 newest); the complete episode list
    // comes from the parent product's relationships, with details batched by
    // ASIN. The `/catalog/products?parent_asin` page pagination is NOT used —
    // it silently caps below `episode_count` (AUD-49).
    if announced.is_some_and(|count| count as usize > episodes.len()) {
        tracing::info!(
            parent_asin,
            announced,
            from_library = episodes.len(),
            "library listing is incomplete; resolving episodes from the catalog relationships"
        );
        let child_asins = fetch_episode_asins(client, marketplace, parent_asin).await?;
        // Only the episodes not already covered by the (authoritative) library
        // docs need a catalog detail fetch.
        let missing: Vec<String> = child_asins
            .into_iter()
            .filter(|asin| asin != parent_asin && !episodes.contains_key(asin))
            .collect();
        for item in fetch_catalog_details(client, marketplace, missing).await? {
            push(&mut episodes, parent_asin, item);
        }
        if let Some(announced) = announced
            && announced as usize != episodes.len()
        {
            tracing::warn!(
                parent_asin,
                announced,
                fetched = episodes.len(),
                "episode count still differs after the catalog relationships fetch"
            );
        }
    }

    Ok(db
        .apply_episodes(
            marketplace.to_owned(),
            parent_asin.to_owned(),
            episodes.into_values().collect(),
        )
        .await?)
}

/// Episode child ASINs of a podcast parent, from the parent product's
/// catalog relationships (`relationships_v2`, a single request). Filters to
/// `relationship_type == "episode"` so the `PodcastSeason` container (a
/// `child` whose own children are the episodes) is excluded. This list is
/// complete — unlike the `/catalog/products?parent_asin` page pagination,
/// which silently caps below `episode_count` (AUD-49).
async fn fetch_episode_asins(
    client: &Client,
    marketplace: &str,
    parent_asin: &str,
) -> Result<Vec<String>> {
    let response = client
        .request(Method::GET, format!("/1.0/catalog/products/{parent_asin}"))
        .country_code(marketplace)
        .query("response_groups", "relationships_v2")
        .send()
        .await?;
    let body: serde_json::Value = response.error_for_status()?.json().await?;
    let Some(relationships) = body
        .get("product")
        .and_then(|product| product.get("relationships"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(Vec::new());
    };
    Ok(relationships
        .iter()
        .filter(|rel| {
            rel.get("relationship_to_product")
                .and_then(serde_json::Value::as_str)
                == Some("child")
                && rel
                    .get("relationship_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("episode")
        })
        .filter_map(|rel| {
            rel.get("asin")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect())
}

/// Catalog product documents for a set of ASINs, batched 50 per request
/// (CSV `asins=`, the catalog's per-request limit), with the catalog-valid
/// response groups. The batches run sequentially: the whole episode
/// resolution already holds one shared permit (see `sync_library`), so the
/// global concurrency cap stays meaningful and a single podcast does one
/// request at a time.
async fn fetch_catalog_details(
    client: &Client,
    marketplace: &str,
    asins: Vec<String>,
) -> Result<Vec<serde_json::Value>> {
    let mut products = Vec::new();
    for chunk in asins.chunks(50) {
        let joined = chunk.join(",");
        let response = client
            .request(Method::GET, "/1.0/catalog/products")
            .country_code(marketplace)
            .query("asins", &joined)
            .query("response_groups", CATALOG_RESPONSE_GROUPS)
            .send()
            .await?;
        let body: serde_json::Value = response.error_for_status()?.json().await?;
        if let Some(items) = body.get("products").and_then(serde_json::Value::as_array) {
            products.extend(items.iter().cloned());
        }
    }
    Ok(products)
}

/// `library sync` — also reused by `collections archive … --sync`
/// (AUD-111), which runs a plain delta sync after an archive mutation.
pub(crate) async fn sync(
    ctx: &Ctx,
    full: bool,
    no_podcasts: bool,
    show_volatile: bool,
) -> Result<()> {
    let client = ctx.client().await?;
    let db = ctx.open_library_db().await?;

    // One sync per library at a time (fd-lock, D9).
    let lock_path = db.path().with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .try_write()
        .map_err(|_| anyhow::anyhow!("another sync is already running for this library"))?;

    let config = ctx.db_config()?;
    let marketplaces = ctx.marketplaces()?;

    let options = SyncOptions {
        full,
        page_size: config.page_size.clamp(10, 1000),
        resolve_podcasts: config.resolve_podcasts && !no_podcasts,
        record_changes: config.record_changes,
        change_retention_days: config.change_retention_days,
    };
    // Shared cap on concurrent resolution fetches across all marketplaces.
    let sem = Arc::new(Semaphore::new(SYNC_RESOLUTION_CONCURRENCY));
    let multi = marketplaces.len() > 1;
    let db_ref = &db;

    // Sync marketplaces concurrently (each paginates sequentially via its own
    // continuation/state token); one failing marketplace must not abort the
    // others, so results are aggregated and failures reported at the end.
    let mut results: Vec<(String, Result<SyncSummary>)> =
        futures::stream::iter(marketplaces.clone())
            .map(|marketplace| {
                let options = options.clone();
                let sem = sem.clone();
                async move {
                    let summary = sync_library(client, db_ref, &marketplace, options, &sem).await;
                    (marketplace, summary)
                }
            })
            .buffer_unordered(SYNC_MARKETPLACE_CONCURRENCY)
            .collect()
            .await;
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let mut pages = 0u32;
    let mut added = 0usize;
    let mut changed = 0usize;
    let mut removed = 0usize;
    let mut podcasts_resolved = 0usize;
    let mut episodes_upserted = 0usize;
    let mut episodes_deleted = 0usize;
    let mut mode = "delta";
    let mut failures = 0usize;
    // Per-marketplace change details for the listing; the initial sync (empty
    // DB) is skipped — everything would just be "added".
    let mut change_sections: Vec<(String, db::ApplyOutcome)> = Vec::new();
    for (marketplace, result) in results {
        match result {
            Ok(summary) => {
                if multi {
                    eprintln!(
                        "{marketplace}: {} mode, +{} ~{} -{}",
                        summary.mode,
                        summary.changes.added.len(),
                        summary.changes.changed.len(),
                        summary.changes.removed.len()
                    );
                }
                mode = summary.mode;
                pages += summary.pages;
                added += summary.changes.added.len();
                changed += summary.changes.changed.len();
                removed += summary.changes.removed.len();
                podcasts_resolved += summary.podcasts_resolved;
                episodes_upserted += summary.episodes_upserted;
                episodes_deleted += summary.episodes_deleted;
                if !summary.initial {
                    change_sections.push((marketplace, summary.changes));
                }
            }
            Err(error) => {
                eprintln!("{marketplace}: sync failed: {error:#}");
                failures += 1;
            }
        }
    }

    ctx.print(&crate::output::Output::KeyValue(vec![
        ("marketplaces".into(), marketplaces.join(",")),
        ("mode".into(), mode.into()),
        ("pages".into(), pages.to_string()),
        ("added".into(), added.to_string()),
        ("changed".into(), changed.to_string()),
        ("removed".into(), removed.to_string()),
        ("podcasts_resolved".into(), podcasts_resolved.to_string()),
        ("episodes_upserted".into(), episodes_upserted.to_string()),
        ("episodes_deleted".into(), episodes_deleted.to_string()),
        ("database".into(), db.path().display().to_string()),
    ]));
    print_changes(&change_sections, multi, show_volatile);
    if failures > 0 {
        anyhow::bail!(
            "{failures} of {} marketplace(s) failed to sync",
            marketplaces.len()
        );
    }
    Ok(())
}

/// Whether the last sync is older than `sync_max_age` (no sync = stale).
fn is_stale(last_sync_utc: Option<&str>, max_age: std::time::Duration) -> bool {
    let Some(last) = last_sync_utc else {
        return true;
    };
    let format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    let Ok(parsed) = time::PrimitiveDateTime::parse(last, format) else {
        return true;
    };
    let age = time::OffsetDateTime::now_utc() - parsed.assume_utc();
    age > max_age
}

/// Runs a delta sync before a read when `auto_sync = delta` and the
/// data is older than `sync_max_age` (archived architecture §8). Skipped when
/// another process holds the sync lock.
pub(crate) async fn maybe_auto_sync(ctx: &Ctx, db: &Db) -> Result<()> {
    let config = ctx.db_config()?;
    if config.auto_sync == crate::config::schema::AutoSync::None {
        return Ok(());
    }
    let max_age = crate::config::schema::parse_duration(&config.sync_max_age)?;
    if !is_stale(db.last_sync_utc().await?.as_deref(), max_age) {
        return Ok(());
    }

    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(db.path().with_extension("lock"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let Ok(_guard) = lock.try_write() else {
        tracing::debug!("another sync is running; skipping auto-sync");
        return Ok(());
    };

    tracing::info!("library data is stale; running auto-sync");
    let client = ctx.client().await?;
    let sem = Semaphore::new(SYNC_RESOLUTION_CONCURRENCY);
    for marketplace in ctx.marketplaces()? {
        let options = SyncOptions {
            full: false,
            page_size: config.page_size.clamp(10, 1000),
            resolve_podcasts: config.resolve_podcasts,
            record_changes: config.record_changes,
            change_retention_days: config.change_retention_days,
        };
        sync_library(client, db, &marketplace, options, &sem).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::time::Duration;

    #[test]
    fn staleness_decision() {
        // No sync yet or unparsable timestamp: stale.
        assert!(is_stale(None, Duration::from_secs(3600)));
        assert!(is_stale(Some("garbage"), Duration::from_secs(3600)));
        // Fresh sync within the window: not stale.
        let now = db::now_iso_utc();
        assert!(!is_stale(Some(&now), Duration::from_secs(3600)));
        // Old sync: stale.
        assert!(is_stale(
            Some("2020-01-01T00:00:00Z"),
            Duration::from_secs(3600)
        ));
    }
}
