//! `audible self` — release awareness (AUD-179): is there a newer version,
//! and what changed since the one you run?
//!
//! Read-only by design: it informs, it never replaces the binary (that is
//! `self update`, AUD-142). The data comes from the GitHub Releases API,
//! unauthenticated — the release **body** already carries the generated
//! changelog plus any hand-added notes (e.g. a database migration notice),
//! so it is exactly what the user needs to see.
//!
//! Pre-release handling mirrors `install.sh` (`resolve_tag`): stable by
//! default, `--pre` to track pre-releases, and an automatic fall back to
//! pre-releases while no stable release exists — without that, the check
//! would report "up to date" forever until v0.1.0 ships. A user already
//! running a pre-release always sees pre-releases, or they would be blind
//! to their own track.

use std::cmp::Ordering;

use anyhow::Result;
use clap::{Arg, ArgAction};
use semver::Version;
use serde::Deserialize;

use crate::config::ctx::Ctx;

/// The repository this binary is built from.
const REPO_API: &str = "https://api.github.com/repos/mkb79/audible-rs";
/// Releases of this repository (the binary's own project).
const RELEASES_URL: &str = "https://api.github.com/repos/mkb79/audible-rs/releases";
/// The branch releases are cut from — the head `--unreleased` compares to.
const DEFAULT_BRANCH: &str = "main";
/// The installer the upgrade hint points at — `install.sh` on Unix, its
/// PowerShell analog `install.ps1` on Windows.
#[cfg(not(windows))]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/mkb79/audible-rs/main/install.sh";
#[cfg(windows)]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/mkb79/audible-rs/main/install.ps1";

/// The platform-appropriate upgrade one-liner printed by `self check`: the
/// `curl … | sh` installer on Unix (honouring `--pre`), the PowerShell
/// `irm … | iex` form on Windows (the piped one-liner takes no flags — `-Pre`
/// lives on `install.ps1` itself).
#[cfg(not(windows))]
fn upgrade_hint(tracking_pre: bool) -> String {
    let flag = if tracking_pre { " -s -- --pre" } else { "" };
    format!("curl -fsSL {INSTALL_URL} | sh{flag}")
}
#[cfg(windows)]
fn upgrade_hint(_tracking_pre: bool) -> String {
    format!("irm {INSTALL_URL} | iex")
}
/// GitHub's unauthenticated rate limit, named in the fail-soft message so
/// the user can make sense of a 403 (it is theirs to keep an eye on).
const RATE_LIMIT_HINT: &str =
    "GitHub allows 60 unauthenticated requests per hour and IP — try again later";

/// One release as the GitHub API returns it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Release {
    /// Git tag, e.g. `v0.1.0-alpha.4`.
    pub tag_name: String,
    /// `true` for a pre-release.
    pub prerelease: bool,
    /// Release notes (the generated changelog plus any hand-added notes).
    #[serde(default)]
    pub body: Option<String>,
    /// Publication timestamp (ISO-8601), if published.
    #[serde(default)]
    pub published_at: Option<String>,
}

impl Release {
    /// The tag parsed as a SemVer version (`v` stripped). `None` for tags
    /// that are not valid SemVer — those are ignored rather than guessed at.
    fn version(&self) -> Option<Version> {
        Version::parse(self.tag_name.trim_start_matches('v')).ok()
    }
}

/// The version this binary reports. Not the bare crate version: a build from
/// source between two releases carries its commit as build metadata
/// (`0.1.0-alpha.4+g1a2b3c4`, see `build.rs`, AUD-180).
fn running_version() -> Version {
    Version::parse(env!("AUDIBLE_BUILD_VERSION"))
        .expect("the build version is valid SemVer (build.rs derives it from the crate version)")
}

/// The commit of a build that is not a published release, `None` for a
/// release build.
///
/// SemVer's *precedence* order ignores build metadata, so such a build ranks
/// equal to the release it was cut from — "no newer release" would then read
/// as "you run that release". Every "are you up to date?" answer therefore has
/// to consult this, not the version alone. (The derived `==` and `<` on
/// `Version`, by contrast, *do* compare build metadata — see
/// [`select_releases`].)
fn dev_commit(running: &Version) -> Option<&str> {
    running.build.as_str().strip_prefix('g')
}

/// Which slice of the release history to render. Each variant is named for
/// what it *contains*, so no flag ever excludes the thing it is named after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    /// Every release (the `self changelog` default — a changelog is the whole
    /// document).
    All,
    /// Exactly the installed version: "what did I actually get?" (`--current`).
    Installed,
    /// Strictly newer than the installed version: "what would an upgrade
    /// bring?" (`--newer`, and `self check`).
    Newer,
    /// This version and everything after it — **inclusive**, because the user
    /// named the release and means "start here" (`--since`). It is also the
    /// only way to reach the notes of the very first release.
    From(Version),
}

