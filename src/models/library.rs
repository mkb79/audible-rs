//! Library item payload helpers, ported from the Python reference
//! branch `feature/db-library` (normalize/full-title/soft-delete
//! decisions). Pure functions over JSON values.
//!
//! Named after the library item document, but several readers serve the
//! **shared product-doc shape** — authenticated catalog products carry
//! the same fields — and are deliberately called from `catalog` too
//! (`item_kind`, `join_title_subtitle`, the `customer_rights` readers;
//! AUD-104). The dependency always points here (`catalog → models`,
//! never back): this module imports nothing but `std` and `serde_json`.

use std::collections::BTreeSet;

use serde_json::Value;

/// Extracts the item list from a library payload (`{"items": [...]}` or
/// a bare array).
pub fn normalize_items(payload: &Value) -> Vec<Value> {
    if let Some(items) = payload.get("items").and_then(Value::as_array) {
        return items.clone();
    }
    payload.as_array().cloned().unwrap_or_default()
}

/// `Title: Subtitle` (subtitle optional); `None` when the title is
/// missing or empty.
pub fn build_full_title(item: &Value) -> Option<String> {
    let title = item.get("title")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    Some(join_title_subtitle(
        title,
        item.get("subtitle").and_then(Value::as_str),
    ))
}

/// The display-title rule (audit 2026-07-17, D6): `Title: Subtitle` when a
/// non-empty subtitle is present, else the title alone. One home for the
/// library-doc path ([`build_full_title`]) and the catalog struct
/// (`CatalogDetails::full_title`), which each spelled it out.
pub fn join_title_subtitle(title: &str, subtitle: Option<&str>) -> String {
    match subtitle.map(str::trim).filter(|s| !s.is_empty()) {
        Some(subtitle) => format!("{title}: {subtitle}"),
        None => title.to_owned(),
    }
}

/// The `(title, subtitle)` a DB upsert stores from a doc — one home for
/// the item and episode upsert paths in `library sync`, which extracted
/// the same two fields the same way (audit 2026-07-17, D6). `title` falls
/// back to the empty string; `subtitle` stays `None` when absent.
pub fn title_subtitle(item: &Value) -> (String, Option<String>) {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let subtitle = item
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (title, subtitle)
}

