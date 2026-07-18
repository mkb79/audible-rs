//! Shared command context (D10): eager config, lazy client via
//! `tokio::sync::OnceCell`. `&Ctx` is `Sync` and shareable as `Arc<Ctx>`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use secrecy::SecretString;
use tokio::sync::OnceCell;

use crate::api::client::Client;
use crate::auth::AuthError;
use crate::auth::Authenticator;
use crate::auth::authfile::AuthFileError;
use crate::auth::legacy::LegacyError;
use crate::output::{self, Output, OutputFormat};

use super::resolve::SettingsView;
use super::schema::{Account, Config, PasswordSource};
use super::{passwords, paths, resolve};

/// A `prompt`-source account whose passphrase cannot be asked for
/// headlessly (audit 2026-07-17, E5). Typed — not a bare message — so the
/// `/v1` router can recognize it in an anyhow chain and answer 423
/// (Locked) instead of a generic 500 (audit 2026-07-18, A7).
#[derive(Debug)]
pub struct AccountLocked {
    /// The locked account's name.
    pub account: String,
}

impl std::fmt::Display for AccountLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "account {:?} is locked (password_source = prompt) — unlock it with \
             `audible account unlock -a {}`",
            self.account, self.account
        )
    }
}

impl std::error::Error for AccountLocked {}

/// The three global selectors (`-a/-s/-m`), as passed from the CLI.
#[derive(Debug, Clone, Default)]
pub struct Selectors {
    /// `-a/--account`.
    pub account: Option<String>,
    /// `-s/--settings`.
    pub settings: Option<String>,
    /// `-m/--marketplace`.
    pub marketplace: Option<String>,
}

/// Context handed to every command.
pub struct Ctx {
    config: Config,
    config_dir: PathBuf,
    selectors: Selectors,
    output: OutputFormat,
    client: OnceCell<Client>,
    /// The account's library DB, opened once per process (one connection
    /// thread); handles are cheap clones sharing it.
    db: OnceCell<crate::db::Db>,
}

impl Ctx {
    /// Creates the context from the platform config location.
    pub fn new(selectors: Selectors) -> Result<Self> {
        Self::with_dir(paths::config_dir(), selectors)
    }

    /// Creates the context from an explicit config directory (tests).
    pub fn with_dir(config_dir: PathBuf, selectors: Selectors) -> Result<Self> {
        let config_file = config_dir.join(paths::CONFIG_FILE_NAME);
        let config = Config::load(&config_file)
            .with_context(|| format!("could not load {}", config_file.display()))?;
        Ok(Self::with_config(config_dir, config, selectors))
    }

    /// Creates the context from an **already-loaded** config — the
    /// agent's mtime-cached one (audit 2026-07-17, E4: the daemon
    /// re-read and re-parsed `config.toml` on every `/v1` request and
    /// gained no freshness by it). The config must come from
    /// [`Config::load`], which validated it.
    pub fn with_config(config_dir: PathBuf, config: Config, selectors: Selectors) -> Self {
        Self {
            config,
            config_dir,
            selectors,
            output: OutputFormat::default(),
            client: OnceCell::new(),
            db: OnceCell::new(),
        }
    }

    /// Sets the rendering format (global `--output` flag).
    pub fn with_output(mut self, format: OutputFormat) -> Self {
        self.output = format;
        self
    }

    /// The selected rendering format.
    pub fn output_format(&self) -> OutputFormat {
        self.output
    }

    /// Renders structured command output to stdout.
    pub fn print(&self, output: &Output) {
        output::print(output, self.output);
    }

