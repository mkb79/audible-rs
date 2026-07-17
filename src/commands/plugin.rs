//! `audible plugin` — manage and inspect plugins (AUD-68, AUD-163,
//! AUD-165): `list` shows every plugin in the plugin dir with tier,
//! version, scopes and broken-status; `info` prints one plugin's full
//! manifest and source; `add` verifies a plugin and installs it — from
//! a local file (copy or `--symlink`) or an `https://` URL (downloaded,
//! then confirmation-gated with size/sha256/scopes shown); `remove`
//! deletes the plugin-dir entry (never a symlink's target). List/info
//! run the `--audible-describe` protocol on demand, so what is shown is
//! exactly what would be trusted on invocation.

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
                    .about("Verify a plugin and install it into the plugin dir")
                    .arg(Arg::new("source").required(true).value_name("SOURCE").help(
                        "Plugin file or https:// URL — an audible-<name> executable or a \
                         cmd_<name>.py script",
                    ))
                    .arg(
                        Arg::new("symlink")
                            .long("symlink")
                            .action(clap::ArgAction::SetTrue)
                            .help(
                                "Symlink instead of copying (local files only), so edits to the \
                                 original apply immediately; moving or deleting the original \
                                 breaks the plugin",
                            ),
                    )
                    .arg(super::yes_arg()),
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
                    args.get_one::<String>("source").expect("required"),
                    args.get_flag("symlink"),
                    args.get_flag("yes"),
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

/// `plugin add [--symlink] [--yes] <SOURCE>` — verify a plugin (naming,
/// no collisions, valid manifest) and install it into the plugin dir.
/// SOURCE is a local file or an `https://` URL (scheme detection);
/// remote installs are confirmation-gated.
async fn add(ctx: &Ctx, source: &str, symlink: bool, yes: bool) -> Result<()> {
    let installed = if source.starts_with("https://") {
        if symlink {
            bail!("--symlink only applies to local files");
        }
        match add_remote(ctx, source, yes).await? {
            Some(installed) => installed,
            None => return Ok(()), // user declined
        }
    } else if source.starts_with("http://") {
        bail!("plain http is not supported — use an https:// URL or a local file");
    } else {
        plugins::install(
            &plugins::plugin_dir(ctx),
            Path::new(source),
            symlink,
            &builtin_names(),
        )
        .await?
    };
    println!(
        "installed: {} ({}, scopes: {}) -> {}",
        installed.name,
        installed.tier.label(),
        scopes_or_none(&installed.manifest.scopes),
        installed.target.display()
    );
    if symlink {
        eprintln!("note: symlinked — moving or deleting the original breaks the plugin");
    }
    Ok(())
}

fn scopes_or_none(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "none".to_owned()
    } else {
        scopes.join(",")
    }
}

/// Longest plugin file we are willing to download. A compiled Tier-A
/// binary stays well under this; scripts are tiny.
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

