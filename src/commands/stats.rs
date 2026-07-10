//! `audible stats` — listening statistics (AUD-155).
//!
//! Reads `/1.0/stats/aggregates` per selected marketplace. The stats are
//! **per store**: the host scopes them (verified live — the DE and US hosts
//! return different data for one account), so each marketplace gets its own
//! section and `-m all` queries one host per store. The server caps
//! `monthly_listening_interval_duration` at 12, so one calendar year is one
//! request; `aggregated_sum` is milliseconds, `interval_identifier` is
//! `YYYY-MM`.
//!
//! Read-only. Table mode prints per-store sections with a bar per month and a
//! total; `-o json` emits a structured report (empty months kept), `-o plain`
//! emits `marketplace<TAB>month<TAB>minutes` rows.

use anyhow::Result;
use clap::Arg;
use reqwest::Method;
use serde::Deserialize;

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
            .about("Show listening statistics (time per month, per marketplace)")
            .arg(
                Arg::new("year")
                    .long("year")
                    .value_name("YYYY")
                    .value_parser(clap::value_parser!(u16).range(1995..=2100))
                    .help("Calendar year to report (default: the current year)"),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let year = matches
            .get_one::<u16>("year")
            .copied()
            .unwrap_or_else(current_year);
        stats(ctx, year).await
    }
}

fn current_year() -> u16 {
    time::OffsetDateTime::now_utc().year() as u16
}

async fn stats(ctx: &Ctx, year: u16) -> Result<()> {
    let client = ctx.client().await?;
    let mut stores = Vec::new();
    for marketplace in ctx.marketplaces()? {
        let body: Aggregates = client
            .request(Method::GET, "/1.0/stats/aggregates")
            .country_code(&marketplace)
            // The server caps the window at 12 months; a calendar year is one call.
            .query("monthly_listening_interval_duration", "12")
            .query(
                "monthly_listening_interval_start_date",
                format!("{year}-01"),
            )
            .query("store", "Audible")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        stores.push(StoreStats::from_body(&marketplace, body));
    }

    let report = Report { year, stores };
    match ctx.output_format() {
        OutputFormat::Json => ctx.print(&Output::Json(report.to_json())),
        OutputFormat::Plain => ctx.print(&Output::Text(report.to_plain())),
        OutputFormat::Table => ctx.print(&Output::Text(report.to_table())),
    }
    Ok(())
}

/// Partial model of `/1.0/stats/aggregates` — only the monthly series.
#[derive(Debug, Default, Deserialize)]
struct Aggregates {
    #[serde(default)]
    aggregated_monthly_listening_stats: Vec<MonthAgg>,
}

#[derive(Debug, Deserialize)]
struct MonthAgg {
    #[serde(default)]
    interval_identifier: Option<String>,
    /// Listening sum in milliseconds; the API sends a number, but tolerate a
    /// stringified one too.
    #[serde(default)]
    aggregated_sum: Option<serde_json::Value>,
}

