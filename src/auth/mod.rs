//! Authentication: the [`Authenticator`] holding an account's auth
//! material (wrapped in `secrecy`/`zeroize`), login and device
//! registration flows, request signing, activation bytes and password
//! sources (`prompt | keyring | env`).
//!
//! The on-disk auth data (the `data` part of the envelope, §6) is
//! grouped to mirror Amazon's own register response: `identity`,
//! `device`, `signing` (mac_dms), `bearer` and domain-keyed
//! `website_cookies`. `identity`/`device` keep a flattened `extra`
//! catch-all so no unknown field is ever lost.
//
// NOTE(M4 register): the register request must NOT request the
// `store_authentication_cookie` extension (legacy FIRS/Kindle-store
// device cookie) — we deliberately do not store it; the modern app
// does not request it either.

pub mod authfile;
pub mod cookies;
pub mod device;
pub mod legacy;
pub mod login;
pub mod signing;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::api::locale::{self, Locale};
use authfile::{AuthFileError, KdfParams, Protection};
use legacy::LegacyError;
use signing::{RequestSigner, SigningError};

/// `account_pool` prefix that marks a pre-merger Audible account.
const AUDIBLE_LEGACY_POOL_PREFIX: &str = "pool-";

/// Refresh the access token this many seconds before its expiry (A7): a
/// token expiring mid-request would fail once, as there is no 401 retry.
const TOKEN_REFRESH_MARGIN_SECS: f64 = 60.0;

/// Origin of the account's identity, derived from `account_pool`.
///
/// Amazon's register response carries `customer_info.account_pool`:
/// `"Amazon"` (and e.g. `"AmazonCN"`) for a regular Amazon identity,
/// `"pool-<marketplace_id>"` for a pre-merger Audible account. The
/// latter authenticates against `audible.{domain}` instead of
/// `amazon.{domain}` (the `with_username` flag in the Python lib). Such
/// accounts can be merged with an Amazon account:
/// <https://help.audible.ca/s/article/merge-audible-and-amazon-accounts>
//
// `account import` prints a merge hint for such accounts. TODO(M4): do
// the same in `account login` (and cover it in docs/migration.md, M8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountOrigin {
    /// Regular Amazon identity.
    Amazon,
    /// Pre-merger Audible identity (`account_pool` is `pool-…`).
    AudibleLegacy,
}

impl AccountOrigin {
    /// Derives the origin from Amazon's `account_pool` value.
    pub fn from_account_pool(account_pool: &str) -> Self {
        if account_pool.starts_with(AUDIBLE_LEGACY_POOL_PREFIX) {
            AccountOrigin::AudibleLegacy
        } else {
            AccountOrigin::Amazon
        }
    }

    /// Base URL of the auth API (token refresh, registration) for this
    /// identity on the given marketplace.
    pub fn auth_url(&self, locale: &Locale) -> String {
        match self {
            AccountOrigin::Amazon => locale.amazon_auth_url(),
            AccountOrigin::AudibleLegacy => locale.audible_auth_url(),
        }
    }
}

/// Errors raised while loading, using or saving auth material.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Reading or writing the new auth file format failed.
    #[error(transparent)]
    File(#[from] AuthFileError),
    /// Reading a legacy Python auth file failed.
    #[error(transparent)]
    Legacy(#[from] LegacyError),
    /// The device private key could not be parsed.
    #[error(transparent)]
    Signing(#[from] SigningError),
    /// The auth data does not match the expected schema.
    #[error("invalid auth data: {0}")]
    InvalidData(String),
    /// The auth data references an unknown marketplace.
    #[error("unknown marketplace country code {0:?}")]
    UnknownLocale(String),
    /// The file is not in the audible-rs format. Live auth material is
    /// always in the new format; legacy Python files are import-only.
    #[error(
        "not an audible-rs auth file — legacy Python files must be \
         converted first (`audible account import <file>`)"
    )]
    LegacyFormat,
    /// File IO failed.
    #[error("auth file IO failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Serialized auth data — the `data` part of an auth file (§6), grouped
/// to mirror the register response.
#[derive(Serialize, Deserialize)]
struct AuthData {
    country_code: String,
    #[serde(default)]
    identity: Identity,
    #[serde(default)]
    device: Device,
    #[serde(default)]
    signing: Signing,
    #[serde(default)]
    bearer: Bearer,
    /// Cookies keyed by domain (`.amazon.de`), the cookie-exchange shape.
    #[serde(default)]
    website_cookies: BTreeMap<String, Vec<Cookie>>,
    /// Absolute expiry (Unix epoch seconds) of the exchanged cookies per
    /// domain, from the exchange's `tokens.ttl`. A domain present in
    /// `website_cookies` but absent here (a registration/import, which carries
    /// no TTL) is treated as needing a fresh exchange on first use.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cookie_ttls: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation_bytes: Option<String>,
}

/// Account identity (`customer_info`). Unknown fields are preserved.
#[derive(Clone, Serialize, Deserialize)]
struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    customer_id: Option<String>,
    /// Amazon identity pool; `pool-…` marks a pre-merger Audible account.
    #[serde(default = "default_account_pool")]
    account_pool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home_region: Option<String>,
    /// Catch-all for fields Amazon sends but we do not model
    /// (`account_pool` aside: e.g. `given_name`) — lossless round-trip.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            customer_id: None,
            account_pool: default_account_pool(),
            name: None,
            home_region: None,
            extra: serde_json::Map::new(),
        }
    }
}

fn default_account_pool() -> String {
    "Amazon".to_owned()
}

/// Device identity (`device_info`). Unknown fields are preserved.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Device {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// Signing material (`mac_dms`).
#[derive(Default, Serialize, Deserialize)]
struct Signing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adp_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_private_key: Option<String>,
}

/// Bearer tokens.
#[derive(Default, Serialize, Deserialize)]
struct Bearer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// Absolute expiry of the access token (Unix epoch seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<f64>,
}

/// One website cookie. Only `name`/`value` are required; the rest
/// tolerate absence so legacy imports (flat `name→value`) still work.
#[derive(Clone, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Path scope.
    #[serde(default = "default_cookie_path")]
    pub path: String,
    /// Secure flag.
    #[serde(default = "default_true")]
    pub secure: bool,
    /// Expiry as the raw server string; `null` when unknown (e.g. a
    /// legacy import). Always serialized as a placeholder so every
    /// cookie has the same, self-documenting shape.
    #[serde(default)]
    pub expires: Option<String>,
    /// HttpOnly flag; `null` when unknown. Always serialized.
    #[serde(default)]
    pub http_only: Option<bool>,
}

fn default_cookie_path() -> String {
    "/".to_owned()
}

