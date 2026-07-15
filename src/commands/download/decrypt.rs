//! Decrypt a downloaded aaxc to a playable m4b (AUD-27), losslessly, via an
//! external tool invoked as a **subprocess** (both tools are GPL-3.0; mere
//! aggregation keeps this MIT crate unaffected — never link/bundle them).
//!
//! Two backends: **aaxclean-cli** (Mbucari, purpose-built, ~2.8× faster than
//! ffmpeg in our benchmark) and **ffmpeg** (`-audible_key/-audible_iv` need
//! ≥ 4.4). `Auto` prefers aaxclean-cli and falls back to ffmpeg. Tool
//! discovery: `AUDIBLE_AAXCLEAN_CLI` / `AUDIBLE_FFMPEG` env override → `PATH`.
//!
//! The content key/iv are passed on the tool's argv but never logged by us.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::config::ctx::Ctx;
use crate::config::schema::DecryptBackend;
use crate::db::DownloadRecord;

/// A resolved decrypt tool (its executable path).
pub(super) enum Tool {
    Aaxclean(PathBuf),
    Ffmpeg(PathBuf),
}

impl Tool {
    /// Human label for messages.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Tool::Aaxclean(_) => "aaxclean-cli",
            Tool::Ffmpeg(_) => "ffmpeg",
        }
    }
}

/// Selects the decrypt tool for `backend`, discovering it up front so a
/// missing/too-old tool fails before any download starts.
pub(super) async fn select(backend: DecryptBackend) -> Result<Tool> {
    match backend {
        DecryptBackend::Aaxclean => aaxclean_path()
            .map(Tool::Aaxclean)
            .with_context(|| format!("{NO_AAXCLEAN}{INSTALL_HINT}")),
        DecryptBackend::Ffmpeg => usable_ffmpeg().await,
        DecryptBackend::Auto => {
            if let Some(path) = aaxclean_path() {
                Ok(Tool::Aaxclean(path))
            } else if ffmpeg_path().is_some() {
                usable_ffmpeg().await
            } else {
                bail!(
                    "no decrypt tool found — install aaxclean-cli or ffmpeg (≥ 4.4), \
                     or set AUDIBLE_AAXCLEAN_CLI / AUDIBLE_FFMPEG{INSTALL_HINT}"
                )
            }
        }
    }
}

const NO_AAXCLEAN: &str =
    "aaxclean-cli not found — install it or set AUDIBLE_AAXCLEAN_CLI to its path";

/// Where to obtain a decrypt tool. Empty off Windows, where "install ffmpeg /
/// aaxclean-cli" is already actionable; on Windows the tools are less obvious,
/// so point at winget and the projects' releases.
#[cfg(windows)]
const INSTALL_HINT: &str = " (on Windows: `winget install ffmpeg`, or a gyan.dev \
     build; aaxclean-cli from its GitHub releases)";
#[cfg(not(windows))]
const INSTALL_HINT: &str = "";

/// ffmpeg, but only if it supports the aaxc demuxer (`-audible_key`, ≥ 4.4).
async fn usable_ffmpeg() -> Result<Tool> {
    let path = ffmpeg_path().with_context(|| {
        format!("ffmpeg not found — install it or set AUDIBLE_FFMPEG to its path{INSTALL_HINT}")
    })?;
    match ffmpeg_version(&path).await {
        // Unparseable version (e.g. a git build) → assume recent enough.
        None => Ok(Tool::Ffmpeg(path)),
        Some((major, minor)) if (major, minor) >= (4, 4) => Ok(Tool::Ffmpeg(path)),
        Some((major, minor)) => bail!(
            "ffmpeg {major}.{minor} is too old for aaxc (needs ≥ 4.4 for -audible_key/-audible_iv); \
             use aaxclean-cli or a newer ffmpeg"
        ),
    }
}

