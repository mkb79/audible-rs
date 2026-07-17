//! `library sync` — the command body over the sync engine in
//! [`crate::library_sync`] (per-marketplace fan-out, summary printing,
//! change listing), plus the bounded reflection poller shared by the
//! membership and archive mutations. The poller lives here, not with the
//! engine: it drives the full `sync` command body (whose summary output
//! is part of the `--sync` contract) and speaks to the user via stderr.

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt as _;
use tokio::sync::Semaphore;

use crate::config::ctx::Ctx;
use crate::db;
use crate::library_sync::{SYNC_RESOLUTION_CONCURRENCY, SyncOptions, SyncSummary, sync_library};

use super::changes::print_changes;
use super::*;

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

/// Delays before each bounded reflection attempt: the server indexes
/// membership/archive mutations asynchronously (within seconds).
const REFLECT_ATTEMPT_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(10),
];

/// Runs bounded delta syncs until `reflected` accepts every item's stored
/// doc (audit 2026-07-17, D4 — the membership and archive pollers were
/// two copies of this loop). Each attempt waits first (the server indexes
/// mutations asynchronously), then delta-syncs, then checks the predicate
/// over each item's stored doc (`None` = no doc). `what` words the final
/// warning. Without `--sync`, prints the standard hint and returns.
pub(crate) async fn poll_until_reflected(
    ctx: &Ctx,
    sync_requested: bool,
    marketplace: &str,
    asins: &[String],
    what: &str,
    reflected: impl Fn(Option<&str>) -> bool,
) -> Result<()> {
    if !sync_requested {
        eprintln!("note: run `audible library sync` to reflect the change in the local library");
        return Ok(());
    }
    let db = ctx.open_library_db().await?;
    for (attempt, delay) in REFLECT_ATTEMPT_DELAYS.iter().enumerate() {
        if attempt > 0 {
            eprintln!("change not in the library view yet; retrying the sync…");
        }
        tokio::time::sleep(*delay).await;
        sync(ctx, false, false, false).await?;
        let mut all_reflected = true;
        for asin in asins {
            let doc = db.item_doc(asin.clone(), marketplace.to_owned()).await?;
            if !reflected(doc.as_deref()) {
                all_reflected = false;
                break;
            }
        }
        if all_reflected {
            return Ok(());
        }
    }
    eprintln!(
        "warning: {what} has not reached the library view yet — \
         run `audible library sync` again in a moment"
    );
    Ok(())
}
