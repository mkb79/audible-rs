//! Comment-preserving config writes via `toml_edit`, with full schema
//! validation before anything reaches disk: every mutation is applied to
//! a copy, re-parsed into the typed [`Config`] and validated — only then
//! is the file replaced (atomically).

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::ConfigError;
use super::schema::Config;

/// Sets a dotted key (e.g. `defaults.download_dir`) to a value given as
/// CLI string. The TOML type is inferred (bool, integer, else string);
/// if the inferred type does not fit the schema, the string form is
/// tried before giving up. Comments and formatting are preserved.
pub fn set(content: &str, key: &str, raw_value: &str) -> Result<String, ConfigError> {
    set_many(content, &[(key, raw_value)])
}

/// Sets several dotted keys in one validated step. Needed when the keys
/// are only valid together (e.g. `account.marketplaces` and
/// `default_marketplaces`, which validate as a subset relation).
///
/// Each value's TOML type is decided independently: every entry starts as
/// a string, then is upgraded to its inferred type (bool/integer) only
/// when that keeps the whole config valid. So a string field whose value
/// looks numeric (`sync_max_age = "12h"`) stays a string while a real
/// integer field (`filename_max_length = 230`) becomes an integer. As a
/// last step, a value that validates only as a string array (e.g.
/// `chapter_type`, `cover_size`) is comma-split into a TOML array — this
/// is meant for single-key `config set`, not for batches that mix an
/// array field with other typed fields (those route arrays through
/// [`set_array`] separately).
pub fn set_many(content: &str, entries: &[(&str, &str)]) -> Result<String, ConfigError> {
    let doc = parse_document(content)?;

    // Baseline: every entry as a string (all "valid-together" keys are set
    // at once, so the doc can become valid).
    let mut candidate = doc;
    for (key, raw_value) in entries {
        set_path(&mut candidate, key, Value::from(*raw_value))?;
    }

    // Upgrade entries to their inferred non-string types. First try upgrading
    // them all at once — the common case where each value's inferred type is
    // the right one (e.g. a batch with several bool/integer fields, which a
    // one-at-a-time pass can't validate because the others are still strings).
    let mut all = candidate.clone();
    for (key, raw_value) in entries {
        set_path(&mut all, key, infer_value(raw_value))?;
    }
    if config_is_valid(&all) {
        candidate = all;
    } else {
        // Fall back to per-entry upgrades so a genuine string field whose value
        // happens to look numeric/boolean stays a string.
        for (key, raw_value) in entries {
            let inferred = infer_value(raw_value);
            if matches!(inferred, Value::String(_)) {
                continue;
            }
            let mut trial = candidate.clone();
            set_path(&mut trial, key, inferred)?;
            if config_is_valid(&trial) {
                candidate = trial;
            }
        }
    }

    // Array fallback: a value that does not validate as a scalar but whose
    // comma-split form validates as an array of strings becomes a TOML
    // array — so `config set settings.default.chapter_type tree,flat`
    // (and the marketplace lists) write arrays from a CSV CLI value. The
    // whole-config validity guard keeps genuine string fields as strings.
    if !config_is_valid(&candidate) {
        for (key, raw_value) in entries {
            let mut array = Array::new();
            for item in raw_value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                array.push(item);
            }
            let mut trial = candidate.clone();
            set_path(&mut trial, key, Value::Array(array))?;
            if config_is_valid(&trial) {
                candidate = trial;
            }
        }
    }

    let serialized = candidate.to_string();
    match toml::from_str::<Config>(&serialized) {
        Ok(config) => {
            config.validate()?;
            Ok(serialized)
        }
        Err(error) => {
            let keys: Vec<&str> = entries.iter().map(|(key, _)| *key).collect();
            Err(ConfigError::Invalid(format!(
                "cannot set {}: {}",
                keys.join(", "),
                error
            )))
        }
    }
}

/// Sets a dotted key to a TOML array of strings (e.g.
/// `accounts.<n>.marketplaces`). Validates the whole config before
/// returning, like [`set`]. An empty slice writes an empty array.
pub fn set_array(content: &str, key: &str, values: &[String]) -> Result<String, ConfigError> {
    let mut doc = parse_document(content)?;
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    set_path(&mut doc, key, Value::Array(array))?;

    let serialized = doc.to_string();
    match toml::from_str::<Config>(&serialized) {
        Ok(config) => {
            config.validate()?;
            Ok(serialized)
        }
        Err(error) => Err(ConfigError::Invalid(format!("cannot set {key}: {error}"))),
    }
}

