//! Widevine glue for `download` (AUD-56c): obtain the content key — via
//! `drmlicense` and the native Widevine client — and cache it in a `.wvkey`
//! sidecar next to the CENC file (the Widevine counterpart of the aaxc
//! `.voucher`), so resume and re-decrypt need no fresh `drmlicense`.
//!
//! The content key is a secret: held in `Zeroizing`, written 0600, never logged.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use indicatif::MultiProgress;

use super::decrypt::Tool;
use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::downloader::{
    DownloadOutcome, Quality, WidevineGrant, download_cenc_to_file, request_drmlicense,
    request_widevine_license,
};
use crate::naming::join_relative;
use crate::widevine::{Cdm, ContentKey, Device, mpd};

/// Loads the account's configured Widevine CDM. Returns the client plus the
/// device's Widevine security level (1 = L1 hardware; 3 = software/L3) — used
/// to gate Atmos, which the server only licenses to L1. Errors clearly if no
/// CDM is configured for the account.
pub(super) fn load_cdm(ctx: &Ctx) -> Result<(Cdm, u8)> {
    let account = ctx.account()?;
    let configured = account.widevine_cdm.as_ref().ok_or_else(|| {
        anyhow!(
            "no Widevine CDM configured for this account — needed for streaming-only \
             (Widevine) titles. Add one with `account widevine fetch <URL>` or \
             `account widevine set <PATH>`."
        )
    })?;
    let path = if configured.is_absolute() {
        configured.clone()
    } else {
        ctx.config_dir().join(configured)
    };
    let bytes = std::fs::read(&path).with_context(|| format!("reading CDM {}", path.display()))?;
    let device =
        Device::from_wvd(&bytes).with_context(|| format!("parsing CDM {}", path.display()))?;
    let security_level = device.security_level();
    let cdm = Cdm::from_device(&device)?;
    Ok((cdm, security_level))
}

/// Obtains the content key for a title: build the challenge from the PSSH,
/// request the license (`drmlicense`) and parse it into the first CONTENT key.
pub(super) async fn content_key(
    client: &Client,
    country_code: &str,
    asin: &str,
    cdm: &Cdm,
    pssh_init_data: &[u8],
) -> Result<ContentKey> {
    let challenge = cdm.challenge(pssh_init_data, true)?;
    let license = request_drmlicense(client, country_code, asin, &challenge.message).await?;
    cdm.parse_license(&challenge, &license)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("the license returned no content key"))
}

/// Reads a `.wvkey` sidecar (`{ "kid", "key" }` hex). `None` if absent/invalid.
pub(super) fn read_wvkey(path: &Path) -> Option<ContentKey> {
    #[derive(serde::Deserialize)]
    struct KidKey {
        kid: String,
        key: String,
    }
    let parsed: KidKey = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let kid: [u8; 16] = hex::decode(&parsed.kid).ok()?.try_into().ok()?;
    let key = hex::decode(&parsed.key).ok()?;
    Some(ContentKey {
        kid,
        key: zeroize::Zeroizing::new(key),
    })
}

