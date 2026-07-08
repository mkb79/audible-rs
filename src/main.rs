//! Entry point of the `audible` CLI binary: builds the clap tree from
//! the command registry (D10), resolves global flags and dispatches.

use clap::{Arg, ArgAction};
use tracing_subscriber::EnvFilter;

use audible_rs::commands::{self, Command};

// Global allocator (AUD-140): mimalloc replaces the platform malloc —
// it neutralizes musl's slower default allocator for the static Linux
// release binaries (and is a modest win on glibc/macOS too).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use audible_rs::config::ctx::Ctx;
use audible_rs::output::OutputFormat;

fn parse_output_format(s: &str) -> Result<OutputFormat, String> {
    s.parse()
}

/// Help section of the global flags; separates them from
/// command-specific options in every `--help` output.
const GLOBAL_OPTIONS: &str = "Global Options";

fn build_cli(registry: &[Box<dyn Command>]) -> clap::Command {
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

/// Log filter precedence per the config resolution order (§2):
/// CLI flag → RUST_LOG → default (`warn`).
fn log_filter(matches: &clap::ArgMatches) -> EnvFilter {
    if matches.get_flag("quiet") {
        return EnvFilter::new("error");
    }
    match matches.get_count("verbose") {
        0 => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        // `audible_rs` is the library, `audible` the binary target.
        1 => EnvFilter::new("warn,audible_rs=info,audible=info"),
        2 => EnvFilter::new("info,audible_rs=debug,audible=debug"),
        _ => EnvFilter::new("trace"),
    }
}

/// Prints a migration hint when an old Python command name is used (D2).
fn print_old_command_hint(error: &clap::Error) {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if error.kind() == ErrorKind::InvalidSubcommand
        && let Some(ContextValue::String(name)) = error.get(ContextKind::InvalidSubcommand)
        && let Some(hint) = commands::old_command_hint(name)
    {
        eprintln!("hint: {hint}\n");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = commands::registry();
    let matches = match build_cli(&registry).try_get_matches() {
        Ok(matches) => matches,
        Err(error) => {
            print_old_command_hint(&error);
            error.exit();
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&matches))
        .with_writer(std::io::stderr)
        .init();

    let selectors = audible_rs::config::ctx::Selectors {
        account: matches.get_one::<String>("account").cloned(),
        settings: matches.get_one::<String>("settings").cloned(),
        marketplace: matches.get_one::<String>("marketplace").cloned(),
    };
    let output = matches
        .get_one::<OutputFormat>("output")
        .copied()
        .unwrap_or_default();
    // Arc: the plugin broker (AUD-69) serves RPC from a background task
    // while the plugin child runs; `&Ctx` derefs out of the Arc for the
    // built-in commands.
    let ctx = std::sync::Arc::new(Ctx::new(selectors)?.with_output(output));

    let (name, sub_matches) = matches.subcommand().expect("subcommand required");
    for command in &registry {
        if command.name() == name {
            return command.run(&ctx, sub_matches).await;
        }
    }

    // Not a built-in: external subcommand → plugin (AUD-68). With
    // `allow_external_subcommands`, clap no longer errors on unknown
    // names, so the old-command hint moves here too.
    let args: Vec<std::ffi::OsString> = sub_matches
        .get_many::<std::ffi::OsString>("")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let builtins = audible_rs::commands::plugin::builtin_names();
    match audible_rs::plugins::run_external(&ctx, name, &builtins, &args).await? {
        Some(code) => std::process::exit(code),
        None => {
            if let Some(hint) = commands::old_command_hint(name) {
                eprintln!("hint: {hint}\n");
            }
            anyhow::bail!(
                "unrecognized subcommand {name:?} — not a built-in and no plugin of \
                 that name (plugin dir: {}; see `audible plugin list`)",
                audible_rs::plugins::plugin_dir(&ctx).display()
            );
        }
    }
}
