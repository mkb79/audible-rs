//! Remote CDM provisioning (AUD-56f) — a swappable module.
//!
//! One method today: sign a minimal `GET /1.0/account/information` and POST
//! `{Url, Headers}` to a **user-provided** endpoint (the shape the community
//! `AudibleCdm` lambda expects). Nothing is hardcoded except this protocol
//! shape — the endpoint URL comes from the CLI. To swap in another provider
//! (e.g. a user_id-based one), add a function here; callers only use `fetch_wvd`.

use anyhow::{Result, bail};

use crate::auth::signing::SignedHeaders;

/// Fetches a `.wvd` from a remote provider: POSTs the signed account-proof
/// request `{Url, Headers}` to `endpoint` and returns the device blob bytes.
pub async fn fetch_wvd(endpoint: &str, api_url: &str, signed: &SignedHeaders) -> Result<Vec<u8>> {
    let body = serde_json::json!({
        "Url": api_url,
        "Headers": {
            "x-adp-token": signed.adp_token,
            "x-adp-alg": signed.alg,
            "x-adp-signature": signed.signature,
        }
    });
    // Timeouts match the API client (AUD-98) so an unreachable provider
    // endpoint fails fast instead of hanging.
    let response = reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .read_timeout(crate::api::client::READ_TIMEOUT)
        .build()?
        .post(endpoint)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes);
        let snippet: String = detail.chars().take(200).collect();
        bail!("CDM provider returned {status}: {snippet}");
    }
    Ok(bytes.to_vec())
}
