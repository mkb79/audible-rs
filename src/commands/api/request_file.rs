//! Request files for `audible api`: a TOML description of a whole request
//! (method, path, query, headers, body, auth, marketplace) that can be loaded
//! and saved, plus `{{var}}` templating.
//!
//! Values are substituted verbatim from `[vars]` (file defaults) overlaid by
//! `--var` (CLI wins). `{{asin}}` is just an ordinary variable — it is NOT
//! resolved against the library, because `api` legitimately targets catalog
//! and other endpoints with ASINs the user does not own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// TOML representation of a request. Scalar fields come before the tables so
/// the serialized output is valid TOML (tables must follow top-level keys).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestFile {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub marketplace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub body_file: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub query: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub vars: BTreeMap<String, String>,
}

impl RequestFile {
    /// File `[query]` as `key=value` strings (for merging with `--query`).
    pub fn query_as_strings(&self) -> Vec<String> {
        self.query.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// File `[headers]` as `Name: value` strings (for merging with `--header`).
    pub fn headers_as_strings(&self) -> Vec<String> {
        self.headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect()
    }
}

/// Loads and parses a TOML request file.
pub(super) async fn load(path: &Path) -> Result<RequestFile> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("could not read request file {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid request file {}", path.display()))
}

/// Serializes a request to a TOML file.
pub(super) async fn save(path: &Path, file: &RequestFile) -> Result<()> {
    let text = toml::to_string(file).context("could not serialize the request")?;
    tokio::fs::write(path, text)
        .await
        .with_context(|| format!("could not write request file {}", path.display()))
}

