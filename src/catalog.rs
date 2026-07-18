//! Catalog (`/1.0/catalog/products`) domain: batched product fetches with
//! hollow-echo filtering, ranked title search, per-ASIN display enrichment,
//! and subscription eligibility (`customer_rights`). The network counterpart
//! to the local-library resolver in `commands::items`. The interactive title
//! picker on top of [`catalog_search`] stays with the CLI in
//! `commands::catalog`.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::Value;

use crate::api::client::{ApiError, Client};

/// Errors of the catalog domain calls.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The API request failed.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The HTTP layer failed (error status or body decoding).
    #[error("catalog request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// ASINs per `/1.0/catalog/products` batch (server limit).
const CATALOG_BATCH: usize = 50;

/// Results requested per catalog title search.
const CATALOG_SEARCH_RESULTS: u32 = 50;

/// One batched `GET /1.0/catalog/products` sweep — the single home of the
/// catalog batch protocol (audit 2026-07-17, D2). Its rules are server
/// invariants and silent-failure traps, so nobody reimplements them:
///
/// - at most [`CATALOG_BATCH`] ASINs per request (51 → HTTP 400);
/// - the `asins` param is **one comma-joined value** — repeated `asins=`
///   params return 200 with a single product, no error;
/// - an ASIN the catalog has never heard of is echoed back as a hollow
///   product carrying **only** that ASIN (verified live 2026-07-17, with
///   and without title-bearing response groups). Presence proves nothing;
///   hollow echoes are dropped before any caller sees them.
///
/// Returns the real products; order across chunks is unspecified when
/// `concurrency > 1` (callers key by ASIN). `image_sizes` is appended
/// only when given.
pub async fn products_batched(
    client: &Client,
    marketplace: &str,
    asins: &[String],
    response_groups: &str,
    image_sizes: Option<&str>,
    concurrency: usize,
) -> Result<Vec<Value>, CatalogError> {
    use futures::{StreamExt as _, TryStreamExt as _};
    // Owned chunks: borrowing them from `asins` trips rustc's
    // "Send is not general enough" limitation under buffer_unordered.
    let chunks: Vec<Vec<String>> = asins
        .chunks(CATALOG_BATCH)
        .map(<[String]>::to_vec)
        .collect();
    let parts: Vec<Vec<Value>> = futures::stream::iter(chunks)
        .map(|chunk| async move {
            let mut request = client
                .request(Method::GET, "/1.0/catalog/products")
                .country_code(marketplace)
                .query("asins", chunk.join(","))
                .query("response_groups", response_groups);
            if let Some(sizes) = image_sizes {
                request = request.query("image_sizes", sizes);
            }
            let mut body: Value = request.send().await?.error_for_status()?.json().await?;
            let products = match body.get_mut("products").map(Value::take) {
                Some(Value::Array(products)) => products,
                _ => Vec::new(),
            };
            Ok::<_, CatalogError>(products.into_iter().filter(is_real_product).collect())
        })
        .buffer_unordered(concurrency.max(1))
        .try_collect()
        .await?;
    Ok(parts.into_iter().flatten().collect())
}

/// A real catalog product carries fields beyond its `asin`; a hollow echo
/// (unknown ASIN, see [`products_batched`]) carries only the `asin` key.
fn is_real_product(product: &Value) -> bool {
    product.get("asin").and_then(Value::as_str).is_some()
        && product
            .as_object()
            .is_some_and(|fields| fields.keys().any(|key| key != "asin"))
}