fn default_true() -> bool {
    true
}

/// Current Unix time in seconds (fractional), for cookie-TTL comparisons.
fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Freshness of the website cookies that apply to a request host, used to
/// decide whether a lazy cookie exchange is needed before sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieFreshness {
    /// Matching cookies exist and none has a known-past expiry; the value is
    /// the ready-to-send `Cookie` header line. Cookies without an `Expires`
    /// (a registration/import carries no TTL) count as fresh — they are never
    /// treated as stale because their lifetime is unknown.
    Fresh(String),
    /// Matching cookies exist, but at least one carries a real `Expires` in
    /// the past: the short-lived exchange cookies (`at-*`/`sess-at-*`) have
    /// lapsed even though longer-lived ones (e.g. `session-id`) remain. A
    /// fresh exchange is needed.
    Stale,
    /// No cookie matches the host (nor its Amazon↔Audible SSO sibling).
    Absent,
}

/// Legacy Python auth data (`Authenticator.to_dict` layout), read only
/// during `account import` and mapped onto the grouped format.
#[derive(Deserialize)]
struct LegacyAuthData {
    adp_token: Option<String>,
    device_private_key: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires: Option<f64>,
    locale_code: String,
    #[serde(default)]
    with_username: bool,
    // Very old files used `login_cookies` instead of `website_cookies`.
    #[serde(default, alias = "login_cookies")]
    website_cookies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    device_info: serde_json::Value,
    #[serde(default)]
    customer_info: serde_json::Value,
    #[serde(default)]
    activation_bytes: Option<String>,
}

/// The field group a background write-back owns (audit 2026-07-18, A6):
/// a refresh/exchange persists **only** these, merged onto the current
/// on-disk state, so it cannot roll back a concurrent CLI edit to an
/// unrelated field. See [`Authenticator::save_merged`].
pub enum MergeScope {
    /// The refreshed access token and its expiry. The refresh token is
    /// left as on disk — a token refresh never changes it.
    BearerToken,
    /// The exchanged website cookies and their TTLs for exactly these
    /// domains; every other domain is left as on disk.
    Cookies(Vec<String>),
}

/// Overwrites `base` with the fields `mine` owns per `scope`, leaving
/// everything else in `base` (the on-disk state) untouched.
fn merge_scope(base: &mut AuthData, mut mine: AuthData, scope: &MergeScope) {
    match scope {
        MergeScope::BearerToken => {
            base.bearer.access_token = mine.bearer.access_token;
            base.bearer.expires = mine.bearer.expires;
        }
        MergeScope::Cookies(domains) => {
            for domain in domains {
                match mine.website_cookies.remove(domain) {
                    Some(jar) => {
                        base.website_cookies.insert(domain.clone(), jar);
                    }
                    None => {
                        base.website_cookies.remove(domain);
                    }
                }
                match mine.cookie_ttls.remove(domain) {
                    Some(ttl) => {
                        base.cookie_ttls.insert(domain.clone(), ttl);
                    }
                    None => {
                        base.cookie_ttls.remove(domain);
                    }
                }
            }
        }
    }
}

/// Where and how a loaded auth file is written back after token
/// refreshes or cookie updates (§6 write-back).
struct WriteBack {
    path: PathBuf,
    protection: Protection,
    /// Kept (secrecy-wrapped) so the file can be re-encrypted without a
    /// new prompt; every write uses a fresh salt and nonce anyway.
    password: Option<SecretString>,
}

/// An account's authentication material.
///
/// Constructed from an auth file ([`Authenticator::load_file`]) or from
/// raw auth data. The device key is parsed once into a [`RequestSigner`]
/// and shared from there (§10). Secrets are `secrecy`-wrapped; the
/// non-secret identity/device/cookies round-trip losslessly.
pub struct Authenticator {
    adp_token: Option<SecretString>,
    device_private_key: Option<SecretString>,
    access_token: Option<SecretString>,
    refresh_token: Option<SecretString>,
    expires: Option<f64>,
    locale: Locale,
    identity: Identity,
    device: Device,
    website_cookies: BTreeMap<String, Vec<Cookie>>,
    cookie_ttls: BTreeMap<String, f64>,
    activation_bytes: Option<String>,
    signer: Option<Arc<RequestSigner>>,
    write_back: Option<WriteBack>,
}

impl fmt::Debug for Authenticator {
    // Holds credentials; never derive Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator")
            .field("locale", &self.locale.country_code)
            .field("origin", &self.origin())
            .field("has_signing_material", &self.signer.is_some())
            .finish_non_exhaustive()
    }
}

impl Authenticator {
    fn from_auth_data(data: AuthData) -> Result<Self, AuthError> {
        let locale = locale::find(&data.country_code)
            .ok_or_else(|| AuthError::UnknownLocale(data.country_code.clone()))?;

        let signer = match (&data.signing.adp_token, &data.signing.device_private_key) {
            (Some(adp_token), Some(key_pem)) => {
                Some(Arc::new(RequestSigner::new(key_pem, adp_token.clone())?))
            }
            _ => None,
        };

        Ok(Self {
            adp_token: data.signing.adp_token.map(SecretString::from),
            device_private_key: data.signing.device_private_key.map(SecretString::from),
            access_token: data.bearer.access_token.map(SecretString::from),
            refresh_token: data.bearer.refresh_token.map(SecretString::from),
            expires: data.bearer.expires,
            locale,
            identity: data.identity,
            device: data.device,
            website_cookies: data.website_cookies,
            cookie_ttls: data.cookie_ttls,
            activation_bytes: data.activation_bytes,
            signer,
            write_back: None,
        })
    }

    /// Builds an authenticator from auth data in the new format.
    pub fn from_value(data: serde_json::Value) -> Result<Self, AuthError> {
        let data: AuthData =
            serde_json::from_value(data).map_err(|err| AuthError::InvalidData(err.to_string()))?;
        Self::from_auth_data(data)
    }

