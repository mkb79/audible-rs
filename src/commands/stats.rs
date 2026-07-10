//! `audible stats` — listening statistics (AUD-155).
//!
//! Reads `/1.0/stats/aggregates` per selected marketplace. The stats are
//! **per store**: the host scopes them (verified live — the DE and US hosts
//! return different data for one account), so each marketplace gets its own
//! section and `-m all` queries one host per store.
//!
//! Two spans, mutually exclusive:
//! - `--year YYYY` (default: current year) → that year, **month by month**
//!   (`monthly_listening_interval_*`, capped at 12 months = one request).
//! - `--since YYYY` → every year up to now, **as yearly totals**
//!   (`yearly_listening_interval_*`, capped at 5 years per request, so the
//!   range is fetched in ≤5-year chunks, concurrently).
//!
//! Every request also asks for `response_groups=total_listening_stats`, which
//! adds the account's **all-time** listening total for that store; a parallel
//! `stats/status/finished` request gives the **all-time** count of finished
//! titles. Both are all-time and labelled as such in the footer (finished can
//! only be all-time: its per-title timestamps are unreliable — historical
//! finishes collapse onto a single bulk-migration date). Durations render as
//! `HH:MM`, gaining a `Nd ` day prefix past 24h. Sums are milliseconds;
//! `interval_identifier` is `YYYY-MM` (monthly) or `YYYY` (yearly). Read-only.

use anyhow::{Result, bail};
use clap::Arg;
use reqwest::Method;
use serde::Deserialize;

use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::output::{Output, OutputFormat};

/// `audible stats`.
pub struct StatsCommand;

#[async_trait::async_trait]
impl super::Command for StatsCommand {
    fn name(&self) -> &'static str {
        "stats"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Show listening statistics (time per month/year, per marketplace)")
            .arg(
                Arg::new("year")
                    .long("year")
                    .value_name("YYYY")
                    .value_parser(clap::value_parser!(u16).range(1995..=2100))
                    .conflicts_with("since")
                    .help("Single calendar year, month by month (default: the current year)"),
            )
            .arg(
                Arg::new("since")
                    .long("since")
                    .value_name("YYYY")
                    .value_parser(clap::value_parser!(u16).range(1995..=2100))
                    .help("Every year from YYYY up to now, as yearly totals"),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let now = current_year();
        let span = if let Some(&from) = matches.get_one::<u16>("since") {
            if from > now {
                bail!("--since {from} is in the future (this year is {now})");
            }
            Span::Since { from, to: now }
        } else {
            Span::Year(matches.get_one::<u16>("year").copied().unwrap_or(now))
        };
        stats(ctx, span).await
    }
}

fn current_year() -> u16 {
    time::OffsetDateTime::now_utc().year() as u16
}

/// What to report: a single year (monthly) or a range of years (yearly).
#[derive(Clone, Copy)]
enum Span {
    Year(u16),
    Since { from: u16, to: u16 },
}

impl Span {
    /// Human label for the span (`2026` or `2015–2026`).
    fn label(&self) -> String {
        match self {
            Span::Year(year) => year.to_string(),
            Span::Since { from, to } if from == to => from.to_string(),
            Span::Since { from, to } => format!("{from}–{to}"),
        }
    }
}

async fn stats(ctx: &Ctx, span: Span) -> Result<()> {
    let client = ctx.client().await?;
    let mut stores = Vec::new();
    for marketplace in ctx.marketplaces()? {
        stores.push(fetch_store(client, &marketplace, span).await?);
    }

    let report = Report { span, stores };
    match ctx.output_format() {
        OutputFormat::Json => ctx.print(&Output::Json(report.to_json())),
        OutputFormat::Plain => ctx.print(&Output::Text(report.to_plain())),
        OutputFormat::Table => ctx.print(&Output::Text(report.to_table())),
    }
    Ok(())
}

/// Fetches one store's stats — its intervals + all-time total, concurrently
/// with the finished-title count.
async fn fetch_store(client: &Client, marketplace: &str, span: Span) -> Result<StoreStats> {
    let ((rows, all_time), finished) = futures::try_join!(
        fetch_rows(client, marketplace, span),
        fetch_finished(client, marketplace),
    )?;
    let window_total = rows.iter().map(|(_, minutes)| *minutes).sum();
    Ok(StoreStats {
        marketplace: marketplace.to_owned(),
        rows,
        window_total,
        all_time,
        finished,
    })
}

