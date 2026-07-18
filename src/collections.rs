//! The collections wire (`/1.0/collections`), capture-proven against the
//! official app (2026-07-06, `__WISHLIST` and `__ARCHIVE`): body-level
//! `continuation_token` pagination on `GET …/items` (no `Continuation-Token`
//! header — the library paginator does not apply), and the batched
//! `DELETE …/items?asins=…&asins=…&state_token=…` (repeated params, like
//! `/library`). Lists are server-authoritative — nothing is cached or
//! persisted, and the mutation `state_token` encodes *server* state,
//! fetched fresh per run (unlike the library delta-sync token, which
//! encodes our local sync position and therefore lives in the DB). The
//! command bodies on top live in `commands::collections`.

use std::collections::HashSet;

use reqwest::Method;
use serde_json::Value;

use crate::api::client::{ApiError, Client};

/// Errors of the collections wire calls.
#[derive(Debug, thiserror::Error)]
pub enum CollectionsError {
    /// The API request failed.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The HTTP layer failed (error status or body decoding).
    #[error("collections request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A mutation needs the collection's fresh state token, but the
    /// response carried none.
    #[error("the collection response carries no state_token")]
    MissingStateToken,
}

/// Server-side id of the permanent wishlist collection.
pub const WISHLIST_ID: &str = "__WISHLIST";

/// Server-side id of the permanent archive collection (titles hidden from
/// the library view).
pub const ARCHIVE_ID: &str = "__ARCHIVE";

/// One entry of a collection's item list.
pub struct CollectionItem {
    pub asin: String,
    /// `addition_date` (ISO timestamp), if reported.
    pub added: Option<String>,
}

/// Fetches every item of a collection, following the **body**
/// `continuation_token` (this endpoint does not use the library's
/// `Continuation-Token` header).
pub async fn collection_items(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
) -> Result<Vec<CollectionItem>, CollectionsError> {
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
pub struct CollectionMeta {
    pub state_token: String,
}

pub async fn collection_meta(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
) -> Result<CollectionMeta, CollectionsError> {
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
        .ok_or(CollectionsError::MissingStateToken)?
        .to_owned();
    Ok(CollectionMeta { state_token })
}

/// Removes `asins` from `__ARCHIVE` if present, returning the ASINs that
/// were actually in the archive (and thus removed). Used by
/// `library return` to clear a returned loan's archive membership so it
/// disappears from `collections archive list`, not just the library
/// (AUD-171). A no-op — no request — when none of the ASINs are archived.
pub async fn remove_from_archive(
    client: &Client,
    marketplace: &str,
    asins: &[String],
) -> Result<Vec<String>, CollectionsError> {
    let archived: HashSet<String> = collection_items(client, marketplace, ARCHIVE_ID)
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
    delete_collection_items(client, marketplace, ARCHIVE_ID, &targets).await?;
    Ok(targets)
}

/// The batched collection-item POST: fetch the collection's fresh
/// `state_token` and add `asins` to it, returning the server's
/// `num_items_added` (`None` when it did not report one). The wire home
/// for `collections … add` (audit 2026-07-18, E1: the only raw
/// `/1.0/collections` call left in the CLI layer); the iceboxed user
/// collections (AUD-109) reuse it.
pub async fn add_collection_items(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
    asins: &[String],
) -> Result<Option<u64>, CollectionsError> {
    let meta = collection_meta(client, marketplace, collection_id).await?;
    let reply: Value = client
        .request(
            Method::POST,
            format!("/1.0/collections/{collection_id}/items"),
        )
        .country_code(marketplace)
        .body(serde_json::json!({
            "collection_id": collection_id,
            "asins": asins,
            "state_token": meta.state_token,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(reply.get("num_items_added").and_then(Value::as_u64))
}

/// The batched collection-item DELETE (audit 2026-07-17, D6): fetch the
/// collection's `state_token` and issue `DELETE …/items` with repeated
/// `asins=` params. One home for `collections … remove` and the archive
/// removal used by `library add` (re-adding a title unarchives it).
pub async fn delete_collection_items(
    client: &Client,
    marketplace: &str,
    collection_id: &str,
    asins: &[String],
) -> Result<(), CollectionsError> {
    let meta = collection_meta(client, marketplace, collection_id).await?;
    let mut request = client
        .request(
            Method::DELETE,
            format!("/1.0/collections/{collection_id}/items"),
        )
        .country_code(marketplace)
        .query("state_token", &meta.state_token);
    for asin in asins {
        request = request.query("asins", asin);
    }
    request.send().await?.error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