    /// Builds an authenticator from legacy Python auth data, mapping the
    /// flat layout onto the grouped format. `with_username` becomes
    /// `account_pool` (`pool-<marketplace_id>` when true), flat cookies
    /// move into the account's `.amazon.{domain}` bucket.
    pub fn from_legacy_value(data: serde_json::Value) -> Result<Self, AuthError> {
        let legacy: LegacyAuthData =
            serde_json::from_value(data).map_err(|err| AuthError::InvalidData(err.to_string()))?;
        let locale = locale::find(&legacy.locale_code)
            .ok_or_else(|| AuthError::UnknownLocale(legacy.locale_code.clone()))?;

        // Build `identity` from the whole `customer_info` object, exactly like
        // the register flow (`login::build_authenticator`): rename `user_id` ->
        // `customer_id` and let every other field survive through the Identity
        // `extra` catch-all (e.g. `given_name`), so a legacy import round-trips
        // the same fields a fresh registration does. Python auth files carry
        // the authoritative `account_pool`; only ancient files that lack it
        // fall back to `with_username`.
        let mut identity_obj = legacy
            .customer_info
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(user_id) = identity_obj.remove("user_id") {
            identity_obj.insert("customer_id".to_owned(), user_id);
        }
        identity_obj
            .entry("account_pool".to_owned())
            .or_insert_with(|| {
                let pool = if legacy.with_username {
                    format!("{AUDIBLE_LEGACY_POOL_PREFIX}{}", locale.market_place_id)
                } else {
                    default_account_pool()
                };
                serde_json::Value::String(pool)
            });
        let identity: Identity = serde_json::from_value(serde_json::Value::Object(identity_obj))
            .map_err(|err| AuthError::InvalidData(err.to_string()))?;

        // Same for `device`: rename the two keys the model lifts into explicit
        // fields; every other `device_info` key survives via `extra`.
        let mut device_obj = legacy.device_info.as_object().cloned().unwrap_or_default();
        if let Some(serial) = device_obj.remove("device_serial_number") {
            device_obj.insert("serial".to_owned(), serial);
        }
        if let Some(name) = device_obj.remove("device_name") {
            device_obj.insert("name".to_owned(), name);
        }
        let device: Device = serde_json::from_value(serde_json::Value::Object(device_obj))
            .map_err(|err| AuthError::InvalidData(err.to_string()))?;

        // Flat name->value cookies have no domain; bucket them under the
        // account marketplace's Amazon domain.
        let mut website_cookies = BTreeMap::new();
        if let Some(flat) = legacy.website_cookies.filter(|m| !m.is_empty()) {
            let domain = format!(".amazon.{}", locale.domain);
            let cookies = flat
                .into_iter()
                .map(|(name, value)| Cookie {
                    name,
                    value,
                    path: default_cookie_path(),
                    secure: true,
                    expires: None,
                    http_only: None,
                })
                .collect();
            website_cookies.insert(domain, cookies);
        }

        Self::from_auth_data(AuthData {
            country_code: legacy.locale_code,
            identity,
            device,
            signing: Signing {
                adp_token: legacy.adp_token,
                device_private_key: legacy.device_private_key,
            },
            bearer: Bearer {
                access_token: legacy.access_token,
                refresh_token: legacy.refresh_token,
                expires: legacy.expires,
            },
            website_cookies,
            // A legacy import has no exchange TTL; the first cookie request
            // exchanges to obtain a session with a known lifetime.
            cookie_ttls: BTreeMap::new(),
            activation_bytes: legacy.activation_bytes,
        })
    }

    /// Loads an auth file in the audible-rs format (plain or encrypted
    /// envelope) and configures a write-back so token refreshes update
    /// the file in place.
    ///
    /// Legacy Python files are rejected: live auth material is always in
    /// the new format — convert with `audible account import` (which
    /// uses [`Self::import_file`]).
    pub async fn load_file(
        path: impl Into<PathBuf>,
        password: Option<SecretString>,
    ) -> Result<Self, AuthError> {
        let path = path.into();
        let content = read_file(path.clone()).await?;

        // CPU-bound work (Argon2id) runs off the async executor.
        tokio::task::spawn_blocking(move || {
            if !is_new_format(&content) {
                return Err(AuthError::LegacyFormat);
            }
            Self::load_new_format_sync(&content, password, Some(path))
        })
        .await
        .expect("blocking load task must not panic")
    }

    /// Loads a file for `account import`: accepts the audible-rs format
    /// (re-registration of an existing file) and all legacy Python
    /// variants. Never configures a write-back — the import writes the
    /// data to its new home via [`Self::save_to`].
    pub async fn import_file(
        path: impl Into<PathBuf>,
        password: Option<SecretString>,
    ) -> Result<Self, AuthError> {
        let content = read_file(path.into()).await?;

        // CPU-bound work (Argon2id/PBKDF2) runs off the async executor.
        tokio::task::spawn_blocking(move || {
            if is_new_format(&content) {
                Self::load_new_format_sync(&content, password, None)
            } else {
                let data = legacy::read(&content, password.as_ref())?;
                tracing::debug!("read legacy Python auth data for import");
                Self::from_legacy_value(data)
            }
        })
        .await
        .expect("blocking load task must not panic")
    }

    fn load_new_format_sync(
        content: &[u8],
        password: Option<SecretString>,
        write_back_path: Option<PathBuf>,
    ) -> Result<Self, AuthError> {
        let text = std::str::from_utf8(content)
            .map_err(|_| AuthError::InvalidData("auth file is not UTF-8".into()))?;
        let loaded = authfile::read(text, password.as_ref())?;
        tracing::info!(
            encrypted = matches!(loaded.protection, Protection::Encrypted(_)),
            "loaded auth file (audible-rs format)"
        );
        let mut auth = Self::from_value(loaded.data)?;
        if let Some(path) = write_back_path {
            auth.write_back = Some(WriteBack {
                path,
                protection: loaded.protection,
                password,
            });
        }
        Ok(auth)
    }

    /// Serializes the auth data (new format). Exposes all secrets — only
    /// for writing auth files.
    fn to_value(&self) -> serde_json::Value {
        let expose =
            |secret: &Option<SecretString>| secret.as_ref().map(|s| s.expose_secret().to_owned());
        serde_json::to_value(AuthData {
            country_code: self.locale.country_code.to_owned(),
            identity: self.identity.clone(),
            device: self.device.clone(),
            signing: Signing {
                adp_token: expose(&self.adp_token),
                device_private_key: expose(&self.device_private_key),
            },
            bearer: Bearer {
                access_token: expose(&self.access_token),
                refresh_token: expose(&self.refresh_token),
                expires: self.expires,
            },
            website_cookies: self.website_cookies.clone(),
            cookie_ttls: self.cookie_ttls.clone(),
            activation_bytes: self.activation_bytes.clone(),
        })
        .expect("AuthData always serializes")
    }

    /// The auth data in the new (grouped) format, for `account export`
    /// (lib guarantee: auth material serializes without a file). Exposes
    /// all secrets — only for writing export files; never log it.
    pub fn export_value(&self) -> serde_json::Value {
        self.to_value()
    }

