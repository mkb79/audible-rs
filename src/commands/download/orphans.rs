//! `download orphans` — files under `download_dir` that no database record
//! references (AUD-102): leftovers of `db reset`, aborted runs or manual
//! edits. The reverse direction of `db downloads check` (which starts from
//! the records; this starts from the disk). Read-only report by default;
//! `--remove` deletes after confirmation.
//!
//! A `download_dir` shared across accounts must never misclassify another
//! account's files, so the reference set is the union of **all** account
//! databases found in the configured db dir(s), read directly and
//! read-only — no auth material is needed. An unreadable database aborts
//! the scan: never report (or delete) on a partial view.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::{Arg, ArgAction};

use crate::commands::db::{human_size, remove_if_present};
use crate::commands::prompt::confirm;
use crate::config::ctx::Ctx;
use crate::output::Output;

/// `download orphans` — clap.
pub(super) fn orphans_command() -> clap::Command {
    clap::Command::new("orphans")
        .about("Find files in the download dir that no database record references")
        .long_about(
            "Walk the download dir and report files no download or annotation record \
             references (leftovers of `db reset`, aborted runs, manual edits). The records \
             of ALL account databases in the configured db dir(s) count as references, so a \
             download dir shared across accounts never misclassifies the other account's \
             files. `.part` resume files are in-progress artifacts — listed separately, \
             never deleted. A `.voucher`/`.wvkey` key sidecar belongs to its audio record \
             and is only an orphan when that record is gone.",
        )
        .arg(
            Arg::new("remove")
                .long("remove")
                .action(ArgAction::SetTrue)
                .help("Delete the orphaned files (asks for confirmation)"),
        )
        .arg(crate::commands::yes_arg())
}

/// Result of one download-dir walk.
struct Scan {
    /// Unreferenced files: `(path, on-disk size)`, sorted by path.
    orphans: Vec<(PathBuf, Option<u64>)>,
    /// `.part` resume files (in progress — reported, never deleted).
    parts: Vec<PathBuf>,
    /// All regular files seen under the download dir.
    total: usize,
}

/// `download orphans`: report (and with `--remove` delete) unreferenced files.
pub(super) async fn orphans(ctx: &Ctx, args: &clap::ArgMatches) -> Result<()> {
    let remove = args.get_flag("remove");
    let yes = args.get_flag("yes");

    let root = crate::naming::download_dir(ctx)?;
    if !root.is_dir() {
        eprintln!(
            "download dir {} does not exist; nothing to scan",
            root.display()
        );
        return Ok(());
    }

    let db_dirs = db_dirs(ctx);
    let walk_root = root.clone();
    let (scan, databases) = tokio::task::spawn_blocking(move || -> Result<(Scan, usize)> {
        let (referenced, databases) = referenced_paths(&db_dirs)?;
        Ok((
            scan_download_dir(&walk_root, &db_dirs, &referenced)?,
            databases,
        ))
    })
    .await??;

    if !scan.parts.is_empty() {
        eprintln!(
            "{} in-progress .part file(s) skipped (resume data of `download`):",
            scan.parts.len()
        );
        for part in &scan.parts {
            eprintln!("  {}", part.display());
        }
    }

    if scan.orphans.is_empty() {
        eprintln!(
            "no orphans: every file under {} is referenced \
             ({} file(s) checked against {databases} account database(s))",
            root.display(),
            scan.total
        );
        return Ok(());
    }

    ctx.print(&Output::table(
        vec!["size", "orphaned path"],
        scan.orphans
            .iter()
            .map(|(path, size)| vec![human_size(*size), path.display().to_string()])
            .collect(),
    ));
    eprintln!(
        "{} of {} file(s) under {} have no download or annotation record \
         ({databases} account database(s) checked)",
        scan.orphans.len(),
        scan.total,
        root.display()
    );

    if !remove {
        eprintln!("re-run with --remove to delete them");
        return Ok(());
    }
    if !confirm(
        yes,
        &format!("Delete {} orphaned file(s)?", scan.orphans.len()),
    )? {
        eprintln!("aborted; nothing deleted");
        return Ok(());
    }
    let deleted = scan
        .orphans
        .iter()
        .filter(|(path, _)| remove_if_present(path))
        .count();
    eprintln!("deleted {deleted} orphaned file(s)");
    Ok(())
}