/// Writes the content key to a `.wvkey` sidecar (0600).
pub(super) fn write_wvkey(path: &Path, key: &ContentKey) -> Result<()> {
    let json = serde_json::json!({
        "kid": hex::encode(key.kid),
        "key": hex::encode(&*key.key),
    })
    .to_string();
    super::write_private(path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// The full Widevine audio path for one title: license → MPD → content key
/// (+ `.wvkey` sidecar) → CENC download (resumeable) → optional lossless decrypt
/// (`--decrypt`) → DB records. Mirrors the aaxc variant model: the encrypted
/// CENC is `(audio, original)` and the decrypted file `(audio, decrypted)`.
///
/// Returns the written `(kind, path)` pairs and the companion `pdf_url` the
/// grant carried (if any), so the caller's PDF target can fetch it — a
/// Widevine-only title has no aaxc license to source the URL from.
#[allow(clippy::too_many_arguments)]
pub(super) async fn download_audio_widevine(
    ctx: &Ctx,
    client: &Client,
    marketplace: &str,
    asin: &str,
    quality: Quality,
    spatial: bool,
    xhe: bool,
    cdm: &Cdm,
    security_level: u8,
    dir: &Path,
    base: &str,
    force: bool,
    no_db_write: bool,
    decrypt: Option<&Tool>,
    keep_source: bool,
    mp: Option<&MultiProgress>,
) -> Result<(Vec<(String, String)>, Option<String>)> {
    // Atmos needs Widevine L1; an L3 CDM is denied ec+3/ac-4 at drmlicense.
    let spatial = if spatial && security_level != 1 {
        eprintln!(
            "{asin}: Dolby Atmos needs a Widevine L1 device (this CDM is L{security_level}); \
             downloading stereo instead"
        );
        false
    } else {
        spatial
    };

    // 1. license -> grant. A Mpeg fallback (podcast / no Widevine asset, e.g.
    // `--widevine` forced on a podcast) is a plain MP3 — download it directly.
    let license = match request_widevine_license(client, marketplace, asin, quality, spatial, xhe)
        .await?
    {
        WidevineGrant::Widevine(license) => license,
        WidevineGrant::Mpeg(mpeg) => {
            eprintln!("{asin}: no Widevine asset — downloading the plain MP3");
            let dest = join_relative(
                dir,
                &format!(
                    "{base}{}",
                    crate::naming::artifact_suffix("audio", &mpeg.content_format, "mp3")
                ),
            );
            let (_, dest) = download_cenc_to_file(
                &mpeg.offline_url,
                &dest,
                mpeg.content_size,
                force,
                mp,
                &["audio/mpeg"],
                // The Mpeg fallback grant carries no content_reference
                // version; its size guard covers a re-issue.
                None,
            )
            .await?;
            let size = std::fs::metadata(&dest).ok().map(|meta| meta.len());
            let rk = super::request_kind::resolved(super::request_kind::Grant::Mpeg, xhe, quality);
            super::item::record_download(
                ctx,
                marketplace,
                asin,
                None,
                "audio",
                &mpeg.content_format,
                "original",
                &rk,
                &dest,
                size,
                no_db_write,
            )
            .await?;
            // A Mpeg fallback (podcast / asset-less title) has no companion PDF.
            return Ok((
                vec![("audio".to_string(), dest.display().to_string())],
                None,
            ));
        }
    };
    let pdf_url = license.pdf_url.clone();
    let mpd_xml = fetch_text(&license.mpd_url).await?;
    let stream = mpd::parse(&mpd_xml, &license.mpd_url)?;
    let format = stream.content_format.clone();
    // Intent key for the format-aware skip/reuse (AUD-93). A Widevine grant
    // keys by the requested codec/quality (even if the server downgraded xHE).
    let request_kind =
        super::request_kind::resolved(super::request_kind::Grant::Widevine, xhe, quality);

    // 2. content key (+ `.wvkey` sidecar next to the encrypted file).
    let enc_path = join_relative(
        dir,
        &format!(
            "{base}{}",
            crate::naming::artifact_suffix("audio", &format, "cenc")
        ),
    );
    // The key-sidecar location comes from the shared extension map
    // (AUD-99) — the same rule reorganize/orphans/remove follow.
    let wvkey_path =
        crate::naming::sidecar_path(&enc_path).expect("a .cenc path always has a key-sidecar twin");
    let key = match read_wvkey(&wvkey_path) {
        Some(key) => key,
        None => {
            let key = content_key(client, marketplace, asin, cdm, &stream.pssh_init_data).await?;
            write_wvkey(&wvkey_path, &key)?;
            key
        }
    };

    // 3. download the CENC file (ranged, resumeable). The CENC media is an fMP4
    // regardless of codec (AAC-LC, xHE, or a future Atmos) → audio/mp4.
    let (outcome, enc_dest) = download_cenc_to_file(
        &stream.content_url,
        &enc_path,
        license.content_size,
        force,
        mp,
        &["audio/mp4", "video/mp4"],
        // Gate a resumed CENC partial on the grant's content version (A9).
        license.version_tag().as_deref(),
    )
    .await?;
    if matches!(outcome, DownloadOutcome::AlreadyComplete) {
        eprintln!("{} already complete", enc_dest.display());
    }
    let enc_size = std::fs::metadata(&enc_dest).ok().map(|meta| meta.len());
    super::item::record_download(
        ctx,
        marketplace,
        asin,
        None,
        "audio",
        &format,
        "original",
        &request_kind,
        &enc_dest,
        enc_size,
        no_db_write,
    )
    .await?;
    let mut written = vec![("audio".to_string(), enc_dest.display().to_string())];

    // 4. optional lossless decrypt (only with --decrypt).
    if let Some(tool) = decrypt {
        if !force
            && !no_db_write
            && super::decrypt::decrypted_recorded(ctx, marketplace, asin, &format).await?
        {
            eprintln!("{asin}: skipping decrypt — {format} already decrypted (use --force)");
            return Ok((written, pdf_url));
        }
        let out = join_relative(
            dir,
            &format!(
                "{base}{}",
                crate::naming::artifact_suffix("audio", &format, decrypted_ext(&stream.codec))
            ),
        );
        let kid = hex::encode(key.kid);
        let key_hex = hex::encode(&*key.key);
        eprintln!("decrypting {} with {} …", enc_dest.display(), tool.label());
        super::decrypt::run_cenc(tool, &enc_dest, &kid, &key_hex, &out)
            .await
            .with_context(|| format!("decrypting {}", enc_dest.display()))?;
        let out_size = std::fs::metadata(&out).ok().map(|meta| meta.len());
        super::item::record_download(
            ctx,
            marketplace,
            asin,
            None,
            "audio",
            &format,
            "decrypted",
            &request_kind,
            &out,
            out_size,
            no_db_write,
        )
        .await?;

        // `--remove-source` drops the encrypted CENC (+ key) and its record.
        if !keep_source {
            let _ = tokio::fs::remove_file(&enc_dest).await;
            let _ = tokio::fs::remove_file(&wvkey_path).await;
            super::decrypt::drop_original_record(ctx, marketplace, asin, &format, no_db_write)
                .await?;
            written.clear();
        }
        written.push(("audio".into(), out.display().to_string()));
    }
    Ok((written, pdf_url))
}

/// The playable-file extension for a CENC codec: AAC-family → `.m4b` (the
/// audiobook convention); the Dolby codecs (unreachable without L1) → `.mp4`.
fn decrypted_ext(codec: &str) -> &'static str {
    if codec.starts_with("mp4a") {
        "m4b"
    } else {
        "mp4"
    }
}

/// Fetches a signed MPD/text URL with the player UA (no auth, no compression —
/// the signed CloudFront URL is picky, like the CENC content host). Shares the
/// timeout'd, uncompressed client with the CENC download (AUD-98).
async fn fetch_text(url: &str) -> Result<String> {
    let text = crate::downloader::plain_http_client()?
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            crate::downloader::CENC_USER_AGENT,
        )
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypted_ext_by_codec() {
        assert_eq!(decrypted_ext("mp4a.40.2"), "m4b"); // AAC-LC
        assert_eq!(decrypted_ext("mp4a.40.42"), "m4b"); // xHE-AAC
        assert_eq!(decrypted_ext("ec+3"), "mp4"); // E-AC-3 JOC
        assert_eq!(decrypted_ext("ac-4"), "mp4"); // AC-4
    }

    #[test]
    fn wvkey_sidecar_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.wvkey");
        let key = ContentKey {
            kid: [0xABu8; 16],
            key: zeroize::Zeroizing::new(vec![0xCDu8; 16]),
        };
        write_wvkey(&path, &key).unwrap();
        let back = read_wvkey(&path).unwrap();
        assert_eq!(back.kid, key.kid);
        assert_eq!(*back.key, *key.key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
