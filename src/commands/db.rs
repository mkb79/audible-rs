//! `audible db` — local database maintenance. The
//! `db` group holds operations the regular `library` commands do not
//! cover; table-specific ones are grouped under a table noun
//! (`db downloads …`, `db library …`), whole-database ones stay at the
//! top level: `db downloads list`/`add`/`remove`/`check`/`prune`,
//! `db library remove`, `db info`, `db backup`/`restore`, `db vacuum`,
//! `db check`, `db reset` (archived architecture §12).

use super::prompt::confirm;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Arg, ArgAction};

use crate::config::ctx::Ctx;
use crate::db::{
    DOWNLOAD_KINDS as KINDS, DOWNLOAD_VARIANTS as VARIANTS, DownloadEntry, DownloadRecord,
};
use crate::output::Output;

/// `audible db`.
pub struct DbCommand;

#[async_trait::async_trait]
impl super::Command for DbCommand {
    fn name(&self) -> &'static str {
        "db"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Maintain the local library database")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("downloads")
                    .about("Maintain tracked downloads")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        crate::commands::items::item_source_args(
                            clap::Command::new("list").about("List all tracked downloads"),
                        )
                        .arg(
                            Arg::new("kind")
                                .long("kind")
                                .value_name("KIND")
                                .value_parser(KINDS)
                                .help("Only show downloads of this kind"),
                        )
                        .arg(
                            Arg::new("variant")
                                .long("variant")
                                .value_name("VARIANT")
                                .value_parser(VARIANTS)
                                .help(
                                    "Only show this audio variant (original|decrypted|reencoded)",
                                ),
                        ),
                    )
                    .subcommand(
                        clap::Command::new("add")
                            .about("Manually record a download (e.g. fetched outside this tool)")
                            .arg(
                                Arg::new("asin")
                                    .long("asin")
                                    .required(true)
                                    .value_name("ASIN")
                                    .help("Item the file belongs to"),
                            )
                            .arg(
                                Arg::new("kind")
                                    .long("kind")
                                    .required(true)
                                    .value_parser(KINDS)
                                    .value_name("KIND")
                                    .help("Artifact kind of the file"),
                            )
                            .arg(
                                Arg::new("file")
                                    .long("file")
                                    .required(true)
                                    .value_name("PATH")
                                    .help("Path of the downloaded file"),
                            )
                            .arg(
                                Arg::new("format")
                                    .long("format")
                                    .value_name("FORMAT")
                                    .help("Content format/quality/size (part of the key)"),
                            )
                            .arg(
                                Arg::new("variant")
                                    .long("variant")
                                    .value_parser(VARIANTS)
                                    .default_value("original")
                                    .value_name("VARIANT")
                                    .help("Audio variant (original|decrypted|reencoded)"),
                            )
                            .arg(
                                Arg::new("request_kind")
                                    .long("request-kind")
                                    .value_parser(crate::commands::download::request_kind::ALL)
                                    .value_name("REQUEST_KIND")
                                    .help(
                                        "Which download request this file satisfies (audio \
                                         only, e.g. adrm-high = aaxc in high quality) — a \
                                         later `download` run with matching flags then \
                                         skips the item",
                                    ),
                            )
                            .arg(
                                Arg::new("require_file")
                                    .long("require-file")
                                    .action(ArgAction::SetTrue)
                                    .help("Fail if the file does not exist (default: warn)"),
                            ),
                    )
                    .subcommand(
                        crate::commands::items::item_source_args(
                            clap::Command::new("remove")
                                .about("Remove specific tracked downloads by filter"),
                        )
                        .arg(
                            Arg::new("kind")
                                .long("kind")
                                .value_parser(KINDS)
                                .value_name("KIND")
                                .help("Only records of this artifact kind"),
                        )
                        .arg(
                            Arg::new("format")
                                .long("format")
                                .value_name("FORMAT")
                                .help("Only records with this content format/quality label"),
                        )
                        .arg(
                            Arg::new("variant")
                                .long("variant")
                                .value_parser(VARIANTS)
                                .value_name("VARIANT")
                                .help("Only records of this audio variant"),
                        )
                        .arg(
                            Arg::new("with_files")
                                .long("with-files")
                                .action(ArgAction::SetTrue)
                                .help("Also delete the files on disk (not just the records)"),
                        )
                        .arg(super::yes_arg()),
                    )
                    .subcommand(clap::Command::new("check").about(
                        "List tracked downloads whose file is missing, has an \
                             unexpected size, or lacks its key sidecar (read-only)",
                    ))
                    .subcommand(
                        clap::Command::new("prune")
                            .about("Remove tracked downloads whose file is missing")
                            .arg(crate::commands::yes_arg()),
                    ),
            )
            .subcommand(
                clap::Command::new("library")
                    .about("Maintain stored library items")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        crate::commands::items::item_source_args(
                            clap::Command::new("remove").about(
                                "Hard-delete library items (episodes, series memberships, \
                                     downloads and licenses go with them)",
                            ),
                        )
                        .group(
                            clap::ArgGroup::new("source")
                                .args(["asin", "title"])
                                .multiple(true)
                                .required(true),
                        )
                        .arg(
                            Arg::new("with_files")
                                .long("with-files")
                                .action(ArgAction::SetTrue)
                                .help(
                                    "Also delete the downloaded files \
                                     (not just the records)",
                                ),
                        )
                        .arg(super::yes_arg()),
                    ),
            )
            .subcommand(
                clap::Command::new("info")
                    .about("Show database status (path, size, schema, counts)"),
            )
            .subcommand(
                clap::Command::new("backup")
                    .about("Write a consistent single-file snapshot of the database")
                    .arg(
                        Arg::new("path")
                            .required(true)
                            .value_name("PATH")
                            .help("Destination file (must not exist)"),
                    ),
            )
            .subcommand(
                clap::Command::new("restore")
                    .about("Replace the database with a snapshot (overwrites the current DB)")
                    .arg(
                        Arg::new("path")
                            .required(true)
                            .value_name("PATH")
                            .help("Snapshot to restore from"),
                    )
                    .arg(super::yes_arg()),
            )
            .subcommand(
                clap::Command::new("vacuum")
                    .about("Compact the database (checkpoint WAL + VACUUM)"),
            )
            .subcommand(
                clap::Command::new("check")
                    .about("Run an integrity check (PRAGMA integrity_check)"),
            )
            .subcommand(
                clap::Command::new("reset")
                    .about("Delete the whole database and its sidecars")
                    .arg(super::yes_arg()),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some(("downloads", sub)) => match sub.subcommand() {
                Some(("list", list)) => {
                    let asins: Vec<String> = list
                        .get_many::<String>("asin")
                        .map(|v| v.cloned().collect())
                        .unwrap_or_default();
                    let titles: Vec<String> = list
                        .get_many::<String>("title")
                        .map(|v| v.cloned().collect())
                        .unwrap_or_default();
                    let has_source = list.contains_id("asin") || list.contains_id("title");
                    downloads_list(
                        ctx,
                        asins,
                        titles,
                        has_source,
                        list.get_one::<String>("kind").cloned(),
                        list.get_one::<String>("variant").cloned(),
                    )
                    .await
                }
                Some(("add", add)) => {
                    downloads_add(
                        ctx,
                        add.get_one::<String>("asin").expect("required"),
                        add.get_one::<String>("kind").expect("required"),
                        add.get_one::<String>("file").expect("required"),
                        add.get_one::<String>("format").cloned(),
                        add.get_one::<String>("variant").expect("default"),
                        add.get_one::<String>("request_kind").cloned(),
                        add.get_flag("require_file"),
                    )
                    .await
                }
                Some(("remove", remove)) => {
                    let asins: Vec<String> = remove
                        .get_many::<String>("asin")
                        .map(|v| v.cloned().collect())
                        .unwrap_or_default();
                    let titles: Vec<String> = remove
                        .get_many::<String>("title")
                        .map(|v| v.cloned().collect())
                        .unwrap_or_default();
                    let has_source = remove.contains_id("asin") || remove.contains_id("title");
                    downloads_remove(
                        ctx,
                        asins,
                        titles,
                        has_source,
                        remove.get_one::<String>("kind").cloned(),
                        remove.get_one::<String>("format").cloned(),
                        remove.get_one::<String>("variant").cloned(),
                        remove.get_flag("with_files"),
                        remove.get_flag("yes"),
                    )
                    .await
                }
                Some(("check", _)) => downloads_check(ctx).await,
                Some(("prune", prune)) => downloads_prune(ctx, prune.get_flag("yes")).await,
                _ => unreachable!("subcommand required"),
            },
            Some(("library", sub)) => match sub.subcommand() {
                Some(("remove", remove)) => {
                    library_remove(
                        ctx,
                        remove
                            .get_many::<String>("asin")
                            .map(|v| v.cloned().collect())
                            .unwrap_or_default(),
                        remove
                            .get_many::<String>("title")
                            .map(|v| v.cloned().collect())
                            .unwrap_or_default(),
                        remove.get_flag("with_files"),
                        remove.get_flag("yes"),
                    )
                    .await
                }
                _ => unreachable!("subcommand required"),
            },
            Some(("info", _)) => db_info(ctx).await,
            Some(("backup", backup)) => {
                db_backup(ctx, backup.get_one::<String>("path").expect("required")).await
            }
            Some(("restore", restore)) => {
                db_restore(
                    ctx,
                    restore.get_one::<String>("path").expect("required"),
                    restore.get_flag("yes"),
                )
                .await
            }
            Some(("vacuum", _)) => db_vacuum(ctx).await,
            Some(("check", _)) => db_check(ctx).await,
            Some(("reset", reset)) => db_reset(ctx, reset.get_flag("yes")).await,
            _ => unreachable!("subcommand required"),
        }
    }
}

