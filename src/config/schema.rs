//! Typed config schema (archived architecture §2). Loading is strict:
//! unknown keys are rejected, references and value ranges validated.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::locale;

use super::ConfigError;

/// Current config schema version.
pub const CONFIG_VERSION: u32 = 1;

/// The whole `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version; enables later migrations.
    pub version: u32,
    /// Account selected when `-a/--account` and `AUDIBLE_ACCOUNT` are
    /// unset (and more than one account exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_account: Option<String>,
    /// Accounts: one Amazon user = one device registration = one auth
    /// file, plus the marketplace axis the account may operate on.
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
    /// Reusable settings bundles. `settings.default` is the fallback
    /// layer every other bundle inherits unset fields from.
    #[serde(default)]
    pub settings: BTreeMap<String, Settings>,
    /// Library database settings (engine of the library commands, M2).
    #[serde(default)]
    pub db: DbConfig,
    /// Session agent settings (M5).
    #[serde(default)]
    pub session: SessionConfig,
    /// Plugin discovery settings (M6).
    #[serde(default)]
    pub plugins: PluginsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            default_account: None,
            accounts: BTreeMap::new(),
            settings: BTreeMap::new(),
            db: DbConfig::default(),
            session: SessionConfig::default(),
            plugins: PluginsConfig::default(),
        }
    }
}

/// `[plugins]` — plugin discovery settings (M6, AUD-68).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Plugin directory; default is the platform data dir's `plugins`
    /// subfolder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// User-approved external hosts a `hosts`-scoped plugin/caller may
    /// reach through `api.request` (M6, AUD-120). Deny-by-default: a host
    /// not listed here is refused. Managed via `plugin allow-host`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<AllowedHost>,
}

/// One `[[plugins.allowed_hosts]]` entry (AUD-120): a host plus the auth
/// mode the account uses against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedHost {
    /// Exact host name (no wildcards), e.g. `cde-ta-g7g.amazon.com`.
    pub host: String,
    /// Auth mode against this host: `auto | signing | token`. Default
    /// `signing` — request-bound, no reusable credential leaks.
    #[serde(default = "default_host_auth")]
    pub auth: String,
}

fn default_host_auth() -> String {
    "signing".to_owned()
}

/// `[accounts.<name>]` — identity/auth plus the marketplace axis the
/// account may operate on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// Auth file path; relative paths resolve against the config dir.
    pub auth_file: PathBuf,
    /// Where the auth file password comes from.
    #[serde(default)]
    pub password_source: PasswordSource,
    /// Command whose stdout is the auth-file passphrase (used when
    /// `password_source = "command"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_command: Option<String>,
    /// Passwords file for `password_source = "file"` (default:
    /// `<config_dir>/passwords`). Keyed by account name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<PathBuf>,
    /// Marketplaces this account is registered/allowed to use (country
    /// codes). `-m all` expands to this set.
    #[serde(default)]
    pub marketplaces: Vec<String>,
    /// Default marketplace set when `-m/--marketplace` and
    /// `AUDIBLE_MARKETPLACE` are unset. Must be a subset of `marketplaces`.
    #[serde(default)]
    pub default_marketplaces: Vec<String>,
    /// Settings bundle used when `-s/--settings` and `AUDIBLE_SETTINGS`
    /// are unset (else `settings.default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_settings: Option<String>,
    /// Path to the account's Widevine CDM (`.wvd`) for the Widevine/DASH
    /// download path (AUD-56). Set by `account widevine fetch|set`; relative
    /// paths resolve against the config dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widevine_cdm: Option<PathBuf>,
}

