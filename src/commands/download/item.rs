//! The per-item download pipeline: the immutable [`DownloadPlan`],
//! [`download_one`] (skip logic, license choice, artifact loop, decrypt
//! tail), the batch counters and the download-record bookkeeping.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::config::ctx::Ctx;
use crate::models::content::DownloadLicense;

use super::artifacts::{download_audio, download_covers, download_pdf, write_chapters};
use super::license::{acquire_license, is_no_aaxc_asset, reuse_license};
use super::*;

/// Per-invocation download settings shared across every resolved ASIN.
pub(super) struct DownloadPlan<'a> {
    pub(super) quality: Quality,
    /// The single marketplace this download targets (host + DB scope).
    pub(super) marketplace: &'a str,
    pub(super) base_targets: &'a BTreeSet<Artifact>,
    pub(super) dir: &'a Path,
    pub(super) cover_sizes: &'a [String],
    pub(super) chapter_types: &'a [crate::config::schema::ChapterType],
    pub(super) relicense: bool,
    pub(super) force: bool,
    /// Quick-grab mode (`--no-db-write`): no database writes, no
    /// record-based skip; only the on-disk checks in `dir` apply.
    pub(super) no_db_write: bool,
    /// Decrypt tool to run after the audio download; `None` = no decrypt.
    pub(super) decrypt: Option<&'a decrypt::Tool>,
    /// Keep the source aaxc after a successful decrypt.
    pub(super) keep_source: bool,
    /// Force the Widevine/DASH path (else it is an automatic fallback for
    /// streaming-only titles that have no aaxc asset).
    pub(super) widevine: bool,
    /// Offer xHE-AAC on the Widevine path (`--codec xhe`).
    pub(super) codec_xhe: bool,
    /// Request Dolby Atmos on the Widevine path (guarded by the CDM's L1 level).
    pub(super) spatial: bool,
    /// The account's loaded Widevine CDM (+ security level), if configured.
    pub(super) cdm: Option<&'a (crate::widevine::Cdm, u8)>,
    /// Where heavy (audio) transfers add their byte bar; `None` when no
    /// progress is shown (quiet / non-interactive).
    pub(super) mp: Option<&'a MultiProgress>,
}

/// Running tally for the batch summary line. Items are counted once each;
/// the per-kind fields count written artifacts.
#[derive(Default)]
pub(super) struct Counters {
    pub(super) items_done: usize,
    pub(super) failed: usize,
    pub(super) audio: usize,
    pub(super) cover: usize,
    pub(super) chapter: usize,
    pub(super) pdf: usize,
    pub(super) decrypted: usize,
}

impl Counters {
    pub(super) fn bump(&mut self, kind: &str) {
        match kind {
            "audio" => self.audio += 1,
            "cover" => self.cover += 1,
            "chapter" => self.chapter += 1,
            "pdf" => self.pdf += 1,
            "decrypted" => self.decrypted += 1,
            _ => {}
        }
    }
}

/// The one-line batch summary (item count + per-kind artifact counters).
pub(super) fn summary_line(c: &Counters, total: usize) -> String {
    format!(
        "[{}/{total}] · audio {} · cover {} · chapter {} · pdf {} · decrypted {} · failed {}",
        c.items_done, c.audio, c.cover, c.chapter, c.pdf, c.decrypted, c.failed
    )
}

