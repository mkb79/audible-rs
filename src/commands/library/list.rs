//! The read commands over the database: `list` (incl. `--missing` and
//! `--remote`), `search` and `export`.

use anyhow::Result;
use futures::TryStreamExt as _;
use reqwest::Method;

use crate::api::paginator;
use crate::config::ctx::Ctx;
use crate::db::{self};
use crate::library_sync::{DEFAULT_RESPONSE_GROUPS, maybe_auto_sync_for_reads};
use crate::models::library as model;

const BOOK_COLUMNS: [&str; 5] = ["asin", "title", "purchase_date", "runtime_min", "language"];

const MP_BOOK_COLUMNS: [&str; 7] = [
    "mp",
    "asin",
    "kind",
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
                book.kind.clone(),
                book.full_title.clone(),
                book.purchase_date.clone().unwrap_or_default(),
                book.runtime_min.clone().unwrap_or_default(),
                book.language.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

pub(super) async fn list(ctx: &Ctx, kinds: Vec<String>, limit: u32, page: u32) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let books = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.list_books(marketplaces.clone(), kinds.clone(), query_limit, offset)
            .await?
    };
    if books.is_empty() {
        // Count only on the empty path: page-end error or empty-DB hint.
        let total = db.count_books(marketplaces, kinds.clone()).await?;
        if page > 1 {
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        if total == 0 {
            if kinds.is_empty() {
                eprintln!("library database is empty — run `audible library sync`");
            } else {
                eprintln!("no items of kind {}", kinds.join("/"));
                // `list` shows library memberships (the `items` table). A
                // followed show's episodes live in `episodes` and surface here
                // only when subscribed to individually, so `--kind episode` is
                // almost always empty — point at what actually lists them
                // (AUD-205).
                if kinds.iter().any(|kind| kind == "episode") {
                    eprintln!(
                        "a followed show's episodes are not library items — \
                         list them with `audible library episodes <SHOW>`"
                    );
                }
            }
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
    item_kinds: Vec<String>,
    limit: u32,
    page: u32,
    include_archived: bool,
) -> Result<()> {
    // Expand `all` and normalize to the canonical kind order, deduped.
    let kinds = crate::db::normalize_download_kinds(&kinds);

    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let rows = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.books_missing_downloads(
            marketplaces.clone(),
            kinds.clone(),
            item_kinds.clone(),
            query_limit,
            offset,
            include_archived,
            // Lapsed titles are never "missing" — they cannot be downloaded
            // (AUD-104; skipped like archived ones, which is also silent).
            false,
        )
        .await?
    };
    if rows.is_empty() {
        if page > 1 {
            let total = db
                .count_books_missing_downloads(
                    marketplaces,
                    kinds,
                    item_kinds,
                    include_archived,
                    false,
                )
                .await?;
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        eprintln!("no items lacking {} downloads", kinds.join("/"));
        // Falls through: `-o table` renders nothing for an empty table, while
        // `-o json` still yields `[]` for consumers.
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

/// `library list --borrowed` — titles the user did not purchase (access via
/// a subscription/grant), i.e. `origin_type != Purchase` (AUD-153). Owned
/// titles are never listed. Splits the item's plans into `eligible` (the ones
/// you can play through) and `not_eligible` (other plans the title is in that
/// you're not on — e.g. a promo, or the membership plan you'd need once a
/// subscription lapses; the generic AccessViaMusic route is omitted). Ordered
/// by title. Uses `origin_type`, not the unstable `is_ayce` flag. Reuses
/// `--limit`/`--page`.
///
/// In the human table an empty `eligible` shows as `none` (nothing plays it
/// right now) and an empty `not_eligible` as `—`; `-o json`/`plain` keep the
/// empty string so consumers can test emptiness.
pub(super) async fn list_borrowed(
    ctx: &Ctx,
    item_kinds: Vec<String>,
    limit: u32,
    page: u32,
) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let (query_limit, offset) = crate::commands::page_window(limit, page);
    let rows = if limit == 0 && page > 1 {
        Vec::new() // page 1 holds everything; no need to query
    } else {
        db.books_borrowed(
            marketplaces.clone(),
            item_kinds.clone(),
            query_limit,
            offset,
        )
        .await?
    };
    if rows.is_empty() {
        if page > 1 {
            let total = db.count_books_borrowed(marketplaces, item_kinds).await?;
            return Err(crate::commands::empty_page_error(page, limit, total));
        }
        eprintln!("no borrowed titles — every title in your library is owned");
        return Ok(());
    }
    // Placeholders for empty cells — only in the human table; JSON and plain
    // keep the empty string so callers can still test for "none".
    let human = matches!(ctx.output_format(), crate::output::OutputFormat::Table);
    let placeholder = |value: &str, empty: &str| match (human, value.is_empty()) {
        (true, true) => empty.to_string(),
        _ => value.to_owned(),
    };
    ctx.print(&crate::output::Output::table(
        vec!["mp", "asin", "title", "eligible", "not_eligible"],
        rows.iter()
            .map(|row| {
                vec![
                    row.marketplace.clone(),
                    row.asin.clone(),
                    row.full_title.clone(),
                    placeholder(&row.eligible, "none"),
                    placeholder(&row.not_eligible, "—"),
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

pub(super) async fn search(
    ctx: &Ctx,
    kinds: Vec<String>,
    query: String,
    limit: u32,
    fts: bool,
) -> Result<()> {
    // An empty query reached FTS5 as `MATCH ''` and errored with a raw
    // "fts5: syntax error" (audit 2026-07-18, A7).
    if query.trim().is_empty() {
        anyhow::bail!("the search query is empty");
    }
    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    let fts = fts && ctx.db_config()?.fts;
    // One query across the whole set: FTS gives a single global BM25
    // ranking; LIKE orders by title. Rows carry their marketplace.
    let books = db.search(marketplaces, kinds, query, limit, fts).await?;
    if books.is_empty() {
        eprintln!("no matches");
    }
    ctx.print(&crate::output::Output::table(
        MP_BOOK_COLUMNS.to_vec(),
        mp_book_rows(&books),
    ));
    Ok(())
}

pub(super) async fn export(ctx: &Ctx, kinds: Vec<String>, csv: bool) -> Result<()> {
    let db = ctx.open_library_db().await?;
    maybe_auto_sync_for_reads(ctx, &db).await?;
    let marketplaces = ctx.marketplaces()?;

    if csv {
        let mut out = String::from(
            "mp,asin,kind,title,subtitle,full_title,purchase_date,runtime_min,language\n",
        );
        for book in db.export_books(marketplaces, kinds).await? {
            let fields = [
                book.marketplace,
                book.asin,
                book.kind,
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
        // Explicit CSV format on stdout (documented passthrough — the
        // `-o` format does not apply to a `--csv` dump).
        ctx.mark_raw_stdout();
        print!("{out}");
        return Ok(());
    }

    // JSON export: the full item docs across the set — filtered by the
    // shared --kind classification when set. response_groups are pinned
    // identically per marketplace, so the first one labels the dump.
    let response_groups = db
        .ensure_sync_state(marketplaces[0].clone(), DEFAULT_RESPONSE_GROUPS.to_owned())
        .await?
        .response_groups;
    let items: Vec<serde_json::Value> = db
        .export_docs(marketplaces.clone())
        .await?
        .into_iter()
        .filter(|doc| kinds.is_empty() || kinds.iter().any(|k| k == model::item_kind(doc)))
        .collect();
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