/// Catalog display data for one ASIN.
pub struct CatalogDetails {
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
    pub fn full_title(&self) -> String {
        crate::models::library::join_title_subtitle(&self.title, self.subtitle.as_deref())
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
pub struct CatalogHit {
    pub asin: String,
    pub details: CatalogDetails,
}

/// One catalog title search, results in server ranking order.
pub async fn catalog_search(
    client: &Client,
    marketplace: &str,
    query: &str,
) -> Result<Vec<CatalogHit>, CatalogError> {
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
pub fn verbatim_title_shortlist(hits: Vec<CatalogHit>, query: &str) -> Vec<CatalogHit> {
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
pub async fn catalog_details(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, CatalogDetails>, CatalogError> {
    let mut details = BTreeMap::new();
    let products = products_batched(
        client,
        marketplace,
        asins,
        "product_desc,product_attrs,contributors",
        None,
        1,
    )
    .await?;
    for product in &products {
        if let Some((asin, detail)) = parse_product(product) {
            details.insert(asin, detail);
        }
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
pub async fn documents(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, Value>, CatalogError> {
    let mut docs = BTreeMap::new();
    let products = products_batched(
        client,
        marketplace,
        asins,
        "product_desc,product_attrs,product_extended_attrs,customer_rights",
        None,
        1,
    )
    .await?;
    for product in products {
        let Some(asin) = product
            .get("asin")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        // On top of the protocol-level hollow-echo drop: the naming engine
        // needs a title, so a title-less document is useless here.
        let titled = product
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|title| !title.trim().is_empty());
        if titled {
            docs.insert(asin, product);
        }
    }
    Ok(docs)
}

/// Subscription eligibility for one ASIN, from the **authenticated**
/// catalog doc's `customer_rights` (absent/null when unauthenticated).
pub struct Eligibility {
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
    /// The shared taxonomy kind (`book`/`podcast`/`episode`), classified
    /// by the one classifier [`crate::models::library::item_kind`] —
    /// selects the membership wording (an audiobook is "borrowed", a show
    /// "followed", an episode "added") and the `--kind` guard. The wire
    /// call is the same `PUT /1.0/library/item` regardless.
    pub kind: &'static str,
}

impl Eligibility {
    /// Whether the account can borrow this title now: consumable but not
    /// kept indefinitely (a loan, not a purchase). `false` for buy-only
    /// titles (`is_consumable == false/None`).
    pub fn is_borrowable(&self) -> bool {
        self.is_consumable == Some(true) && self.is_consumable_indefinitely != Some(true)
    }
}

/// Fetches subscription eligibility for `asins` from the authenticated
/// catalog, batched 50 per request. Marketplace-relative — the caller
/// must use the same marketplace it intends to borrow on.
pub async fn eligibility(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<BTreeMap<String, Eligibility>, CatalogError> {
    let mut out = BTreeMap::new();
    let products = products_batched(
        client,
        marketplace,
        asins,
        "product_desc,customer_rights",
        None,
        1,
    )
    .await?;
    for product in &products {
        let Some(asin) = product.get("asin").and_then(Value::as_str) else {
            continue;
        };
        let (_, details) = match parse_product(product) {
            Some(parsed) => parsed,
            None => continue,
        };
        out.insert(
            asin.to_owned(),
            Eligibility {
                full_title: details.full_title(),
                // The `customer_rights` readers live in the models layer —
                // the doc shape is shared with library items (AUD-104).
                is_consumable: crate::models::library::is_consumable(product),
                is_consumable_indefinitely: crate::models::library::is_consumable_indefinitely(
                    product,
                ),
                kind: crate::models::library::item_kind(product),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hollow-echo shape is a live-verified server behavior
    /// (2026-07-17): an unknown ASIN comes back as `{"asin": "…"}` and
    /// nothing else, in every response-group combination probed — while a
    /// real product always carries more (even `relationships_v2`-only
    /// responses add `sku`/`sku_lite`).
    #[test]
    fn hollow_echoes_are_not_real_products() {
        assert!(!is_real_product(&serde_json::json!({"asin": "B0EXAMPLE1"})));
        assert!(!is_real_product(&serde_json::json!({"title": "no asin"})));
        assert!(is_real_product(
            &serde_json::json!({"asin": "B0EXAMPLE1", "title": "Example"})
        ));
        assert!(is_real_product(
            &serde_json::json!({"asin": "B0EXAMPLE1", "sku": "X", "sku_lite": "Y"})
        ));
    }

    #[test]
    fn borrowable_only_for_consumable_loans() {
        let case = |consumable, indefinitely| Eligibility {
            full_title: "t".into(),
            is_consumable: consumable,
            is_consumable_indefinitely: indefinitely,
            kind: "book",
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
    fn parses_catalog_details_with_authors() {
        let product = serde_json::json!(
            {"asin": "B0A", "title": "Book One",
             "authors": [{"name": "Alice"}, {"name": "Bob"}],
             "runtime_length_min": 754}
        );
        let (asin, one) = parse_product(&product).unwrap();
        assert_eq!(asin, "B0A");
        assert_eq!(one.title, "Book One");
        assert_eq!(one.authors, "Alice, Bob");
        assert_eq!(one.runtime_min, Some(754));
    }

    #[test]
    fn catalog_hits_parse_in_server_order() {
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
        assert_eq!(hits[1].asin, "B0A");
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
