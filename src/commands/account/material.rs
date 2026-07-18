//! Per-account credential material: `cookies`, `token`,
//! `activation-bytes` and the Widevine CDM (`widevine fetch|set`).

use anyhow::{Context as _, Result, bail};

use crate::activation::ActivationMethod;
use crate::auth::Authenticator;
use crate::config::ctx::Ctx;
use crate::config::write;

use super::*;

/// `account cookies status`: per-domain cookie status (no network).
pub(super) async fn cookies_status(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    let summary = client.cookie_summary().await;
    if summary.is_empty() {
        eprintln!("no website cookies stored for this account");
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let rows: Vec<Vec<String>> = summary
        .into_iter()
        .map(|(domain, total, expired, unknown, ttl)| {
            let status = if expired > 0 {
                if expired == total {
                    "expired".to_owned()
                } else {
                    format!("{expired} expired")
                }
            } else if unknown == total {
                "unknown".to_owned()
            } else if unknown > 0 {
                format!("valid ({unknown} unknown)")
            } else {
                "valid".to_owned()
            };
            vec![
                domain,
                total.to_string(),
                expired.to_string(),
                unknown.to_string(),
                status,
                session_cell(ttl, now),
            ]
        })
        .collect();
    ctx.print(&crate::output::Output::table(
        vec![
            "domain", "cookies", "expired", "unknown", "status", "session",
        ],
        rows,
    ));
    Ok(())
}

/// The `session` cell for `account cookies show`: the exchange-TTL end time
/// (UTC) with remaining validity, or a marker when there is no recorded TTL
/// (a registration/import → exchanges on next use) or it has lapsed.
fn session_cell(ttl_expiry: Option<f64>, now: f64) -> String {
    match ttl_expiry {
        None => "no ttl (exchanges next use)".to_owned(),
        Some(expiry) if now >= expiry => format!("{} (lapsed)", format_unix_utc(expiry)),
        Some(expiry) => format!(
            "{} (in {})",
            format_unix_utc(expiry),
            format_secs((expiry - now) as i64)
        ),
    }
}

/// Formats a Unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC`.
fn format_unix_utc(unix_secs: f64) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute] UTC");
    time::OffsetDateTime::from_unix_timestamp(unix_secs as i64)
        .ok()
        .and_then(|dt| dt.format(&format).ok())
        .unwrap_or_else(|| "?".to_owned())
}