/// `[settings.<name>]` — a reusable bundle of per-value options, bound to
/// neither an account nor a marketplace. Every field is optional; unset
/// fields inherit from `settings.default`, then the built-in code default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Where downloads land.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
    /// How downloaded files are named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename_mode: Option<FilenameMode>,
    /// Template for `filename_mode = "custom"` (e.g. `%publication%/%fulltitle%`);
    /// variables are documented in `config::filename_template`. `/` creates
    /// folders under `download_dir`. Required when the mode is custom.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename_template: Option<String>,
    /// What `download` does with artifacts that were already fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<OverwritePolicy>,
    /// Cover size(s) for `--get cover`, in pixels (e.g. `["500", "900"]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_size: Option<Vec<String>>,
    /// Chapter title layout(s) for `--get chapter`: `tree`, `flat` or both
    /// (e.g. `["tree", "flat"]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chapter_type: Option<Vec<String>>,
    /// Maximum download file name length in bytes (`0` = no limit). Long
    /// titles are truncated to stay within filesystem limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename_max_length: Option<usize>,
    /// Per-bundle override of the global `[db]` section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db: Option<DbConfig>,
    /// Decrypt a downloaded aaxc to a playable m4b (AUD-27).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decrypt: Option<bool>,
    /// Which tool decrypts (`auto` prefers aaxclean-cli, falls back to ffmpeg).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decrypt_backend: Option<DecryptBackend>,
    /// Keep the source aaxc after a successful decrypt (built-in: `true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decrypt_keep_source: Option<bool>,
    /// Include podcast shows and their episodes in `download` (built-in:
    /// `true`). When `false`, `download` drops podcast parents and episodes
    /// from every selection (AUD-196). A podcast parent given by `--asin`
    /// expands to all its episodes; `--exclude-podcasts` overrides this per
    /// run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_podcasts: Option<bool>,
}

/// The reserved name of the fallback settings bundle.
pub const DEFAULT_SETTINGS_NAME: &str = "default";

/// Cover size meaning "the largest available for this title". A number cannot
/// express it — the maximum differs from title to title (AUD-208).
pub const COVER_SIZE_NATIVE: &str = "native";

/// Upper bound for a *numeric* cover size — a **typo guard**, not a claim about
/// what exists: an oversized request does not fail, it simply returns the
/// largest available, so `5000` typed for `500` would quietly produce far larger
/// files. Above this, [`COVER_SIZE_NATIVE`] is what was meant, which is why the
/// bound costs no capability. Rationale: AUD-208.
pub const COVER_SIZE_MAX: u32 = 4000;

/// Validates one cover size: a positive number of px up to [`COVER_SIZE_MAX`],
/// or [`COVER_SIZE_NATIVE`]. Shared by the config write paths (via
/// [`Config::validate`], which covers `config set`, `settings add/set` and
/// `setup` alike) and by `download --cover-size`.
pub fn validate_cover_size(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case(COVER_SIZE_NATIVE) {
        return Ok(());
    }
    match value.parse::<u32>() {
        Ok(px) if px > 0 && px <= COVER_SIZE_MAX => Ok(()),
        Ok(0) => Err(format!("invalid cover size {value:?}: 0 px is not a size")),
        Ok(_) => Err(format!(
            "cover size {value:?} is above {COVER_SIZE_MAX} px and is almost certainly a typo; \
             use \"{COVER_SIZE_NATIVE}\" for each title's largest available cover"
        )),
        Err(_) => Err(format!(
            "invalid cover size {value:?} — a positive number of px (e.g. 500) \
             or \"{COVER_SIZE_NATIVE}\""
        )),
    }
}

/// Password source for an account's auth file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordSource {
    /// Ask interactively (default).
    #[default]
    Prompt,
    /// Read `AUDIBLE_AUTH_PASSWORD_<NAME>` (falling back to
    /// `AUDIBLE_AUTH_PASSWORD`).
    Env,
    /// Run the account's `password_command`; its stdout is the passphrase.
    Command,
    /// Read the passphrase from the passwords file (`password_file`, or
    /// `<config_dir>/passwords`), keyed by account name.
    File,
}

/// Which tool decrypts a downloaded aaxc to m4b (AUD-27). `Auto` prefers
/// aaxclean-cli and falls back to ffmpeg (≥ 4.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecryptBackend {
    /// aaxclean-cli if available, else ffmpeg.
    #[default]
    Auto,
    /// Force aaxclean-cli.
    Aaxclean,
    /// Force ffmpeg (needs ≥ 4.4 for aaxc).
    Ffmpeg,
}

/// What `download` does when an artifact was already fetched (tracked in
/// the `downloads` table). The built-in default is `Skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverwritePolicy {
    /// Download only what is missing (skip already-recorded artifacts).
    Skip,
    /// Re-download everything that was requested.
    Force,
}

/// How chapter titles are returned: a flat list or a nested tree. The
/// built-in default is `Tree` (what the app requests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChapterType {
    /// One flat list of chapters.
    Flat,
    /// Nested chapters (parts containing chapters).
    Tree,
}