/// `db library remove` — hard-delete items together with their
/// episodes, series memberships, download records and licenses.
async fn library_remove(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    with_files: bool,
    yes: bool,
) -> Result<()> {
    let db = ctx.open_library_db().await?;
    // Destructive single-marketplace operation: -m must select one.
    let marketplace = ctx.marketplace_single()?;

    let resolved = crate::commands::items::resolve_asins(
        &db,
        &marketplace,
        asins,
        titles,
        crate::commands::items::PodcastMode::ItemsOnly,
    )
    .await?;
    if resolved.is_empty() {
        eprintln!("no items to remove");
        return Ok(());
    }

    // Preview what is about to go (titles where known).
    for asin in &resolved {
        match db.find_title(asin.clone(), marketplace.clone()).await? {
            Some(title) => eprintln!("  {asin}  {title}"),
            None => eprintln!("  {asin}"),
        }
    }
    let prompt = if with_files {
        format!(
            "Remove {} item(s) AND delete their downloaded files?",
            resolved.len()
        )
    } else {
        format!("Remove {} item(s)?", resolved.len())
    };
    if !confirm(yes, &prompt)? {
        eprintln!("aborted; nothing removed");
        return Ok(());
    }

    let removal = db.remove_items(marketplace, resolved).await?;
    for asin in &removal.missing_asins {
        eprintln!("warning: {asin} is not in the database");
    }
    eprintln!(
        "removed {} item(s), {} episode(s), {} download record(s), {} license(s)",
        removal.removed_asins.len(),
        removal.episodes_removed,
        removal.downloads_removed,
        removal.licenses_removed,
    );
    if with_files {
        eprintln!("deleted {} file(s)", delete_files(&removal.file_paths));
    }
    Ok(())
}