    /// The loaded (validated) config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Directory holding `config.toml` and the auth files.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Full path of `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(paths::CONFIG_FILE_NAME)
    }

    /// Resolves the account name to operate on:
    /// `-a/--account` → `AUDIBLE_ACCOUNT` → `default_account` → the sole
    /// account.
    pub fn account_name(&self) -> Result<String> {
        let env = std::env::var("AUDIBLE_ACCOUNT").ok();
        let name = resolve::account_name(
            self.selectors.account.as_deref(),
            env.as_deref(),
            &self.config,
        )?;
        Ok(name.to_owned())
    }

    /// The resolved account.
    pub fn account(&self) -> Result<&Account> {
        let name = self.account_name()?;
        Ok(resolve::account(&self.config, &name)?)
    }

    /// The settings fallback view (selected bundle → `settings.default`):
    /// `-s/--settings` → `AUDIBLE_SETTINGS` → `account.default_settings`
    /// → `"default"`.
    pub fn settings_view(&self) -> Result<SettingsView<'_>> {
        let name = self.settings_name()?;
        Ok(SettingsView::resolve(&self.config, &name)?)
    }

    /// The resolved settings bundle name (`-s/--settings` → `AUDIBLE_SETTINGS`
    /// → `account.default_settings` → `"default"`). Used to write settings to
    /// the right `[settings.<name>]` (e.g. `download reorganize`).
    pub fn settings_name(&self) -> Result<String> {
        let account = self.account()?;
        let env = std::env::var("AUDIBLE_SETTINGS").ok();
        Ok(
            resolve::settings_name(self.selectors.settings.as_deref(), env.as_deref(), account)
                .to_owned(),
        )
    }

    /// The resolved marketplace set (`-m/--marketplace` → `AUDIBLE_MARKETPLACE`
    /// → the account's `default_marketplaces`; `all` → its `marketplaces`).
    pub fn marketplaces(&self) -> Result<Vec<String>> {
        let account = self.account()?;
        let env = std::env::var("AUDIBLE_MARKETPLACE").ok();
        Ok(resolve::marketplaces(
            self.selectors.marketplace.as_deref(),
            env.as_deref(),
            account,
        )?)
    }

    /// The raw `-m/--marketplace` selector (or `AUDIBLE_MARKETPLACE`), before
    /// any account-based resolution. Used by `account login`, which registers a
    /// brand-new account and so has no marketplace axis to resolve against yet.
    pub fn marketplace_selector(&self) -> Option<String> {
        self.selectors
            .marketplace
            .clone()
            .or_else(|| std::env::var("AUDIBLE_MARKETPLACE").ok())
    }

    /// The single marketplace for single-host commands (`api`, direct
    /// `download`). Errors when the set is not exactly one.
    pub fn marketplace_single(&self) -> Result<String> {
        let mut set = self.marketplaces()?;
        match set.len() {
            1 => Ok(set.pop().expect("len checked")),
            n => bail!(
                "this command works on a single marketplace, but -m/--marketplace \
                 selected {n} ({}); narrow it with -m <cc>",
                set.join(",")
            ),
        }
    }

    /// Whether the client cell is already initialized (the session is
    /// unlocked). Used by the agent's `unlock` (AUD-116).
    pub fn is_unlocked(&self) -> bool {
        self.client.initialized()
    }

    /// Unlocks this context's client with an **explicit** passphrase and
    /// caches it, for the agent's `account unlock` (AUD-116) — bypasses
    /// `password_source`/prompt so `prompt` accounts can be opened
    /// headless. Errors if already unlocked or the passphrase is wrong;
    /// the passphrase is dropped (zeroized) at the end of the call.
    pub async fn unlock_with_password(&self, password: SecretString) -> Result<()> {
        if self.client.initialized() {
            bail!("account is already unlocked");
        }
        let account_name = self.account_name()?;
        let account = resolve::account(&self.config, &account_name)?;
        let auth_path = self.auth_path(account);
        let auth = Authenticator::load_file(&auth_path, Some(password))
            .await
            .context("could not unlock the account with this passphrase")?;
        let client = Client::builder(auth).build()?;
        // Another task may have unlocked concurrently; that is fine.
        let _ = self.client.set(client);
        Ok(())
    }

    /// Resolves an account's auth-file path: an absolute `auth_file` as-is,
    /// else relative to the config dir. One rule (audit 2026-07-18, D2) —
    /// the loaders and the `account remove`/`logout` teardown must resolve
    /// the **identical** file, or logout could delete a different file than
    /// the loader opens.
    pub fn auth_path(&self, account: &Account) -> PathBuf {
        if account.auth_file.is_absolute() {
            account.auth_file.clone()
        } else {
            self.config_dir.join(&account.auth_file)
        }
    }

    /// The API client for the selected account, built once per process.
    /// The client carries no marketplace default (it uses the account's
    /// registration locale); marketplace-specific requests set their own
    /// `country_code`.
    pub async fn client(&self) -> Result<&Client> {
        self.client
            .get_or_try_init(|| async {
                let auth = self.authenticator().await?;
                Ok(Client::builder(auth).build()?)
            })
            .await
    }

    /// Loads the auth material of the selected account, honoring its
    /// `password_source`.
    pub async fn authenticator(&self) -> Result<Authenticator> {
        let account_name = self.account_name()?;
        let account = resolve::account(&self.config, &account_name)?;

        let auth_path = self.auth_path(account);

        let password = resolve_password(&self.config_dir, &account_name, account).await?;
        let prompt_allowed = account.password_source == PasswordSource::Prompt;

        match Authenticator::load_file(&auth_path, password).await {
            Ok(auth) => Ok(auth),
            Err(error) if prompt_allowed && password_required(&error) => {
                let term = console::Term::stderr();
                // Headless callers (the agent's session map) cannot answer
                // a prompt — fail with the actionable state instead of a
                // cryptic read error (audit 2026-07-17, E5). Typed so the
                // `/v1` router can answer 423 instead of a 500.
                if !term.is_term() {
                    return Err(AccountLocked {
                        account: account_name,
                    }
                    .into());
                }
                term.write_str(&format!("Password for account {account_name:?}: "))?;
                let password = SecretString::from(term.read_secure_line()?);
                Authenticator::load_file(&auth_path, Some(password))
                    .await
                    .context("could not open the auth file with this password")
            }
            Err(error) => Err(anyhow::Error::new(error)
                .context(format!("could not load {}", auth_path.display()))),
        }
    }

    /// Resolved `[db]` settings: the selected settings bundle's `db`
    /// override wins over the global `[db]` section.
    pub(crate) fn db_config(&self) -> Result<super::schema::DbConfig> {
        let view = self.settings_view()?;
        Ok(view
            .db_override()
            .cloned()
            .unwrap_or_else(|| self.config.db.clone()))
    }

    /// Path of the selected account's library database
    /// (`account_{sha256(user_id)[..16]}.sqlite` under `db.dir`),
    /// resolved without opening the file — for `db backup`/`restore`.
    /// One file per account; the marketplace is a column inside the DB.
    pub(crate) async fn library_db_path(&self) -> Result<PathBuf> {
        let client = self.client().await?;
        let user_id = client.customer_id().context(
            "auth data carries no customer_info.user_id — re-import the account \
             from a freshly used Python auth file",
        )?;
        let dir = self
            .db_config()?
            .dir
            .clone()
            .unwrap_or_else(|| paths::data_dir().join("db"));
        Ok(dir.join(crate::db::account_file_name(user_id)))
    }

    /// Opens (and migrates) the selected account's library database —
    /// once per process; later calls get a cheap clone of the cached
    /// handle (same connection thread). `db reset`/`restore` never open
    /// the DB (they use [`Self::library_db_path`]), so the cache cannot
    /// hold a handle to a replaced file.
    pub(crate) async fn open_library_db(&self) -> Result<crate::db::Db> {
        let db = self
            .db
            .get_or_try_init(|| async {
                let path = self.library_db_path().await?;
                let busy_timeout_ms = self.db_config()?.busy_timeout_ms;
                anyhow::Ok(crate::db::Db::open(path, busy_timeout_ms).await?)
            })
            .await?;
        Ok(db.clone())
    }
}

