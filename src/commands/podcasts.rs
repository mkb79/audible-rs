//! `audible podcasts` — DEPRECATED (AUD-175), removed in v0.1.0. The
//! library is the one container: `podcasts list` is `library list --kind
//! podcast`, `podcasts episodes` moved to `library episodes <SHOW>`.
//! Both subcommands keep working until removal but print a deprecation
//! notice on every invocation.

use anyhow::Result;
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::output::Output;

use super::library::maybe_auto_sync;

/// `audible podcasts` (deprecated).
pub struct PodcastsCommand;

#[async_trait::async_trait]
impl super::Command for PodcastsCommand {
    fn name(&self) -> &'static str {
        "podcasts"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about(
                "[deprecated] Your podcast subscriptions — use `library list --kind podcast` \
                 and `library episodes` (removed in v0.1.0)",
            )
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("list").about(
                    "[deprecated] List subscribed podcasts — use `library list --kind podcast`",
                ),
            )
            .subcommand(
                clap::Command::new("episodes")
                    .about("[deprecated] List episodes of a podcast — use `library episodes`")
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
        eprintln!(
            "warning: `podcasts` is deprecated and will be removed in v0.1.0 — \
             use `library list --kind podcast` and `library episodes <SHOW>`"
        );
        match matches.subcommand() {
            Some(("list", _)) => list(ctx).await,
            Some(("episodes", sub)) => {
                let podcast = sub.get_one::<String>("podcast").expect("required").clone();
                let limit = *sub.get_one::<u32>("limit").expect("default");
                let page = *sub.get_one::<u32>("page").expect("default");
                // No `--missing` on the deprecated noun (AUD-175): use
                // `library episodes <SHOW> --missing`.
                super::library::episodes::episodes(ctx, podcast, None, limit, page).await
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

/// The legacy podcast listing with stored/announced episode counts. Its
/// replacement is `library list --kind podcast`; the counts live on in
/// the `library episodes <SHOW>` header.
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
