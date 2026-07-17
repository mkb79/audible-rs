//! `library add` / `library remove` — library membership over the single
//! `PUT`/`DELETE /1.0/library/item` mechanism (AUD-171). One pair of
//! commands for every kind of membership Audible manages this way:
//! subscription (AYCL/Plus) audiobooks, podcast follows, and single podcast
//! episodes. Purchases go through the store, which we do not touch.
//!
//! `add` resolves `--title` against the **catalog** (candidates are not
//! local yet) and refuses buy-only titles up front via an authenticated
//! `customer_rights` eligibility check — mirroring the app's "included in
//! subscription". `remove` resolves `--title` against the **local library**
//! (memberships are already there), refuses purchases, and — matching the
//! app's proven order — first clears an item's `__ARCHIVE` membership, then
//! drops it from the library.
//!
//! The wire call is identical across kinds; only the wording differs (an
//! audiobook is "borrowed"/"returned", a podcast "followed"/"unfollowed", an
//! episode "added"/"removed"), driven by the shared taxonomy
//! (`models::library::item_kind`).

use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::{Value, json};

use crate::commands::{catalog, collections, items};
use crate::config::ctx::Ctx;

/// The membership kind, from the shared taxonomy — selects the wording
/// only; the wire call (`PUT`/`DELETE /1.0/library/item`) is the same.
#[derive(Clone, Copy)]
enum Kind {
    Audiobook,
    Podcast,
    Episode,
}

impl Kind {
    /// Maps the shared taxonomy (`models::library::item_kind`, the one
    /// classifier) onto the membership wording. This used to reimplement
    /// the classification and had drifted (audit 2026-07-17, C1):
    /// `PodcastSeason` and the `content_type == "Podcast"` fallback were
    /// missing, so a show without a delivery type was worded — and
    /// `--kind`-filtered — as an audiobook while every other command
    /// called it a podcast.
    fn from_item_kind(kind: &str) -> Kind {
        match kind {
            "podcast" => Kind::Podcast,
            "episode" => Kind::Episode,
            _ => Kind::Audiobook,
        }
    }

    /// The `--kind` filter value this kind matches (the shared
    /// book/podcast/episode vocabulary, AUD-173).
    fn as_filter(self) -> &'static str {
        match self {
            Kind::Audiobook => "book",
            Kind::Podcast => "podcast",
            Kind::Episode => "episode",
        }
    }

    /// Whether this kind passes a `--kind` filter (empty = all).
    fn passes(self, kinds: &[String]) -> bool {
        kinds.is_empty() || kinds.iter().any(|kind| kind == self.as_filter())
    }

    /// Confirmation after a successful add.
    fn added(self, named: &str) -> String {
        match self {
            Kind::Audiobook => format!("borrowed {named}"),
            Kind::Podcast => format!("now following {named}"),
            Kind::Episode => format!("added episode {named}"),
        }
    }

    /// Confirmation after a successful remove.
    fn removed(self, named: &str) -> String {
        match self {
            Kind::Audiobook => format!("returned {named}"),
            Kind::Podcast => format!("unfollowed {named}"),
            Kind::Episode => format!("removed episode {named}"),
        }
    }

    /// Reason we won't add an ineligible item (not consumable).
    fn unavailable(self, named: &str) -> String {
        match self {
            Kind::Audiobook => format!("{named} isn't available to borrow — it's purchase-only"),
            Kind::Podcast => format!("{named} isn't available to follow"),
            Kind::Episode => format!("{named} isn't available to add"),
        }
    }
}

/// `library add <ASIN…> | --title <QUERY>` — add subscription titles,
/// podcasts, or episodes to the library. `--title` resolves against the
/// catalog.
pub(super) async fn add(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    kinds: Vec<String>,
    sync: bool,
) -> Result<()> {
    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;
    let asins = catalog::resolve_catalog_titles(client, &marketplace, asins, titles).await?;
    if asins.is_empty() {
        bail!("nothing to add — pass --asin or --title");
    }

    // Authenticated eligibility (customer_rights) — refuse buy-only up
    // front instead of letting the PUT fail; plus the local library so an
    // already-held title is skipped rather than re-added.
    let eligibility = crate::catalog::eligibility(client, &marketplace, &asins).await?;
    let db = ctx.open_library_db().await?;

    let mut added = Vec::new();
    for asin in &asins {
        if db
            .item_doc(asin.clone(), marketplace.clone())
            .await?
            .is_some()
        {
            eprintln!("{asin} is already in your library; skipping");
            continue;
        }
        let entry = eligibility.get(asin);
        let title = entry.map(|e| e.full_title.as_str()).unwrap_or("");
        let named = if title.is_empty() {
            asin.clone()
        } else {
            format!("{asin} ({title})")
        };
        let kind = Kind::from_item_kind(entry.map(|e| e.kind).unwrap_or("book"));
        // The --kind guard: never add something outside the requested
        // kinds — applies to explicit --asin values too, no silent
        // substitution (AUD-173).
        if !kind.passes(&kinds) {
            eprintln!(
                "{named} is a {} — skipped by --kind {}",
                kind.as_filter(),
                kinds.join(",")
            );
            continue;
        }
        match entry {
            Some(e) if e.is_borrowable() => {
                client
                    .request(Method::PUT, "/1.0/library/item")
                    .country_code(&marketplace)
                    .body(json!({ "asin": asin }))
                    .send()
                    .await?
                    .error_for_status()?;
                eprintln!("{}", kind.added(&named));
                added.push(asin.clone());
            }
            Some(e) if e.is_consumable_indefinitely == Some(true) => {
                eprintln!("{named} is already owned — a purchase can't be added this way");
            }
            _ => {
                eprintln!("{}", kind.unavailable(&named));
            }
        }
    }

    if added.is_empty() {
        return Ok(());
    }
    reflect_present(ctx, sync, &marketplace, &added).await
}

