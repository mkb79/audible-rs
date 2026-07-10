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
//! adds the account's **all-time** listening total for that store. Sums are
//! milliseconds; `interval_identifier` is `YYYY-MM` (monthly) or `YYYY`
//! (yearly). Read-only.

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

/// Fetches one store's rows (months or years) plus its all-time total.
async fn fetch_store(client: &Client, marketplace: &str, span: Span) -> Result<StoreStats> {
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
    let window_total = rows.iter().map(|(_, minutes)| *minutes).sum();
    Ok(StoreStats {
        marketplace: marketplace.to_owned(),
        rows,
        window_total,
        all_time,
    })
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

/// `73` → `"1h 13m"`, `5` → `"5m"`.
fn hm(minutes: i64) -> String {
    let (hours, mins) = (minutes / 60, minutes % 60);
    if hours == 0 {
        format!("{mins}m")
    } else {
        format!("{hours}h {mins}m")
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
                    out.push_str(&format!("{interval}  {minutes:>5} min  {bar}\n"));
                }
            }
            out.push_str(&format!(
                "{}: {} in {label} · {} all-time\n\n",
                store.marketplace,
                hm(store.window_total),
                hm(store.all_time),
            ));
        }
        // A grand total only helps when more than one store was queried.
        if self.stores.len() > 1 {
            out.push_str(&format!(
                "all stores: {} in {label} · {} all-time",
                hm(self.window_total()),
                hm(self.all_time()),
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

    fn store(marketplace: &str, rows: &[(&str, i64)], all_time: i64) -> StoreStats {
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
        }
    }

    #[test]
    fn table_shows_sections_footer_all_time_and_grand_total() {
        let de = store(
            "de",
            &[("2026-03", 72), ("2026-04", 0), ("2026-06", 1)],
            7249,
        );
        let us = store("us", &[], 0);
        let table = Report {
            span: Span::Year(2026),
            stores: vec![de, us],
        }
        .to_table();
        assert!(table.contains("== de =="));
        assert!(table.contains("de: 1h 13m in 2026 · 120h 49m all-time"));
        assert!(table.contains("(no listening in this span)"));
        assert!(table.contains("all stores: 1h 13m in 2026 · 120h 49m all-time"));
        assert!(!table.contains("2026-04")); // zero interval not drawn
    }

    #[test]
    fn single_store_has_no_grand_total_line() {
        let table = Report {
            span: Span::Year(2026),
            stores: vec![store("de", &[("2026-03", 72)], 100)],
        }
        .to_table();
        assert!(!table.contains("all stores"));
    }

    #[test]
    fn json_reports_window_all_time_and_granularity() {
        let json = Report {
            span: Span::Since {
                from: 2015,
                to: 2026,
            },
            stores: vec![store("de", &[("2015", 1861), ("2016", 104)], 5000)],
        }
        .to_json();
        assert_eq!(json["granularity"], "yearly");
        assert_eq!(json["since"], 2015);
        assert_eq!(json["to"], 2026);
        assert_eq!(json["window_minutes"], 1965);
        assert_eq!(json["all_time_minutes"], 5000);
        assert_eq!(
            json["marketplaces"][0]["intervals"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
}
