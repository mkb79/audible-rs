//! `audible download` — download owned titles (M3). Targets the Adrm
//! aaxc path with resume support; aax is phasing out and not handled.
//! `--kind` selects artifacts (audio, chapter, pdf so far) so covers and
//! metadata can be fetched without the audiobook. Each fetched artifact
//! is recorded in the `downloads` table; granted licenses are stored in
//! the `licenses` table and re-used (the content URL is stable) until
//! they expire, so a repeat run needs no fresh licenserequest. The audio
//! key/iv lands in a small `<name>.voucher` sidecar next to the file.
//!
//! `--decrypt` (AUD-27) runs a lossless decrypt of the aaxc to a playable
//! m4b (aaxclean-cli or ffmpeg, subprocess) as the last step of each item's
//! job. Both are `kind = audio`, told apart by the `variant` column
//! (`original` aaxc vs `decrypted` m4b), so `db downloads list --kind audio`
//! shows both. `--keep-source` (default) keeps the aaxc; `--remove-source`
//! deletes it and drops the now-obsolete `(audio, original)` record.
//!
//! By default already-recorded artifacts are skipped (the database
//! record is authoritative — the file may have been decrypted and
//! deleted); `--force`/`defaults.overwrite = force` re-download, and
//! `--relicense` requests a fresh grant. Items are downloaded with a
//! bounded worker pool (`--jobs`, default 3); on an interactive stderr a
//! MultiProgress shows a summary line (item count + per-kind counters)
//! plus a byte bar per heavy (audio) transfer in flight.
//!
//! `--no-db-write` (AUD-101) is a quick-grab mode: the run writes nothing
//! to the database (no download records, no license persistence, no record
//! deletions) and applies no record-based skip — only the on-disk checks in
//! the target dir remain (resume, already-complete). It requires `--dir`,
//! which must lie outside the configured download_dir, so a throwaway run
//! can never touch the managed file tree. Database reads (`--title`,
//! `--missing`, naming, license reuse) stay available.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Arg, ArgAction};
use futures::StreamExt as _;
use indicatif::MultiProgress;

use crate::config::ctx::Ctx;
use crate::downloader::{
    DownloadError, DownloadOutcome, Quality, download_to_file, request_chapters, request_cover_url,
    request_license,
};
use crate::output::Output;

mod decrypt;
mod info;
mod orphans;
mod reorganize;
pub(crate) mod request_kind;
mod widevine;

use crate::naming::{base_filename, download_dir};
pub(crate) use reorganize::{hint_reorganize, key_affects_filenames};

/// `audible download`.
pub struct DownloadCommand;