/// `account cookies refresh`: fetches website cookies for each selected
/// marketplace (`-m`) and stores them alongside any existing domains.
pub(super) async fn cookies_refresh(ctx: &Ctx, show_response: bool) -> Result<()> {
    let marketplaces = ctx.marketplaces()?;
    let client = ctx.client().await?;
    for country_code in &marketplaces {
        let (domains, payload) = client.refresh_cookies(country_code).await?;
        // Status to stderr so stdout stays clean JSON when --show-response is used.
        if show_response {
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
        if domains.is_empty() {
            eprintln!("no cookies returned for {country_code:?}");
        } else {
            eprintln!(
                "refreshed website cookies for {country_code}: {}",
                domains.join(", ")
            );
        }
    }
    Ok(())
}

/// `account cookies remove`: removes one domain's cookies (`--domain`) or all
/// of them (`--all`).
pub(super) async fn cookies_remove(ctx: &Ctx, domain: Option<&str>) -> Result<()> {
    let client = ctx.client().await?;
    let removed = client.remove_cookies(domain).await?;
    if removed.is_empty() {
        eprintln!("no matching cookies to remove");
    } else {
        eprintln!("removed website cookies for: {}", removed.join(", "));
    }
    Ok(())
}

/// `account token status`: shows the access token's remaining validity
/// (no network). Token values are never printed — only presence and the
/// remaining time.
pub(super) async fn token_status(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    let status = client.token_status().await;
    let access = if status.has_access_token {
        "present"
    } else {
        "absent"
    };
    let expiry = match status.remaining_secs {
        Some(secs) if secs > 0 => format!("valid for {}", format_secs(secs)),
        Some(secs) => format!("expired {} ago", format_secs(-secs)),
        None => "-".to_owned(),
    };
    let refresh = if status.has_refresh_token {
        "present"
    } else {
        "absent"
    };
    ctx.print(&crate::output::Output::table(
        vec!["access_token", "expiry", "refresh_token"],
        vec![vec![access.to_owned(), expiry, refresh.to_owned()]],
    ));
    Ok(())
}

/// `account token refresh`: forces an access-token refresh via the refresh
/// token and writes it back.
pub(super) async fn token_refresh(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    client.force_refresh_access_token().await?;
    let status = client.token_status().await;
    match status.remaining_secs {
        Some(secs) if secs > 0 => {
            eprintln!("access token refreshed; valid for {}", format_secs(secs));
        }
        _ => eprintln!("access token refreshed"),
    }
    Ok(())
}

/// `account token remove`: removes only the access token (forces a refresh
/// on the next request). The refresh token is deliberately kept — it is
/// the account's lifeline (new access tokens, cookie exchange, and a
/// device deregistration all need it).
pub(super) async fn token_remove(ctx: &Ctx) -> Result<()> {
    let client = ctx.client().await?;
    client.clear_access_token().await?;
    eprintln!(
        "removed the stored access token; the next request refreshes it from the \
         refresh token (which is kept)"
    );
    Ok(())
}

pub(super) fn parse_activation_method(value: &str) -> Result<ActivationMethod, String> {
    value.parse()
}

/// `account activation-bytes`: without `--fetch`, prints the stored
/// activation bytes (or hints how to fetch them); with `--fetch`, fetches
/// them from Audible and stores them in the auth file. The value goes to
/// stdout (pipeable); status and hints go to stderr.
pub(super) async fn activation_bytes(
    ctx: &Ctx,
    fetch: bool,
    method: ActivationMethod,
) -> Result<()> {
    let mut auth = ctx.authenticator().await?;
    if fetch {
        let activation_bytes = crate::activation::fetch(&auth, method).await?;
        auth.set_activation_bytes(activation_bytes.clone());
        auth.save().await?;
        eprintln!("fetched activation bytes and saved them to the auth file");
        println!("{activation_bytes}");
    } else {
        match auth.activation_bytes() {
            Some(activation_bytes) => println!("{activation_bytes}"),
            None => eprintln!(
                "no activation bytes stored — fetch them with \
                 `audible account activation-bytes --fetch`"
            ),
        }
    }
    Ok(())
}

/// Formats a non-negative seconds count compactly (`45s`, `42m`, `3h 5m`,
/// `2d 4h`) for `account token status`.
fn format_secs(secs: i64) -> String {
    let secs = secs.max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Loads the account's authenticator, refusing non-Android registrations —
/// Widevine `drmlicense` is only granted to an Android device.
async fn ensure_android(ctx: &Ctx) -> Result<Authenticator> {
    let auth = ctx.authenticator().await?;
    let device_type = auth.device_type().unwrap_or("unknown");
    if device_type != ANDROID_DEVICE_TYPE {
        bail!(
            "Widevine needs an Android-registered device, but this account is registered as \
             {device_type:?}. Register an Android device with `account login --device android`."
        );
    }
    Ok(auth)
}

/// `account widevine fetch <URL>` — provision a CDM from a remote provider: sign
/// a minimal `account/information` request, POST it to the endpoint, save the
/// returned `.wvd` (0600) and point `widevine_cdm` at it.
pub(super) async fn widevine_fetch(ctx: &Ctx, url: &str) -> Result<()> {
    let account = ctx.account_name()?;
    let auth = ensure_android(ctx).await?;
    let signer = auth
        .signer()
        .cloned()
        .context("this account has no signing material")?;
    let api_url = format!(
        "https://api.audible.{}/1.0/account/information",
        auth.locale().domain
    );
    let signed = tokio::task::spawn_blocking(move || {
        signer.sign_request("GET", "/1.0/account/information", b"")
    })
    .await
    .expect("signing task must not panic");

    let wvd = crate::widevine::provider::fetch_wvd(url, &api_url, &signed).await?;
    crate::widevine::Device::from_wvd(&wvd).context("the provider did not return a valid .wvd")?;
    let path = ctx.config_dir().join(format!("{account}.wvd"));
    crate::fsutil::write_private(&path, &wvd)
        .with_context(|| format!("writing {}", path.display()))?;
    set_widevine_cdm(ctx, &account, &path)?;
    eprintln!(
        "fetched Widevine CDM ({} bytes) → {} and set widevine_cdm",
        wvd.len(),
        path.display()
    );
    Ok(())
}

/// `account widevine set <PATH>` — use an existing `.wvd` (BYO).
pub(super) async fn widevine_set(ctx: &Ctx, path: &str) -> Result<()> {
    let account = ctx.account_name()?;
    ensure_android(ctx).await?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    crate::widevine::Device::from_wvd(&bytes).context("not a valid .wvd file")?;
    set_widevine_cdm(ctx, &account, std::path::Path::new(path))?;
    eprintln!("set widevine_cdm = {path}");
    Ok(())
}

/// Writes `accounts.<name>.widevine_cdm` into the config.
fn set_widevine_cdm(ctx: &Ctx, account: &str, path: &std::path::Path) -> Result<()> {
    // `write::set` renders the raw string as a quoted TOML string — must not
    // be pre-quoted, or the path ends up wrapped in literal double-quotes.
    let key = format!("accounts.{account}.widevine_cdm");
    let value = path.display().to_string();
    write::edit_file(&ctx.config_file(), |content| {
        write::set(content, &key, &value)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cell_states() {
        // No recorded TTL → registration/import marker.
        assert_eq!(session_cell(None, 1_000.0), "no ttl (exchanges next use)");
        // TTL in the past → lapsed.
        assert!(session_cell(Some(500.0), 1_000.0).ends_with("(lapsed)"));
        // TTL in the future → end time + remaining.
        let cell = session_cell(Some(1_000.0 + 86_400.0), 1_000.0);
        assert!(cell.contains("(in 1d 0h)"), "{cell}");
    }

    #[test]
    fn format_unix_utc_is_iso_like() {
        // 2001-09-09 01:46:40 UTC, rendered to minute precision.
        assert_eq!(format_unix_utc(1_000_000_000.0), "2001-09-09 01:46 UTC");
    }
}
