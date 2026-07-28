//! `audible db` — local database maintenance. The
//! `db` group holds operations the regular `library` commands do not
//! cover; table-specific ones are grouped under a table noun
//! (`db downloads …`, `db library …`), whole-database ones stay at the
//! top level: `db downloads list`/`add`/`remove`/`check`/`prune`,
//! `db library remove`, `db info`, `db backup`/`restore`, `db vacuum`,
//! `db check`, `db reset` (archived architecture §12).

use super::prompt::confirm;
use super::strings;
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
                            clap::Command::new("remove")
                                .about(
                                    "Hard-delete library items (episodes and series \
                                     memberships go with them)",
                                )
                                .long_about(
                                    "Hard-delete library items from the local database, together \
                                     with their episodes and series memberships.\n\n\
                                     Their downloads are untouched: the files stay tracked, stay \
                                     reorganizable, and are not reported by `download orphans`. \
                                     To forget a title's downloads — or delete the files — use \
                                     `db downloads remove`.",
                                ),
                        )
                        .group(
                            clap::ArgGroup::new("source")
                                .args(["asin", "title"])
                                .multiple(true)
                                .required(true),
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
                    let has_source = list.contains_id("asin") || list.contains_id("title");
                    downloads_list(
                        ctx,
                        strings(list, "asin"),
                        strings(list, "title"),
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
                    let has_source = remove.contains_id("asin") || remove.contains_id("title");
                    downloads_remove(
                        ctx,
                        strings(remove, "asin"),
                        strings(remove, "title"),
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
                        strings(remove, "asin"),
                        strings(remove, "title"),
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

/// `db library remove` — hard-delete items together with their episodes and
/// series memberships. The library, and nothing else (AUD-217): the download
/// records and the files stay, so they remain tracked, reorganizable, and are
/// not reported as orphans. Forgetting a title's files is `db downloads
/// remove`.
async fn library_remove(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
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
    let prompt = format!("Remove {} item(s) from the library?", resolved.len());
    if !confirm(yes, &prompt)? {
        eprintln!("aborted; nothing removed");
        return Ok(());
    }

    let removal = db.remove_items(marketplace, resolved).await?;
    for asin in &removal.missing_asins {
        eprintln!("warning: {asin} is not in the database");
    }
    eprintln!(
        "removed {} item(s) and {} episode(s) from the library; their downloads \
         are untouched (`db downloads remove` forgets those)",
        removal.removed_asins.len(),
        removal.episodes_removed,
    );
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

/// Takes the database's cross-process write lock — the same
/// `<db>.lock` a running `library sync` holds (D9; audit 2026-07-18, D4)
/// — for the length of a destructive operation.
///
/// Where an auto-sync losing this race skips quietly, a `reset` or
/// `restore` losing it is an error: deleting or overwriting a database
/// another process is writing must never happen, least of all quietly.
/// The lock file itself is never deleted — unlinking it would not stop
/// the holder (it keeps the lock on the unlinked inode) but would let
/// the next process create and lock a fresh one, so two processes would
/// both believe they hold the database.
///
/// **What this does not cover:** only the sync pair takes this lock, and
/// even they take it *after* opening (and migrating) the database, so a
/// live sync connection can exist before its lock attempt. Every other
/// command opens the database and relies on SQLite's own locking, so a
/// `db downloads add` or a `download` run in another process is
/// invisible here and can still lose its writes to a reset (which is
/// what the user asked for) or, worse, keep writing into a database that
/// `restore` has just replaced. Closing that hole needs a lock held for
/// the lifetime of every connection, not just by syncs — tracked as its
/// own issue.
fn lock_for_destructive_op(path: &Path) -> Result<fd_lock::RwLock<std::fs::File>> {
    crate::fsutil::write_lock(path)
        .with_context(|| format!("could not open the lock file next to {}", path.display()))
}

/// Why a destructive operation gave up before touching anything. A lost
/// race reads differently from a broken lock file, so the underlying
/// error is not thrown away.
fn database_busy(path: &Path, error: &std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        anyhow::anyhow!(
            "another audible process is using {} (a library sync may be running); \
             nothing was changed — try again once it is done",
            path.display()
        )
    } else {
        anyhow::anyhow!(
            "could not lock {} ({error}); nothing was changed",
            path.display()
        )
    }
}

/// The database's own files that exist right now — the database and its
/// `-wal`/`-shm` sidecars. A sidecar can outlive its database (a previous
/// reset that only got half-way), and leaving those behind would hand the
/// next database a foreign write-ahead log.
fn existing_database_files(path: &Path) -> Vec<PathBuf> {
    [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ]
    .into_iter()
    .filter(|file| file.exists())
    .collect()
}

/// `db reset` — delete the database and its sidecars after confirmation.
async fn db_reset(ctx: &Ctx, yes: bool) -> Result<()> {
    let path = ctx.library_db_path().await?;
    if existing_database_files(&path).is_empty() {
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

    // Only now, so no prompt waits with the lock held — and the check
    // above is repeated under it, since anything could have happened
    // while the question sat on screen.
    let mut lock = lock_for_destructive_op(&path)?;
    let _guard = lock
        .try_write()
        .map_err(|error| database_busy(&path, &error))?;
    if existing_database_files(&path).is_empty() {
        eprintln!("no database at {}", path.display());
        return Ok(());
    }

    remove_database_files(&path)?;
    eprintln!("database deleted; the next `library sync` creates a fresh one");
    Ok(())
}

/// Deletes a database and its `-wal`/`-shm` sidecars, reporting what it
/// could not delete.
///
/// A file that is already gone is a no-op, but any other failure is an
/// error — on Windows a database another program still holds refuses
/// deletion with a sharing violation, and answering that with a warning
/// plus "database deleted" and exit code 0 would tell a script the data
/// is gone while every row is still there.
fn remove_database_files(path: &Path) -> Result<()> {
    let mut failed = Vec::new();
    for victim in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if let Err(error) = std::fs::remove_file(&victim)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("{}: {error}", victim.display()));
        }
    }
    if !failed.is_empty() {
        bail!(
            "the database was not fully deleted — could not remove:\n  {}\n\
             Another program may still have it open.",
            failed.join("\n  ")
        );
    }
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
/// the target's stale `-wal`/`-shm` so no old WAL is applied on top.
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

    let mut lock = lock_for_destructive_op(&dest)?;
    let _guard = lock
        .try_write()
        .map_err(|error| database_busy(&dest, &error))?;

    install_snapshot(src, &dest)
        .await
        .with_context(|| format!("could not restore {} from {source}", dest.display()))?;

    eprintln!("restored database from {source}");
    Ok(())
}

/// Puts `src` in place as the database at `dest`, with the old
/// database's `-wal`/`-shm` gone and nothing half-done in between.
///
/// The order is what makes this safe:
///
/// 1. **Stage.** The snapshot is copied to a sibling temp file and
///    fsynced. Copying straight onto `dest` would truncate it before
///    writing a byte, so an I/O error half-way would leave a torn
///    database where a working one was — and it would lose to a source
///    that is a *hard link* to `dest` (which `same_file` cannot see, as
///    it compares paths, not inodes): truncating the destination
///    truncates the source with it, and the "restore" writes zero bytes.
/// 2. **Quarantine.** The old `-wal`/`-shm` are moved aside, not deleted.
///    They must be gone before the snapshot is published, never after:
///    a database paired with a foreign write-ahead log is what SQLite
///    calls a hot journal, and it is corruption, not an inconvenience.
///    Failing here leaves everything as it was.
/// 3. **Publish.** One rename replaces `dest` with the complete staged
///    file. If it fails, the quarantined sidecars go back and the
///    database is exactly as before.
/// 4. **Clean up.** Only now are the quarantined files deleted. They sit
///    under temp names by then, so even failing to remove them cannot
///    pair a stale log with a database.
///
/// What this order does *not* buy: killed between 2 and 3, the old
/// database is left without its own write-ahead log, losing whatever it
/// had not checkpointed. That is the deliberate trade — the alternative
/// window (a published snapshot paired with the previous database's log)
/// is corruption, and this one is a consistent database that lost the
/// last commits of a database the user was replacing anyway. Closing it
/// entirely would need a recovery journal, which a rebuildable local
/// cache does not warrant.
///
/// Nor does it help against a process that opens the *old* database
/// between 2 and 3 and writes a fresh log for it: that log then belongs
/// to a database that no longer exists. Nothing here can tell such a
/// file apart from the log of a process that legitimately opened the
/// *restored* database a moment later, so this does not guess — the
/// cure is to exclude those openers in the first place, which is the
/// lock gap named in [`lock_for_destructive_op`].
async fn install_snapshot(src: &Path, dest: &Path) -> Result<()> {
    let source = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("could not open {}", src.display()))?;
    let permissions = source
        .metadata()
        .await
        .with_context(|| format!("could not read the mode of {}", src.display()))?
        .permissions();

    let staged = crate::fsutil::unique_tmp_path(dest);
    // Created before anything else, and only cleaned up from here on:
    // if this very call did not create the file, it belongs to someone
    // else and must not be deleted on the way out. On Unix it is born
    // with the snapshot's mode — setting it afterwards would leave a
    // private database readable to everyone for the length of the copy.
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        options.mode(permissions.mode());
    }
    let target = options
        .open(&staged)
        .await
        .with_context(|| format!("could not create {}", staged.display()))?;

    if let Err(error) = fill_staged(source, target, &staged, &permissions).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error);
    }

    let quarantined = match quarantine_sidecars(dest).await {
        Ok(quarantined) => quarantined,
        Err(error) => {
            let _ = tokio::fs::remove_file(&staged).await;
            return Err(error.context(format!(
                "could not move the old database's sidecar(s) out of the way; \
                 {} was not replaced",
                dest.display()
            )));
        }
    };
    // Make the quarantine durable before publishing. Without this barrier
    // the two renames can reach the disk out of order, and a power loss in
    // between would leave the restored database beside the old
    // write-ahead log — the one pairing this whole order exists to
    // prevent. Best-effort, like every directory sync here.
    crate::fsutil::sync_parent_dir(dest);

    if let Err(error) = tokio::fs::rename(&staged, dest).await {
        let mut stranded = Vec::new();
        for (original, moved) in &quarantined {
            if let Err(error) = tokio::fs::rename(moved, original).await {
                stranded.push(format!(
                    "{} is at {}: {error}",
                    original.display(),
                    moved.display()
                ));
            }
        }
        let _ = tokio::fs::remove_file(&staged).await;
        crate::fsutil::sync_parent_dir(dest);
        // Saying "unchanged" would be a lie if a sidecar did not make it
        // back, so that case gets its own answer.
        if !stranded.is_empty() {
            bail!(
                "could not put the snapshot in place as {} ({error}), and the old \
                 database's sidecar(s) did not make it back:\n  {}\n\
                 The snapshot was not installed; move those file(s) back under their \
                 original names before using the database.",
                dest.display(),
                stranded.join("\n  ")
            );
        }
        return Err(error).with_context(|| {
            format!(
                "could not put the snapshot in place as {} (the database is unchanged)",
                dest.display()
            )
        });
    }

    // Only now — a read-only staged file could not have been deleted on
    // the way out of a failed restore (Windows refuses), so the attribute
    // goes on the published database instead. Unix already carries the
    // exact mode from [`fill_staged`].
    #[cfg(not(unix))]
    if permissions.readonly() {
        tokio::fs::set_permissions(dest, permissions.clone())
            .await
            .with_context(|| format!("could not set the mode of {}", dest.display()))?;
    }

    for (_, moved) in &quarantined {
        if let Err(error) = tokio::fs::remove_file(moved).await {
            eprintln!(
                "warning: could not delete the old {}: {error}",
                moved.display()
            );
        }
    }
    crate::fsutil::sync_parent_dir(dest);
    Ok(())
}

