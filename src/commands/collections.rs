//! `audible collections` — the account's server-side lists (AUD-107/111):
//! `collections list` enumerates them (`/1.0/lists`), `collections
//! wishlist …` maintains `__WISHLIST` and `collections archive …`
//! maintains `__ARCHIVE` (titles hidden from the library view; the local
//! `is_archived` flag follows on the next `library sync`). Stateless by
//! design: lists are server-authoritative, nothing is cached or
//! persisted. The mutation `state_token` encodes *server* state and is
//! fetched fresh per run (unlike the library delta-sync token, which
//! encodes our local sync position and therefore lives in the DB).
//!
//! Both nouns share one wiring, generic over the collection id, so
//! favorites/user collections (AUD-109) become thin additions. Wire
//! format is capture-proven against the official app (2026-07-06,
//! `__WISHLIST` and `__ARCHIVE`): body-level `continuation_token`
//! pagination on `GET …/items` (no `Continuation-Token` header — the
//! library paginator does not apply), batched `POST …/items` with
//! `{collection_id, asins, state_token}`, batched
//! `DELETE …/items?asins=…&asins=…&state_token=…` (repeated params, like
//! `/library`), each mutation response returning the next token.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context as _, Result, bail};
use clap::{Arg, ArgAction};
use reqwest::Method;
use serde_json::Value;

use crate::api::client::Client;
use crate::commands::catalog::{
    CatalogDetails, catalog_details, format_runtime, resolve_catalog_titles,
};
use crate::config::ctx::Ctx;
use crate::output::Output;

/// Where a noun's `add --title` queries resolve.
#[derive(PartialEq)]
enum AddTitleSource {
    /// Local library FTS — you archive what you own.
    Library,
    /// Catalog search (`GET /1.0/catalog/products?title=…`, the
    /// audible-cli approach) — wishlist candidates are usually not owned,
    /// so there are no library rows to resolve against.
    Catalog,
}

/// One permanent collection exposed as a CLI subcommand.
struct Noun {
    /// Subcommand name.
    name: &'static str,
    /// Server-side collection id.
    collection_id: &'static str,
    /// Message phrase ("the wishlist").
    phrase: &'static str,
    /// Where `add --title` queries resolve.
    add_title_source: AddTitleSource,
    /// Whether mutations offer `--sync`: the archive feeds the local
    /// `is_archived` flag (and `download --missing`, AUD-110), which
    /// only refreshes on a library sync.
    sync_flag: bool,
}

const WISHLIST: Noun = Noun {
    name: "wishlist",
    collection_id: "__WISHLIST",
    phrase: "the wishlist",
    add_title_source: AddTitleSource::Catalog,
    sync_flag: false,
};

const ARCHIVE: Noun = Noun {
    name: "archive",
    collection_id: "__ARCHIVE",
    phrase: "the archive",
    add_title_source: AddTitleSource::Library,
    sync_flag: true,
};

/// `audible collections`.
pub struct CollectionsCommand;

