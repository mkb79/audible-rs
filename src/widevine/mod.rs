//! Widevine DRM support (AUD-56): the native CDM client for the DASH/CENC
//! download path (Widevine-DRM'd audio, incl. Dolby Atmos). No Python /
//! pywidevine runtime — the device blob, license protocol and key derivation
//! are reimplemented here in Rust.
//!
//! This module holds only the Widevine-specific pieces; the licenserequest /
//! MPD / download / decrypt wiring lives in `downloader` and
//! `commands/download`. All heavy crypto (RSA, AES) runs via `spawn_blocking`.

pub mod client;
pub mod device;
pub mod mpd;
mod proto;
pub mod provider;
pub mod pssh;

pub use client::{Cdm, Challenge, ClientError, ContentKey};
pub use device::{Device, DeviceType, WvdError};
pub use mpd::{MpdError, WidevineStream};
