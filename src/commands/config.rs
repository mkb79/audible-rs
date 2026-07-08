//! `audible config` — validated access to `config.toml`. Every write
//! goes through the schema gate in `config::write`; invalid changes
//! never reach disk.

use anyhow::{Context as _, Result, bail};
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::config::{Config, write};

/// `audible config`.
pub struct ConfigCommand;

#[async_trait::async_trait]
impl super::Command for ConfigCommand {
    fn name(&self) -> &'static str {
        "config"
    }

    fn clap(&self) -> clap::Command {
        let key = || {
            Arg::new("key")
                .required(true)
                .value_name("KEY")
                .help("Dotted config key (e.g. default_account, settings.default.download_dir)")
        };
        clap::Command::new(self.name())
            .about("Get and set configuration values")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("get")
                    .about("Print a config value (dotted key, e.g. default_account)")
                    .arg(key()),
            )
            .subcommand(
                clap::Command::new("set")
                    .about("Set a config value (validated before writing)")
                    .arg(key())
                    .arg(
                        Arg::new("value")
                            .required(true)
                            .value_name("VALUE")
                            .help("New value (validated against the schema before writing)"),
                    ),
            )
            .subcommand(
                clap::Command::new("unset")
                    .about("Remove a config value (or a whole table)")
                    .arg(key()),
            )
            .subcommand(
                clap::Command::new("list")
                    .about("List all set values; the config path goes to stderr"),
            )
            .subcommand(
                clap::Command::new("edit")
                    .about("Edit the config in $EDITOR; the result is validated"),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let get_key = |m: &clap::ArgMatches| m.get_one::<String>("key").expect("required").clone();
        match matches.subcommand() {
            Some(("get", sub)) => get(ctx, &get_key(sub)),
            Some(("set", sub)) => {
                let key = get_key(sub);
                let value = sub.get_one::<String>("value").expect("required").clone();
                set(ctx, &key, &value)?;
                if crate::commands::download::key_affects_filenames(&key) {
                    crate::commands::download::hint_reorganize(ctx).await;
                }
                Ok(())
            }
            Some(("unset", sub)) => {
                let key = get_key(sub);
                unset(ctx, &key)?;
                // Removing a naming key changes the effective scheme too
                // (fallback to `settings.default` / the built-in default).
                if crate::commands::download::key_affects_filenames(&key) {
                    crate::commands::download::hint_reorganize(ctx).await;
                }
                Ok(())
            }
            Some(("list", _)) => list(ctx),
            Some(("edit", _)) => edit(ctx),
            _ => unreachable!("subcommand required"),
        }
    }
}

fn read_content(ctx: &Ctx) -> Result<String> {
    match std::fs::read_to_string(ctx.config_file()) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(format!("version = {}\n", crate::config::CONFIG_VERSION))
        }
        Err(error) => Err(error.into()),
    }
}

fn get(ctx: &Ctx, key: &str) -> Result<()> {
    match write::get(&read_content(ctx)?, key)? {
        Some(value) => {
            println!("{value}");
            Ok(())
        }
        None => bail!("{key} is not set"),
    }
}

fn set(ctx: &Ctx, key: &str, value: &str) -> Result<()> {
    write::edit_file(&ctx.config_file(), |content| {
        write::set(content, key, value)
    })?;
    eprintln!("set {key} = {value}");
    Ok(())
}

fn unset(ctx: &Ctx, key: &str) -> Result<()> {
    write::edit_file(&ctx.config_file(), |content| write::unset(content, key))?;
    eprintln!("unset {key}");
    Ok(())
}

fn list(ctx: &Ctx) -> Result<()> {
    eprintln!("config file: {}", ctx.config_file().display());
    for (key, value) in write::flatten(&read_content(ctx)?)? {
        println!("{key} = {value}");
    }
    Ok(())
}

/// Opens the config in `$VISUAL`/`$EDITOR`. The edit happens on a temp
/// copy; only a result that parses and validates replaces the file.
fn edit(ctx: &Ctx) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());

    let dir = tempfile_dir()?;
    let tmp = dir.join("audible-config-edit.toml");
    std::fs::write(&tmp, read_content(ctx)?)?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .with_context(|| format!("could not launch editor {editor:?}"))?;
    if !status.success() {
        bail!("editor {editor:?} exited with {status}; config unchanged");
    }

    let edited = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    let config: Config = toml::from_str(&edited)
        .map_err(|error| anyhow::anyhow!("rejected invalid config (file unchanged): {error}"))?;
    config.validate()?;

    write::edit_file(&ctx.config_file(), move |_| Ok(edited))?;
    eprintln!("config updated");
    Ok(())
}

fn tempfile_dir() -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