/// The releases the selection covers, oldest first — so a user several
/// versions behind reads the changelogs in the order they happened and
/// nothing in between is skipped (AUD-179).
///
/// `pre` requests pre-releases explicitly. They are also included when the
/// installed version (or an explicitly named `--since` version) is itself a
/// pre-release — else the user could not see their own track — and,
/// mirroring the installer, while the repository has no stable release yet.
pub(crate) fn select_releases(
    releases: &[Release],
    selection: &Selection,
    running: &Version,
    pre: bool,
) -> Vec<Release> {
    let include_pre = tracking_prereleases(releases, selection, running, pre);
    let mut selected: Vec<(Version, Release)> = releases
        .iter()
        .filter(|release| include_pre || !release.prerelease)
        .filter_map(|release| release.version().map(|v| (v, release.clone())))
        // `cmp_precedence`, never `==`/`>`: those derive over *all* fields,
        // build metadata included. A build from source carries its commit
        // there (`0.1.0-alpha.4+g1a2b3c4`), so `==` would match no release at
        // all — `--current` would answer nothing instead of naming the release
        // the build was cut from. `cmp_precedence` is the SemVer-spec order,
        // which ignores build metadata; the release it was cut from therefore
        // compares equal, and only genuinely later releases count as newer
        // (AUD-180).
        .filter(|(version, _)| match selection {
            Selection::All => true,
            Selection::Installed => version.cmp_precedence(running) == Ordering::Equal,
            Selection::Newer => version.cmp_precedence(running) == Ordering::Greater,
            Selection::From(from) => version.cmp_precedence(from) != Ordering::Less,
        })
        .collect();
    selected.sort_by(|a, b| a.0.cmp_precedence(&b.0));
    selected.into_iter().map(|(_, release)| release).collect()
}

/// One commit of the GitHub compare API.
#[derive(Debug, Clone, Deserialize)]
struct CompareCommit {
    commit: CommitDetail,
}

#[derive(Debug, Clone, Deserialize)]
struct CommitDetail {
    message: String,
}

/// The compare API's payload (`/compare/<base>...<head>`).
#[derive(Debug, Clone, Deserialize)]
struct Comparison {
    #[serde(default)]
    commits: Vec<CompareCommit>,
}

/// The changelog section a commit belongs to — the Rust twin of
/// `cliff.toml`'s `commit_parsers`, so `--unreleased` groups and filters
/// exactly like a released changelog would (AUD-179). `None` = not
/// user-visible (ci/chore/docs/style/test/refactor/build, the release bump
/// commit, and anything unrecognized) and therefore omitted, mirroring the
/// catch-all `skip = true`.
fn changelog_group(subject: &str) -> Option<&'static str> {
    // Order matters, as in cliff.toml: fix(security) must win over fix.
    if subject.starts_with("feat") {
        Some("Added")
    } else if subject.starts_with("fix(security)") || subject.starts_with("security") {
        Some("Security")
    } else if subject.starts_with("fix") {
        Some("Fixed")
    } else if subject.starts_with("perf") {
        Some("Performance")
    } else if subject.starts_with("revert") {
        Some("Reverted")
    } else {
        // Includes `release: vX.Y.Z` (the workflow's own bump commit).
        None
    }
}

/// Groups the user-visible commits of a comparison, in changelog order.
/// Merge commits from squash-merged PRs carry the PR title as the subject,
/// which is exactly the conventional-commit line the changelog uses.
fn unreleased_entries(comparison: &Comparison) -> Vec<(&'static str, Vec<String>)> {
    const ORDER: [&str; 5] = ["Added", "Fixed", "Performance", "Security", "Reverted"];
    let mut grouped: std::collections::BTreeMap<&'static str, Vec<String>> =
        std::collections::BTreeMap::new();
    for entry in &comparison.commits {
        // Only the subject line; the body is not changelog material.
        let subject = entry.commit.message.lines().next().unwrap_or("").trim();
        if let Some(group) = changelog_group(subject) {
            grouped.entry(group).or_default().push(subject.to_owned());
        }
    }
    ORDER
        .iter()
        .filter_map(|group| grouped.remove_entry(group))
        .collect()
}

/// The newest pre-release above `base` — the one `--pre` would have counted
/// as shipped. `None` while pre-releases are tracked anyway (nothing is being
/// skipped then).
///
/// Without `--pre` the comparison starts at the newest *stable* release, so
/// commits that already ship in a pre-release are listed as unreleased. They
/// are perfectly installable — just not on the track the user asked for — and
/// `--unreleased` says so rather than leaving them to wonder.
fn newest_skipped_prerelease(
    releases: &[Release],
    base: &Version,
    include_pre: bool,
) -> Option<String> {
    if include_pre {
        return None;
    }
    let mut newer: Vec<(Version, &Release)> = releases
        .iter()
        .filter(|release| release.prerelease)
        .filter_map(|release| release.version().map(|version| (version, release)))
        .filter(|(version, _)| version.cmp_precedence(base) == Ordering::Greater)
        .collect();
    newer.sort_by(|a, b| a.0.cmp_precedence(&b.0));
    newer.pop().map(|(_, release)| release.tag_name.clone())
}

/// Whether pre-releases are being tracked (for the upgrade hint and the
/// "no stable release yet" notice).
fn tracking_prereleases(
    releases: &[Release],
    selection: &Selection,
    running: &Version,
    pre: bool,
) -> bool {
    let named_prerelease = match selection {
        Selection::From(from) => !from.pre.is_empty(),
        _ => false,
    };
    pre || named_prerelease
        || !running.pre.is_empty()
        || !releases.iter().any(|release| !release.prerelease)
}

/// `audible self`.
pub struct SelfCommand;

