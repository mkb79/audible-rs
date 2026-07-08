//! `download reorganize` — relocate recorded files (and their sidecars) to a
//! new naming scheme and keep the database in sync (AUD-54). The engine that
//! computes the paths lives in [`crate::naming`].

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::{Arg, ArgAction};

use crate::commands::prompt::confirm;
use crate::config::ctx::Ctx;
use crate::naming::{artifact_suffix, download_dir, expand_tilde, resolve_base, template_context};

fn filename_mode_from_str(value: &str) -> crate::config::schema::FilenameMode {
    use crate::config::schema::FilenameMode;
    match value {
        "unicode" => FilenameMode::Unicode,
        "asin_ascii" => FilenameMode::AsinAscii,
        "asin_unicode" => FilenameMode::AsinUnicode,
        "custom" => FilenameMode::Custom,
        _ => FilenameMode::Ascii,
    }
}

/// `download reorganize` — clap.
pub(super) fn reorganize_command() -> clap::Command {
    clap::Command::new("reorganize")
        .about("Move already-downloaded files to match a filename scheme")
        .long_about(
            "Relocate downloaded files (and their .voucher/.wvkey/.annot sidecars) to match the \
             current — or a newly given — naming scheme, and update the database. Old file \
             locations come from the database, so this is safe across earlier setting changes.\n\n\
             With no --filename-*/--download-dir flags it migrates to the current config. Those \
             flags are also persisted to the selected -s settings bundle (--filename-template \
             implies custom mode; switching to a fixed mode drops the template).",
        )
        .arg(
            Arg::new("filename_mode")
                .long("filename-mode")
                .value_name("MODE")
                .value_parser(["ascii", "unicode", "asin_ascii", "asin_unicode", "custom"])
                .help("Also set filename_mode on the -s bundle"),
        )
        .arg(
            Arg::new("filename_template")
                .long("filename-template")
                .value_name("TEMPLATE")
                .help("Also set filename_template (implies --filename-mode custom)"),
        )
        .arg(
            Arg::new("download_dir")
                .long("download-dir")
                .value_name("DIR")
                .help("Also set download_dir and relocate files there"),
        )
        .arg(
            Arg::new("dry_run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help("Print the planned moves without changing anything"),
        )
        .arg(
            Arg::new("copy")
                .long("copy")
                .action(ArgAction::SetTrue)
                .help("Copy instead of move (keep old files as a backup; doubles disk use)"),
        )
        .arg(crate::commands::yes_arg())
}

/// How a planned relocation updates the database afterwards.
enum ReorgUpdate {
    Download {
        asin: String,
        marketplace: String,
        kind: String,
        content_format: String,
        variant: String,
    },
    Annotation {
        asin: String,
        marketplace: String,
    },
}

/// One planned relocation: the file (+ its optional key sidecar) and the row.
struct PlannedMove {
    old: PathBuf,
    new: PathBuf,
    /// The `(old, new)` paths of the file's key sidecar (`.voucher`/`.wvkey`),
    /// moved alongside it so it never orphans (AUD-99).
    sidecar: Option<(PathBuf, PathBuf)>,
    update: ReorgUpdate,
}

/// `download reorganize`: relocate recorded files to the target naming.
pub(super) async fn reorganize(ctx: &Ctx, args: &clap::ArgMatches) -> Result<()> {
    use crate::config::schema::FilenameMode;
    let dry_run = args.get_flag("dry_run");
    let copy = args.get_flag("copy");
    let yes = args.get_flag("yes");

    // Current naming (owned, so the config borrow is released before the loop).
    let (cur_mode, cur_template, max_len) = {
        let view = ctx.settings_view()?;
        (
            view.filename_mode(None, None),
            view.filename_template(),
            view.filename_max_length(None, None),
        )
    };

    // Resolve the target naming from the flags + settings to persist.
    let mode_flag = args.get_one::<String>("filename_mode").map(String::as_str);
    let template_flag = args.get_one::<String>("filename_template").cloned();
    let bundle = ctx.settings_name()?;
    let key = |field: &str| format!("settings.{bundle}.{field}");
    let mut sets: Vec<(String, String)> = Vec::new();
    let mut unsets: Vec<String> = Vec::new();
    let (target_mode, target_template): (FilenameMode, Option<String>) =
        match (mode_flag, template_flag) {
            (Some("custom"), None) => {
                anyhow::bail!("--filename-mode custom requires --filename-template")
            }
            (None, None) => (cur_mode, cur_template),
            (None, Some(template)) | (Some("custom"), Some(template)) => {
                sets.push((key("filename_mode"), "custom".to_owned()));
                sets.push((key("filename_template"), template.clone()));
                (FilenameMode::Custom, Some(template))
            }
            (Some(mode), None) => {
                // Switch to a fixed mode: set it and drop a stale template so
                // it can't mislead later — but only when the bundle itself has
                // one (`unset` errors on a missing key; an inherited template
                // in `settings.default` is not this bundle's to remove).
                sets.push((key("filename_mode"), mode.to_owned()));
                if ctx
                    .config()
                    .settings
                    .get(&bundle)
                    .is_some_and(|settings| settings.filename_template.is_some())
                {
                    unsets.push(key("filename_template"));
                }
                (filename_mode_from_str(mode), None)
            }
            (Some(_), Some(_)) => anyhow::bail!(
                "--filename-template is only valid with --filename-mode custom \
                 (or omit --filename-mode)"
            ),
        };
    if let Some(dir) = args.get_one::<String>("download_dir") {
        sets.push((key("download_dir"), dir.clone()));
    }
    // The currently configured directory doubles as the bound for cleaning up
    // emptied folders after the moves (files live under it, not the target).
    let current_dir = download_dir(ctx)?;
    let target_dir = match args.get_one::<String>("download_dir") {
        Some(dir) => expand_tilde(Path::new(dir)),
        None => current_dir.clone(),
    };

    // Build the plan: for each recorded file, old = stored path, new = target.
    let marketplaces = ctx.marketplaces()?;
    let db = ctx.open_library_db().await?;
    let mut planned: Vec<PlannedMove> = Vec::new();
    let mut skipped = 0usize;
    for marketplace in &marketplaces {
        for entry in db.reorg_downloads(marketplace.clone()).await? {
            let Some(values) = template_context(ctx, marketplace, &entry.asin).await else {
                skipped += 1;
                continue;
            };
            let base = resolve_base(
                target_mode,
                target_template.as_deref(),
                max_len,
                &entry.asin,
                &values,
            )?;
            let old = PathBuf::from(&entry.file_path);
            let ext = old.extension().and_then(|e| e.to_str()).unwrap_or("");
            let new = target_dir.join(format!(
                "{base}{}",
                artifact_suffix(&entry.kind, &entry.content_format, ext)
            ));
            if old == new {
                continue;
            }
            // An original audio file carries a key sidecar keyed by its
            // extension: `.aaxc` → `.voucher`, `.cenc` → `.wvkey` (AUD-99). old
            // and new share the extension, so both resolve or neither does.
            let sidecar = (entry.kind == "audio" && entry.variant == "original")
                .then(|| crate::naming::sidecar_path(&old).zip(crate::naming::sidecar_path(&new)))
                .flatten();
            planned.push(PlannedMove {
                old,
                new,
                sidecar,
                update: ReorgUpdate::Download {
                    asin: entry.asin,
                    marketplace: marketplace.clone(),
                    kind: entry.kind,
                    content_format: entry.content_format,
                    variant: entry.variant,
                },
            });
        }
        for (asin, path) in db.reorg_annotations(marketplace.clone()).await? {
            let Some(values) = template_context(ctx, marketplace, &asin).await else {
                skipped += 1;
                continue;
            };
            let base = resolve_base(
                target_mode,
                target_template.as_deref(),
                max_len,
                &asin,
                &values,
            )?;
            let old = PathBuf::from(&path);
            let new = target_dir.join(format!("{base}.annot"));
            if old == new {
                continue;
            }
            planned.push(PlannedMove {
                old,
                new,
                sidecar: None,
                update: ReorgUpdate::Annotation {
                    asin,
                    marketplace: marketplace.clone(),
                },
            });
        }
    }

    // Refuse if two files would land on the same path.
    let mut seen = std::collections::HashSet::new();
    let collisions: Vec<&Path> = planned
        .iter()
        .filter(|m| !seen.insert(m.new.as_path()))
        .map(|m| m.new.as_path())
        .collect();
    if !collisions.is_empty() {
        for path in &collisions {
            eprintln!("collision: {}", path.display());
        }
        anyhow::bail!(
            "{} destination(s) would collide — refusing (disambiguate the template)",
            collisions.len()
        );
    }
    if skipped > 0 {
        eprintln!(
            "note: {skipped} recorded file(s) have no library metadata (item not synced) — skipped"
        );
    }
    if planned.is_empty() {
        // The requested setting change still applies (the files simply already
        // match it) — dropping it silently would leave the user's ask undone.
        if !dry_run {
            persist_naming(ctx, &sets, &unsets)?;
        }
        eprintln!("nothing to reorganize — files already match the target naming");
        return Ok(());
    }

    let (verb, verb_plural, verb_past) = if copy {
        ("copy", "copies", "copied")
    } else {
        ("move", "moves", "moved")
    };
    eprintln!("planned {verb_plural} ({}):", planned.len());
    for m in &planned {
        eprintln!("  {}\n    → {}", m.old.display(), m.new.display());
    }
    if dry_run {
        eprintln!("dry run — nothing changed");
        return Ok(());
    }
    if !confirm(yes, &format!("{verb} {} file(s)?", planned.len()))? {
        eprintln!("aborted");
        return Ok(());
    }

    // Persist the naming changes to the -s bundle only after the confirmation,
    // so an abort leaves config and files untouched alike.
    persist_naming(ctx, &sets, &unsets)?;

    // Move/copy each file, then update the DB row (never lose track of a file).
    let (mut done, mut failed) = (0usize, 0usize);
    for m in &planned {
        if let Err(error) = relocate(&m.old, &m.new, copy).await {
            eprintln!("skip {}: {error}", m.old.display());
            failed += 1;
            continue;
        }
        if let Some((old_sidecar, new_sidecar)) = &m.sidecar {
            let _ = relocate(old_sidecar, new_sidecar, copy).await; // best-effort
        }
        let update = match &m.update {
            ReorgUpdate::Download {
                asin,
                marketplace,
                kind,
                content_format,
                variant,
            } => {
                db.update_download_path(
                    asin.clone(),
                    marketplace.clone(),
                    kind.clone(),
                    content_format.clone(),
                    variant.clone(),
                    m.new.display().to_string(),
                )
                .await
            }
            ReorgUpdate::Annotation { asin, marketplace } => {
                db.set_annotation_path(
                    marketplace.clone(),
                    asin.clone(),
                    m.new.display().to_string(),
                )
                .await
            }
        };
        if let Err(error) = update {
            eprintln!(
                "warning: {verb_past} {} but could not update the database: {error}",
                m.new.display()
            );
        }
        done += 1;
    }
    if !copy {
        cleanup_empty_dirs(&planned, &current_dir).await;
    }
    eprintln!(
        "{verb_past} {done} file(s){}",
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Persists the collected naming changes to the config (validated write path);
/// a no-op when nothing was requested. Reports each change on stderr.
fn persist_naming(ctx: &Ctx, sets: &[(String, String)], unsets: &[String]) -> Result<()> {
    if sets.is_empty() && unsets.is_empty() {
        return Ok(());
    }
    let set_refs: Vec<(&str, &str)> = sets
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    crate::config::write::edit_file(&ctx.config_file(), |content| {
        let mut content = crate::config::write::set_many(content, &set_refs)?;
        for key in unsets {
            content = crate::config::write::unset(&content, key)?;
        }
        Ok(content)
    })?;
    for (key, value) in sets {
        eprintln!("set {key} = {value}");
    }
    for key in unsets {
        eprintln!("unset {key}");
    }
    Ok(())
}

/// Moves (or copies) `old` → `new`, creating parents. Falls back to copy+remove
/// across filesystems (where `rename` fails). A missing source is reported, and
/// an existing (different) target is never overwritten — untracked files and
/// order-dependent swaps stay safe. All IO is async (`tokio::fs` runs it on the
/// blocking pool), so a cross-filesystem copy of a multi-GB audio file never
/// stalls the executor (rule #10).
async fn relocate(old: &Path, new: &Path, copy: bool) -> Result<()> {
    if !tokio::fs::try_exists(old).await.unwrap_or(false) {
        anyhow::bail!("source file is gone: {}", old.display());
    }
    if tokio::fs::try_exists(new).await.unwrap_or(false) {
        // Allow only the same-file case (e.g. a case-only rename on a
        // case-insensitive filesystem); anything else would clobber data.
        let same = match (
            tokio::fs::canonicalize(old).await,
            tokio::fs::canonicalize(new).await,
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same {
            anyhow::bail!("target already exists: {}", new.display());
        }
    }
    if let Some(parent) = new.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    if copy {
        tokio::fs::copy(old, new)
            .await
            .with_context(|| format!("could not copy to {}", new.display()))?;
        return Ok(());
    }
    if tokio::fs::rename(old, new).await.is_err() {
        // Cross-filesystem rename fails → copy then remove.
        tokio::fs::copy(old, new)
            .await
            .with_context(|| format!("could not move to {}", new.display()))?;
        tokio::fs::remove_file(old)
            .await
            .with_context(|| format!("could not remove {}", old.display()))?;
    }
    Ok(())
}

/// Removes directories left empty by the moves (walking upward), bounded by the
/// **source** download dir (where the old files lived — the relevant root even
/// when `--download-dir` moved everything elsewhere) and never removing it.
async fn cleanup_empty_dirs(planned: &[PlannedMove], base: &Path) {
    let mut dirs: Vec<PathBuf> = planned
        .iter()
        .filter_map(|m| m.old.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let mut current = dir.as_path();
        while current != base && current.starts_with(base) {
            let empty = match tokio::fs::read_dir(current).await {
                Ok(mut entries) => matches!(entries.next_entry().await, Ok(None)),
                Err(_) => false,
            };
            if !empty || tokio::fs::remove_dir(current).await.is_err() {
                break;
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
    }
}

/// Whether a dotted config key affects download file names — so changing it is
/// worth a `download reorganize` hint.
pub(crate) fn key_affects_filenames(key: &str) -> bool {
    key.starts_with("settings.")
        && matches!(
            key.rsplit('.').next(),
            Some("filename_mode" | "filename_template" | "download_dir")
        )
}

/// Prints a hint to run `download reorganize`, but only when there are recorded
/// downloads to migrate. Best-effort — silent on any error (e.g. no account/DB).
pub(crate) async fn hint_reorganize(ctx: &Ctx) {
    let has_downloads = async {
        let db = ctx.open_library_db().await.ok()?;
        for marketplace in ctx.marketplaces().ok()? {
            if !db.reorg_downloads(marketplace).await.ok()?.is_empty() {
                return Some(true);
            }
        }
        Some(false)
    }
    .await
    .unwrap_or(false);
    if has_downloads {
        eprintln!(
            "note: existing downloads still use the previous naming — migrate them with:\n  \
             audible download reorganize --dry-run   (preview; then without --dry-run to apply)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_mode_from_str_maps_all() {
        use crate::config::schema::FilenameMode;
        assert_eq!(filename_mode_from_str("ascii"), FilenameMode::Ascii);
        assert_eq!(filename_mode_from_str("unicode"), FilenameMode::Unicode);
        assert_eq!(
            filename_mode_from_str("asin_ascii"),
            FilenameMode::AsinAscii
        );
        assert_eq!(
            filename_mode_from_str("asin_unicode"),
            FilenameMode::AsinUnicode
        );
        assert_eq!(filename_mode_from_str("custom"), FilenameMode::Custom);
    }

    #[test]
    fn key_affects_filenames_only_naming_settings() {
        assert!(key_affects_filenames("settings.default.filename_mode"));
        assert!(key_affects_filenames(
            "settings.audiobooks.filename_template"
        ));
        assert!(key_affects_filenames("settings.default.download_dir"));
        assert!(!key_affects_filenames("settings.default.overwrite"));
        assert!(!key_affects_filenames("db.page_size"));
        assert!(!key_affects_filenames("filename_mode"));
    }

    #[tokio::test]
    async fn relocate_moves_creates_parents_and_copies() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old/x.txt");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"data").unwrap();

        // Move into a not-yet-existing nested folder.
        let moved = dir.path().join("new/sub/x.txt");
        relocate(&old, &moved, false).await.unwrap();
        assert!(!old.exists() && moved.exists());

        // Copy leaves the source in place (backup mode).
        let copied = dir.path().join("copy/x.txt");
        relocate(&moved, &copied, true).await.unwrap();
        assert!(moved.exists() && copied.exists());

        // A missing source is an error.
        assert!(
            relocate(
                &dir.path().join("gone.txt"),
                &dir.path().join("o.txt"),
                false
            )
            .await
            .is_err()
        );

        // An existing (different) target is never overwritten — move or copy.
        std::fs::write(&moved, b"other").unwrap();
        assert!(relocate(&copied, &moved, false).await.is_err());
        assert!(relocate(&copied, &moved, true).await.is_err());
        assert_eq!(std::fs::read(&moved).unwrap(), b"other");
    }
}
