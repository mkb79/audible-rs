//! `download reorganize` — relocate recorded files (and their sidecars) to a
//! new naming scheme and keep the database in sync (AUD-54). The engine that
//! computes the paths lives in [`crate::naming`].

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::{Arg, ArgAction};

use crate::commands::prompt::confirm;
use crate::config::ctx::Ctx;
use crate::naming::{
    EXTERNAL_DIR, artifact_suffix, download_dir, expand_tilde, join_relative, resolve_base,
    template_context,
};

/// Where a file with no library document goes: the same place inside the tree,
/// under [`EXTERNAL_DIR`] (AUD-197).
///
/// Its name is not recomputed — with nothing to read it from, the name it was
/// given when it was downloaded is the only one there is. So only the tree's
/// root moves, and its place within the tree is preserved.
///
/// `None` for a file that is not under the download directory at all (a `--dir`
/// grab): those are left where they are rather than pulled in.
fn external_target(file_path: &str, current_dir: &Path, target_dir: &Path) -> Option<PathBuf> {
    let relative = Path::new(file_path).strip_prefix(current_dir).ok()?;
    // A file already filed here keeps one level of it, not one per run.
    let relative = relative.strip_prefix(EXTERNAL_DIR).unwrap_or(relative);
    Some(target_dir.join(EXTERNAL_DIR).join(relative))
}

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

/// The `(old, new)` paths for one recorded download under the target naming, or
/// `None` when it already matches. Pure over its inputs (no `Ctx`/IO) so the
/// migration is unit-testable: `old` is the stored path (keeping whatever slug
/// it was written with), while `new` is re-resolved from the item's title with
/// the *current* slug. Any naming-engine change (e.g. AUD-199) therefore surfaces
/// here as `old != new` and plans a rename, with no reorganize-specific code.
#[allow(clippy::too_many_arguments)]
fn planned_paths(
    asin: &str,
    kind: &str,
    content_format: &str,
    file_path: &str,
    values: &std::collections::HashMap<&'static str, String>,
    target_mode: crate::config::schema::FilenameMode,
    target_template: Option<&str>,
    max_len: usize,
    target_dir: &Path,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let base = resolve_base(target_mode, target_template, max_len, asin, values)?;
    let old = PathBuf::from(file_path);
    let ext = old.extension().and_then(|e| e.to_str()).unwrap_or("");
    let new = join_relative(
        target_dir,
        &format!("{base}{}", artifact_suffix(kind, content_format, ext)),
    );
    Ok((old != new).then_some((old, new)))
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
            // Where a file belongs follows from one question: does the library
            // have a document for it (AUD-197)? With one, the template decides
            // — which also carries a title *out* of the external folder the
            // moment it enters the library. Without one, nothing can re-derive
            // a name, so the file keeps the one it was given and only follows
            // the download directory.
            let paths = match template_context(ctx, marketplace, &entry.asin).await {
                Some(values) => planned_paths(
                    &entry.asin,
                    &entry.kind,
                    &entry.content_format,
                    &entry.file_path,
                    &values,
                    target_mode,
                    target_template.as_deref(),
                    max_len,
                    &target_dir,
                )?,
                None => match external_target(&entry.file_path, &current_dir, &target_dir) {
                    Some(new) => {
                        let old = PathBuf::from(&entry.file_path);
                        (old != new).then_some((old, new))
                    }
                    // Outside the download tree entirely (a `--dir` grab): left
                    // where it is rather than dragged in.
                    None => {
                        skipped += 1;
                        None
                    }
                },
            };
            let Some((old, new)) = paths else {
                continue;
            };
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
            let new = join_relative(&target_dir, &format!("{base}.annot"));
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
            "note: {skipped} recorded file(s) lie outside the download directory \
             and have no library entry to rename them by — left where they are"
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
        // The file moved; any defect from here still counts the item as
        // failed, but the database must keep tracking the moved file.
        let mut item_ok = true;
        if let Some((old_sidecar, new_sidecar)) = &m.sidecar
            && let Err(error) = relocate_sidecar(old_sidecar, new_sidecar, copy).await
        {
            eprintln!(
                "{verb_past} {} but its key sidecar did not follow: {error} — \
                 the audio cannot be decrypted without it (sidecar left at {})",
                m.new.display(),
                old_sidecar.display()
            );
            item_ok = false;
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
                "{verb_past} {} but could not update the database: {error} — \
                 its record still names the old path",
                m.new.display()
            );
            item_ok = false;
        }
        if item_ok {
            done += 1;
        } else {
            failed += 1;
        }
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
    // Exit-code honesty: a partly-failed reorganize must not report success
    // (its siblings `download`/`library sync` bail the same way).
    if failed > 0 {
        anyhow::bail!("{failed} of {} file(s) did not fully {verb}", planned.len());
    }
    Ok(())
}