#[async_trait::async_trait]
impl super::Command for DownloadCommand {
    fn name(&self) -> &'static str {
        "download"
    }

    fn clap(&self) -> clap::Command {
        let cmd = clap::Command::new(self.name())
            .about("Download owned titles (aaxc, with resume)")
            .long_about(format!(
                "Download owned titles (aaxc, chapters, cover, pdf), with resume. \
                 Streaming-only titles (no aaxc asset) automatically use the Widevine/DASH \
                 path when a CDM is configured (see `account widevine`); force it with \
                 --widevine.\n\n{}",
                crate::config::filename_template::help_text()
            ))
            .arg(
                Arg::new("kind")
                    .long("kind")
                    .value_name("KIND,...")
                    .default_value("audio")
                    .help(
                        "Artifacts: audio,chapter,pdf,cover or all (comma-separated)",
                    ),
            )
            .arg(
                Arg::new("quality")
                    .long("quality")
                    .value_name("QUALITY")
                    .value_parser(["high", "normal"])
                    .default_value("high")
                    .help("Audio quality"),
            )
            .arg(
                Arg::new("cover_size")
                    .long("cover-size")
                    .value_name("PX")
                    .help("Cover size(s) in px, comma-separated (overrides config)"),
            )
            .arg(
                Arg::new("chapter_type")
                    .long("chapter-type")
                    .value_name("TYPE,...")
                    .help("Chapter title layout(s): flat, tree or both, comma-separated (overrides config)"),
            )
            .arg(
                Arg::new("dir")
                    .long("dir")
                    .value_name("DIR")
                    .help("Download directory (overrides the configured download_dir)"),
            )
            .arg(
                Arg::new("license_only")
                    .long("license-only")
                    .action(ArgAction::SetTrue)
                    .help("Only request the license and print what was granted"),
            )
            .arg(
                Arg::new("force")
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("skip_existing")
                    .help("Re-download artifacts even if already recorded (overrides config)"),
            )
            .arg(
                Arg::new("skip_existing")
                    .long("skip-existing")
                    .action(ArgAction::SetTrue)
                    .help("Skip artifacts already recorded in the database (overrides config)"),
            )
            .arg(
                Arg::new("relicense")
                    .long("relicense")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("skip_existing")
                    .help("Request a fresh license instead of reusing the stored one (implies --force)"),
            )
            .arg(
                Arg::new("no_db_write")
                    .long("no-db-write")
                    .action(ArgAction::SetTrue)
                    .requires("dir")
                    .conflicts_with("skip_existing")
                    .help(
                        "Quick-grab mode: record nothing in the database and apply no \
                         record-based skip (files already in --dir still resume/skip); \
                         requires --dir outside the configured download_dir",
                    ),
            )
            .arg(
                Arg::new("jobs")
                    .long("jobs")
                    .short('j')
                    .value_name("N")
                    .value_parser(clap::value_parser!(u32).range(1..))
                    .default_value("3")
                    .help("Number of items to download concurrently"),
            )
            .arg(
                Arg::new("decrypt")
                    .long("decrypt")
                    .action(ArgAction::SetTrue)
                    .help("Decrypt the downloaded aaxc to a playable m4b (overrides config)"),
            )
            .arg(
                Arg::new("decrypt_backend")
                    .long("decrypt-backend")
                    .value_name("TOOL")
                    .value_parser(["auto", "aaxclean", "ffmpeg"])
                    .help("Decrypt tool (auto = aaxclean-cli preferred, ffmpeg fallback)"),
            )
            .arg(
                Arg::new("remove_source")
                    .long("remove-source")
                    .action(ArgAction::SetTrue)
                    .help("Delete the source aaxc after a successful decrypt (overrides config)"),
            )
            .arg(
                Arg::new("keep_source")
                    .long("keep-source")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("remove_source")
                    .help("Keep the source aaxc after decrypt (overrides config)"),
            )
            .arg(
                Arg::new("widevine")
                    .long("widevine")
                    .action(ArgAction::SetTrue)
                    .help(
                        "Force the Widevine/DASH path (needs a configured CDM + Android \
                         account). Streaming-only titles use it automatically.",
                    ),
            )
            .arg(
                Arg::new("codec")
                    .long("codec")
                    .value_name("CODEC")
                    .value_parser(["aac", "xhe"])
                    .default_value("aac")
                    .help("Widevine audio codec: aac (AAC-LC, universal) or xhe (xHE-AAC)"),
            )
            .arg(
                Arg::new("spatial")
                    .long("spatial")
                    .action(ArgAction::SetTrue)
                    .help("Request Dolby Atmos (Widevine; needs a Widevine L1 device)"),
            )
            .arg(
                Arg::new("no_spatial")
                    .long("no-spatial")
                    .action(ArgAction::SetTrue)
                    .conflicts_with("spatial")
                    .help("Never request Atmos, even for spatial titles"),
            )
            .subcommand(reorganize::reorganize_command())
            .subcommand(orphans::orphans_command())
            .subcommand(info::info_command());
        add_source_args(cmd)
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        if let Some(("reorganize", sub)) = matches.subcommand() {
            return reorganize::reorganize(ctx, sub).await;
        }
        if let Some(("orphans", sub)) = matches.subcommand() {
            return orphans::orphans(ctx, sub).await;
        }
        if let Some(("info", sub)) = matches.subcommand() {
            return info::info(ctx, sub).await;
        }
        // A source is required for the download action (the group is not
        // clap-`required` so the subcommands need none).
        if !matches.contains_id("asin")
            && !matches.contains_id("title")
            && !matches.get_flag("missing")
        {
            bail!("specify what to download: --asin <ASIN>, --title <QUERY>, or --missing");
        }
        // Not expressible via clap `requires`: SetTrue flags count as
        // present through their default, so the relation never fires.
        if matches.get_flag("include_archived") && !matches.get_flag("missing") {
            bail!("--include-archived only applies to the --missing selection");
        }
        let quality = match matches.get_one::<String>("quality").map(String::as_str) {
            Some("normal") => Quality::Normal,
            _ => Quality::High,
        };
        // Parsed early: the Widevine/DASH flags feed both the download plan
        // and the format-aware `--missing` selection (AUD-96), which uses the
        // same request_kind candidates as the per-item skip.
        let force_widevine = matches.get_flag("widevine");
        let codec_xhe = matches.get_one::<String>("codec").map(String::as_str) == Some("xhe");

        // Parsed up front: `--missing` resolves the item source from the
        // requested artifact kinds (items lacking any of them).
        let mut base_targets = parse_kinds(matches.get_one::<String>("kind").expect("default"))?;

        // Decrypt (AUD-27): the flag overrides the settings bundle. Enabling
        // it implies the audio artifact (we need the aaxc to decrypt).
        let decrypt_on = matches.get_flag("decrypt")
            || ctx.settings_view().map(|v| v.decrypt()).unwrap_or(false);
        if decrypt_on {
            base_targets.insert(Artifact::Audio);
        }
        let keep_source = if matches.get_flag("keep_source") {
            true
        } else if matches.get_flag("remove_source") {
            false
        } else {
            ctx.settings_view()
                .map(|v| v.decrypt_keep_source())
                .unwrap_or(true)
        };
        let decrypt_backend = match matches
            .get_one::<String>("decrypt_backend")
            .map(String::as_str)
        {
            Some("aaxclean") => crate::config::schema::DecryptBackend::Aaxclean,
            Some("ffmpeg") => crate::config::schema::DecryptBackend::Ffmpeg,
            Some("auto") => crate::config::schema::DecryptBackend::Auto,
            _ => ctx
                .settings_view()
                .map(|v| v.decrypt_backend())
                .unwrap_or_default(),
        };

        let client = ctx.client().await?;
        // `download` operates on a single marketplace (one host per
        // request); `-m` must resolve to exactly one.
        let marketplace = ctx.marketplace_single()?;
        let asins = resolve_source(
            ctx,
            &marketplace,
            matches,
            &base_targets,
            request_kind::candidates(force_widevine, codec_xhe, quality),
        )
        .await?;
        if asins.is_empty() {
            eprintln!("no items to download");
            return Ok(());
        }

        // `--license-only` is a probe: always request a fresh license so
        // the user sees the current grant, never a cached sidecar.
        if matches.get_flag("license_only") {
            for asin in &asins {
                let license = request_license(client, &marketplace, asin, quality).await?;
                print_license(ctx, client, &license)?;
            }
            return Ok(());
        }

        let dir = match matches.get_one::<String>("dir") {
            Some(dir) => PathBuf::from(dir),
            None => download_dir(ctx)?,
        };

        // `--no-db-write` (AUD-101): the quick-grab dir must not touch the
        // managed file tree — checked before create_dir_all so a rejected
        // run does not even leave an empty directory behind (clap's
        // `requires` guarantees an explicit --dir).
        let no_db_write = matches.get_flag("no_db_write");
        if no_db_write {
            ensure_outside_download_dir(&dir, &download_dir(ctx)?)?;
        }

        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;

        let cover_sizes = if base_targets.contains(&Artifact::Cover) {
            parse_cover_sizes(&resolve_cover_sizes(ctx, matches))?
        } else {
            Vec::new()
        };
        let chapter_types = if base_targets.contains(&Artifact::Chapter) {
            parse_chapter_types(&resolve_chapter_types(ctx, matches))?
        } else {
            Vec::new()
        };

        // Progress is shown only on an interactive stderr and not under
        // --quiet. The MultiProgress hosts the summary line plus a byte bar
        // per heavy (audio) transfer in flight.
        let show_progress = !matches.get_flag("quiet") && console::Term::stderr().is_term();
        let multi = show_progress.then(MultiProgress::new);

        // Resolve the decrypt tool up front so a missing/too-old tool fails
        // before anything is downloaded.
        let decryptor = if decrypt_on {
            Some(decrypt::select(decrypt_backend).await?)
        } else {
            None
        };

        // Widevine/DASH path (AUD-56e): `--widevine` forces it; otherwise it is
        // an automatic fallback for streaming-only titles with no aaxc asset.
        // The CDM is loaded once and shared; absent = only aaxc titles work.
        let spatial = matches.get_flag("spatial");
        let cdm = widevine::load_cdm(ctx).ok();
        if force_widevine && cdm.is_none() {
            // Surface the real reason (no CDM configured) instead of per-item.
            widevine::load_cdm(ctx)?;
        }

        let plan = DownloadPlan {
            quality,
            marketplace: &marketplace,
            base_targets: &base_targets,
            dir: &dir,
            cover_sizes: &cover_sizes,
            chapter_types: &chapter_types,
            relicense: matches.get_flag("relicense"),
            force: overwrite_policy(ctx, matches) == crate::config::schema::OverwritePolicy::Force,
            no_db_write,
            decrypt: decryptor.as_ref(),
            keep_source,
            widevine: force_widevine,
            codec_xhe,
            spatial,
            cdm: cdm.as_ref(),
            mp: multi.as_ref(),
        };

        let jobs = *matches.get_one::<u32>("jobs").expect("default") as usize;
        let total = asins.len();
        let single = total == 1;

        let mut counters = Counters::default();
        // A persistent summary line (item count + per-kind counters) above
        // the per-transfer bars.
        let summary = multi.as_ref().map(|m| {
            let bar = m.add(indicatif::ProgressBar::new_spinner());
            bar.set_style(
                indicatif::ProgressStyle::with_template("{msg}").expect("valid template"),
            );
            bar.set_message(summary_line(&counters, total));
            bar
        });

        // Bounded worker pool over the items: up to `jobs` downloads in
        // flight (no explicit queue — the work set is known up front). Each
        // future yields its result so the consumer can aggregate and refresh
        // the summary as items complete.
        let plan_ref = &plan;
        let mut stream = futures::stream::iter(asins)
            .map(|asin| async move {
                let result = download_one(ctx, client, &asin, plan_ref).await;
                (asin, result)
            })
            .buffer_unordered(jobs);

        let mut rows: Vec<Vec<String>> = Vec::new();
        while let Some((asin, result)) = stream.next().await {
            match result {
                Ok(written) => {
                    for (kind, path) in written {
                        counters.bump(&kind);
                        rows.push(vec![asin.clone(), kind, path]);
                    }
                }
                // A single explicit target keeps the old behavior: propagate.
                Err(error) if single => return Err(error),
                // In a batch one bad item must not abort the rest; warn
                // (without clobbering the bars) and fail at the end.
                Err(error) => {
                    let line = format!("error for {asin}: {error:#}");
                    match multi.as_ref() {
                        Some(m) => {
                            let _ = m.println(line);
                        }
                        None => eprintln!("{line}"),
                    }
                    counters.failed += 1;
                }
            }
            counters.items_done += 1;
            if let Some(bar) = &summary {
                bar.set_message(summary_line(&counters, total));
            }
        }
        drop(stream);
        if let Some(bar) = &summary {
            bar.finish_and_clear();
        }

        // Deterministic table order regardless of completion order.
        rows.sort();
        if !rows.is_empty() {
            ctx.print(&Output::table(vec!["asin", "artifact", "path"], rows));
        } else if counters.failed == 0 {
            eprintln!("nothing downloaded");
        }
        if counters.failed > 0 {
            bail!("{} of {total} item(s) failed", counters.failed);
        }
        Ok(())
    }
}