/// Fills the already-created staged file from the snapshot and syncs it,
/// so the rename that follows can only publish a complete file.
///
/// Everything goes through the handle the caller created: Windows
/// refuses to flush a read-only one, so copying and reopening to sync
/// would fail every restore there.
///
/// On Unix the mode is applied twice, and both times matter: at creation
/// (by the caller) so a private snapshot is never briefly world-readable
/// while it streams in, and again here so the published file carries the
/// snapshot's exact mode — the creation mode is filtered through the
/// process umask, which would otherwise turn a `0644` snapshot into
/// `0600` under a restrictive one. A snapshot's Windows ACL is *not*
/// carried over the way `fs::copy` would: the staged file inherits the
/// database directory's, which for a library cache holding no secrets is
/// the acceptable side of not hand-rolling `CopyFileEx`.
async fn fill_staged(
    mut source: tokio::fs::File,
    mut target: tokio::fs::File,
    staged: &Path,
    permissions: &std::fs::Permissions,
) -> Result<()> {
    tokio::io::copy(&mut source, &mut target)
        .await
        .with_context(|| format!("could not copy the snapshot to {}", staged.display()))?;
    // A read-only snapshot must not make the *staged* file read-only:
    // Windows would then refuse to delete it on the way out of a failed
    // restore, stranding a temp file. The attribute is applied to the
    // published database instead (see [`install_snapshot`]).
    #[cfg(unix)]
    target
        .set_permissions(permissions.clone())
        .await
        .with_context(|| format!("could not set the mode of {}", staged.display()))?;
    #[cfg(not(unix))]
    let _ = permissions;
    target
        .sync_all()
        .await
        .with_context(|| format!("could not flush {} to disk", staged.display()))?;
    Ok(())
}

