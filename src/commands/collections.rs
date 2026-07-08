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
use crate::config::ctx::Ctx;
use crate::output::Output;

/// ASINs per `/1.0/catalog/products` enrichment request (server limit).
const CATALOG_BATCH: usize = 50;

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
                crate::commands::items::resolve_asins(&db, &marketplace, asins, titles).await?
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

/// Catalog display data for one ASIN.
struct CatalogDetails {
    title: String,
    subtitle: Option<String>,
    /// `publication_name` — the series-like collection name (the filename
    /// template's `%publication%`); telling for series volumes.
    publication: Option<String>,
    authors: String,
    runtime_min: Option<u64>,
}

impl CatalogDetails {
    /// `Title: Subtitle` (or just the title).
    fn full_title(&self) -> String {
        match &self.subtitle {
            Some(subtitle) => format!("{}: {subtitle}", self.title),
            None => self.title.clone(),
        }
    }
}

/// Extracts one product's ASIN + display details from a
/// `/1.0/catalog/products` entry (shared by the ASIN-batch enrichment
/// and the title search).
fn parse_product(product: &Value) -> Option<(String, CatalogDetails)> {
    let asin = product.get("asin").and_then(Value::as_str)?;
    let text = |key: &str| {
        product
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    };
    let authors = product
        .get("authors")
        .and_then(Value::as_array)
        .map(|authors| {
            authors
                .iter()
                .filter_map(|author| author.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some((
        asin.to_owned(),
        CatalogDetails {
            title: text("title").unwrap_or_default(),
            subtitle: text("subtitle"),
            publication: text("publication_name"),
            authors,
            runtime_min: product.get("runtime_length_min").and_then(Value::as_u64),
        },
    ))
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

/// Results per catalog title search (the reference's `num_results`).
const CATALOG_SEARCH_RESULTS: u32 = 50;

/// One catalog search result, in server ranking order.
struct CatalogHit {
    asin: String,
    details: CatalogDetails,
}

/// Picker/listing row: ASIN + full title, plus the publication (when it
/// is not already part of the full title), authors and runtime —
/// meaningful to people who do not think in ASINs.
fn hit_label(hit: &CatalogHit) -> String {
    let details = &hit.details;
    let full_title = details.full_title();
    let mut extras = Vec::new();
    if let Some(publication) = &details.publication
        && !full_title
            .to_lowercase()
            .contains(&publication.to_lowercase())
    {
        extras.push(publication.clone());
    }
    if !details.authors.is_empty() {
        extras.push(details.authors.clone());
    }
    if let Some(minutes) = details.runtime_min {
        extras.push(format_runtime(minutes));
    }
    let mut label = format!("{}  {full_title}", hit.asin);
    if !extras.is_empty() {
        label.push_str(&format!("  [{}]", extras.join(" · ")));
    }
    label
}

/// Resolves `wishlist add --title` queries against the catalog
/// (`GET /1.0/catalog/products?title=…`, the audible-cli approach) into a
/// deduped, order-preserving ASIN list after the explicit `asins`.
/// Per query: 0 hits → warning + skip; 1 → taken (echoed); several →
/// interactive multi-select on a TTY, else an error listing the
/// candidates. Hits whose full title contains the query verbatim
/// shortlist the fuzzier rest — the reference's 100%-shortlist.
async fn resolve_catalog_titles(
    client: &Client,
    marketplace: &str,
    asins: Vec<String>,
    queries: Vec<String>,
) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
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

    let mut interacted = false;
    for query in queries {
        let query = query.trim();
        if query.is_empty() {
            eprintln!("ignoring empty --title");
            continue;
        }
        let hits = catalog_search(client, marketplace, query).await?;
        let total = hits.len();
        let hits = verbatim_title_shortlist(hits, query);
        if hits.len() < total {
            eprintln!(
                "({} of {total} catalog results contain {query:?} verbatim; the rest are hidden)",
                hits.len()
            );
        }
        match hits.as_slice() {
            [] => eprintln!("no catalog title matches {query:?}"),
            [hit] => {
                eprintln!("{query:?} → {}", hit_label(hit));
                push(hit.asin.clone());
            }
            many => {
                if console::Term::stderr().is_term() {
                    let labels: Vec<String> = many.iter().map(hit_label).collect();
                    // `report(false)`: as in the library resolver, the
                    // echoed one-line summary replaces dialoguer's default.
                    let selection = dialoguer::MultiSelect::with_theme(
                        &dialoguer::theme::ColorfulTheme::default(),
                    )
                    .with_prompt(format!(
                        "Catalog matches for {query:?} — space toggles · a all · enter confirms"
                    ))
                    .items(&labels)
                    .report(false)
                    .interact_on(&console::Term::stderr())?;
                    interacted = true;
                    if selection.is_empty() {
                        eprintln!("no titles selected for {query:?}");
                    } else {
                        eprintln!(
                            "selected {} of {} for {query:?}",
                            selection.len(),
                            many.len()
                        );
                        for index in selection {
                            push(many[index].asin.clone());
                        }
                    }
                } else {
                    let listing: Vec<String> = many
                        .iter()
                        .map(|hit| format!("  {}", hit_label(hit)))
                        .collect();
                    bail!(
                        "{} catalog titles match {query:?}; pass --asin or run interactively:\n{}",
                        many.len(),
                        listing.join("\n"),
                    );
                }
            }
        }
    }
    if interacted {
        eprintln!();
    }
    Ok(result)
}

/// One catalog title search, results in server ranking order.
async fn catalog_search(
    client: &Client,
    marketplace: &str,
    query: &str,
) -> Result<Vec<CatalogHit>> {
    let body: Value = client
        .request(Method::GET, "/1.0/catalog/products")
        .country_code(marketplace)
        .query("title", query)
        .query("num_results", CATALOG_SEARCH_RESULTS.to_string())
        .query("response_groups", "product_desc,product_attrs,contributors")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_catalog_hits(&body))
}

/// Extracts the ranked search results from a `/1.0/catalog/products` body.
fn parse_catalog_hits(body: &Value) -> Vec<CatalogHit> {
    let Some(products) = body.get("products").and_then(Value::as_array) else {
        return Vec::new();
    };
    products
        .iter()
        .filter_map(|product| {
            parse_product(product).map(|(asin, details)| CatalogHit { asin, details })
        })
        .collect()
}

/// The reference's 100%-shortlist with its actual semantics: audible-cli
/// scores a fuzzy *partial* match, so 100 means the query appears
/// verbatim in the title — not that it equals it (title equality would
/// collapse "Outlander" to the one volume literally titled that and hide
/// every "…Outlander-Saga…" volume). When any hit contains the query
/// (case-insensitive) in its full title, only those hits are offered
/// (trims fuzzy server noise); otherwise every hit stays.
fn verbatim_title_shortlist(hits: Vec<CatalogHit>, query: &str) -> Vec<CatalogHit> {
    let needle = query.to_lowercase();
    let contains = |hit: &CatalogHit| {
        hit.details.full_title().to_lowercase().contains(&needle)
            || hit
                .details
                .publication
                .as_ref()
                .is_some_and(|publication| publication.to_lowercase().contains(&needle))
    };
    if !hits.iter().any(&contains) {
        return hits;
    }
    hits.into_iter().filter(|hit| contains(hit)).collect()
}

/// Title/authors/runtime for catalog ASINs, batched 50 per request.
/// Sequential — these lists are far below the scale where concurrency pays.
async fn catalog_details(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, CatalogDetails>> {
    let mut details = BTreeMap::new();
    for chunk in asins.chunks(CATALOG_BATCH) {
        let body: Value = client
            .request(Method::GET, "/1.0/catalog/products")
            .country_code(marketplace)
            .query("asins", chunk.join(","))
            .query("response_groups", "product_desc,product_attrs,contributors")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        parse_catalog_details(&body, &mut details);
    }
    Ok(details)
}

/// Extracts per-ASIN display details from a `/1.0/catalog/products` body.
fn parse_catalog_details(body: &Value, details: &mut BTreeMap<String, CatalogDetails>) {
    let Some(products) = body.get("products").and_then(Value::as_array) else {
        return;
    };
    for product in products {
        if let Some((asin, detail)) = parse_product(product) {
            details.insert(asin, detail);
        }
    }
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

/// Minutes → `12h 34m` / `45m`.
fn format_runtime(minutes: u64) -> String {
    if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
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
    fn parses_catalog_details_with_authors() {
        let body = serde_json::json!({"products": [
            {"asin": "B0A", "title": "Book One",
             "authors": [{"name": "Alice"}, {"name": "Bob"}],
             "runtime_length_min": 754},
        ]});
        let mut details = BTreeMap::new();
        parse_catalog_details(&body, &mut details);
        let one = details.get("B0A").unwrap();
        assert_eq!(one.title, "Book One");
        assert_eq!(one.authors, "Alice, Bob");
        assert_eq!(one.runtime_min, Some(754));
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
    fn catalog_hits_parse_in_order_with_display_fields() {
        let body = serde_json::json!({"products": [
            {"asin": "B0B", "title": "Echo der Hoffnung", "subtitle": "Outlander 7",
             "publication_name": "Outlander [German Edition]",
             "authors": [{"name": "Diana Gabaldon"}], "runtime_length_min": 3255},
            {"asin": "B0A", "title": "Der Hobbit"},
            {"title": "no asin — skipped"},
        ]});
        let hits = parse_catalog_hits(&body);
        // Server ranking order is preserved (no map re-sorting).
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].asin, "B0B");
        assert_eq!(
            hits[0].details.full_title(),
            "Echo der Hoffnung: Outlander 7"
        );
        // The publication is shown when the full title does not carry it…
        assert_eq!(
            hit_label(&hits[0]),
            "B0B  Echo der Hoffnung: Outlander 7  [Outlander [German Edition] · Diana Gabaldon · 54h 15m]"
        );
        // …and missing extras degrade gracefully.
        assert_eq!(hit_label(&hits[1]), "B0A  Der Hobbit");

        // A publication already contained in the full title is not repeated.
        let body = serde_json::json!({"products": [
            {"asin": "B0C", "title": "Outlander", "subtitle": "Outlander, Book 1",
             "publication_name": "Outlander"},
        ]});
        let hits = parse_catalog_hits(&body);
        assert_eq!(hit_label(&hits[0]), "B0C  Outlander: Outlander, Book 1");
    }

    #[test]
    fn verbatim_matches_shortlist_the_hits() {
        let hit = |asin: &str, title: &str, subtitle: Option<&str>| CatalogHit {
            asin: asin.to_owned(),
            details: CatalogDetails {
                title: title.to_owned(),
                subtitle: subtitle.map(str::to_owned),
                publication: None,
                authors: String::new(),
                runtime_min: None,
            },
        };
        // Regression (maintainer, 2026-07-07): searching "Outlander" must
        // offer EVERY volume whose full title contains the query — the old
        // title-equality shortlist collapsed the list to the one volume
        // literally titled "Outlander" and auto-added it without a picker.
        let hits = vec![
            hit("B0EN", "Outlander", Some("Outlander, Book 1")),
            hit("B0DE", "Feuer und Stein", Some("Die Outlander-Saga 1")),
            hit("B0XX", "Cross Stitch", None), // fuzzy server noise
        ];
        let shortlisted = verbatim_title_shortlist(hits, "outlander");
        assert_eq!(shortlisted.len(), 2);
        assert_eq!(shortlisted[0].asin, "B0EN");
        assert_eq!(shortlisted[1].asin, "B0DE");

        // No hit contains the query verbatim: everything stays.
        let hits = vec![
            hit("B0A", "Der Hobbit", None),
            hit("B0B", "Der kleine Hobbit", None),
        ];
        assert_eq!(verbatim_title_shortlist(hits, "hobit").len(), 2);

        // The publication counts as matching surface too: a volume whose
        // series name only lives in publication_name survives the shortlist.
        let mut with_publication = hit("B0P", "Feuer und Stein", None);
        with_publication.details.publication = Some("Outlander [German Edition]".to_owned());
        let hits = vec![
            hit("B0EN", "Outlander", Some("Outlander, Book 1")),
            with_publication,
        ];
        assert_eq!(verbatim_title_shortlist(hits, "outlander").len(), 2);
    }

    #[test]
    fn archived_flag_parses_from_doc() {
        assert_eq!(archived_in_doc(r#"{"is_archived": true}"#), Some(true));
        assert_eq!(archived_in_doc(r#"{"is_archived": false}"#), Some(false));
        assert_eq!(archived_in_doc(r#"{"title": "x"}"#), None);
        assert_eq!(archived_in_doc("not json"), None);
    }

    #[test]
    fn date_and_runtime_formatting() {
        assert_eq!(date_only("2026-06-19T09:20:50.749Z"), "2026-06-19");
        assert_eq!(date_only("short"), "short");
        assert_eq!(format_runtime(754), "12h 34m");
        assert_eq!(format_runtime(45), "45m");
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