/// `db info` — read-only database status.
async fn db_info(ctx: &Ctx) -> Result<()> {
    let db = ctx.open_library_db().await?;
    let stats = db.stats().await?;
    let path = db.path().to_path_buf();

    ctx.print(&Output::KeyValue(vec![
        ("path".into(), path.display().to_string()),
        ("size".into(), human_size(file_size(&path))),
        (
            "wal / shm".into(),
            format!(
                "{} / {}",
                human_size(file_size(&sidecar(&path, "-wal"))),
                human_size(file_size(&sidecar(&path, "-shm"))),
            ),
        ),
        ("schema".into(), format!("v{}", stats.schema_version)),
        (
            "items".into(),
            format!(
                "{} active, {} deleted",
                stats.items_active, stats.items_deleted
            ),
        ),
        ("episodes".into(), stats.episodes_active.to_string()),
        ("series".into(), stats.series.to_string()),
        ("downloads".into(), stats.downloads.to_string()),
        ("licenses".into(), stats.licenses.to_string()),
        (
            "last sync".into(),
            stats.last_sync_utc.unwrap_or_else(|| "never".to_owned()),
        ),
    ]));
    Ok(())
}

/// `db vacuum` — compact the database, reporting the size change.
async fn db_vacuum(ctx: &Ctx) -> Result<()> {
    let db = ctx.open_library_db().await?;
    let path = db.path().to_path_buf();
    let before = file_size(&path);
    db.vacuum().await?;
    let after = file_size(&path);
    eprintln!("vacuumed: {} -> {}", human_size(before), human_size(after));
    Ok(())
}

