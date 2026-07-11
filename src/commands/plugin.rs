//! `audible plugin` — manage and inspect plugins (AUD-68, AUD-163):
//! `list` shows every plugin in the plugin dir with tier, version,
//! scopes and broken-status; `info` prints one plugin's full manifest
//! and source; `add` verifies a plugin file and installs it (copy or
//! `--symlink`); `remove` deletes the plugin-dir entry (never a
//! symlink's target). List/info run the `--audible-describe` protocol
//! on demand, so what is shown is exactly what would be trusted on
//! invocation.

use std::path::Path;

use anyhow::{Result, bail};
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::output::Output;
use crate::plugins;

use super::hosts;

/// `audible plugin`.
pub struct PluginCommand;

#[async_trait::async_trait]
impl super::Command for PluginCommand {
    fn name(&self) -> &'static str {
        "plugin"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Manage and inspect installed plugins")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(clap::Command::new("list").about("List installed plugins"))
            .subcommand(
                clap::Command::new("info")
                    .about("Show a plugin's manifest and source")
                    .arg(
                        Arg::new("name")
                            .required(true)
                            .value_name("PLUGIN")
                            .help("Plugin name (see `plugin list`)"),
                    ),
            )
            .subcommand(
                clap::Command::new("add")
                    .about("Verify a plugin file and install it into the plugin dir")
                    .arg(Arg::new("file").required(true).value_name("FILE").help(
                        "Plugin file — an audible-<name> executable or a cmd_<name>.py script",
                    ))
                    .arg(
                        Arg::new("symlink")
                            .long("symlink")
                            .action(clap::ArgAction::SetTrue)
                            .help(
                                "Symlink instead of copying, so edits to the original apply \
                                 immediately; moving or deleting the original breaks the plugin",
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("remove")
                    .about("Remove a plugin from the plugin dir (a symlink's original is kept)")
                    .arg(
                        Arg::new("name")
                            .required(true)
                            .value_name("PLUGIN")
                            .help("Plugin name (see `plugin list`)"),
                    )
                    .arg(super::yes_arg()),
            )
            .subcommands(hosts::subcommands("`hosts`-scoped plugins"))
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some(("list", _)) => list(ctx).await,
            Some(("info", info)) => {
                show(ctx, info.get_one::<String>("name").expect("required")).await
            }
            Some(("add", args)) => {
                add(
                    ctx,
                    Path::new(args.get_one::<String>("file").expect("required")),
                    args.get_flag("symlink"),
                )
                .await
            }
            Some(("remove", args)) => {
                remove(
                    ctx,
                    args.get_one::<String>("name").expect("required"),
                    args.get_flag("yes"),
                )
                .await
            }
            Some(("allow-host", args)) => allow_host(
                ctx,
                args.get_one::<String>("host").expect("required"),
                args.get_one::<String>("auth").expect("default"),
            ),
            Some(("list-hosts", _)) => list_hosts(ctx),
            Some(("remove-host", args)) => {
                remove_host(ctx, args.get_one::<String>("host").expect("required"))
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

/// `plugin allow-host <host> [--auth]` — approve an external host for
/// **plugins** (the `[plugins]` allowlist; the agent has its own under
/// `agent allow-host`, AUD-124).
fn allow_host(ctx: &Ctx, host: &str, auth: &str) -> Result<()> {
    hosts::allow(ctx, "plugins", host, auth)
}

fn list_hosts(ctx: &Ctx) -> Result<()> {
    hosts::list(ctx, &ctx.config().plugins.allowed_hosts, "plugin")
}

fn remove_host(ctx: &Ctx, host: &str) -> Result<()> {
    hosts::remove(ctx, "plugins", host)
}

/// Names of all built-in commands (collision guard for discovery).
/// `pub` because main.rs is a separate crate target.
pub fn builtin_names() -> Vec<String> {
    super::registry()
        .iter()
        .map(|command| command.name().to_owned())
        .collect()
}

/// `plugin list` — every discovered plugin, described on the spot.
async fn list(ctx: &Ctx) -> Result<()> {
    let discovered = plugins::discover(ctx, &builtin_names());
    if discovered.is_empty() {
        eprintln!(
            "no plugins found (plugin dir: {})",
            plugins::plugin_dir(ctx).display()
        );
        return Ok(());
    }
    let mut rows = Vec::new();
    for plugin in &discovered {
        let (version, scopes, status) = match plugins::describe(plugin).await {
            Ok(manifest) => (manifest.version, manifest.scopes.join(","), "ok".to_owned()),
            Err(reason) => (String::new(), String::new(), format!("broken: {reason}")),
        };
        rows.push(vec![
            plugin.name.clone(),
            plugin.tier.label().to_owned(),
            version,
            scopes,
            status,
            plugin.source.display().to_string(),
        ]);
    }
    ctx.print(&Output::table(
        vec!["name", "tier", "version", "scopes", "status", "source"],
        rows,
    ));
    Ok(())
}

/// `plugin add [--symlink] <FILE>` — verify the file (naming, no
/// collisions, valid manifest) and install it into the plugin dir.
async fn add(ctx: &Ctx, source: &Path, symlink: bool) -> Result<()> {
    let installed =
        plugins::install(&plugins::plugin_dir(ctx), source, symlink, &builtin_names()).await?;
    let scopes = if installed.manifest.scopes.is_empty() {
        "none".to_owned()
    } else {
        installed.manifest.scopes.join(",")
    };
    println!(
        "installed: {} ({}, scopes: {scopes}) -> {}",
        installed.name,
        installed.tier.label(),
        installed.target.display()
    );
    if symlink {
        eprintln!("note: symlinked — moving or deleting the original breaks the plugin");
    }
    Ok(())
}

/// `plugin remove <NAME>` — delete the plugin-dir entry: the symlink
/// itself or the copied file, never a symlink's target.
async fn remove(ctx: &Ctx, name: &str, yes: bool) -> Result<()> {
    let discovered = plugins::discover(ctx, &builtin_names());
    let Some(plugin) = discovered.iter().find(|plugin| plugin.name == name) else {
        bail!(
            "no plugin named {name:?} (plugin dir: {})",
            plugins::plugin_dir(ctx).display()
        );
    };
    let is_symlink = plugin
        .source
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    let what = if is_symlink {
        "the symlink (the original file stays)"
    } else {
        "the file"
    };
    let question = format!(
        "Remove plugin {name} — deletes {what} {}?",
        plugin.source.display()
    );
    if !super::prompt::confirm(yes, &question)? {
        eprintln!("aborted");
        return Ok(());
    }
    std::fs::remove_file(&plugin.source).map_err(|error| {
        anyhow::anyhow!("could not remove {}: {error}", plugin.source.display())
    })?;
    println!("removed: {}", plugin.source.display());
    Ok(())
}

/// `plugin info <name>` — full manifest plus discovery facts.
async fn show(ctx: &Ctx, name: &str) -> Result<()> {
    let discovered = plugins::discover(ctx, &builtin_names());
    let Some(plugin) = discovered.iter().find(|plugin| plugin.name == name) else {
        bail!(
            "no plugin named {name:?} (plugin dir: {})",
            plugins::plugin_dir(ctx).display()
        );
    };
    let manifest = plugins::describe(plugin).await;
    let value = |field: Option<&str>| field.unwrap_or("-").to_owned();
    let mut rows = vec![
        vec!["name".to_owned(), plugin.name.clone()],
        vec!["tier".to_owned(), plugin.tier.label().to_owned()],
        vec!["source".to_owned(), plugin.source.display().to_string()],
    ];
    match &manifest {
        Ok(manifest) => {
            rows.push(vec!["version".to_owned(), value(Some(&manifest.version))]);
            rows.push(vec![
                "description".to_owned(),
                value(Some(&manifest.description)),
            ]);
            rows.push(vec!["scopes".to_owned(), manifest.scopes.join(",")]);
            rows.push(vec!["help".to_owned(), value(manifest.help.as_deref())]);
            rows.push(vec!["status".to_owned(), "ok".to_owned()]);
        }
        Err(reason) => {
            rows.push(vec!["status".to_owned(), format!("broken: {reason}")]);
        }
    }
    ctx.print(&Output::table(vec!["field", "value"], rows));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_shape() {
        use crate::commands::Command as _;
        let parse = |args: &[&str]| PluginCommand.clap().try_get_matches_from(args);
        assert!(parse(&["plugin", "list"]).is_ok());
        assert!(parse(&["plugin", "info", "stats"]).is_ok());
        assert!(parse(&["plugin", "info"]).is_err());
        assert!(parse(&["plugin", "add", "cmd_x.py"]).is_ok());
        assert!(parse(&["plugin", "add", "cmd_x.py", "--symlink"]).is_ok());
        assert!(parse(&["plugin", "add"]).is_err());
        assert!(parse(&["plugin", "remove", "x", "--yes"]).is_ok());
        assert!(parse(&["plugin", "remove"]).is_err());
        assert!(parse(&["plugin"]).is_err());
    }

    #[test]
    fn builtin_names_cover_the_registry() {
        let names = builtin_names();
        for expected in ["api", "download", "library", "collections", "plugin"] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
    }
}
