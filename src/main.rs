//! Entry point of the `audible` CLI binary: builds the clap tree from
//! the command registry (D10), resolves global flags and dispatches.

use tracing_subscriber::EnvFilter;

use audible_rs::commands;

// Global allocator (AUD-140): mimalloc replaces the platform malloc —
// it neutralizes musl's slower default allocator for the static Linux
// release binaries (and is a modest win on glibc/macOS too).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use audible_rs::config::ctx::Ctx;
use audible_rs::output::OutputFormat;

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
    let matches = match commands::build_root(&registry).try_get_matches() {
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
    let ctx = match Ctx::new(selectors) {
        Ok(ctx) => std::sync::Arc::new(ctx.with_output(output)),
        Err(error) => {
            // The `-o json` envelope contract (AUD-279) also covers a run
            // that dies before the context exists (config load/validation).
            if output == OutputFormat::Json {
                audible_rs::output::print_envelope(
                    Some(&format!("{error:#}")),
                    &[],
                    serde_json::Value::Null,
                );
            }
            return Err(error);
        }
    };

    let (name, sub_matches) = matches.subcommand().expect("subcommand required");
    for command in &registry {
        if command.name() == name {
            let result = command.run(&ctx, sub_matches).await;
            // The dispatch boundary completes the envelope contract
            // (AUD-279): a failure emits `error` + `result: null`, a
            // success without a payload the empty envelope — so every
            // command answers `-o json` with exactly one envelope. Both
            // are no-ops if the run printed (or declared raw) stdout;
            // stderr + exit code behave as always.
            match &result {
                Ok(()) => ctx.emit_success_envelope(),
                Err(error) => ctx.emit_error_envelope(&format!("{error:#}")),
            }
            return result;
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
    match audible_rs::plugins::run_external(&ctx, name, &builtins, &args).await {
        // A plugin owns its stdout entirely (documented passthrough) —
        // its exit code propagates, no envelope.
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {
            if let Some(hint) = commands::old_command_hint(name) {
                eprintln!("hint: {hint}\n");
            }
            let error = anyhow::anyhow!(
                "unrecognized subcommand {name:?} — not a built-in and no plugin of \
                 that name (plugin dir: {}; see `audible plugin list`)",
                audible_rs::plugins::plugin_dir(&ctx).display()
            );
            ctx.emit_error_envelope(&format!("{error:#}"));
            Err(error)
        }
        Err(error) => {
            ctx.emit_error_envelope(&format!("{error:#}"));
            Err(error)
        }
    }
}