/// The **child** relationships of a catalog product (`relationship_to_
/// product == "child"`), for callers to filter by `relationship_type`
/// and project — one home for the series-child and episode-child walks
/// (audit 2026-07-17, D6), which shared this skeleton and differed only
/// in the type predicate and what they extracted.
pub fn child_relationships(product: &Value) -> impl Iterator<Item = &Value> {
    product
        .get("relationships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|rel| rel.get("relationship_to_product").and_then(Value::as_str) == Some("child"))
}

/// Whether an item should be soft-deleted: `status == "Revoked"`, with
/// the legacy fallback `library_status.is_visible == false`.
pub fn should_soft_delete(item: &Value) -> bool {
    match item.get("status").and_then(Value::as_str) {
        Some("Revoked") => return true,
        Some("Active") => return false,
        _ => {}
    }
    item.get("library_status")
        .and_then(|status| status.get("is_visible"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// One series membership from an item document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesEntry {
    /// ASIN of the series.
    pub asin: String,
    /// Series title.
    pub title: String,
    /// Position within the series — may be a range (`"1-6"`); `None`
    /// when empty or missing.
    pub sequence: Option<String>,
}

/// Extracts the series memberships of an item (possibly several).
pub fn extract_series(item: &Value) -> Vec<SeriesEntry> {
    item.get("series")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(SeriesEntry {
                        asin: entry.get("asin")?.as_str()?.to_owned(),
                        title: entry.get("title")?.as_str()?.to_owned(),
                        sequence: entry
                            .get("sequence")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an item is a podcast parent whose episodes are child
/// products: `content_delivery_type` PodcastParent/Periodical, with
/// `content_type == "Podcast"` as a defensive fallback for parents
/// without a delivery type. Episodes and seasons also carry
/// `content_type == "Podcast"` and are explicitly excluded (AUD-173) —
/// only shows get their episodes resolved. MultiPartBooks also have
/// children (audio parts, `component` relationships) but are NOT
/// podcasts (`content_type == "Product"`).
pub fn is_parent_podcast(item: &Value) -> bool {
    let delivery_type = item.get("content_delivery_type").and_then(Value::as_str);
    matches!(delivery_type, Some("PodcastParent" | "Periodical"))
        || (item.get("content_type").and_then(Value::as_str) == Some("Podcast")
            && !matches!(delivery_type, Some("PodcastEpisode" | "PodcastSeason")))
}

/// The content kinds of the shared `--kind` filter (AUD-173), in display
/// order. `book` is the catch-all for everything not recognized as a
/// podcast show or episode.
pub const ITEM_KINDS: &[&str] = &["book", "podcast", "episode"];

/// Classifies an item document for the shared `--kind` filter: `episode`
/// (a `PodcastEpisode`, e.g. an individually-subscribed one), `podcast`
/// (a show parent; `PodcastSeason` defensively counts as podcast, though
/// seasons have not been observed as library items), else `book`. The
/// SQL twin lives in [`crate::db`] (`v_books.kind`) — the two are kept in
/// lockstep by a functional test.
pub fn item_kind(item: &Value) -> &'static str {
    match item.get("content_delivery_type").and_then(Value::as_str) {
        Some("PodcastEpisode") => "episode",
        Some("PodcastSeason") => "podcast",
        _ if is_parent_podcast(item) => "podcast",
        _ => "book",
    }
}

/// Whether a document advertises a companion PDF: `is_pdf_url_available`,
/// with a `pdf_url`-presence fallback (a JSON `null` counts as absent).
/// `None` when the document carries neither field — the source cannot
/// know (catalog products hold no ownership data), so callers may probe.
///
/// This is the one Rust source of truth for the predicate; the SQL twin
/// lives in [`crate::db`] (`pdf_available_sql`) and the two are kept in
/// lockstep by a functional test there. Every consumer (`download`'s
/// pre-check, `download info`, the `--missing=pdf` gate) must answer the
/// same way — `download info` once read only the flag and reported
/// "no PDF" for titles `download --kind pdf` would happily fetch.
pub fn pdf_available(doc: &Value) -> Option<bool> {
    let flag = doc.get("is_pdf_url_available").and_then(Value::as_bool);
    let url_present = doc.get("pdf_url").map(|url| !url.is_null());
    match (flag, url_present) {
        (None, None) => None,
        (flag, url) => Some(flag.unwrap_or(false) || url.unwrap_or(false)),
    }
}

/// The account's right to consume this title, from the document's
/// `customer_rights.is_consumable`. The field lives in the shared
/// product-doc shape — library items and authenticated catalog products
/// alike — which is why this reader lives here and not in `catalog`
/// (AUD-104): the external-download check reads it off a catalog doc, the
/// lapsed-right pre-check off the stored library doc. `None` when absent
/// (e.g. an unauthenticated catalog doc — the field is the account's).
///
/// This is the one Rust source of truth for the right; the SQL twin lives
/// in [`crate::db`] (`consumable_sql`) and the two are kept in lockstep by
/// a functional test there. `download` treats anything not provably
/// consumable (`!= Some(true)`) as not downloadable — before the first
/// licenserequest.
pub fn is_consumable(doc: &Value) -> Option<bool> {
    doc.get("customer_rights")?.get("is_consumable")?.as_bool()
}

/// `customer_rights.is_consumable_indefinitely` — `true` = kept
/// indefinitely (a purchase), `false` = a lapsing right (subscription
/// loan). Same shared shape and `None` semantics as [`is_consumable`].
pub fn is_consumable_indefinitely(doc: &Value) -> Option<bool> {
    doc.get("customer_rights")?
        .get("is_consumable_indefinitely")?
        .as_bool()
}

/// `customer_rights.is_consumable_offline` — whether a download right
/// exists at all (`download info` reports it). Same shared shape and
/// `None` semantics as [`is_consumable`].
pub fn is_consumable_offline(doc: &Value) -> Option<bool> {
    doc.get("customer_rights")?
        .get("is_consumable_offline")?
        .as_bool()
}

/// Expands a series sequence into the volume numbers it covers:
/// `"3"` → `[3.0]`, `"1-6"` → `[1.0 … 6.0]` (omnibus editions),
/// empty/unparsable → `[]`.
pub fn sequence_numbers(sequence: &str) -> Vec<f64> {
    let sequence = sequence.trim();
    if let Some((start, end)) = sequence.split_once('-')
        && let (Ok(start), Ok(end)) = (start.trim().parse::<i64>(), end.trim().parse::<i64>())
        && start <= end
    {
        return (start..=end).map(|n| n as f64).collect();
    }
    sequence.parse::<f64>().map(|n| vec![n]).unwrap_or_default()
}

/// Child volumes of a series product (`relationships` response group):
/// `(sequence, asin)` for every child relationship. Live payloads carry
/// the volume number in `sequence` (possibly a range like `"1-6"` for
/// omnibus editions) — `sort` is only the display order and offset by
/// such editions, so it must NOT be used as the volume number.
pub fn extract_series_children(product: &Value) -> Vec<(Option<String>, String)> {
    child_relationships(product)
        .filter(|rel| {
            matches!(
                rel.get("relationship_type").and_then(Value::as_str),
                None | Some("series")
            )
        })
        .filter_map(|rel| {
            let asin = rel.get("asin")?.as_str()?.to_owned();
            let sequence = rel
                .get("sequence")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Some((sequence, asin))
        })
        .collect()
}

/// Collects removed/revoked ASINs from the payload's various shapes.
pub fn extract_removed_asins(payload: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for key in ["removed_asins", "deleted_asins", "revoked_asins"] {
        if let Some(asins) = payload.get(key).and_then(Value::as_array) {
            out.extend(asins.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    for key in ["deleted_items", "removed_items", "revoked_items"] {
        if let Some(items) = payload.get(key).and_then(Value::as_array) {
            out.extend(
                items
                    .iter()
                    .filter_map(|item| item.get("asin"))
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_title_handles_subtitle_and_missing_title() {
        assert_eq!(
            build_full_title(&json!({"title": "Der Hobbit", "subtitle": "oder Hin und zurück"})),
            Some("Der Hobbit: oder Hin und zurück".into())
        );
        assert_eq!(
            build_full_title(&json!({"title": " Buch ", "subtitle": "  "})),
            Some("Buch".into())
        );
        assert_eq!(build_full_title(&json!({"title": ""})), None);
        assert_eq!(build_full_title(&json!({})), None);
    }

    #[test]
    fn soft_delete_prefers_status_over_visibility() {
        assert!(should_soft_delete(&json!({"status": "Revoked"})));
        assert!(!should_soft_delete(
            &json!({"status": "Active", "library_status": {"is_visible": false}})
        ));
        assert!(should_soft_delete(
            &json!({"library_status": {"is_visible": false}})
        ));
        assert!(!should_soft_delete(&json!({})));
    }

    #[test]
    fn removed_asins_from_all_shapes() {
        let payload = json!({
            "removed_asins": ["A1"],
            "revoked_asins": ["A2", "A1"],
            "deleted_items": [{"asin": "A3"}, {}],
        });
        let asins = extract_removed_asins(&payload);
        assert_eq!(asins.len(), 3);
        assert!(asins.contains("A3"));
    }

    #[test]
    fn series_extraction_handles_multi_and_empty_sequences() {
        let item = json!({"series": [
            {"asin": "S1", "title": "Taschenbuch", "sequence": "1"},
            {"asin": "S2", "title": "Andromeda", "sequence": "1-6"},
            {"asin": "S3", "title": "Ohne Nummer", "sequence": ""},
            {"title": "kaputt ohne asin"},
        ]});
        let series = extract_series(&item);
        assert_eq!(series.len(), 3);
        assert_eq!(series[1].sequence.as_deref(), Some("1-6"));
        assert_eq!(series[2].sequence, None);
        assert!(extract_series(&json!({})).is_empty());
    }

    #[test]
    fn podcast_detection_excludes_multipart_books() {
        assert!(is_parent_podcast(
            &json!({"content_delivery_type": "PodcastParent"})
        ));
        assert!(is_parent_podcast(
            &json!({"content_delivery_type": "Periodical"})
        ));
        assert!(is_parent_podcast(&json!({"content_type": "Podcast"})));
        assert!(!is_parent_podcast(
            &json!({"content_delivery_type": "MultiPartBook"})
        ));
        assert!(!is_parent_podcast(&json!({})));
    }

    #[test]
    fn podcast_detection_excludes_episodes_and_seasons() {
        // Episodes and seasons also carry content_type Podcast, but only
        // shows get their episodes resolved (AUD-173).
        assert!(!is_parent_podcast(&json!({
            "content_delivery_type": "PodcastEpisode", "content_type": "Podcast"
        })));
        assert!(!is_parent_podcast(&json!({
            "content_delivery_type": "PodcastSeason", "content_type": "Podcast"
        })));
    }

    #[test]
    fn item_kind_classifies_the_taxonomy() {
        // Live-verified taxonomy (AUD-173): episodes and seasons carry
        // content_type Podcast too; MultiPartBook children are components,
        // not episodes, and the book itself stays a book.
        let kind = |doc: serde_json::Value| item_kind(&doc);
        assert_eq!(
            kind(json!({"content_delivery_type": "PodcastEpisode", "content_type": "Podcast"})),
            "episode"
        );
        assert_eq!(
            kind(json!({"content_delivery_type": "PodcastParent", "content_type": "Podcast"})),
            "podcast"
        );
        assert_eq!(
            kind(json!({"content_delivery_type": "Periodical", "content_type": "Show"})),
            "podcast"
        );
        assert_eq!(
            kind(json!({"content_delivery_type": "PodcastSeason", "content_type": "Podcast"})),
            "podcast"
        );
        // The defensive content_type fallback for parents without a
        // delivery type.
        assert_eq!(kind(json!({"content_type": "Podcast"})), "podcast");
        assert_eq!(
            kind(json!({"content_delivery_type": "SinglePartBook", "content_type": "Product"})),
            "book"
        );
        assert_eq!(
            kind(json!({"content_delivery_type": "MultiPartBook", "content_type": "Product"})),
            "book"
        );
        // book is the catch-all.
        assert_eq!(kind(json!({})), "book");
    }

    #[test]
    fn sequence_expansion() {
        assert_eq!(sequence_numbers("3"), vec![3.0]);
        assert_eq!(sequence_numbers("1-6"), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(sequence_numbers("0.5"), vec![0.5]);
        assert!(sequence_numbers("").is_empty());
        assert!(sequence_numbers("?").is_empty());
        assert!(sequence_numbers("6-1").is_empty());
    }

    #[test]
    fn series_children_from_relationships() {
        // Shape of a real series product: sequence carries the volume
        // number, sort is display order (offset by the omnibus).
        let product = json!({"relationships": [
            {"asin": "OMN", "relationship_to_product": "child", "relationship_type": "series",
             "sequence": "1-6", "sort": "1"},
            {"asin": "V1", "relationship_to_product": "child", "relationship_type": "series",
             "sequence": "1", "sort": "2"},
            {"asin": "V3", "relationship_to_product": "child"},
            {"asin": "P", "relationship_to_product": "parent", "sequence": "9"},
            {"asin": "E", "relationship_to_product": "child", "relationship_type": "episode"},
        ]});
        let children = extract_series_children(&product);
        assert_eq!(children.len(), 3);
        assert_eq!(children[0], (Some("1-6".into()), "OMN".into()));
        assert_eq!(children[1], (Some("1".into()), "V1".into()));
        assert_eq!(children[2], (None, "V3".into()));
    }

    #[test]
    fn normalize_accepts_object_and_array() {
        assert_eq!(normalize_items(&json!({"items": [{"asin": "A"}]})).len(), 1);
        assert_eq!(
            normalize_items(&json!([{"asin": "A"}, {"asin": "B"}])).len(),
            2
        );
        assert!(normalize_items(&json!({"other": 1})).is_empty());
    }
}
