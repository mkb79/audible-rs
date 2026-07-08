//! Shared --asin/--title item input: an arg builder plus a resolver that
//! turns explicit ASINs and title searches into a deduped ASIN list.
//! Title search reuses the FTS5 engine (Db::search), so the same query
//! syntax works here and in `library search`.

use anyhow::{Result, bail};
use clap::{Arg, ArgAction};

/// Adds the shared `--asin`/`--title` inputs (both multi: comma-separated
/// and repeatable). Requiredness/grouping is the caller's choice.
pub(crate) fn item_source_args(cmd: clap::Command) -> clap::Command {
    cmd.arg(
        Arg::new("asin")
            .long("asin")
            .value_name("ASIN")
            .value_delimiter(',')
            .action(ArgAction::Append)
            .help("Item ASIN(s) — comma-separated or repeated"),
    )
    .arg(
        Arg::new("title")
            .long("title")
            .value_name("QUERY")
            .value_delimiter(',')
            .action(ArgAction::Append)
            .help(
                "Title search — comma-separated or repeated. Plain words are prefix-matched \
                 (\"jed\" finds Jedi) across title and subtitle; FTS5 syntax (quotes, *, OR/NOT) \
                 is respected. Append \"~N\" to cap matches (default 15).",
            ),
    )
}

const DEFAULT_TITLE_CAP: usize = 15;

struct TitleQuery {
    query: String,
    cap: usize,
}

fn parse_title_arg(raw: &str) -> Result<TitleQuery> {
    if let Some(tilde_pos) = raw.rfind('~') {
        let query = raw[..tilde_pos].trim().to_owned();
        let cap_str = raw[tilde_pos + 1..].trim();
        let cap: usize = cap_str
            .parse()
            .ok()
            .filter(|&n: &usize| n >= 1)
            .ok_or_else(|| {
                anyhow::anyhow!("invalid --title cap in {raw:?}: expected \"query~N\" with N >= 1")
            })?;
        Ok(TitleQuery { query, cap })
    } else {
        Ok(TitleQuery {
            query: raw.trim().to_owned(),
            cap: DEFAULT_TITLE_CAP,
        })
    }
}