/// Relocates a key sidecar when it exists on disk. A sidecar may
/// legitimately be absent (it is regenerable from the stored license, and
/// `db downloads check` reports that state on its own) — only an existing
/// sidecar that fails to move is an error.
async fn relocate_sidecar(old: &Path, new: &Path, copy: bool) -> Result<()> {
    match tokio::fs::try_exists(old).await {
        Ok(true) => relocate(old, new, copy).await,
        Ok(false) => Ok(()),
        Err(error) => anyhow::bail!("could not probe {}: {error}", old.display()),
    }
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

    /// A file with no library document follows the download directory and
    /// nothing else — its name cannot be re-derived, so only the root moves
    /// (AUD-197).
    #[test]
    fn a_file_without_a_document_follows_the_download_dir() {
        let old_root = Path::new("/books");
        let new_root = Path::new("/moved");

        // Downloaded as external: keeps its place inside the folder.
        assert_eq!(
            external_target(
                "/books/__external__/unknown/Title [B0EXAMPLE1].aaxc",
                old_root,
                new_root
            ),
            Some(PathBuf::from(
                "/moved/__external__/unknown/Title [B0EXAMPLE1].aaxc"
            ))
        );

        // Never nests: a second run must not produce __external__/__external__.
        let once = external_target("/books/__external__/X.aaxc", old_root, old_root).unwrap();
        assert_eq!(once, PathBuf::from("/books/__external__/X.aaxc"));
        assert_eq!(
            external_target(once.to_str().unwrap(), old_root, old_root),
            Some(once),
            "already filed, so the plan is a no-op rather than another level"
        );

        // A title that lost its document *after* being filed normally is drawn
        // in, keeping the folders it sat in.
        assert_eq!(
            external_target("/books/Some Show/Ep 1.aaxc", old_root, new_root),
            Some(PathBuf::from("/moved/__external__/Some Show/Ep 1.aaxc"))
        );

        // Outside the tree entirely (a `--dir` grab): not ours to move.
        assert!(external_target("/tmp/grab/X.aaxc", old_root, new_root).is_none());
    }

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

    #[tokio::test]
    async fn a_missing_sidecar_is_fine_a_stuck_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();

        // Absent on disk (regenerable from the stored license): no error —
        // otherwise every voucher-less move would now fail the run.
        let gone = dir.path().join("book.voucher");
        let target = dir.path().join("moved/book.voucher");
        relocate_sidecar(&gone, &target, false).await.unwrap();

        // Present but its move fails (target exists as a different file):
        // that is a counted error — a stranded key sidecar means the audio
        // cannot be decrypted after the move.
        std::fs::write(&gone, b"key").unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"other").unwrap();
        assert!(relocate_sidecar(&gone, &target, false).await.is_err());
    }

    // Proves the migration (AUD-199): a file recorded with the OLD slug — the
    // stray `_ ` a replaced `:` used to leave — is planned for rename to the
    // current gap-free slug, with no reorganize-specific migration code. `old` is
    // the stored path; `new` is re-resolved from the title. Before the unicode
    // fix the slug still produced `_ `, so `new == old` and this returns None
    // (nothing to do); the fix is exactly what makes them differ.
    #[test]
    fn a_slug_change_plans_a_rename_of_existing_downloads() {
        use crate::config::schema::FilenameMode;
        let values: std::collections::HashMap<&'static str, String> = [
            ("publication", "Marvel's Wastelanders: Star-Lord"),
            ("fulltitle", "Kapitel Zehn: Götterdämmerung"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_owned()))
        .collect();
        // The path a pre-fix download wrote: `_ ` after each replaced colon.
        let stored =
            "/dl/Marvel's Wastelanders_ Star-Lord/Kapitel Zehn_ Götterdämmerung.AAX_44_128.aaxc";

        let (old, new) = planned_paths(
            "B0",
            "audio",
            "AAX_44_128",
            stored,
            &values,
            FilenameMode::Custom,
            Some("%publication%/%fulltitle%"),
            230,
            Path::new("/dl"),
        )
        .unwrap()
        .expect("a rename must be planned once the slug drops the stray space");

        assert_eq!(old, PathBuf::from(stored));
        assert_eq!(
            new,
            PathBuf::from(
                "/dl/Marvel's Wastelanders_Star-Lord/Kapitel Zehn_Götterdämmerung.AAX_44_128.aaxc"
            )
        );
    }
}
