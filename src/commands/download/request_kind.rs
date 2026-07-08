//! Request-kind aliases (AUD-93): a pre-request-knowable, stable key for the
//! download *intent* — decoupled from the server-derived `content_format`.
//!
//! `content_format` (e.g. `AAX_44_128`, `AAC_44_131`) is only known *after* the
//! licenserequest (for Widevine, after the MPD), so a skip/reuse keyed on it
//! would force a request per item per run. The request_kind is derived from the
//! CLI flags (path/codec/quality) and recorded next to each audio download, so
//! the skip and license-reuse can match by it with a pure DB lookup.
//!
//! MPEG is not a user intent — it is the universal fallback. A resolved Mpeg
//! grant (podcasts, asset-less titles) records the codec/quality-less `mpeg`,
//! and every lookup includes `mpeg` as a candidate.

use crate::downloader::Quality;

/// Every selectable request_kind value — the CLI validation set for
/// `db downloads add --request-kind` (AUD-96).
pub(crate) const ALL: [&str; 7] = [
    "adrm-high",
    "adrm-normal",
    "widevine-aac-high",
    "widevine-aac-normal",
    "widevine-xhe-high",
    "widevine-xhe-normal",
    "mpeg",
];

/// The download path a licenserequest resolved to.
pub(super) enum Grant {
    /// aaxc (Adrm) — the portable, per-account-decryptable format.
    Adrm,
    /// Widevine/DASH (CENC).
    Widevine,
    /// Plain MPEG/MP3 fallback (podcasts / titles with no aax or Widevine).
    Mpeg,
}

fn quality_tag(quality: Quality) -> &'static str {
    match quality {
        Quality::High => "high",
        Quality::Normal => "normal",
    }
}

fn codec_tag(xhe: bool) -> &'static str {
    if xhe { "xhe" } else { "aac" }
}

/// The request_kind recorded for a *resolved* audio download. The path comes
/// from the grant; the codec/quality reflect what was *requested* (so asking
/// for xHE that the server downgraded to AAC still keys as `widevine-xhe-*`,
/// which is what a re-run would request again).
pub(super) fn resolved(grant: Grant, xhe: bool, quality: Quality) -> String {
    match grant {
        Grant::Adrm => format!("adrm-{}", quality_tag(quality)),
        Grant::Widevine => format!("widevine-{}-{}", codec_tag(xhe), quality_tag(quality)),
        Grant::Mpeg => "mpeg".to_string(),
    }
}

/// The candidate request_kinds a *requested* audio download could resolve to.
/// Any recorded match means "already downloaded" → skip without a request.
/// A title resolves deterministically to one path, so the ≤2 non-`mpeg`
/// candidates are unambiguous; `mpeg` is always included as the fallback.
pub(super) fn candidates(widevine_forced: bool, xhe: bool, quality: Quality) -> Vec<String> {
    let codec = codec_tag(xhe);
    let q = quality_tag(quality);
    let mut kinds = if widevine_forced {
        vec![format!("widevine-{codec}-{q}")]
    } else {
        vec![format!("adrm-{q}"), format!("widevine-{codec}-{q}")]
    };
    kinds.push("mpeg".to_string());
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_keys() {
        assert_eq!(resolved(Grant::Adrm, false, Quality::High), "adrm-high");
        assert_eq!(resolved(Grant::Adrm, true, Quality::Normal), "adrm-normal");
        assert_eq!(
            resolved(Grant::Widevine, false, Quality::Normal),
            "widevine-aac-normal"
        );
        assert_eq!(
            resolved(Grant::Widevine, true, Quality::High),
            "widevine-xhe-high"
        );
        assert_eq!(resolved(Grant::Mpeg, true, Quality::High), "mpeg");
    }

    #[test]
    fn candidate_sets() {
        // Default (auto): aaxc or the widevine-<codec> fallback, plus mpeg.
        assert_eq!(
            candidates(false, false, Quality::High),
            ["adrm-high", "widevine-aac-high", "mpeg"]
        );
        assert_eq!(
            candidates(false, true, Quality::Normal),
            ["adrm-normal", "widevine-xhe-normal", "mpeg"]
        );
        // Forced Widevine: exactly the one widevine kind, plus mpeg.
        assert_eq!(
            candidates(true, false, Quality::Normal),
            ["widevine-aac-normal", "mpeg"]
        );
    }

    /// `ALL` covers every kind the derivation can produce (the CLI validation
    /// set must accept exactly what record/lookup time can emit).
    #[test]
    fn all_covers_every_derivable_kind() {
        for widevine in [false, true] {
            for xhe in [false, true] {
                for q in [Quality::High, Quality::Normal] {
                    for kind in candidates(widevine, xhe, q) {
                        assert!(ALL.contains(&kind.as_str()), "{kind} missing from ALL");
                    }
                }
            }
        }
    }

    /// Every resolvable outcome of a request is inside that request's candidates.
    #[test]
    fn resolved_is_always_a_candidate() {
        for widevine in [false, true] {
            for xhe in [false, true] {
                for q in [Quality::High, Quality::Normal] {
                    let cands = candidates(widevine, xhe, q);
                    // Widevine + Mpeg are reachable from any request; Adrm only
                    // from the default (non-forced) path.
                    assert!(cands.contains(&resolved(Grant::Widevine, xhe, q)));
                    assert!(cands.contains(&resolved(Grant::Mpeg, xhe, q)));
                    if !widevine {
                        assert!(cands.contains(&resolved(Grant::Adrm, xhe, q)));
                    }
                }
            }
        }
    }
}
