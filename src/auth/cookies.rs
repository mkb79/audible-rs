//! Website-cookie exchange (archived architecture §7): build the
//! `ap/exchangetoken/cookies` request and parse its response. Pure helpers —
//! the HTTP call and write-back live in [`crate::api::client::Client`].
//!
//! Reference: mkb79/Audible `src/audible/auth.py::refresh_website_cookies`.
//! Cookies are stored multi-domain and **non-destructively** (an exchange for
//! one marketplace keeps the other domains' cookies), so survival across an
//! exchange can be verified empirically.

use std::collections::BTreeMap;

use super::{AccountOrigin, Cookie, default_cookie_path};

/// The host family for the exchange: pre-merger Audible accounts use the
/// `audible` domain, regular Amazon identities use `amazon`.
pub(crate) fn exchange_target(origin: AccountOrigin) -> &'static str {
    match origin {
        AccountOrigin::AudibleLegacy => "audible",
        AccountOrigin::Amazon => "amazon",
    }
}

/// The form body for `POST https://www.{target}.{domain}/ap/exchangetoken/cookies`.
pub(crate) fn exchange_body(
    refresh_token: &str,
    target: &str,
    cookies_domain: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("app_name", "Audible".to_owned()),
        ("app_version", "3.56.2".to_owned()),
        ("source_token", refresh_token.to_owned()),
        ("source_token_type", "refresh_token".to_owned()),
        ("requested_token_type", "auth_cookies".to_owned()),
        ("domain", format!(".{target}.{cookies_domain}")),
    ]
}

/// Parses `response.tokens.cookies` (`{ domain: [ {Name, Value} ] }`) into
/// cookie buckets keyed by domain. `Value` is wrapped in quotes by the server
/// and stripped here.
pub(crate) fn parse_exchange_response(
    payload: &serde_json::Value,
) -> BTreeMap<String, Vec<Cookie>> {
    let mut out: BTreeMap<String, Vec<Cookie>> = BTreeMap::new();
    let Some(by_domain) = payload
        .get("response")
        .and_then(|r| r.get("tokens"))
        .and_then(|t| t.get("cookies"))
        .and_then(|c| c.as_object())
    else {
        return out;
    };
    for (domain, list) in by_domain {
        let Some(array) = list.as_array() else {
            continue;
        };
        let cookies: Vec<Cookie> = array
            .iter()
            .filter_map(|entry| {
                let name = entry.get("Name")?.as_str()?.to_owned();
                let value = entry.get("Value")?.as_str()?.trim_matches('"').to_owned();
                let path = entry
                    .get("Path")
                    .and_then(|v| v.as_str())
                    .map_or_else(default_cookie_path, str::to_owned);
                let secure = entry
                    .get("Secure")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let expires = entry
                    .get("Expires")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let http_only = entry.get("HttpOnly").and_then(|v| v.as_bool());
                Some(Cookie {
                    name,
                    value,
                    path,
                    secure,
                    expires,
                    http_only,
                })
            })
            .collect();
        if !cookies.is_empty() {
            out.insert(domain.clone(), cookies);
        }
    }
    out
}

/// True when `host` is within the cookie `domain` (e.g. host `www.amazon.de`
/// is within `.amazon.de`). Scopes cookie sending to the issuing host.
pub(crate) fn host_matches_domain(host: &str, domain: &str) -> bool {
    let bare = domain.trim_start_matches('.');
    host == bare || host.ends_with(domain)
}

/// The marketplace TLD of an Amazon/Audible host: the part after the
/// `audible.`/`amazon.` label (`www.amazon.co.uk` → `co.uk`, `api.audible.de`
/// → `de`, `www.amazon.com.au` → `com.au`). `None` for hosts of neither
/// family — those can't be mapped to a marketplace for a cookie exchange.
/// Pair with [`crate::api::locale::find_by_domain`] to get the country code.
pub(crate) fn marketplace_domain_from_host(host: &str) -> Option<&str> {
    host.split_once("audible.")
        .or_else(|| host.split_once("amazon."))
        .map(|(_, tld)| tld)
        .filter(|tld| !tld.is_empty())
}

/// Fallback session lifetime (30 days, in seconds) used when an exchange
/// response omits `tokens.ttl` (older API variants): the freshly exchanged
/// cookies still get a finite expiry so they aren't re-exchanged every request.
pub(crate) const DEFAULT_TTL_SECS: i64 = 2_592_000;

/// The exchange's authoritative session lifetime: `response.tokens.ttl`
/// (seconds). Present on a cookie exchange, absent on a registration.
pub(crate) fn parse_ttl(payload: &serde_json::Value) -> Option<i64> {
    payload
        .get("response")?
        .get("tokens")?
        .get("ttl")
        .and_then(serde_json::Value::as_i64)
}