/// Appends an `[[<table>.allowed_hosts]]` entry (`plugins` for the
/// ephemeral broker, `session` for the agent — AUD-120/124), creating
/// the parent table and the array as needed. Fails if the host is
/// already listed or the result would not validate.
pub fn add_allowed_host(
    content: &str,
    table: &str,
    host: &str,
    auth: &str,
) -> Result<String, ConfigError> {
    use toml_edit::{ArrayOfTables, value};
    let mut doc = parse_document(content)?;
    let parent = doc
        .entry(table)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("[{table}] is not a table")))?;
    let hosts = parent
        .entry("allowed_hosts")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("{table}.allowed_hosts is not an array")))?;
    if hosts
        .iter()
        .any(|table| table.get("host").and_then(Item::as_str) == Some(host))
    {
        return Err(ConfigError::Invalid(format!(
            "host {host:?} is already allowed"
        )));
    }
    let mut table = Table::new();
    table["host"] = value(host);
    table["auth"] = value(auth);
    hosts.push(table);
    validate_document(&doc)
}

/// Removes an `[[<table>.allowed_hosts]]` entry by host. Fails if absent.
pub fn remove_allowed_host(content: &str, table: &str, host: &str) -> Result<String, ConfigError> {
    let mut doc = parse_document(content)?;
    let Some(hosts) = doc
        .get_mut(table)
        .and_then(Item::as_table_mut)
        .and_then(|parent| parent.get_mut("allowed_hosts"))
        .and_then(Item::as_array_of_tables_mut)
    else {
        return Err(ConfigError::Invalid(format!(
            "host {host:?} is not allowed"
        )));
    };
    let before = hosts.len();
    hosts.retain(|table| table.get("host").and_then(Item::as_str) != Some(host));
    if hosts.len() == before {
        return Err(ConfigError::Invalid(format!(
            "host {host:?} is not allowed"
        )));
    }
    validate_document(&doc)
}

/// Serializes a document, re-parses it into the typed [`Config`] and
/// validates — the shared tail of the array-of-tables mutations.
fn validate_document(doc: &DocumentMut) -> Result<String, ConfigError> {
    let serialized = doc.to_string();
    match toml::from_str::<Config>(&serialized) {
        Ok(config) => {
            config.validate()?;
            Ok(serialized)
        }
        Err(error) => Err(ConfigError::Invalid(error.to_string())),
    }
}

/// Renames an account: moves the `[accounts.<old>]` table to
/// `[accounts.<new>]` (keeping its contents and formatting) and repoints
/// the top-level `default_account` when it referenced the old name.
/// Fails if `<old>` is absent, `<new>` already exists, or the result
/// would not validate.
pub fn rename_account(content: &str, old: &str, new: &str) -> Result<String, ConfigError> {
    let mut doc = parse_document(content)?;

    let accounts = doc
        .get_mut("accounts")
        .and_then(Item::as_table_mut)
        .filter(|table| table.contains_key(old))
        .ok_or_else(|| ConfigError::Invalid(format!("account {old:?} is not set")))?;
    if accounts.contains_key(new) {
        return Err(ConfigError::Invalid(format!(
            "account {new:?} already exists"
        )));
    }
    let item = accounts.remove(old).expect("contains_key checked above");
    accounts.insert(new, item);

    // Repoint default_account if it pointed at the renamed account.
    if doc.get("default_account").and_then(Item::as_str) == Some(old) {
        set_path(&mut doc, "default_account", Value::from(new))?;
    }

    let serialized = doc.to_string();
    match toml::from_str::<Config>(&serialized) {
        Ok(config) => {
            config.validate()?;
            Ok(serialized)
        }
        Err(error) => Err(ConfigError::Invalid(format!(
            "cannot rename account {old:?} to {new:?}: {error}"
        ))),
    }
}

/// Whether a candidate document parses and validates as a [`Config`].
fn config_is_valid(doc: &DocumentMut) -> bool {
    toml::from_str::<Config>(&doc.to_string())
        .map(|config| config.validate().is_ok())
        .unwrap_or(false)
}