/// The listening intervals (months or years) and all-time total for a store.
async fn fetch_rows(
    client: &Client,
    marketplace: &str,
    span: Span,
) -> Result<(Vec<(String, i64)>, i64)> {
    let (mut rows, all_time) = match span {
        Span::Year(year) => {
            let agg = fetch(client, marketplace, "monthly", format!("{year}-01"), 12).await?;
            let all_time = total_minutes(&agg);
            (to_rows(agg.aggregated_monthly_listening_stats), all_time)
        }
        Span::Since { from, to } => {
            // Yearly buckets cap at 5 per request → fetch the range in chunks,
            // concurrently.
            let requests = year_chunks(from, to)
                .into_iter()
                .map(|(start, len)| fetch(client, marketplace, "yearly", start.to_string(), len));
            let parts = futures::future::try_join_all(requests).await?;
            // Every chunk echoes the same all-time total; take the largest
            // (robust if a chunk omits it).
            let all_time = parts.iter().map(total_minutes).max().unwrap_or(0);
            let rows = parts
                .into_iter()
                .flat_map(|agg| to_rows(agg.aggregated_yearly_listening_stats))
                .collect();
            (rows, all_time)
        }
    };
    // The API's window can spill past the requested bound, so keep only
    // in-span intervals, then drop any duplicate a chunk boundary produced.
    match span {
        Span::Year(year) => {
            let prefix = format!("{year}-");
            rows.retain(|(label, _)| label.starts_with(&prefix));
        }
        Span::Since { from, to } => rows.retain(|(label, _)| {
            label
                .parse::<u16>()
                .map(|year| from <= year && year <= to)
                .unwrap_or(false)
        }),
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.dedup_by(|a, b| a.0 == b.0);
    Ok((rows, all_time))
}

/// All-time count of distinct titles marked finished on this store
/// (`stats/status/finished`; the list also carries un-marked entries, which
/// are excluded).
async fn fetch_finished(client: &Client, marketplace: &str) -> Result<i64> {
    let body: FinishedStatus = client
        .request(Method::GET, "/1.0/stats/status/finished")
        .country_code(marketplace)
        .query("start_date", "2001-01-01T00:00:00Z")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut finished = std::collections::HashSet::new();
    for item in body.mark_as_finished_status_list {
        if item.is_marked_as_finished
            && let Some(asin) = item.asin
        {
            finished.insert(asin);
        }
    }
    Ok(finished.len() as i64)
}

/// One `/1.0/stats/aggregates` request for the given interval family
/// (`monthly`/`yearly`), always asking for the all-time total group too.
async fn fetch(
    client: &Client,
    marketplace: &str,
    family: &str,
    start: String,
    duration: u16,
) -> Result<Aggregates> {
    let body: Aggregates = client
        .request(Method::GET, "/1.0/stats/aggregates")
        .country_code(marketplace)
        .query(
            format!("{family}_listening_interval_duration"),
            duration.to_string(),
        )
        .query(format!("{family}_listening_interval_start_date"), start)
        .query("response_groups", "total_listening_stats")
        .query("store", "Audible")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(body)
}

/// Splits `from..=to` into consecutive, non-overlapping `(start, duration)`
/// windows. The API's yearly `duration` is an **offset**: a request returns
/// `start ..= start + duration`, and `duration` is capped at 5, so each window
/// spans up to 6 years.
fn year_chunks(from: u16, to: u16) -> Vec<(u16, u16)> {
    let mut chunks = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + 5).min(to);
        chunks.push((start, end - start));
        start = end + 1;
    }
    chunks
}

/// Partial model of `/1.0/stats/aggregates`.
#[derive(Debug, Default, Deserialize)]
struct Aggregates {
    #[serde(default)]
    aggregated_monthly_listening_stats: Vec<IntervalAgg>,
    #[serde(default)]
    aggregated_yearly_listening_stats: Vec<IntervalAgg>,
    #[serde(default)]
    aggregated_total_listening_stats: Option<TotalAgg>,
}

