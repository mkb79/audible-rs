//! Variable catalog for the `custom` filename template (AUD-53).
//!
//! Single source of truth for the `%name%` variables a user may put in
//! `filename_template` when `filename_mode = "custom"`. Template expansion,
//! template validation, and the generated `--help` / user documentation all
//! read [`TEMPLATE_VARS`] — add a variable here once and everything follows.
//!
//! # Grammar
//!
//! A token is `%name%`, optionally with a charset modifier:
//! - `%name%`   — value as Unicode (filesystem-unsafe characters replaced).
//!   Equivalent to `%name!u%`.
//! - `%name!u%` — Unicode (the default).
//! - `%name!a%` — transliterated to ASCII (e.g. `ä`→`a`, spaces→`_`).
//!
//! A `/` in the template starts a subdirectory (always relative to
//! `download_dir`); each path segment is sanitized independently and capped to
//! `filename_max_length`. An empty variable renders as `unknown` (see
//! [`EMPTY_PLACEHOLDER`]) rather than vanishing, so a title without a
//! `%publication%` lands in an `unknown/` folder and the folder structure stays
//! regular and predictable. `custom` mode **requires** an explicit
//! `filename_template` — there is no default template.
//!
//! # Scope (decided in AUD-53)
//!
//! Variables are **scalar only**: list/dict item fields (authors, narrators,
//! series) are intentionally excluded. The audio quality / codec
//! (`content_format`), the cover size and the chapter quality are **not**
//! variables — they stay hard-wired filename suffixes so downloading a title in
//! different qualities/sizes never overwrites earlier files.

/// One variable available in a custom `filename_template`.
///
/// The list/dict item fields are excluded by design; every entry here maps to a
/// scalar. The name → item-field mapping lives in the template expander (kept in
/// sync with this catalog).
pub struct TemplateVar {
    /// Token name; used as `%name%` (optionally `%name!a%` / `%name!u%`).
    pub name: &'static str,
    /// One-line description for `--help` and the user documentation.
    pub description: &'static str,
    /// An example expanded value.
    pub example: &'static str,
}

/// Every variable available in a custom filename template, in the order used
/// for `--help` / documentation. Verified against a real library (2026-07-01):
/// each field is scalar with broad coverage.
pub const TEMPLATE_VARS: &[TemplateVar] = &[
    // Identity / title (core).
    TemplateVar {
        name: "asin",
        description: "Audible product id (ASIN)",
        example: "B08XDZCH78",
    },
    TemplateVar {
        name: "title",
        description: "Main title",
        example: "Star Force: Enlightenment",
    },
    TemplateVar {
        name: "subtitle",
        description: "Subtitle (empty for many titles)",
        example: "A History of Nazi Germany",
    },
    TemplateVar {
        name: "fulltitle",
        description: "Title plus subtitle",
        example: "Star Force: Enlightenment",
    },
    TemplateVar {
        name: "account",
        description: "Resolved account name",
        example: "alice",
    },
    TemplateVar {
        name: "marketplace",
        description: "Marketplace country code",
        example: "de",
    },
    // Metadata (scalar, high coverage).
    TemplateVar {
        name: "publisher",
        description: "Publisher (from publisher_name)",
        example: "Blackstone Audio, Inc.",
    },
    TemplateVar {
        name: "language",
        description: "Language",
        example: "english",
    },
    TemplateVar {
        name: "release_year",
        description: "Year of the original release (from release_date)",
        example: "2021",
    },
    TemplateVar {
        name: "purchase_year",
        description: "Year the title entered the library (from purchase_date)",
        example: "2024",
    },
    TemplateVar {
        name: "runtime",
        description: "Runtime in minutes (from runtime_length_min)",
        example: "247",
    },
    TemplateVar {
        name: "format",
        description: "Abridgement / recording type (from format_type)",
        example: "unabridged",
    },
    TemplateVar {
        name: "type",
        description: "Content delivery type (from content_delivery_type)",
        example: "MultiPartBook",
    },
    TemplateVar {
        name: "publication",
        description: "Collection / publication name, series-like (empty for standalone titles)",
        example: "Star Force Universe (Jyr)",
    },
];

