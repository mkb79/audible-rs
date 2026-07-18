//! Per-value resolution:
//! CLI flag → env (`AUDIBLE_*`) → selected `[settings.<name>]` bundle →
//! `[settings.default]` bundle → built-in default.
//!
//! Account, settings bundle and marketplace set are resolved up front;
//! the per-value getters then walk the settings fallback chain. The
//! functions are pure: callers fetch env values themselves, which keeps
//! tests deterministic and the precedence explicit.

use std::path::PathBuf;

use crate::api::locale;

use super::ConfigError;
use super::schema::{
    Account, Config, DEFAULT_SETTINGS_NAME, DecryptBackend, FilenameMode, OverwritePolicy, Settings,
};

/// Resolves the account name to operate on:
/// `-a/--account` → `AUDIBLE_ACCOUNT` → `default_account` → the sole
/// account. Errors when none applies (no account, or several without a
/// default).
pub fn account_name<'a>(
    cli: Option<&'a str>,
    env: Option<&'a str>,
    config: &'a Config,
) -> Result<&'a str, ConfigError> {
    if let Some(name) = cli.or(env) {
        return if config.accounts.contains_key(name) {
            Ok(name)
        } else {
            Err(ConfigError::UnknownAccount(name.to_owned()))
        };
    }
    if let Some(default) = config.default_account.as_deref() {
        // Existence is enforced by validate(), but re-check defensively.
        return if config.accounts.contains_key(default) {
            Ok(default)
        } else {
            Err(ConfigError::UnknownAccount(default.to_owned()))
        };
    }
    let mut accounts = config.accounts.keys();
    match (accounts.next(), accounts.next()) {
        (Some(only), None) => Ok(only.as_str()),
        (None, _) => Err(ConfigError::Invalid(
            "no account configured: register one with `audible account login` \
             (or import with `audible account import`)"
                .into(),
        )),
        _ => Err(ConfigError::Invalid(
            "no account selected: pass -a/--account, set AUDIBLE_ACCOUNT or configure \
             default_account"
                .into(),
        )),
    }
}

/// Looks up an account, with a helpful error for unknown names.
pub fn account<'a>(config: &'a Config, name: &str) -> Result<&'a Account, ConfigError> {
    config
        .accounts
        .get(name)
        .ok_or_else(|| ConfigError::UnknownAccount(name.to_owned()))
}

/// The settings bundle name to use:
/// `-s/--settings` → `AUDIBLE_SETTINGS` → `account.default_settings` →
/// `"default"`.
pub fn settings_name<'a>(
    cli: Option<&'a str>,
    env: Option<&'a str>,
    account: &'a Account,
) -> &'a str {
    cli.or(env)
        .or(account.default_settings.as_deref())
        .unwrap_or(DEFAULT_SETTINGS_NAME)
}

/// The selected settings bundle plus the `default` bundle, forming the
/// per-value fallback chain (selected → default → code default). Either
/// may be absent (an unconfigured bundle resolves to code defaults).
///
/// A *named* bundle (`-s foo`) that does not exist is an error; the
/// implicit `default` bundle is allowed to be missing.
pub struct SettingsView<'a> {
    selected: Option<&'a Settings>,
    default: Option<&'a Settings>,
}

impl<'a> SettingsView<'a> {
    /// Resolves the view for `name`. When `name` is the reserved
    /// `default`, the selected and default layers coincide.
    pub fn resolve(config: &'a Config, name: &str) -> Result<Self, ConfigError> {
        let default = config.settings.get(DEFAULT_SETTINGS_NAME);
        if name == DEFAULT_SETTINGS_NAME {
            return Ok(Self {
                selected: default,
                default,
            });
        }
        let selected = config
            .settings
            .get(name)
            .ok_or_else(|| ConfigError::UnknownSettings(name.to_owned()))?;
        Ok(Self {
            selected: Some(selected),
            default,
        })
    }