/// Every directory that may hold account databases: the built-in default,
/// the global `[db].dir` and each settings bundle's `db.dir` override —
/// whichever bundle an account uses, its database is in one of these.
fn db_dirs(ctx: &Ctx) -> Vec<PathBuf> {
    let config = ctx.config();
    let overrides = std::iter::once(config.db.dir.as_ref())
        .chain(
            config
                .settings
                .values()
                .map(|settings| settings.db.as_ref().and_then(|db| db.dir.as_ref())),
        )
        .flatten();
    let mut dirs = vec![crate::config::paths::data_dir().join("db")];
    for dir in overrides {
        let dir = crate::naming::expand_tilde(dir);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The union of every file path any account database records
/// (`downloads.file_path` + `annotations.file_path` — the only
/// path-recording tables), plus each audio record's derived key sidecar
/// (AUD-99: a sidecar whose audio record exists is owned, not an orphan).
/// Returns the set and the number of databases read; fails when any
/// database cannot be read — a partial view is never safe to act on.
fn referenced_paths(db_dirs: &[PathBuf]) -> Result<(HashSet<PathBuf>, usize)> {
    let mut db_files = Vec::new();
    for dir in db_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).context(format!("could not list db dir {}", dir.display()));
            }
        };
        for entry in entries {
            let path = entry
                .with_context(|| format!("could not list db dir {}", dir.display()))?
                .path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_account_db)
            {
                db_files.push(path);
            }
        }
    }

    let mut referenced = HashSet::new();
    for db_file in &db_files {
        let recorded = recorded_paths(db_file).with_context(|| {
            format!(
                "could not read the records of {} — aborting: an orphan scan on a \
                 partial view could misreport another account's files",
                db_file.display()
            )
        })?;
        for path in recorded {
            let path = PathBuf::from(path);
            if let Some(sidecar) = crate::naming::sidecar_path(&path) {
                insert_with_canonical(&mut referenced, sidecar);
            }
            insert_with_canonical(&mut referenced, path);
        }
    }
    Ok((referenced, db_files.len()))
}

/// All recorded file paths of one account database, read via a direct
/// read-only connection — another account's database is never migrated,
/// locked for writing, or decrypted-auth-dependent.
fn recorded_paths(db_file: &Path) -> Result<Vec<String>> {
    let conn =
        rusqlite::Connection::open_with_flags(db_file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let mut recorded = Vec::new();
    for sql in [
        "SELECT file_path FROM downloads",
        "SELECT file_path FROM annotations WHERE file_path IS NOT NULL",
    ] {
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            recorded.push(row?);
        }
    }
    Ok(recorded)
}

/// Inserts a path and, when it resolves, its canonical form — recorded and
/// walked paths may differ through symlinks or `..` segments.
fn insert_with_canonical(set: &mut HashSet<PathBuf>, path: PathBuf) {
    if let Ok(canonical) = path.canonicalize() {
        set.insert(canonical);
    }
    set.insert(path);
}