/// `AUDIBLE_AUTH_PASSWORD_<NAME>` (name uppercased, `-` → `_`), falling
/// back to `AUDIBLE_AUTH_PASSWORD`.
fn env_password(account_name: &str) -> Result<SecretString> {
    let specific = format!(
        "AUDIBLE_AUTH_PASSWORD_{}",
        account_name.to_ascii_uppercase().replace('-', "_")
    );
    for var in [specific.as_str(), "AUDIBLE_AUTH_PASSWORD"] {
        if let Ok(value) = std::env::var(var) {
            return Ok(SecretString::from(value));
        }
    }
    bail!("password_source = \"env\": neither {specific} nor AUDIBLE_AUTH_PASSWORD is set")
}

/// Resolves the non-interactive auth-file passphrase for an account per its
/// `password_source`. `Ok(None)` for `prompt` — the caller prompts on
/// demand. Blocking work (command/file IO) runs off the async executor.
pub(crate) async fn resolve_password(
    config_dir: &Path,
    account_name: &str,
    account: &Account,
) -> Result<Option<SecretString>> {
    match account.password_source {
        PasswordSource::Prompt => Ok(None),
        PasswordSource::Env => Ok(Some(env_password(account_name)?)),
        PasswordSource::Command => {
            let command = account.password_command.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "account {account_name:?}: password_source = \"command\" but \
                     password_command is unset"
                )
            })?;
            Ok(Some(run_password_command(&command).await?))
        }
        PasswordSource::File => {
            let path = passwords::resolve_path(config_dir, account);
            let lookup_path = path.clone();
            let account_owned = account_name.to_owned();
            let found = tokio::task::spawn_blocking(move || {
                passwords::lookup(&lookup_path, &account_owned)
            })
            .await
            .expect("blocking passwords lookup must not panic")?;
            found.map(Some).ok_or_else(|| {
                anyhow::anyhow!(
                    "no passwords entry for account {account_name:?} in {} — add it with \
                     `audible account password source {account_name} file`",
                    path.display()
                )
            })
        }
    }
}

