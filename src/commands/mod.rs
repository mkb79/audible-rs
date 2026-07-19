//! CLI commands (archived architecture §5): one module per noun group and a
//! dynamic registry of [`Command`] trait objects (D10). Typed arguments
//! live in `#[derive(Args)]` structs; `run` re-parses them via
//! `from_arg_matches`. Old Python command names map to hints in
//! [`old_command_hint`].

pub mod account;
#[cfg(unix)]
pub mod agent;
pub mod annotations;
pub mod api;
pub mod catalog;
pub mod collections;
pub mod completions;
pub mod config;
pub mod db;
pub mod download;
pub(crate) mod hosts;
pub mod items;

/// Splits a comma-separated value into trimmed, non-empty items — the
/// one CSV rule for option values (audit 2026-07-17, D6; two byte-equal
/// copies lived in `setup` and `download`).
pub(crate) fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Renders an optional config enum as its serde string (audit 2026-07-18,
/// D10 — `settings` and `setup` each had an `enum_str` with a drifted
/// signature). `None` (unset) or a non-string serialization yields `None`;
/// serializing a plain field enum never actually fails.
pub(crate) fn enum_str<T: serde::Serialize>(value: &Option<T>) -> Option<String> {
    value.as_ref().and_then(|value| {
        serde_json::to_value(value)
            .ok()
            .and_then(|json| json.as_str().map(str::to_owned))
    })
}

/// The shared `--missing[=KINDS]` selector (audit 2026-07-18, D10 — the
/// clap block was copied for `library list` and `library episodes`, and
/// the artifact-kind vocabulary lived in three shapes). No value means
/// `audio`, `all` covers every kind; the kinds come from the one
/// [`crate::db::DOWNLOAD_KINDS`] home. `noun` words the help; the caller
/// adds any `conflicts_with`/`help_heading`.
pub(crate) fn missing_kinds_arg(noun: &str) -> clap::Arg {
    let values: Vec<&str> = crate::db::DOWNLOAD_KINDS
        .iter()
        .copied()
        .chain(std::iter::once("all"))
        .collect();
    clap::Arg::new("missing")
        .long("missing")
        .value_name("KINDS")
        .num_args(0..)
        .require_equals(true)
        .value_delimiter(',')
        .default_missing_value("audio")
        .value_parser(clap::builder::PossibleValuesParser::new(values))
        .help(format!(
            "Only {noun} lacking a download record of these kinds \
             (no value: audio; `all` covers every kind)"
        ))
}

/// The shared `--limit <N>` row cap (default 50; 0 = all). One builder for
/// the several list commands (D10).
pub(crate) fn limit_arg() -> clap::Arg {
    clap::Arg::new("limit")
        .long("limit")
        .value_name("N")
        .default_value("50")
        .value_parser(clap::value_parser!(u32))
        .help("Maximum number of rows (0 = all)")
}

/// The shared `--page <N>` pagination arg (1-based). Pairs with
/// [`limit_arg`] (D10).
pub(crate) fn page_arg() -> clap::Arg {
    clap::Arg::new("page")
        .long("page")
        .value_name("N")
        .default_value("1")
        .value_parser(clap::value_parser!(u32).range(1..))
        .help("Show the N-th page of --limit rows")
}

/// Collects a repeatable/multi-value string option into an owned `Vec`,
/// empty when the option is absent — the one `get_many→cloned` shape
/// (audit 2026-07-17, D6; it lived inline and as local closures across a
/// dozen commands).
pub(crate) fn strings(matches: &clap::ArgMatches, key: &str) -> Vec<String> {
    matches
        .get_many::<String>(key)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}
pub mod library;
pub mod plugin;
pub(crate) mod prompt;
pub mod selfcmd;
pub mod series;
pub mod settings;
pub mod setup;
pub mod stats;

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
        // Not CARGO_PKG_VERSION: a build from source between two releases
        // carries its commit (`+g<sha>`), so `--version` in a bug report
        // names what the reporter actually runs (AUD-180, see `build.rs`).
        .version(env!("AUDIBLE_BUILD_VERSION"))
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
        #[cfg(unix)]
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
        Box::new(selfcmd::SelfCommand),
        Box::new(series::SeriesCommand),
        Box::new(settings::SettingsCommand),
        Box::new(setup::SetupCommand),
        Box::new(stats::StatsCommand),
    ]
}

/// Migration hint for a retired command name — old Python names (D2) and
/// removed audible-rs nouns alike.
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
        // Removed at v0.1.0 (AUD-175/AUD-176): the library is the one
        // container. Deprecation notice was live from PR #24 to alpha.8.
        "podcasts" => Some(
            "`podcasts` is gone: use `audible library list --kind podcast` for the shows \
             and `audible library episodes <SHOW>` for their episodes",
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

/// The shared `--kind` content filter (AUD-173): restrict a command to
/// books, podcast shows and/or single episodes. One definition so the
/// filter reads the same on every command that takes it. The display
/// commands default to `book` (podcasts/episodes are shown on request);
/// `add`/`remove` default to `all` — there the filter is an opt-in
/// guard, and a `book` default would refuse podcast follows outright.
pub(crate) fn kind_arg(default: &'static str) -> clap::Arg {
    debug_assert!(matches!(default, "book" | "all"));
    clap::Arg::new("kind")
        .long("kind")
        .value_name("KIND,...")
        .action(clap::ArgAction::Append)
        .value_delimiter(',')
        .default_value(default)
        .value_parser(["book", "podcast", "episode", "all"])
        .help(format!(
            "Only these content kinds: book, podcast, episode (CSV; default {default})"
        ))
}

/// The selected `--kind` values as a filter list — empty means no filter
/// (`all`, the default).
pub(crate) fn kind_filter(matches: &clap::ArgMatches) -> Vec<String> {
    let mut kinds: Vec<String> = strings(matches, "kind");
    if kinds.iter().any(|kind| kind == "all") {
        return Vec::new();
    }
    kinds.sort();
    kinds.dedup();
    kinds
}

/// Turns raw `--limit`/`--page` values into the SQL LIMIT and OFFSET
/// (`limit` 0 = everything on page 1, so the offset stays 0; callers
/// reject `page > 1` there via [`empty_page_error`]).
pub(crate) fn page_window(limit: u32, page: u32) -> (u32, u64) {
    let query_limit = if limit == 0 { u32::MAX } else { limit };
    (query_limit, u64::from(page - 1) * u64::from(limit))
}

/// Uniform end-of-pages error for paged commands (`library list`,
/// `library episodes`): `--page` pointed past the last page. `limit`
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
