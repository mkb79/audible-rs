//! `library episodes <SHOW>` — the episode drill-down of a followed
//! podcast, answered from the local database (episodes are stored by
//! `library sync`). Moved here from the deprecated `podcasts` noun
//! (AUD-175): child episodes of a followed show live only in the
//! `episodes` table, so no `library list --kind …` variant can show
//! them — this listing is the way to obtain episode ASINs (e.g. for
//! downloads).

use anyhow::{Result, bail};

use crate::config::ctx::Ctx;
use crate::output::Output;

use crate::library_sync::maybe_auto_sync_for_reads;

/// Lists the stored episodes of one followed show, newest first. The
/// show is matched by ASIN or unique title substring across the selected
/// marketplaces. Also used by the deprecated `podcasts episodes`.
///
/// `missing` (AUD-205) narrows the listing to the episodes lacking a download
/// of those kinds, naming what each one lacks — the drill-down of the show
/// roll-up that `library list --missing` displays.
pub(crate) async fn episodes(
    ctx: &Ctx,
    show: String,
    missing: Option<Vec<String>>,
    limit: u32,
    page: u32,
) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    // Resolve the parent show (ASIN or unique title substring) across
    // the selected marketplaces; the episodes live in the marketplace
    // that owns the parent.
    let candidates = db.podcasts(marketplaces).await?;
    let needle = show.to_lowercase();
    let matched: Vec<&crate::db::PodcastRow> = candidates
        .iter()
        .filter(|p| p.asin == show || p.full_title.to_lowercase().contains(&needle))
        .collect();
    let show_row = match matched.as_slice() {
        [] => bail!("no followed podcast matches {show:?}"),
        [unique] => *unique,
        several => bail!(
            "{show:?} is ambiguous: {}",
            several
                .iter()
                .map(|p| format!("{} ({}, {})", p.full_title, p.asin, p.marketplace))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let (marketplace, parent) = (show_row.marketplace.clone(), show_row.asin.clone());
    // The stored/announced counts previously shown by `podcasts list` —
    // kept as a header line here (stderr, so `-o json` stays clean).
    eprintln!(
        "{} — {} stored / {} announced episode(s)",
        show_row.full_title,
        show_row.stored_episodes,
        show_row.announced_episodes.as_deref().unwrap_or("?"),
    );

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    if let Some(kinds) = missing {
        // Expand `all` and normalize to the canonical kind order, deduped —
        // the same rule as `library list --missing`.
        let kinds = crate::db::normalize_download_kinds(&kinds);
        let rows = if limit == 0 && page > 1 {
            Vec::new() // page 1 holds everything; no need to query
        } else {
            db.episodes_missing_downloads(
                marketplace.clone(),
                parent.clone(),
                kinds.clone(),
                query_limit,
                offset,
            )
            .await?
        };
        if rows.is_empty() {
            if page > 1 {
                let total = db
                    .count_episodes_missing_downloads(marketplace, parent, kinds)
                    .await?;
                return Err(crate::commands::empty_page_error(page, limit, total));
            }
            eprintln!("no episodes lacking {} downloads", kinds.join("/"));
        }
        // Printed even when empty: `-o table` renders nothing, `-o json`
        // still yields `[]` for consumers.
        ctx.print(&Output::table(
            vec!["asin", "title", "release_date", "missing"],
            rows.into_iter()
                .map(|episode| {
                    vec![
                        episode.asin,
                        episode.full_title,
                        episode.release_date.unwrap_or_default(),
                        episode.missing,
                    ]
                })
                .collect(),
        ));
        return Ok(());
    }
    let episodes = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.episodes(
            marketplace.clone(),
            Some(parent.clone()),
            query_limit,
            offset,
        )
        .await?
    };
    if episodes.is_empty() && page > 1 {
        let total = db.count_episodes(marketplace, Some(parent)).await?;
        return Err(crate::commands::empty_page_error(page, limit, total));
    }
    let rows = episodes
        .into_iter()
        .map(|episode| {
            vec![
                episode.asin,
                episode.full_title,
                episode.release_date.unwrap_or_default(),
                episode.runtime_min.unwrap_or_default(),
            ]
        })
        .collect();
    ctx.print(&Output::table(
        vec!["asin", "title", "release_date", "runtime_min"],
        rows,
    ));
    Ok(())
}