/// Downloads an https source into a private temp dir and installs it
/// after **two** confirmations: first for the describe probe — which
/// executes the downloaded file — and then, with the manifest and its
/// scopes on the table, for the install. `None` = declined. Declining
/// the first prompt means the downloaded code never ran (audit
/// 2026-07-17, B2: the probe used to run before any consent).
async fn add_remote(ctx: &Ctx, url: &str, yes: bool) -> Result<Option<plugins::Installed>> {
    let file_name = https_file_name(url)?;
    // An unpredictable, owner-only temp dir (B4): a fixed
    // `/tmp/audible-plugin-add-<pid>` could be pre-owned by any local
    // user, and the file swapped between probe and install.
    let temp = tempfile::TempDir::new()?;
    let path = temp.path().join(&file_name);
    let (size, sha256) = download_https(url, &path).await?;
    // A downloaded file has no exec bit; a Tier-A plugin needs one (both
    // for the describe probe and for the installed copy).
    #[cfg(unix)]
    if file_name.starts_with("audible-") {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }

    let (name, tier) = plugins::classify_file_name(&file_name).ok_or_else(|| {
        anyhow::anyhow!(
            "{file_name} does not follow the plugin naming convention — the URL must end \
             in audible-<name> or cmd_<name>.py"
        )
    })?;

    // Consent BEFORE the probe: `describe` executes the fetched file
    // (sandboxed: no TTY, 5 s timeout, captured stdio — AUD-162), and a
    // decline must mean the code never ran.
    eprintln!("fetched {url}");
    eprintln!("  file:    {file_name} ({})", indicatif::BinaryBytes(size));
    eprintln!("  sha256:  {sha256}");
    if !super::prompt::confirm(
        yes,
        "Read its manifest? This EXECUTES the downloaded file (--audible-describe)",
    )? {
        eprintln!("aborted — the downloaded file was never executed, nothing installed");
        return Ok(None);
    }

    let candidate = plugins::Discovered {
        name,
        tier,
        source: path.clone(),
        broken: None,
    };
    let manifest = plugins::describe(&candidate)
        .await
        .map_err(|reason| anyhow::anyhow!("{url} is not a usable plugin: {reason}"))?;

    eprintln!("  plugin:  {} {}", manifest.name, manifest.version);
    if !manifest.description.is_empty() {
        eprintln!("  about:   {}", manifest.description);
    }
    eprintln!("  scopes:  {}", scopes_or_none(&manifest.scopes));
    if !super::prompt::confirm(yes, "Install this plugin?")? {
        eprintln!("aborted — nothing installed");
        return Ok(None);
    }
    let installed =
        plugins::install(&plugins::plugin_dir(ctx), &path, false, &builtin_names()).await?;
    Ok(Some(installed))
}

/// File name of an https source: the last path segment, query/fragment
/// stripped. (Percent-encoded names fail the naming convention later.)
fn https_file_name(url: &str) -> Result<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let rest = path.strip_prefix("https://").unwrap_or(path);
    let name = match rest.split_once('/') {
        Some((_host, path)) => path.rsplit('/').next().unwrap_or_default(),
        None => "",
    };
    if name.is_empty() {
        bail!("the URL must end in a plugin file name (audible-<name> or cmd_<name>.py)");
    }
    Ok(name.to_owned())
}

/// Streams `url` to `dest`, enforcing https-only redirects and the size
/// cap; returns (size, sha256-hex).
async fn download_https(url: &str, dest: &Path) -> Result<(u64, String)> {
    use futures::StreamExt as _;
    use sha2::{Digest as _, Sha256};
    use std::io::Write as _;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.error("redirect to a non-https URL")
            }
        }))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("could not fetch {url}: {error}"))?
        .error_for_status()
        .map_err(|error| anyhow::anyhow!("{url}: {error}"))?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_DOWNLOAD)
    {
        bail!(
            "{url} is larger than the {} MiB limit",
            MAX_DOWNLOAD / (1024 * 1024)
        );
    }

    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size += chunk.len() as u64;
        if size > MAX_DOWNLOAD {
            bail!(
                "{url} is larger than the {} MiB limit",
                MAX_DOWNLOAD / (1024 * 1024)
            );
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    let sha256: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, sha256))
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
        assert!(parse(&["plugin", "add", "https://host/cmd_x.py", "--yes"]).is_ok());
        assert!(parse(&["plugin", "add"]).is_err());
        assert!(parse(&["plugin", "remove", "x", "--yes"]).is_ok());
        assert!(parse(&["plugin", "remove"]).is_err());
        assert!(parse(&["plugin"]).is_err());
    }

    #[test]
    fn https_file_name_takes_the_last_path_segment() {
        let name = |url: &str| https_file_name(url).map_err(|e| e.to_string());
        assert_eq!(
            name("https://raw.githubusercontent.com/a/b/main/examples/cmd_gui.py").as_deref(),
            Ok("cmd_gui.py")
        );
        assert_eq!(
            name("https://host/dir/audible-foo?token=x#frag").as_deref(),
            Ok("audible-foo")
        );
        assert!(name("https://host").is_err());
        assert!(name("https://host/dir/").is_err());
    }

    #[test]
    fn builtin_names_cover_the_registry() {
        let names = builtin_names();
        for expected in ["api", "download", "library", "collections", "plugin"] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
    }
}