    /// The auth data in the legacy Python `Authenticator.to_dict` layout —
    /// the inverse of [`Self::from_legacy_value`] — so an exported account
    /// can be used from the Python `audible`/`audible-cli` again.
    ///
    /// Deliberately lossy for cookies: the Python format stores one flat
    /// `name→value` map, so only the account marketplace's `.amazon.<tld>`
    /// bucket is exported and cookie attributes are dropped. The second
    /// element lists the domains that were left behind, for a caller-side
    /// warning. Exposes all secrets — only for writing export files.
    pub fn export_legacy_value(&self) -> (serde_json::Value, Vec<String>) {
        let expose =
            |secret: &Option<SecretString>| secret.as_ref().map(|s| s.expose_secret().to_owned());

        // customer_info / device_info: explicit fields on top of the
        // lossless `extra` passthrough (the reverse of the import mapping).
        let mut customer_info = self.identity.extra.clone();
        if let Some(customer_id) = &self.identity.customer_id {
            customer_info.insert("user_id".into(), customer_id.clone().into());
        }
        customer_info.insert(
            "account_pool".into(),
            self.identity.account_pool.clone().into(),
        );
        if let Some(name) = &self.identity.name {
            customer_info.insert("name".into(), name.clone().into());
        }
        if let Some(home_region) = &self.identity.home_region {
            customer_info.insert("home_region".into(), home_region.clone().into());
        }

        let mut device_info = self.device.extra.clone();
        if let Some(device_type) = &self.device.device_type {
            device_info.insert("device_type".into(), device_type.clone().into());
        }
        if let Some(serial) = &self.device.serial {
            device_info.insert("device_serial_number".into(), serial.clone().into());
        }
        if let Some(name) = &self.device.name {
            device_info.insert("device_name".into(), name.clone().into());
        }

        // Flat cookies: the account marketplace's Amazon bucket only.
        let bucket = format!(".amazon.{}", self.locale.domain);
        let cookies: serde_json::Map<String, serde_json::Value> = self
            .website_cookies
            .get(&bucket)
            .map(|jar| {
                jar.iter()
                    .map(|cookie| (cookie.name.clone(), cookie.value.clone().into()))
                    .collect()
            })
            .unwrap_or_default();
        let dropped: Vec<String> = self
            .website_cookies
            .keys()
            .filter(|domain| **domain != bucket)
            .cloned()
            .collect();

        let value = serde_json::json!({
            "website_cookies": cookies,
            "adp_token": expose(&self.adp_token),
            "access_token": expose(&self.access_token),
            "refresh_token": expose(&self.refresh_token),
            "device_private_key": expose(&self.device_private_key),
            // We never store the legacy FIRS cookie (see the register note);
            // the key is kept so the shape matches Python's to_dict exactly.
            "store_authentication_cookie": serde_json::Value::Null,
            "device_info": device_info,
            "customer_info": customer_info,
            "expires": self.expires,
            "locale_code": self.locale.country_code,
            "with_username": self.origin() == AccountOrigin::AudibleLegacy,
            "activation_bytes": self.activation_bytes,
        });
        (value, dropped)
    }

    /// Writes the current auth data back to the file it was loaded from,
    /// preserving its protection (fresh salt and nonce per write).
    ///
    /// Whole-file: for **foreground, user-initiated** edits (`account
    /// token remove`, `cookies remove`, `activation-bytes --fetch`,
    /// `logout`) where this `Authenticator` was just loaded and the user
    /// is present. A background refresh/exchange must NOT use this — see
    /// [`Self::save_merged`] (A6).
    ///
    /// No-op for auth data without a write-back target (legacy files,
    /// in-memory construction).
    pub async fn save(&self) -> Result<(), AuthError> {
        let Some(write_back) = &self.write_back else {
            tracing::debug!("no write-back target; skipping auth file save");
            return Ok(());
        };

        let data = self.to_value();
        let protection = write_back.protection;
        let password = write_back.password.clone();
        let path = write_back.path.clone();

        tokio::task::spawn_blocking(move || {
            let content = authfile::write(&data, protection, password.as_ref())?;
            atomic_write(&path, content.as_bytes())?;
            tracing::info!(path = %path.display(), "auth file updated (write-back)");
            Ok(())
        })
        .await
        .expect("blocking save task must not panic")
    }

