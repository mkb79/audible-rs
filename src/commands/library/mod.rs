//! `audible library` — DB-backed library commands (archived architecture §8).
//! The database is the engine: `sync` fills it via the paginator, the
//! read commands answer from SQLite.

use anyhow::Result;
use clap::{Arg, ArgAction};

use crate::config::ctx::Ctx;

/// How many marketplaces sync concurrently (each paginates sequentially via
/// its own continuation/state token).
const SYNC_MARKETPLACE_CONCURRENCY: usize = 4;

/// `audible library`.
pub struct LibraryCommand;

#[async_trait::async_trait]
impl super::Command for LibraryCommand {
    fn name(&self) -> &'static str {
        "library"
    }

    fn clap(&self) -> clap::Command {
        let limit = || {
            Arg::new("limit")
                .long("limit")
                .value_name("N")
                .default_value("50")
                .value_parser(clap::value_parser!(u32))
                .help("Maximum number of rows (0 = all)")
        };
        clap::Command::new(self.name())
            .about("Work with your library (backed by a local database)")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("sync")
                    .about("Sync the library database (delta when possible)")
                    .arg(
                        Arg::new("full")
                            .long("full")
                            .action(ArgAction::SetTrue)
                            .help("Force a full resync instead of a delta"),
                    )
                    .arg(
                        Arg::new("no_podcasts")
                            .long("no-podcasts")
                            .action(ArgAction::SetTrue)
                            .help("Skip resolving podcast episodes"),
                    )
                    .arg(
                        Arg::new("show_volatile")
                            .long("show-volatile")
                            .short('V')
                            .action(ArgAction::SetTrue)
                            .help(
                                "Also list volatile-only changes (rating/listening \
                                 progress; always recorded, hidden by default)",
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("list")
                    .about("List your library (from the local database)")
                    .arg(
                        Arg::new("remote")
                            .long("remote")
                            .action(ArgAction::SetTrue)
                            .help("Bypass the database and list straight from the API"),
                    )
                    .arg(
                        Arg::new("missing")
                            .long("missing")
                            .value_name("KINDS")
                            .num_args(0..)
                            .require_equals(true)
                            .value_delimiter(',')
                            .default_missing_value("audio")
                            .value_parser(["audio", "chapter", "pdf", "cover", "all"])
                            .conflicts_with("remote")
                            .help_heading("Selection")
                            .help(
                                "Only items lacking a download record of these kinds \
                                 (no value: audio; `all` covers every kind)",
                            ),
                    )
                    .arg(
                        Arg::new("borrowed")
                            .long("borrowed")
                            .action(ArgAction::SetTrue)
                            .conflicts_with("remote")
                            .conflicts_with("missing")
                            .help_heading("Selection")
                            .help(
                                "Only titles you don't own (access via a subscription \
                                 or grant); shows the plans you're eligible for vs the \
                                 other plans the title needs",
                            ),
                    )
                    .arg(
                        Arg::new("include_archived")
                            .long("include-archived")
                            .action(ArgAction::SetTrue)
                            .requires("missing")
                            .help_heading("Selection")
                            .help(
                                "Also list archived titles — --missing skips them by \
                                 default (archive state is as of the last library sync)",
                            ),
                    )
                    .arg(
                        super::kind_arg("book")
                            .conflicts_with("remote")
                            .help_heading("Selection"),
                    )
                    .arg(limit())
                    .arg(
                        Arg::new("page")
                            .long("page")
                            .value_name("N")
                            .default_value("1")
                            .value_parser(clap::value_parser!(u32).range(1..))
                            .help_heading("Pagination")
                            .help("Show the N-th page of --limit rows"),
                    )
                    .mut_arg("limit", |arg| arg.help_heading("Pagination")),
            )
            .subcommand(
                clap::Command::new("search")
                    .about("Search your library (FTS5 by default)")
                    .arg(Arg::new("query").required(true).value_name("QUERY").help(
                        "Search terms — plain words are prefix-matched; FTS5 \
                                 syntax (quotes, *, OR/NOT) is respected (--like for \
                                 plain substring matching)",
                    ))
                    .arg(limit())
                    .arg(
                        Arg::new("like")
                            .long("like")
                            .action(ArgAction::SetTrue)
                            .help("Substring matching instead of FTS5 syntax"),
                    )
                    .arg(super::kind_arg("book")),
            )
            .subcommand(
                clap::Command::new("episodes")
                    .about("List the stored episodes of a followed podcast")
                    .arg(
                        Arg::new("show")
                            .required(true)
                            .value_name("SHOW")
                            .help("The followed podcast, by ASIN or title substring"),
                    )
                    .arg(
                        Arg::new("missing")
                            .long("missing")
                            .value_name("KINDS")
                            .num_args(0..)
                            .require_equals(true)
                            .value_delimiter(',')
                            .default_missing_value("audio")
                            .value_parser(["audio", "chapter", "pdf", "cover", "all"])
                            .help(
                                "Only episodes lacking a download record of these kinds \
                                 (no value: audio; `all` covers every kind)",
                            ),
                    )
                    .arg(limit())
                    .arg(
                        Arg::new("page")
                            .long("page")
                            .value_name("N")
                            .default_value("1")
                            .value_parser(clap::value_parser!(u32).range(1..))
                            .help("Show the N-th page of --limit rows"),
                    ),
            )
            .subcommand(
                clap::Command::new("export")
                    .about("Export the library")
                    .arg(
                        Arg::new("format")
                            .long("format")
                            .value_name("FORMAT")
                            .default_value("json")
                            .value_parser(["json", "csv"])
                            .help(
                                "json: full item documents in a versioned envelope; \
                                 csv: flat book columns for spreadsheets",
                            ),
                    )
                    .arg(super::kind_arg("book")),
            )
            .subcommand(
                clap::Command::new("changes")
                    .about("Review recorded library changes (added/changed/removed)")
                    .arg(
                        Arg::new("asin")
                            .long("asin")
                            .value_name("ASIN")
                            .help("Only this item"),
                    )
                    .arg(
                        Arg::new("since")
                            .long("since")
                            .value_name("DATE")
                            .help("Only changes on/after this date (e.g. 2026-06-26)"),
                    )
                    .arg(
                        Arg::new("mode")
                            .long("mode")
                            .value_name("MODE")
                            .value_parser(["full", "delta"])
                            .help("Only this sync mode"),
                    )
                    .arg(
                        Arg::new("change")
                            .long("change")
                            .value_name("CHANGE")
                            .value_parser(["added", "changed", "removed"])
                            .help("Only this change"),
                    )
                    .arg(super::kind_arg("book"))
                    .arg(
                        Arg::new("limit")
                            .long("limit")
                            .value_name("N")
                            .default_value("50")
                            .value_parser(clap::value_parser!(u32))
                            .help("Max rows, most recent first (0 = all)"),
                    )
                    .arg(
                        Arg::new("values")
                            .long("values")
                            .action(ArgAction::SetTrue)
                            .help("Show old→new values for changed fields, not just the keys"),
                    )
                    .arg(
                        Arg::new("show_volatile")
                            .long("show-volatile")
                            .short('V')
                            .action(ArgAction::SetTrue)
                            .help(
                                "Also show volatile-only changes (rating/listening \
                                 progress; recorded but hidden by default)",
                            ),
                    )
                    .subcommand(
                        clap::Command::new("prune")
                            .about("Delete change-log entries older than the retention")
                            .arg(
                                Arg::new("older-than")
                                    .long("older-than")
                                    .value_name("DAYS")
                                    .value_parser(clap::value_parser!(u32))
                                    .help("Override the configured retention (days)"),
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("add")
                    .about("Add subscription (AYCL/Plus) titles, podcasts or episodes to your library")
                    .arg(
                        super::items::asin_arg()
                            .help("ASIN(s) to add — comma-separated or repeated"),
                    )
                    .arg(
                        Arg::new("title")
                            .long("title")
                            .action(ArgAction::Append)
                            .value_name("QUERY")
                            .help(
                                "Title to add, searched in the Audible catalog (repeatable; \
                                 several matches open a selection list)",
                            ),
                    )
                    .group(
                        clap::ArgGroup::new("source")
                            .args(["asin", "title"])
                            .multiple(true)
                            .required(true),
                    )
                    .arg(super::kind_arg("all").help(
                        "Only add these content kinds — a guard against \
                         accidentally adding the wrong match (CSV; default all)",
                    ))
                    .arg(
                        Arg::new("sync")
                            .long("sync")
                            .action(ArgAction::SetTrue)
                            .help(
                                "Run a delta library sync right after, so the new item \
                                 shows up in the local library immediately",
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("remove")
                    .about("Remove titles, podcasts or episodes from your library (returns loans, unfollows podcasts)")
                    .arg(
                        super::items::asin_arg()
                            .help("ASIN(s) to remove — comma-separated or repeated"),
                    )
                    .arg(
                        Arg::new("title")
                            .long("title")
                            .action(ArgAction::Append)
                            .value_name("QUERY")
                            .help("Library title to remove (substring search; repeatable)"),
                    )
                    .group(
                        clap::ArgGroup::new("source")
                            .args(["asin", "title"])
                            .multiple(true)
                            .required(true),
                    )
                    .arg(super::kind_arg("all").help(
                        "Only remove these content kinds — a guard against \
                         accidentally removing the wrong match (CSV; default all)",
                    ))
                    .arg(super::yes_arg()),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        use crate::commands::strings;
        let limit = |m: &clap::ArgMatches| match *m.get_one::<u32>("limit").expect("default") {
            0 => u32::MAX,
            n => n,
        };
        let raw_limit = |m: &clap::ArgMatches| *m.get_one::<u32>("limit").expect("default");
        let page = |m: &clap::ArgMatches| *m.get_one::<u32>("page").expect("default");
        match matches.subcommand() {
            Some(("sync", sub)) => {
                sync(
                    ctx,
                    sub.get_flag("full"),
                    sub.get_flag("no_podcasts"),
                    sub.get_flag("show_volatile"),
                    true,
                )
                .await
            }
            Some(("list", sub)) if sub.get_flag("remote") => list_remote(ctx, limit(sub)).await,
            Some(("list", sub)) if sub.get_flag("borrowed") => {
                list_borrowed(ctx, super::kind_filter(sub), raw_limit(sub), page(sub)).await
            }
            Some(("list", sub)) if sub.contains_id("missing") => {
                list_missing(
                    ctx,
                    sub.get_many::<String>("missing")
                        .expect("contains_id")
                        .cloned()
                        .collect(),
                    super::kind_filter(sub),
                    raw_limit(sub),
                    page(sub),
                    sub.get_flag("include_archived"),
                )
                .await
            }
            Some(("list", sub)) => {
                list(ctx, super::kind_filter(sub), raw_limit(sub), page(sub)).await
            }
            Some(("search", sub)) => {
                let query = sub.get_one::<String>("query").expect("required").clone();
                search(
                    ctx,
                    super::kind_filter(sub),
                    query,
                    limit(sub),
                    !sub.get_flag("like"),
                )
                .await
            }
            Some(("episodes", sub)) => {
                let show = sub.get_one::<String>("show").expect("required").clone();
                // `--missing` present (with or without a value) switches the
                // listing to the download drill-down.
                let missing = sub.contains_id("missing").then(|| strings(sub, "missing"));
                episodes::episodes(ctx, show, missing, raw_limit(sub), page(sub)).await
            }
            Some(("export", sub)) => {
                let csv = sub.get_one::<String>("format").expect("default") == "csv";
                export(ctx, super::kind_filter(sub), csv).await
            }
            Some(("changes", sub)) => match sub.subcommand() {
                Some(("prune", prune)) => {
                    changes_prune(ctx, prune.get_one::<u32>("older-than").copied()).await
                }
                _ => changes(ctx, sub).await,
            },
            Some(("add", sub)) => {
                membership::add(
                    ctx,
                    strings(sub, "asin"),
                    strings(sub, "title"),
                    super::kind_filter(sub),
                    sub.get_flag("sync"),
                )
                .await
            }
            Some(("remove", sub)) => {
                membership::remove(
                    ctx,
                    strings(sub, "asin"),
                    strings(sub, "title"),
                    super::kind_filter(sub),
                    sub.get_flag("yes"),
                )
                .await
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

mod changes;
pub(crate) mod episodes;
mod list;
mod membership;
mod sync;

pub(crate) use sync::{poll_until_reflected, sync};

use changes::{changes, changes_prune};
use list::{export, list, list_borrowed, list_missing, list_remote, search};

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared `--asin` contract (AUD-220/C3): `--asin A,B` splits into
    /// two ASINs in `library add`/`remove`, exactly like everywhere else —
    /// it used to reach the API as one literal ASIN "A,B" here.
    #[test]
    fn add_and_remove_split_comma_separated_asins() {
        use crate::commands::Command as _;
        for verb in ["add", "remove"] {
            let matches = LibraryCommand
                .clap()
                .try_get_matches_from(["library", verb, "--asin", "B0A,B0B"])
                .unwrap();
            let (_, sub) = matches.subcommand().unwrap();
            let asins: Vec<&String> = sub.get_many::<String>("asin").unwrap().collect();
            assert_eq!(asins, ["B0A", "B0B"], "{verb} must split comma ASINs");
        }
    }
}