/// Runs `command` through the shell and returns its stdout as the passphrase
/// (one trailing newline stripped). Never surfaces stdout in errors.
pub(crate) async fn run_password_command(command: &str) -> Result<SecretString> {
    let command = command.to_owned();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = if cfg!(windows) {
            let mut cmd = std::process::Command::new("cmd");
            cmd.arg("/C").arg(&command);
            cmd
        } else {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(&command);
            cmd
        };
        cmd.output()
    })
    .await
    .expect("blocking password command must not panic")
    .context("could not run password_command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("password_command failed ({})", output.status);
        }
        bail!("password_command failed ({}): {stderr}", output.status);
    }

    let mut pass =
        String::from_utf8(output.stdout).context("password_command output is not UTF-8")?;
    if pass.ends_with('\n') {
        pass.pop();
        if pass.ends_with('\r') {
            pass.pop();
        }
    }
    Ok(SecretString::from(pass))
}

fn password_required(error: &AuthError) -> bool {
    matches!(
        error,
        AuthError::File(AuthFileError::PasswordRequired)
            | AuthError::Legacy(LegacyError::PasswordRequired)
    )
}

#[cfg(test)]
mod password_tests {
    use super::*;
    use crate::config::schema::{Account, PasswordSource};
    use secrecy::ExposeSecret as _;

    fn account(source: PasswordSource) -> Account {
        Account {
            auth_file: "alice.auth".into(),
            password_source: source,
            password_command: None,
            password_file: None,
            marketplaces: vec!["de".into()],
            default_marketplaces: vec!["de".into()],
            default_settings: None,
            widevine_cdm: None,
        }
    }

    #[tokio::test]
    async fn prompt_resolves_to_none() {
        let acc = account(PasswordSource::Prompt);
        let got = resolve_password(Path::new("/nonexistent"), "alice", &acc)
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn command_runs_the_shell_and_strips_one_newline() {
        let mut acc = account(PasswordSource::Command);
        acc.password_command = Some("echo hunter2".to_owned());
        let got = resolve_password(Path::new("/nonexistent"), "alice", &acc)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.expose_secret(), "hunter2");
    }

    #[tokio::test]
    async fn command_failure_is_an_error_without_leaking_stdout() {
        let mut acc = account(PasswordSource::Command);
        // The command prints to stdout, then exits non-zero. The shell differs
        // by platform (sh -c vs cmd /C), so the command syntax does too — this
        // way the failure path is really exercised on each, not just Unix.
        #[cfg(unix)]
        let command = "echo secret-should-not-appear; exit 3";
        #[cfg(windows)]
        let command = "echo secret-should-not-appear& exit 3";
        acc.password_command = Some(command.to_owned());
        let err = resolve_password(Path::new("/nonexistent"), "alice", &acc)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("password_command failed"), "{err}");
        assert!(
            !err.contains("secret-should-not-appear"),
            "stdout leaked: {err}"
        );
    }

    #[tokio::test]
    async fn file_looks_up_the_account_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = passwords::default_path(dir.path());
        passwords::upsert(&path, "alice", &SecretString::from("filepass")).unwrap();
        let acc = account(PasswordSource::File);
        let got = resolve_password(dir.path(), "alice", &acc)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.expose_secret(), "filepass");
    }

    #[tokio::test]
    async fn file_missing_entry_points_at_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let acc = account(PasswordSource::File);
        let err = resolve_password(dir.path(), "alice", &acc)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no passwords entry"), "{err}");
        assert!(err.contains("account password source"), "{err}");
    }
}