    /// Walks selected → default, returning the first set value.
    fn pick<T>(&self, get: impl Fn(&'a Settings) -> Option<T>) -> Option<T> {
        self.selected
            .and_then(&get)
            .or_else(|| self.default.and_then(&get))
    }

    /// Resolves the download directory.
    pub fn download_dir(&self, cli: Option<PathBuf>, env: Option<PathBuf>) -> Option<PathBuf> {
        cli.or(env)
            .or_else(|| self.pick(|s| s.download_dir.clone()))
    }

    /// Resolves the filename mode (built-in default: ascii).
    pub fn filename_mode(
        &self,
        cli: Option<FilenameMode>,
        env: Option<FilenameMode>,
    ) -> FilenameMode {
        cli.or(env)
            .or_else(|| self.pick(|s| s.filename_mode))
            .unwrap_or(FilenameMode::Ascii)
    }

    /// Resolves the custom filename template (no built-in default — a custom
    /// `filename_mode` requires one to be set explicitly).
    pub fn filename_template(&self) -> Option<String> {
        self.pick(|s| s.filename_template.clone())
    }

    /// Resolves the overwrite policy (built-in default: skip).
    pub fn overwrite(
        &self,
        cli: Option<OverwritePolicy>,
        env: Option<OverwritePolicy>,
    ) -> OverwritePolicy {
        cli.or(env)
            .or_else(|| self.pick(|s| s.overwrite))
            .unwrap_or(OverwritePolicy::Skip)
    }

    /// Resolves whether downloaded aaxc is decrypted to m4b (built-in: `false`).
    pub fn decrypt(&self) -> bool {
        self.pick(|s| s.decrypt).unwrap_or(false)
    }

    /// Resolves the decrypt backend (built-in: `Auto`).
    pub fn decrypt_backend(&self) -> DecryptBackend {
        self.pick(|s| s.decrypt_backend).unwrap_or_default()
    }

    /// Resolves whether the source aaxc is kept after decrypt (built-in: `true`).
    pub fn decrypt_keep_source(&self) -> bool {
        self.pick(|s| s.decrypt_keep_source).unwrap_or(true)
    }

    /// Resolves whether `download` includes podcasts/episodes (built-in:
    /// `true`; AUD-196).
    pub fn include_podcasts(&self) -> bool {
        self.pick(|s| s.include_podcasts).unwrap_or(true)
    }

    /// Resolves the default cover size(s) (built-in: `["500"]`).
    pub fn cover_size(&self, cli: Option<Vec<String>>, env: Option<Vec<String>>) -> Vec<String> {
        cli.or(env)
            .or_else(|| self.pick(|s| s.cover_size.clone()))
            .unwrap_or_else(|| vec!["500".to_owned()])
    }

    /// Resolves the chapter title layout(s) (built-in: `["tree"]`).
    pub fn chapter_type(&self, cli: Option<Vec<String>>, env: Option<Vec<String>>) -> Vec<String> {
        cli.or(env)
            .or_else(|| self.pick(|s| s.chapter_type.clone()))
            .unwrap_or_else(|| vec!["tree".to_owned()])
    }

    /// Resolves the maximum file name length (bytes). `0` disables the
    /// limit; built-in default is [`DEFAULT_FILENAME_MAX_LENGTH`].
    pub fn filename_max_length(&self, cli: Option<usize>, env: Option<usize>) -> usize {
        cli.or(env)
            .or_else(|| self.pick(|s| s.filename_max_length))
            .unwrap_or(DEFAULT_FILENAME_MAX_LENGTH)
    }

    /// The selected bundle's `db` override, if any (for [`db_config`]).
    pub fn db_override(&self) -> Option<&'a super::schema::DbConfig> {
        self.pick(|s| s.db.as_ref())
    }
}