/// `db check` — integrity check; non-zero exit when problems are found.
async fn db_check(ctx: &Ctx) -> Result<()> {
    let db = ctx.open_library_db().await?;
    let result = db.integrity_check().await?;
    if result == ["ok"] {
        eprintln!("integrity check: ok");
        return Ok(());
    }
    for line in &result {
        eprintln!("{line}");
    }
    bail!("integrity check found {} problem(s)", result.len());
}

/// `db reset` — delete the database and its sidecars after confirmation.
async fn db_reset(ctx: &Ctx, yes: bool) -> Result<()> {
    let path = ctx.library_db_path().await?;
    if !path.exists() {
        eprintln!("no database at {}", path.display());
        return Ok(());
    }

    eprintln!(
        "This deletes the database and its sidecars:\n  {} ({})",
        path.display(),
        human_size(file_size(&path))
    );
    if !confirm(yes, "Delete the whole database?")? {
        eprintln!("aborted; database unchanged");
        return Ok(());
    }

    for victim in [
        path.clone(),
        sidecar(&path, "-wal"),
        sidecar(&path, "-shm"),
        path.with_extension("lock"),
    ] {
        if let Err(error) = std::fs::remove_file(&victim)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("warning: could not delete {}: {error}", victim.display());
        }
    }
    eprintln!("database deleted; the next `library sync` creates a fresh one");
    Ok(())
}

/// `db backup` — consistent single-file snapshot via `VACUUM INTO`.
async fn db_backup(ctx: &Ctx, dest: &str) -> Result<()> {
    let dest_path = Path::new(dest);
    if dest_path.exists() {
        bail!("{dest} already exists (choose another path or remove it first)");
    }
    let db = ctx.open_library_db().await?;
    db.backup_into(dest.to_owned()).await?;
    eprintln!(
        "backed up database to {dest} ({})",
        human_size(file_size(dest_path))
    );
    Ok(())
}

/// `db restore` — replace the current database with a snapshot. Removes
/// the target's stale `-wal`/`-shm`/`.lock` so no old WAL is applied.
async fn db_restore(ctx: &Ctx, source: &str, yes: bool) -> Result<()> {
    let src = Path::new(source);
    if !is_sqlite(src) {
        bail!("{source} is not a SQLite database");
    }
    let dest = ctx.library_db_path().await?;
    if same_file(src, &dest) {
        bail!("{source} is the current database");
    }

    eprintln!("This overwrites the current database");
    eprintln!("  {}", dest.display());
    eprintln!("with the snapshot");
    eprintln!("  {source}");
    if !confirm(yes, "Continue?")? {
        eprintln!("aborted; database unchanged");
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, &dest)
        .with_context(|| format!("could not copy {source} to {}", dest.display()))?;
    // A stale WAL/shm/lock from the old DB must not survive, or SQLite
    // would apply the old WAL on top of the restored file.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(sidecar(&dest, suffix));
    }
    let _ = std::fs::remove_file(dest.with_extension("lock"));

    eprintln!("restored database from {source}");
    Ok(())
}

/// Whether a file starts with the SQLite header magic.
fn is_sqlite(path: &Path) -> bool {
    use std::io::Read as _;
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0"
}

/// Whether two paths point to the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// File size in bytes, or `None` if the file is absent.
fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// Human-readable size, `-` when absent.
pub(crate) fn human_size(size: Option<u64>) -> String {
    size.map(|n| indicatif::BinaryBytes(n).to_string())
        .unwrap_or_else(|| "-".to_owned())
}

/// The DB file path with a suffix appended (`-wal`, `-shm`).
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// A tracked download whose on-disk size differs from the recorded one
/// (truncated or replaced file).
struct SizeMismatch {
    entry: DownloadEntry,
    /// Actual size found on disk.
    found: u64,
}

/// A tracked encrypted original whose derived key sidecar
/// (`.voucher`/`.wvkey`) is gone — the audio exists but cannot be
/// decrypted without it (AUD-106; sidecars have no DB row, AUD-99).
struct SidecarMissing {
    entry: DownloadEntry,
    /// The missing sidecar path, derived from the recorded file.
    sidecar: PathBuf,
}