#[async_trait::async_trait]
impl super::Command for CollectionsCommand {
    fn name(&self) -> &'static str {
        "collections"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Your server-side lists (wishlist, archive, favorites, custom lists)")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("list")
                    .about("List all lists (wishlist, archive, favorites, library views, custom)"),
            )
            .subcommand(noun_command(&WISHLIST))
            .subcommand(noun_command(&ARCHIVE))
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let strings = |m: &clap::ArgMatches, id: &str| -> Vec<String> {
            m.get_many::<String>(id)
                .map(|v| v.cloned().collect())
                .unwrap_or_default()
        };
        match matches.subcommand() {
            Some(("list", _)) => list_lists(ctx).await,
            Some((name, sub)) => {
                let noun = if name == WISHLIST.name {
                    &WISHLIST
                } else {
                    &ARCHIVE
                };
                // `--sync`/add-`--title` only exist where the noun defines them.
                let sync = |m: &clap::ArgMatches| noun.sync_flag && m.get_flag("sync");
                match sub.subcommand() {
                    Some(("list", _)) => noun_list(ctx, noun).await,
                    Some(("add", add)) => {
                        noun_add(
                            ctx,
                            noun,
                            strings(add, "asin"),
                            strings(add, "title"),
                            sync(add),
                        )
                        .await
                    }
                    Some(("remove", remove)) => {
                        noun_remove(
                            ctx,
                            noun,
                            strings(remove, "asin"),
                            strings(remove, "title"),
                            sync(remove),
                        )
                        .await
                    }
                    _ => unreachable!("subcommand required"),
                }
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

/// The `wishlist`/`archive` subcommand tree for one noun.
fn noun_command(noun: &'static Noun) -> clap::Command {
    let add_title_help = match noun.add_title_source {
        AddTitleSource::Library => "Library title to add (substring search; repeatable)",
        AddTitleSource::Catalog => {
            "Title to add, searched in the Audible catalog (repeatable; \
             several matches open a selection list)"
        }
    };
    let mut add = clap::Command::new("add")
        .about(format!("Add titles to {}", noun.phrase))
        .arg(
            Arg::new("asin")
                .long("asin")
                .action(ArgAction::Append)
                .value_name("ASIN")
                .help("ASIN to add (repeatable)"),
        )
        .arg(
            Arg::new("title")
                .long("title")
                .action(ArgAction::Append)
                .value_name("QUERY")
                .help(add_title_help),
        )
        .group(
            clap::ArgGroup::new("source")
                .args(["asin", "title"])
                .multiple(true)
                .required(true),
        );
    let mut remove = clap::Command::new("remove")
        .about(format!("Remove titles from {}", noun.phrase))
        .arg(
            Arg::new("asin")
                .long("asin")
                .action(ArgAction::Append)
                .value_name("ASIN")
                .help("ASIN to remove (repeatable)"),
        )
        .arg(
            Arg::new("title")
                .long("title")
                .action(ArgAction::Append)
                .value_name("QUERY")
                .help(format!(
                    "Title substring, matched against {} (repeatable; must match \
                     exactly one title; no wildcards — a trailing * is ignored)",
                    noun.phrase
                )),
        )
        .group(
            clap::ArgGroup::new("source")
                .args(["asin", "title"])
                .multiple(true)
                .required(true),
        );
    if noun.sync_flag {
        let sync = Arg::new("sync")
            .long("sync")
            .action(ArgAction::SetTrue)
            .help(
                "Run a delta library sync right after, so the local library \
                 (is_archived) reflects the change immediately",
            );
        add = add.arg(sync.clone());
        remove = remove.arg(sync);
    }
    clap::Command::new(noun.name)
        .about(format!("Maintain {}", noun.phrase))
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(clap::Command::new("list").about(format!("List titles in {}", noun.phrase)))
        .subcommand(add)
        .subcommand(remove)
}

/// `collections list` — every list `/1.0/lists` reports, across the
/// selected marketplaces.
async fn list_lists(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    let mut rows = Vec::new();
    for marketplace in ctx.marketplaces()? {
        let body: Value = client
            .request(Method::GET, "/1.0/lists")
            .country_code(&marketplace)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for row in lists_rows(&body) {
            let mut full = vec![marketplace.clone()];
            full.extend(row);
            rows.push(full);
        }
    }
    if rows.is_empty() {
        eprintln!("no lists reported");
        return Ok(());
    }
    ctx.print(&Output::table(
        vec!["mp", "id", "name", "type", "section", "items"],
        rows,
    ));
    Ok(())
}

/// Table rows (id, name, type, section, items) from a `/1.0/lists` body.
fn lists_rows(body: &Value) -> Vec<Vec<String>> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_owned()
    };
    let Some(lists) = body.get("lists").and_then(Value::as_array) else {
        return Vec::new();
    };
    lists
        .iter()
        .map(|list| {
            let items = list
                .get("item_count")
                .and_then(Value::as_u64)
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".to_owned());
            vec![
                text(list, "list_id"),
                text(list, "name"),
                text(list, "list_type"),
                text(list, "list_section"),
                items,
            ]
        })
        .collect()
}

