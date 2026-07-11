//! Plugin system (archived architecture §9, AUD-68; discovery revised in
//! AUD-163): discovery of `audible-<name>` executables (Tier A) and
//! `cmd_<name>.py` scripts (Tier B, run with `python3`) — both in the
//! dedicated plugin dir only, installed via `plugin add` — the
//! `--audible-describe` manifest protocol, and external invocation with
//! verbatim argv pass-through. **No command override**: a plugin whose
//! name collides with a built-in is never loaded — and structurally
//! cannot fire anyway, because built-ins are registered clap subcommands
//! while externals only reach the plugin path for unknown names. Broker
//! RPC/scopes enforcement is AUD-69; until then a plugin simply gets no
//! RPC access.

pub mod broker;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::ctx::Ctx;
use crate::config::paths;

/// Scopes a manifest may request: spec §9's `api|download|config`, plus
/// `invoke` (AUD-114 — run built-in commands through the broker) and
/// `hosts` (AUD-120 — reach user-approved external hosts via
/// `api.request`). Validation is strict so a manifest is auditable
/// before anything runs.
pub const VALID_SCOPES: [&str; 5] = ["api", "download", "config", "invoke", "hosts"];

/// How long a plugin may take to answer `--audible-describe`.
const DESCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How a discovered plugin is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// `audible-<name>` executable in the plugin dir.
    Executable,
    /// `cmd_<name>.py` in the plugin dir, run via `python3`.
    Python,
}

impl Tier {
    /// Short label for tables (`exec` / `python`).
    pub fn label(self) -> &'static str {
        match self {
            Tier::Executable => "exec",
            Tier::Python => "python",
        }
    }
}

/// A discovered plugin — possibly unusable (`broken`), but still listed
/// so `plugin list` can tell the user why.
#[derive(Debug)]
pub struct Discovered {
    /// Command name (`audible <name>`).
    pub name: String,
    pub tier: Tier,
    /// The file discovery found.
    pub source: PathBuf,
    /// Why the plugin cannot run (collision with a built-in, missing
    /// exec bit, …) — checked before describe.
    pub broken: Option<String>,
}

/// The manifest a plugin prints for `--audible-describe`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// Resolved plugin directory: `[plugins].dir`, else the platform data
/// dir's `plugins` subfolder.
pub fn plugin_dir(ctx: &Ctx) -> PathBuf {
    ctx.config()
        .plugins
        .dir
        .clone()
        .map(|dir| crate::naming::expand_tilde(&dir))
        .unwrap_or_else(|| paths::data_dir().join("plugins"))
}

/// Discovers every plugin: Tier A (`audible-<name>`) and Tier B
/// (`cmd_<name>.py`) in the **plugin dir only** (AUD-163, Docker
/// cli-plugins model — `PATH` is deliberately not scanned: the
/// `audible-*` namespace is occupied by non-plugins such as
/// `audible-quickstart`). First occurrence of a name wins. Names that
/// collide with built-in commands are kept as broken entries (auditable
/// in `plugin list`) and additionally warned about.
pub fn discover(ctx: &Ctx, builtins: &[String]) -> Vec<Discovered> {
    discover_in(&plugin_dir(ctx), builtins, python3())
}

