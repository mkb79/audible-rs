//! Parse a Widevine DASH MPD (AUD-56b).
//!
//! Audible's Widevine `license_response` is a DASH manifest with a single
//! audio `Representation`: one CENC-encrypted `.mp4` at a `BaseURL`, plus the
//! Widevine PSSH. We pull out the init data (for the license challenge), the
//! resolved content URL, and a **bitrate-inclusive `content_format`** derived
//! from the structured `Representation` attributes (codec + sample rate +
//! bandwidth) — so different qualities never collide in the download model.

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::pssh;

const WIDEVINE_SCHEME_UUID: &str = "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";

/// The single audio stream described by a Widevine MPD.
#[derive(Debug, Clone)]
pub struct WidevineStream {
    /// Widevine PSSH init data (input to the license challenge).
    pub pssh_init_data: Vec<u8>,
    /// Absolute URL of the CENC-encrypted `.mp4` (BaseURL resolved).
    pub content_url: String,
    /// Bitrate-inclusive content format, e.g. `XHE_44_137`, `AC4_48_326`.
    pub content_format: String,
    /// Raw `@codecs` (e.g. `mp4a.40.42`, `ac-4.02.02.00`).
    pub codec: String,
}

/// Errors from MPD parsing.
#[derive(Debug, thiserror::Error)]
pub enum MpdError {
    /// The MPD is not valid XML.
    #[error("malformed MPD")]
    Xml,
    /// No Widevine ContentProtection / PSSH present.
    #[error("no Widevine PSSH in the MPD")]
    NoPssh,
    /// No audio Representation present.
    #[error("no Representation in the MPD")]
    NoRepresentation,
    /// No BaseURL to resolve the content file.
    #[error("no BaseURL in the MPD")]
    NoBaseUrl,
}

/// Parses a Widevine MPD (fetched from `mpd_url`, used to resolve the BaseURL).
pub fn parse(mpd_xml: &str, mpd_url: &str) -> Result<WidevineStream, MpdError> {
    let doc = roxmltree::Document::parse(mpd_xml).map_err(|_| MpdError::Xml)?;
    let local = |name: &'static str| move |n: &roxmltree::Node| n.tag_name().name() == name;

    // Widevine ContentProtection -> cenc:pssh (base64) -> init data.
    let pssh_b64 = doc
        .descendants()
        .find(|n| {
            n.tag_name().name() == "ContentProtection"
                && n.attribute("schemeIdUri")
                    .is_some_and(|s| s.eq_ignore_ascii_case(WIDEVINE_SCHEME_UUID))
        })
        .and_then(|cp| cp.descendants().find(local("pssh")))
        .and_then(|n| n.text())
        .ok_or(MpdError::NoPssh)?;
    let pssh_box = STANDARD
        .decode(pssh_b64.trim())
        .map_err(|_| MpdError::NoPssh)?;
    let pssh_init_data = pssh::init_data(&pssh_box).map_err(|_| MpdError::NoPssh)?;

    // The single audio Representation carries the quality attributes + BaseURL.
    let rep = doc
        .descendants()
        .find(local("Representation"))
        .ok_or(MpdError::NoRepresentation)?;
    let codec = rep.attribute("codecs").unwrap_or_default().to_string();
    let sample_rate: u32 = rep
        .attribute("audioSamplingRate")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bandwidth: u32 = rep
        .attribute("bandwidth")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let base = rep
        .descendants()
        .find(local("BaseURL"))
        .or_else(|| doc.descendants().find(local("BaseURL")))
        .and_then(|n| n.text())
        .ok_or(MpdError::NoBaseUrl)?;
    let content_url = url::Url::parse(mpd_url)
        .and_then(|u| u.join(base.trim()))
        .map(|u| u.to_string())
        .map_err(|_| MpdError::NoBaseUrl)?;

    Ok(WidevineStream {
        pssh_init_data,
        content_url,
        content_format: content_format(&codec, sample_rate, bandwidth),
        codec,
    })
}

/// Builds a stable, quality-distinct content format from the codec, sample
/// rate and bandwidth: `<CODEC>_<kHz>_<kbps>` (e.g. `XHE_44_137`, `AC4_48_326`,
/// `AAC_44_131`). Mirrors the aaxc `AAX_44_128` style so each quality is its
/// own row/file.
fn content_format(codec: &str, sample_rate: u32, bandwidth: u32) -> String {
    // Order matters: mp4a.40.42 (xHE) is not a prefix of mp4a.40.2 (AAC-LC).
    let short = if codec.starts_with("mp4a.40.42") {
        "XHE"
    } else if codec.starts_with("mp4a.40.2") {
        "AAC"
    } else if codec.starts_with("ac-4") {
        "AC4"
    } else if codec.starts_with("ec") {
        "EC3"
    } else {
        "AUD"
    };
    let khz = (sample_rate + 500) / 1000;
    let kbps = (bandwidth + 500) / 1000;
    format!("{short}_{khz}_{kbps}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal v0 Widevine PSSH box carrying `data` as init data.
    fn pssh_box(data: &[u8]) -> String {
        let mut b = Vec::new();
        b.extend_from_slice(&(32 + data.len() as u32).to_be_bytes());
        b.extend_from_slice(b"pssh");
        b.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        b.extend_from_slice(&[
            0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d,
            0x21, 0xed,
        ]);
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        STANDARD.encode(&b)
    }

    #[test]
    fn parses_stream_and_derives_format() {
        let mpd = format!(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:cenc="urn:mpeg:cenc:2013" type="static">
  <Period><AdaptationSet contentType="audio">
    <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/>
    <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed">
      <cenc:pssh>{}</cenc:pssh>
    </ContentProtection>
    <Representation id="0" codecs="mp4a.40.42" audioSamplingRate="44100" bandwidth="136707">
      <BaseURL>../../base/bk_1_44_128-xhe.mp4?t=x</BaseURL>
    </Representation>
  </AdaptationSet></Period>
</MPD>"#,
            pssh_box(&[0x08, 0x01])
        );
        let s = parse(&mpd, "https://cdn.example/a/b/c/manifest.mpd").unwrap();
        assert_eq!(s.codec, "mp4a.40.42");
        assert_eq!(s.content_format, "XHE_44_137");
        assert_eq!(s.pssh_init_data, vec![0x08, 0x01]);
        assert!(s.content_url.ends_with("bk_1_44_128-xhe.mp4?t=x"));
        assert!(s.content_url.starts_with("https://cdn.example/a/"));
    }

    #[test]
    fn content_format_variants() {
        assert_eq!(content_format("mp4a.40.2", 44100, 131072), "AAC_44_131");
        assert_eq!(content_format("mp4a.40.42", 44100, 49502), "XHE_44_50");
        assert_eq!(content_format("ac-4.02.02.00", 48000, 325746), "AC4_48_326");
    }
}