#[derive(Debug, Deserialize)]
struct IntervalAgg {
    #[serde(default)]
    interval_identifier: Option<String>,
    #[serde(default)]
    aggregated_sum: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TotalAgg {
    #[serde(default)]
    aggregated_sum: Option<serde_json::Value>,
}

/// Partial model of `/1.0/stats/status/finished`.
#[derive(Debug, Default, Deserialize)]
struct FinishedStatus {
    #[serde(default)]
    mark_as_finished_status_list: Vec<FinishedItem>,
}

#[derive(Debug, Deserialize)]
struct FinishedItem {
    #[serde(default)]
    asin: Option<String>,
    #[serde(default)]
    is_marked_as_finished: bool,
}

/// Milliseconds (number, scientific notation, or numeric string) → whole
/// minutes, floored.
fn minutes_of(sum: &Option<serde_json::Value>) -> i64 {
    let ms = match sum {
        Some(serde_json::Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(text)) => text.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    (ms / 60_000.0) as i64
}

/// All-time listening minutes from the `total_listening_stats` group.
fn total_minutes(agg: &Aggregates) -> i64 {
    agg.aggregated_total_listening_stats
        .as_ref()
        .map(|total| minutes_of(&total.aggregated_sum))
        .unwrap_or(0)
}

/// `(label, minutes)` for every interval the API returned (empty ids dropped,
/// zero-minute intervals kept for JSON).
fn to_rows(items: Vec<IntervalAgg>) -> Vec<(String, i64)> {
    let mut rows = Vec::new();
    for item in items {
        let label = item.interval_identifier.unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        rows.push((label, minutes_of(&item.aggregated_sum)));
    }
    rows
}

/// One marketplace/store's listening for the span.
struct StoreStats {
    marketplace: String,
    /// Every interval returned, with its minutes (may be zero).
    rows: Vec<(String, i64)>,
    /// Sum over the queried span.
    window_total: i64,
    /// Lifetime total for the store (`total_listening_stats`).
    all_time: i64,
    /// Distinct titles marked finished on the store (all-time).
    finished: i64,
}

impl StoreStats {
    /// Intervals with actual listening, for the human/plain views.
    fn active(&self) -> impl Iterator<Item = &(String, i64)> {
        self.rows.iter().filter(|(_, minutes)| *minutes > 0)
    }
}

struct Report {
    span: Span,
    stores: Vec<StoreStats>,
}

/// Minutes → `HH:MM`, gaining a `Nd ` day prefix once it reaches a full day
/// (so the hours field stays 00–23): `06:39`, `23:59`, `1d 00:23`, `5d 00:49`.
fn dur(minutes: i64) -> String {
    let (days, rest) = (minutes / 1440, minutes % 1440);
    let (hours, mins) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}d {hours:02}:{mins:02}")
    } else {
        format!("{hours:02}:{mins:02}")
    }
}

const BAR_WIDTH: i64 = 24;

impl Report {
    fn window_total(&self) -> i64 {
        self.stores.iter().map(|store| store.window_total).sum()
    }

    fn all_time(&self) -> i64 {
        self.stores.iter().map(|store| store.all_time).sum()
    }

    fn finished(&self) -> i64 {
        self.stores.iter().map(|store| store.finished).sum()
    }

    fn to_table(&self) -> String {
        let label = self.span.label();
        let mut out = String::new();
        for store in &self.stores {
            out.push_str(&format!("== {} ==\n", store.marketplace));
            let peak = store
                .active()
                .map(|(_, minutes)| *minutes)
                .max()
                .unwrap_or(0);
            if peak == 0 {
                out.push_str("(no listening in this span)\n");
            } else {
                for (interval, minutes) in store.active() {
                    // Scale the bar to the store's busiest interval; a non-zero
                    // interval always shows at least one block.
                    let filled = ((*minutes * BAR_WIDTH + peak - 1) / peak).max(1);
                    let bar = "▇".repeat(filled as usize);
                    out.push_str(&format!("{interval}  {:>8}  {bar}\n", dur(*minutes)));
                }
            }
            out.push_str(&format!(
                "{}: {} in {label} · all-time: {} listened, {} finished\n\n",
                store.marketplace,
                dur(store.window_total),
                dur(store.all_time),
                store.finished,
            ));
        }
        // A grand total only helps when more than one store was queried.
        if self.stores.len() > 1 {
            out.push_str(&format!(
                "all stores: {} in {label} · all-time: {} listened, {} finished",
                dur(self.window_total()),
                dur(self.all_time()),
                self.finished(),
            ));
        } else {
            while out.ends_with('\n') {
                out.pop();
            }
        }
        out
    }

    fn to_plain(&self) -> String {
        let mut lines = Vec::new();
        for store in &self.stores {
            for (interval, minutes) in store.active() {
                lines.push(format!("{}\t{}\t{}", store.marketplace, interval, minutes));
            }
        }
        lines.join("\n")
    }