/// [`discover`] with every environment input explicit (testable).
fn discover_in(plugin_dir: &Path, builtins: &[String], python: Option<PathBuf>) -> Vec<Discovered> {
    let mut found: Vec<Discovered> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |mut plugin: Discovered| {
        if !seen.insert(plugin.name.clone()) {
            return; // earlier source wins
        }
        if builtins.iter().any(|builtin| builtin == &plugin.name) {
            tracing::warn!(
                name = plugin.name,
                source = %plugin.source.display(),
                "plugin name collides with a built-in command; not loaded"
            );
            plugin.broken = Some("name collides with a built-in command".to_owned());
        }
        found.push(plugin);
    };

    for entry in read_dir_sorted(plugin_dir) {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // A symlink whose target vanished must stay visible instead of
        // silently dropping out of `plugin list` — with `--symlink`
        // installs, moving the original is the expected failure mode.
        let dangling = path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
            && !path.exists();
        if !dangling && !path.is_file() {
            continue;
        }
        if let Some(name) = file_name.strip_prefix("audible-") {
            if dangling {
                push(broken_symlink(name, path, Tier::Executable));
            } else {
                push(tier_a(name, path));
            }
        } else if let Some(name) = file_name
            .strip_prefix("cmd_")
            .and_then(|rest| rest.strip_suffix(".py"))
        {
            if dangling {
                push(broken_symlink(name, path, Tier::Python));
            } else {
                match &python {
                    Some(_) => push(Discovered {
                        name: name.to_owned(),
                        tier: Tier::Python,
                        source: path,
                        broken: None,
                    }),
                    // Per the spec: no python3 → Tier B is silently skipped.
                    None => tracing::debug!(source = %path.display(), "no python3; skipping"),
                }
            }
        }
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// A plugin-dir symlink whose target is gone (AUD-163).
fn broken_symlink(name: &str, path: PathBuf, tier: Tier) -> Discovered {
    Discovered {
        name: name.to_owned(),
        tier,
        source: path,
        broken: Some("symlink target missing (moved or deleted?)".to_owned()),
    }
}

/// A successful [`install`]: what landed where, plus the verified
/// manifest.
#[derive(Debug)]
pub struct Installed {
    /// Command name (derived from the file name — that is what wins).
    pub name: String,
    pub tier: Tier,
    pub manifest: Manifest,
    /// The new entry inside the plugin dir.
    pub target: PathBuf,
}

/// Installs `source` into `plugin_dir` (AUD-163) — fail-closed: the file
/// name must follow the naming convention, the name must collide with
/// neither a built-in nor an installed plugin, and the source must
/// answer `--audible-describe` with a valid manifest **before** anything
/// lands. `symlink` links instead of copying (dev workflow — moving or
/// deleting the original breaks the plugin).
pub async fn install(
    plugin_dir: &Path,
    source: &Path,
    symlink: bool,
    builtins: &[String],
) -> Result<Installed> {
    let source = source
        .canonicalize()
        .with_context(|| format!("cannot read {}", source.display()))?;
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("source has no usable file name")?
        .to_owned();
    let (name, tier) = match classify_file_name(&file_name) {
        Some(classified) => classified,
        None => bail!(
            "{file_name} does not follow the plugin naming convention — rename it to \
             audible-<name> (executable) or cmd_<name>.py (python)"
        ),
    };
    if tier == Tier::Executable && !is_executable(&source) {
        bail!("{} is not executable (chmod +x it first)", source.display());
    }
    if tier == Tier::Python && python3().is_none() {
        bail!("cmd_<name>.py plugins need python3 on PATH");
    }
    if builtins.iter().any(|builtin| builtin == &name) {
        bail!("{name:?} collides with a built-in command (plugins cannot override built-ins)");
    }
    if let Some(existing) = discover_in(plugin_dir, builtins, python3())
        .into_iter()
        .find(|plugin| plugin.name == name)
    {
        bail!(
            "{name:?} is already installed from {} — `plugin remove {name}` first",
            existing.source.display()
        );
    }

    // Verify before installing: the source must describe itself.
    let candidate = Discovered {
        name: name.clone(),
        tier,
        source: source.clone(),
        broken: None,
    };
    let manifest = describe(&candidate).await.map_err(|reason| {
        anyhow::anyhow!("{} is not a usable plugin: {reason}", source.display())
    })?;

    std::fs::create_dir_all(plugin_dir)
        .with_context(|| format!("could not create {}", plugin_dir.display()))?;
    let target = plugin_dir.join(&file_name);
    if target.symlink_metadata().is_ok() {
        bail!("{} already exists — remove it first", target.display());
    }
    if symlink {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &target)
            .with_context(|| format!("could not symlink {}", target.display()))?;
        #[cfg(not(unix))]
        bail!("--symlink installs are unix-only; install as a copy instead");
    } else {
        std::fs::copy(&source, &target)
            .with_context(|| format!("could not copy to {}", target.display()))?;
    }
    Ok(Installed {
        name,
        tier,
        manifest,
        target,
    })
}

/// Classifies a would-be plugin file by its name: `audible-<name>`
/// (Tier A) or `cmd_<name>.py` (Tier B); `None` for anything else.
pub(crate) fn classify_file_name(file_name: &str) -> Option<(String, Tier)> {
    if let Some(name) = file_name.strip_prefix("audible-") {
        return Some((name.to_owned(), Tier::Executable));
    }
    file_name
        .strip_prefix("cmd_")
        .and_then(|rest| rest.strip_suffix(".py"))
        .map(|name| (name.to_owned(), Tier::Python))
}

/// A Tier-A candidate; broken when the exec bit is missing.
fn tier_a(name: &str, path: PathBuf) -> Discovered {
    let broken = if is_executable(&path) {
        None
    } else {
        Some("not executable".to_owned())
    };
    Discovered {
        name: name.to_owned(),
        tier: Tier::Executable,
        source: path,
        broken,
    }
}

/// Directory entries sorted by name (stable discovery order); missing or
/// unreadable directories yield nothing.
fn read_dir_sorted(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    entries
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

/// First `python3` on `PATH` (Tier B interpreter).
fn python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| is_executable(candidate))
}

