//! Interactive catalog title resolution: turns `--title` queries into ASINs
//! via the search half in [`crate::catalog`], keeping the multi-select
//! picker and its progress chatter with the CLI — used by `collections`
//! (wishlist/archive) and `library` (borrow/return).

use std::collections::HashSet;

use anyhow::Result;

use crate::api::client::Client;
use crate::catalog::{CatalogHit, catalog_search, verbatim_title_shortlist};

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
                let labels: Vec<String> = many.iter().map(hit_label).collect();
                let selection = crate::commands::prompt::pick_many(
                    "Catalog matches",
                    "catalog titles",
                    query,
                    &labels,
                    "; pass --asin or run interactively",
                )?;
                interacted = true;
                for index in selection {
                    push(many[index].asin.clone());
                }
            }
        }
    }
    if interacted {
        eprintln!();
    }
    Ok(result)
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
    use crate::catalog::CatalogDetails;

    #[test]
    fn format_runtime_splits_hours() {
        assert_eq!(format_runtime(45), "45m");
        assert_eq!(format_runtime(60), "1h 0m");
        assert_eq!(format_runtime(754), "12h 34m");
    }

    #[test]
    fn hit_labels_carry_display_fields() {
        let hit = CatalogHit {
            asin: "B0B".to_owned(),
            details: CatalogDetails {
                title: "Echo der Hoffnung".to_owned(),
                subtitle: Some("Outlander 7".to_owned()),
                publication: Some("Outlander [German Edition]".to_owned()),
                authors: "Diana Gabaldon".to_owned(),
                runtime_min: Some(3255),
            },
        };
        // The publication is shown when the full title does not carry it…
        assert_eq!(
            hit_label(&hit),
            "B0B  Echo der Hoffnung: Outlander 7  [Outlander [German Edition] · Diana Gabaldon · 54h 15m]"
        );

        // …and missing extras degrade gracefully.
        let bare = CatalogHit {
            asin: "B0A".to_owned(),
            details: CatalogDetails {
                title: "Der Hobbit".to_owned(),
                subtitle: None,
                publication: None,
                authors: String::new(),
                runtime_min: None,
            },
        };
        assert_eq!(hit_label(&bare), "B0A  Der Hobbit");

        // A publication already contained in the full title is not repeated.
        let contained = CatalogHit {
            asin: "B0C".to_owned(),
            details: CatalogDetails {
                title: "Outlander".to_owned(),
                subtitle: Some("Outlander, Book 1".to_owned()),
                publication: Some("Outlander".to_owned()),
                authors: String::new(),
                runtime_min: None,
            },
        };
        assert_eq!(hit_label(&contained), "B0C  Outlander: Outlander, Book 1");
    }
}
