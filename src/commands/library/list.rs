//! The read commands over the database: `list` (incl. `--missing` and
//! `--remote`), `search` and `export`.

use anyhow::Result;
use futures::TryStreamExt as _;
use reqwest::Method;

use crate::api::paginator;
use crate::config::ctx::Ctx;
use crate::db::{self};
use crate::models::library as model;

use super::*;

const BOOK_COLUMNS: [&str; 5] = ["asin", "title", "purchase_date", "runtime_min", "language"];

const MP_BOOK_COLUMNS: [&str; 6] = [
    "mp",
    "asin",
    "title",
    "purchase_date",
    "runtime_min",
    "language",
];

/// Renders book rows with a leading `mp` column (from each row's
/// marketplace).
fn mp_book_rows(books: &[crate::db::BookRow]) -> Vec<Vec<String>> {
    books
        .iter()
        .map(|book| {
            vec![
                book.marketplace.clone(),
                book.asin.clone(),
                book.full_title.clone(),
                book.purchase_date.clone().unwrap_or_default(),
                book.runtime_min.clone().unwrap_or_default(),
                book.language.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

pub(super) async fn list(ctx: &Ctx, limit: u32, page: u32) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let books = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.list_books(marketplaces.clone(), query_limit, offset)
            .await?
    };
    if books.is_empty() {
        // Count only on the empty path: page-end error or empty-DB hint.
        let total = db.count_active(marketplaces).await?;
        if page > 1 {
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        if total == 0 {
            eprintln!("library database is empty — run `audible library sync`");
        }
    }
    ctx.print(&crate::output::Output::table(
        MP_BOOK_COLUMNS.to_vec(),
        mp_book_rows(&books),
    ));
    Ok(())
}

/// `library list --missing[=KINDS]` — active items lacking a download
/// record of the given kinds, with the missing kinds per item. Archived
/// items are skipped unless `--include-archived` (AUD-110).
pub(super) async fn list_missing(
    ctx: &Ctx,
    kinds: Vec<String>,
    limit: u32,
    page: u32,
    include_archived: bool,
) -> Result<()> {
    // Expand `all` and normalize to the canonical kind order, deduped.
    let all = kinds.iter().any(|kind| kind == "all");
    let kinds: Vec<String> = crate::db::DOWNLOAD_KINDS
        .iter()
        .filter(|kind| all || kinds.iter().any(|k| k == *kind))
        .map(|kind| (*kind).to_owned())
        .collect();

    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let rows = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.books_missing_downloads(
            marketplaces.clone(),
            kinds.clone(),
            query_limit,
            offset,
            include_archived,
        )
        .await?
    };
    if rows.is_empty() {
        if page > 1 {
            let total = db
                .count_books_missing_downloads(marketplaces, kinds, include_archived)
                .await?;
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        eprintln!("no items lacking {} downloads", kinds.join("/"));
        return Ok(());
    }
    ctx.print(&crate::output::Output::table(
        vec!["mp", "asin", "title", "missing"],
        rows.iter()
            .map(|row| {
                vec![
                    row.marketplace.clone(),
                    row.asin.clone(),
                    row.full_title.clone(),
                    row.missing.clone(),
                ]
            })
            .collect(),
    ));
    Ok(())
}

/// `library list --leaving` — titles whose consumption right ends on a set
/// date (`customer_rights.is_consumable_until`), soonest first (AUD-153) —
/// the ones that will leave the library. Permanent titles have no such date
/// (or the `2099` sentinel) and are never listed. Does not depend on the
/// unstable `is_ayce` flag. Reuses `--limit`/`--page`.
pub(super) async fn list_leaving(ctx: &Ctx, limit: u32, page: u32) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let rows = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.books_leaving(marketplaces.clone(), query_limit, offset)
            .await?
    };
    if rows.is_empty() {
        if page > 1 {
            let total = db.count_books_leaving(marketplaces).await?;
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        eprintln!("no titles with a known access-end date (permanent titles are not shown)");
        return Ok(());
    }
    ctx.print(&crate::output::Output::table(
        vec!["mp", "asin", "title", "leaving"],
        rows.iter()
            .map(|row| {
                vec![
                    row.marketplace.clone(),
                    row.asin.clone(),
                    row.full_title.clone(),
                    row.leaving.clone(),
                ]
            })
            .collect(),
    ));
    Ok(())
}

/// `--remote`: lists straight from the API, bypassing the database.
/// Single-host: `-m` must select exactly one marketplace.
pub(super) async fn list_remote(ctx: &Ctx, limit: u32) -> Result<()> {
    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;
    let page_size = ctx.db_config()?.page_size.clamp(10, 1000).to_string();

    let stream = paginator::pages(|_continuation| {
        client
            .request(Method::GET, "/1.0/library")
            .country_code(&marketplace)
            .query("response_groups", "product_desc,product_attrs")
            .query("num_results", &page_size)
            .query("status", "Active")
    });
    futures::pin_mut!(stream);

    let mut rows = Vec::new();
    'pages: while let Some(page) = stream.try_next().await? {
        for item in model::normalize_items(&page.body) {
            let asin = item
                .get("asin")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let title = model::build_full_title(&item).unwrap_or_default();
            let field = |key: &str| {
                item.get(key)
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default()
            };
            rows.push(vec![
                asin,
                title,
                field("purchase_date"),
                field("runtime_length_min"),
                field("language"),
            ]);
            if rows.len() as u32 >= limit {
                break 'pages;
            }
        }
    }

    ctx.print(&crate::output::Output::table(BOOK_COLUMNS.to_vec(), rows));
    Ok(())
}

pub(super) async fn search(ctx: &Ctx, query: String, limit: u32, fts: bool) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let fts = fts && ctx.db_config()?.fts;
    // One query across the whole set: FTS gives a single global BM25
    // ranking; LIKE orders by title. Rows carry their marketplace.
    let books = db.search(marketplaces, query, limit, fts).await?;
    if books.is_empty() {
        eprintln!("no matches");
    }
    ctx.print(&crate::output::Output::table(
        MP_BOOK_COLUMNS.to_vec(),
        mp_book_rows(&books),
    ));
    Ok(())
}