/// Downloads the planned artifacts for one ASIN, returning the
/// `(kind, path)` pairs actually written.
pub(super) async fn download_one(
    ctx: &Ctx,
    client: &crate::api::client::Client,
    asin: &str,
    plan: &DownloadPlan<'_>,
) -> Result<Vec<(String, String)>> {
    let mut targets = plan.base_targets.clone();
    let base = base_filename(ctx, plan.marketplace, asin).await?;
    // The custom filename mode may nest the title in subfolders; create them
    // before any artifact is written (a no-op for the flat modes).
    let stem = crate::naming::join_relative(plan.dir, &base);
    if let Some(parent) = stem.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create directory {}", parent.display()))?;
    }

    // Skip (the default) drops, before any license request, every artifact
    // already recorded in the database — the record is authoritative
    // regardless of whether the file is still on disk (users routinely
    // decrypt and delete the .aaxc). Force re-downloads; --no-db-write
    // ignores the records entirely (the grab targets a different dir).
    // The audio request-kind candidates feed both the record-based skip
    // and the license-reuse lookup below.
    let audio_candidates = request_kind::candidates(plan.widevine, plan.codec_xhe, plan.quality);
    if !plan.force && !plan.no_db_write {
        skip_already_downloaded(ctx, plan.marketplace, asin, &mut targets, &audio_candidates).await;
        // With --decrypt, fall through even when every download target is
        // already recorded — the decrypt step may still have work (AUD-97)
        // and brings its own format-aware skip. Without it, bulk re-runs
        // keep returning fast, before any license work.
        if targets.is_empty() && plan.decrypt.is_none() {
            eprintln!("{asin}: all requested artifacts already recorded");
            return Ok(Vec::new());
        }
    }

    // Re-use a stored, still-valid license for this title (the content URL
    // is stable), which skips a fresh licenserequest entirely — unless
    // `--relicense` forces a fresh grant. With no download targets left
    // (decrypt-only fall-through) no license is needed at all.
    let mut license = if plan.relicense || targets.is_empty() {
        None
    } else {
        reuse_license(ctx, plan.marketplace, asin, audio_candidates).await
    };
    if license.is_some() {
        eprintln!("reusing stored license for {asin}");
    }

    // `--widevine` forces the DASH path for audio; otherwise a title with no
    // downloadable aaxc asset (aaxc licenserequest → 000307/acr:null — e.g. an
    // AYCL/Plus title Audible now serves via Widevine only) falls back to it.
    let mut widevine_audio = plan.widevine;

    // A license is needed only for the aaxc audio download: PDFs load from the
    // companion-file URL without one (AUD-103), chapters/cover come from
    // metadata, and the Widevine path brings its own license.
    let needs_license = license.is_none()
        && targets
            .iter()
            .any(|target| matches!(target, Artifact::Audio) && !widevine_audio);
    if needs_license {
        match acquire_license(
            ctx,
            client,
            plan.marketplace,
            asin,
            plan.quality,
            plan.no_db_write,
        )
        .await
        {
            Ok(granted) => license = Some(granted),
            // No downloadable aaxc asset (an AYCL/Plus title Audible serves via
            // Widevine only): route audio through Widevine.
            Err(error) if is_no_aaxc_asset(&error) => {
                eprintln!("{asin}: no aaxc asset (Widevine only) — using the Widevine path");
                widevine_audio = true;
            }
            Err(error) => return Err(error),
        }
    }

    let mut written: Vec<(String, String)> = Vec::new();
    // The aaxc just written this run (if any), so the decrypt step below can
    // use it without re-deriving the path.
    let mut aaxc_path: Option<PathBuf> = None;
    // The aaxc audio's request_kind (AUD-93), reused by the decrypt step so the
    // decrypted variant carries the same intent key. Stays empty when the audio
    // target is skipped — the decrypt step then reads it from the recorded
    // original row instead (AUD-94).
    let mut audio_request_kind = String::new();
    for target in &targets {
        match target {
            Artifact::Audio if widevine_audio => {
                let (cdm, security_level) = plan.cdm.context(
                    "this title is streaming-only and needs a Widevine CDM — configure one \
                     with `account widevine fetch <URL>` or `account widevine set <PATH>`",
                )?;
                // The grant's pdf_url (AUD-103 phase 1) is intentionally unused:
                // PDFs load via the companion-file URL now (verified equivalent).
                let (paths, _pdf_url) = widevine::download_audio_widevine(
                    ctx,
                    client,
                    plan.marketplace,
                    asin,
                    plan.quality,
                    plan.spatial,
                    plan.codec_xhe,
                    cdm,
                    *security_level,
                    plan.dir,
                    &base,
                    plan.force,
                    plan.no_db_write,
                    plan.decrypt,
                    plan.keep_source,
                    plan.mp,
                )
                .await?;
                written.extend(paths);
            }
            Artifact::Audio => {
                let license = license.as_ref().context("audio download needs a license")?;
                // A Mpeg grant (podcasts / asset-less titles) keys as `mpeg`;
                // an Adrm grant keys by the requested quality.
                let grant = if license.drm_type.as_deref() == Some("Mpeg") {
                    request_kind::Grant::Mpeg
                } else {
                    request_kind::Grant::Adrm
                };
                audio_request_kind = request_kind::resolved(grant, false, plan.quality);
                let path = download_audio(
                    ctx,
                    client,
                    plan.marketplace,
                    license,
                    plan.dir,
                    &base,
                    plan.force,
                    plan.no_db_write,
                    &audio_request_kind,
                    plan.mp,
                )
                .await?;
                aaxc_path = Some(PathBuf::from(&path));
                written.push(("audio".into(), path));
            }
            Artifact::Chapter => {
                for chapter_type in plan.chapter_types {
                    if let Some(path) = write_chapters(
                        ctx,
                        client,
                        plan.marketplace,
                        asin,
                        *chapter_type,
                        plan.quality,
                        plan.dir,
                        &base,
                        plan.force,
                        plan.no_db_write,
                    )
                    .await?
                    {
                        written.push(("chapter".into(), path));
                    }
                }
            }
            Artifact::Pdf => {
                // PDFs load from the companion-file URL — no licenserequest
                // needed (AUD-103). The license, when this run acquired one for
                // audio, only backfills the record's acr/version/sku.
                match download_pdf(
                    ctx,
                    client,
                    plan.marketplace,
                    asin,
                    license.as_ref(),
                    plan.dir,
                    &base,
                    plan.force,
                    plan.no_db_write,
                )
                .await?
                {
                    Some(path) => written.push(("pdf".into(), path)),
                    None => eprintln!("no PDF available for this title"),
                }
            }
            Artifact::Cover => {
                let paths = download_covers(
                    ctx,
                    client,
                    plan.marketplace,
                    asin,
                    plan.cover_sizes,
                    plan.dir,
                    &base,
                    plan.force,
                    plan.no_db_write,
                )
                .await?;
                written.extend(paths.into_iter().map(|p| ("cover".into(), p)));
            }
        }
    }

    // Decrypt runs last, in this same item job (AUD-27), so `--jobs` finishes
    // each item incl. the m4b and it can later use the item's cover/chapters.
    // The audio's content_format (authoritative — it keys the `audio` record
    // and the file name) comes from the license used this run.
    // The Widevine path already decrypted inline (with the content key), so the
    // aaxc decrypt step only runs for the aaxc path.
    if let Some(tool) = plan.decrypt.filter(|_| !widevine_audio) {
        let audio_format = license
            .as_ref()
            .map(|l| l.content_format.clone().unwrap_or_default());
        if let Some((entry, audio_superseded)) = decrypt::decrypt_item(
            ctx,
            plan.marketplace,
            tool,
            asin,
            aaxc_path.as_deref(),
            audio_format,
            &audio_request_kind,
            plan.keep_source,
            plan.force,
            plan.no_db_write,
        )
        .await?
        {
            // The playable file replaced the aaxc (remove-source) or the aaxc
            // was already an mp3 → drop the now-obsolete audio row from the
            // run summary too.
            if audio_superseded {
                written.retain(|(kind, _)| kind != "audio");
            }
            written.push(entry);
        }
    }
    Ok(written)
}