mod artifacts;
mod item;
mod license;

use item::{Counters, DownloadPlan, download_one, summary_line};
use license::print_license;

/// Adds the item-source flags: the shared `--asin`/`--title` pair plus
/// `--missing` (at least one required; `--missing` is exclusive).
fn add_source_args(cmd: clap::Command) -> clap::Command {
    crate::commands::items::item_source_args(cmd)
        .arg(
            Arg::new("missing")
                .long("missing")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["asin", "title"])
                .help(
                    "Download every owned item in the marketplace that is missing the \
                     requested --kind artifact(s); audio counts as missing unless a \
                     format the current flags resolve to is already downloaded",
                ),
        )
        .arg(
            Arg::new("include_archived")
                .long("include-archived")
                .action(ArgAction::SetTrue)
                .help(
                    "Also fetch archived titles — --missing skips them by default \
                     (archive state is as of the last library sync)",
                ),
        )
        .group(
            // Not `required`: the `reorganize`/`orphans` subcommands need no
            // source. The download path enforces a source manually (see `run`).
            clap::ArgGroup::new("source")
                .args(["asin", "title", "missing"])
                .multiple(true),
        )
}

/// Resolves the item source to a list of ASINs: `--asin`/`--title` go
/// through the shared resolver (title search + interactive pick), and
/// `--missing` queries the library for items lacking any of the requested
/// `kinds` — audio counts as present only in one of the run's
/// `audio_request_kinds` (format-aware, AUD-96).
async fn resolve_source(
    ctx: &Ctx,
    marketplace: &str,
    matches: &clap::ArgMatches,
    kinds: &BTreeSet<Artifact>,
    audio_request_kinds: Vec<String>,
) -> Result<Vec<String>> {
    let asins: Vec<String> = matches
        .get_many::<String>("asin")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();

    // Scope the database handle so it is dropped before the download
    // helpers open their own connections.
    let db = ctx.open_library_db().await?;
    if matches.get_flag("missing") {
        // The selection is DB-based, so honor the auto-sync policy like
        // `library list --missing` does — this is also what keeps the
        // archive filter (below) from working on a stale view (AUD-110).
        crate::commands::library::maybe_auto_sync(ctx, &db).await?;
        let kind_values: Vec<String> = kinds.iter().map(|kind| kind.kind().to_owned()).collect();
        let include_archived = matches.get_flag("include_archived");
        let asins = db
            .missing_download_asins(
                vec![marketplace.to_owned()],
                kind_values.clone(),
                audio_request_kinds.clone(),
                include_archived,
            )
            .await?;
        if !include_archived {
            let unfiltered = db
                .missing_download_asins(
                    vec![marketplace.to_owned()],
                    kind_values,
                    audio_request_kinds,
                    true,
                )
                .await?;
            let skipped = unfiltered.len().saturating_sub(asins.len());
            if skipped > 0 {
                eprintln!(
                    "skipped {skipped} archived title(s) (as of the last library sync); \
                     use --include-archived to fetch them"
                );
            }
        }
        return Ok(asins);
    }
    let titles: Vec<String> = matches
        .get_many::<String>("title")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    crate::commands::items::resolve_asins(&db, marketplace, asins, titles).await
}