fn aaxclean_path() -> Option<PathBuf> {
    tool_path("AUDIBLE_AAXCLEAN_CLI", "aaxclean-cli")
}

fn ffmpeg_path() -> Option<PathBuf> {
    tool_path("AUDIBLE_FFMPEG", "ffmpeg")
}

/// Resolves a tool path: `env_var` override (must exist) → first match on `PATH`.
fn tool_path(env_var: &str, name: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(env_var) {
        let path = PathBuf::from(value);
        return is_executable(&path).then_some(path);
    }
    which(name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| executable_candidates(&dir, name))
        .find(|candidate| is_executable(candidate))
}

/// The filenames to try for `name` in one PATH directory. On Unix a program is
/// its bare name; on Windows an executable carries an extension, so a bare
/// `ffmpeg` never matches `ffmpeg.exe` — try each `PATHEXT` extension (unless
/// the caller already gave one).
#[cfg(not(windows))]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

#[cfg(windows)]
fn executable_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    if Path::new(name).extension().is_some() {
        return vec![dir.join(name)];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    pathext
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| dir.join(format!("{name}{ext}")))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Parses `<ffmpeg> -version` into `(major, minor)`; `None` if unparseable.
async fn ffmpeg_version(path: &Path) -> Option<(u32, u32)> {
    let output = tokio::process::Command::new(path)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // First line: "ffmpeg version 5.1.9-0+deb12u1 Copyright ...".
    let token = text.split_whitespace().nth(2)?;
    let digits = token.trim_start_matches(['n', 'N']);
    let mut parts = digits.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Decrypts `aaxc` to `out` (lossless m4b) using the aaxc content `key`/`iv`.
/// The m4b is written moov-first (faststart) like the source aaxc, so it
/// plays before it is fully transferred (HTTP streaming, e.g. Audiobookshelf).
/// Removes a partial output on failure; surfaces the tool's error (never the
/// key/iv, which are only on argv).
pub(super) async fn run(tool: &Tool, aaxc: &Path, key: &str, iv: &str, out: &Path) -> Result<()> {
    let mut cmd = match tool {
        Tool::Aaxclean(path) => {
            let mut cmd = tokio::process::Command::new(path);
            cmd.arg("-f")
                .arg(aaxc)
                .arg("--audible_key")
                .arg(key)
                .arg("--audible_iv")
                .arg(iv)
                .arg("--moov_faststart")
                .arg("-o")
                .arg(out);
            cmd
        }
        Tool::Ffmpeg(path) => {
            let mut cmd = tokio::process::Command::new(path);
            cmd.arg("-y")
                .arg("-nostdin")
                .arg("-loglevel")
                .arg("error")
                .arg("-audible_key")
                .arg(key)
                .arg("-audible_iv")
                .arg(iv)
                .arg("-i")
                .arg(aaxc)
                .arg("-c")
                .arg("copy")
                .arg("-map_metadata")
                .arg("0")
                .arg("-movflags")
                .arg("+faststart")
                .arg(out);
            cmd
        }
    };
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("could not run {}", tool.label()))?;

    if !output.status.success() {
        let _ = tokio::fs::remove_file(out).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = pick_error(stderr.trim(), stdout.trim());
        bail!("{} failed ({}){detail}", tool.label(), output.status);
    }
    Ok(())
}