/// Drops, from `targets`, every artifact already recorded in the
/// `downloads` table (the record is authoritative — the file may have
/// been decrypted and deleted).
async fn skip_already_downloaded(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    targets: &mut BTreeSet<Artifact>,
    audio_request_kinds: &[String],
) {
    let Ok(db) = ctx.open_library_db().await else {
        return;
    };
    let mut keep = BTreeSet::new();
    for target in targets.iter().copied() {
        // Covers (per size) and chapters (per flat/tree) have variants; their
        // skip is decided per variant in-branch.
        if matches!(target, Artifact::Cover | Artifact::Chapter) {
            keep.insert(target);
            continue;
        }
        let recorded = if matches!(target, Artifact::Audio) {
            // Format-aware (AUD-93): skip only when a format the request could
            // resolve to is already downloaded, so a different format/codec of
            // the same title is not blocked (covers both variants).
            db.downloaded_request_kinds(asin.to_owned(), marketplace.to_owned())
                .await
                .map(|kinds| kinds.iter().any(|k| audio_request_kinds.contains(k)))
                .unwrap_or(false)
        } else {
            // PDF has no format variants — the coarse (asin, kind) check fits.
            db.download_files(
                asin.to_owned(),
                marketplace.to_owned(),
                target.kind().to_owned(),
            )
            .await
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
        };
        if recorded {
            eprintln!(
                "skipping {} — already recorded (use --force)",
                target.kind()
            );
        } else {
            keep.insert(target);
        }
    }
    *targets = keep;
}

