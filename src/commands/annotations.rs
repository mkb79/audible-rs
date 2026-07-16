//! `audible annotations` — sync / list / show item annotations (last
//! position, bookmarks, notes, clips). Annotations are mutable user data,
//! kept in the database (always fetched fresh, no change detection). A
//! `--save` additionally writes the response as a `.annot` file in the
//! download directory; the database stays the source of truth.

use anyhow::{Result, bail};
use clap::{Arg, ArgAction};
use futures::StreamExt as _;
use serde_json::Value;

use crate::config::ctx::Ctx;
use crate::db::Db;
use crate::downloader::request_annotations;
use crate::output::Output;

use super::library::maybe_auto_sync;
use crate::naming::{base_filename, download_dir};

/// How many annotation fetches run concurrently.
const SYNC_CONCURRENCY: usize = 10;

/// `audible annotations`.
pub struct AnnotationsCommand;

#[async_trait::async_trait]
impl super::Command for AnnotationsCommand {
    fn name(&self) -> &'static str {
        "annotations"
    }

    fn clap(&self) -> clap::Command {
        let save = || {
            Arg::new("save")
                .long("save")
                .action(ArgAction::SetTrue)
                .help("Also write a .annot file to the download directory")
        };
        clap::Command::new(self.name())
            .about("Item annotations: last position, bookmarks, notes, clips")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                crate::commands::items::item_source_args(
                    clap::Command::new("sync")
                        .about("Fetch annotations into the database (always fresh)"),
                )
                .arg(
                    Arg::new("missing")
                        .long("missing")
                        .action(ArgAction::SetTrue)
                        .conflicts_with_all(["asin", "title", "all"])
                        .help("Only items never synced"),
                )
                .arg(
                    Arg::new("all")
                        .long("all")
                        .action(ArgAction::SetTrue)
                        .conflicts_with_all(["asin", "title", "missing"])
                        .help("Every owned item"),
                )
                .arg(save()),
            )
            .subcommand(
                clap::Command::new("list")
                    .about("Inventory: which items have annotations / when last synced")
                    .arg(
                        Arg::new("hide_none")
                            .long("hide-none")
                            .action(ArgAction::SetTrue)
                            .help("Hide items with no annotations (status none)"),
                    ),
            )
            .subcommand(
                clap::Command::new("show")
                    .about("Show an item's annotations")
                    .arg(
                        Arg::new("asin")
                            .value_name("ASIN")
                            .required(true)
                            .help("Item ASIN"),
                    )
                    .arg(
                        Arg::new("refresh")
                            .long("refresh")
                            .alias("fresh")
                            .action(ArgAction::SetTrue)
                            .help("Fetch fresh before showing (updates the database)"),
                    )
                    .arg(save()),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some(("sync", sub)) => sync(ctx, sub).await,
            Some(("list", sub)) => list(ctx, sub).await,
            Some(("show", sub)) => show(ctx, sub).await,
            _ => unreachable!("subcommand required"),
        }
    }
}

async fn sync(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let client = ctx.client().await?;
    let marketplaces = ctx.marketplaces()?;
    let save = matches.get_flag("save");

    let many = |key| -> Vec<String> {
        matches
            .get_many::<String>(key)
            .map(|values| values.cloned().collect())
            .unwrap_or_default()
    };
    let asins = many("asin");
    let titles = many("title");
    let all = matches.get_flag("all");
    let missing = matches.get_flag("missing");
    if asins.is_empty() && titles.is_empty() && !all && !missing {
        bail!("select items with --asin/--title, or --all / --missing");
    }

    let (mut ok, mut none, mut failed) = (0usize, 0usize, 0usize);
    for marketplace in &marketplaces {
        let targets = if all {
            db.annotation_target_asins(marketplace.clone(), false)
                .await?
        } else if missing {
            db.annotation_target_asins(marketplace.clone(), true)
                .await?
        } else {
            crate::commands::items::resolve_asins(
                &db,
                marketplace,
                asins.clone(),
                titles.clone(),
                crate::commands::items::PodcastMode::Episodes,
            )
            .await?
        };

        let db_ref = &db;
        let outcomes: Vec<&'static str> =
            futures::stream::iter(targets)
                .map(|asin| async move {
                    fetch_one(ctx, db_ref, client, marketplace, &asin, save).await
                })
                .buffer_unordered(SYNC_CONCURRENCY)
                .collect()
                .await;
        for outcome in outcomes {
            match outcome {
                "ok" => ok += 1,
                "none" => none += 1,
                _ => failed += 1,
            }
        }
    }
    eprintln!("annotations: {ok} synced · {none} none · {failed} failed");
    Ok(())
}