/// Decrypts a CENC-encrypted MP4 (`input`) to `out` (lossless) using the
/// Widevine content `kid`/`key` (both 32-hex-char / 16 bytes). The output is
/// written moov-first (faststart), as in [`run`]. Removes a partial
/// output on failure; the key is only on argv, never logged by us. Wired into
/// the Widevine download flow in AUD-56e.
#[allow(dead_code)]
pub(super) async fn run_cenc(
    tool: &Tool,
    input: &Path,
    kid: &str,
    key: &str,
    out: &Path,
) -> Result<()> {
    let mut cmd = match tool {
        Tool::Aaxclean(path) => {
            let mut cmd = tokio::process::Command::new(path);
            cmd.arg("-f")
                .arg(input)
                .arg("--encryption_kid")
                .arg(kid)
                .arg("--encryption_key")
                .arg(key)
                .arg("--moov_faststart")
                .arg("-o")
                .arg(out);
            cmd
        }
        Tool::Ffmpeg(path) => {
            let mut cmd = tokio::process::Command::new(path);
            // `-decryption_key` is an input (demuxer) option → before `-i`.
            cmd.arg("-y")
                .arg("-nostdin")
                .arg("-loglevel")
                .arg("error")
                .arg("-decryption_key")
                .arg(key)
                .arg("-i")
                .arg(input)
                .arg("-c")
                .arg("copy")
                .arg("-map_metadata")
                .arg("0")
                .arg("-movflags")
                .arg("+faststart")
                .arg(out);
            cmd
        }
    };
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("could not run {}", tool.label()))?;
    if !output.status.success() {
        let _ = tokio::fs::remove_file(out).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = pick_error(stderr.trim(), stdout.trim());
        bail!("{} failed ({}){detail}", tool.label(), output.status);
    }
    Ok(())
}