/// Walks the download dir and classifies every regular file as referenced,
/// `.part` resume file, or orphan. Never descends into a db dir and never
/// classifies database files themselves (in case `db.dir` sits inside the
/// download dir). Symlinks are left alone entirely.
fn scan_download_dir(
    root: &Path,
    db_dirs: &[PathBuf],
    referenced: &HashSet<PathBuf>,
) -> Result<Scan> {
    let mut scan = Scan {
        orphans: Vec::new(),
        parts: Vec::new(),
        total: 0,
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("could not list {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("could not list {}", dir.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("could not stat {}", entry.path().display()))?;
            let path = entry.path();
            if file_type.is_dir() {
                if db_dirs.iter().any(|db_dir| same_dir(db_dir, &path)) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            scan.total += 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_account_db_artifact(&name) {
                continue;
            }
            if name.ends_with(".part") {
                scan.parts.push(path);
                continue;
            }
            if referenced.contains(&path) {
                continue;
            }
            if let Ok(canonical) = path.canonicalize()
                && referenced.contains(&canonical)
            {
                continue;
            }
            let size = std::fs::metadata(&path).ok().map(|meta| meta.len());
            scan.orphans.push((path, size));
        }
    }
    scan.orphans.sort();
    scan.parts.sort();
    Ok(scan)
}

/// Whether two directory paths refer to the same directory (canonical
/// comparison when both exist).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Whether `name` is an account database file (`account_<16 hex>.sqlite`,
/// see [`crate::db::account_file_name`]).
fn is_account_db(name: &str) -> bool {
    account_db_suffix(name) == Some("")
}

/// Whether `name` is an account database file or one of its WAL/SHM
/// sidecars.
fn is_account_db_artifact(name: &str) -> bool {
    matches!(account_db_suffix(name), Some("" | "-wal" | "-shm"))
}

/// For `account_<16 hex>.sqlite<suffix>` names, the suffix (`""`, `-wal`,
/// `-shm`); `None` for everything else.
fn account_db_suffix(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("account_")?;
    let (hex, rest) = rest.split_at_checked(16)?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    rest.strip_prefix(".sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, DownloadRecord, account_file_name};

    #[test]
    fn account_db_names() {
        let db = account_file_name("amzn1.account.TEST");
        assert!(is_account_db(&db));
        assert!(is_account_db_artifact(&format!("{db}-wal")));
        assert!(is_account_db_artifact(&format!("{db}-shm")));
        assert!(!is_account_db(&format!("{db}-wal")));
        assert!(!is_account_db("account_notahexstring1.sqlite"));
        assert!(!is_account_db("account_0123456789abcdef.sqlite.bak"));
        assert!(!is_account_db("library.sqlite"));
    }

    /// End-to-end over a real account database: recorded paths (downloads +
    /// annotations) and the derived key sidecar are referenced; everything
    /// else under the download dir is an orphan, `.part` files are listed
    /// separately.
    #[tokio::test]
    async fn scan_classifies_orphans_parts_and_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        let dl_dir = tmp.path().join("downloads");
        std::fs::create_dir_all(&dl_dir).unwrap();

        let audio = dl_dir.join("Book.AAX_44_128.aaxc");
        let annot = dl_dir.join("Book.annot");
        for path in [
            &audio,
            &dl_dir.join("Book.AAX_44_128.voucher"), // owned by the audio record
            &annot,
            &dl_dir.join("Stray.m4b"),              // orphan
            &dl_dir.join("Stray.AAC_44_131.wvkey"), // sidecar without a record → orphan
            &dl_dir.join("Resume.aaxc.part"),       // in progress, never cleaned
        ] {
            std::fs::write(path, b"x").unwrap();
        }

        let db_path = db_dir.join(account_file_name("amzn1.account.TEST"));
        let db = Db::open(db_path, 100).await.unwrap();
        db.record_download(
            "de".into(),
            DownloadRecord {
                asin: "B0TEST".into(),
                kind: "audio".into(),
                acr: None,
                content_format: "AAX_44_128".into(),
                variant: "original".into(),
                request_kind: String::new(),
                version: None,
                sku: None,
                file_path: audio.display().to_string(),
                file_size: Some(1),
            },
        )
        .await
        .unwrap();
        db.upsert_annotation("de".into(), "B0TEST".into(), None, "ok".into())
            .await
            .unwrap();
        db.set_annotation_path("de".into(), "B0TEST".into(), annot.display().to_string())
            .await
            .unwrap();
        drop(db);

        let db_dirs = vec![db_dir];
        let (referenced, databases) = referenced_paths(&db_dirs).unwrap();
        assert_eq!(databases, 1);
        let scan = scan_download_dir(&dl_dir, &db_dirs, &referenced).unwrap();

        let orphans: Vec<String> = scan
            .orphans
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(orphans, ["Stray.AAC_44_131.wvkey", "Stray.m4b"]);
        assert_eq!(scan.parts.len(), 1);
        assert!(scan.parts[0].ends_with("Resume.aaxc.part"));
        assert_eq!(scan.total, 6);
    }

    /// An unreadable account database aborts the scan instead of reporting
    /// on a partial view.
    #[test]
    fn unreadable_database_fails_the_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let bogus = db_dir.join(account_file_name("amzn1.account.BAD"));
        std::fs::write(&bogus, b"not a sqlite file").unwrap();

        let error = referenced_paths(&[db_dir]).unwrap_err();
        assert!(error.to_string().contains("partial view"), "{error:#}");
    }
}