/// A `--kind` artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Artifact {
    Audio,
    Chapter,
    Pdf,
    Cover,
}

impl Artifact {
    /// Every currently implemented artifact (the `all` keyword).
    const ALL: [Artifact; 4] = [
        Artifact::Audio,
        Artifact::Chapter,
        Artifact::Pdf,
        Artifact::Cover,
    ];

    /// The `downloads.kind` value this artifact is tracked under.
    fn kind(self) -> &'static str {
        match self {
            Artifact::Audio => "audio",
            Artifact::Chapter => "chapter",
            Artifact::Pdf => "pdf",
            Artifact::Cover => "cover",
        }
    }
}

/// Common cover sizes (pixels) shown as a hint. Not exhaustive — Amazon's
/// image CDN resizes to any requested size (e.g. 1242 seen in catalog
/// traffic), so any positive number is accepted.
const COVER_SIZES_HINT: &str = "252, 315, 360, 408, 500, 558, 570, 882, 900, 1215";

/// Parses a comma-separated cover-size list (any positive pixel value),
/// de-duplicating. A size the catalog has no image for is reported later.
fn parse_cover_sizes(sizes: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for size in sizes.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if size.parse::<u32>().is_ok_and(|n| n > 0) {
            if !out.iter().any(|s| s == size) {
                out.push(size.to_owned());
            }
        } else {
            bail!("invalid cover size {size:?} (a positive number of px, e.g. {COVER_SIZES_HINT})");
        }
    }
    if out.is_empty() {
        bail!("--cover-size selected nothing");
    }
    Ok(out)
}

