//! `audible completions` — generate a shell completion script (AUD-143).
//!
//! By default the script for the target shell is printed to stdout, ready
//! to be redirected into the shell's completion directory (pipeable, and
//! used by `install.sh --completions`). With `--install` the command writes
//! it to that directory itself. The script is generated from the full
//! command tree via [`crate::commands::build_root`], so it covers every
//! subcommand, option and the global flags.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Arg, ArgAction, ArgMatches};
use clap_complete::Shell;

use crate::config::ctx::Ctx;

/// `audible completions`.
pub struct CompletionsCommand;

#[async_trait::async_trait]
impl super::Command for CompletionsCommand {
    fn name(&self) -> &'static str {
        "completions"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Generate a shell completion script")
            .long_about(
                "Print a shell completion script to stdout, or place it directly with \
                 --install.\n\n\
                 Supported shells: bash, zsh, fish, powershell, elvish \
                 (--install: bash, zsh, fish).\n\n\
                 --install target directories:\n  \
                 bash  $XDG_DATA_HOME/bash-completion/completions/audible\n  \
                 zsh   $XDG_DATA_HOME/zsh/site-functions/_audible   (dir must be on $fpath)\n  \
                 fish  $XDG_CONFIG_HOME/fish/completions/audible.fish\n\n\
                 Without --install, redirect the output there yourself, then open a new shell.",
            )
            .arg(
                Arg::new("shell")
                    .value_name("SHELL")
                    .value_parser(clap::value_parser!(Shell))
                    .help("Target shell (default: detected from $SHELL)"),
            )
            .arg(
                Arg::new("install")
                    .long("install")
                    .action(ArgAction::SetTrue)
                    .help("Write the script to the shell's completion dir instead of stdout"),
            )
    }

    async fn run(&self, _ctx: &Ctx, matches: &ArgMatches) -> Result<()> {
        let shell = match matches.get_one::<Shell>("shell").copied() {
            Some(shell) => shell,
            None => Shell::from_env().ok_or_else(|| {
                anyhow!(
                    "could not detect the shell from $SHELL — pass one explicitly \
                     (bash, zsh, fish, powershell, elvish)"
                )
            })?,
        };

        if matches.get_flag("install") {
            install_script(shell)
        } else {
            let mut out = io::stdout();
            clap_complete::generate(shell, &mut build(), "audible", &mut out);
            // When dumped to a terminal (exploring), point at the easy path.
            // Redirects/pipes (`> file`, `source <(…)`) keep clean output.
            if io::stdout().is_terminal() {
                eprintln!(
                    "\ntip: `audible completions {shell} --install` places this in the \
                     right directory for you"
                );
            }
            Ok(())
        }
    }
}

/// The full top-level tree, ready for the generator.
fn build() -> clap::Command {
    super::build_root(&super::registry())
}

/// Writes the completion script to the shell's standard directory.
fn install_script(shell: Shell) -> Result<()> {
    let data = xdg_dir("XDG_DATA_HOME", ".local/share")?;
    let config = xdg_dir("XDG_CONFIG_HOME", ".config")?;
    let (path, fpath_hint) = completion_target(shell, &data, &config)?;

    let dir = path.parent().expect("target has a parent directory");
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut build(), "audible", &mut buf);
    std::fs::write(&path, &buf).with_context(|| format!("could not write {}", path.display()))?;

    eprintln!("installed {}", path.display());
    if let Some(hint) = fpath_hint {
        eprintln!("note: {hint}");
    }
    eprintln!("open a new shell to load the completions");
    Ok(())
}

/// Standard completion file for `shell` under the given XDG base dirs, plus
/// an optional extra-setup hint. `--install` supports the three common
/// shells; the others have no simple, portable auto-load location.
fn completion_target(
    shell: Shell,
    data: &Path,
    config: &Path,
) -> Result<(PathBuf, Option<&'static str>)> {
    Ok(match shell {
        Shell::Bash => (
            data.join("bash-completion/completions").join("audible"),
            None,
        ),
        Shell::Zsh => (
            data.join("zsh/site-functions").join("_audible"),
            Some("its directory must be on your $fpath (e.g. add it in ~/.zshrc before compinit)"),
        ),
        Shell::Fish => (config.join("fish/completions").join("audible.fish"), None),
        other => bail!(
            "--install supports bash, zsh and fish; for {other} redirect the script \
             yourself: audible completions {other} > <file>"
        ),
    })
}

/// `$VAR` when set and non-empty, else `$HOME/<default_suffix>`.
fn xdg_dir(var: &str, default_suffix: &str) -> Result<PathBuf> {
    match std::env::var_os(var) {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
            Ok(PathBuf::from(home).join(default_suffix))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_script_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let mut buf: Vec<u8> = Vec::new();
            clap_complete::generate(shell, &mut build(), "audible", &mut buf);
            let script = String::from_utf8(buf).expect("completion script is UTF-8");
            // Non-empty, names the binary, and reaches a real subcommand.
            assert!(!script.is_empty(), "{shell} produced an empty script");
            assert!(script.contains("audible"), "{shell} omits the binary name");
            assert!(script.contains("download"), "{shell} omits a subcommand");
        }
    }

    #[test]
    fn install_targets_follow_xdg_conventions() {
        let data = Path::new("/d");
        let config = Path::new("/c");
        assert_eq!(
            completion_target(Shell::Bash, data, config).unwrap().0,
            Path::new("/d/bash-completion/completions/audible")
        );
        assert_eq!(
            completion_target(Shell::Zsh, data, config).unwrap().0,
            Path::new("/d/zsh/site-functions/_audible")
        );
        assert_eq!(
            completion_target(Shell::Fish, data, config).unwrap().0,
            Path::new("/c/fish/completions/audible.fish")
        );
        // No simple auto-load dir → --install refuses, print-and-redirect only.
        assert!(completion_target(Shell::PowerShell, data, config).is_err());
    }
}
