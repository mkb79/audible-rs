//! Marketplace locales: mapping `country_code` to Audible/Amazon domains
//! and building request URLs from a profile's marketplace.
//!
//! The registry mirrors `LOCALE_TEMPLATES` from the Python reference
//! implementation (`audible.localization`).

/// A single Audible marketplace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Locale {
    /// Two-letter marketplace code used in profiles (`de`, `us`, …).
    pub country_code: &'static str,
    /// Top-level domain of the marketplace (`de`, `com`, `co.uk`, …).
    pub domain: &'static str,
    /// Amazon marketplace identifier.
    pub market_place_id: &'static str,
}

/// All known marketplaces.
pub const LOCALES: [Locale; 11] = [
    Locale {
        country_code: "de",
        domain: "de",
        market_place_id: "AN7V1F1VY261K",
    },
    Locale {
        country_code: "us",
        domain: "com",
        market_place_id: "AF2M0KC94RCEA",
    },
    Locale {
        country_code: "uk",
        domain: "co.uk",
        market_place_id: "A2I9A3Q2GNFNGQ",
    },
    Locale {
        country_code: "fr",
        domain: "fr",
        market_place_id: "A2728XDNODOQ8T",
    },
    Locale {
        country_code: "ca",
        domain: "ca",
        market_place_id: "A2CQZ5RBY40XE",
    },
    Locale {
        country_code: "it",
        domain: "it",
        market_place_id: "A2N7FU2W2BU2ZC",
    },
    Locale {
        country_code: "au",
        domain: "com.au",
        market_place_id: "AN7EY7DTAW63G",
    },
    Locale {
        country_code: "in",
        domain: "in",
        market_place_id: "AJO3FBRUE6J4S",
    },
    Locale {
        country_code: "jp",
        domain: "co.jp",
        market_place_id: "A1QAP3MOU4173J",
    },
    Locale {
        country_code: "es",
        domain: "es",
        market_place_id: "ALMIKO4SZCSAR",
    },
    Locale {
        country_code: "br",
        domain: "com.br",
        market_place_id: "A10J1VAYUDTYRN",
    },
];

/// Looks up a marketplace by country code (case-insensitive).
pub fn find(country_code: &str) -> Option<Locale> {
    LOCALES
        .iter()
        .find(|locale| locale.country_code.eq_ignore_ascii_case(country_code))
        .copied()
}

/// Looks up a marketplace by its TLD/domain (case-insensitive) — the reverse
/// of [`Locale::domain`] (`com` → us, `co.uk` → uk, `com.au` → au). Used to map
/// a request host back to a country code for the cookie exchange.
pub fn find_by_domain(domain: &str) -> Option<Locale> {
    LOCALES
        .iter()
        .find(|locale| locale.domain.eq_ignore_ascii_case(domain))
        .copied()
}

impl Locale {
    /// Base URL of the Audible API for this marketplace.
    pub fn audible_api_url(&self) -> String {
        format!("https://api.audible.{}", self.domain)
    }

    /// Base URL of the Amazon auth API (token refresh, registration) for
    /// accounts with an Amazon identity.
    pub fn amazon_auth_url(&self) -> String {
        format!("https://api.amazon.{}", self.domain)
    }

    /// Base URL of the Audible auth API, used instead of
    /// [`Self::amazon_auth_url`] for pre-merger Audible accounts
    /// (`AccountOrigin::AudibleLegacy`).
    pub fn audible_auth_url(&self) -> String {
        format!("https://api.audible.{}", self.domain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(find("de"), find("DE"));
        assert_eq!(find("us").unwrap().domain, "com");
        assert!(find("xx").is_none());
    }

    #[test]
    fn find_by_domain_reverses_the_lookup() {
        assert_eq!(find_by_domain("com").unwrap().country_code, "us");
        assert_eq!(find_by_domain("co.uk").unwrap().country_code, "uk");
        assert_eq!(find_by_domain("com.au").unwrap().country_code, "au");
        assert_eq!(find_by_domain("DE").unwrap().country_code, "de");
        assert!(find_by_domain("example.com").is_none());
    }

    #[test]
    fn registry_has_unique_codes_and_marketplace_ids() {
        for (i, a) in LOCALES.iter().enumerate() {
            for b in &LOCALES[i + 1..] {
                assert_ne!(a.country_code, b.country_code);
                assert_ne!(a.market_place_id, b.market_place_id);
            }
        }
    }

    #[test]
    fn url_construction() {
        let uk = find("uk").unwrap();
        assert_eq!(uk.audible_api_url(), "https://api.audible.co.uk");
        assert_eq!(uk.amazon_auth_url(), "https://api.amazon.co.uk");
        assert_eq!(uk.audible_auth_url(), "https://api.audible.co.uk");
    }
}