/// Splits a CLI CSV value (`--cover-size 500,900`) into trimmed items.
fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolves the cover size(s): `--cover-size` (CSV) else the settings bundle.
fn resolve_cover_sizes(ctx: &Ctx, matches: &clap::ArgMatches) -> Vec<String> {
    if let Some(value) = matches.get_one::<String>("cover_size") {
        return split_csv(value);
    }
    ctx.settings_view()
        .map(|view| view.cover_size(None, None))
        .unwrap_or_else(|_| vec!["500".to_owned()])
}

/// Resolves the chapter layout(s): `--chapter-type` (CSV) else the
/// settings bundle.
fn resolve_chapter_types(ctx: &Ctx, matches: &clap::ArgMatches) -> Vec<String> {
    if let Some(value) = matches.get_one::<String>("chapter_type") {
        return split_csv(value);
    }
    ctx.settings_view()
        .map(|view| view.chapter_type(None, None))
        .unwrap_or_else(|_| vec!["tree".to_owned()])
}

/// Parses a chapter-layout list, de-duplicating.
fn parse_chapter_types(spec: &[String]) -> Result<Vec<crate::config::schema::ChapterType>> {
    use crate::config::schema::ChapterType;
    let mut types: Vec<ChapterType> = Vec::new();
    for item in spec.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let chapter_type = match item.to_ascii_lowercase().as_str() {
            "flat" => ChapterType::Flat,
            "tree" => ChapterType::Tree,
            other => bail!("unknown chapter type {other:?} (flat, tree)"),
        };
        if !types.contains(&chapter_type) {
            types.push(chapter_type);
        }
    }
    if types.is_empty() {
        bail!("--chapter-type selected nothing");
    }
    Ok(types)
}