/// Parses `key=value` strings into the file's `[query]` map. The map holds
/// one value per key, but the send path keeps repeats as repeated
/// parameters (`asins=A&asins=B`) — so a key appearing with **different**
/// values must fail loudly here instead of silently saving a request that
/// replays differently than the one sent (audit 2026-07-18, C1).
/// Identical repeats collapse, like the send path's exact-duplicate rule.
pub(super) fn parse_query_strings(strings: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for raw in strings {
        let (key, value) = raw
            .split_once('=')
            .with_context(|| format!("query parameter {raw:?} is not KEY=VALUE"))?;
        let key = key.trim();
        match map.get(key) {
            Some(existing) if existing != value => bail!(
                "query key {key:?} appears with different values ({existing:?} and {value:?}) — \
                 the request file stores one value per key and cannot represent repeated \
                 parameters"
            ),
            _ => {
                map.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    Ok(map)
}

/// Parses `Name: value` strings into the file's `[headers]` map, applying
/// the send path's **policy** checks (auth-managed headers and
/// `content-type` are refused — a file must never store a header the send
/// would reject; audit 2026-07-18, C1). Name/value *syntax* is deliberately
/// not validated here: headers may carry `{{var}}` placeholders, which
/// only the post-substitution send can check. A name appearing with
/// different values fails like a repeated query key.
pub(super) fn parse_header_strings(strings: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for raw in strings {
        let (name, value) = raw
            .split_once(':')
            .with_context(|| format!("header {raw:?} is not \"Name: value\""))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            bail!("header {raw:?} has an empty name");
        }
        if super::is_auth_managed_header(name) {
            bail!(
                "header {name:?} is managed by --auth and cannot be stored in a request file; \
                 use the file's `auth` field instead"
            );
        }
        if name.eq_ignore_ascii_case("content-type") {
            bail!("store the body content type in the file's `content_type` field, not a header");
        }
        match map.get(name) {
            Some(existing) if existing != value => bail!(
                "header {name:?} appears with different values ({existing:?} and {value:?}) — \
                 the request file stores one value per header"
            ),
            _ => {
                map.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    Ok(map)
}

/// `[vars]` defaults from the file, overlaid by `--var key=value` (CLI wins).
pub(super) fn collect_vars(
    file: Option<&RequestFile>,
    cli: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut vars = file.map(|f| f.vars.clone()).unwrap_or_default();
    for raw in cli {
        let (key, value) = raw
            .split_once('=')
            .with_context(|| format!("--var {raw:?} is not KEY=VALUE"))?;
        vars.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(vars)
}

/// `{{name}}` substitution with backslash escaping. `\{` → `{` and `\}` → `}`
/// (so `\{\{` / `\}\}` emit literal `{{` / `}}`); single braces (as in JSON
/// bodies) are left untouched. Unknown names are collected and reported by
/// [`Templater::finish`].
pub(super) struct Templater<'v> {
    vars: &'v BTreeMap<String, String>,
    missing: BTreeSet<String>,
}

impl<'v> Templater<'v> {
    pub fn new(vars: &'v BTreeMap<String, String>) -> Self {
        Self {
            vars,
            missing: BTreeSet::new(),
        }
    }

    pub fn render(&mut self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if matches!(chars.peek(), Some('{') | Some('}')) => {
                    out.push(chars.next().expect("peeked"));
                }
                '{' if chars.peek() == Some(&'{') => {
                    chars.next(); // consume the second '{'
                    let mut name = String::new();
                    let mut closed = false;
                    while let Some(nc) = chars.next() {
                        if nc == '}' && chars.peek() == Some(&'}') {
                            chars.next();
                            closed = true;
                            break;
                        }
                        name.push(nc);
                    }
                    if !closed {
                        // Unterminated placeholder: keep it verbatim.
                        out.push_str("{{");
                        out.push_str(&name);
                    } else {
                        let key = name.trim();
                        match self.vars.get(key) {
                            Some(value) => out.push_str(value),
                            None => {
                                self.missing.insert(key.to_owned());
                            }
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    pub fn finish(self) -> Result<()> {
        if self.missing.is_empty() {
            Ok(())
        } else {
            let names = self.missing.into_iter().collect::<Vec<_>>().join(", ");
            bail!("undefined template variable(s): {names} (set them with --var or [vars])");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parses_full_file() {
        let toml = r#"
            method = "POST"
            path = "/catalog/products/{{asin}}"
            auth = "signing"
            content_type = "application/xml"
            body = "<x/>"
            [query]
            image_sizes = "500"
            [headers]
            Accept-Language = "en-US"
            [vars]
            asin = "B0XXXXXXXX"
        "#;
        let file: RequestFile = toml::from_str(toml).unwrap();
        assert_eq!(file.method.as_deref(), Some("POST"));
        assert_eq!(file.path.as_deref(), Some("/catalog/products/{{asin}}"));
        assert_eq!(
            file.query.get("image_sizes").map(String::as_str),
            Some("500")
        );
        assert_eq!(
            file.vars.get("asin").map(String::as_str),
            Some("B0XXXXXXXX")
        );
    }

    #[test]
    fn rejects_unknown_field() {
        assert!(toml::from_str::<RequestFile>("method = \"GET\"\nbogus = 1\n").is_err());
    }

    /// C1 (audit 2026-07-18): the saved file must describe the request
    /// that was sent — repeated keys with different values (kept as
    /// repeated parameters by the send path) and headers the send would
    /// refuse fail the save instead of silently saving something else.
    #[test]
    fn save_parsers_match_the_send_contract() {
        // Identical repeats collapse, like the send path …
        let map = parse_query_strings(&["asins=A".into(), "asins=A".into()]).unwrap();
        assert_eq!(map.get("asins").map(String::as_str), Some("A"));
        // … but different values cannot be represented: loud error.
        let error = parse_query_strings(&["asins=A".into(), "asins=B".into()]).unwrap_err();
        assert!(error.to_string().contains("repeated"), "{error:#}");

        let error = parse_header_strings(&["authorization: x".into()]).unwrap_err();
        assert!(error.to_string().contains("--auth"), "{error:#}");
        let error = parse_header_strings(&["Content-Type: text/xml".into()]).unwrap_err();
        assert!(error.to_string().contains("content_type"), "{error:#}");
        let error = parse_header_strings(&["Accept: a".into(), "Accept: b".into()]).unwrap_err();
        assert!(error.to_string().contains("different values"), "{error:#}");
        // Templated values pass — only the post-substitution send
        // validates header syntax.
        let map = parse_header_strings(&["Accept-Language: {{lang}}".into()]).unwrap();
        assert_eq!(
            map.get("Accept-Language").map(String::as_str),
            Some("{{lang}}")
        );
    }

    #[tokio::test]
    async fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.toml");
        let mut file = RequestFile {
            method: Some("GET".into()),
            path: Some("/1.0/library".into()),
            ..Default::default()
        };
        file.query.insert("num_results".into(), "50".into());
        file.vars.insert("size".into(), "500".into());
        save(&path, &file).await.unwrap();
        let loaded = load(&path).await.unwrap();
        assert_eq!(file, loaded);
    }

    #[test]
    fn collect_vars_cli_overrides_file_defaults() {
        let file = RequestFile {
            vars: vars(&[("size", "500"), ("fmt", "json")]),
            ..Default::default()
        };
        let merged = collect_vars(Some(&file), &["size=900".to_owned()]).unwrap();
        assert_eq!(merged.get("size").map(String::as_str), Some("900"));
        assert_eq!(merged.get("fmt").map(String::as_str), Some("json"));
    }

    #[test]
    fn renders_variables() {
        let v = vars(&[("asin", "B01"), ("size", "500")]);
        let mut t = Templater::new(&v);
        assert_eq!(
            t.render("/catalog/products/{{asin}}"),
            "/catalog/products/B01"
        );
        assert_eq!(t.render("size={{ size }}"), "size=500");
        t.finish().unwrap();
    }

    #[test]
    fn undefined_variable_errors() {
        let v = vars(&[]);
        let mut t = Templater::new(&v);
        let _ = t.render("{{missing}}");
        assert!(t.finish().is_err());
    }

    #[test]
    fn escapes_braces() {
        let v = vars(&[("asin", "B01")]);
        let mut t = Templater::new(&v);
        // \{\{ and \}\} become literal {{ }} and are not placeholders.
        assert_eq!(t.render("\\{\\{asin\\}\\}"), "{{asin}}");
        // single braces (JSON) untouched; real placeholder still expands.
        assert_eq!(t.render("{\"asin\":\"{{asin}}\"}"), "{\"asin\":\"B01\"}");
        t.finish().unwrap();
    }
}
