//! `library changes` — render and prune the recorded change log
//! (added/changed/removed, volatile handling).

use anyhow::Result;

use crate::config::ctx::Ctx;
use crate::db::{self};

/// `library changes`: review the recorded change log (AUD-64).
pub(super) async fn changes(ctx: &Ctx, args: &clap::ArgMatches) -> Result<()> {
    let db = ctx.open_library_db().await?;
    let filter = db::ChangeFilter {
        marketplaces: ctx.marketplaces()?,
        asin: args.get_one::<String>("asin").cloned(),
        since: args
            .get_one::<String>("since")
            .map(|raw| normalize_since(raw))
            .transpose()?,
        mode: args.get_one::<String>("mode").cloned(),
        change: args.get_one::<String>("change").cloned(),
        item_kinds: crate::commands::kind_filter(args),
        show_volatile: args.get_flag("show_volatile"),
        limit: *args.get_one::<u32>("limit").expect("default"),
    };
    let values = args.get_flag("values");
    let show_volatile = filter.show_volatile;
    let records = db.list_changes(filter).await?;

    // JSON is the lossless view: emit the full, parsed field diff (the table's
    // `fields` column is compacted/truncated for the terminal, which would be
    // lossy here). Always valid JSON, including `[]` for no changes.
    if ctx.output_format() == crate::output::OutputFormat::Json {
        let array: Vec<serde_json::Value> = records
            .iter()
            .map(|record| {
                let fields = record
                    .changed
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "recorded": record.recorded_utc,
                    "mp": record.marketplace,
                    "change": record.change,
                    "kind": record.item_kind,
                    "asin": record.asin,
                    "title": record.full_title,
                    "fields": fields,
                })
            })
            .collect();
        ctx.print(&crate::output::Output::Json(serde_json::Value::Array(
            array,
        )));
        return Ok(());
    }

    if records.is_empty() {
        eprintln!(
            "no recorded changes — recording starts after the initial sync \
             (and only if `record_changes` is on){}",
            if show_volatile {
                ""
            } else {
                "; volatile-only changes (rating/progress) are hidden, pass --show-volatile"
            }
        );
        return Ok(());
    }
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            vec![
                record.recorded_utc.clone(),
                record.marketplace.clone(),
                record.change.clone(),
                record.item_kind.clone(),
                record.asin.clone(),
                record.full_title.clone(),
                format_change_fields(record.changed.as_deref(), values),
            ]
        })
        .collect();
    ctx.print(&crate::output::Output::table(
        vec![
            "recorded", "mp", "change", "kind", "asin", "title", "fields",
        ],
        rows,
    ));
    Ok(())
}

/// `library changes prune`: drop entries older than the retention (or
/// `--older-than`), keeping the change log bounded on demand.
pub(super) async fn changes_prune(ctx: &Ctx, older_than: Option<u32>) -> Result<()> {
    let days = match older_than {
        Some(days) => days,
        None => ctx.db_config()?.change_retention_days,
    };
    if days == 0 {
        eprintln!(
            "retention is 0 (keep forever) — nothing pruned; pass --older-than <days> to prune anyway"
        );
        return Ok(());
    }
    let db = ctx.open_library_db().await?;
    let pruned = db.prune_change_log(days).await?;
    eprintln!(
        "pruned {pruned} change-log entr{} older than {days} days",
        if pruned == 1 { "y" } else { "ies" }
    );
    Ok(())
}

/// Validates and normalizes `--since`: the value is compared **lexically**
/// against the stored `YYYY-MM-DDTHH:MM:SSZ` timestamps, so free text
/// silently filtered out everything (audit 2026-07-18, A7 — `2026-6-26`
/// sorts before every zero-padded timestamp, exit 0, plus the misleading
/// "recording starts after the initial sync" hint). Accepted: a full ISO
/// timestamp (verbatim) or a date, normalized to its midnight timestamp.
fn normalize_since(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if crate::timefmt::parse_iso(raw).is_some() {
        return Ok(raw.to_owned());
    }
    let date = time::macros::format_description!("[year]-[month]-[day]");
    if time::Date::parse(raw, &date).is_ok() {
        return Ok(format!("{raw}T00:00:00Z"));
    }
    anyhow::bail!(
        "--since {raw:?} is not a recognized time — use YYYY-MM-DD or \
         YYYY-MM-DDTHH:MM:SSZ"
    )
}

/// Formats the `changed` JSON (`[{key,old,new}]`) for the `fields` column: the
/// keys alone, or `key: old → new` (values compacted) when `values` is set.
fn format_change_fields(changed: Option<&str>, values: bool) -> String {
    let Some(raw) = changed else {
        return "—".to_owned();
    };
    let Ok(diff) = serde_json::from_str::<Vec<serde_json::Value>>(raw) else {
        return "—".to_owned();
    };
    let parts: Vec<String> = diff
        .iter()
        .map(|entry| {
            let key = entry
                .get("key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            if values {
                format!(
                    "{key}: {} → {}",
                    compact_value(entry.get("old")),
                    compact_value(entry.get("new"))
                )
            } else {
                key.to_owned()
            }
        })
        .collect();
    parts.join(if values { "; " } else { ", " })
}

/// Compacts a JSON value to a short single-line string for the diff display.
fn compact_value(value: Option<&serde_json::Value>) -> String {
    let text = match value {
        Some(serde_json::Value::String(string)) => string.clone(),
        Some(other) => other.to_string(),
        None => "null".to_owned(),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 60 {
        format!("{}…", text.chars().take(57).collect::<String>())
    } else {
        text
    }
}

/// Prints the added/changed/removed items of a sync to stderr (the structured
/// summary stays on stdout, so `-o json` is unaffected). Marketplaces with no
/// changes are skipped; the marketplace is named only in multi-marketplace runs.
pub(super) fn print_changes(
    sections: &[(String, db::ApplyOutcome)],
    multi: bool,
    show_volatile: bool,
) {
    for (marketplace, changes) in sections {
        let volatile = show_volatile && !changes.changed_volatile.is_empty();
        if changes.added.is_empty()
            && changes.changed.is_empty()
            && changes.removed.is_empty()
            && !volatile
        {
            continue;
        }
        if multi {
            eprintln!("changes ({marketplace}):");
        } else {
            eprintln!("changes:");
        }
        let mut groups: Vec<(&str, &Vec<db::ChangedItem>)> =
            vec![("+ added", &changes.added), ("~ changed", &changes.changed)];
        if show_volatile {
            groups.push(("~ changed (volatile)", &changes.changed_volatile));
        }
        groups.push(("- removed", &changes.removed));
        for (label, items) in groups {
            if items.is_empty() {
                continue;
            }
            eprintln!("  {label} ({})", items.len());
            for item in items {
                eprintln!("      {}  {}", item.asin, item.full_title);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A7 (audit 2026-07-18): only values that compare correctly against
    /// the stored zero-padded timestamps may pass — free text used to
    /// lexically filter out every change and exit 0.
    #[test]
    fn since_is_validated_and_normalized() {
        assert_eq!(
            normalize_since("2026-06-26").unwrap(),
            "2026-06-26T00:00:00Z"
        );
        assert_eq!(
            normalize_since(" 2026-06-26T12:00:00Z ").unwrap(),
            "2026-06-26T12:00:00Z"
        );
        for invalid in ["2026-6-26", "26.06.2026", "yesterday", ""] {
            assert!(normalize_since(invalid).is_err(), "{invalid:?}");
        }
    }
}