impl ChapterType {
    /// The `chapter_titles_type` query value the API expects.
    pub fn api_value(self) -> &'static str {
        match self {
            ChapterType::Flat => "Flat",
            ChapterType::Tree => "Tree",
        }
    }

    /// Lowercase token used in file names and the `downloads` table, so a
    /// flat and a tree chapter file of the same title stay distinct.
    pub fn as_str(self) -> &'static str {
        match self {
            ChapterType::Flat => "flat",
            ChapterType::Tree => "tree",
        }
    }
}

/// Naming scheme for downloaded files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilenameMode {
    /// Title transliterated to ASCII.
    Ascii,
    /// Title as-is (Unicode).
    Unicode,
    /// ASIN plus ASCII title.
    AsinAscii,
    /// ASIN plus Unicode title.
    AsinUnicode,
    /// User-defined template from `filename_template` (variables + `/` folders);
    /// see [`crate::config::filename_template`]. Requires a template to be set.
    Custom,
}

/// `[db]` (archived architecture §8; used from M2 on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    /// DB directory; default is the platform data dir (paths::data_dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    #[serde(default = "default_auto_sync")]
    pub auto_sync: AutoSync,
    #[serde(default = "default_sync_max_age")]
    pub sync_max_age: String,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default = "default_fts")]
    pub fts: bool,
    #[serde(default = "default_sync_log_retention")]
    pub sync_log_retention: u32,
    #[serde(default = "default_busy_timeout_ms")]
    pub busy_timeout_ms: u64,
    /// Resolve podcast episodes into the episodes table during sync.
    #[serde(default = "default_resolve_podcasts")]
    pub resolve_podcasts: bool,
    /// Record per-item changes (added/changed/removed) to the change_log,
    /// reviewable via `library changes`.
    #[serde(default = "default_record_changes")]
    pub record_changes: bool,
    /// Days to keep change_log entries (pruned at sync end; 0 = keep forever).
    #[serde(default = "default_change_retention_days")]
    pub change_retention_days: u32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            dir: None,
            auto_sync: default_auto_sync(),
            sync_max_age: default_sync_max_age(),
            page_size: default_page_size(),
            fts: default_fts(),
            sync_log_retention: default_sync_log_retention(),
            busy_timeout_ms: default_busy_timeout_ms(),
            resolve_podcasts: default_resolve_podcasts(),
            record_changes: default_record_changes(),
            change_retention_days: default_change_retention_days(),
        }
    }
}

fn default_resolve_podcasts() -> bool {
    true
}

fn default_auto_sync() -> AutoSync {
    AutoSync::Delta
}
fn default_sync_max_age() -> String {
    "6h".to_owned()
}
fn default_page_size() -> u32 {
    1000
}
fn default_fts() -> bool {
    true
}
fn default_sync_log_retention() -> u32 {
    90
}
fn default_busy_timeout_ms() -> u64 {
    5000
}
fn default_record_changes() -> bool {
    true
}
fn default_change_retention_days() -> u32 {
    90
}

/// Library sync behaviour before DB-backed reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSync {
    /// Never sync implicitly.
    None,
    /// Delta sync when the local data is older than `sync_max_age`.
    Delta,
}

/// `[session]` (archived architecture §9; used from M5 on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: String,
    /// Socket directory; default is the platform runtime dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_dir: Option<PathBuf>,
    /// Opt-in TCP listen address for the agent (M5, AUD-119), e.g.
    /// `127.0.0.1:4595`. Off by default; over TCP only app tokens work
    /// (no admin endpoints). Exposing beyond localhost is the operator's
    /// reverse-proxy/TLS responsibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Whether external-host calls (AUD-120) are allowed over the
    /// untrusted TCP listener. Off by default — the account signature
    /// reaching approved hosts from a network client is its own opt-in;
    /// over the local socket they always work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_external_over_tcp: bool,
    /// External hosts **agent** callers may reach (AUD-124) — separate
    /// from `[plugins] allowed_hosts` (different trust domains: a local
    /// plugin vs. a network web-app). Managed via `agent allow-host`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<AllowedHost>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout: default_idle_timeout(),
            socket_dir: None,
            listen: None,
            allow_external_over_tcp: false,
            allowed_hosts: Vec::new(),
        }
    }
}

fn default_idle_timeout() -> String {
    "30m".to_owned()
}