/// Runs the decrypt step for one item (AUD-27): turns its downloaded audio
/// into a playable file recorded under the `decrypted` kind. Returns
/// `((\"decrypted\", path), audio_superseded)` — `audio_superseded` is true
/// when the obsolete `audio` record was dropped (the caller drops it from the
/// run summary too). `None` when there is nothing to do.
///
/// Two sources of truth, per audio source (AUD-94): for audio downloaded
/// *this run*, this run's license is authoritative (`audio_format` +
/// `request_kind` describe the fresh download). For a *previously recorded*
/// aaxc, its `downloads` row is authoritative for both — this run's license
/// may belong to a newer encode, and the run's `request_kind` is empty when
/// the audio target was skipped. The file name is only a last-resort format
/// fallback.
#[allow(clippy::too_many_arguments)]
pub(super) async fn decrypt_item(
    ctx: &Ctx,
    marketplace: &str,
    tool: &Tool,
    asin: &str,
    downloaded: Option<&Path>,
    audio_format: Option<String>,
    request_kind: &str,
    keep_source: bool,
    force: bool,
    no_db_write: bool,
) -> Result<Option<((String, String), bool)>> {
    // The audio file: freshly downloaded this run, else the recorded aaxc
    // (whose row supplies the authoritative content_format + request_kind).
    // `--no-db-write` never falls back to a recorded aaxc: the record points
    // into the managed download_dir and the decrypt would write the m4b next
    // to it, breaking the isolation promise. (Unreachable in practice — with
    // the record skip disabled the audio is always downloaded this run.)
    let (audio, request_kind, recorded_format) = match downloaded {
        Some(path) => (path.to_path_buf(), request_kind.to_owned(), None),
        None if no_db_write => return Ok(None),
        None => match recorded_audio(ctx, marketplace, asin).await {
            Some(record) => (
                PathBuf::from(&record.file_path),
                record.request_kind,
                Some(record.content_format),
            ),
            None => return Ok(None),
        },
    };
    if !audio.exists() {
        eprintln!(
            "{asin}: cannot decrypt — {} is not on disk (re-run with --kind audio)",
            audio.display()
        );
        return Ok(None);
    }
    let format = match recorded_format {
        Some(format) => format,
        None => audio_format.unwrap_or_else(|| aaxc_format(&audio)),
    };

    match audio.extension().and_then(|e| e.to_str()) {
        Some("aaxc") => {
            decrypt_aaxc(
                ctx,
                marketplace,
                tool,
                asin,
                &audio,
                &format,
                &request_kind,
                keep_source,
                force,
                no_db_write,
            )
            .await
        }
        // Plain mp3/m4a is already the playable, DRM-free file and was recorded
        // as (audio, original) by the download — nothing to decrypt. Both are
        // already covered by `db downloads list --kind audio`. (Some podcast
        // episodes arrive as AAC-in-MP4 → .m4a, AUD-159.)
        Some(kind @ ("mp3" | "m4a")) => {
            eprintln!("{asin}: audio is already a playable {kind} — no decryption needed");
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn decrypt_aaxc(
    ctx: &Ctx,
    marketplace: &str,
    tool: &Tool,
    asin: &str,
    aaxc: &Path,
    format: &str,
    request_kind: &str,
    keep_source: bool,
    force: bool,
    no_db_write: bool,
) -> Result<Option<((String, String), bool)>> {
    let out = aaxc.with_extension("m4b");
    if !force && !no_db_write && decrypted_recorded(ctx, marketplace, asin, format).await {
        eprintln!("skipping decrypt — {format} already decrypted (use --force)");
        return Ok(None);
    }

    // Key/iv from the `<name>.voucher` sidecar written next to the audio.
    // `download --decrypt` always fetches audio first, so it is present; a
    // future standalone `audible decrypt` would regenerate it from the stored
    // license when the sidecar is gone.
    let voucher = aaxc.with_extension("voucher");
    let Some((key, iv)) = read_keyfile(&voucher) else {
        eprintln!(
            "{asin}: cannot decrypt — no key/iv sidecar at {} (re-run with --kind audio)",
            voucher.display()
        );
        return Ok(None);
    };

    eprintln!("decrypting {} with {} …", aaxc.display(), tool.label());
    run(tool, aaxc, &key, &iv, &out)
        .await
        .with_context(|| format!("decrypting {}", aaxc.display()))?;

    let size = std::fs::metadata(&out).ok().map(|m| m.len());
    // The decrypted file is audio too — same kind, variant = decrypted.
    super::item::record_download(
        ctx,
        marketplace,
        asin,
        None,
        "audio",
        format,
        "decrypted",
        request_kind,
        &out,
        size,
        no_db_write,
    )
    .await;

    let mut audio_superseded = false;
    if !keep_source {
        let _ = std::fs::remove_file(aaxc);
        let _ = std::fs::remove_file(&voucher);
        // Drop the now-obsolete (audio, original) record so the DB keeps no
        // ghost path (reorganize never tries to move a deleted aaxc); the
        // decrypted row marks the item as downloaded.
        drop_original_record(ctx, marketplace, asin, format, no_db_write).await;
        audio_superseded = true;
    }
    Ok(Some((
        ("decrypted".into(), out.display().to_string()),
        audio_superseded,
    )))
}

/// Removes the `(audio, original)` record after its aaxc was deleted
/// (`--remove-source`): the tool never leaves a `downloaded` row for a file
/// it removed itself. The `decrypted` row — same content_format and
/// request_kind — keeps the item recorded for the format-aware skip. A
/// no-op under `--no-db-write` (there is no record to drop, and existing
/// records must stay untouched).
pub(super) async fn drop_original_record(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    content_format: &str,
    no_db_write: bool,
) {
    if no_db_write {
        return;
    }
    let Ok(db) = ctx.open_library_db().await else {
        return;
    };
    let key = (
        asin.to_owned(),
        marketplace.to_owned(),
        "audio".to_owned(),
        content_format.to_owned(),
        "original".to_owned(),
    );
    if let Err(error) = db.delete_downloads(vec![key]).await {
        tracing::warn!(%error, "could not drop the original audio record after decrypt");
    }
}

/// Reads the `{ "key", "iv" }` voucher sidecar. `None` if absent/unreadable.
/// The values are sensitive and never logged.
fn read_keyfile(path: &Path) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct KeyIv {
        key: String,
        iv: String,
    }
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: KeyIv = serde_json::from_str(&text).ok()?;
    Some((parsed.key, parsed.iv))
}

/// The content-format segment of an audio file name (`<base>.<format>.<ext>`
/// → `<format>`), a fallback when no license format is available. Empty when
/// there is none.
fn aaxc_format(audio: &Path) -> String {
    audio
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit_once('.').map(|(_, format)| format.to_owned()))
        .unwrap_or_default()
}

/// Whether a `decrypted` audio row for exactly this `content_format` is
/// recorded (AUD-95: keyed by format, so one format's playable file does not
/// block decrypting another format of the same title).
pub(super) async fn decrypted_recorded(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    content_format: &str,
) -> bool {
    let Ok(db) = ctx.open_library_db().await else {
        return false;
    };
    db.download_records_variant(
        asin.to_owned(),
        marketplace.to_owned(),
        "audio".to_owned(),
        "decrypted".to_owned(),
    )
    .await
    .map(|records| {
        records
            .iter()
            .any(|record| record.content_format == content_format)
    })
    .unwrap_or(false)
}

/// The recorded `(audio, original)` download (the aaxc to decrypt) whose file
/// still exists on disk, if any.
async fn recorded_audio(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<DownloadRecord> {
    let db = ctx.open_library_db().await.ok()?;
    let records = db
        .download_records_variant(
            asin.to_owned(),
            marketplace.to_owned(),
            "audio".to_owned(),
            "original".to_owned(),
        )
        .await
        .ok()?;
    first_on_disk(records)
}

/// The decrypt source among the recorded originals: the first on-disk
/// `.aaxc` — with multi-format coexistence a `.cenc`/`.mp3` original may sit
/// next to it (AUD-96) — else the first on-disk record, so an mp3-only item
/// still reports "already playable".
fn first_on_disk(records: Vec<DownloadRecord>) -> Option<DownloadRecord> {
    let mut fallback = None;
    for record in records {
        if !Path::new(&record.file_path).exists() {
            continue;
        }
        if record.file_path.ends_with(".aaxc") {
            return Some(record);
        }
        if fallback.is_none() {
            fallback = Some(record);
        }
    }
    fallback
}

/// Picks a short error detail: stderr if any, else stdout; first 3 lines.
fn pick_error(stderr: &str, stdout: &str) -> String {
    let source = if stderr.is_empty() { stdout } else { stderr };
    if source.is_empty() {
        return String::new();
    }
    let brief: Vec<&str> = source.lines().take(3).collect();
    format!(": {}", brief.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Unix a tool is looked up by its bare name.
    #[cfg(not(windows))]
    #[test]
    fn candidates_are_the_bare_name_on_unix() {
        assert_eq!(
            executable_candidates(Path::new("/usr/bin"), "ffmpeg"),
            vec![PathBuf::from("/usr/bin/ffmpeg")]
        );
    }

    /// On Windows the PATH search must try the executable extensions, so a
    /// bare `ffmpeg` finds `ffmpeg.exe`. A name that already has an extension
    /// is used verbatim.
    #[cfg(windows)]
    #[test]
    fn candidates_apply_pathext_on_windows() {
        let cands = executable_candidates(Path::new(r"C:\bin"), "ffmpeg");
        assert!(
            cands
                .iter()
                .any(|c| c.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))),
            "{cands:?} should include an .exe candidate"
        );
        assert!(
            cands.iter().all(|c| c.starts_with(r"C:\bin")),
            "candidates stay in the given dir"
        );
        assert_eq!(
            executable_candidates(Path::new(r"C:\bin"), "ffmpeg.exe"),
            vec![PathBuf::from(r"C:\bin\ffmpeg.exe")],
            "an explicit extension is used as-is"
        );
    }

    #[tokio::test]
    async fn ffmpeg_version_parses_release_and_git_strings() {
        // We can't run a fake binary here; test the parse via a wrapper.
        assert_eq!(
            parse_ffmpeg_version("ffmpeg version 5.1.9-0+deb12u1 x"),
            Some((5, 1))
        );
        assert_eq!(parse_ffmpeg_version("ffmpeg version 4.4 x"), Some((4, 4)));
        assert_eq!(
            parse_ffmpeg_version("ffmpeg version n4.3.2 x"),
            Some((4, 3))
        );
        assert_eq!(parse_ffmpeg_version("ffmpeg version N-109592-gd6 x"), None);
    }

    // Mirror of the parse in `ffmpeg_version` (over a captured first line).
    fn parse_ffmpeg_version(first_line: &str) -> Option<(u32, u32)> {
        let token = first_line.split_whitespace().nth(2)?;
        let digits = token.trim_start_matches(['n', 'N']);
        let mut parts = digits.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts
            .next()?
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()?;
        Some((major, minor))
    }

    #[test]
    fn error_detail_prefers_stderr_and_is_brief() {
        assert_eq!(pick_error("boom", "noise"), ": boom");
        assert_eq!(pick_error("", "only stdout"), ": only stdout");
        assert_eq!(pick_error("", ""), "");
        assert_eq!(pick_error("a\nb\nc\nd", ""), ": a; b; c");
    }

    fn record(path: &Path, format: &str, request_kind: &str) -> DownloadRecord {
        DownloadRecord {
            asin: "A1".into(),
            kind: "audio".into(),
            acr: None,
            content_format: format.into(),
            variant: "original".into(),
            request_kind: request_kind.into(),
            version: None,
            sku: None,
            file_path: path.display().to_string(),
            file_size: None,
        }
    }

    /// The later-run decrypt must inherit format + request_kind from exactly
    /// the record whose file it decrypts (AUD-94).
    #[test]
    fn first_on_disk_returns_the_existing_files_record() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("A1.AAC_44_131.cenc");
        let present = dir.path().join("A1.AAX_44_128.aaxc");
        std::fs::write(&present, b"x").unwrap();

        let found = first_on_disk(vec![
            record(&missing, "AAC_44_131", "widevine-aac-normal"),
            record(&present, "AAX_44_128", "adrm-high"),
        ])
        .expect("the on-disk record");
        assert_eq!(found.file_path, present.display().to_string());
        assert_eq!(found.request_kind, "adrm-high");
        assert_eq!(found.content_format, "AAX_44_128");
    }

    #[test]
    fn first_on_disk_none_when_no_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("A1.AAX_44_128.aaxc");
        assert!(first_on_disk(vec![record(&gone, "AAX_44_128", "adrm-high")]).is_none());
        assert!(first_on_disk(Vec::new()).is_none());
    }

    /// With coexisting originals (AUD-96) the aaxc wins even when another
    /// on-disk original (e.g. a Widevine `.cenc`) is listed first.
    #[test]
    fn first_on_disk_prefers_the_aaxc() {
        let dir = tempfile::tempdir().unwrap();
        let cenc = dir.path().join("A1.AAC_44_131.cenc");
        let aaxc = dir.path().join("A1.AAX_44_128.aaxc");
        std::fs::write(&cenc, b"x").unwrap();
        std::fs::write(&aaxc, b"x").unwrap();

        let found = first_on_disk(vec![
            record(&cenc, "AAC_44_131", "widevine-aac-high"),
            record(&aaxc, "AAX_44_128", "adrm-high"),
        ])
        .expect("the aaxc record");
        assert_eq!(found.file_path, aaxc.display().to_string());

        // Without any aaxc the first on-disk record still wins (an mp3-only
        // item must keep reporting "already playable").
        let mp3 = dir.path().join("A1.MPEG.mp3");
        std::fs::write(&mp3, b"x").unwrap();
        let found = first_on_disk(vec![
            record(&mp3, "MPEG", "mpeg"),
            record(&cenc, "AAC_44_131", "widevine-aac-high"),
        ])
        .expect("the mp3 record");
        assert_eq!(found.file_path, mp3.display().to_string());
    }
}