/// Resolves the effective overwrite policy: explicit flags win, then the
/// profile/defaults config, then the built-in `Skip`. `--relicense`
/// re-downloads, so it implies `Force`.
fn overwrite_policy(
    ctx: &Ctx,
    matches: &clap::ArgMatches,
) -> crate::config::schema::OverwritePolicy {
    use crate::config::schema::OverwritePolicy;
    if matches.get_flag("force") || matches.get_flag("relicense") {
        return OverwritePolicy::Force;
    }
    if matches.get_flag("skip_existing") {
        return OverwritePolicy::Skip;
    }
    ctx.settings_view()
        .map(|view| view.overwrite(None, None))
        .unwrap_or(OverwritePolicy::Skip)
}

/// Enforces the `--no-db-write` isolation promise for `--dir`: the target
/// must not be the configured `download_dir` nor live inside it, so a
/// quick-grab run can never touch the managed file tree. Runs before the
/// target is created; a `download_dir` that does not resolve cannot
/// collide.
fn ensure_outside_download_dir(dir: &Path, download_dir: &Path) -> Result<()> {
    let Ok(managed) = download_dir.canonicalize() else {
        return Ok(());
    };
    // `starts_with` compares whole components and matches the path itself,
    // so equality and any nesting depth are both caught.
    if resolve_for_containment(dir)?.starts_with(&managed) {
        bail!(
            "--no-db-write requires a --dir outside the configured download_dir ({})",
            managed.display()
        );
    }
    Ok(())
}

