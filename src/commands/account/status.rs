//! `audible account status` — membership/subscription overview (AUD-152).
//!
//! Reads `/1.0/account/information?...&source=Credit` per selected marketplace
//! and renders one row per membership. `source=Credit` is **required** — the
//! subscription block is omitted entirely without it (verified live). Only the
//! universally-valid response groups `subscription_details,customer_segment`
//! are requested: `subscription_details_rodizio`/`_channels` 400 on some
//! marketplaces (e.g. `.com`) and would fail the whole request, and the PII
//! groups (`customer_profile`, `directed_ids`) are deliberately never fetched.
//! The account has no credit balance field (verified live at balance 0; see
//! AUD-152), so credits are not shown.

use anyhow::Result;
use reqwest::Method;
use serde::Deserialize;

use crate::config::ctx::Ctx;
use crate::output::Output;

pub(super) async fn status(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    let mut rows = Vec::new();
    for marketplace in ctx.marketplaces()? {
        let info: AccountInfo = client
            .request(Method::GET, "/1.0/account/information")
            .country_code(&marketplace)
            .query("response_groups", "subscription_details,customer_segment")
            // Required: without `source` the subscription block is omitted
            // entirely (verified live). The app sends `source=Credit`.
            .query("source", "Credit")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let segment = info
            .customer_information
            .segment
            .active_segment
            .unwrap_or_default();
        for sub in info.customer_information.subscription.subscription_details {
            rows.push(sub.into_row(&marketplace, &segment));
        }
    }
    if rows.is_empty() {
        eprintln!("no active membership in the selected marketplace(s)");
        return Ok(());
    }
    ctx.print(&Output::table(
        vec![
            "mp",
            "plan",
            "status",
            "renews",
            "next bill",
            "ends",
            "since",
            "segment",
        ],
        rows,
    ));
    Ok(())
}

/// Partial model of `/1.0/account/information` — only the rendered fields.
/// serde ignores everything else (the buy-flow `offers[]` tree, the PII
/// groups we never request, identifiers).
#[derive(Debug, Deserialize)]
struct AccountInfo {
    #[serde(default)]
    customer_information: CustomerInformation,
}

#[derive(Debug, Default, Deserialize)]
struct CustomerInformation {
    #[serde(default)]
    segment: Segment,
    #[serde(default)]
    subscription: Subscription,
}

#[derive(Debug, Default, Deserialize)]
struct Segment {
    #[serde(default)]
    active_segment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Subscription {
    #[serde(default)]
    subscription_details: Vec<SubscriptionDetail>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionDetail {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    auto_renew_enabled: Option<bool>,
    #[serde(default)]
    subscription_start_date: Option<String>,
    #[serde(default)]
    expected_status_end_date: Option<String>,
    #[serde(default)]
    next_bill_date: Option<String>,
    #[serde(default)]
    next_bill_amount: Option<Money>,
    #[serde(default)]
    plan: Option<Plan>,
}

#[derive(Debug, Deserialize)]
struct Money {
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    membership_plan_details: Option<PlanDetails>,
}

#[derive(Debug, Deserialize)]
struct PlanDetails {
    #[serde(default)]
    title: Option<String>,
}

impl SubscriptionDetail {
    fn into_row(self, marketplace: &str, segment: &str) -> Vec<String> {
        let dash = || "-".to_owned();
        let plan = self
            .plan
            .and_then(|plan| plan.membership_plan_details)
            .and_then(|details| details.title)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(dash);
        let renews = match self.auto_renew_enabled {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        };
        // A cancelled membership has no next bill (the date, if any, equals the
        // end date); show it only when it will actually renew.
        let next_bill = if self.auto_renew_enabled == Some(true) {
            match (self.next_bill_date.as_deref(), &self.next_bill_amount) {
                (Some(date), Some(amount)) if !amount.display().is_empty() => {
                    format!("{} {}", date_only(date), amount.display())
                }
                (Some(date), _) => date_only(date).to_owned(),
                (None, _) => dash(),
            }
        } else {
            dash()
        };
        let opt_date = |value: Option<String>| {
            value
                .as_deref()
                .map(date_only)
                .map(str::to_owned)
                .unwrap_or_else(dash)
        };
        vec![
            marketplace.to_owned(),
            plan,
            self.status.unwrap_or_else(dash),
            renews.to_owned(),
            next_bill,
            opt_date(self.expected_status_end_date),
            opt_date(self.subscription_start_date),
            if segment.is_empty() {
                dash()
            } else {
                segment.to_owned()
            },
        ]
    }
}

impl Money {
    fn display(&self) -> String {
        match (self.amount, &self.currency) {
            (Some(amount), Some(currency)) => format!("{amount:.2} {currency}"),
            (Some(amount), None) => format!("{amount:.2}"),
            _ => String::new(),
        }
    }
}

/// The date portion of an ISO-8601 timestamp (`2026-07-15T…` → `2026-07-15`).
fn date_only(timestamp: &str) -> &str {
    timestamp
        .split_once('T')
        .map_or(timestamp, |(date, _)| date)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic payload (no real account data) mirroring the live shape.
    const SAMPLE: &str = r#"{
      "customer_information": {
        "segment": {"active_segment": "CreditAL"},
        "subscription": {"subscription_details": [{
          "status": "Active",
          "auto_renew_enabled": false,
          "subscription_start_date": "2019-04-01T10:00:00.000Z",
          "expected_status_end_date": "2026-07-14T07:12:02.000Z",
          "next_bill_date": "2026-07-14T07:12:02.000Z",
          "next_bill_amount": {"amount": 9.949999809, "currency": "EUR"},
          "plan": {"membership_plan_details": {"title": "Audible-Abo"}},
          "offers": [{"ignored": true}]
        }]}
      }
    }"#;

    #[test]
    fn renders_a_cancelled_membership_row() {
        let info: AccountInfo = serde_json::from_str(SAMPLE).unwrap();
        let segment = info
            .customer_information
            .segment
            .active_segment
            .clone()
            .unwrap();
        let sub = info
            .customer_information
            .subscription
            .subscription_details
            .into_iter()
            .next()
            .unwrap();
        let row = sub.into_row("de", &segment);
        assert_eq!(
            row,
            vec![
                "de",
                "Audible-Abo",
                "Active",
                "no",         // auto_renew_enabled = false
                "-",          // cancelled → no next bill
                "2026-07-14", // ends (date only)
                "2019-04-01", // member since
                "CreditAL",
            ]
        );
    }

    #[test]
    fn renders_next_bill_only_when_renewing() {
        let mut info: AccountInfo = serde_json::from_str(SAMPLE).unwrap();
        info.customer_information.subscription.subscription_details[0].auto_renew_enabled =
            Some(true);
        let sub = info
            .customer_information
            .subscription
            .subscription_details
            .into_iter()
            .next()
            .unwrap();
        let row = sub.into_row("de", "CreditAL");
        assert_eq!(row[3], "yes");
        assert_eq!(row[4], "2026-07-14 9.95 EUR"); // amount rounded to 2 dp
    }

    #[test]
    fn tolerates_missing_fields() {
        let info: AccountInfo =
            serde_json::from_str(r#"{"customer_information": {"subscription": {}}}"#).unwrap();
        assert!(
            info.customer_information
                .subscription
                .subscription_details
                .is_empty()
        );
    }
}
