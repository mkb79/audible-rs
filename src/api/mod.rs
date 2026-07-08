//! Audible API access: HTTP client with per-request `AuthMode`, locale
//! handling (host URL construction from the profile) and response
//! pagination as streams.

pub mod client;
pub mod locale;
pub mod paginator;
