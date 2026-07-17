//! Shared catalog (`/1.0/catalog/products`) access: title→ASIN search and
//! resolution, per-ASIN display enrichment, and subscription eligibility
//! (`customer_rights`). The network counterpart to [`super::items`]'s
//! local-library resolver — used by `collections` (wishlist/archive) and
//! `library` (borrow/return).

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::Value;

use crate::api::client::Client;

/// ASINs per `/1.0/catalog/products` batch (server limit).
const CATALOG_BATCH: usize = 50;

/// Results requested per catalog title search.
const CATALOG_SEARCH_RESULTS: u32 = 50;

/// Catalog display data for one ASIN.
pub(crate) struct CatalogDetails {
    pub title: String,
    pub subtitle: Option<String>,
    /// `publication_name` — the series-like collection name (the filename
    /// template's `%publication%`); telling for series volumes.
    pub publication: Option<String>,
    pub authors: String,
    pub runtime_min: Option<u64>,
}

impl CatalogDetails {
    /// `Title: Subtitle` (or just the title).
    pub(crate) fn full_title(&self) -> String {
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

/// Resolves `--title` queries against the catalog
/// (`GET /1.0/catalog/products?title=…`, the audible-cli approach) into a
/// deduped, order-preserving ASIN list after the explicit `asins`.
/// Per query: 0 hits → warning + skip; 1 → taken (echoed); several →
/// interactive multi-select on a TTY, else an error listing the
/// candidates. Hits whose full title contains the query verbatim
/// shortlist the fuzzier rest — the reference's 100%-shortlist.
pub(crate) async fn resolve_catalog_titles(
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
pub(crate) async fn catalog_details(
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

/// The raw catalog documents for `asins`, batched 50 per request, carrying the
/// fields the naming engine reads plus `customer_rights`.
///
/// For titles the library holds no document for (AUD-197): the catalog names
/// them under the same keys, so they can be filed like any other title. The one
/// field it cannot carry is `purchase_date` — a title that was never bought has
/// none, so `%purchase_year%` renders empty.
///
/// **An ASIN that is no product is absent from the result.** The catalog does
/// not reject one it has never heard of; it echoes it back as a product
/// carrying only that ASIN, counted in `total_results`. Presence therefore
/// proves nothing, and a missing title is what "no such product" looks like
/// from here — so the hollow echoes are dropped rather than passed on as if
/// they were real.
pub(crate) async fn documents(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, Value>> {
    let mut docs = BTreeMap::new();
    for chunk in asins.chunks(CATALOG_BATCH) {
        let body: Value = client
            .request(Method::GET, "/1.0/catalog/products")
            .country_code(marketplace)
            .query("asins", chunk.join(","))
            .query(
                "response_groups",
                "product_desc,product_attrs,product_extended_attrs,customer_rights",
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for product in body
            .get("products")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(asin) = product.get("asin").and_then(Value::as_str) else {
                continue;
            };
            let titled = product
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| !title.trim().is_empty());
            if titled {
                docs.insert(asin.to_owned(), product.clone());
            }
        }
    }
    Ok(docs)
}

/// Whether the account may fetch this title, from a catalog document's
/// `customer_rights` — the same signal [`Eligibility::is_consumable`] carries,
/// for callers that already hold the document. `None` when unauthenticated:
/// the field is the account's, so it is absent without one.
pub(crate) fn is_consumable(doc: &Value) -> Option<bool> {
    doc.get("customer_rights")?.get("is_consumable")?.as_bool()
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

/// Subscription eligibility for one ASIN, from the **authenticated**
/// catalog doc's `customer_rights` (absent/null when unauthenticated).
pub(crate) struct Eligibility {
    /// `Title: Subtitle` if present, else the title.
    pub full_title: String,
    /// `customer_rights.is_consumable` — the account may consume this
    /// title (owned, or borrowable via its current subscription on this
    /// marketplace). `None` if the field is absent. This is the
    /// borrowability signal — the app's "included in subscription".
    pub is_consumable: Option<bool>,
    /// `customer_rights.is_consumable_indefinitely` — `true` = kept
    /// forever (a purchase), `false` = a time-limited loan.
    pub is_consumable_indefinitely: Option<bool>,
    /// `content_delivery_type` — classifies the wording of the action
    /// (an audiobook is "borrowed", a `PodcastParent`/`Periodical` is
    /// "followed", a `PodcastEpisode` is "added"). The wire call is the
    /// same `PUT /1.0/library/item` regardless.
    pub content_delivery_type: Option<String>,
}

impl Eligibility {
    /// Whether the account can borrow this title now: consumable but not
    /// kept indefinitely (a loan, not a purchase). `false` for buy-only
    /// titles (`is_consumable == false/None`).
    pub(crate) fn is_borrowable(&self) -> bool {
        self.is_consumable == Some(true) && self.is_consumable_indefinitely != Some(true)
    }
}

/// Fetches subscription eligibility for `asins` from the authenticated
/// catalog, batched 50 per request. Marketplace-relative — the caller
/// must use the same marketplace it intends to borrow on.
pub(crate) async fn eligibility(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, Eligibility>> {
    let mut out = BTreeMap::new();
    for chunk in asins.chunks(CATALOG_BATCH) {
        let body: Value = client
            .request(Method::GET, "/1.0/catalog/products")
            .country_code(marketplace)
            .query("asins", chunk.join(","))
            .query("response_groups", "product_desc,customer_rights")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for product in body
            .get("products")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(asin) = product.get("asin").and_then(Value::as_str) else {
                continue;
            };
            let (_, details) = match parse_product(product) {
                Some(parsed) => parsed,
                None => continue,
            };
            let rights = product.get("customer_rights");
            let flag = |key: &str| {
                rights
                    .and_then(|rights| rights.get(key))
                    .and_then(Value::as_bool)
            };
            out.insert(
                asin.to_owned(),
                Eligibility {
                    full_title: details.full_title(),
                    is_consumable: flag("is_consumable"),
                    is_consumable_indefinitely: flag("is_consumable_indefinitely"),
                    content_delivery_type: product
                        .get("content_delivery_type")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            );
        }
    }
    Ok(out)
}

/// Minutes → `12h 34m` / `45m`.
pub(crate) fn format_runtime(minutes: u64) -> String {
    if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowable_only_for_consumable_loans() {
        let case = |consumable, indefinitely| Eligibility {
            full_title: "t".into(),
            is_consumable: consumable,
            is_consumable_indefinitely: indefinitely,
            content_delivery_type: None,
        };
        // Consumable + not-indefinitely = a loan → borrowable.
        assert!(case(Some(true), Some(false)).is_borrowable());
        // Consumable + indefinitely = already a purchase → not "borrowable".
        assert!(!case(Some(true), Some(true)).is_borrowable());
        // Not consumable = buy-only.
        assert!(!case(Some(false), Some(false)).is_borrowable());
        assert!(!case(None, None).is_borrowable());
    }

    #[test]
    fn format_runtime_splits_hours() {
        assert_eq!(format_runtime(45), "45m");
        assert_eq!(format_runtime(60), "1h 0m");
        assert_eq!(format_runtime(754), "12h 34m");
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
}
