//! CLI commands (archived architecture §5): one module per noun group and a
//! dynamic registry of [`Command`] trait objects (D10). Typed arguments
//! live in `#[derive(Args)]` structs; `run` re-parses them via
//! `from_arg_matches`. Old Python command names map to hints in
//! [`old_command_hint`].

pub mod account;
pub mod agent;
pub mod annotations;
pub mod api;
pub mod collections;
pub mod completions;
pub mod config;
pub mod db;
pub mod download;
pub(crate) mod hosts;
pub mod items;
pub mod library;
pub mod plugin;
pub mod podcasts;
pub(crate) mod prompt;
pub mod series;
pub mod settings;
pub mod setup;

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::{Arg, ArgAction};
use secrecy::SecretString;

use crate::auth::authfile::AuthFileError;
use crate::auth::legacy::LegacyError;
use crate::auth::{AuthError, Authenticator};
use crate::config::ctx::Ctx;
use crate::output::OutputFormat;

/// Help section of the global flags; separates them from
/// command-specific options in every `--help` output.
pub(crate) const GLOBAL_OPTIONS: &str = "Global Options";

fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    s.parse()
}

/// Builds the full top-level `audible` clap tree: the root with its global
/// flags plus every registered subcommand. Used both to parse/dispatch
/// (`main`) and by the meta-commands that need the whole tree, e.g.
/// `completions` (AUD-143) and, later, man-page generation.
pub fn build_root(registry: &[Box<dyn Command>]) -> clap::Command {
    let mut root = clap::Command::new("audible")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Access your Audible library from the command line")
        .subcommand_required(true)
        .arg_required_else_help(true)
        // Unknown names fall through to plugin discovery (AUD-68).
        // Built-ins are registered subcommands and therefore always win.
        .allow_external_subcommands(true)
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::Count)
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .help("Increase log verbosity (-v info, -vv debug, -vvv trace)")
                .long_help(
                    "Increase log verbosity (-v info, -vv debug, -vvv trace).\n\
                     Verbosity never changes WHAT is logged: credentials appear \
                     at no level. Explicit flags take precedence over RUST_LOG.",
                ),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .conflicts_with("verbose")
                .help("Only log errors"),
        )
        .arg(
            Arg::new("account")
                .short('a')
                .long("account")
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .value_name("NAME")
                .help("Account to use (default: AUDIBLE_ACCOUNT, then default_account)"),
        )
        .arg(
            Arg::new("settings")
                .short('s')
                .long("settings")
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .value_name("NAME")
                .help("Settings bundle (default: AUDIBLE_SETTINGS, then the account's default_settings, then \"default\")"),
        )
        .arg(
            Arg::new("marketplace")
                .short('m')
                .long("marketplace")
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .value_name("CC|CSV|all")
                .help("Marketplace(s): a country code, a comma list, or \"all\" (default: AUDIBLE_MARKETPLACE, then the account's default_marketplaces)"),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .global(true)
                .help_heading(GLOBAL_OPTIONS)
                .value_name("FORMAT")
                .value_parser(parse_output_format)
                .help("Output format: table (default), json or plain"),
        );
    for command in registry {
        root = root.subcommand(command.clap());
    }
    root
}

/// A CLI command: clap definition plus async execution against `&Ctx`.
#[async_trait::async_trait]
pub trait Command: Send + Sync {
    /// The subcommand's name — also the name of the [`Command::clap`]
    /// tree, kept separate so dispatch and the plugin collision guard
    /// never have to build the whole tree just to read it.
    fn name(&self) -> &'static str;
    /// The clap subcommand definition.
    fn clap(&self) -> clap::Command;
    /// Runs the command with the matches of its subcommand.
    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()>;
}

/// All built-in commands. The registry is open by design (D10); kept
/// alphabetical — the order is also the top-level `--help` order.
pub fn registry() -> Vec<Box<dyn Command>> {
    vec![
        Box::new(account::AccountCommand),
        Box::new(agent::AgentCommand),
        Box::new(annotations::AnnotationsCommand),
        Box::new(api::ApiCommand),
        Box::new(collections::CollectionsCommand),
        Box::new(completions::CompletionsCommand),
        Box::new(config::ConfigCommand),
        Box::new(db::DbCommand),
        Box::new(download::DownloadCommand),
        Box::new(library::LibraryCommand),
        Box::new(plugin::PluginCommand),
        Box::new(podcasts::PodcastsCommand),
        Box::new(series::SeriesCommand),
        Box::new(settings::SettingsCommand),
        Box::new(setup::SetupCommand),
    ]
}