/// `collections <noun> list` — items plus catalog titles, across the
/// selected marketplaces.
async fn noun_list(ctx: &Ctx, noun: &Noun) -> Result<()> {
    let client = ctx.client().await?;
    let mut rows = Vec::new();
    let mut total = 0usize;
    for marketplace in ctx.marketplaces()? {
        let items = collection_items(client, &marketplace, noun.collection_id).await?;
        total += items.len();
        let asins: Vec<String> = items.iter().map(|item| item.asin.clone()).collect();
        let details = catalog_details(client, &marketplace, &asins).await?;
        for item in items {
            let detail = details.get(&item.asin);
            rows.push(vec![
                marketplace.clone(),
                item.asin.clone(),
                item.added
                    .as_deref()
                    .map(date_only)
                    .unwrap_or("-")
                    .to_owned(),
                detail.map(|d| d.full_title()).unwrap_or_default(),
                detail
                    .and_then(|d| d.publication.clone())
                    .unwrap_or_else(|| "-".to_owned()),
                detail.map(|d| d.authors.clone()).unwrap_or_default(),
                detail
                    .and_then(|d| d.runtime_min)
                    .map(format_runtime)
                    .unwrap_or_else(|| "-".to_owned()),
            ]);
        }
    }
    if rows.is_empty() {
        eprintln!("{} is empty", noun.phrase);
        return Ok(());
    }
    ctx.print(&Output::table(
        vec![
            "mp",
            "asin",
            "added",
            "title",
            "publication",
            "authors",
            "length",
        ],
        rows,
    ));
    eprintln!("{total} title(s) in {}", noun.phrase);
    Ok(())
}