/// Runs `--audible-describe` and parses the manifest. Errors are plain
/// strings — they end up verbatim in `plugin list`'s broken column.
pub async fn describe(plugin: &Discovered) -> Result<Manifest, String> {
    describe_with_timeout(plugin, DESCRIBE_TIMEOUT).await
}

async fn describe_with_timeout(
    plugin: &Discovered,
    timeout: std::time::Duration,
) -> Result<Manifest, String> {
    if let Some(reason) = &plugin.broken {
        return Err(reason.clone());
    }
    let mut command = base_command(plugin).map_err(|error| error.to_string())?;
    command
        .arg("--audible-describe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // A probe must never reach the user's terminal: stdio is captured
    // above, but a candidate may open /dev/tty directly (a non-plugin
    // tool that happens to match the name pattern and prompts, AUD-162).
    // Give the probe its own session so it has no controlling TTY and
    // that open fails instantly instead of prompting/hanging.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("describe timed out after {}s", timeout.as_secs()))?
        .map_err(|error| format!("could not run: {error}"))?;
    if !output.status.success() {
        return Err(describe_failure(&output));
    }
    let manifest: Manifest = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid manifest JSON: {error}"))?;
    for scope in &manifest.scopes {
        if !VALID_SCOPES.contains(&scope.as_str()) {
            return Err(format!(
                "unknown scope {scope:?} (valid: {})",
                VALID_SCOPES.join(", ")
            ));
        }
    }
    Ok(manifest)
}

/// Runs the plugin with argv passed through verbatim and stdio
/// inherited; returns the child's exit code. Describe runs first, so an
/// unusable plugin (broken, invalid manifest, bad scopes) never
/// executes. Manifest scopes ≥ 1 spin up the ephemeral broker (AUD-69):
/// the plugin gets `AUDIBLE_SOCKET`/`AUDIBLE_BROKER_TOKEN` and the
/// socket lives exactly as long as the child.
pub async fn invoke(
    ctx: &Arc<Ctx>,
    plugin: &Discovered,
    args: &[std::ffi::OsString],
) -> Result<i32> {
    let manifest = match describe(plugin).await {
        Ok(manifest) => manifest,
        Err(reason) => bail!(
            "plugin {:?} is not usable: {reason} (source: {})",
            plugin.name,
            plugin.source.display()
        ),
    };
    let broker = if manifest.scopes.is_empty() {
        None
    } else {
        Some(broker::Broker::start(Arc::clone(ctx), manifest.scopes).await?)
    };
    let mut envs: Vec<(String, String)> = Vec::new();
    if let Some(broker) = &broker {
        envs.push((
            "AUDIBLE_SOCKET".to_owned(),
            broker.socket_path.display().to_string(),
        ));
        envs.push(("AUDIBLE_BROKER_TOKEN".to_owned(), broker.token.clone()));
    }
    let result = run(plugin, args, &envs).await;
    if let Some(broker) = broker {
        broker.shutdown().await;
    }
    result
}

/// Spawns the plugin process (extra env for the broker handshake).
async fn run(
    plugin: &Discovered,
    args: &[std::ffi::OsString],
    envs: &[(String, String)],
) -> Result<i32> {
    let mut command = base_command(plugin)?;
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command
        .status()
        .await
        .with_context(|| format!("could not run {}", plugin.source.display()))?;
    Ok(exit_code(status))
}

/// Formats a failed describe probe: the exit status plus the probe's
/// last non-empty stderr line, so `plugin list`/`plugin info` say WHY
/// (e.g. a Python ImportError) — with a targeted hint for the common
/// missing-SDK case.
fn describe_failure(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut reason = format!("describe failed ({})", output.status);
    if let Some(detail) = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        let mut detail: String = detail.chars().take(120).collect();
        if detail.chars().count() == 120 {
            detail.push('…');
        }
        reason.push_str(": ");
        reason.push_str(&detail);
    }
    if stderr.contains("audible_plugin_sdk") {
        reason.push_str(
            " — install the Python SDK (pip install <repo>/sdk/python) or add it to PYTHONPATH",
        );
    }
    reason
}