/// Fetches one item's annotations, stores them (incl. the `none` outcome) and
/// optionally writes the `.annot` file. Never fails the batch: errors are
/// warned and reported as `failed`.
async fn fetch_one(
    ctx: &Ctx,
    db: &Db,
    client: &crate::api::client::Client,
    marketplace: &str,
    asin: &str,
    save: bool,
) -> &'static str {
    match request_annotations(client, asin, None, None, None).await {
        Ok(Some(doc)) => {
            if let Err(error) = db
                .upsert_annotation(
                    marketplace.to_owned(),
                    asin.to_owned(),
                    Some(doc.to_string()),
                    "ok".to_owned(),
                )
                .await
            {
                tracing::warn!(%error, asin, "could not store annotations");
                return "failed";
            }
            if save {
                match save_annot(ctx, marketplace, asin, &doc).await {
                    Ok(dest) => {
                        if let Err(error) = db
                            .set_annotation_path(
                                marketplace.to_owned(),
                                asin.to_owned(),
                                dest.display().to_string(),
                            )
                            .await
                        {
                            tracing::warn!(%error, asin, "could not record .annot path");
                        }
                    }
                    Err(error) => tracing::warn!(%error, asin, "could not write .annot file"),
                }
            }
            "ok"
        }
        Ok(None) => {
            match db
                .upsert_annotation(
                    marketplace.to_owned(),
                    asin.to_owned(),
                    None,
                    "none".to_owned(),
                )
                .await
            {
                Ok(()) => "none",
                Err(error) => {
                    tracing::warn!(%error, asin, "could not store annotation status");
                    "failed"
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, asin, "annotation fetch failed");
            "failed"
        }
    }
}

/// Writes an annotation document as `{base}.annot` in the download directory,
/// returning the path written (recorded in the DB so `download reorganize` can
/// relocate it).
async fn save_annot(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    doc: &Value,
) -> Result<std::path::PathBuf> {
    let base = base_filename(ctx, marketplace, asin).await?;
    let dir = download_dir(ctx)?;
    let dest = dir.join(format!("{base}.annot"));
    // `base` may nest folders (custom filename mode); create the parent.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, serde_json::to_vec_pretty(doc)?)?;
    Ok(dest)
}

async fn list(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;
    let hide_none = matches.get_flag("hide_none");

    let rows: Vec<Vec<String>> = db
        .annotation_inventory(marketplaces)
        .await?
        .into_iter()
        .filter(|item| !(hide_none && item.status.as_deref() == Some("none")))
        .map(|item| {
            let state = match item.status.as_deref() {
                Some("ok") => "synced",
                Some("none") => "none",
                _ => "never",
            };
            vec![
                item.marketplace,
                item.asin,
                item.full_title,
                state.to_owned(),
                item.fetched_utc.unwrap_or_default(),
            ]
        })
        .collect();
    if rows.is_empty() {
        eprintln!("no items in the library — run `audible library sync` first");
    }
    ctx.print(&Output::table(
        vec!["mp", "asin", "title", "annotations", "last_synced"],
        rows,
    ));
    Ok(())
}

async fn show(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let asin = matches.get_one::<String>("asin").expect("required").clone();
    let db = ctx.open_library_db().await?;
    let marketplaces = ctx.marketplaces()?;

    if matches.get_flag("refresh") {
        let client = ctx.client().await?;
        let save = matches.get_flag("save");
        let mut refreshed = false;
        for marketplace in &marketplaces {
            if db
                .find_title(asin.clone(), marketplace.clone())
                .await?
                .is_some()
            {
                if fetch_one(ctx, &db, client, marketplace, &asin, save).await == "failed" {
                    bail!("could not refresh annotations for {asin}");
                }
                refreshed = true;
                break;
            }
        }
        if !refreshed {
            bail!("{asin} is not in the library for the selected marketplace(s)");
        }
    }

    let Some((_, annotations)) = db.annotation_doc(asin.clone(), marketplaces).await? else {
        bail!(
            "no annotation record for {asin} — run `audible annotations sync --asin {asin}` first"
        );
    };
    if annotations.status != "ok" {
        eprintln!(
            "no annotations for {asin} (last synced {})",
            annotations.fetched_utc
        );
        return Ok(());
    }
    let doc: Value = serde_json::from_str(annotations.doc.as_deref().unwrap_or("{}"))?;
    let rows: Vec<Vec<String>> = doc
        .get("payload")
        .and_then(|p| p.get("records"))
        .and_then(Value::as_array)
        .map(|records| records.iter().map(record_row).collect())
        .unwrap_or_default();
    if rows.is_empty() {
        eprintln!("no annotation records for {asin}");
    }
    ctx.print(&Output::table(
        vec!["type", "position", "created", "text"],
        rows,
    ));
    Ok(())
}

/// One annotation record → a display row (`type`, `position`, `created`, `text`).
fn record_row(record: &Value) -> Vec<String> {
    let field = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let kind = field("type");
    let kind = kind.strip_prefix("audible.").unwrap_or(&kind).to_owned();
    let start = field("startPosition");
    let position = match record.get("endPosition").and_then(Value::as_str) {
        Some(end) if !end.is_empty() => format!("{start}–{end}"),
        _ => start,
    };
    let text = record
        .get("text")
        .or_else(|| record.get("note"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    vec![kind, position, field("creationTime"), text]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_row_maps_fields() {
        let last_heard = serde_json::json!({
            "type": "audible.last_heard",
            "startPosition": "123",
            "creationTime": "2026-06-19 07:25:41.0",
        });
        assert_eq!(
            record_row(&last_heard),
            vec!["last_heard", "123", "2026-06-19 07:25:41.0", ""]
        );

        // Clips span a range; notes carry text; the `audible.` prefix is stripped.
        let clip = serde_json::json!({
            "type": "audible.clip", "startPosition": "10", "endPosition": "20", "text": "hi",
        });
        assert_eq!(record_row(&clip), vec!["clip", "10–20", "", "hi"]);
    }
}
