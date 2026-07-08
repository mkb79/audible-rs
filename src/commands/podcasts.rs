//! `audible podcasts` — podcast subscriptions and their episodes,
//! answered from the local database (episodes are stored by
//! `library sync`).

use anyhow::{Result, bail};
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::output::Output;

use super::library::maybe_auto_sync;

/// `audible podcasts`.
pub struct PodcastsCommand;

#[async_trait::async_trait]
impl super::Command for PodcastsCommand {
    fn name(&self) -> &'static str {
        "podcasts"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Your podcast subscriptions (from the local database)")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(clap::Command::new("list").about("List subscribed podcasts"))
            .subcommand(
                clap::Command::new("episodes")
                    .about("List episodes of a podcast")
                    .arg(
                        Arg::new("podcast")
                            .required(true)
                            .value_name("ASIN|TITLE")
                            .help("Parent podcast, by ASIN or title substring"),
                    )
                    .arg(
                        Arg::new("limit")
                            .long("limit")
                            .value_name("N")
                            .default_value("50")
                            .value_parser(clap::value_parser!(u32))
                            .help("Maximum number of rows (0 = all)"),
                    )
                    .arg(
                        Arg::new("page")
                            .long("page")
                            .value_name("N")
                            .default_value("1")
                            .value_parser(clap::value_parser!(u32).range(1..))
                            .help("Show the N-th page of --limit rows"),
                    ),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some(("list", _)) => list(ctx).await,
            Some(("episodes", sub)) => {
                let podcast = sub.get_one::<String>("podcast").expect("required").clone();
                let limit = *sub.get_one::<u32>("limit").expect("default");
                let page = *sub.get_one::<u32>("page").expect("default");
                episodes(ctx, podcast, limit, page).await
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
        .podcasts(marketplaces)
        .await?
        .into_iter()
        .map(|podcast| {
            vec![
                podcast.marketplace,
                podcast.asin,
                podcast.full_title,
                podcast.stored_episodes.to_string(),
                podcast.announced_episodes.unwrap_or_default(),
            ]
        })
        .collect();
    if rows.is_empty() {
        eprintln!("no podcast subscriptions in the library — run `audible library sync` first");
    }
    ctx.print(&Output::table(
        vec!["mp", "asin", "title", "episodes", "announced"],
        rows,
    ));
    Ok(())
}

async fn episodes(ctx: &Ctx, podcast: String, limit: u32, page: u32) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    // Resolve the parent podcast (ASIN or unique title substring) across
    // the selected marketplaces; the episodes live in the marketplace
    // that owns the parent.
    let candidates = db.podcasts(marketplaces).await?;
    let needle = podcast.to_lowercase();
    let matched: Vec<&crate::db::PodcastRow> = candidates
        .iter()
        .filter(|p| p.asin == podcast || p.full_title.to_lowercase().contains(&needle))
        .collect();
    let (marketplace, parent) = match matched.as_slice() {
        [] => bail!("no subscribed podcast matches {podcast:?}"),
        [unique] => (unique.marketplace.clone(), unique.asin.clone()),
        several => bail!(
            "{podcast:?} is ambiguous: {}",
            several
                .iter()
                .map(|p| format!("{} ({}, {})", p.full_title, p.asin, p.marketplace))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let (query_limit, offset) = super::page_window(limit, page);
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
        return Err(super::empty_page_error(page, limit, total));
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