/// Moves the database's `-wal`/`-shm` aside, returning
/// `(original, moved)` for each one that was there.
///
/// Failing part-way undoes what it already moved, so the database keeps
/// its own sidecars. When even that undoing fails, the error says which
/// file is where instead of letting the caller report "unchanged" — a
/// database quietly missing its write-ahead log is exactly the kind of
/// thing nobody discovers until the rows are gone.
async fn quarantine_sidecars(dest: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for suffix in ["-wal", "-shm"] {
        let side = sidecar(dest, suffix);
        let aside = crate::fsutil::unique_tmp_path(&side);
        match tokio::fs::rename(&side, &aside).await {
            Ok(()) => moved.push((side, aside)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let mut stranded = Vec::new();
                for (original, aside) in &moved {
                    if let Err(error) = tokio::fs::rename(aside, original).await {
                        stranded.push(format!(
                            "{} is at {}: {error}",
                            original.display(),
                            aside.display()
                        ));
                    }
                }
                if !stranded.is_empty() {
                    bail!(
                        "could not move {} ({error}), and putting the already-moved \
                         sidecar(s) back failed too:\n  {}\n\
                         The database was not replaced; move those file(s) back under \
                         their original names before using it.",
                        side.display(),
                        stranded.join("\n  ")
                    );
                }
                return Err(error).with_context(|| format!("could not move {}", side.display()));
            }
        }
    }
    Ok(moved)
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
        vec![
            "asin",
            "mp",
            "kind",
            "variant",
            "format",
            "size",
            path_header,
        ],
        entries
            .iter()
            .map(|entry| {
                let size = entry
                    .file_size
                    .map(|n| indicatif::BinaryBytes(n).to_string())
                    .unwrap_or_else(|| "-".to_owned());
                vec![
                    entry.asin.clone(),
                    entry.marketplace.clone(),
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

/// The marketplace set to narrow `db downloads` matching to, but only
/// when the user explicitly selected one (`-m`/`AUDIBLE_MARKETPLACE`) —
/// without a selector these commands deliberately span the whole
/// per-account database.
fn explicit_marketplaces(ctx: &Ctx) -> Result<Option<std::collections::HashSet<String>>> {
    if ctx.marketplace_selector().is_none() {
        return Ok(None);
    }
    Ok(Some(ctx.marketplaces()?.into_iter().collect()))
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
/// The asin/marketplace/kind/format/variant selection shared by
/// `db downloads list` and `remove` (audit 2026-07-18, D1): the preview
/// (`list`) and the destructive action (`remove`) must resolve the
/// **identical** set — two copies of this pipeline would let a filter
/// change delete a different set than was just shown. `remove` filters on
/// `--format`; `list` has no such flag and passes `None`. Empty-result
/// handling differs per caller (list notes, remove bails), so it stays
/// with them.
///
/// Title resolution (the asin filter) is single-marketplace; an explicit
/// `-m` also narrows the records (A15). Without either, the whole
/// per-account database is in scope.
/// The item selection for `db downloads list`/`remove`: `--asin`/`--title`
/// (with `has_source` marking whether any was given) plus the column
/// filters. `remove` uses `format`; `list` leaves it `None`.
#[derive(Default)]
struct DownloadSelection {
    asins: Vec<String>,
    titles: Vec<String>,
    has_source: bool,
    kind: Option<String>,
    format: Option<String>,
    variant: Option<String>,
}

async fn select_downloads(
    ctx: &Ctx,
    db: &crate::db::Db,
    selection: DownloadSelection,
) -> Result<Vec<DownloadEntry>> {
    let asin_filter: Option<std::collections::HashSet<String>> = if selection.has_source {
        let marketplace = ctx.marketplace_single()?;
        let resolved = crate::commands::items::resolve_asins(
            db,
            &marketplace,
            selection.asins,
            selection.titles,
            crate::commands::items::PodcastMode::Episodes,
        )
        .await?;
        // D7: a named selection that resolves to nothing fails.
        crate::commands::items::require_nonempty(&resolved, "items")?;
        Some(resolved.into_iter().collect())
    } else {
        None
    };

    let marketplace_filter = explicit_marketplaces(ctx)?;
    let mut entries = db.download_entries().await?;
    entries.retain(|entry| {
        asin_filter
            .as_ref()
            .is_none_or(|set| set.contains(&entry.asin))
            && marketplace_filter
                .as_ref()
                .is_none_or(|set| set.contains(&entry.marketplace))
            && selection.kind.as_ref().is_none_or(|k| &entry.kind == k)
            && selection
                .format
                .as_ref()
                .is_none_or(|f| &entry.content_format == f)
            && selection
                .variant
                .as_ref()
                .is_none_or(|v| &entry.variant == v)
    });
    Ok(entries)
}

async fn downloads_list(
    ctx: &Ctx,
    asins: Vec<String>,
    titles: Vec<String>,
    has_source: bool,
    kind: Option<String>,
    variant: Option<String>,
) -> Result<()> {
    let db = ctx.open_library_db().await?;
    let entries = select_downloads(
        ctx,
        &db,
        DownloadSelection {
            asins,
            titles,
            has_source,
            kind,
            variant,
            ..Default::default()
        },
    )
    .await?;

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

    // The same selection the user would preview with `db downloads list`
    // (D1) — an explicit -m narrows what gets removed (A15): matching was
    // by ASIN across the whole per-account database, so with the same
    // title on de and us a `remove --asin X -m de --with-files` also
    // deleted the us record and file, invisibly.
    let matched = select_downloads(
        ctx,
        &db,
        DownloadSelection {
            asins,
            titles,
            has_source,
            kind,
            format,
            variant,
        },
    )
    .await?;

    if matched.is_empty() {
        // D7: remove always names its selection (a filter is required), so
        // matching nothing is an error scripts can see.
        bail!("no tracked downloads match the given filters — nothing removed");
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

    /// Creates a database file with both sidecars and the sync lock file
    /// next to it; returns the tempdir (keep it alive) and the db path.
    fn database_with_sidecars() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("account_0123456789abcdef.sqlite");
        for path in [
            db.clone(),
            sidecar(&db, "-wal"),
            sidecar(&db, "-shm"),
            db.with_extension("lock"),
        ] {
            std::fs::write(path, b"x").unwrap();
        }
        (tmp, db)
    }

    /// A reset takes the database and both sidecars — and leaves the sync
    /// lock file alone. Deleting it would not stop a process holding it
    /// (the lock lives on the unlinked inode) but would let the next one
    /// lock a fresh file, so two processes would both think the database
    /// is theirs.
    #[test]
    fn reset_removes_the_database_but_never_the_lock_file() {
        let (_tmp, db) = database_with_sidecars();

        remove_database_files(&db).unwrap();

        assert!(!db.exists());
        assert!(!sidecar(&db, "-wal").exists());
        assert!(!sidecar(&db, "-shm").exists());
        assert!(db.with_extension("lock").exists(), "sync lock file deleted");
    }

    /// Sidecars that are already gone are a no-op, not a failure.
    #[test]
    fn reset_tolerates_missing_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("account_0123456789abcdef.sqlite");
        std::fs::write(&db, b"x").unwrap();

        remove_database_files(&db).unwrap();
        assert!(!db.exists());
    }

    /// A database that cannot be deleted is an error, not a warning
    /// followed by "database deleted" and exit code 0 — a script must
    /// never be told the data is gone while every row is still there.
    #[test]
    fn an_undeletable_database_fails_the_reset() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory in the database's place: `remove_file` refuses it on
        // every platform, standing in for the sharing violation Windows
        // raises while another program still holds the file.
        let db = tmp.path().join("account_0123456789abcdef.sqlite");
        std::fs::create_dir(&db).unwrap();

        let error = remove_database_files(&db).unwrap_err();

        assert!(
            error.to_string().contains("was not fully deleted"),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("account_0123456789abcdef.sqlite"),
            "{error:#}"
        );
        assert!(db.exists(), "the database must survive a failed reset");
    }

    /// A sidecar can outlive its database — a reset that removed the main
    /// file but failed on the `-wal`. Re-running must clean the remains
    /// instead of reporting "no database" and leaving a foreign
    /// write-ahead log for the next one.
    #[test]
    fn a_stranded_sidecar_is_still_the_database() {
        let (_tmp, db) = database_with_sidecars();
        std::fs::remove_file(&db).unwrap();

        assert_eq!(
            existing_database_files(&db),
            vec![sidecar(&db, "-wal"), sidecar(&db, "-shm")]
        );
        remove_database_files(&db).unwrap();
        assert!(existing_database_files(&db).is_empty());
    }

    /// A destructive command refuses a database another process holds —
    /// the same `<db>.lock` a running `library sync` takes. Two separate
    /// descriptors conflict through the OS the same way two processes do,
    /// which is what `fd-lock` builds on.
    #[test]
    fn a_held_database_refuses_a_destructive_op() {
        let (_tmp, db) = database_with_sidecars();

        let mut running_sync = lock_for_destructive_op(&db).unwrap();
        let _held = running_sync.write().unwrap();

        let mut reset = lock_for_destructive_op(&db).unwrap();
        let error = reset.try_write().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            database_busy(&db, &error)
                .to_string()
                .contains("nothing was changed")
        );
    }

    /// A lock that fails for a reason other than contention says so,
    /// instead of blaming a process that is not there.
    #[test]
    fn a_broken_lock_is_not_reported_as_contention() {
        let path = Path::new("/db/account_0123456789abcdef.sqlite");
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

        let message = database_busy(path, &error).to_string();
        assert!(message.contains("could not lock"), "{message}");
        assert!(!message.contains("another audible process"), "{message}");
    }

    /// The snapshot is staged and renamed, never copied onto the live
    /// file: a source that is a hard link to the database restores its
    /// real content instead of truncating both to nothing.
    #[tokio::test]
    async fn a_hard_linked_snapshot_does_not_zero_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("account_0123456789abcdef.sqlite");
        std::fs::write(&db, b"SQLite format 3\0payload").unwrap();
        let snapshot = tmp.path().join("snapshot.sqlite");
        std::fs::hard_link(&db, &snapshot).unwrap();

        install_snapshot(&snapshot, &db).await.unwrap();

        assert_eq!(std::fs::read(&db).unwrap(), b"SQLite format 3\0payload");
    }

    /// A restore that fails while copying leaves the database and its
    /// sidecars exactly as they were, and no staged leftovers behind.
    ///
    /// On Unix a directory opens as a file and fails on the first read,
    /// so the failure lands *after* the staged file exists — the case
    /// worth covering. Windows refuses the open outright, so there the
    /// test only proves the earlier path; the staged cleanup itself is
    /// pinned by mutation on Unix.
    #[tokio::test]
    async fn a_failed_restore_leaves_the_database_untouched() {
        let (_tmp, db) = database_with_sidecars();
        std::fs::write(&db, b"the original database").unwrap();
        let broken = db.parent().unwrap().join("broken.sqlite");
        std::fs::create_dir(&broken).unwrap();

        assert!(install_snapshot(&broken, &db).await.is_err());

        assert_eq!(std::fs::read(&db).unwrap(), b"the original database");
        assert!(sidecar(&db, "-wal").exists(), "the old WAL was taken");
        assert!(sidecar(&db, "-shm").exists(), "the old shm was taken");
        assert!(staged_leftovers(db.parent().unwrap()).is_empty());
    }

    /// A successful restore replaces the database and takes the old
    /// sidecars with it — a restored database must never be paired with
    /// the previous one's write-ahead log.
    #[tokio::test]
    async fn a_restore_removes_the_old_sidecars_and_leaves_no_leftovers() {
        let (_tmp, db) = database_with_sidecars();
        let snapshot = db.parent().unwrap().join("snapshot.sqlite");
        std::fs::write(&snapshot, b"the snapshot").unwrap();

        install_snapshot(&snapshot, &db).await.unwrap();

        assert_eq!(std::fs::read(&db).unwrap(), b"the snapshot");
        assert!(!sidecar(&db, "-wal").exists());
        assert!(!sidecar(&db, "-shm").exists());
        assert!(db.with_extension("lock").exists(), "sync lock file deleted");
        assert!(staged_leftovers(db.parent().unwrap()).is_empty());
    }

    /// The restored database keeps the snapshot's mode exactly — a plain
    /// `fs::copy` carried it, a freshly created staged file would not, so
    /// a 0600 snapshot must not be published as 0644 and a 0644 one must
    /// not arrive as 0600.
    ///
    /// The permissive case is what the creation mode alone cannot
    /// guarantee: it is filtered through the process umask. Under the
    /// usual 022 this test passes either way; under a restrictive umask
    /// it only passes because the mode is applied again after the copy.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_restore_keeps_the_snapshots_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        for mode in [0o600, 0o644] {
            let (tmp, db) = database_with_sidecars();
            let snapshot = tmp.path().join("snapshot.sqlite");
            std::fs::write(&snapshot, b"the snapshot").unwrap();
            std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(mode)).unwrap();

            install_snapshot(&snapshot, &db).await.unwrap();

            let published = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                published, mode,
                "{published:o} published for a {mode:o} snapshot"
            );
        }
    }

    /// When publishing the snapshot fails, the sidecars that were moved
    /// aside come back: the old database keeps its own write-ahead log
    /// instead of being left without it.
    #[tokio::test]
    async fn a_failed_publish_puts_the_sidecars_back() {
        let (tmp, db) = database_with_sidecars();
        let snapshot = tmp.path().join("snapshot.sqlite");
        std::fs::write(&snapshot, b"the snapshot").unwrap();
        // A directory cannot be renamed over: the publish step fails
        // after the sidecars have already been moved aside.
        let blocked = tmp.path().join("account_fedcba9876543210.sqlite");
        std::fs::create_dir(&blocked).unwrap();
        for suffix in ["-wal", "-shm"] {
            std::fs::write(sidecar(&blocked, suffix), b"old log").unwrap();
        }

        assert!(install_snapshot(&snapshot, &blocked).await.is_err());

        for suffix in ["-wal", "-shm"] {
            let side = sidecar(&blocked, suffix);
            assert!(side.exists(), "{} was not put back", side.display());
            assert_eq!(std::fs::read(&side).unwrap(), b"old log");
        }
        assert!(staged_leftovers(tmp.path()).is_empty());
        let _ = db;
    }

    /// Temp files of an interrupted staging or quarantine step.
    fn staged_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect()
    }
}