/// Parses durations like `90s`, `30m`, `6h`, `1d`.
pub fn parse_duration(value: &str) -> Result<Duration, ConfigError> {
    let invalid =
        || ConfigError::Invalid(format!("invalid duration {value:?} (use 90s/30m/6h/1d)"));
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let seconds = match unit {
        "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        "d" => number * 86_400,
        _ => return Err(invalid()),
    };
    Ok(Duration::from_secs(seconds))
}

/// Account and settings names: lowercase alphanumerics, `-` and `_`.
/// Keeps file names, env-var derivation and CLI quoting trivial.
pub fn validate_name(name: &str) -> Result<(), ConfigError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "invalid name {name:?}: use lowercase letters, digits, '-' and '_'"
        )))
    }
}

impl Config {
    /// Validates references and value ranges; called after every load and
    /// before every write.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }

        for (name, account) in &self.accounts {
            validate_name(name)?;
            for cc in &account.marketplaces {
                if locale::find(cc).is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "account {name:?} lists unknown marketplace {cc:?}"
                    )));
                }
            }
            for cc in &account.default_marketplaces {
                if !account.marketplaces.iter().any(|m| m == cc) {
                    return Err(ConfigError::Invalid(format!(
                        "account {name:?}: default_marketplaces entry {cc:?} is not in \
                         its marketplaces"
                    )));
                }
            }
            if let Some(settings) = &account.default_settings
                && !self.settings.contains_key(settings)
            {
                return Err(ConfigError::Invalid(format!(
                    "account {name:?} references unknown settings bundle {settings:?}"
                )));
            }
            if account.password_source == PasswordSource::Command
                && account.password_command.is_none()
            {
                return Err(ConfigError::Invalid(format!(
                    "account {name:?}: password_source = \"command\" requires password_command"
                )));
            }
        }
        for (name, settings) in &self.settings {
            validate_name(name)?;
            if let Some(db) = &settings.db {
                parse_duration(&db.sync_max_age)?;
            }
            // A present filename_template must be grammatically valid (known
            // variables, balanced `%…%`) — caught here on write/load, not only
            // at download time.
            if let Some(template) = &settings.filename_template
                && let Err(reason) = super::filename_template::validate(template)
            {
                return Err(ConfigError::Invalid(format!(
                    "settings {name:?} filename_template: {reason}"
                )));
            }
            // Same idea for the cover sizes: rejected on write/load rather than
            // at download time. This one check covers `config set`,
            // `settings add/set` and `setup` at once — they all write through
            // the validated path (AUD-208).
            for size in settings.cover_size.iter().flatten() {
                if let Err(reason) = validate_cover_size(size) {
                    return Err(ConfigError::Invalid(format!(
                        "settings {name:?} cover_size: {reason}"
                    )));
                }
            }
        }
        if let Some(default_account) = &self.default_account
            && !self.accounts.contains_key(default_account)
        {
            return Err(ConfigError::Invalid(format!(
                "default_account references unknown account {default_account:?}"
            )));
        }
        parse_duration(&self.db.sync_max_age)?;
        parse_duration(&self.session.idle_timeout)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_template(template: &str) -> Config {
        let mut config = Config::default();
        config.settings.insert(
            "smoke".to_owned(),
            Settings {
                filename_template: Some(template.to_owned()),
                ..Settings::default()
            },
        );
        config
    }

    #[test]
    fn validate_requires_command_for_command_source() {
        let mut config = Config::default();
        config.accounts.insert(
            "alice".to_owned(),
            Account {
                auth_file: "alice.auth".into(),
                password_source: PasswordSource::Command,
                password_command: None,
                password_file: None,
                marketplaces: vec!["de".into()],
                default_marketplaces: vec!["de".into()],
                default_settings: None,
                widevine_cdm: None,
            },
        );
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("password_command"), "{err}");

        config.accounts.get_mut("alice").unwrap().password_command =
            Some("gpg -d pw.gpg".to_owned());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_invalid_filename_template() {
        // Valid template passes.
        assert!(
            config_with_template("%publication%/%fulltitle% (%release_year%)")
                .validate()
                .is_ok()
        );
        // Grammar/variable errors are caught on validate (i.e. on `config set`),
        // not only later at download time.
        assert!(
            config_with_template("%publication/%fulltitle")
                .validate()
                .is_err()
        );
        assert!(config_with_template("%author%").validate().is_err());
        assert!(config_with_template("%title!x%").validate().is_err());
    }
}
