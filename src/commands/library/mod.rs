//! `audible library` — DB-backed library commands (archived architecture §8).
//! The database is the engine: `sync` fills it via the paginator, the
//! read commands answer from SQLite.

use anyhow::Result;
use clap::{Arg, ArgAction};

use crate::config::ctx::Ctx;

/// Response groups every sync request uses; pinned per database
/// (reference branch default).
pub const DEFAULT_RESPONSE_GROUPS: &str = "badge_types,is_archived,is_finished,is_playable,is_removable,is_visible,\
     order_details,origin_asin,percent_complete,shared,ws4v_rights,badges,\
     category_ladders,category_media,category_metadata,contributors,customer_rights,\
     media,product_attrs,product_desc,product_details,product_extended_attrs,\
     product_plans,product_plan_details,profile_sharing,rating,relationships_v2,\
     sample,sku,pdf_url,series";

const DEFAULT_IMAGE_SIZES: &str = "900,1215,252,558,408,500";

/// Hard cap on concurrent resolution fetches (episode + catalog) across all
/// marketplaces, shared via one semaphore — multiplexed over HTTP/2.
const SYNC_RESOLUTION_CONCURRENCY: usize = 10;
/// How many marketplaces sync concurrently (each paginates sequentially via
/// its own continuation/state token).
const SYNC_MARKETPLACE_CONCURRENCY: usize = 4;

/// Response groups valid on `/1.0/catalog/products` — taken verbatim
/// from captured iOS-app traffic (minus the app-specific
/// `feature_support`). Library-only groups are rejected there, and
/// without any groups the catalog returns documents without titles.
pub(crate) const CATALOG_RESPONSE_GROUPS: &str = "badges,category_ladders,contributors,customer_rights,media,product_attrs,\
     product_desc,product_details,product_extended_attrs,product_plans,\
     product_plan_details,profile_sharing,rating,relationships_v2,sample,sku,ws4v_rights";

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
                            .value_parser(["audio", "chapter", "pdf", "cover", "annotation", "all"])
                            .conflicts_with("remote")
                            .help_heading("Selection")
                            .help(
                                "Only items lacking a download record of these kinds \
                                 (no value: audio; `all` covers every kind)",
                            ),
                    )
                    .arg(
                        Arg::new("leaving")
                            .long("leaving")
                            .action(ArgAction::SetTrue)
                            .conflicts_with("remote")
                            .conflicts_with("missing")
                            .help_heading("Selection")
                            .help(
                                "Only subscription (Plus/AYCL) titles with a known \
                                 expiry, soonest first — the ones leaving your library; \
                                 owned titles are permanent and never shown",
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
                    ),
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
                        Arg::new("kind")
                            .long("kind")
                            .value_name("KIND")
                            .value_parser(["added", "changed", "removed"])
                            .help("Only this kind"),
                    )
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
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
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
                )
                .await
            }
            Some(("list", sub)) if sub.get_flag("remote") => list_remote(ctx, limit(sub)).await,
            Some(("list", sub)) if sub.get_flag("leaving") => {
                list_leaving(ctx, raw_limit(sub), page(sub)).await
            }
            Some(("list", sub)) if sub.contains_id("missing") => {
                list_missing(
                    ctx,
                    sub.get_many::<String>("missing")
                        .expect("contains_id")
                        .cloned()
                        .collect(),
                    raw_limit(sub),
                    page(sub),
                    sub.get_flag("include_archived"),
                )
                .await
            }
            Some(("list", sub)) => list(ctx, raw_limit(sub), page(sub)).await,
            Some(("search", sub)) => {
                let query = sub.get_one::<String>("query").expect("required").clone();
                search(ctx, query, limit(sub), !sub.get_flag("like")).await
            }
            Some(("export", sub)) => {
                let csv = sub.get_one::<String>("format").expect("default") == "csv";
                export(ctx, csv).await
            }
            Some(("changes", sub)) => match sub.subcommand() {
                Some(("prune", prune)) => {
                    changes_prune(ctx, prune.get_one::<u32>("older-than").copied()).await
                }
                _ => changes(ctx, sub).await,
            },
            _ => unreachable!("subcommand required"),
        }
    }
}

mod changes;
mod list;
mod sync;

pub use sync::{SyncOptions, SyncSummary, sync_library};
pub(crate) use sync::{maybe_auto_sync, sync};

use changes::{changes, changes_prune};
use list::{export, list, list_leaving, list_missing, list_remote, search};