    /// Persists only the fields named by `scope`, merged onto the current
    /// **on-disk** auth data under the write lock (audit 2026-07-18, A6).
    ///
    /// A background refresh/exchange runs against an in-memory
    /// `Authenticator` that may be hours old — the agent holds one per
    /// session for the session's lifetime. The plain [`Self::save`] would
    /// serialize that whole stale copy and roll back any CLI edit made
    /// since it loaded (a removed access token / cookie, freshly stored
    /// activation bytes silently reappear or vanish). Re-reading the file
    /// under the lock and overwriting only the just-changed group closes
    /// the lost-update gap while the fsutil lock keeps it torn-write-safe.
    ///
    /// A missing file falls back to a full write; a no-op without a
    /// write-back target.
    pub async fn save_merged(&self, scope: MergeScope) -> Result<(), AuthError> {
        let Some(write_back) = &self.write_back else {
            tracing::debug!("no write-back target; skipping auth file save");
            return Ok(());
        };

        let mine = self.to_value();
        let protection = write_back.protection;
        let password = write_back.password.clone();
        let path = write_back.path.clone();

        tokio::task::spawn_blocking(move || {
            // The lock spans read→write so no concurrent writer slips a
            // change in between (fsutil B5 contract; the plain write-back
            // held it only around the final persist — the A6 gap).
            let mut lock = crate::fsutil::write_lock(&path)?;
            let _guard = lock.write()?;

            let mut base: AuthData = match std::fs::read(&path) {
                Ok(bytes) => {
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|_| AuthError::InvalidData("auth file is not UTF-8".into()))?;
                    let loaded = authfile::read(text, password.as_ref())?;
                    serde_json::from_value(loaded.data)
                        .map_err(|err| AuthError::InvalidData(err.to_string()))?
                }
                // Deleted since load: recreate it from our full state.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    serde_json::from_value(mine.clone())
                        .map_err(|err| AuthError::InvalidData(err.to_string()))?
                }
                Err(err) => return Err(AuthError::Io(err)),
            };
            let mine: AuthData = serde_json::from_value(mine)
                .map_err(|err| AuthError::InvalidData(err.to_string()))?;
            merge_scope(&mut base, mine, &scope);

            let merged = serde_json::to_value(base).expect("AuthData always serializes");
            let content = authfile::write(&merged, protection, password.as_ref())?;
            crate::fsutil::persist_atomically(&path, content.as_bytes(), Some(0o600))?;
            tracing::info!(path = %path.display(), "auth file updated (merged write-back)");
            Ok(())
        })
        .await
        .expect("blocking save task must not panic")
    }

    /// Writes the auth data as a new-format file to `path`: encrypted
    /// when a password is given (`account import`), plain otherwise.
    pub async fn save_to(
        &self,
        path: impl Into<PathBuf>,
        password: Option<SecretString>,
        params: KdfParams,
    ) -> Result<(), AuthError> {
        let path = path.into();
        let data = self.to_value();
        let protection = match password {
            Some(_) => Protection::Encrypted(params),
            None => Protection::Plain,
        };

        tokio::task::spawn_blocking(move || {
            let content = authfile::write(&data, protection, password.as_ref())?;
            atomic_write(&path, content.as_bytes())?;
            Ok(())
        })
        .await
        .expect("blocking save task must not panic")
    }

    /// Stores a refreshed access token and its expiry timestamp.
    pub fn apply_token_refresh(&mut self, access_token: SecretString, expires: f64) {
        self.access_token = Some(access_token);
        self.expires = Some(expires);
    }

    /// Whether the access token needs refreshing: missing, past its
    /// expiry, or within [`TOKEN_REFRESH_MARGIN_SECS`] of it. The margin
    /// (audit 2026-07-18, A7) keeps a token that would expire mid-flight
    /// from being sent — there is no 401-driven retry, so a request on a
    /// one-second-valid token would just fail once.
    pub fn access_token_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs_f64();
        match (&self.access_token, self.expires) {
            (Some(_), Some(expires)) => now >= expires - TOKEN_REFRESH_MARGIN_SECS,
            _ => true,
        }
    }

    /// The request signer, present when the file carried signing material.
    pub fn signer(&self) -> Option<&Arc<RequestSigner>> {
        self.signer.as_ref()
    }

    /// The account's access token, if any.
    pub fn access_token(&self) -> Option<&SecretString> {
        self.access_token.as_ref()
    }

    /// The account's refresh token, if any.
    pub fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    /// Absolute expiry of the access token (Unix epoch seconds), if set.
    pub fn expires(&self) -> Option<f64> {
        self.expires
    }

    /// The stored activation bytes (legacy `.aax` decryption key), if any.
    pub fn activation_bytes(&self) -> Option<&str> {
        self.activation_bytes.as_deref()
    }

    /// Stores freshly fetched activation bytes. Persist with [`Self::save`].
    pub fn set_activation_bytes(&mut self, activation_bytes: String) {
        self.activation_bytes = Some(activation_bytes);
    }

    /// Marketplace the account was registered in.
    pub fn locale(&self) -> &Locale {
        &self.locale
    }

    /// Identity origin (Amazon or pre-merger Audible), derived from
    /// `account_pool`.
    pub fn origin(&self) -> AccountOrigin {
        AccountOrigin::from_account_pool(&self.identity.account_pool)
    }

    /// Stored website cookies, keyed by domain.
    pub fn website_cookies(&self) -> &BTreeMap<String, Vec<Cookie>> {
        &self.website_cookies
    }

    /// The recorded exchange-TTL expiry (Unix epoch seconds) for `domain`, or
    /// `None` when the cookies were never exchanged (a registration/import).
    /// Read-only view for `account cookies show`.
    pub fn cookie_ttl(&self, domain: &str) -> Option<f64> {
        self.cookie_ttls.get(domain).copied()
    }

    /// Replaces the cookie bucket for one domain, keeping the other domains
    /// (multi-domain). Used by the cookie exchange; persist with [`Self::save`].
    pub fn set_website_cookies(&mut self, domain: String, cookies: Vec<Cookie>) {
        self.website_cookies.insert(domain, cookies);
    }

    /// Records the absolute expiry (Unix epoch seconds) of `domain`'s exchanged
    /// cookies, derived from the exchange's `tokens.ttl`. Drives the lazy
    /// re-exchange in [`Self::cookie_status`]. Persist with [`Self::save`].
    pub fn set_cookie_ttl(&mut self, domain: String, expiry_unix: f64) {
        self.cookie_ttls.insert(domain, expiry_unix);
    }

    /// Removes the cookie bucket (and its TTL) for one domain. Returns whether
    /// it existed.
    pub fn remove_website_cookies(&mut self, domain: &str) -> bool {
        self.cookie_ttls.remove(domain);
        self.website_cookies.remove(domain).is_some()
    }

    /// Removes all stored website cookies (and their TTLs).
    pub fn clear_website_cookies(&mut self) {
        self.cookie_ttls.clear();
        self.website_cookies.clear();
    }

    /// Removes the access token and its expiry. The refresh token is kept
    /// on purpose — it is required to mint new access tokens, exchange
    /// website cookies and (via a fresh access token) deregister the
    /// device. Persist with [`Self::save`].
    pub fn clear_access_token(&mut self) {
        self.access_token = None;
        self.expires = None;
    }

    /// Classifies the website cookies that apply to `host` as
    /// [`CookieFreshness`]: `Fresh` (sendable header), `Stale` (a matching
    /// bucket's exchange TTL is missing or has lapsed → re-exchange) or
    /// `Absent` (no bucket matches).
    ///
    /// Freshness is driven by the per-domain exchange TTL (`tokens.ttl`), not
    /// the per-cookie `Expires` (which the server inflates). A bucket with no
    /// recorded TTL — a registration/import — is `Stale`, so the first cookie
    /// request exchanges to obtain a session with a known lifetime.
    ///
    /// Matching is Amazon↔Audible SSO-aware within a marketplace: a request to
    /// `www.audible.de` also matches stored `.amazon.de` cookies (and vice
    /// versa), because the exchange only returns `.amazon.<tld>` cookies yet
    /// those session cookies are valid on the sibling Audible host. Cookies are
    /// still never sent to any unrelated host.
    pub fn cookie_status(&self, host: &str) -> CookieFreshness {
        let sibling = cookies::sso_sibling_host(host);
        let matched: Vec<(&String, &Vec<Cookie>)> = self
            .website_cookies
            .iter()
            .filter(|(domain, _)| {
                cookies::host_matches_domain(host, domain)
                    || sibling
                        .as_deref()
                        .is_some_and(|sibling| cookies::host_matches_domain(sibling, domain))
            })
            .collect();
        if matched.is_empty() {
            return CookieFreshness::Absent;
        }
        // Stale if any matched bucket's exchange TTL is missing (never
        // exchanged) or has lapsed.
        let now = now_unix();
        let stale = matched.iter().any(|(domain, _)| {
            self.cookie_ttls
                .get(*domain)
                .is_none_or(|&expiry| now >= expiry)
        });
        if stale {
            return CookieFreshness::Stale;
        }
        // Fresh: send the host-scoped cookies, still dropping any individually
        // expired by their real server `Expires`. An empty result (all lapsed)
        // forces an exchange rather than sending nothing.
        let chosen: Vec<&Cookie> = matched
            .iter()
            .flat_map(|(_, cookies)| cookies.iter())
            .filter(|cookie| {
                cookie
                    .expires
                    .as_deref()
                    .is_none_or(|expires| !cookies::is_expired(expires))
            })
            .collect();
        if chosen.is_empty() {
            return CookieFreshness::Stale;
        }
        let header = chosen
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        CookieFreshness::Fresh(header)
    }

    /// Builds a `Cookie` request-header value for `host`, or `None` when no
    /// fresh cookies apply (missing or stale). Thin wrapper over
    /// [`Self::cookie_status`]; the lazy-exchange path uses the full status.
    pub fn cookie_header(&self, host: &str) -> Option<String> {
        match self.cookie_status(host) {
            CookieFreshness::Fresh(header) => Some(header),
            CookieFreshness::Stale | CookieFreshness::Absent => None,
        }
    }

    /// The Amazon customer id, used e.g. to scope library database files
    /// (D9).
    pub fn customer_id(&self) -> Option<&str> {
        self.identity.customer_id.as_deref()
    }

    /// The registered device type (`X-Device-Type-Id` header, voucher
    /// key derivation). Present once an account was registered/imported.
    pub fn device_type(&self) -> Option<&str> {
        self.device.device_type.as_deref()
    }

    /// The registered device serial number (voucher key derivation).
    pub fn device_serial(&self) -> Option<&str> {
        self.device.serial.as_deref()
    }

    /// The device name Amazon assigned at registration (e.g.
    /// `Alice's 4th Audible for iPhone`). Shown after `account login` so the
    /// device can be identified and removed in the Amazon account.
    pub fn device_name(&self) -> Option<&str> {
        self.device.name.as_deref()
    }
}

