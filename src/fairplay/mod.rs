//! FairPlay Streaming support (AUD-321) — **experimental, macOS only**: the
//! whole module exists only on macOS.
//!
//! Audible serves some titles (AYCL/Plus among them) as FairPlay-protected
//! HLS, the format its Apple apps play. Unlike Adrm (aaxc) and Widevine,
//! FairPlay yields nothing decryptable: the content keys never leave Apple's
//! media stack, so a download is an AVFoundation package that only plays
//! through AVFoundation with its stored key — no m4b, no export.
//!
//! The module is deliberately self-contained. It carries its own copy of the
//! few licenserequest pieces it needs instead of sharing them with the Adrm
//! and Widevine paths in `downloader`, so the experimental code cannot change
//! the main download paths; it is reached only through `audible fairplay`
//! and by apps that use the library. Should it ever join the main paths, the
//! duplicates can be folded together then.
//!
//! [`license`] holds the protocol (content license, provider certificate,
//! SPC → CKC exchange) and [`hls`] the playlist walk to the `skd://` key
//! URIs. [`apple`] is the AVFoundation half — key session, package
//! download, player — and [`driver`] wraps it in blocking calls.
//!
//! Nothing secret is logged: the playlist URLs are signed, and the SPC, the
//! CKC and the persistable keys are DRM material.

pub mod apple;
pub mod driver;
pub mod hls;
pub mod license;

pub use hls::{Discovery, Playlist, Rendition, Variant, content_id, discover};
pub use license::{
    Consumption, FairPlayGrant, LicenseOptions, Quality, request_certificate, request_ckc,
    request_license,
};

/// Errors of the FairPlay protocol layer.
#[derive(Debug, thiserror::Error)]
pub enum FairPlayError {
    /// The API client failed (auth material, transport, marketplace).
    #[error(transparent)]
    Api(#[from] crate::api::client::ApiError),
    /// Reading a response or fetching a playlist failed at the HTTP layer.
    /// Playlist errors never carry their (signed) URL.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server denied the license (`status_code: "Denied"`).
    #[error("FairPlay license for {asin:?} denied: {message}")]
    Denied {
        /// The requested item.
        asin: String,
        /// The server's reason.
        message: String,
    },
    /// The license was granted, but not as FairPlay.
    #[error("{asin:?} is not available as FairPlay (the server granted {drm_type})")]
    NotFairPlay {
        /// The requested item.
        asin: String,
        /// The DRM type the server granted instead (`none` when absent).
        drm_type: String,
    },
    /// An endpoint answered with an error status. The server's `error_code`
    /// and `message` are passed on verbatim (these bodies carry no
    /// credentials), with the echoed request id for reports to Amazon.
    #[error(
        "{target} failed (HTTP {status}, error_code {error_code:?}, request id {request_id}): {message}"
    )]
    Rejected {
        /// What was requested, e.g. `licenserequest for B0EXAMPLE1`.
        target: String,
        /// HTTP status returned by the endpoint.
        status: reqwest::StatusCode,
        /// Server-supplied `error_code` (empty when the body had none).
        error_code: String,
        /// Server-supplied message (or a truncated body).
        message: String,
        /// The echoed `x-amzn-requestid` (`-` when absent).
        request_id: String,
    },
    /// An endpoint answered 2xx, but not in the expected shape.
    #[error("{target} returned an unexpected response: {reason}")]
    Unexpected {
        /// What was requested.
        target: String,
        /// What was missing or malformed.
        reason: &'static str,
    },
    /// The HLS playlists did not lead to a FairPlay key.
    #[error("HLS playlist: {0}")]
    Playlist(&'static str),
}