pub(super) async fn export(ctx: &Ctx, csv: bool) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    if csv {
        let mut out =
            String::from("mp,asin,title,subtitle,full_title,purchase_date,runtime_min,language\n");
        for book in db.export_books(marketplaces).await? {
            let fields = [
                book.marketplace,
                book.asin,
                book.title,
                book.subtitle.unwrap_or_default(),
                book.full_title,
                book.purchase_date.unwrap_or_default(),
                book.runtime_min.unwrap_or_default(),
                book.language.unwrap_or_default(),
            ];
            out.push_str(&fields.map(|f| csv_field(&f)).join(","));
            out.push('\n');
        }
        print!("{out}");
        return Ok(());
    }

    // JSON export: the full item docs across the set. response_groups are
    // pinned identically per marketplace, so the first one labels the dump.
    let response_groups = db
        .ensure_sync_state(marketplaces[0].clone(), DEFAULT_RESPONSE_GROUPS.to_owned())
        .await?
        .response_groups;
    let items = db.export_docs(marketplaces.clone()).await?;
    ctx.print(&crate::output::Output::Json(serde_json::json!({
        "format": "audible-rs-library-export",
        "version": 1,
        "exported_utc": db::now_iso_utc(),
        "marketplaces": marketplaces,
        "response_groups": response_groups,
        "count": items.len(),
        "items": items,
    })));
    Ok(())
}

/// RFC-4180 field quoting: quote when the field contains a comma,
/// quote or newline; double embedded quotes.
fn csv_field(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_field_quoting() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }
}