/// The bare command to run a plugin (interpreter included for Tier B).
fn base_command(plugin: &Discovered) -> Result<tokio::process::Command> {
    Ok(match plugin.tier {
        Tier::Executable => tokio::process::Command::new(&plugin.source),
        Tier::Python => {
            let python = python3().context("python3 is no longer on PATH")?;
            let mut command = tokio::process::Command::new(python);
            command.arg(&plugin.source);
            command
        }
    })
}

/// Exit code of a child: the real code, or 128+signal on unix kills.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

/// Entry point for an unknown CLI subcommand (main.rs, after the
/// built-ins): find the plugin, run it, return its exit code — or
/// `None` when no plugin of that name exists.
pub async fn run_external(
    ctx: &Arc<Ctx>,
    name: &str,
    builtins: &[String],
    args: &[std::ffi::OsString],
) -> Result<Option<i32>> {
    let plugins = discover(ctx, builtins);
    let Some(plugin) = plugins.iter().find(|plugin| plugin.name == name) else {
        return Ok(None);
    };
    invoke(ctx, plugin, args).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn write_plugin(dir: &Path, file_name: &str, body: &str, executable: bool) -> PathBuf {
        let path = dir.join(file_name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// A Tier-A shell plugin answering describe and echoing its argv.
    fn demo_plugin(dir: &Path, name: &str, scopes: &str, exit: i32) -> PathBuf {
        write_plugin(
            dir,
            &format!("audible-{name}"),
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--audible-describe\" ]; then\n\
                   printf '{{\"name\":\"{name}\",\"version\":\"1.0\",\
                 \"description\":\"demo\",\"scopes\":[{scopes}]}}'\n\
                   exit 0\n\
                 fi\n\
                 echo \"ran:$@\"\n\
                 exit {exit}\n"
            ),
            true,
        )
    }

    #[test]
    fn discovery_is_plugin_dir_only_and_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins");
        let path_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();

        demo_plugin(&plugin_dir, "stats", "", 0);
        write_plugin(&plugin_dir, "cmd_stats.py", "print('dup')\n", false);
        demo_plugin(&plugin_dir, "download", "", 0); // collides with a built-in
        write_plugin(&plugin_dir, "audible-noexec", "#!/bin/sh\n", false);
        write_plugin(&plugin_dir, "cmd_pytool.py", "print('hi')\n", false);
        write_plugin(&plugin_dir, "README.md", "not a plugin\n", false);
        // `audible-*` outside the plugin dir is NOT a plugin (AUD-163) —
        // the PATH scan is gone, so this must never surface.
        demo_plugin(&path_dir, "elsewhere", "", 0);

        let python = Some(PathBuf::from("/usr/bin/python3"));
        let plugins = discover_in(&plugin_dir, &["download".to_owned()], python);
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["download", "noexec", "pytool", "stats"]);

        let by_name = |name: &str| plugins.iter().find(|p| p.name == name).unwrap();
        assert_eq!(
            by_name("download").broken.as_deref(),
            Some("name collides with a built-in command")
        );
        assert_eq!(by_name("noexec").broken.as_deref(), Some("not executable"));
        assert_eq!(by_name("pytool").tier, Tier::Python);
        // Same name twice in the dir: the first entry (Tier A) wins.
        assert_eq!(by_name("stats").tier, Tier::Executable);

        // Without python3, Tier B disappears silently.
        let without = discover_in(&plugin_dir, &[], None);
        assert!(!without.iter().any(|p| p.name == "pytool"));
    }

    #[test]
    fn dangling_symlink_is_listed_broken() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let original = write_plugin(tmp.path(), "cmd_linked.py", "print('hi')\n", false);
        std::os::unix::fs::symlink(&original, plugin_dir.join("cmd_linked.py")).unwrap();

        // Healthy symlink: a normal Tier-B plugin.
        let python = Some(PathBuf::from("/usr/bin/python3"));
        let plugins = discover_in(&plugin_dir, &[], python.clone());
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].broken.is_none());

        // Target moves away: the entry must stay visible, with a reason.
        std::fs::remove_file(&original).unwrap();
        let plugins = discover_in(&plugin_dir, &[], python);
        assert_eq!(plugins.len(), 1);
        assert_eq!(
            plugins[0].broken.as_deref(),
            Some("symlink target missing (moved or deleted?)")
        );
    }

    #[tokio::test]
    async fn install_verifies_names_collisions_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugins");
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let good = demo_plugin(&src_dir, "good", "\"api\"", 0);

        // Copy install: verified manifest, file lands in the dir.
        let installed = install(&plugin_dir, &good, false, &[]).await.unwrap();
        assert_eq!(installed.name, "good");
        assert_eq!(installed.manifest.scopes, ["api"]);
        assert!(installed.target.is_file());
        // A second add of the same name is refused.
        let error = install(&plugin_dir, &good, false, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already installed"), "{error}");

        // Built-in collision is refused before anything runs.
        let stats = demo_plugin(&src_dir, "stats", "", 0);
        let error = install(&plugin_dir, &stats, false, &["stats".to_owned()])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("built-in"), "{error}");

        // Naming convention and describe verification are enforced.
        let misnamed = write_plugin(&src_dir, "myplugin", "#!/bin/sh\nexit 0\n", true);
        let error = install(&plugin_dir, &misnamed, false, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("naming convention"), "{error}");
        let undescribed = write_plugin(&src_dir, "audible-mute", "#!/bin/sh\nexit 7\n", true);
        let error = install(&plugin_dir, &undescribed, false, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a usable plugin"), "{error}");
        assert!(!plugin_dir.join("audible-mute").exists());

        // Symlink install: the dir entry is a link to the original.
        let linked = demo_plugin(&src_dir, "linked", "", 0);
        let installed = install(&plugin_dir, &linked, true, &[]).await.unwrap();
        assert!(
            installed
                .target
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // Removing the dir entry keeps the original (remove semantics).
        std::fs::remove_file(&installed.target).unwrap();
        assert!(linked.is_file());
    }

    #[tokio::test]
    async fn describe_parses_validates_and_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        demo_plugin(tmp.path(), "good", "\"api\",\"config\"", 0);
        demo_plugin(tmp.path(), "badscope", "\"root\"", 0);
        write_plugin(
            tmp.path(),
            "audible-notjson",
            "#!/bin/sh\necho not json\n",
            true,
        );
        write_plugin(tmp.path(), "audible-slow", "#!/bin/sh\nsleep 5\n", true);

        let plugins = discover_in(tmp.path(), &[], None);
        let by_name = |name: &str| plugins.iter().find(|p| p.name == name).unwrap();

        let manifest = describe(by_name("good")).await.unwrap();
        assert_eq!(manifest.name, "good");
        assert_eq!(manifest.scopes, ["api", "config"]);

        let error = describe(by_name("badscope")).await.unwrap_err();
        assert!(error.contains("unknown scope"), "{error}");
        let error = describe(by_name("notjson")).await.unwrap_err();
        assert!(error.contains("invalid manifest JSON"), "{error}");
        let error = describe_with_timeout(by_name("slow"), std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn broken_reason_carries_stderr_and_sdk_hint() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(
            tmp.path(),
            "audible-crash",
            "#!/bin/sh\necho 'Traceback (most recent call last):' >&2\n\
             echo \"ModuleNotFoundError: No module named 'audible_plugin_sdk'\" >&2\nexit 1\n",
            true,
        );
        write_plugin(
            tmp.path(),
            "audible-mute",
            "#!/bin/sh\nexit 7\n", // no stderr at all
            true,
        );

        let plugins = discover_in(tmp.path(), &[], None);
        let by_name = |name: &str| plugins.iter().find(|p| p.name == name).unwrap();

        // The last non-empty stderr line lands in the reason, plus the
        // targeted hint for the missing-SDK case.
        let error = describe(by_name("crash")).await.unwrap_err();
        assert!(
            error.contains("ModuleNotFoundError: No module named 'audible_plugin_sdk'"),
            "{error}"
        );
        assert!(error.contains("PYTHONPATH"), "{error}");

        // Silent failures keep the bare status form.
        let error = describe(by_name("mute")).await.unwrap_err();
        assert_eq!(error, "describe failed (exit status: 7)");
    }

    #[tokio::test]
    async fn tty_grabbing_probe_fails_fast_without_prompting() {
        let tmp = tempfile::tempdir().unwrap();
        // Mimics an interactive non-plugin tool (AUD-162): prompts and
        // reads from /dev/tty directly, bypassing the captured stdio.
        // With the probe in its own session there is no controlling TTY,
        // so the open fails immediately — classified broken, no timeout.
        write_plugin(
            tmp.path(),
            "audible-ttygrab",
            "#!/bin/sh\nprintf 'passphrase: ' > /dev/tty || exit 9\nread -r _ < /dev/tty\nexit 0\n",
            true,
        );

        let plugins = discover_in(tmp.path(), &[], None);
        let error = describe(&plugins[0]).await.unwrap_err();
        assert!(error.contains("describe failed"), "{error}");
        assert!(!error.contains("timed out"), "{error}");
    }

    /// A Ctx over a throwaway config dir (no auth involved).
    fn test_ctx(tmp: &Path) -> Arc<Ctx> {
        let config_dir = tmp.join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "version = 1\ndefault_account = \"t\"\n\n[accounts.t]\nauth_file = \"t.auth\"\n\
                 marketplaces = [\"de\"]\ndefault_marketplaces = [\"de\"]\n\n\
                 [session]\nsocket_dir = {:?}\n",
                tmp.join("run")
            ),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("run")).unwrap();
        Arc::new(Ctx::with_dir(config_dir, Default::default()).unwrap())
    }

    #[tokio::test]
    async fn invoke_passes_argv_and_propagates_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        demo_plugin(tmp.path(), "seven", "", 7);
        let plugins = discover_in(tmp.path(), &[], None);
        let code = invoke(&ctx, &plugins[0], &["--flag".into(), "value".into()])
            .await
            .unwrap();
        assert_eq!(code, 7);

        // A broken plugin never executes.
        write_plugin(tmp.path(), "audible-dead", "#!/bin/sh\n", false);
        let plugins = discover_in(tmp.path(), &[], None);
        let dead = plugins.iter().find(|p| p.name == "dead").unwrap();
        let error = invoke(&ctx, dead, &[]).await.unwrap_err().to_string();
        assert!(error.contains("not executable"), "{error}");
    }

    /// A scoped plugin gets the broker env pair; an unscoped one does not.
    #[tokio::test]
    async fn invoke_injects_broker_env_for_scoped_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        write_plugin(
            tmp.path(),
            "audible-scoped",
            "#!/bin/sh\n\
             if [ \"$1\" = \"--audible-describe\" ]; then\n\
               printf '{\"name\":\"scoped\",\"scopes\":[\"config\"]}'\n exit 0\n\
             fi\n\
             [ -n \"$AUDIBLE_SOCKET\" ] && [ -n \"$AUDIBLE_BROKER_TOKEN\" ] || exit 3\n\
             [ -S \"$AUDIBLE_SOCKET\" ] || exit 4\n",
            true,
        );
        write_plugin(
            tmp.path(),
            "audible-unscoped",
            "#!/bin/sh\n\
             if [ \"$1\" = \"--audible-describe\" ]; then\n\
               printf '{\"name\":\"unscoped\",\"scopes\":[]}'\n exit 0\n\
             fi\n\
             [ -z \"$AUDIBLE_SOCKET\" ] || exit 5\n",
            true,
        );
        let plugins = discover_in(tmp.path(), &[], None);
        let by_name = |name: &str| plugins.iter().find(|p| p.name == name).unwrap();
        assert_eq!(invoke(&ctx, by_name("scoped"), &[]).await.unwrap(), 0);
        assert_eq!(invoke(&ctx, by_name("unscoped"), &[]).await.unwrap(), 0);
    }
}