/// Resolves explicit `--asin` values (passed through verbatim, trusted as
/// exact ids) and `--title` searches into a deduped, order-preserving
/// ASIN list. Per title: 0 hits → warning + skip; 1 → taken; many →
/// interactive multi-select on a TTY, otherwise an "ambiguous" error.
pub(crate) async fn resolve_asins(
    db: &crate::db::Db,
    marketplace: &str,
    asins: Vec<String>,
    titles: Vec<String>,
) -> Result<Vec<String>> {
    let mut result: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut push = |asin: String| {
        if seen.insert(asin.clone()) {
            result.push(asin);
        }
    };

    for asin in asins {
        let asin = asin.trim().to_owned();
        if !asin.is_empty() {
            push(asin);
        }
    }

    // Tracks whether an interactive picker was shown, so a single blank
    // line can separate the resolution phase from the command's output.
    let mut interacted = false;
    for raw in titles {
        let tq = parse_title_arg(&raw)?;
        if tq.query.is_empty() {
            eprintln!("ignoring empty --title");
            continue;
        }
        let rows = db
            .search(
                vec![marketplace.to_owned()],
                tq.query.clone(),
                tq.cap as u32,
                true,
            )
            .await?;
        match rows.len() {
            0 => {
                eprintln!("no title matches {:?}", tq.query);
            }
            1 => {
                push(rows[0].asin.clone());
            }
            _ => {
                if console::Term::stderr().is_term() {
                    let labels: Vec<String> = rows
                        .iter()
                        .map(|r| format!("{}  {}", r.asin, r.full_title))
                        .collect();
                    // `report(false)`: the default echoes the whole chosen
                    // list back as one long line — we clear the picker and
                    // print a concise confirmation instead.
                    let selection = dialoguer::MultiSelect::with_theme(
                        &dialoguer::theme::ColorfulTheme::default(),
                    )
                    .with_prompt(format!(
                        "Matches for {:?} — space toggles · a all · enter confirms",
                        tq.query
                    ))
                    .items(&labels)
                    .report(false)
                    .interact_on(&console::Term::stderr())?;
                    interacted = true;
                    if selection.is_empty() {
                        eprintln!("no titles selected for {:?}", tq.query);
                    } else {
                        eprintln!(
                            "selected {} of {} for {:?}",
                            selection.len(),
                            rows.len(),
                            tq.query
                        );
                        for i in selection {
                            push(rows[i].asin.clone());
                        }
                    }
                } else {
                    let listing: Vec<String> = rows
                        .iter()
                        .map(|r| format!("  {}  {}", r.asin, r.full_title))
                        .collect();
                    bail!(
                        "{} titles match {:?}; pass --asin or run interactively:\n{}",
                        rows.len(),
                        tq.query,
                        listing.join("\n"),
                    );
                }
            }
        }
    }

    // Separate the interactive picker block from the command output.
    if interacted {
        eprintln!();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, SyncLogEntry, UpsertItem, now_iso_utc};

    fn item(asin: &str, title: &str) -> UpsertItem {
        UpsertItem {
            asin: asin.into(),
            doc: serde_json::json!({
                "asin": asin,
                "title": title,
                "purchase_date": "2024-01-01",
                "runtime_length_min": 60,
                "language": "english",
            })
            .to_string(),
            title: title.into(),
            subtitle: None,
            full_title: title.into(),
            series: Vec::new(),
        }
    }

    fn default_log() -> SyncLogEntry {
        SyncLogEntry {
            request_time_utc: now_iso_utc(),
            response_time_utc: now_iso_utc(),
            http_status: Some(200),
            ..Default::default()
        }
    }

    const MP: &str = "de";

    async fn open_temp() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library_test.sqlite"), 5000)
            .await
            .unwrap();
        (dir, db)
    }

    #[test]
    fn parse_title_plain() {
        let tq = parse_title_arg("star wars").unwrap();
        assert_eq!(tq.query, "star wars");
        assert_eq!(tq.cap, 15);
    }

    #[test]
    fn parse_title_with_cap() {
        let tq = parse_title_arg("jedi~5").unwrap();
        assert_eq!(tq.query, "jedi");
        assert_eq!(tq.cap, 5);
    }

    #[test]
    fn parse_title_cap_zero_is_err() {
        assert!(parse_title_arg("jedi~0").is_err());
    }

    #[test]
    fn parse_title_cap_non_numeric_is_err() {
        assert!(parse_title_arg("jedi~x").is_err());
    }

    #[tokio::test]
    async fn resolve_asins_verbatim_and_dedupe() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![item("A1", "Jedi Quest"), item("A2", "Star Wars")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();

        // (a) verbatim ASINs: dedupe + passthrough of unknown "ZZ"
        let result = resolve_asins(&db, MP, vec!["A1".into(), "A1".into(), "ZZ".into()], vec![])
            .await
            .unwrap();
        assert_eq!(result, vec!["A1", "ZZ"]);

        // (b) title single match → taken
        let result = resolve_asins(&db, MP, vec![], vec!["jedi".into()])
            .await
            .unwrap();
        assert_eq!(result, vec!["A1"]);

        // (c) title no match → skip, empty result
        let result = resolve_asins(&db, MP, vec![], vec!["nomatch".into()])
            .await
            .unwrap();
        assert!(result.is_empty());

        // (d) title matching both items in non-TTY context → Err (ambiguous)
        // Tests run without a terminal, so the "many" branch bails.
        // "Jedi OR Star" uses FTS5 OR passthrough and matches both A1 and A2.
        let err = resolve_asins(&db, MP, vec![], vec!["Jedi OR Star".into()]).await;
        assert!(
            err.is_err(),
            "expected ambiguous error for multi-match title"
        );

        // (e) combined: asins + title, cross-source dedupe
        let result = resolve_asins(
            &db,
            MP,
            vec!["A1".into()],
            vec!["jedi".into()], // also resolves to A1
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["A1"], "A1 from both sources deduped to one");
    }
}