/// Removes a dotted key (or a whole table). Fails if the key is not set
/// or removing it would leave an invalid config.
pub fn unset(content: &str, key: &str) -> Result<String, ConfigError> {
    let mut doc = parse_document(content)?;

    let segments = split_key(key)?;
    let (last, parents) = segments.split_last().expect("split_key rejects empty keys");
    let mut current = doc.as_item_mut();
    for segment in parents {
        current = current
            .as_table_mut()
            .and_then(|table| table.get_mut(segment))
            .ok_or_else(|| ConfigError::Invalid(format!("{key} is not set")))?;
    }
    current
        .as_table_mut()
        .and_then(|table| table.remove(last))
        .ok_or_else(|| ConfigError::Invalid(format!("{key} is not set")))?;

    let serialized = doc.to_string();
    let config: Config = toml::from_str(&serialized)
        .map_err(|error| ConfigError::Invalid(format!("cannot unset {key}: {error}")))?;
    config.validate()?;
    Ok(serialized)
}

/// Reads a dotted key as display string, `None` if unset.
pub fn get(content: &str, key: &str) -> Result<Option<String>, ConfigError> {
    let value: toml::Value = toml::from_str(content)?;
    let mut current = &value;
    for segment in split_key(key)? {
        match current.get(segment) {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(Some(render(current)))
}

/// Flattens all set values into sorted `(dotted key, value)` pairs.
pub fn flatten(content: &str) -> Result<Vec<(String, String)>, ConfigError> {
    let value: toml::Value = toml::from_str(content)?;
    let mut entries = Vec::new();
    walk(&value, String::new(), &mut entries);
    entries.sort();
    Ok(entries)
}

/// Applies a string-level edit to the config file, creating it (and its
/// directory) on demand, and writes the result atomically.
pub fn edit_file(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<String, ConfigError>,
) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // The cross-process lock spans the whole read-modify-write: the CLI
    // and the agent may edit concurrently (`config set` vs `allow-host`),
    // and without it one side's edit silently vanished (audit B5). The
    // write itself fsyncs via a unique temp file — no zero-length config
    // on power loss, no shared temp inode between writers.
    let mut lock = crate::fsutil::write_lock(path)?;
    let _guard = lock.write()?;
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            format!("version = {}\n", super::CONFIG_VERSION)
        }
        Err(error) => return Err(error.into()),
    };
    let updated = edit(&content)?;
    crate::fsutil::persist_atomically(path, updated.as_bytes(), None)?;
    Ok(())
}

fn parse_document(content: &str) -> Result<DocumentMut, ConfigError> {
    content.parse().map_err(|error: toml_edit::TomlError| {
        ConfigError::Invalid(format!("config is not valid TOML: {error}"))
    })
}

fn split_key(key: &str) -> Result<Vec<&str>, ConfigError> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(ConfigError::Invalid(format!("invalid key {key:?}")));
    }
    Ok(segments)
}

/// Infers the most specific TOML type for a CLI-supplied value.
fn infer_value(raw: &str) -> Value {
    if let Ok(boolean) = raw.parse::<bool>() {
        return Value::from(boolean);
    }
    if let Ok(integer) = raw.parse::<i64>() {
        return Value::from(integer);
    }
    Value::from(raw)
}

fn set_path(doc: &mut DocumentMut, key: &str, value: Value) -> Result<(), ConfigError> {
    let segments = split_key(key)?;
    let (last, parents) = segments.split_last().expect("split_key rejects empty keys");

    let mut current = doc.as_item_mut();
    for segment in parents {
        let table = current
            .as_table_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("{key}: {segment} is not a table")))?;
        if !table.contains_key(segment) {
            let mut intermediate = Table::new();
            // Don't emit headers for purely structural tables ([profiles]
            // when setting profiles.x.y).
            intermediate.set_implicit(true);
            table.insert(segment, Item::Table(intermediate));
        }
        current = table.get_mut(segment).expect("just inserted");
    }
    let table = current
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("{key}: {last} is not settable")))?;
    table.insert(last, Item::Value(value));
    Ok(())
}

fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn walk(value: &toml::Value, prefix: String, entries: &mut Vec<(String, String)>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(child, path, entries);
            }
        }
        other => entries.push((prefix, render(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "\
# my config
version = 1

[accounts.alice]            # main account
auth_file = \"alice.auth\"
marketplaces = [\"de\", \"us\"]
default_marketplaces = [\"de\"]

[settings.default]
download_dir = \"~/Audible\"
";

    #[test]
    fn set_preserves_comments_and_formatting() {
        let updated = set(BASE, "default_account", "alice").unwrap();
        assert!(updated.contains("# my config"));
        assert!(updated.contains("# main account"));
        assert!(updated.contains("default_account = \"alice\""));
    }

    #[test]
    fn set_infers_types_against_the_schema() {
        let updated = set(BASE, "db.page_size", "500").unwrap();
        assert!(updated.contains("page_size = 500"));
        let updated = set(BASE, "db.fts", "false").unwrap();
        assert!(updated.contains("fts = false"));
        // Numeric-looking value for a string field stays a string.
        let updated = set(BASE, "db.sync_max_age", "12h").unwrap();
        assert!(updated.contains("sync_max_age = \"12h\""));
    }

    #[test]
    fn set_many_infers_int_vs_string() {
        // filename_max_length is an integer; download_dir a string. Both
        // scalars in one batch (set_many handles scalar batches; array
        // fields are written separately via set_array).
        let updated = set_many(
            BASE,
            &[
                ("settings.default.filename_max_length", "230"),
                ("settings.default.download_dir", "/x"),
            ],
        )
        .unwrap();
        assert!(updated.contains("filename_max_length = 230"), "{updated}");
        assert!(updated.contains("download_dir = \"/x\""), "{updated}");
        let config: Config = toml::from_str(&updated).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn set_many_handles_several_non_string_fields() {
        // The `setup` batch mixes a bool and two integers in one call; each must
        // infer its type even though the others are still strings in the
        // baseline (regression: the one-at-a-time pass left them all strings,
        // because the whole doc never validated mid-upgrade).
        let updated = set_many(
            BASE,
            &[
                ("db.record_changes", "false"),
                ("db.change_retention_days", "30"),
                ("settings.default.filename_max_length", "200"),
                ("settings.default.download_dir", "/x"),
            ],
        )
        .unwrap();
        assert!(updated.contains("record_changes = false"), "{updated}");
        assert!(updated.contains("change_retention_days = 30"), "{updated}");
        assert!(updated.contains("filename_max_length = 200"), "{updated}");
        assert!(updated.contains("download_dir = \"/x\""), "{updated}");
        let config: Config = toml::from_str(&updated).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn array_fallback_only_applies_to_array_fields() {
        // chapter_type is an array field: a single-key `config set` with a
        // CSV value becomes an array (cover_size likewise).
        let updated = set(BASE, "settings.default.chapter_type", "tree,flat").unwrap();
        let config: Config = toml::from_str(&updated).unwrap();
        assert_eq!(
            config.settings["default"].chapter_type.as_deref(),
            Some(&["tree".to_owned(), "flat".to_owned()][..])
        );
        // A single value becomes a one-element array, not an int.
        let updated = set(BASE, "settings.default.cover_size", "500").unwrap();
        let config: Config = toml::from_str(&updated).unwrap();
        assert_eq!(
            config.settings["default"].cover_size.as_deref(),
            Some(&["500".to_owned()][..])
        );
        // download_dir is a string field: a comma value stays a string.
        let updated = set(BASE, "settings.default.download_dir", "/a,/b").unwrap();
        let config: Config = toml::from_str(&updated).unwrap();
        assert_eq!(
            config.settings["default"].download_dir.as_deref(),
            Some(std::path::Path::new("/a,/b"))
        );
    }

    #[test]
    fn set_array_writes_and_validates() {
        // Extend the account's marketplaces (and an unknown cc is rejected).
        let updated = set_array(
            BASE,
            "accounts.alice.marketplaces",
            &["de".into(), "us".into(), "uk".into()],
        )
        .unwrap();
        assert!(updated.contains("\"uk\""), "{updated}");
        let config: Config = toml::from_str(&updated).unwrap();
        config.validate().unwrap();
        assert!(
            set_array(BASE, "accounts.alice.marketplaces", &["zz".into()]).is_err(),
            "unknown marketplace must be rejected"
        );
    }

    #[test]
    fn rename_account_moves_table_and_repoints_default() {
        let with_default = set(BASE, "default_account", "alice").unwrap();
        let renamed = rename_account(&with_default, "alice", "alice-de").unwrap();

        let config: Config = toml::from_str(&renamed).unwrap();
        config.validate().unwrap();
        assert!(config.accounts.contains_key("alice-de"));
        assert!(!config.accounts.contains_key("alice"));
        // The default pointer follows the rename.
        assert_eq!(config.default_account.as_deref(), Some("alice-de"));
        // Contents (and the auth file path) survive the move.
        assert_eq!(
            config.accounts["alice-de"].auth_file,
            std::path::PathBuf::from("alice.auth")
        );
        assert_eq!(
            config.accounts["alice-de"].marketplaces,
            vec!["de".to_owned(), "us".to_owned()]
        );
        // The top-of-file comment is preserved.
        assert!(renamed.contains("# my config"));

        // Unknown source and a clash with an existing name are rejected.
        assert!(rename_account(BASE, "ghost", "x").is_err());
        let two = "\
version = 1

[accounts.alice]
auth_file = \"alice.auth\"
marketplaces = [\"de\"]

[accounts.bob]
auth_file = \"bob.auth\"
marketplaces = [\"de\"]
";
        assert!(rename_account(two, "alice", "bob").is_err());
    }

    #[test]
    fn rename_account_without_default_leaves_it_unset() {
        let renamed = rename_account(BASE, "alice", "carol").unwrap();
        let config: Config = toml::from_str(&renamed).unwrap();
        config.validate().unwrap();
        assert!(config.accounts.contains_key("carol"));
        assert!(config.default_account.is_none());
    }

    #[test]
    fn set_rejects_schema_violations() {
        // Type mismatch.
        assert!(set(BASE, "db.page_size", "many").is_err());
        // Unknown key.
        assert!(set(BASE, "settings.default.download_dirr", "/x").is_err());
        // Dangling reference.
        assert!(set(BASE, "default_account", "ghost").is_err());
        // Invalid duration value.
        assert!(set(BASE, "db.sync_max_age", "soon").is_err());
    }

    #[test]
    fn unset_removes_values_and_guards_references() {
        let with_default = set(BASE, "default_account", "alice").unwrap();
        let removed = unset(&with_default, "default_account").unwrap();
        assert!(!removed.contains("default_account = \"alice\""));

        // Removing something unset is an error.
        assert!(unset(BASE, "session.socket_dir").is_err());
        // Removing the whole settings bundle is fine.
        assert!(unset(BASE, "settings.default").is_ok());
        // Removing the whole account table is fine (nothing references it).
        assert!(unset(BASE, "accounts.alice").is_ok());
    }

    #[test]
    fn get_and_flatten() {
        assert_eq!(
            get(BASE, "accounts.alice.auth_file").unwrap(),
            Some("alice.auth".to_owned())
        );
        assert_eq!(get(BASE, "default_account").unwrap(), None);

        let entries = flatten(BASE).unwrap();
        assert!(entries.contains(&("version".to_owned(), "1".to_owned())));
        assert!(entries.contains(&(
            "accounts.alice.auth_file".to_owned(),
            "alice.auth".to_owned()
        )));
    }

    #[test]
    fn edit_file_creates_config_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        edit_file(&path, |content| {
            set(content, "settings.default.download_dir", "/tmp/audio")
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("version = 1"));
        assert!(written.contains("download_dir = \"/tmp/audio\""));
        // And the result is loadable.
        Config::load(&path).unwrap();
    }

    #[test]
    fn allowed_hosts_add_dedup_remove() {
        let base = "version = 1\n";
        // Add creates [plugins] + the array-of-tables entry.
        let one = add_allowed_host(base, "plugins", "cde-ta-g7g.amazon.com", "signing").unwrap();
        let config: Config = toml::from_str(&one).unwrap();
        assert_eq!(config.plugins.allowed_hosts.len(), 1);
        assert_eq!(
            config.plugins.allowed_hosts[0].host,
            "cde-ta-g7g.amazon.com"
        );
        assert_eq!(config.plugins.allowed_hosts[0].auth, "signing");

        // A second host appends; a duplicate is rejected.
        let two = add_allowed_host(&one, "plugins", "other.amazon.com", "token").unwrap();
        assert_eq!(
            toml::from_str::<Config>(&two)
                .unwrap()
                .plugins
                .allowed_hosts
                .len(),
            2
        );
        assert!(add_allowed_host(&two, "plugins", "other.amazon.com", "auto").is_err());

        // Remove drops one; removing an absent host errors.
        let back = remove_allowed_host(&two, "plugins", "other.amazon.com").unwrap();
        let config: Config = toml::from_str(&back).unwrap();
        assert_eq!(config.plugins.allowed_hosts.len(), 1);
        assert!(remove_allowed_host(&back, "plugins", "nope.com").is_err());

        // The agent list is independent (AUD-124): a session entry does
        // not appear under plugins and vice versa.
        let agent = add_allowed_host(&back, "session", "cde-ta-g7g.amazon.com", "signing").unwrap();
        let config: Config = toml::from_str(&agent).unwrap();
        assert_eq!(config.session.allowed_hosts.len(), 1);
        assert_eq!(config.plugins.allowed_hosts.len(), 1);
        assert!(remove_allowed_host(&agent, "plugins", "cde-ta-g7g.amazon.com").is_ok());
    }
}