/// Canonicalizes `path` without requiring it to exist yet: the longest
/// existing ancestor resolves symlinks, the not-yet-created remainder is
/// appended verbatim. A `..` inside the nonexistent remainder stays
/// unresolved — the containment check then fails closed.
fn resolve_for_containment(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("could not resolve {}", path.display()))?;
    let mut existing = absolute.as_path();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(base) = existing.canonicalize() {
            return Ok(rest.iter().rev().fold(base, |p, c| p.join(c)));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            // No existing ancestor at all (e.g. a dangling root on
            // Windows) — use the lexical absolute path as-is.
            _ => return Ok(absolute),
        }
    }
}

fn parse_kinds(list: &str) -> Result<BTreeSet<Artifact>> {
    let mut targets = BTreeSet::new();
    for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item {
            "all" => targets.extend(Artifact::ALL),
            "audio" => {
                targets.insert(Artifact::Audio);
            }
            "chapter" => {
                targets.insert(Artifact::Chapter);
            }
            "pdf" => {
                targets.insert(Artifact::Pdf);
            }
            "cover" => {
                targets.insert(Artifact::Cover);
            }
            other => {
                bail!("unknown --kind value {other:?} (audio, chapter, pdf, cover, all)")
            }
        }
    }
    if targets.is_empty() {
        bail!("--kind selected nothing");
    }
    Ok(targets)
}