/// `library remove <ASIN…> | --title <QUERY>` — remove subscription
/// titles, podcasts, or episodes from the library. `--title` resolves
/// against the local library. This is a server-side membership removal; the
/// local `db library remove` is a different, DB-only operation.
pub(super) async fn remove(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    kinds: Vec<String>,
    yes: bool,
) -> Result<()> {
    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;
    let db = ctx.open_library_db().await?;
    // No episode hits here: a child episode of a followed show is not an
    // own membership (individually-subscribed episodes are items rows and
    // are found either way, AUD-174).
    let asins = items::resolve_asins(
        &db,
        &marketplace,
        asins,
        titles,
        items::PodcastMode::ItemsOnly,
    )
    .await?;
    if asins.is_empty() {
        bail!("nothing to remove — pass --asin or --title");
    }

    // Only removable memberships: `is_removable` is the authoritative flag
    // (a purchase is not removable); `origin_type == Purchase` corroborates.
    let mut targets = Vec::new();
    for asin in &asins {
        let Some(doc) = db.item_doc(asin.clone(), marketplace.clone()).await? else {
            eprintln!("{asin} is not in your library; skipping");
            continue;
        };
        let doc: Value = serde_json::from_str(&doc).unwrap_or(Value::Null);
        let title = doc.get("title").and_then(Value::as_str).unwrap_or("");
        let named = if title.is_empty() {
            asin.clone()
        } else {
            format!("{asin} ({title})")
        };
        let removable = doc
            .get("is_removable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owned = doc.get("origin_type").and_then(Value::as_str) == Some("Purchase");
        if owned || !removable {
            eprintln!("{named} is a purchase — it can't be removed from the library");
            continue;
        }
        let kind = Kind::from_item_kind(crate::models::library::item_kind(&doc));
        // The --kind guard: never remove something outside the requested
        // kinds — applies to explicit --asin values too, no silent
        // substitution (AUD-173).
        if !kind.passes(&kinds) {
            eprintln!(
                "{named} is a {} — skipped by --kind {}",
                kind.as_filter(),
                kinds.join(",")
            );
            continue;
        }
        targets.push((asin.clone(), named, kind));
    }
    if targets.is_empty() {
        return Ok(());
    }

    let names: Vec<&str> = targets.iter().map(|(_, named, _)| named.as_str()).collect();
    let question = format!(
        "Remove {} item(s) from your library (downloaded files are kept): {}?",
        targets.len(),
        names.join(", ")
    );
    if !crate::commands::prompt::confirm(yes, &question)? {
        eprintln!("aborted");
        return Ok(());
    }

    // Un-archive first, then remove from the library — the order the app
    // uses. Checked once against the live archive rather than the local
    // `is_archived` flag (which trails a sync), so it holds even when an item
    // was archived elsewhere (app/web) since the last sync. A no-op when
    // nothing is archived. (The DELETE also works on an archived item and the
    // server clears the archive itself, so this is defensive/explicit.)
    let all: Vec<String> = targets.iter().map(|(asin, _, _)| asin.clone()).collect();
    let cleared = collections::remove_from_archive(client, &marketplace, &all).await?;
    for asin in &cleared {
        eprintln!("removed {asin} from the archive");
    }

    for (asin, named, kind) in &targets {
        client
            .request(
                Method::DELETE,
                format!("/1.0/library/item/{asin}/default_loan_ID"),
            )
            .country_code(&marketplace)
            .send()
            .await?
            .error_for_status()?;
        // Membership is gone the moment the server accepts the DELETE, so
        // drop it from the local library view right away. The delta
        // change-feed reflects a removal only minutes later (unlike an add,
        // which the server indexes within seconds), so waiting on a sync
        // would leave the item lingering in `library list`. Downloaded files,
        // `downloads` and `licenses` rows are kept.
        db.soft_delete_item(marketplace.clone(), asin.clone())
            .await?;
        eprintln!("{}", kind.removed(named));
    }
    Ok(())
}

/// After an add with `--sync`: run delta syncs until the new item is
/// present in the local library (shared poller — D4); without `--sync`,
/// point at the sync dependency. Remove needs no counterpart — it
/// soft-deletes locally.
async fn reflect_present(ctx: &Ctx, sync: bool, marketplace: &str, asins: &[String]) -> Result<()> {
    super::sync::poll_until_reflected(ctx, sync, marketplace, asins, "the change", |doc| {
        doc.is_some()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C1: membership classifies through the one taxonomy. The old local
    /// copy missed `PodcastSeason` and the `content_type == "Podcast"`
    /// fallback — a show without a delivery type was worded and
    /// `--kind`-filtered as an audiobook.
    #[test]
    fn membership_kind_follows_the_shared_taxonomy() {
        let kind_of = |doc: serde_json::Value| {
            Kind::from_item_kind(crate::models::library::item_kind(&doc)).as_filter()
        };
        assert_eq!(
            kind_of(serde_json::json!({"content_type": "Podcast"})),
            "podcast",
            "the content_type fallback must classify as podcast"
        );
        assert_eq!(
            kind_of(serde_json::json!({"content_delivery_type": "PodcastSeason"})),
            "podcast"
        );
        assert_eq!(
            kind_of(serde_json::json!({"content_delivery_type": "PodcastEpisode"})),
            "episode"
        );
        assert_eq!(
            kind_of(serde_json::json!({"content_delivery_type": "SinglePartBook"})),
            "book"
        );
    }
}
