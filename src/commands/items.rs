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
///
/// `mode` (AUD-174, AUD-196) controls podcast handling:
/// [`PodcastMode::ItemsOnly`] surfaces only items (membership commands — a
/// child episode is not an own membership); [`PodcastMode::Episodes`]
/// additionally matches the child episodes of followed podcasts (`episodes`
/// table, LIKE), labeled `episode of <show>` (annotations, download records);
/// [`PodcastMode::Download`] treats episodes as targets and expands a podcast
/// parent to its episodes — an `--asin` parent becomes all its episodes and a
/// `--title` parent is offered in the picker with its episodes beneath it —
/// or, with `include == false`, drops all podcast content. Individually-
/// subscribed episodes are `items` rows and are found either way.
pub(crate) async fn resolve_asins(
    db: &crate::db::Db,
    marketplace: &str,
    asins: Vec<String>,
    titles: Vec<String>,
    mode: PodcastMode,
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
        if asin.is_empty() {
            continue;
        }
        match mode {
            // Download expands a podcast parent to its episodes (or drops
            // podcast content under `--exclude-podcasts`); every other consumer
            // takes the explicit ASIN verbatim (trusted as an exact id).
            PodcastMode::Download { include } => {
                resolve_asin_download(db, marketplace, &asin, include, &mut push).await?;
            }
            _ => push(asin),
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
        let candidates = title_candidates(db, marketplace, &tq, mode).await?;
        match candidates.len() {
            0 => {
                eprintln!("no title matches {:?}", tq.query);
            }
            1 => {
                choose(&candidates[0], &mut push);
            }
            _ => {
                if console::Term::stderr().is_term() {
                    let labels: Vec<&str> = candidates.iter().map(|c| c.label.as_str()).collect();
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
                            candidates.len(),
                            tq.query
                        );
                        for i in selection {
                            choose(&candidates[i], &mut push);
                        }
                    }
                } else {
                    // Cap the dump — a single podcast show can expand to
                    // hundreds of episode rows.
                    const MAX_LISTED: usize = 20;
                    let mut listing: Vec<String> = candidates
                        .iter()
                        .take(MAX_LISTED)
                        .map(|c| format!("  {}", c.label))
                        .collect();
                    if candidates.len() > MAX_LISTED {
                        listing.push(format!("  … and {} more", candidates.len() - MAX_LISTED));
                    }
                    // A podcast show can only be taken whole non-interactively.
                    let hint = if candidates.iter().any(|c| c.kind == "podcast") {
                        "; a podcast show is among them — use --asin <ASIN> to take all its \
                         episodes, or narrow the query"
                    } else {
                        "; pass --asin or run interactively"
                    };
                    bail!(
                        "{} titles match {:?}{}:\n{}",
                        candidates.len(),
                        tq.query,
                        hint,
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

/// How [`resolve_asins`] treats podcast shows and their episodes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodcastMode {
    /// Child episodes are not surfaced; podcast parents pass through as plain
    /// items. (membership / collections / non-episode db paths)
    ItemsOnly,
    /// Child episodes are valid individual targets in title search; podcast
    /// parents pass through unchanged, no expansion. (annotations, download
    /// info, episode db paths)
    Episodes,
    /// `download`: episodes are targets and a podcast **parent expands to its
    /// episodes** — a parent given by `--asin` becomes all its episodes; a
    /// parent matched by `--title` is offered in the picker with its episodes
    /// listed beneath it (AUD-196). When `include` is false, all podcast
    /// parents and episodes are dropped from the result.
    Download { include: bool },
}

impl PodcastMode {
    /// Whether episode rows are surfaced as individual title-search targets.
    fn surfaces_episodes(self) -> bool {
        !matches!(self, PodcastMode::ItemsOnly)
    }
}

/// A single row offered for a `--title` match.
struct Candidate {
    /// The row's own asin.
    asin: String,
    /// `book` | `podcast` | `episode`.
    kind: &'static str,
    /// Plain title (no episode-count suffix), used for the "expanding …" note.
    title: String,
    /// Preformatted picker label (mode-aware; a type column under `Download`).
    label: String,
    /// Episode asins to enqueue when a podcast **parent** is chosen; `None`
    /// for a plain item (enqueue `asin`). A parent never enqueues its own
    /// (un-downloadable) asin — only its episodes.
    expand: Option<Vec<String>>,
}

/// Enqueues the asin(s) a chosen candidate stands for, announcing a podcast
/// parent's expansion (or the absence of episodes).
fn choose<F: FnMut(String)>(cand: &Candidate, push: &mut F) {
    match &cand.expand {
        None => push(cand.asin.clone()),
        Some(eps) if eps.is_empty() => {
            eprintln!(
                "no episodes for \"{}\" in the library — run `library sync`",
                cand.title
            );
        }
        Some(eps) => {
            eprintln!("expanding \"{}\" → {} episodes", cand.title, eps.len());
            for asin in eps {
                push(asin.clone());
            }
        }
    }
}

/// Builds the `--title` candidate rows for one query under `mode`.
async fn title_candidates(
    db: &crate::db::Db,
    marketplace: &str,
    tq: &TitleQuery,
    mode: PodcastMode,
) -> Result<Vec<Candidate>> {
    let (download, include) = match mode {
        PodcastMode::Download { include } => (true, include),
        _ => (false, false),
    };
    // `--exclude-podcasts` drops podcast content; the include path expands a
    // show into its episodes.
    let exclude = download && !include;
    let include_show = download && include;
    let rows = db
        .search(
            vec![marketplace.to_owned()],
            Vec::new(), // all kinds — callers guard on kind themselves
            tq.query.clone(),
            tq.cap as u32,
            true,
        )
        .await?;

    // Only a lone podcast parent gets its episodes expanded into individual
    // rows; with several shows in the results they stay collapsed (each still
    // selectable as "all its episodes") to keep the picker readable.
    let expand_rows = include_show && rows.iter().filter(|r| r.kind == "podcast").count() == 1;

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_asins: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        seen_asins.insert(row.asin.clone());
        let kind: &'static str = match row.kind.as_str() {
            "podcast" => "podcast",
            "episode" => "episode",
            _ => "book",
        };
        // --exclude-podcasts: no podcast content is a target.
        if exclude && kind != "book" {
            continue;
        }
        if include_show && kind == "podcast" {
            let episodes = db
                .episodes(marketplace.to_owned(), Some(row.asin.clone()), u32::MAX, 0)
                .await?;
            let ep_asins: Vec<String> = episodes.iter().map(|e| e.asin.clone()).collect();
            candidates.push(Candidate {
                label: format!(
                    "{:<8} {}  {}  ({} episodes)",
                    "podcast",
                    row.asin,
                    row.full_title,
                    ep_asins.len()
                ),
                asin: row.asin,
                kind: "podcast",
                title: row.full_title,
                expand: Some(ep_asins),
            });
            if expand_rows {
                for e in episodes {
                    seen_asins.insert(e.asin.clone());
                    candidates.push(Candidate {
                        label: format!("  {:<6} {}  {}", "episode", e.asin, e.full_title),
                        asin: e.asin,
                        kind: "episode",
                        title: e.full_title,
                        expand: None,
                    });
                }
            }
        } else {
            candidates.push(Candidate {
                label: if download {
                    format!("{:<8} {}  {}", kind, row.asin, row.full_title)
                } else {
                    format!("{}  {}", row.asin, row.full_title)
                },
                asin: row.asin,
                kind,
                title: row.full_title,
                expand: None,
            });
        }
    }

    // Episode text matches (individually-subscribed, or by episode title),
    // deduped against item hits and any already-listed show episodes.
    if mode.surfaces_episodes() && !exclude {
        for hit in db
            .search_episodes(marketplace.to_owned(), tq.query.clone(), tq.cap as u32)
            .await?
        {
            if !seen_asins.insert(hit.asin.clone()) {
                continue;
            }
            candidates.push(Candidate {
                label: if download {
                    format!(
                        "{:<8} {}  {}  (episode of {})",
                        "episode", hit.asin, hit.full_title, hit.parent_title
                    )
                } else {
                    format!(
                        "{}  {}  (episode of {})",
                        hit.asin, hit.full_title, hit.parent_title
                    )
                },
                asin: hit.asin,
                kind: "episode",
                title: hit.full_title,
                expand: None,
            });
        }
    }
    Ok(candidates)
}

/// The library classification of an explicit `--asin` for `download` mode.
enum AsinClass {
    Parent {
        title: String,
    },
    Episode,
    /// A book, or an ASIN not (yet) in the library.
    Other,
}

/// Classifies an explicit `--asin` against the local library.
async fn classify_asin(db: &crate::db::Db, marketplace: &str, asin: &str) -> Result<AsinClass> {
    if let Some(doc) = db.item_doc(asin.to_owned(), marketplace.to_owned()).await? {
        let value: serde_json::Value =
            serde_json::from_str(&doc).unwrap_or(serde_json::Value::Null);
        return Ok(match crate::models::library::item_kind(&value) {
            "podcast" => AsinClass::Parent {
                title: crate::models::library::build_full_title(&value)
                    .unwrap_or_else(|| asin.to_owned()),
            },
            "episode" => AsinClass::Episode,
            _ => AsinClass::Other,
        });
    }
    if db
        .episode_doc(asin.to_owned(), marketplace.to_owned())
        .await?
        .is_some()
    {
        return Ok(AsinClass::Episode);
    }
    Ok(AsinClass::Other)
}

/// Resolves one explicit `--asin` under `download` mode: a podcast parent
/// expands to all its episodes (or is dropped with `--exclude-podcasts`), an
/// episode is dropped under `--exclude-podcasts`, and everything else passes
/// through — books, plus AYCL/Plus titles not yet in the library (the
/// unknown-ASIN gate is AUD-197).
async fn resolve_asin_download<F: FnMut(String)>(
    db: &crate::db::Db,
    marketplace: &str,
    asin: &str,
    include: bool,
    push: &mut F,
) -> Result<()> {
    match classify_asin(db, marketplace, asin).await? {
        AsinClass::Parent { title } => {
            if !include {
                eprintln!("skipping podcast show {asin} (\"{title}\") — --exclude-podcasts");
                return Ok(());
            }
            let episodes = db
                .episodes(marketplace.to_owned(), Some(asin.to_owned()), u32::MAX, 0)
                .await?;
            if episodes.is_empty() {
                eprintln!(
                    "no episodes for \"{title}\" ({asin}) in the library — run `library sync`"
                );
            } else {
                eprintln!("expanding \"{title}\" → {} episodes", episodes.len());
                for e in episodes {
                    push(e.asin);
                }
            }
        }
        AsinClass::Episode if !include => {
            eprintln!("skipping podcast episode {asin} — --exclude-podcasts");
        }
        AsinClass::Episode | AsinClass::Other => push(asin.to_owned()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, SyncLogEntry, UpsertEpisode, UpsertItem, now_iso_utc};

    fn episode(asin: &str, title: &str) -> UpsertEpisode {
        UpsertEpisode {
            asin: asin.into(),
            doc: serde_json::json!({"asin": asin, "title": title}).to_string(),
            title: title.into(),
            subtitle: None,
            full_title: title.into(),
        }
    }

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

    /// A podcast show (parent) item: `item_kind` classifies it as `podcast`.
    fn parent(asin: &str, title: &str) -> UpsertItem {
        UpsertItem {
            asin: asin.into(),
            doc: serde_json::json!({
                "asin": asin,
                "title": title,
                "content_type": "Podcast",
                "content_delivery_type": "PodcastParent",
                "purchase_date": "2024-01-01",
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
        let result = resolve_asins(
            &db,
            MP,
            vec!["A1".into(), "A1".into(), "ZZ".into()],
            vec![],
            PodcastMode::ItemsOnly,
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["A1", "ZZ"]);

        // (b) title single match → taken
        let result = resolve_asins(&db, MP, vec![], vec!["jedi".into()], PodcastMode::ItemsOnly)
            .await
            .unwrap();
        assert_eq!(result, vec!["A1"]);

        // (c) title no match → skip, empty result
        let result = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["nomatch".into()],
            PodcastMode::ItemsOnly,
        )
        .await
        .unwrap();
        assert!(result.is_empty());

        // (d) title matching both items in non-TTY context → Err (ambiguous)
        // Tests run without a terminal, so the "many" branch bails.
        // "Jedi OR Star" uses FTS5 OR passthrough and matches both A1 and A2.
        let err = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["Jedi OR Star".into()],
            PodcastMode::ItemsOnly,
        )
        .await;
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
            PodcastMode::ItemsOnly,
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["A1"], "A1 from both sources deduped to one");
    }

    /// Episode resolution (AUD-174): child episodes of a followed show are
    /// found only when episodes are surfaced (`Episodes`/`Download`, not
    /// `ItemsOnly`); an individually-subscribed episode (an `items` row) is
    /// found either way; an ASIN stored in both tables is offered once.
    #[tokio::test]
    async fn resolve_asins_episode_scope() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        // A followed show (item) with a child episode, plus a standalone
        // (individually-subscribed) episode that is stored as an item AND
        // as a child of the show.
        db.apply_page(
            MP.into(),
            vec![item("P1", "Mein Podcast"), item("E9", "Sonderfolge Neun")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![
                episode("E1", "Folge Eins: Anfang"),
                episode("E9", "Sonderfolge Neun"),
            ],
            crate::db::ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        // Child episode: invisible without the episode scope …
        let result = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["Anfang".into()],
            PodcastMode::ItemsOnly,
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "child episode must not match: {result:?}"
        );
        // … found with it.
        let result = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["Anfang".into()],
            PodcastMode::Episodes,
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["E1"]);

        // Individually-subscribed episode: an items row, found even
        // without the episode scope (the library-remove case).
        let result = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["Sonderfolge".into()],
            PodcastMode::ItemsOnly,
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["E9"]);
        // With the scope it is offered once (deduped across the tables),
        // so the single-hit fast path still applies.
        let result = resolve_asins(
            &db,
            MP,
            vec![],
            vec!["Sonderfolge".into()],
            PodcastMode::Episodes,
        )
        .await
        .unwrap();
        assert_eq!(result, vec!["E9"]);
    }

    /// Download mode (AUD-196): an explicit `--asin` podcast parent expands
    /// to all its episodes; `--exclude-podcasts` drops podcast content; an
    /// explicit child episode dedupes against the expansion.
    #[tokio::test]
    async fn resolve_asins_download_podcast_expansion() {
        let (_dir, db) = open_temp().await;
        db.ensure_sync_state(MP.into(), "g".into()).await.unwrap();
        db.apply_page(
            MP.into(),
            vec![parent("P1", "Mein Podcast"), item("B1", "A Book")],
            vec![],
            default_log(),
            None,
        )
        .await
        .unwrap();
        db.apply_episodes(
            MP.into(),
            "P1".into(),
            vec![episode("E1", "Folge Eins"), episode("E2", "Folge Zwei")],
            crate::db::ChangeRecording {
                record: false,
                mode: "delta",
            },
        )
        .await
        .unwrap();

        // include: the show ASIN becomes all its episodes.
        let mut result = resolve_asins(
            &db,
            MP,
            vec!["P1".into()],
            vec![],
            PodcastMode::Download { include: true },
        )
        .await
        .unwrap();
        result.sort();
        assert_eq!(result, vec!["E1".to_owned(), "E2".to_owned()]);

        // exclude: the show ASIN is dropped entirely.
        let result = resolve_asins(
            &db,
            MP,
            vec!["P1".into()],
            vec![],
            PodcastMode::Download { include: false },
        )
        .await
        .unwrap();
        assert!(result.is_empty(), "show dropped under exclude: {result:?}");

        // exclude also drops an explicit child episode.
        let result = resolve_asins(
            &db,
            MP,
            vec!["E1".into()],
            vec![],
            PodcastMode::Download { include: false },
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "episode dropped under exclude: {result:?}"
        );

        // include: parent expansion + an explicit child + a book, deduped and
        // order-preserving (book first, then the show's episodes once each).
        let result = resolve_asins(
            &db,
            MP,
            vec!["B1".into(), "P1".into(), "E1".into()],
            vec![],
            PodcastMode::Download { include: true },
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            vec!["B1".to_owned(), "E1".to_owned(), "E2".to_owned()]
        );
    }
}