/// All tracked downloads, partitioned by on-disk state.
struct DownloadsReport {
    total: usize,
    /// Records whose file is gone.
    missing: Vec<DownloadEntry>,
    /// Records whose file exists but has an unexpected size. Records
    /// without a stored size (manual `db downloads add`) are never here.
    mismatched: Vec<SizeMismatch>,
    /// Encrypted originals whose key sidecar is gone. A record can be
    /// here and in `mismatched` at once — two distinct problems.
    sidecar_missing: Vec<SidecarMissing>,
}

/// Loads all tracked downloads and classifies them by on-disk state.
async fn scan_downloads(ctx: &Ctx) -> Result<DownloadsReport> {
    let db = ctx.open_library_db().await?;
    let entries = db.download_entries().await?;
    Ok(classify_downloads(entries))
}

/// Partitions download records into missing files, size mismatches and
/// missing key sidecars — one `stat` per record covers existence + size
/// (AUD-102), plus one per encrypted original for its sidecar (AUD-106;
/// `sidecar_path` is extension-based, so everything without a sidecar
/// self-selects out).
fn classify_downloads(entries: Vec<DownloadEntry>) -> DownloadsReport {
    let total = entries.len();
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    let mut sidecar_missing = Vec::new();
    for entry in entries {
        match file_size(Path::new(&entry.file_path)) {
            None => missing.push(entry),
            Some(found) => {
                if let Some(sidecar) = crate::naming::sidecar_path(Path::new(&entry.file_path))
                    && !sidecar.exists()
                {
                    sidecar_missing.push(SidecarMissing {
                        entry: entry.clone(),
                        sidecar,
                    });
                }
                if entry.file_size.is_some_and(|expected| expected != found) {
                    mismatched.push(SizeMismatch { entry, found });
                }
            }
        }
    }
    DownloadsReport {
        total,
        missing,
        mismatched,
        sidecar_missing,
    }
}

/// Renders download records as a table (the path column header varies:
/// `path` vs `missing path`).
fn download_table(ctx: &Ctx, entries: &[DownloadEntry], path_header: &str) {
    ctx.print(&Output::table(
        vec!["asin", "kind", "variant", "format", "size", path_header],
        entries
            .iter()
            .map(|entry| {
                let size = entry
                    .file_size
                    .map(|n| indicatif::BinaryBytes(n).to_string())
                    .unwrap_or_else(|| "-".to_owned());
                vec![
                    entry.asin.clone(),
                    entry.kind.clone(),
                    entry.variant.clone(),
                    show_format(&entry.content_format),
                    size,
                    entry.file_path.clone(),
                ]
            })
            .collect(),
    ));
}

/// Human-readable content_format (empty → `-`).
fn show_format(content_format: &str) -> String {
    if content_format.is_empty() {
        "-".to_owned()
    } else {
        content_format.to_owned()
    }
}

/// Deletes the given files plus each one's key sidecar (`.voucher`/`.wvkey`
/// for an aaxc/cenc — AUD-99), tolerating already-missing ones. Returns the
/// number actually deleted; failures are warnings, not errors.
fn delete_files(paths: &[String]) -> usize {
    let mut deleted = 0;
    for path in paths {
        if remove_if_present(std::path::Path::new(path)) {
            deleted += 1;
        }
        // The DRM key sidecar is a derived file (no DB row); drop it with the
        // audio so no 0600 key material orphans.
        if let Some(sidecar) = crate::naming::sidecar_path(std::path::Path::new(path))
            && remove_if_present(&sidecar)
        {
            deleted += 1;
        }
    }
    deleted
}

/// Removes a file, treating "already gone" as success-with-no-op. Returns
/// whether a file was actually deleted; other errors warn.
pub(crate) fn remove_if_present(path: &std::path::Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            eprintln!("warning: could not delete {}: {error}", path.display());
            false
        }
    }
}

