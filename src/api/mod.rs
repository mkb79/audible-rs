//! Audible API access: HTTP client with per-request `AuthMode`, locale
//! handling (host URL construction from the profile) and response
//! pagination as streams.

pub mod client;
pub mod locale;
pub mod paginator;

/// Normalizes an API path: a version-less path gets the `/1.0` prefix; an
/// explicit version segment (`1.0`, `2.0`, …) is left untouched. The query
/// string is preserved.
///
/// Lives in the API layer because both the CLI (`api` command) and the
/// broker's `POST /v1/api/request` normalize with exactly this rule — the
/// shared `/v1` router must never reach upward into the commands layer
/// for it (audit 2026-07-17, E1).
pub fn normalize_api_path(path: &str) -> String {
    let (path_part, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let trimmed = path_part.trim_start_matches('/');
    let first_segment = trimmed.split('/').next().unwrap_or("");
    // A version looks like "1.0": only digits and dots, and at least one dot
    // (so a resource named "123" is not mistaken for a version).
    let is_version = first_segment.contains('.')
        && first_segment
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.');
    let normalized = if is_version {
        format!("/{trimmed}")
    } else {
        format!("/1.0/{trimmed}")
    };
    match query {
        Some(q) => format!("{normalized}?{q}"),
        None => normalized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_default_version() {
        assert_eq!(normalize_api_path("/library"), "/1.0/library");
        assert_eq!(normalize_api_path("/wishlist"), "/1.0/wishlist");
        // Missing leading slash is tolerated.
        assert_eq!(normalize_api_path("library"), "/1.0/library");
    }

    #[test]
    fn normalize_keeps_explicit_version() {
        assert_eq!(normalize_api_path("/1.0/library"), "/1.0/library");
        assert_eq!(normalize_api_path("/2.0/foo"), "/2.0/foo");
    }

    #[test]
    fn normalize_preserves_query() {
        assert_eq!(
            normalize_api_path("/library?num_results=5"),
            "/1.0/library?num_results=5"
        );
        assert_eq!(
            normalize_api_path("/1.0/library?a=b&c=d"),
            "/1.0/library?a=b&c=d"
        );
    }
}