/// Whether the content carries the audible-rs format marker.
fn is_new_format(content: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(content)
        .ok()
        .and_then(|v| v.get("format").and_then(|f| f.as_str()).map(String::from))
        .is_some_and(|format| format == authfile::ENVELOPE_FORMAT)
}

async fn read_file(path: PathBuf) -> Result<Vec<u8>, AuthError> {
    tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .expect("blocking read task must not panic")
        .map_err(AuthError::Io)
}

/// Writes via a unique temp file next to the target plus rename, with
/// owner-only (`0o600`) permissions on Unix, under the file's
/// cross-process write lock — the CLI and the agent both write auth
/// files back after a token refresh, and a shared temp name interleaved
/// them into one torn file (audit 2026-07-17, B5). On Windows the mode
/// is a no-op — the auth envelope rests on user-profile isolation, not
/// an ACL (AUD-198).
fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut lock = crate::fsutil::write_lock(path)?;
    let _guard = lock.write()?;
    crate::fsutil::persist_atomically(path, content, Some(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_origin_from_account_pool() {
        assert_eq!(
            AccountOrigin::from_account_pool("Amazon"),
            AccountOrigin::Amazon
        );
        assert_eq!(
            AccountOrigin::from_account_pool("AmazonCN"),
            AccountOrigin::Amazon
        );
        assert_eq!(
            AccountOrigin::from_account_pool("pool-AN7V1F1VY261K"),
            AccountOrigin::AudibleLegacy
        );
    }

    #[test]
    fn account_origin_picks_auth_host() {
        let de = locale::find("de").unwrap();
        assert_eq!(AccountOrigin::Amazon.auth_url(&de), "https://api.amazon.de");
        assert_eq!(
            AccountOrigin::AudibleLegacy.auth_url(&de),
            "https://api.audible.de"
        );
    }

    #[test]
    fn clear_access_token_keeps_refresh_token() {
        let data = serde_json::json!({
            "country_code": "de",
            "bearer": {
                "access_token": "Atna|x",
                "refresh_token": "Atnr|x",
                "expires": 9_999_999_999.0_f64,
            },
        });
        let mut auth = Authenticator::from_value(data).unwrap();
        assert!(auth.access_token().is_some());
        assert!(auth.expires().is_some());

        auth.clear_access_token();
        assert!(auth.access_token().is_none());
        assert!(auth.expires().is_none());
        // The refresh token is the account's lifeline and must remain.
        assert!(auth.refresh_token().is_some());
    }

    /// A7: the token counts as needing a refresh once it is within the
    /// margin of expiry, so it is never sent about to expire.
    #[test]
    fn access_token_expired_honors_the_refresh_margin() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let with_expiry = |expires: f64| {
            Authenticator::from_value(serde_json::json!({
                "country_code": "de",
                "bearer": {"access_token": "Atna|x", "expires": expires},
            }))
            .unwrap()
        };
        // Comfortably valid, within-margin, and past all read as expected.
        assert!(!with_expiry(now + TOKEN_REFRESH_MARGIN_SECS + 30.0).access_token_expired());
        assert!(with_expiry(now + TOKEN_REFRESH_MARGIN_SECS - 5.0).access_token_expired());
        assert!(with_expiry(now - 1.0).access_token_expired());
    }

    /// A6: a background merged write-back persists only its own group and
    /// keeps a concurrent CLI edit to another field — the lost-update the
    /// plain whole-file write-back caused when the agent held a stale copy.
    #[tokio::test]
    async fn merged_bearer_write_back_preserves_a_concurrent_edit() {
        use secrecy::ExposeSecret as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("alice.auth");

        Authenticator::from_value(serde_json::json!({
            "country_code": "de",
            "bearer": {"access_token": "Atna|old", "refresh_token": "Atnr|keep", "expires": 1000.0_f64},
        }))
        .unwrap()
        .save_to(&path, None, KdfParams::default())
        .await
        .unwrap();

        // The agent's long-lived (now stale) in-memory copy.
        let mut stale = Authenticator::load_file(&path, None).await.unwrap();

        // Meanwhile the user stores activation bytes via a fresh load and a
        // whole-file save (the foreground path).
        let mut cli = Authenticator::load_file(&path, None).await.unwrap();
        cli.set_activation_bytes("DEADBEEF".into());
        cli.save().await.unwrap();

        // The agent refreshes its token and persists only that group.
        stale.apply_token_refresh(SecretString::from("Atna|new".to_owned()), 5000.0);
        stale.save_merged(MergeScope::BearerToken).await.unwrap();

        let reloaded = Authenticator::load_file(&path, None).await.unwrap();
        assert_eq!(reloaded.access_token().unwrap().expose_secret(), "Atna|new");
        assert_eq!(reloaded.expires(), Some(5000.0));
        // The concurrent edit survived (the A6 lost-update is closed), and
        // the refresh token is untouched.
        assert_eq!(reloaded.activation_bytes(), Some("DEADBEEF"));
        assert_eq!(
            reloaded.refresh_token().unwrap().expose_secret(),
            "Atnr|keep"
        );
    }

    /// A6: the cookie merge is per-domain — re-exchanging one domain must
    /// not resurrect a domain the user removed concurrently.
    #[tokio::test]
    async fn merged_cookie_write_back_is_per_domain() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("alice.auth");

        Authenticator::from_value(serde_json::json!({
            "country_code": "de",
            "website_cookies": {
                ".amazon.de": [{"name": "sid", "value": "de-old"}],
                ".amazon.co.uk": [{"name": "sid", "value": "uk"}],
            },
            "cookie_ttls": {".amazon.de": 1000.0_f64, ".amazon.co.uk": 1000.0_f64},
        }))
        .unwrap()
        .save_to(&path, None, KdfParams::default())
        .await
        .unwrap();

        let mut stale = Authenticator::load_file(&path, None).await.unwrap();

        // Concurrent CLI edit: remove the uk domain, whole-file save.
        let mut cli = Authenticator::load_file(&path, None).await.unwrap();
        assert!(cli.remove_website_cookies(".amazon.co.uk"));
        cli.save().await.unwrap();

        // The agent re-exchanges only .amazon.de and merges that domain.
        let jar: Vec<Cookie> =
            serde_json::from_value(serde_json::json!([{"name": "sid", "value": "de-new"}]))
                .unwrap();
        stale.set_website_cookies(".amazon.de".to_owned(), jar);
        stale.set_cookie_ttl(".amazon.de".to_owned(), 5000.0);
        stale
            .save_merged(MergeScope::Cookies(vec![".amazon.de".to_owned()]))
            .await
            .unwrap();

        let reloaded = Authenticator::load_file(&path, None).await.unwrap();
        let cookies = reloaded.website_cookies();
        // .amazon.de was updated …
        assert_eq!(cookies[".amazon.de"][0].value, "de-new");
        // … and the concurrent uk removal was NOT undone.
        assert!(!cookies.contains_key(".amazon.co.uk"));
    }

    #[test]
    fn cookie_header_is_amazon_audible_sso_aware() {
        let data = serde_json::json!({
            "country_code": "de",
            "website_cookies": {
                ".amazon.de": [{"name": "session-id", "value": "abc"}]
            },
            // A future TTL keeps the bucket fresh so scoping is what's tested.
            "cookie_ttls": {".amazon.de": 9_999_999_999.0_f64},
        });
        let auth = Authenticator::from_value(data).unwrap();
        // Presented to the amazon host and its sibling audible host.
        assert_eq!(
            auth.cookie_header("www.amazon.de").as_deref(),
            Some("session-id=abc")
        );
        assert_eq!(
            auth.cookie_header("www.audible.de").as_deref(),
            Some("session-id=abc")
        );
        // Never to an unrelated host or a different marketplace tld.
        assert!(auth.cookie_header("www.example.com").is_none());
        assert!(auth.cookie_header("www.audible.com").is_none());
    }

    #[test]
    fn cookie_status_is_stale_without_or_with_a_lapsed_ttl() {
        // No recorded TTL (a registration/import) → Stale → the first cookie
        // request exchanges to obtain a session with a known lifetime.
        let no_ttl = serde_json::json!({
            "country_code": "de",
            "website_cookies": {".amazon.de": [{"name": "session-id", "value": "abc"}]},
        });
        let auth = Authenticator::from_value(no_ttl).unwrap();
        assert_eq!(auth.cookie_status("www.amazon.de"), CookieFreshness::Stale);

        // A recorded TTL in the past → Stale (sibling host sees the same).
        let lapsed = serde_json::json!({
            "country_code": "de",
            "website_cookies": {".amazon.de": [{"name": "session-id", "value": "abc"}]},
            "cookie_ttls": {".amazon.de": 1.0_f64},
        });
        let auth = Authenticator::from_value(lapsed).unwrap();
        assert_eq!(auth.cookie_status("www.amazon.de"), CookieFreshness::Stale);
        assert_eq!(auth.cookie_status("www.audible.de"), CookieFreshness::Stale);
        assert!(auth.cookie_header("www.amazon.de").is_none());
    }

    #[test]
    fn cookie_status_is_fresh_with_a_future_ttl_and_absent_elsewhere() {
        let data = serde_json::json!({
            "country_code": "de",
            "website_cookies": {".amazon.de": [{"name": "session-id", "value": "abc"}]},
            "cookie_ttls": {".amazon.de": 9_999_999_999.0_f64},
        });
        let auth = Authenticator::from_value(data).unwrap();
        assert_eq!(
            auth.cookie_status("www.amazon.de"),
            CookieFreshness::Fresh("session-id=abc".to_owned())
        );
        assert_eq!(
            auth.cookie_status("www.example.com"),
            CookieFreshness::Absent
        );
    }

    #[test]
    fn fresh_ttl_still_drops_individually_expired_cookies() {
        // The bucket TTL is fresh, but one cookie's real `Expires` has lapsed:
        // it is filtered out of the header, the rest are still sent.
        let data = serde_json::json!({
            "country_code": "de",
            "website_cookies": {".amazon.de": [
                {"name": "session-id", "value": "abc"},
                {"name": "at-acbde", "value": "x", "expires": "1 May 2000 18:31:04 GMT"},
            ]},
            "cookie_ttls": {".amazon.de": 9_999_999_999.0_f64},
        });
        let auth = Authenticator::from_value(data).unwrap();
        assert_eq!(
            auth.cookie_status("www.amazon.de"),
            CookieFreshness::Fresh("session-id=abc".to_owned())
        );
    }

    #[test]
    fn legacy_import_takes_account_pool_over_with_username() {
        // account_pool from customer_info is authoritative; the
        // contradictory with_username=false is ignored.
        let data = serde_json::json!({
            "adp_token": null,
            "device_private_key": null,
            "access_token": "Atna|x",
            "refresh_token": "Atnr|x",
            "expires": 1.0,
            "locale_code": "de",
            "with_username": false,
            "website_cookies": {"session-id": "123"},
            "device_info": {"device_type": "A2CZJZGLK2JJVM"},
            "customer_info": {
                "user_id": "amzn1.account.X",
                "name": "Alice",
                "account_pool": "pool-AN7V1F1VY261K"
            },
            "activation_bytes": null
        });
        let auth = Authenticator::from_legacy_value(data).unwrap();
        assert_eq!(auth.origin(), AccountOrigin::AudibleLegacy);
        assert_eq!(auth.customer_id(), Some("amzn1.account.X"));
        assert!(auth.signer().is_none());
        // Flat cookie bucketed under the marketplace domain.
        assert!(auth.website_cookies().contains_key(".amazon.de"));
        let value = auth.to_value();
        assert_eq!(value["identity"]["account_pool"], "pool-AN7V1F1VY261K");
        // Missing cookie attributes appear as explicit null placeholders.
        let cookie = &value["website_cookies"][".amazon.de"][0];
        assert!(
            cookie
                .get("expires")
                .is_some_and(serde_json::Value::is_null)
        );
        assert!(
            cookie
                .get("http_only")
                .is_some_and(serde_json::Value::is_null)
        );
    }

    #[test]
    fn legacy_import_falls_back_to_with_username_without_pool() {
        // Ancient file lacking account_pool: with_username is the fallback.
        let data = serde_json::json!({
            "access_token": "Atna|x",
            "locale_code": "de",
            "with_username": true,
            "customer_info": {"user_id": "amzn1.account.X"}
        });
        let auth = Authenticator::from_legacy_value(data).unwrap();
        assert_eq!(auth.origin(), AccountOrigin::AudibleLegacy);
        assert_eq!(
            auth.to_value()["identity"]["account_pool"],
            "pool-AN7V1F1VY261K"
        );
    }

    #[test]
    fn regular_amazon_account_origin() {
        let data = serde_json::json!({
            "country_code": "de",
            "identity": {"customer_id": "amzn1.account.X", "account_pool": "Amazon"},
        });
        let auth = Authenticator::from_value(data).unwrap();
        assert_eq!(auth.origin(), AccountOrigin::Amazon);
    }

    #[test]
    fn extra_identity_fields_round_trip() {
        let data = serde_json::json!({
            "country_code": "de",
            "identity": {
                "customer_id": "amzn1.account.X",
                "account_pool": "Amazon",
                "given_name": "Alice",
                "future_field": 42
            },
        });
        let auth = Authenticator::from_value(data).unwrap();
        let value = auth.to_value();
        // Unmodelled fields survive losslessly via the flatten catch-all.
        assert_eq!(value["identity"]["given_name"], "Alice");
        assert_eq!(value["identity"]["future_field"], 42);
    }

    #[test]
    fn export_legacy_value_is_the_inverse_of_the_import() {
        // Grouped data with everything the Python layout carries, plus a
        // second cookie domain that the flat format cannot represent.
        let data = serde_json::json!({
            "country_code": "de",
            "identity": {
                "customer_id": "amzn1.account.X",
                "account_pool": "Amazon",
                "name": "Alice",
                "home_region": "EU",
                "given_name": "Alice"
            },
            "device": {
                "device_type": "A2CZJZGLK2JJVM",
                "serial": "SERIAL123",
                "name": "Alice's Audible für iPhone",
                "device_serial_number_extra": true
            },
            "signing": { "adp_token": "{enc:token}" },
            "bearer": { "access_token": "Atna|abc", "refresh_token": "Atnr|def", "expires": 123.5 },
            "website_cookies": {
                ".amazon.de": [ { "name": "session-id", "value": "sid" } ],
                ".amazon.com": [ { "name": "other", "value": "x" } ]
            },
            "activation_bytes": "deadbeef"
        });
        let auth = Authenticator::from_value(data).unwrap();
        let (legacy, dropped) = auth.export_legacy_value();

        // Flat Python keys, with_username from the account pool, flat cookies
        // of the account marketplace's bucket only.
        assert_eq!(legacy["locale_code"], "de");
        assert_eq!(legacy["with_username"], false);
        assert_eq!(legacy["adp_token"], "{enc:token}");
        assert_eq!(legacy["access_token"], "Atna|abc");
        assert_eq!(legacy["expires"], 123.5);
        assert_eq!(legacy["website_cookies"]["session-id"], "sid");
        assert!(legacy["website_cookies"].get("other").is_none());
        assert_eq!(dropped, vec![".amazon.com".to_owned()]);
        assert_eq!(legacy["customer_info"]["user_id"], "amzn1.account.X");
        assert_eq!(legacy["customer_info"]["given_name"], "Alice");
        assert_eq!(legacy["device_info"]["device_serial_number"], "SERIAL123");
        assert_eq!(
            legacy["store_authentication_cookie"],
            serde_json::Value::Null
        );

        // The Python import path reads the export back losslessly (modulo
        // the documented cookie flattening).
        let back = Authenticator::from_legacy_value(legacy).unwrap();
        assert_eq!(back.locale().country_code, "de");
        assert_eq!(back.customer_id(), Some("amzn1.account.X"));
        assert_eq!(back.device_serial(), Some("SERIAL123"));
        assert_eq!(back.expires(), Some(123.5));
        assert_eq!(back.origin(), AccountOrigin::Amazon);
        let jar = back.website_cookies().get(".amazon.de").unwrap();
        assert_eq!(jar.len(), 1);
        assert_eq!(jar[0].name, "session-id");

        // Unknown identity/device fields survive the *legacy import* too, not
        // just the export — parity with a fresh registration (AUD-80). A
        // re-export must still carry them.
        let (again, _) = back.export_legacy_value();
        assert_eq!(again["customer_info"]["given_name"], "Alice");
        assert_eq!(again["device_info"]["device_serial_number_extra"], true);
    }

    #[test]
    fn export_legacy_value_marks_pre_merger_accounts() {
        let data = serde_json::json!({
            "country_code": "de",
            "identity": { "account_pool": "pool-AN7V1F1VY261K" },
        });
        let auth = Authenticator::from_value(data).unwrap();
        let (legacy, _) = auth.export_legacy_value();
        assert_eq!(legacy["with_username"], true);
    }

    #[test]
    fn export_value_round_trips_through_from_value() {
        let data = serde_json::json!({
            "country_code": "us",
            "signing": { "adp_token": "T" },
            "bearer": { "access_token": "A", "refresh_token": "R", "expires": 1.0 },
        });
        let auth = Authenticator::from_value(data).unwrap();
        let back = Authenticator::from_value(auth.export_value()).unwrap();
        assert_eq!(back.locale().country_code, "us");
        assert_eq!(back.expires(), Some(1.0));
        assert!(back.refresh_token().is_some());
    }

    #[test]
    fn unknown_locale_is_rejected() {
        let data = serde_json::json!({ "country_code": "zz" });
        assert!(matches!(
            Authenticator::from_value(data),
            Err(AuthError::UnknownLocale(_))
        ));
    }

    #[test]
    fn debug_hides_credentials() {
        let data = serde_json::json!({
            "country_code": "de",
            "bearer": { "access_token": "Atna|super-secret" },
        });
        let auth = Authenticator::from_value(data).unwrap();
        assert!(!format!("{auth:?}").contains("super-secret"));
    }
}