/// `db downloads list` — all tracked downloads, optionally filtered.
async fn downloads_list(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    has_source: bool,
    kind: Option<String>,
    variant: Option<String>,
) -> Result<()> {
    let db = ctx.open_library_db().await?;

    // Title resolution (the asin filter) is single-marketplace; listing
    // without a filter spans the whole per-account database.
    let asin_filter: Option<std::collections::HashSet<String>> = if has_source {
        let marketplace = ctx.marketplace_single()?;
        Some(
            crate::commands::items::resolve_asins(
                &db,
                &marketplace,
                asins,
                titles,
                crate::commands::items::PodcastMode::Episodes,
            )
            .await?
            .into_iter()
            .collect(),
        )
    } else {
        None
    };

    let mut entries = db.download_entries().await?;
    entries.retain(|entry| {
        asin_filter
            .as_ref()
            .is_none_or(|set| set.contains(&entry.asin))
            && kind.as_ref().is_none_or(|k| &entry.kind == k)
            && variant.as_ref().is_none_or(|v| &entry.variant == v)
    });

    if entries.is_empty() {
        eprintln!("no tracked downloads");
        return Ok(());
    }
    download_table(ctx, &entries, "path");
    Ok(())
}

/// `db downloads add` — manually record a download for an existing file.
#[allow(clippy::too_many_arguments)]
async fn downloads_add(
    ctx: &Ctx,
    asin: &str,
    kind: &str,
    file: &str,
    format: Option<String>,
    variant: &str,
    request_kind: Option<String>,
    require_file: bool,
) -> Result<()> {
    if request_kind.is_some() && kind != "audio" {
        bail!("--request-kind only applies to --kind audio");
    }
    let file_size = std::fs::metadata(file).ok().map(|meta| meta.len());
    if file_size.is_none() {
        if require_file {
            bail!("{file} does not exist (drop --require-file to record it anyway)");
        }
        eprintln!("warning: {file} does not exist; recording without a size");
    }
    let db = ctx.open_library_db().await?;
    let marketplace = ctx.marketplace_single()?;
    db.record_download(
        marketplace,
        DownloadRecord {
            asin: asin.to_owned(),
            kind: kind.to_owned(),
            acr: None,
            content_format: format.unwrap_or_default(),
            variant: variant.to_owned(),
            request_kind: request_kind.unwrap_or_default(),
            version: None,
            sku: None,
            file_path: file.to_owned(),
            file_size,
        },
    )
    .await?;
    eprintln!("recorded {kind} download for {asin}");
    Ok(())
}

/// `db downloads remove` — delete records matching the given filters
/// (at least one), regardless of whether the file exists.
#[allow(clippy::too_many_arguments)]
async fn downloads_remove(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    has_source: bool,
    kind: Option<String>,
    format: Option<String>,
    variant: Option<String>,
    with_files: bool,
    yes: bool,
) -> Result<()> {
    if !has_source && kind.is_none() && format.is_none() && variant.is_none() {
        bail!(
            "specify at least one of --asin/--title/--kind/--format/--variant \
             (to clear the whole database use `db reset`)"
        );
    }

    let db = ctx.open_library_db().await?;

    let asin_filter: Option<std::collections::HashSet<String>> = if has_source {
        let marketplace = ctx.marketplace_single()?;
        Some(
            crate::commands::items::resolve_asins(
                &db,
                &marketplace,
                asins,
                titles,
                crate::commands::items::PodcastMode::Episodes,
            )
            .await?
            .into_iter()
            .collect(),
        )
    } else {
        None
    };

    let matched: Vec<DownloadEntry> = db
        .download_entries()
        .await?
        .into_iter()
        .filter(|entry| {
            asin_filter
                .as_ref()
                .is_none_or(|set| set.contains(&entry.asin))
                && kind.as_ref().is_none_or(|k| &entry.kind == k)
                && format.as_ref().is_none_or(|f| &entry.content_format == f)
                && variant.as_ref().is_none_or(|v| &entry.variant == v)
        })
        .collect();

    if matched.is_empty() {
        eprintln!("no matching tracked downloads");
        return Ok(());
    }
    download_table(ctx, &matched, "path");

    let prompt = if with_files {
        format!(
            "Remove {} download records AND delete their files?",
            matched.len()
        )
    } else {
        format!("Remove {} download records?", matched.len())
    };
    if !confirm(yes, &prompt)? {
        eprintln!("aborted; nothing removed");
        return Ok(());
    }

    // Capture the paths before consuming the entries into delete keys.
    let paths: Vec<String> = matched
        .iter()
        .map(|entry| entry.file_path.clone())
        .collect();
    let keys = matched
        .into_iter()
        .map(|entry| {
            (
                entry.asin,
                entry.marketplace,
                entry.kind,
                entry.content_format,
                entry.variant,
            )
        })
        .collect();
    let removed = db.delete_downloads(keys).await?;
    eprintln!("removed {removed} download records");

    if with_files {
        eprintln!("deleted {} file(s)", delete_files(&paths));
    }
    Ok(())
}