/// `collections <noun> add` — one batched POST. Explicit ASINs are taken
/// verbatim; `--title` resolves per the noun's source — archive against
/// the local library (you archive what you own), wishlist against the
/// catalog (candidates are usually not owned).
async fn noun_add(
    ctx: &Ctx,
    noun: &Noun,
    asins: Vec<String>,
    titles: Vec<String>,
    sync: bool,
) -> Result<()> {
    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;

    let asins = if titles.is_empty() {
        asins
    } else {
        match noun.add_title_source {
            AddTitleSource::Library => {
                let db = ctx.open_library_db().await?;
                crate::commands::items::resolve_asins(
                    &db,
                    &marketplace,
                    asins,
                    titles,
                    crate::commands::items::PodcastMode::ItemsOnly,
                )
                .await?
            }
            AddTitleSource::Catalog => {
                resolve_catalog_titles(client, &marketplace, asins, titles).await?
            }
        }
    };

    // Pre-check so a duplicate does not silently vanish into the 202.
    let current: HashSet<String> = collection_items(client, &marketplace, noun.collection_id)
        .await?
        .into_iter()
        .map(|item| item.asin)
        .collect();
    let (present, missing): (Vec<String>, Vec<String>) =
        asins.into_iter().partition(|asin| current.contains(asin));
    for asin in &present {
        eprintln!("{asin} is already in {}; skipping", noun.phrase);
    }
    if missing.is_empty() {
        eprintln!("nothing to add");
        return Ok(());
    }

    let meta = collection_meta(client, &marketplace, noun.collection_id).await?;
    let reply: Value = client
        .request(
            Method::POST,
            format!("/1.0/collections/{}/items", noun.collection_id),
        )
        .country_code(&marketplace)
        .body(serde_json::json!({
            "collection_id": noun.collection_id,
            "asins": missing,
            "state_token": meta.state_token,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let added = reply
        .get("num_items_added")
        .and_then(Value::as_u64)
        .map(|count| count.to_string())
        .unwrap_or_else(|| "?".to_owned());
    eprintln!(
        "added {added} of {} title(s) to {}",
        missing.len(),
        noun.phrase
    );
    if noun.sync_flag {
        sync_or_hint(ctx, sync, &marketplace, &missing, true).await?;
    }
    Ok(())
}

/// `collections <noun> remove` — one batched DELETE (repeated `asins`
/// params, capture-proven on `__ARCHIVE`).
async fn noun_remove(
    ctx: &Ctx,
    noun: &Noun,
    asins: Vec<String>,
    titles: Vec<String>,
    sync: bool,
) -> Result<()> {
    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;

    let items = collection_items(client, &marketplace, noun.collection_id).await?;
    // Titles resolve against the collection itself, so enrichment is only
    // needed when a --title query is present.
    let details = if titles.is_empty() {
        BTreeMap::new()
    } else {
        let all: Vec<String> = items.iter().map(|item| item.asin.clone()).collect();
        catalog_details(client, &marketplace, &all).await?
    };
    let targets = resolve_removals(&items, &details, asins, titles, noun.phrase)?;
    if targets.is_empty() {
        eprintln!("nothing to remove");
        return Ok(());
    }

    let meta = collection_meta(client, &marketplace, noun.collection_id).await?;
    let mut request = client
        .request(
            Method::DELETE,
            format!("/1.0/collections/{}/items", noun.collection_id),
        )
        .country_code(&marketplace)
        .query("state_token", &meta.state_token);
    for asin in &targets {
        request = request.query("asins", asin);
    }
    request.send().await?.error_for_status()?;
    for asin in &targets {
        eprintln!("removed {asin} from {}", noun.phrase);
    }
    if noun.sync_flag {
        sync_or_hint(ctx, sync, &marketplace, &targets, false).await?;
    }
    Ok(())
}

/// Delays before each `--sync` delta attempt: the mutation 202 is
/// "accepted" — the library index follows asynchronously (verified live:
/// an immediate delta reported 0 changes, one a few seconds later carried
/// the `is_archived` flip; the app polls `/1.0/library` the same way).
const SYNC_ATTEMPT_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(10),
];

/// After an archive mutation: run delta syncs until the local item docs
/// reflect the new `is_archived` state (`--sync`, bounded retries because
/// the server indexes the change asynchronously), or point at the sync
/// dependency — audible-rs only sees archive changes after a
/// `library sync` (AUD-110/111).
///
/// "Reflected" means the local doc matches the mutation's outcome — that
/// is `--sync`'s contract. A doc that already carried the target value
/// (e.g. re-adding after a remove that was never synced) passes
/// immediately; the server-side flip then lands as a no-op in a later
/// delta.
async fn sync_or_hint(
    ctx: &Ctx,
    sync: bool,
    marketplace: &str,
    asins: &[String],
    expect_archived: bool,
) -> Result<()> {
    if !sync {
        eprintln!("note: run `audible library sync` to reflect the change in the local library");
        return Ok(());
    }
    let db = ctx.open_library_db().await?;
    for (attempt, delay) in SYNC_ATTEMPT_DELAYS.iter().enumerate() {
        if attempt > 0 {
            eprintln!("change not in the library view yet; retrying the sync…");
        }
        tokio::time::sleep(*delay).await;
        crate::commands::library::sync(ctx, false, false, false).await?;
        let mut reflected = true;
        for asin in asins {
            let doc = db.item_doc(asin.clone(), marketplace.to_owned()).await?;
            // Items without a library doc (e.g. archived podcast parents
            // outside the items table) cannot be verified — treat as done.
            if let Some(doc) = doc
                && archived_in_doc(&doc) != Some(expect_archived)
            {
                reflected = false;
                break;
            }
        }
        if reflected {
            return Ok(());
        }
    }
    eprintln!(
        "warning: the archive change has not reached the library view yet — \
         run `audible library sync` again in a moment"
    );
    Ok(())
}

/// The `is_archived` value of a stored library item doc, if present.
fn archived_in_doc(doc: &str) -> Option<bool> {
    serde_json::from_str::<Value>(doc)
        .ok()?
        .get("is_archived")?
        .as_bool()
}

/// One entry of a collection's item list.
struct CollectionItem {
    asin: String,
    /// `addition_date` (ISO timestamp), if reported.
    added: Option<String>,
}

/// Fetches every item of a collection, following the **body**
/// `continuation_token` (this endpoint does not use the library's
/// `Continuation-Token` header).
async fn collection_items(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
) -> Result<Vec<CollectionItem>> {
    let mut items = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut request = client
            .request(
                Method::GET,
                format!("/1.0/collections/{collection_id}/items"),
            )
            .country_code(marketplace)
            .query("response_groups", "always-returned");
        if let Some(token) = &continuation {
            request = request.query("continuation_token", token);
        }
        let body: Value = request.send().await?.error_for_status()?.json().await?;
        let (page, next) = parse_items_page(&body);
        items.extend(page);
        continuation = next;
        if continuation.is_none() {
            return Ok(items);
        }
    }
}

/// Splits an `…/items` response body into its items and the continuation
/// token of the next page (if any).
fn parse_items_page(body: &Value) -> (Vec<CollectionItem>, Option<String>) {
    let mut items = Vec::new();
    if let Some(list) = body.get("items").and_then(Value::as_array) {
        for item in list {
            let Some(asin) = item.get("asin").and_then(Value::as_str) else {
                continue;
            };
            items.push(CollectionItem {
                asin: asin.to_owned(),
                added: item
                    .get("addition_date")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    let next = body
        .get("continuation_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    (items, next)
}

/// Collection metadata; carries the fresh state token every mutation
/// needs (a server-state marker — deliberately never persisted).
struct CollectionMeta {
    state_token: String,
}

async fn collection_meta(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
) -> Result<CollectionMeta> {
    let body: Value = client
        .request(Method::GET, format!("/1.0/collections/{collection_id}"))
        .country_code(marketplace)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let state_token = body
        .get("state_token")
        .and_then(Value::as_str)
        .context("the collection response carries no state_token")?
        .to_owned();
    Ok(CollectionMeta { state_token })
}

/// Removes `asins` from `__ARCHIVE` if present, returning the ASINs that
/// were actually in the archive (and thus removed). Used by
/// `library return` to clear a returned loan's archive membership so it
/// disappears from `collections archive list`, not just the library
/// (AUD-171). A no-op — no request — when none of the ASINs are archived.
pub(crate) async fn remove_from_archive(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<Vec<String>> {
    let archived: HashSet<String> = collection_items(client, marketplace, ARCHIVE.collection_id)
        .await?
        .into_iter()
        .map(|item| item.asin)
        .collect();
    let targets: Vec<String> = asins
        .iter()
        .filter(|asin| archived.contains(*asin))
        .cloned()
        .collect();
    if targets.is_empty() {
        return Ok(targets);
    }
    let meta = collection_meta(client, marketplace, ARCHIVE.collection_id).await?;
    let mut request = client
        .request(
            Method::DELETE,
            format!("/1.0/collections/{}/items", ARCHIVE.collection_id),
        )
        .country_code(marketplace)
        .query("state_token", &meta.state_token);
    for asin in &targets {
        request = request.query("asins", asin);
    }
    request.send().await?.error_for_status()?;
    Ok(targets)
}

/// Resolves `remove` inputs to collection ASINs: explicit ASINs must be
/// in the collection (else warn + skip), each `--title` query must match
/// exactly one title (0 or >1 matches fail with the candidates listed).
fn resolve_removals(
    items: &[CollectionItem],
    details: &BTreeMap<String, CatalogDetails>,
    asins: Vec<String>,
    titles: Vec<String>,
    phrase: &str,
) -> Result<Vec<String>> {
    let in_collection: HashSet<&str> = items.iter().map(|item| item.asin.as_str()).collect();
    let mut targets = Vec::new();
    for asin in asins {
        if in_collection.contains(asin.as_str()) {
            targets.push(asin);
        } else {
            eprintln!("{asin} is not in {phrase}; skipping");
        }
    }
    for query in titles {
        // A trailing FTS5-style wildcard is lossless under substring
        // semantics (`Waffen*` ≙ substring `waffen`) — tolerate it, the
        // library-resolver side of `add --title` accepts it too (AUD-112).
        let stripped = query.trim_end_matches(['*', '?']);
        if stripped.is_empty() {
            bail!("empty title query {query:?}");
        }
        let needle = stripped.to_lowercase();
        let matches: Vec<(&String, String)> = details
            .iter()
            .filter(|(asin, detail)| {
                in_collection.contains(asin.as_str())
                    && detail.full_title().to_lowercase().contains(&needle)
            })
            .map(|(asin, detail)| (asin, detail.full_title()))
            .collect();
        match matches.as_slice() {
            [] => {
                let hint = if needle.contains(['*', '?']) {
                    " (matching is plain substring, not a wildcard — drop the * / ?)"
                } else {
                    ""
                };
                bail!("no title in {phrase} matches {query:?}{hint}")
            }
            [(asin, _)] => targets.push((*asin).clone()),
            many => {
                let listing: Vec<String> = many
                    .iter()
                    .map(|(asin, title)| format!("  {asin}  {title}"))
                    .collect();
                bail!(
                    "{query:?} matches {} titles in {phrase} — narrow the query or use --asin:\n{}",
                    many.len(),
                    listing.join("\n")
                );
            }
        }
    }
    // An ASIN given twice (or via ASIN and title) must not be deleted twice.
    let mut seen = HashSet::new();
    targets.retain(|asin| seen.insert(asin.clone()));
    Ok(targets)
}

/// `2026-06-19T09:20:50.749Z` → `2026-06-19` (defensive on short input).
fn date_only(timestamp: &str) -> &str {
    if timestamp.len() >= 10 && timestamp.is_char_boundary(10) {
        &timestamp[..10]
    } else {
        timestamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(asin: &str) -> CollectionItem {
        CollectionItem {
            asin: asin.to_owned(),
            added: Some("2026-06-19T09:20:50.749Z".to_owned()),
        }
    }

    fn detail(title: &str) -> CatalogDetails {
        CatalogDetails {
            title: title.to_owned(),
            subtitle: None,
            publication: None,
            authors: String::new(),
            runtime_min: None,
        }
    }

    #[test]
    fn parses_items_page_and_continuation() {
        let body = serde_json::json!({
            "continuation_token": "NEXT",
            "items": [
                {"asin": "B0A", "addition_date": "2026-06-19T09:20:50.749Z"},
                {"asin": "B0B", "addition_date": null},
                {"title": "no asin — skipped"},
            ],
        });
        let (items, next) = parse_items_page(&body);
        assert_eq!(next.as_deref(), Some("NEXT"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].asin, "B0A");
        assert_eq!(items[0].added.as_deref(), Some("2026-06-19T09:20:50.749Z"));
        assert_eq!(items[1].added, None);

        // Both null and empty-string tokens end the pagination.
        let done = serde_json::json!({"continuation_token": null, "items": []});
        assert_eq!(parse_items_page(&done).1, None);
        let empty = serde_json::json!({"continuation_token": "", "items": []});
        assert_eq!(parse_items_page(&empty).1, None);
    }

    #[test]
    fn lists_rows_extracts_all_lists() {
        let body = serde_json::json!({"lists": [
            {"list_id": "__WISHLIST", "name": "User Wishlist", "list_type": "WISHLIST",
             "list_section": "YOUR_LISTS", "item_count": 5},
            {"list_id": "__SERIES", "name": "Serien", "list_type": "SERIES",
             "list_section": "LISTS_FROM_LIBRARY", "item_count": null},
        ]});
        let rows = lists_rows(&body);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            ["__WISHLIST", "User Wishlist", "WISHLIST", "YOUR_LISTS", "5"]
        );
        assert_eq!(rows[1][4], "-");
    }

    #[test]
    fn removals_resolve_asins_titles_and_dedupe() {
        let items = vec![item("B0A"), item("B0B"), item("B0C")];
        let mut details = BTreeMap::new();
        details.insert("B0A".to_owned(), detail("Der Hobbit"));
        details.insert("B0B".to_owned(), detail("Der Herr der Ringe 1"));
        details.insert("B0C".to_owned(), detail("Der Herr der Ringe 2"));

        // ASIN off the list is skipped with a warning, not an error.
        let targets = resolve_removals(
            &items,
            &details,
            vec!["B0A".into(), "B0NOTLISTED".into()],
            vec!["hobbit".into()],
            "the wishlist",
        )
        .unwrap();
        // "B0A" arrives via ASIN and title — deleted once.
        assert_eq!(targets, ["B0A"]);

        // Ambiguous title query fails and names the candidates.
        let error = resolve_removals(
            &items,
            &details,
            Vec::new(),
            vec!["herr der ringe".into()],
            "the archive",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 titles in the archive"), "{error}");
        assert!(error.contains("B0B") && error.contains("B0C"), "{error}");

        // No match at all fails too.
        assert!(
            resolve_removals(
                &items,
                &details,
                Vec::new(),
                vec!["narnia".into()],
                "the archive"
            )
            .is_err()
        );

        // The subtitle is matching surface (full title): "Outlander 7"
        // finds "Echo der Hoffnung: Outlander 7".
        let items = vec![item("B0S")];
        let mut with_subtitle = BTreeMap::new();
        let mut d = detail("Echo der Hoffnung");
        d.subtitle = Some("Outlander 7".to_owned());
        with_subtitle.insert("B0S".to_owned(), d);
        let targets = resolve_removals(
            &items,
            &with_subtitle,
            Vec::new(),
            vec!["outlander 7".into()],
            "the wishlist",
        )
        .unwrap();
        assert_eq!(targets, ["B0S"]);
    }

    /// Wildcard tolerance (AUD-112): trailing `*`/`?` are stripped before
    /// the substring match; mid-word wildcards get a hint on zero matches.
    #[test]
    fn removal_titles_tolerate_trailing_wildcards() {
        let items = vec![item("B0A")];
        let mut details = BTreeMap::new();
        details.insert("B0A".to_owned(), detail("Waffenbrüder: Band 11"));

        let targets = resolve_removals(
            &items,
            &details,
            Vec::new(),
            vec!["Waffen*".into()],
            "the archive",
        )
        .unwrap();
        assert_eq!(targets, ["B0A"]);

        let error = resolve_removals(
            &items,
            &details,
            Vec::new(),
            vec!["Waf*fen".into()],
            "the archive",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("plain substring"), "{error}");

        // A pure-wildcard query is rejected instead of matching everything.
        let error = resolve_removals(
            &items,
            &details,
            Vec::new(),
            vec!["*".into()],
            "the archive",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("empty title query"), "{error}");
    }

    #[test]
    fn archived_flag_parses_from_doc() {
        assert_eq!(archived_in_doc(r#"{"is_archived": true}"#), Some(true));
        assert_eq!(archived_in_doc(r#"{"is_archived": false}"#), Some(false));
        assert_eq!(archived_in_doc(r#"{"title": "x"}"#), None);
        assert_eq!(archived_in_doc("not json"), None);
    }

    #[test]
    fn date_formatting() {
        assert_eq!(date_only("2026-06-19T09:20:50.749Z"), "2026-06-19");
        assert_eq!(date_only("short"), "short");
    }

    #[test]
    fn clap_shape() {
        use crate::commands::Command as _;
        let parse = |args: &[&str]| CollectionsCommand.clap().try_get_matches_from(args);
        assert!(parse(&["collections", "list"]).is_ok());
        assert!(parse(&["collections", "wishlist", "list"]).is_ok());
        assert!(
            parse(&[
                "collections",
                "wishlist",
                "add",
                "--asin",
                "B0A",
                "--asin",
                "B0B"
            ])
            .is_ok()
        );
        // Wishlist add takes --asin and/or --title (catalog search) but no --sync.
        assert!(parse(&["collections", "wishlist", "add"]).is_err());
        assert!(parse(&["collections", "wishlist", "add", "--title", "x"]).is_ok());
        assert!(
            parse(&[
                "collections",
                "wishlist",
                "add",
                "--asin",
                "B0A",
                "--title",
                "x"
            ])
            .is_ok()
        );
        assert!(parse(&["collections", "wishlist", "add", "--asin", "B0A", "--sync"]).is_err());
        // Remove requires --asin or --title (mixing is allowed).
        assert!(parse(&["collections", "wishlist", "remove"]).is_err());
        assert!(parse(&["collections", "wishlist", "remove", "--title", "hobbit"]).is_ok());
        assert!(
            parse(&[
                "collections",
                "wishlist",
                "remove",
                "--asin",
                "B0A",
                "--title",
                "x"
            ])
            .is_ok()
        );

        // Archive: --title on add (library resolver) and --sync on both.
        assert!(parse(&["collections", "archive", "list"]).is_ok());
        assert!(
            parse(&[
                "collections",
                "archive",
                "add",
                "--title",
                "waffen",
                "--sync"
            ])
            .is_ok()
        );
        assert!(parse(&["collections", "archive", "add", "--asin", "B0A"]).is_ok());
        assert!(parse(&["collections", "archive", "add"]).is_err());
        assert!(
            parse(&[
                "collections",
                "archive",
                "remove",
                "--asin",
                "B0A",
                "--sync"
            ])
            .is_ok()
        );
        assert!(parse(&["collections", "archive", "remove", "--sync"]).is_err());
    }
}