/// The value an empty variable expands to, so the folder/name structure stays
/// regular (every variable always yields a token) and a missing value is
/// obvious. Deliberately fixed (not configurable).
pub const EMPTY_PLACEHOLDER: &str = "unknown";

/// Whether `name` is a known template variable (the token between `%…%`,
/// without any `!a`/`!u` modifier). Used by the template expander to reject
/// unknown variables.
pub fn is_variable(name: &str) -> bool {
    TEMPLATE_VARS.iter().any(|var| var.name == name)
}

/// Validates a custom template without expanding it: balanced `%…%` tokens,
/// known variable names and valid `!a`/`!u` modifiers (`%%` is a literal `%`).
/// `Err` carries a human-readable reason — used by `setup` for immediate
/// verification and by the expander before it runs.
pub fn validate(template: &str) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            return Err(format!("unterminated %…% near {:?}", &rest[start..]));
        };
        let token = &after[..end];
        rest = &after[end + 1..];
        if token.is_empty() {
            continue; // `%%` → a literal percent sign
        }
        let (name, modifier) = match token.split_once('!') {
            Some((name, modifier)) => (name, Some(modifier)),
            None => (token, None),
        };
        match modifier {
            None | Some("a") | Some("u") => {}
            Some(other) => {
                return Err(format!(
                    "unknown modifier {other:?} in %{token}% (use !a for ascii or !u for unicode)"
                ));
            }
        }
        if !is_variable(name) {
            return Err(format!("unknown variable %{name}%"));
        }
    }
    Ok(())
}

/// A human-readable reference of the template grammar and variables, built from
/// [`TEMPLATE_VARS`] — for `--help` and generated documentation.
pub fn help_text() -> String {
    use std::fmt::Write as _;
    let mut out = String::from(
        "Custom filename template (filename_mode = \"custom\", requires \
         filename_template):\n  tokens are %name%, %name!a% (ascii) or %name!u% \
         (unicode, the default); a `/` creates folders under download_dir. The \
         audio quality, cover size and chapter layout stay fixed suffixes; an \
         empty variable becomes `unknown`.\n\nVariables:\n",
    );
    // Align on the `%name%` token (two extra chars for the percent signs).
    let width = TEMPLATE_VARS
        .iter()
        .map(|v| v.name.len())
        .max()
        .unwrap_or(0)
        + 2;
    for var in TEMPLATE_VARS {
        let token = format!("%{}%", var.name);
        let _ = writeln!(
            out,
            "  {token:<width$}  {} (e.g. {})",
            var.description, var.example
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_well_formed() {
        assert!(!TEMPLATE_VARS.is_empty());

        // Names are unique (a template expander/validator keys off them).
        let mut names: Vec<&str> = TEMPLATE_VARS.iter().map(|var| var.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "template variable names must be unique");

        for var in TEMPLATE_VARS {
            assert!(!var.name.is_empty(), "empty variable name");
            assert!(
                !var.description.is_empty(),
                "{} has no description",
                var.name
            );
            assert!(!var.example.is_empty(), "{} has no example", var.name);
            // Names must be safe tokens (they sit between `%…%`, no modifiers).
            assert!(
                var.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "variable name {:?} must be lowercase ascii/underscore",
                var.name
            );
        }
    }

    #[test]
    fn validate_accepts_valid_and_rejects_invalid() {
        assert!(validate("%publication%/%fulltitle% (%release_year%)").is_ok());
        assert!(validate("%title!a%_%publisher!u%").is_ok());
        assert!(validate("100%%").is_ok()); // literal percent
        assert!(validate("plain text, no tokens").is_ok());
        assert!(validate("%author%").is_err()); // unknown variable
        assert!(validate("%title!x%").is_err()); // unknown modifier
        assert!(validate("%title").is_err()); // unterminated
    }
}