/// `db downloads check` — read-only report of records with missing files,
/// an on-disk size that differs from the recorded one (AUD-102), or a
/// missing key sidecar of an encrypted original (AUD-106).
async fn downloads_check(ctx: &Ctx) -> Result<()> {
    let report = scan_downloads(ctx).await?;
    if report.missing.is_empty()
        && report.mismatched.is_empty()
        && report.sidecar_missing.is_empty()
    {
        eprintln!(
            "all {} tracked downloads check out (files present, sizes match, \
             key sidecars in place)",
            report.total
        );
        return Ok(());
    }

    // One table for all problem categories, so `-o json` stays a single
    // document; the `problem` column separates them.
    ctx.print(&Output::table(
        vec![
            "asin", "kind", "variant", "format", "problem", "expected", "found", "path",
        ],
        report
            .missing
            .iter()
            .map(|entry| problem_row(entry, "missing", entry.file_size, None, &entry.file_path))
            .chain(report.mismatched.iter().map(|mismatch| {
                problem_row(
                    &mismatch.entry,
                    "size mismatch",
                    mismatch.entry.file_size,
                    Some(mismatch.found),
                    &mismatch.entry.file_path,
                )
            }))
            .chain(report.sidecar_missing.iter().map(|item| {
                // The path column names the actionable artifact: the
                // sidecar that is gone, not the (healthy) audio file.
                problem_row(
                    &item.entry,
                    "sidecar missing",
                    None,
                    None,
                    &item.sidecar.display().to_string(),
                )
            }))
            .collect(),
    ));

    if !report.missing.is_empty() {
        eprintln!(
            "{} of {} tracked downloads reference missing files \
             (remove with `audible db downloads prune`)",
            report.missing.len(),
            report.total
        );
    }
    if !report.mismatched.is_empty() {
        eprintln!(
            "{} of {} tracked downloads differ in size from their record — \
             truncated or replaced? Re-fetch with `audible download --force`",
            report.mismatched.len(),
            report.total
        );
    }
    if !report.sidecar_missing.is_empty() {
        eprintln!(
            "{} of {} tracked downloads are missing their key sidecar \
             (.voucher/.wvkey) — the audio cannot be decrypted without it; \
             re-fetch with `audible download --force`",
            report.sidecar_missing.len(),
            report.total
        );
    }
    Ok(())
}

/// One row of the `check` problem table. `path` is the artifact the
/// problem is about (the recorded file, or the missing sidecar).
fn problem_row(
    entry: &DownloadEntry,
    problem: &str,
    expected: Option<u64>,
    found: Option<u64>,
    path: &str,
) -> Vec<String> {
    vec![
        entry.asin.clone(),
        entry.kind.clone(),
        entry.variant.clone(),
        show_format(&entry.content_format),
        problem.to_owned(),
        human_size(expected),
        human_size(found),
        path.to_owned(),
    ]
}