/// Whether a specific artifact variant (`kind` + `content_format`) is
/// already recorded for the item — for per-variant skip (cover sizes,
/// chapter layouts).
pub(super) async fn variant_recorded(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    kind: &str,
    content_format: &str,
) -> bool {
    let Ok(db) = ctx.open_library_db().await else {
        return false;
    };
    // Covers/chapters are always `original` downloads.
    db.download_record(
        asin.to_owned(),
        marketplace.to_owned(),
        kind.to_owned(),
        content_format.to_owned(),
        "original".to_owned(),
    )
    .await
    .map(|record| record.is_some())
    .unwrap_or(false)
}

/// Records a downloaded artifact in the `downloads` table. `license`
/// supplies the audio metadata (acr/version/sku) when present; chapter
/// and PDF artifacts carry an empty `content_format`. A no-op under
/// `--no-db-write` — the quick-grab run leaves no trace in the database.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_download(
    ctx: &Ctx,
    marketplace: &str,
    asin: &str,
    license: Option<&DownloadLicense>,
    kind: &str,
    content_format: &str,
    variant: &str,
    request_kind: &str,
    dest: &Path,
    size: Option<u64>,
    no_db_write: bool,
) {
    if no_db_write {
        return;
    }
    let Ok(db) = ctx.open_library_db().await else {
        return;
    };
    let record = crate::db::DownloadRecord {
        asin: asin.to_owned(),
        kind: kind.to_owned(),
        acr: license.and_then(|l| l.acr.clone()),
        content_format: content_format.to_owned(),
        variant: variant.to_owned(),
        request_kind: request_kind.to_owned(),
        version: license.and_then(|l| l.version.clone()),
        sku: license.and_then(|l| l.sku.clone()),
        file_path: dest.display().to_string(),
        file_size: size,
    };
    if let Err(error) = db.record_download(marketplace.to_owned(), record).await {
        tracing::warn!(%error, "could not record the download in the database");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_line_counts_items_and_artifact_kinds() {
        let mut c = Counters::default();
        for kind in [
            "audio",
            "cover",
            "cover",
            "chapter",
            "pdf",
            "decrypted",
            "bogus",
        ] {
            c.bump(kind); // "bogus" is ignored
        }
        c.items_done = 2;
        c.failed = 1;
        assert_eq!(
            summary_line(&c, 5),
            "[2/5] · audio 1 · cover 2 · chapter 1 · pdf 1 · decrypted 1 · failed 1"
        );
    }
}
