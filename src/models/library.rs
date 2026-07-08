//! Library item payload helpers, ported from the Python reference
//! branch `feature/db-library` (normalize/full-title/soft-delete
//! decisions). Pure functions over JSON values.

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
    match item.get("subtitle").and_then(Value::as_str).map(str::trim) {
        Some(subtitle) if !subtitle.is_empty() => Some(format!("{title}: {subtitle}")),
        _ => Some(title.to_owned()),
    }
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
/// products (`content_delivery_type` PodcastParent/Periodical or
/// `content_type` Podcast). MultiPartBooks also have children (audio
/// parts) but are NOT podcasts.
pub fn is_parent_podcast(item: &Value) -> bool {
    matches!(
        item.get("content_delivery_type").and_then(Value::as_str),
        Some("PodcastParent" | "Periodical")
    ) || item.get("content_type").and_then(Value::as_str) == Some("Podcast")
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
    product
        .get("relationships")
        .and_then(Value::as_array)
        .map(|relationships| {
            relationships
                .iter()
                .filter(|rel| {
                    rel.get("relationship_to_product").and_then(Value::as_str) == Some("child")
                        && matches!(
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
        })
        .unwrap_or_default()
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