    fn to_json(&self) -> serde_json::Value {
        let granularity = match self.span {
            Span::Year(_) => "monthly",
            Span::Since { .. } => "yearly",
        };
        let marketplaces: Vec<serde_json::Value> = self
            .stores
            .iter()
            .map(|store| {
                let intervals: Vec<serde_json::Value> = store
                    .rows
                    .iter()
                    .map(|(interval, minutes)| {
                        serde_json::json!({"interval": interval, "minutes": minutes})
                    })
                    .collect();
                serde_json::json!({
                    "marketplace": store.marketplace,
                    "window_minutes": store.window_total,
                    "all_time_minutes": store.all_time,
                    "all_time_finished_titles": store.finished,
                    "intervals": intervals,
                })
            })
            .collect();
        let mut root = serde_json::json!({
            "granularity": granularity,
            "span": self.span.label(),
            "marketplaces": marketplaces,
            "window_minutes": self.window_total(),
            "all_time_minutes": self.all_time(),
            "all_time_finished_titles": self.finished(),
        });
        match self.span {
            Span::Year(year) => {
                root["year"] = year.into();
            }
            Span::Since { from, to } => {
                root["since"] = from.into();
                root["to"] = to.into();
            }
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_from_ms_number_scientific_and_string() {
        assert_eq!(minutes_of(&Some(serde_json::json!(4_320_000))), 72);
        assert_eq!(minutes_of(&Some(serde_json::json!(4.30522874E8))), 7175);
        assert_eq!(minutes_of(&Some(serde_json::json!("60000"))), 1);
        assert_eq!(minutes_of(&None), 0);
    }

    #[test]
    fn year_chunks_are_non_overlapping_offset_windows() {
        // duration is an offset (span = duration + 1), so a window covers up
        // to 6 years and the next window starts right after it — no overlap.
        assert_eq!(year_chunks(2026, 2026), vec![(2026, 0)]);
        assert_eq!(year_chunks(2015, 2026), vec![(2015, 5), (2021, 5)]);
        assert_eq!(
            year_chunks(2015, 2027),
            vec![(2015, 5), (2021, 5), (2027, 0)]
        );
    }

    #[test]
    fn to_rows_drops_empty_ids_keeps_zero_minutes() {
        let agg: Aggregates = serde_json::from_str(
            r#"{"aggregated_yearly_listening_stats":[
                {"interval_identifier":"2015","aggregated_sum":111660000},
                {"interval_identifier":"","aggregated_sum":5},
                {"interval_identifier":"2016","aggregated_sum":0}]}"#,
        )
        .unwrap();
        assert_eq!(
            to_rows(agg.aggregated_yearly_listening_stats),
            vec![("2015".to_owned(), 1861), ("2016".to_owned(), 0)]
        );
    }

    #[test]
    fn dur_formats_hh_mm_with_days_past_a_day() {
        assert_eq!(dur(2), "00:02");
        assert_eq!(dur(399), "06:39");
        assert_eq!(dur(1439), "23:59");
        assert_eq!(dur(1463), "1d 00:23"); // 24h 23m
        assert_eq!(dur(7249), "5d 00:49"); // all-time example
    }

    fn store(marketplace: &str, rows: &[(&str, i64)], all_time: i64, finished: i64) -> StoreStats {
        let rows: Vec<(String, i64)> = rows
            .iter()
            .map(|(label, minutes)| ((*label).to_owned(), *minutes))
            .collect();
        let window_total = rows.iter().map(|(_, minutes)| *minutes).sum();
        StoreStats {
            marketplace: marketplace.to_owned(),
            rows,
            window_total,
            all_time,
            finished,
        }
    }

    #[test]
    fn table_shows_sections_footer_all_time_finished_and_grand_total() {
        let de = store(
            "de",
            &[("2026-03", 72), ("2026-04", 0), ("2026-06", 1)],
            7249,
            24,
        );
        let us = store("us", &[], 0, 0);
        let table = Report {
            span: Span::Year(2026),
            stores: vec![de, us],
        }
        .to_table();
        assert!(table.contains("== de =="));
        assert!(table.contains("de: 01:13 in 2026 · all-time: 5d 00:49 listened, 24 finished"));
        assert!(table.contains("(no listening in this span)"));
        assert!(
            table.contains("all stores: 01:13 in 2026 · all-time: 5d 00:49 listened, 24 finished")
        );
        assert!(!table.contains("2026-04")); // zero interval not drawn
    }

    #[test]
    fn single_store_has_no_grand_total_line() {
        let table = Report {
            span: Span::Year(2026),
            stores: vec![store("de", &[("2026-03", 72)], 100, 3)],
        }
        .to_table();
        assert!(!table.contains("all stores"));
    }

    #[test]
    fn json_reports_window_all_time_finished_and_granularity() {
        let json = Report {
            span: Span::Since {
                from: 2015,
                to: 2026,
            },
            stores: vec![store("de", &[("2015", 1861), ("2016", 104)], 5000, 24)],
        }
        .to_json();
        assert_eq!(json["granularity"], "yearly");
        assert_eq!(json["since"], 2015);
        assert_eq!(json["to"], 2026);
        assert_eq!(json["window_minutes"], 1965);
        assert_eq!(json["all_time_minutes"], 5000);
        assert_eq!(json["all_time_finished_titles"], 24);
        assert_eq!(json["marketplaces"][0]["all_time_finished_titles"], 24);
        assert_eq!(
            json["marketplaces"][0]["intervals"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}