/// Re-license when a stored license's signed URL is within this many seconds of
/// (or past) its expiry — headroom so a large audiobook download does not race
/// the URL's expiry mid-transfer.
const LICENSE_URL_EXPIRY_MARGIN_SECS: i64 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_all_expands_to_every_artifact() {
        assert_eq!(
            parse_kinds("all").unwrap(),
            Artifact::ALL.into_iter().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn missing_is_exclusive_with_asin_and_title() {
        use crate::commands::Command as _;
        let parse = |args: &[&str]| DownloadCommand.clap().try_get_matches_from(args);
        // `--missing` is a valid source on its own.
        assert!(parse(&["download", "--missing"]).is_ok());
        // But not combined with `--asin` or `--title`.
        assert!(parse(&["download", "--missing", "--asin", "B0ASIN"]).is_err());
        assert!(parse(&["download", "--missing", "--title", "foo"]).is_err());
        // A source is no longer clap-`required` (the `reorganize`/`orphans`
        // subcommands need none); the download path enforces it in `run`.
        assert!(parse(&["download"]).is_ok());
        assert!(parse(&["download", "reorganize", "--dry-run"]).is_ok());
        assert!(parse(&["download", "orphans", "--remove", "--yes"]).is_ok());
        assert!(parse(&["download", "info", "--asin", "B0A"]).is_ok());
        // --include-archived only makes sense for the --missing selection;
        // clap `requires` cannot express that against SetTrue flags
        // (defaults count as present), so `run` enforces it like the
        // source requirement above.
        assert!(parse(&["download", "--missing", "--include-archived"]).is_ok());
        // `--asin` and `--title` together remain allowed.
        assert!(parse(&["download", "--asin", "B0ASIN", "--title", "foo"]).is_ok());
    }

    #[test]
    fn no_db_write_requires_dir_and_conflicts_with_skip_existing() {
        use crate::commands::Command as _;
        let parse = |args: &[&str]| DownloadCommand.clap().try_get_matches_from(args);
        // --dir is mandatory with --no-db-write.
        assert!(parse(&["download", "--asin", "B0", "--no-db-write"]).is_err());
        assert!(
            parse(&[
                "download",
                "--asin",
                "B0",
                "--no-db-write",
                "--dir",
                "/tmp/x"
            ])
            .is_ok()
        );
        // Forcing the record-based skip contradicts the flag.
        assert!(
            parse(&[
                "download",
                "--asin",
                "B0",
                "--no-db-write",
                "--dir",
                "/tmp/x",
                "--skip-existing",
            ])
            .is_err()
        );
        // --force / --relicense / --missing / --decrypt stay combinable.
        for extra in ["--force", "--relicense", "--decrypt"] {
            assert!(
                parse(&[
                    "download",
                    "--asin",
                    "B0",
                    "--no-db-write",
                    "--dir",
                    "/tmp/x",
                    extra,
                ])
                .is_ok()
            );
        }
        assert!(parse(&["download", "--missing", "--no-db-write", "--dir", "/tmp/x"]).is_ok());
    }

    #[test]
    fn no_db_write_dir_must_lie_outside_the_download_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let managed = tmp.path().join("downloads");
        let inside = managed.join("grab");
        let sibling = tmp.path().join("grab");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        // The download_dir itself and anything nested inside are rejected —
        // including a target that does not exist yet (the check runs before
        // create_dir_all).
        assert!(ensure_outside_download_dir(&managed, &managed).is_err());
        assert!(ensure_outside_download_dir(&inside, &managed).is_err());
        assert!(ensure_outside_download_dir(&managed.join("not-yet-created"), &managed).is_err());
        // A sibling passes (existing or not), as does a nonexistent
        // download_dir (no collision possible).
        assert!(ensure_outside_download_dir(&sibling, &managed).is_ok());
        assert!(ensure_outside_download_dir(&tmp.path().join("nope"), &managed).is_ok());
        assert!(ensure_outside_download_dir(&sibling, &tmp.path().join("missing")).is_ok());
    }

    #[test]
    fn jobs_defaults_to_three_and_rejects_zero() {
        use crate::commands::Command as _;
        let parse = |args: &[&str]| DownloadCommand.clap().try_get_matches_from(args);
        assert_eq!(
            *parse(&["download", "--asin", "B0"])
                .unwrap()
                .get_one::<u32>("jobs")
                .unwrap(),
            3
        );
        assert_eq!(
            *parse(&["download", "--asin", "B0", "-j", "8"])
                .unwrap()
                .get_one::<u32>("jobs")
                .unwrap(),
            8
        );
        assert!(parse(&["download", "--asin", "B0", "--jobs", "0"]).is_err());
    }

    #[test]
    fn get_parses_and_dedups() {
        assert_eq!(
            parse_kinds("audio,pdf,audio").unwrap(),
            BTreeSet::from([Artifact::Audio, Artifact::Pdf])
        );
        assert_eq!(
            parse_kinds("cover").unwrap(),
            BTreeSet::from([Artifact::Cover])
        );
        assert!(parse_kinds("nonsense").is_err());
    }

    #[test]
    fn chapter_types_parse_and_dedup() {
        use crate::config::schema::ChapterType;
        let v = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parse_chapter_types(&v(&["flat", "tree", "flat"])).unwrap(),
            vec![ChapterType::Flat, ChapterType::Tree]
        );
        assert_eq!(
            parse_chapter_types(&v(&["TREE"])).unwrap(),
            vec![ChapterType::Tree]
        );
        assert!(parse_chapter_types(&v(&["both"])).is_err());
        assert!(parse_chapter_types(&[]).is_err());
    }

    #[test]
    fn cover_sizes_validate_and_dedup() {
        let v = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            parse_cover_sizes(&v(&["500", "1215", "500"])).unwrap(),
            vec!["500".to_owned(), "1215".to_owned()]
        );
        // Any positive integer is accepted (the CDN resizes), e.g. 1242.
        assert_eq!(
            parse_cover_sizes(&v(&["1242"])).unwrap(),
            vec!["1242".to_owned()]
        );
        assert!(parse_cover_sizes(&v(&["0"])).is_err());
        assert!(parse_cover_sizes(&v(&["abc"])).is_err());
        assert!(parse_cover_sizes(&[]).is_err());
    }
}