/// The Amazon↔Audible SSO sibling of `host` (`www.audible.de` ↔
/// `www.amazon.de`), or `None` for any other host. The exchange only ever
/// returns `.amazon.<tld>` cookies, but those session cookies are SSO-valid
/// on the matching `.audible.<tld>` host (and vice versa for pre-merger
/// Audible accounts) — so cookie matching treats the two as one host within
/// the same marketplace, without ever sending to an unrelated host.
pub(crate) fn sso_sibling_host(host: &str) -> Option<String> {
    if host.contains("audible") {
        Some(host.replacen("audible", "amazon", 1))
    } else if host.contains("amazon") {
        Some(host.replacen("amazon", "audible", 1))
    } else {
        None
    }
}

/// True when a cookie `expires` value is in the past. The format mirrors what
/// the exchange returns (`"11 Jun 2046 20:06:35 GMT"`). Unparseable values are
/// treated as not expired, so a cookie is dropped only when provably stale.
pub(crate) fn is_expired(expires: &str) -> bool {
    let format = time::macros::format_description!(
        "[day padding:none] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    match time::PrimitiveDateTime::parse(expires, format) {
        Ok(parsed) => parsed.assume_utc() < time::OffsetDateTime::now_utc(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_per_origin() {
        assert_eq!(exchange_target(AccountOrigin::Amazon), "amazon");
        assert_eq!(exchange_target(AccountOrigin::AudibleLegacy), "audible");
    }

    #[test]
    fn body_has_expected_fields() {
        let body = exchange_body("RT", "amazon", "de");
        assert!(body.contains(&("source_token", "RT".to_owned())));
        assert!(body.contains(&("requested_token_type", "auth_cookies".to_owned())));
        assert!(body.contains(&("source_token_type", "refresh_token".to_owned())));
        assert!(body.contains(&("domain", ".amazon.de".to_owned())));
    }

    #[test]
    fn parses_all_fields_multi_domain() {
        let payload = serde_json::json!({
            "response": {"tokens": {"cookies": {
                ".amazon.de": [
                    {"Name": "session-id", "Value": "\"123\"", "Path": "/",
                     "Secure": true, "HttpOnly": false,
                     "Expires": "11 Jun 2046 20:06:35 GMT"},
                    {"Name": "ubid", "Value": "\"x\""}
                ],
                ".amazon.com": [
                    {"Name": "session-id", "Value": "\"999\""}
                ]
            }}}
        });
        let parsed = parse_exchange_response(&payload);
        assert_eq!(parsed.len(), 2);
        let de = &parsed[".amazon.de"];
        assert_eq!(de.len(), 2);
        assert_eq!(de[0].name, "session-id");
        assert_eq!(de[0].value, "123"); // surrounding quotes stripped
        assert_eq!(de[0].path, "/");
        assert!(de[0].secure);
        assert_eq!(de[0].http_only, Some(false));
        assert_eq!(de[0].expires.as_deref(), Some("11 Jun 2046 20:06:35 GMT"));
        // absent fields fall back to defaults
        assert_eq!(de[1].path, "/");
        assert!(de[1].secure);
        assert_eq!(de[1].http_only, None);
        assert_eq!(de[1].expires, None);
        assert_eq!(parsed[".amazon.com"][0].value, "999");
    }

    #[test]
    fn missing_cookies_yields_empty() {
        assert!(parse_exchange_response(&serde_json::json!({})).is_empty());
        assert!(parse_exchange_response(&serde_json::json!({"response": {}})).is_empty());
    }

    #[test]
    fn host_matching() {
        assert!(host_matches_domain("www.amazon.de", ".amazon.de"));
        assert!(host_matches_domain("amazon.de", ".amazon.de"));
        assert!(!host_matches_domain("www.amazon.com", ".amazon.de"));
        assert!(!host_matches_domain("evil.example", ".amazon.de"));
    }

    #[test]
    fn sso_sibling() {
        assert_eq!(
            sso_sibling_host("www.audible.de").as_deref(),
            Some("www.amazon.de")
        );
        assert_eq!(
            sso_sibling_host("www.amazon.co.uk").as_deref(),
            Some("www.audible.co.uk")
        );
        assert_eq!(sso_sibling_host("www.example.com"), None);
    }

    #[test]
    fn expiry_check() {
        assert!(!is_expired("11 Jun 2046 20:06:35 GMT")); // far future
        assert!(is_expired("11 Jun 2000 20:06:35 GMT")); // past
        assert!(!is_expired("not a date")); // unparseable → kept
    }

    #[test]
    fn marketplace_domain_extraction() {
        assert_eq!(marketplace_domain_from_host("www.amazon.de"), Some("de"));
        assert_eq!(marketplace_domain_from_host("api.audible.de"), Some("de"));
        assert_eq!(
            marketplace_domain_from_host("www.amazon.co.uk"),
            Some("co.uk")
        );
        assert_eq!(
            marketplace_domain_from_host("www.audible.com.au"),
            Some("com.au")
        );
        assert_eq!(marketplace_domain_from_host("www.example.com"), None);
    }

    #[test]
    fn parses_tokens_ttl() {
        let payload = serde_json::json!({"response": {"tokens": {"ttl": 2_592_000}}});
        assert_eq!(parse_ttl(&payload), Some(2_592_000));
        assert_eq!(
            parse_ttl(&serde_json::json!({"response": {"tokens": {}}})),
            None
        );
        assert_eq!(parse_ttl(&serde_json::json!({})), None);
    }
}
