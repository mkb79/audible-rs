//! Configuration: versioned TOML schema with accounts (identity/auth +
//! marketplace axis) and reusable `[settings.<name>]` bundles, value
//! resolution (CLI flag → env → selected bundle → `settings.default` →
//! built-in default), validation, comment-preserving writes via
//! `toml_edit`, and the shared `Ctx` (eager config, lazy client/auth via
//! `tokio::sync::OnceCell`).

pub mod ctx;
pub mod filename_template;
pub mod passwords;
pub mod paths;
pub mod resolve;
pub mod schema;
pub mod write;

use std::path::Path;

pub use schema::{CONFIG_VERSION, Config};

/// Errors raised while loading, validating or writing the config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Reading or writing the config file failed.
    #[error("config file IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not valid TOML or violates the schema.
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    /// The config schema version is newer than this build understands.
    #[error("unsupported config version {0} (this build understands {CONFIG_VERSION})")]
    UnsupportedVersion(u32),
    /// A reference or value violates the schema rules.
    #[error("{0}")]
    Invalid(String),
    /// An account name did not match any configured account.
    #[error("unknown account {0:?} (see `audible account list`)")]
    UnknownAccount(String),
    /// A settings bundle name did not match any configured bundle.
    #[error("unknown settings bundle {0:?} (see `audible settings list`)")]
    UnknownSettings(String),
}

impl Config {
    /// Loads and validates the config file. A missing file yields the
    /// default (empty) config — `setup` and `account import` create it.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no config file; using defaults");
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
version = 1
default_account = "alice"

[accounts.alice]
auth_file = "alice.auth"
password_source = "env"
marketplaces = ["de", "us"]
default_marketplaces = ["de"]
default_settings = "default"

[settings.default]
download_dir = "~/Audible"

[settings.us]
download_dir = "~/Audible/US"
"#;

    #[test]
    fn parses_the_architecture_example() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        config.validate().unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts["alice"].marketplaces, vec!["de", "us"]);
        assert_eq!(config.accounts["alice"].default_marketplaces, vec!["de"]);
        assert_eq!(config.settings.len(), 2);
        assert_eq!(
            config.accounts["alice"].password_source,
            schema::PasswordSource::Env
        );
        // Untouched sections fall back to their defaults.
        assert_eq!(config.db.page_size, 1000);
        assert_eq!(config.db.auto_sync, schema::AutoSync::Delta);
        assert_eq!(config.session.idle_timeout, "30m");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let result = toml::from_str::<Config>("version = 1\ntypo_key = true\n");
        assert!(result.is_err());
        let result =
            toml::from_str::<Config>("version = 1\n[settings.default]\ndownload_dirr = \"/x\"\n");
        assert!(result.is_err());
        // The old profile model is gone: its sections are now unknown.
        let result = toml::from_str::<Config>(
            "version = 1\n[profiles.p]\naccount = \"a\"\ncountry_code = \"de\"\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn dangling_references_are_rejected() {
        // default_account → unknown account.
        let config: Config = toml::from_str("version = 1\ndefault_account = \"ghost\"\n").unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("ghost"));

        // account.default_settings → unknown bundle.
        let config: Config = toml::from_str(
            "version = 1\n[accounts.a]\nauth_file = \"a.auth\"\ndefault_settings = \"ghost\"\n",
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_marketplace_is_rejected() {
        let config: Config = toml::from_str(
            "version = 1\n[accounts.a]\nauth_file = \"a.auth\"\nmarketplaces = [\"zz\"]\n",
        )
        .unwrap();
        assert!(config.validate().is_err());

        // default_marketplaces must be a subset of marketplaces.
        let config: Config = toml::from_str(
            "version = 1\n[accounts.a]\nauth_file = \"a.auth\"\n\
             marketplaces = [\"de\"]\ndefault_marketplaces = [\"us\"]\n",
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn future_version_is_rejected() {
        let config: Config = toml::from_str("version = 2\n").unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn durations_are_validated() {
        schema::parse_duration("6h").unwrap();
        schema::parse_duration("30m").unwrap();
        schema::parse_duration("90s").unwrap();
        schema::parse_duration("1d").unwrap();
        assert!(schema::parse_duration("6 hours").is_err());
        assert!(schema::parse_duration("h").is_err());
        assert!(schema::parse_duration("-5m").is_err());

        let config: Config =
            toml::from_str("version = 1\n[db]\nsync_max_age = \"soon\"\n").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn name_rules() {
        schema::validate_name("alice-de_2").unwrap();
        assert!(schema::validate_name("Alice").is_err());
        assert!(schema::validate_name("a b").is_err());
        assert!(schema::validate_name("").is_err());
    }

    #[test]
    fn missing_file_yields_default_config() {
        let config = Config::load(Path::new("/definitely/not/here.toml")).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(config.accounts.is_empty());
    }
}