/// Resolves the marketplace set: `-m/--marketplace` → `AUDIBLE_MARKETPLACE`
/// → the account's `default_marketplaces`. The literal `all` expands to
/// the account's `marketplaces`. Every entry must be a known marketplace
/// and registered for the account. Order is preserved, duplicates dropped.
pub fn marketplaces(
    cli: Option<&str>,
    env: Option<&str>,
    account: &Account,
) -> Result<Vec<String>, ConfigError> {
    let requested: Vec<String> = match cli.or(env) {
        Some(raw) if raw.eq_ignore_ascii_case("all") => account.marketplaces.clone(),
        Some(raw) => raw
            .split(',')
            .map(|token| token.trim().to_ascii_lowercase())
            .filter(|token| !token.is_empty())
            .collect(),
        None => account.default_marketplaces.clone(),
    };
    if requested.is_empty() {
        return Err(ConfigError::Invalid(
            "no marketplace selected: pass -m/--marketplace, set AUDIBLE_MARKETPLACE or \
             configure the account's default_marketplaces"
                .into(),
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(requested.len());
    for cc in requested {
        locale::require(&cc).map_err(ConfigError::Invalid)?;
        if !account.marketplaces.iter().any(|m| m == &cc) {
            return Err(ConfigError::Invalid(format!(
                "marketplace {cc:?} is not registered for this account (add it with \
                 `audible account marketplaces add {cc}`)"
            )));
        }
        if !out.contains(&cc) {
            out.push(cc);
        }
    }
    Ok(out)
}

/// Built-in maximum file name length in bytes (leaves room under the
/// common 255-byte filesystem limit for the extension and `.part`).
pub const DEFAULT_FILENAME_MAX_LENGTH: usize = 230;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::PasswordSource;

    fn config() -> Config {
        let mut config = Config::default();
        config.accounts.insert(
            "alice".into(),
            Account {
                auth_file: "alice.auth".into(),
                password_source: PasswordSource::Prompt,
                password_command: None,
                password_file: None,
                marketplaces: vec!["de".into(), "us".into()],
                default_marketplaces: vec!["de".into()],
                default_settings: None,
                widevine_cdm: None,
            },
        );
        config.settings.insert(
            DEFAULT_SETTINGS_NAME.into(),
            Settings {
                download_dir: Some("/defaults-dir".into()),
                cover_size: Some(vec!["500".to_owned()]),
                ..Settings::default()
            },
        );
        config.settings.insert(
            "fast".into(),
            Settings {
                download_dir: Some("/fast-dir".into()),
                filename_mode: Some(FilenameMode::Unicode),
                overwrite: Some(OverwritePolicy::Force),
                ..Settings::default()
            },
        );
        config.default_account = Some("alice".into());
        config
    }

    #[test]
    fn account_resolution_chain() {
        let config = config();
        // CLI beats env beats default_account.
        assert_eq!(account_name(Some("alice"), None, &config).unwrap(), "alice");
        assert_eq!(account_name(None, None, &config).unwrap(), "alice");
        assert!(matches!(
            account_name(Some("ghost"), None, &config),
            Err(ConfigError::UnknownAccount(_))
        ));
    }

    #[test]
    fn sole_account_needs_no_default() {
        let mut config = config();
        config.default_account = None;
        // Exactly one account → picked implicitly.
        assert_eq!(account_name(None, None, &config).unwrap(), "alice");
        // Two accounts and no default → ambiguous.
        config.accounts.insert(
            "other".into(),
            Account {
                auth_file: "other.auth".into(),
                password_source: PasswordSource::Prompt,
                password_command: None,
                password_file: None,
                marketplaces: vec!["de".into()],
                default_marketplaces: vec!["de".into()],
                default_settings: None,
                widevine_cdm: None,
            },
        );
        let error = account_name(None, None, &config).unwrap_err();
        assert!(error.to_string().contains("-a/--account"));
    }

    #[test]
    fn settings_fallback_chain() {
        let config = config();
        // "fast" sets its own download_dir + unicode; cover_size falls
        // back to the default bundle; chapter_type to the code default.
        let fast = SettingsView::resolve(&config, "fast").unwrap();
        assert_eq!(fast.download_dir(None, None), Some("/fast-dir".into()));
        assert_eq!(fast.filename_mode(None, None), FilenameMode::Unicode);
        assert_eq!(fast.cover_size(None, None), vec!["500".to_owned()]); // from default bundle
        assert_eq!(fast.chapter_type(None, None), vec!["tree".to_owned()]); // code default
        // CLI wins over everything.
        assert_eq!(
            fast.download_dir(Some("/cli".into()), None),
            Some("/cli".into())
        );

        // The default view: selected == default layer.
        let default = SettingsView::resolve(&config, "default").unwrap();
        assert_eq!(
            default.download_dir(None, None),
            Some("/defaults-dir".into())
        );
        assert_eq!(default.overwrite(None, None), OverwritePolicy::Skip);

        // A named, unconfigured bundle is an error.
        assert!(matches!(
            SettingsView::resolve(&config, "ghost"),
            Err(ConfigError::UnknownSettings(_))
        ));
    }

    #[test]
    fn marketplace_set_parsing() {
        let config = config();
        let account = &config.accounts["alice"];
        // Default set.
        assert_eq!(marketplaces(None, None, account).unwrap(), vec!["de"]);
        // CSV, deduped, order-preserving.
        assert_eq!(
            marketplaces(Some("us,de,us"), None, account).unwrap(),
            vec!["us", "de"]
        );
        // `all` → the account's marketplaces.
        assert_eq!(
            marketplaces(Some("all"), None, account).unwrap(),
            vec!["de", "us"]
        );
        // Unknown cc, and a cc not registered for the account.
        assert!(marketplaces(Some("zz"), None, account).is_err());
        assert!(marketplaces(Some("fr"), None, account).is_err());
    }
}