/// Milliseconds (number or numeric string) → whole minutes, floored.
fn minutes_of(sum: &Option<serde_json::Value>) -> i64 {
    let ms = match sum {
        Some(serde_json::Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(text)) => text.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    (ms / 60_000.0) as i64
}

/// One marketplace/store's monthly listening for the year.
struct StoreStats {
    marketplace: String,
    /// Every month the API returned, with its minutes (may be zero).
    months: Vec<(String, i64)>,
    total: i64,
}

impl StoreStats {
    fn from_body(marketplace: &str, body: Aggregates) -> Self {
        let mut months = Vec::new();
        let mut total = 0;
        for month in body.aggregated_monthly_listening_stats {
            let id = month.interval_identifier.unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            let minutes = minutes_of(&month.aggregated_sum);
            total += minutes;
            months.push((id, minutes));
        }
        Self {
            marketplace: marketplace.to_owned(),
            months,
            total,
        }
    }

    /// Months with actual listening, for the human/plain views.
    fn active_months(&self) -> impl Iterator<Item = &(String, i64)> {
        self.months.iter().filter(|(_, minutes)| *minutes > 0)
    }
}

struct Report {
    year: u16,
    stores: Vec<StoreStats>,
}

/// `123` → `"2h 3m"`.
fn hours_minutes(minutes: i64) -> String {
    format!("{}h {}m", minutes / 60, minutes % 60)
}

const BAR_WIDTH: i64 = 24;

impl Report {
    fn grand_total(&self) -> i64 {
        self.stores.iter().map(|store| store.total).sum()
    }

    fn to_table(&self) -> String {
        let mut out = String::new();
        for store in &self.stores {
            out.push_str(&format!("== {} ==\n", store.marketplace));
            let peak = store
                .active_months()
                .map(|(_, minutes)| *minutes)
                .max()
                .unwrap_or(0);
            if peak == 0 {
                out.push_str("(no listening on this store)\n");
            } else {
                for (month, minutes) in store.active_months() {
                    // Scale the bar to the store's busiest month; a non-zero
                    // month always shows at least one block.
                    let filled = ((*minutes * BAR_WIDTH + peak - 1) / peak).max(1);
                    let bar = "▇".repeat(filled as usize);
                    out.push_str(&format!("{month}  {minutes:>5} min  {bar}\n"));
                }
            }
            out.push_str(&format!(
                "{} total  {} min ({})\n\n",
                store.marketplace,
                store.total,
                hours_minutes(store.total),
            ));
        }
        // A grand total only helps when more than one store was queried.
        if self.stores.len() > 1 {
            let grand = self.grand_total();
            out.push_str(&format!(
                "all stores {}:  {} min ({})",
                self.year,
                grand,
                hours_minutes(grand),
            ));
        } else {
            // Drop the trailing blank line left by the single section.
            while out.ends_with('\n') {
                out.pop();
            }
        }
        out
    }

    fn to_plain(&self) -> String {
        let mut lines = Vec::new();
        for store in &self.stores {
            for (month, minutes) in store.active_months() {
                lines.push(format!("{}\t{}\t{}", store.marketplace, month, minutes));
            }
        }
        lines.join("\n")
    }

    fn to_json(&self) -> serde_json::Value {
        let marketplaces: Vec<serde_json::Value> = self
            .stores
            .iter()
            .map(|store| {
                let months: Vec<serde_json::Value> = store
                    .months
                    .iter()
                    .map(|(month, minutes)| serde_json::json!({"month": month, "minutes": minutes}))
                    .collect();
                serde_json::json!({
                    "marketplace": store.marketplace,
                    "total_minutes": store.total,
                    "months": months,
                })
            })
            .collect();
        serde_json::json!({
            "year": self.year,
            "marketplaces": marketplaces,
            "total_minutes": self.grand_total(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Aggregates {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn minutes_from_number_or_string_ms() {
        assert_eq!(minutes_of(&Some(serde_json::json!(4_320_000))), 72); // ms → 72 min
        assert_eq!(minutes_of(&Some(serde_json::json!("60000"))), 1);
        assert_eq!(minutes_of(&Some(serde_json::Value::Null)), 0);
        assert_eq!(minutes_of(&None), 0);
    }

    #[test]
    fn store_totals_and_active_months() {
        let store = StoreStats::from_body(
            "de",
            body(
                r#"{"aggregated_monthly_listening_stats":[
                    {"interval_identifier":"2026-03","aggregated_sum":4320000},
                    {"interval_identifier":"2026-04","aggregated_sum":0},
                    {"interval_identifier":"2026-06","aggregated_sum":60000}
                ]}"#,
            ),
        );
        assert_eq!(store.total, 73);
        assert_eq!(store.months.len(), 3); // all returned months kept
        let active: Vec<_> = store.active_months().cloned().collect();
        assert_eq!(
            active,
            vec![("2026-03".to_owned(), 72), ("2026-06".to_owned(), 1)]
        );
    }

    #[test]
    fn table_has_sections_totals_and_grand_total_only_for_multiple_stores() {
        let de = StoreStats::from_body(
            "de",
            body(
                r#"{"aggregated_monthly_listening_stats":[
                    {"interval_identifier":"2026-03","aggregated_sum":4320000}]}"#,
            ),
        );
        let us = StoreStats::from_body("us", body(r#"{"aggregated_monthly_listening_stats":[]}"#));

        // One store: section + total, no grand-total line.
        let single = Report {
            year: 2026,
            stores: vec![de],
        }
        .to_table();
        assert!(single.contains("== de =="));
        assert!(single.contains("de total  72 min (1h 12m)"));
        assert!(!single.contains("all stores"));

        // Two stores: the empty one is flagged, grand total appears.
        let de = StoreStats::from_body(
            "de",
            body(
                r#"{"aggregated_monthly_listening_stats":[
                    {"interval_identifier":"2026-03","aggregated_sum":4320000}]}"#,
            ),
        );
        let both = Report {
            year: 2026,
            stores: vec![de, us],
        }
        .to_table();
        assert!(both.contains("== us =="));
        assert!(both.contains("(no listening on this store)"));
        assert!(both.contains("all stores 2026:  72 min (1h 12m)"));
    }

    #[test]
    fn json_keeps_empty_months_and_sums_all_stores() {
        let de = StoreStats::from_body(
            "de",
            body(
                r#"{"aggregated_monthly_listening_stats":[
                    {"interval_identifier":"2026-03","aggregated_sum":4320000},
                    {"interval_identifier":"2026-04","aggregated_sum":0}]}"#,
            ),
        );
        let us = StoreStats::from_body(
            "us",
            body(
                r#"{"aggregated_monthly_listening_stats":[
                    {"interval_identifier":"2026-05","aggregated_sum":60000}]}"#,
            ),
        );
        let json = Report {
            year: 2026,
            stores: vec![de, us],
        }
        .to_json();
        assert_eq!(json["year"], 2026);
        assert_eq!(json["total_minutes"], 73);
        assert_eq!(json["marketplaces"][0]["marketplace"], "de");
        assert_eq!(json["marketplaces"][0]["total_minutes"], 72);
        // The zero month is preserved in JSON (dropped only from the views).
        assert_eq!(
            json["marketplaces"][0]["months"].as_array().unwrap().len(),
            2
        );
        assert_eq!(json["marketplaces"][1]["total_minutes"], 1);
    }
}