/// `db downloads prune` — remove records whose file is missing, after a
/// confirmation prompt (skipped with `--yes`). Size mismatches and missing
/// key sidecars are never prunable: the file is there, just suspect —
/// re-fetching is `download --force`'s job (AUD-102/AUD-106).
async fn downloads_prune(ctx: &Ctx, yes: bool) -> Result<()> {
    let report = scan_downloads(ctx).await?;
    if !report.mismatched.is_empty() {
        eprintln!(
            "note: {} size mismatch(es) left untouched (see `audible db downloads check`)",
            report.mismatched.len()
        );
    }
    if !report.sidecar_missing.is_empty() {
        eprintln!(
            "note: {} missing key sidecar(s) left untouched (see `audible db downloads check`)",
            report.sidecar_missing.len()
        );
    }
    let (total, missing) = (report.total, report.missing);
    if missing.is_empty() {
        eprintln!("all {total} tracked downloads reference existing files; nothing to prune");
        return Ok(());
    }
    download_table(ctx, &missing, "missing path");

    if !confirm(
        yes,
        &format!(
            "Remove {} download records? Their artifacts will be re-fetched on the next run.",
            missing.len()
        ),
    )? {
        eprintln!("aborted; nothing removed");
        return Ok(());
    }

    let db = ctx.open_library_db().await?;
    let keys = missing
        .into_iter()
        .map(|entry| {
            (
                entry.asin,
                entry.marketplace,
                entry.kind,
                entry.content_format,
                entry.variant,
            )
        })
        .collect();
    let removed = db.delete_downloads(keys).await?;
    eprintln!("pruned {removed} download records");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file_path: String, file_size: Option<u64>) -> DownloadEntry {
        DownloadEntry {
            asin: "B0TEST".into(),
            marketplace: "de".into(),
            kind: "audio".into(),
            content_format: "AAX_44_128".into(),
            variant: "original".into(),
            file_path,
            file_size,
        }
    }

    /// `check`'s classification: missing file, size mismatch, matching
    /// size, and a NULL recorded size (never a mismatch) — AUD-102.
    #[test]
    fn classify_partitions_missing_and_size_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = tmp.path().join("ok.aaxc");
        let truncated = tmp.path().join("truncated.aaxc");
        let no_size = tmp.path().join("no_size.aaxc");
        std::fs::write(&ok, b"1234").unwrap();
        std::fs::write(&truncated, b"12").unwrap();
        std::fs::write(&no_size, b"anything").unwrap();
        // Sidecars present, so this test stays about existence + size.
        for stem in ["ok", "truncated", "no_size"] {
            std::fs::write(tmp.path().join(format!("{stem}.voucher")), b"k").unwrap();
        }

        let report = classify_downloads(vec![
            entry(ok.display().to_string(), Some(4)),
            entry(truncated.display().to_string(), Some(4)),
            entry(no_size.display().to_string(), None),
            entry(tmp.path().join("gone.aaxc").display().to_string(), Some(4)),
        ]);

        assert_eq!(report.total, 4);
        assert_eq!(report.missing.len(), 1);
        assert!(report.missing[0].file_path.ends_with("gone.aaxc"));
        assert_eq!(report.mismatched.len(), 1);
        assert!(
            report.mismatched[0]
                .entry
                .file_path
                .ends_with("truncated.aaxc")
        );
        assert_eq!(report.mismatched[0].found, 2);
        assert!(report.sidecar_missing.is_empty());
    }

    /// Sidecar detection (AUD-106): an encrypted original without its
    /// `.voucher`/`.wvkey` is flagged; sidecar-less artifacts (m4b, pdf)
    /// never are, and a record can be size-mismatched AND sidecar-less.
    #[test]
    fn classify_flags_missing_key_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let with_key = tmp.path().join("with_key.aaxc");
        let keyless_aaxc = tmp.path().join("keyless.aaxc");
        let keyless_cenc = tmp.path().join("keyless.AAC_44_131.cenc");
        let decrypted = tmp.path().join("decrypted.m4b");
        let truncated_keyless = tmp.path().join("truncated_keyless.aaxc");
        for (path, content) in [
            (&with_key, &b"1234"[..]),
            (&keyless_aaxc, b"1234"),
            (&keyless_cenc, b"1234"),
            (&decrypted, b"1234"),
            (&truncated_keyless, b"12"),
        ] {
            std::fs::write(path, content).unwrap();
        }
        std::fs::write(tmp.path().join("with_key.voucher"), b"k").unwrap();

        let report = classify_downloads(vec![
            entry(with_key.display().to_string(), Some(4)),
            entry(keyless_aaxc.display().to_string(), Some(4)),
            entry(keyless_cenc.display().to_string(), Some(4)),
            entry(decrypted.display().to_string(), Some(4)),
            entry(truncated_keyless.display().to_string(), Some(4)),
        ]);

        assert!(report.missing.is_empty());
        let flagged: Vec<String> = report
            .sidecar_missing
            .iter()
            .map(|item| {
                item.sidecar
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            flagged,
            [
                "keyless.voucher",
                "keyless.AAC_44_131.wvkey",
                "truncated_keyless.voucher"
            ]
        );
        // The truncated keyless record carries both problems.
        assert_eq!(report.mismatched.len(), 1);
        assert!(
            report.mismatched[0]
                .entry
                .file_path
                .ends_with("truncated_keyless.aaxc")
        );
    }
}