#[async_trait::async_trait]
impl super::Command for SelfCommand {
    fn name(&self) -> &'static str {
        "self"
    }

    fn clap(&self) -> clap::Command {
        let pre = || {
            Arg::new("pre").long("pre").action(ArgAction::SetTrue).help(
                "Include pre-releases (implied while no stable release exists, \
                     and when you already run a pre-release)",
            )
        };
        clap::Command::new(self.name())
            .about("This installation: check for a newer release and read what changed")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("check")
                    .about("Check whether a newer release is available")
                    .arg(pre())
                    .arg(
                        Arg::new("changelog")
                            .long("changelog")
                            .action(ArgAction::SetTrue)
                            .help("Also print the release notes of each newer release"),
                    ),
            )
            .subcommand(
                clap::Command::new("changelog")
                    .about("Read the release notes (every release by default)")
                    .arg(pre())
                    .arg(
                        Arg::new("current")
                            .long("current")
                            .action(ArgAction::SetTrue)
                            .help("Only the release you run — what your version brought"),
                    )
                    .arg(
                        Arg::new("newer")
                            .long("newer")
                            .action(ArgAction::SetTrue)
                            .help(
                                "Only releases newer than the one you run — what an \
                                 upgrade would bring",
                            ),
                    )
                    .arg(Arg::new("since").long("since").value_name("VERSION").help(
                        "Start at this version, including it (with or without the \
                                 leading v, e.g. 0.1.0-alpha.2 or v0.1.0-alpha.2)",
                    ))
                    .arg(
                        Arg::new("unreleased")
                            .long("unreleased")
                            .action(ArgAction::SetTrue)
                            .help(
                                "Changes merged after the newest release, not shipped yet \
                                 — answers \"is my fix in a release already?\"",
                            ),
                    )
                    // One slice at a time: these are different questions.
                    .group(
                        clap::ArgGroup::new("selection")
                            .args(["current", "newer", "since", "unreleased"])
                            .multiple(false),
                    ),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some(("check", sub)) => {
                run(
                    ctx,
                    Mode::Check {
                        changelog: sub.get_flag("changelog"),
                    },
                    // `check` always asks the upgrade question: what is newer
                    // than what is installed.
                    Selection::Newer,
                    sub.get_flag("pre"),
                    RELEASES_URL,
                    REPO_API,
                )
                .await
            }
            Some(("changelog", sub)) if sub.get_flag("unreleased") => {
                run_unreleased(ctx, sub.get_flag("pre"), RELEASES_URL, REPO_API).await
            }
            Some(("changelog", sub)) => {
                // Bad user input fails closed (unlike the network, which is
                // fail-soft): a typo must not be read as "nothing changed".
                let selection = match sub.get_one::<String>("since") {
                    Some(raw) => Selection::From(parse_version_arg(raw)?),
                    None if sub.get_flag("current") => Selection::Installed,
                    None if sub.get_flag("newer") => Selection::Newer,
                    // A changelog is the whole document unless narrowed.
                    None => Selection::All,
                };
                run(
                    ctx,
                    Mode::Changelog,
                    selection,
                    sub.get_flag("pre"),
                    RELEASES_URL,
                    REPO_API,
                )
                .await
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

/// `self changelog --unreleased`: what is merged on the release branch but
/// not shipped yet. Read from the compare API rather than the repository's
/// `CHANGELOG.md`, whose `[Unreleased]` section only exists when the
/// changelog workflow happened to run — the comparison is always current.
/// Commits are filtered and grouped exactly like a released changelog
/// (see [`changelog_group`], the twin of `cliff.toml`).
async fn run_unreleased(ctx: &Ctx, pre: bool, releases_url: &str, repo_api: &str) -> Result<()> {
    let running = running_version();

    // Fail-soft, like every other network path here.
    let releases = match fetch_releases(releases_url).await {
        Ok(releases) => releases,
        Err(error) => {
            eprintln!("could not reach GitHub to check for releases: {error}");
            return Ok(());
        }
    };
    // The baseline is the newest release on the user's track — so `--pre`
    // decides whether a pre-release counts as "shipped".
    let newest = select_releases(&releases, &Selection::All, &running, pre).pop();
    let Some(base_release) = newest else {
        eprintln!("no releases published yet — everything on {DEFAULT_BRANCH} is unreleased");
        return Ok(());
    };
    let base_version = base_release
        .version()
        .expect("a selected release parses as SemVer");
    let base = base_release.tag_name;
    // On the stable track, everything a newer pre-release already carries is
    // listed here as unreleased. Name that pre-release instead of letting the
    // user conclude the changes are nowhere to be had.
    let include_pre = tracking_prereleases(&releases, &Selection::All, &running, pre);
    let skipped_prerelease = newest_skipped_prerelease(&releases, &base_version, include_pre);

    // A dev build compares against its own commit: "unreleased" then means
    // what *this* binary carries beyond the release, not what happens to sit
    // on main. Its commit may be unknown to GitHub (an unpushed branch) —
    // then fall back to main and say which question is being answered.
    let mut head = dev_commit(&running).unwrap_or(DEFAULT_BRANCH).to_owned();
    let comparison = match fetch_comparison(repo_api, &base, &head).await {
        Ok(comparison) => comparison,
        Err(error) if head != DEFAULT_BRANCH => {
            eprintln!(
                "the commit of your build ({head}) is not on GitHub — an unpushed branch? \
                 Showing what is on {DEFAULT_BRANCH} instead ({error})"
            );
            head = DEFAULT_BRANCH.to_owned();
            match fetch_comparison(repo_api, &base, &head).await {
                Ok(comparison) => comparison,
                Err(error) => {
                    eprintln!("could not compare {base}…{head}: {error}");
                    return Ok(());
                }
            }
        }
        Err(error) => {
            eprintln!("could not compare {base}…{head}: {error}");
            return Ok(());
        }
    };
    let entries = unreleased_entries(&comparison);

    if ctx.output_format() == crate::output::OutputFormat::Json {
        ctx.print(&crate::output::Output::Json(serde_json::json!({
            "running": running.to_string(),
            "since_release": base,
            // What the release was compared against: main, or the commit of
            // this binary when it is a build from source.
            "head": head,
            // The pre-release these commits partly ship in already, if the
            // stable track hid it; null when pre-releases are tracked.
            "skipped_prerelease": skipped_prerelease,
            "groups": entries
                .iter()
                .map(|(group, subjects)| serde_json::json!({
                    "group": group,
                    "commits": subjects,
                }))
                .collect::<Vec<_>>(),
        })));
        return Ok(());
    }

    if entries.is_empty() {
        eprintln!("nothing unreleased — {base} is the current state of {head}");
        return Ok(());
    }

    println!("Unreleased — merged after {base}, not shipped yet:");
    for (group, subjects) in &entries {
        println!();
        println!("### {group}");
        for subject in subjects {
            println!("- {subject}");
        }
    }
    println!();
    match (&skipped_prerelease, head == DEFAULT_BRANCH) {
        (Some(tag), _) => println!(
            "Pre-releases don't count as shipped without `--pre` — part of this \
             already ships in {tag}, which you can install."
        ),
        // The comparison ran against this binary's own commit, so these
        // changes are not "coming" — they are what it is built from.
        (None, false) => {
            println!("These changes are in your build already, but in no release yet.")
        }
        (None, true) => {
            println!("These changes are not installable yet — they ship with the next release.")
        }
    }
    Ok(())
}

/// Which of the two commands is running (they share the fetch, the selection
/// and the renderer; only the framing differs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `self check`: the upgrade question. Terse unless `changelog` is set.
    Check { changelog: bool },
    /// `self changelog`: the release notes themselves.
    Changelog,
}

/// Parses a `--since` value: SemVer, with or without the leading `v`.
fn parse_version_arg(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    Version::parse(trimmed.strip_prefix('v').unwrap_or(trimmed)).map_err(|error| {
        anyhow::anyhow!("{raw:?} is not a valid version: {error} (e.g. 0.1.0 or v0.1.0-alpha.2)")
    })
}

/// Shared implementation of both subcommands; the URLs are injectable for
/// tests.
async fn run(
    ctx: &Ctx,
    mode: Mode,
    selection: Selection,
    pre: bool,
    url: &str,
    repo_api: &str,
) -> Result<()> {
    let running = running_version();
    let dev = dev_commit(&running);

    // Fail-soft: a missing network or an exhausted rate limit is a notice,
    // never an error — the command must not break a script.
    let releases = match fetch_releases(url).await {
        Ok(releases) => releases,
        Err(error) => {
            eprintln!("could not reach GitHub to check for releases: {error}");
            return Ok(());
        }
    };

    let selected = select_releases(&releases, &selection, &running, pre);
    let tracking_pre = tracking_prereleases(&releases, &selection, &running, pre);
    if tracking_pre && !pre && running.pre.is_empty() {
        eprintln!("no stable release yet — showing pre-releases");
    }

    // `--current` on a build from source is only half an answer: the release
    // it was cut from, plus the commits this binary carries on top. Those
    // come from the compare API, as in `--unreleased`.
    let mut adds: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut adds_error: Option<String> = None;
    if let (Some(commit), Selection::Installed, Some(base)) = (dev, &selection, selected.last()) {
        match fetch_comparison(repo_api, &base.tag_name, commit).await {
            Ok(comparison) => adds = unreleased_entries(&comparison),
            Err(error) => adds_error = Some(error.to_string()),
        }
    }

    if selected.is_empty() {
        match (&mode, &selection) {
            // A dev build compares equal to the release it was cut from, so
            // "no newer release" must not be read as "you run that release".
            (Mode::Check { .. }, _) if dev.is_some() => {
                let newest = select_releases(&releases, &Selection::All, &running, pre)
                    .pop()
                    .map(|release| release.tag_name)
                    .unwrap_or_else(|| "none".to_owned());
                eprintln!(
                    "audible {running} is a build from source, not a published release — \
                     the newest release is {newest}"
                );
            }
            (Mode::Check { .. }, _) => {
                eprintln!("audible {running} is the newest release — you're up to date");
            }
            // No release carries this version — the usual reason is a build
            // from source rather than an installed release.
            (_, Selection::Installed) => eprintln!(
                "no release matches audible {running} — a development build, or the \
                 release was withdrawn"
            ),
            (_, Selection::From(from)) => eprintln!("no releases from {from} onwards"),
            (_, Selection::Newer) => {
                eprintln!("no releases published after audible {running}");
            }
            (_, Selection::All) => eprintln!("no releases published yet"),
        }
        return Ok(());
    }

    if ctx.output_format() == crate::output::OutputFormat::Json {
        let array: Vec<serde_json::Value> = selected
            .iter()
            .map(|release| {
                serde_json::json!({
                    "tag": release.tag_name,
                    "prerelease": release.prerelease,
                    "published": release.published_at,
                    "changelog": release.body,
                })
            })
            .collect();
        ctx.print(&crate::output::Output::Json(serde_json::json!({
            "running": running.to_string(),
            // The commit a build from source was made at; null for a release.
            "build_commit": dev,
            "releases": array,
            // What that build carries on top of the release it was cut from
            // (`--current` only); null when it could not be determined.
            "adds": match (dev, &selection) {
                (Some(_), Selection::Installed) if adds_error.is_none() => Some(
                    adds.iter()
                        .map(|(group, subjects)| serde_json::json!({
                            "group": group,
                            "commits": subjects,
                        }))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            },
        })));
        return Ok(());
    }

    let plural = if selected.len() == 1 { "" } else { "s" };
    let count = selected.len();
    match (&mode, &selection) {
        (Mode::Check { .. }, _) if dev.is_some() => {
            println!(
                "You run audible {running} — a build from source; \
                 {count} newer release{plural} available."
            );
        }
        (Mode::Check { .. }, _) => {
            println!("You run audible {running} — {count} newer release{plural} available.");
        }
        (_, Selection::All) => println!("{count} release{plural} (you run {running})."),
        // Not "the release you run": a build from source is none.
        (_, Selection::Installed) if dev.is_some() => {
            println!("audible {running} — a build from source, not a published release.");
            println!("It was cut from this release:");
        }
        (_, Selection::Installed) => println!("audible {running} — the release you run."),
        (_, Selection::Newer) => {
            println!("{count} release{plural} newer than audible {running}.");
        }
        // Inclusive: the named version is part of the listing, so do not
        // claim these were published *since* it.
        (_, Selection::From(from)) => {
            println!("{count} release{plural} from {from} onwards (you run {running}).");
        }
    }

    // Terse for a bare `self check`: which versions are newer, no wall of
    // notes. The notes are one flag away.
    let notes = !matches!(mode, Mode::Check { changelog: false });

    // Oldest first: read forward through everything that happened, so no
    // intermediate release is skipped.
    for release in &selected {
        let date = release
            .published_at
            .as_deref()
            .and_then(|stamp| stamp.split('T').next())
            .unwrap_or("");
        let tag = &release.tag_name;
        let marker = if release.prerelease {
            " [pre-release]"
        } else {
            ""
        };
        let dated = if date.is_empty() {
            String::new()
        } else {
            format!(" ({date})")
        };
        if !notes {
            println!("  {tag}{dated}{marker}");
            continue;
        }
        println!();
        println!("── {tag}{dated}{marker}");
        match release.body.as_deref().map(str::trim) {
            Some(body) if !body.is_empty() => println!("{body}"),
            _ => println!("(no release notes)"),
        }
    }

    // `--current` on a build from source: the release above is where it was
    // cut from, this is what it carries beyond it.
    if let (Some(commit), Selection::Installed) = (dev, &selection) {
        println!();
        if let Some(error) = &adds_error {
            eprintln!(
                "could not list what your build adds: the commit {commit} is unknown to \
                 GitHub — an unpushed branch? ({error})"
            );
        } else if adds.is_empty() {
            println!("Your build adds nothing user-visible on top of it.");
        } else {
            println!("Your build adds, on top of it:");
            for (group, subjects) in &adds {
                println!();
                println!("### {group}");
                for subject in subjects {
                    println!("- {subject}");
                }
            }
        }
    }

    if let Mode::Check { changelog } = mode {
        println!();
        if !changelog {
            println!("Release notes:  audible self check --changelog");
        }
        println!("Upgrade:        {}", upgrade_hint(tracking_pre));
    }
    Ok(())
}

/// GETs a GitHub API endpoint and decodes it. Errors carry a usable reason —
/// a 403/429 names the unauthenticated rate limit, which is the common cause.
async fn github_get<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let http = reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .user_agent(concat!("audible-rs/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = http
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        anyhow::bail!("HTTP {status} — {RATE_LIMIT_HINT}");
    }
    let response = response.error_for_status()?;
    Ok(response.json().await?)
}

/// Fetches the release list.
async fn fetch_releases(url: &str) -> Result<Vec<Release>> {
    github_get(url).await
}

/// Fetches the commits between a release tag and the release branch.
async fn fetch_comparison(repo_api: &str, base: &str, head: &str) -> Result<Comparison> {
    github_get(&format!("{repo_api}/compare/{base}...{head}")).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // The upgrade hint is platform-split, so a Unix CI never sees the Windows
    // branch — assert both against the real installer URL for the target.
    #[cfg(not(windows))]
    #[test]
    fn upgrade_hint_is_the_shell_installer() {
        assert!(INSTALL_URL.ends_with("install.sh"));
        assert_eq!(
            upgrade_hint(false),
            format!("curl -fsSL {INSTALL_URL} | sh")
        );
        assert_eq!(
            upgrade_hint(true),
            format!("curl -fsSL {INSTALL_URL} | sh -s -- --pre")
        );
    }

    #[cfg(windows)]
    #[test]
    fn upgrade_hint_is_the_powershell_installer() {
        assert!(INSTALL_URL.ends_with("install.ps1"));
        // The piped one-liner takes no flags, so --pre does not change it.
        assert_eq!(upgrade_hint(false), format!("irm {INSTALL_URL} | iex"));
        assert_eq!(upgrade_hint(true), format!("irm {INSTALL_URL} | iex"));
    }

    /// A Ctx over a throwaway config dir — `self` needs no account, it only
    /// uses the output format.
    fn test_ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "version = 1\n").unwrap();
        let ctx = Ctx::with_dir(dir.path().to_path_buf(), Default::default()).unwrap();
        (dir, ctx)
    }

    fn tags(releases: &[Release]) -> Vec<&str> {
        releases
            .iter()
            .map(|release| release.tag_name.as_str())
            .collect()
    }

    fn release(tag: &str, prerelease: bool) -> Release {
        Release {
            tag_name: tag.to_owned(),
            prerelease,
            body: Some(format!("notes for {tag}")),
            published_at: Some("2026-07-14T10:00:00Z".to_owned()),
        }
    }

    /// A build from source (AUD-180) must never pass for the release it was
    /// cut from. The trap: `Version` derives `Ord`/`Eq` over **all** fields,
    /// build metadata included — so `0.1.0-alpha.4+g1a2b3c4 == 0.1.0-alpha.4`
    /// is `false` and `<` even orders them. Only `cmp_precedence` implements
    /// the SemVer order that ignores build metadata. Selection must use it,
    /// or `--current` finds no release and `--newer` gets it backwards.
    #[test]
    fn a_dev_build_selects_the_release_it_was_cut_from() {
        let dev = Version::parse("0.1.0-alpha.4+g1a2b3c4").unwrap();
        let cut_from = Version::parse("0.1.0-alpha.4").unwrap();

        // The trap itself — guarding against a future `==`/`>` creeping back.
        assert_ne!(dev, cut_from, "derived equality compares build metadata");
        assert_eq!(dev.cmp_precedence(&cut_from), Ordering::Equal);

        let releases = [
            release("v0.1.0-alpha.5", true),
            release("v0.1.0-alpha.4", true),
        ];
        assert_eq!(dev_commit(&dev), Some("1a2b3c4"));
        assert_eq!(dev_commit(&cut_from), None, "a release build has no commit");

        // --current names the release the build was cut from, not nothing.
        let current = select_releases(&releases, &Selection::Installed, &dev, false);
        assert_eq!(tags(&current), ["v0.1.0-alpha.4"]);

        // --newer: the release it was cut from is not an upgrade; alpha.5 is.
        let newer = select_releases(&releases, &Selection::Newer, &dev, false);
        assert_eq!(tags(&newer), ["v0.1.0-alpha.5"]);
    }

    /// The API returns newest-first; we must render oldest-first and include
    /// EVERY intermediate release, not just the newest one (the core ask).
    #[test]
    fn lists_every_intermediate_release_oldest_first() {
        let releases = [
            release("v0.3.0", false),
            release("v0.2.0", false),
            release("v0.1.0", false),
        ];
        let current = Version::parse("0.1.0").unwrap();
        let newer = select_releases(&releases, &Selection::Newer, &current, false);
        let tags: Vec<&str> = newer.iter().map(|r| r.tag_name.as_str()).collect();
        assert_eq!(tags, ["v0.2.0", "v0.3.0"], "both, oldest first");
    }

    #[test]
    fn up_to_date_and_dev_builds_yield_nothing() {
        let releases = [release("v0.1.0", false)];
        // Exactly the newest release.
        let current = Version::parse("0.1.0").unwrap();
        assert!(select_releases(&releases, &Selection::Newer, &current, false).is_empty());
        // A dev build ahead of every release.
        let ahead = Version::parse("0.2.0").unwrap();
        assert!(select_releases(&releases, &Selection::Newer, &ahead, false).is_empty());
    }

    /// Pre-release identifiers must order numerically (alpha.10 > alpha.2) —
    /// the reason we compare with SemVer instead of strings.
    #[test]
    fn prerelease_ordering_is_semver_not_lexical() {
        let releases = [
            release("v0.1.0-alpha.10", true),
            release("v0.1.0-alpha.3", true),
            release("v0.1.0-alpha.2", true),
        ];
        let current = Version::parse("0.1.0-alpha.2").unwrap();
        let newer = select_releases(&releases, &Selection::Newer, &current, false);
        let tags: Vec<&str> = newer.iter().map(|r| r.tag_name.as_str()).collect();
        assert_eq!(
            tags,
            ["v0.1.0-alpha.3", "v0.1.0-alpha.10"],
            "alpha.10 sorts after alpha.3, and both show without --pre \
             because the running version is a pre-release"
        );
    }

    /// Mirrors `install.sh`: stable-only by default, `--pre` opts in, and
    /// with no stable release at all the pre-releases show anyway (otherwise
    /// the check would report "up to date" forever until v0.1.0).
    #[test]
    fn prerelease_visibility_mirrors_the_installer() {
        let mixed = [
            release("v0.2.0-alpha.1", true),
            release("v0.1.0", false),
            release("v0.1.0-alpha.1", true),
        ];
        let stable_user = Version::parse("0.1.0").unwrap();

        // Default: stable only → nothing newer (the alpha is not offered).
        assert!(select_releases(&mixed, &Selection::Newer, &stable_user, false).is_empty());
        assert!(!tracking_prereleases(
            &mixed,
            &Selection::Newer,
            &stable_user,
            false
        ));

        // --pre: the alpha appears.
        let with_pre = select_releases(&mixed, &Selection::Newer, &stable_user, true);
        assert_eq!(with_pre.len(), 1);
        assert_eq!(with_pre[0].tag_name, "v0.2.0-alpha.1");
        assert!(tracking_prereleases(
            &mixed,
            &Selection::Newer,
            &stable_user,
            true
        ));

        // No stable release exists yet → pre-releases show without --pre.
        let only_pre = [
            release("v0.1.0-alpha.4", true),
            release("v0.1.0-alpha.3", true),
        ];
        let current = Version::parse("0.1.0-alpha.3").unwrap();
        let newer = select_releases(&only_pre, &Selection::Newer, &current, false);
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].tag_name, "v0.1.0-alpha.4");
        assert!(tracking_prereleases(
            &only_pre,
            &Selection::Newer,
            &current,
            false
        ));
    }

    /// A stable user must not be dragged onto the pre-release track, but a
    /// pre-release user must see their own track without passing --pre.
    #[test]
    fn prerelease_user_sees_prereleases_stable_user_does_not() {
        let releases = [release("v0.1.0-alpha.5", true), release("v0.1.0", false)];
        let pre_user = Version::parse("0.1.0-alpha.4").unwrap();
        let newer = select_releases(&releases, &Selection::Newer, &pre_user, false);
        let tags: Vec<&str> = newer.iter().map(|r| r.tag_name.as_str()).collect();
        // 0.1.0 (stable) is newer than 0.1.0-alpha.4, and so is alpha.5.
        assert_eq!(tags, ["v0.1.0-alpha.5", "v0.1.0"]);
    }

    /// Tags that are not valid SemVer are ignored, not guessed at.
    #[test]
    fn non_semver_tags_are_skipped() {
        let releases = [release("nightly", false), release("v0.2.0", false)];
        let current = Version::parse("0.1.0").unwrap();
        let newer = select_releases(&releases, &Selection::Newer, &current, false);
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].tag_name, "v0.2.0");
    }

    /// The real GitHub shape parses, including a release without notes.
    #[tokio::test]
    async fn fetch_parses_the_github_shape() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/releases"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "tag_name": "v0.1.0-alpha.4",
                        "prerelease": true,
                        "body": "### Fixed\n- login: …",
                        "published_at": "2026-07-14T06:00:00Z",
                        "assets": [],
                    },
                    { "tag_name": "v0.1.0-alpha.3", "prerelease": true, "body": null },
                ])),
            )
            .mount(&server)
            .await;

        let releases = fetch_releases(&format!("{}/releases", server.uri()))
            .await
            .unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].tag_name, "v0.1.0-alpha.4");
        assert!(releases[0].body.as_deref().unwrap().contains("login"));
        // Missing/absent fields must not break the parse.
        assert!(releases[1].body.is_none());
        assert!(releases[1].published_at.is_none());
    }

    /// A 403 (the usual symptom of the unauthenticated rate limit) must name
    /// the limit — and the command must survive it (fail-soft).
    #[tokio::test]
    async fn rate_limit_names_the_limit_and_never_aborts() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let url = format!("{}/releases", server.uri());
        let error = fetch_releases(&url).await.unwrap_err().to_string();
        assert!(error.contains("60 unauthenticated requests"), "{error}");

        // The command swallows it: a notice, exit Ok.
        let (_dir, ctx) = test_ctx();
        assert!(
            run(
                &ctx,
                Mode::Check { changelog: false },
                Selection::Newer,
                false,
                &url,
                REPO_API,
            )
            .await
            .is_ok()
        );
    }

    /// An unreachable host is a notice, not an error — `self check` must never
    /// break a script.
    #[tokio::test]
    async fn network_failure_is_not_fatal() {
        let (_dir, ctx) = test_ctx();
        // Reserved TEST-NET-1 address: no route, fails fast.
        let url = "http://192.0.2.1:9/releases";
        assert!(
            run(
                &ctx,
                Mode::Check { changelog: false },
                Selection::Newer,
                false,
                url,
                REPO_API,
            )
            .await
            .is_ok()
        );
    }

    /// `--since` accepts a version with or without the leading `v`, and a
    /// typo fails closed (a bad baseline must never be read as "nothing new").
    #[test]
    fn since_parses_with_and_without_v_and_rejects_garbage() {
        assert_eq!(
            parse_version_arg("0.1.0-alpha.2").unwrap(),
            Version::parse("0.1.0-alpha.2").unwrap()
        );
        assert_eq!(
            parse_version_arg("v0.1.0-alpha.2").unwrap(),
            Version::parse("0.1.0-alpha.2").unwrap()
        );
        assert_eq!(
            parse_version_arg("  v1.2.3  ").unwrap(),
            Version::parse("1.2.3").unwrap()
        );
        let error = parse_version_arg("banana").unwrap_err().to_string();
        assert!(error.contains("not a valid version"), "{error}");
        // A bare tag name without the patch level is not SemVer — say so
        // rather than guessing.
        assert!(parse_version_arg("v0.1").is_err());
    }

    /// A `--since` baseline older than the running version yields the
    /// releases after it — the whole point of the flag.
    #[test]
    fn since_baseline_overrides_the_running_version() {
        let releases = [
            release("v0.3.0", false),
            release("v0.2.0", false),
            release("v0.1.0", false),
        ];
        let running = Version::parse("0.1.0").unwrap();
        let selection = Selection::From(parse_version_arg("v0.1.0").unwrap());
        let selected = select_releases(&releases, &selection, &running, false);
        let tags: Vec<&str> = selected.iter().map(|r| r.tag_name.as_str()).collect();
        // Inclusive: the named release itself is part of the listing.
        assert_eq!(tags, ["v0.1.0", "v0.2.0", "v0.3.0"]);
    }

    /// The four selections of `self changelog`: the whole document by
    /// default, `--current` exactly the installed release, `--newer` strictly
    /// after it, `--since` inclusive of the named one. Each flag contains
    /// what its name says — that is the contract users rely on.
    #[test]
    fn the_four_selections_differ_exactly_where_they_should() {
        let releases = [
            release("v0.3.0", false),
            release("v0.2.0", false),
            release("v0.1.0", false),
        ];
        let running = Version::parse("0.2.0").unwrap();
        let tags = |selection: Selection| -> Vec<String> {
            select_releases(&releases, &selection, &running, false)
                .iter()
                .map(|r| r.tag_name.clone())
                .collect()
        };

        // Default: the whole changelog, oldest first.
        assert_eq!(tags(Selection::All), ["v0.1.0", "v0.2.0", "v0.3.0"]);
        // --current: exactly the installed release — nothing else.
        assert_eq!(tags(Selection::Installed), ["v0.2.0"]);
        // --newer: strictly after the installed 0.2.0 (excludes it).
        assert_eq!(tags(Selection::Newer), ["v0.3.0"]);
        // --since 0.2.0: includes 0.2.0 itself.
        let since = Selection::From(Version::parse("0.2.0").unwrap());
        assert_eq!(tags(since), ["v0.2.0", "v0.3.0"]);
        // --since reaches the very first release — impossible when exclusive.
        let first = Selection::From(Version::parse("0.1.0").unwrap());
        assert_eq!(tags(first), ["v0.1.0", "v0.2.0", "v0.3.0"]);
    }

    /// Naming a pre-release with --since pulls pre-releases into view even
    /// for a user on a stable build — they asked for that track explicitly.
    #[test]
    fn since_a_prerelease_shows_prereleases() {
        let releases = [release("v0.2.0-alpha.1", true), release("v0.1.0", false)];
        let stable_user = Version::parse("0.1.0").unwrap();
        let selection = Selection::From(Version::parse("0.2.0-alpha.1").unwrap());
        let selected = select_releases(&releases, &selection, &stable_user, false);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].tag_name, "v0.2.0-alpha.1");
    }

    fn commit(subject: &str) -> CompareCommit {
        CompareCommit {
            commit: CommitDetail {
                message: format!("{subject}\n\nA body that is not changelog material."),
            },
        }
    }

    /// The commit filter must mirror cliff.toml exactly, or `--unreleased`
    /// would promise entries a release never delivers (and vice versa).
    #[test]
    fn commit_filter_mirrors_cliff_toml() {
        // User-visible types, with their changelog groups.
        assert_eq!(changelog_group("feat(library): add"), Some("Added"));
        assert_eq!(changelog_group("fix(login): detect"), Some("Fixed"));
        assert_eq!(changelog_group("perf(db): faster"), Some("Performance"));
        assert_eq!(changelog_group("revert: bad idea"), Some("Reverted"));
        // fix(security) must win over the plain `fix` rule — order matters.
        assert_eq!(changelog_group("fix(security): leak"), Some("Security"));
        assert_eq!(changelog_group("security: harden"), Some("Security"));

        // Developer-facing types are filtered out, exactly like the release.
        for skipped in [
            "ci: bump actions",
            "chore: tidy",
            "docs(readme): fix typo",
            "style: rustfmt",
            "test(plugins): serialize",
            "refactor(db): split",
            "build: deps",
            // The release workflow's own bump commit is never an entry.
            "release: v0.1.0-alpha.4",
            "some stray subject",
        ] {
            assert_eq!(changelog_group(skipped), None, "{skipped} must be filtered");
        }
    }

    /// Grouping: changelog order, subjects only (no bodies), noise dropped.
    #[test]
    fn unreleased_entries_group_and_filter() {
        let comparison = Comparison {
            commits: vec![
                commit("chore: noise"),
                commit("feat(self): release awareness"),
                commit("fix(login): anti-automation page"),
                commit("release: v0.1.0-alpha.4"),
                commit("feat(library): content kinds"),
            ],
        };
        let entries = unreleased_entries(&comparison);
        let groups: Vec<&str> = entries.iter().map(|(group, _)| *group).collect();
        assert_eq!(groups, ["Added", "Fixed"], "changelog order, noise dropped");
        assert_eq!(entries[0].1.len(), 2, "both feat commits");
        assert_eq!(entries[1].1, ["fix(login): anti-automation page"]);
        // Only the subject line survives — bodies are not changelog material.
        assert!(!entries[0].1[0].contains("body"));
    }

    /// On the stable track the comparison starts at the newest stable
    /// release, so commits that already ship in a newer pre-release land
    /// under "unreleased". That pre-release must be named — it is installable
    /// with `--pre`, and staying silent would imply the changes are nowhere
    /// to be had. Tracking pre-releases skips nothing, so nothing is named.
    #[test]
    fn stable_track_names_the_prerelease_it_skips() {
        let releases = [
            release("v0.2.0-alpha.1", true),
            release("v0.1.0", false),
            release("v0.1.0-alpha.4", true),
        ];
        let stable = Version::parse("0.1.0").unwrap();

        // Stable track: the base is v0.1.0, and v0.2.0-alpha.1 sits above it.
        assert!(!tracking_prereleases(
            &releases,
            &Selection::All,
            &stable,
            false
        ));
        assert_eq!(
            newest_skipped_prerelease(&releases, &stable, false).as_deref(),
            Some("v0.2.0-alpha.1")
        );
        // Older pre-releases are below the base and therefore not "skipped".
        let newer = Version::parse("0.2.0-alpha.1").unwrap();
        assert_eq!(newest_skipped_prerelease(&releases, &newer, false), None);
        // With --pre nothing is skipped: the pre-release *is* the base.
        assert_eq!(newest_skipped_prerelease(&releases, &stable, true), None);
    }

    /// A comparison with nothing user-visible yields no entries — the
    /// command then says "nothing unreleased" rather than printing a header.
    #[test]
    fn only_filtered_commits_means_nothing_unreleased() {
        let comparison = Comparison {
            commits: vec![commit("ci: cache"), commit("docs: readme")],
        };
        assert!(unreleased_entries(&comparison).is_empty());
    }
}