/// Migration hint for old Python command names (D2).
pub fn old_command_hint(name: &str) -> Option<&'static str> {
    match name {
        "quickstart" => Some(
            "`quickstart` is now `audible setup` \
                 (accounts: `audible account login` or `audible account import`)",
        ),
        "manage" => Some(
            "`manage` is split into `audible account`, `audible settings` and `audible config`",
        ),
        "profile" => Some(
            "`profile` is gone: marketplace is now the global `-m/--marketplace`; reusable \
             options live in `audible settings`",
        ),
        "activation-bytes" => Some("`activation-bytes` is now `audible account activation-bytes`"),
        "wishlist" => Some(
            "`wishlist` is now `audible collections wishlist list|add|remove` \
             (the wishlist is one of the account's server-side lists)",
        ),
        _ => None,
    }
}

/// Loads the input file of `account import` (audible-rs or legacy
/// Python format); encrypted files take their password from
/// `AUDIBLE_AUTH_PASSWORD` or an interactive prompt.
pub(crate) async fn load_import_input(path: &Path) -> Result<Authenticator> {
    let env_password = std::env::var("AUDIBLE_AUTH_PASSWORD")
        .ok()
        .map(SecretString::from);
    let had_env_password = env_password.is_some();

    match Authenticator::import_file(path, env_password).await {
        Ok(auth) => Ok(auth),
        Err(error) if password_required(&error) && !had_env_password => {
            let term = console::Term::stderr();
            term.write_str("Auth file password: ")?;
            let password = SecretString::from(term.read_secure_line()?);
            Authenticator::import_file(path, Some(password))
                .await
                .context("could not open the auth file with this password")
        }
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("could not load {}", path.display())))
        }
    }
}

fn password_required(error: &AuthError) -> bool {
    matches!(
        error,
        AuthError::File(AuthFileError::PasswordRequired)
            | AuthError::Legacy(LegacyError::PasswordRequired)
    )
}

/// The shared `--yes`/`-y` confirmation-skip flag of destructive
/// commands — one definition so text and short flag never diverge.
pub(crate) fn yes_arg() -> clap::Arg {
    clap::Arg::new("yes")
        .long("yes")
        .short('y')
        .action(clap::ArgAction::SetTrue)
        .help("Skip the confirmation prompt")
}

/// Turns raw `--limit`/`--page` values into the SQL LIMIT and OFFSET
/// (`limit` 0 = everything on page 1, so the offset stays 0; callers
/// reject `page > 1` there via [`empty_page_error`]).
pub(crate) fn page_window(limit: u32, page: u32) -> (u32, u64) {
    let query_limit = if limit == 0 { u32::MAX } else { limit };
    (query_limit, u64::from(page - 1) * u64::from(limit))
}

/// Uniform end-of-pages error for paged commands (`library list`,
/// `podcasts episodes`): `--page` pointed past the last page. `limit`
/// is the raw user value (0 = everything on one page).
pub(crate) fn empty_page_error(page: u32, limit: u32, total: u64) -> anyhow::Error {
    let per_page = if limit == 0 {
        total.max(1)
    } else {
        limit as u64
    };
    let pages = total.div_ceil(per_page).max(1);
    anyhow::anyhow!("page {page} is empty — {total} row(s) make {pages} page(s) at --limit {limit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_tree_is_valid() {
        // clap's own consistency check over the whole assembled tree
        // (duplicate flags, bad headings, …) — cheap insurance for the
        // registry and every subcommand's clap definition.
        build_root(&registry()).debug_assert();
    }

    #[test]
    fn page_window_maps_limit_and_page_to_sql() {
        assert_eq!(page_window(50, 1), (50, 0));
        assert_eq!(page_window(50, 4), (50, 150));
        // limit 0: everything on page 1.
        assert_eq!(page_window(0, 1), (u32::MAX, 0));
        // No overflow on absurd pages.
        assert_eq!(
            page_window(u32::MAX, u32::MAX),
            (u32::MAX, u64::from(u32::MAX - 1) * u64::from(u32::MAX),)
        );
    }

    #[test]
    fn empty_page_error_is_uniform() {
        assert_eq!(
            empty_page_error(5, 50, 164).to_string(),
            "page 5 is empty — 164 row(s) make 4 page(s) at --limit 50"
        );
        assert_eq!(
            empty_page_error(2, 0, 164).to_string(),
            "page 2 is empty — 164 row(s) make 1 page(s) at --limit 0"
        );
        // Empty table: page 1 always exists, page 2 never.
        assert_eq!(
            empty_page_error(2, 50, 0).to_string(),
            "page 2 is empty — 0 row(s) make 1 page(s) at --limit 50"
        );
    }
}
