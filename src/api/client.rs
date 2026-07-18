//! API client with per-request auth selection (archived architecture §7):
//! `AuthMode::{Auto, Signing, Token, Cookies}`. Callers pass paths only;
//! the host URL is built from the marketplace locale.
//!
//! One [`Client`] serves one account (one `reqwest::Client`, pooling and
//! HTTP/2 multiplexing across all profiles and tasks, D11). The access
//! token is refreshed proactively at its expiry timestamp, serialized
//! per account through a `tokio::sync::Mutex`, and written back to the
//! auth file.

use std::str::FromStr;
use std::sync::Arc;

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Mutex;

use crate::auth::signing::{HEADER_ADP_ALG, HEADER_ADP_SIGNATURE, HEADER_ADP_TOKEN, RequestSigner};
use crate::auth::{AccountOrigin, AuthError, Authenticator, CookieFreshness, cookies};

use super::locale::{self, Locale};

/// User-Agent sent on every request. The content-delivery host
/// (cds.audible.de) 403s the default reqwest UA via its WAF; an
/// Audible-style UA (as audible-cli uses) passes.
const USER_AGENT: &str = "Audible/671 CFNetwork/1240.0.4 Darwin/20.6.0";

/// Connection timeout for every HTTP client in the crate: a hanging
/// server/network fails fast instead of stalling a command until Ctrl-C.
/// Generous enough for slow mobile/tethered links.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Idle read timeout for the API client: errors when a response stalls —
/// no bytes at all — for this long. Applied per read, so a slow but
/// progressing multi-GB download never triggers it (deliberately no
/// total request timeout, which would kill long streams).
pub(crate) const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How a single request authenticates (archived architecture §7).
///
/// Selected per request; without an explicit choice [`AuthMode::Auto`]
/// applies: signing when the account has signing material, the access
/// token otherwise (matching the Python reference's fallback).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// Signing if adp_token and device key are present, else the access token.
    #[default]
    Auto,
    /// RSA-SHA256 request signing (`x-adp-*` headers).
    Signing,
    /// Access token via `x-amz-access-token`, refreshed proactively. (The
    /// header is `x-amz-access-token`, not an RFC-6750 `Authorization:
    /// Bearer` — the live API rejects the Bearer + client-id pair.)
    Token,
    /// Stored website cookies, host-scoped. Lazily re-exchanged before a
    /// request when they are missing or their (`tokens.ttl`-bounded) session
    /// has lapsed.
    Cookies,
}

impl AuthMode {
    /// The mode labels in canonical order — one home (audit 2026-07-17,
    /// D6): the interactive `api` picker and its round-trip test each had
    /// their own copy of this list.
    pub const LABELS: [&'static str; 4] = ["auto", "signing", "token", "cookies"];
}

impl FromStr for AuthMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(AuthMode::Auto),
            "signing" => Ok(AuthMode::Signing),
            "token" => Ok(AuthMode::Token),
            "cookies" => Ok(AuthMode::Cookies),
            other => Err(format!(
                "unknown auth mode {other:?} (expected auto|signing|token|cookies)"
            )),
        }
    }
}

/// Errors raised by the API client.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Auth material handling failed (loading, saving, signing).
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The HTTP layer failed.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The request path is not a plain absolute path.
    #[error("invalid request path {0:?}: expected an absolute path like /1.0/library")]
    InvalidPath(String),
    /// The requested auth mode cannot be served with this account's
    /// material.
    #[error("auth mode {0:?} is not available for this account")]
    AuthModeUnavailable(AuthMode),
    /// The account has no refresh token to renew the access token with.
    #[error("no refresh token available to refresh the access token")]
    NoRefreshToken,
    /// The token refresh endpoint answered with an error status.
    #[error("token refresh failed with HTTP status {0}")]
    TokenRefresh(StatusCode),
    /// The device-deregistration endpoint answered with an error status.
    #[error("device deregistration failed with HTTP status {0}")]
    Deregister(StatusCode),
    /// The website-cookie exchange endpoint answered with an error status.
    #[error("cookie exchange failed with HTTP status {0}")]
    CookieExchange(StatusCode),
    /// The token refresh response did not contain the expected fields.
    #[error("token refresh returned an unexpected response")]
    TokenRefreshResponse,
    /// A country code did not match any known marketplace.
    #[error("unknown marketplace country code {0:?}")]
    UnknownLocale(String),
    /// A custom request header name or value was not valid.
    #[error("invalid request header {0:?}")]
    InvalidHeader(String),
    /// A content license response had an unexpected shape (HTTP 2xx but
    /// no parseable `content_license`).
    #[error("license request for {0:?} returned an unexpected response")]
    LicenseResponse(String),
    /// The license endpoint answered with an error status. Surfaces the
    /// server's `error_code` and `message` verbatim for diagnosis (these
    /// error bodies carry no credentials). The cause is deliberately left
    /// to the reader — a license failure can mean many things.
    #[error(
        "license request for {asin:?} failed (HTTP {status}, error_code {error_code:?}, request id {request_id}): {message}"
    )]
    LicenseRejected {
        /// The requested item.
        asin: String,
        /// HTTP status returned by the endpoint.
        status: StatusCode,
        /// Server-supplied `error_code` (empty when the body had none).
        error_code: String,
        /// Server-supplied human-readable message (or a truncated body).
        message: String,
        /// The `x-amzn-requestid` echoed by the response (`-` when absent)
        /// — quote it when reporting the failure to Amazon (AUD-29).
        request_id: String,
    },
    /// An annotation response was 2xx but its non-empty body was not
    /// parseable JSON (a proxy/HTML error page, a transient fault). A
    /// failure — never "no annotations", which would be recorded and
    /// skip the item's real bookmarks forever.
    #[error("annotation response for {0:?} was 2xx but not parseable JSON")]
    AnnotationResponse(String),
    /// A metadata request returned no chapter information.
    #[error("metadata request for {0:?} returned no chapter_info")]
    ChapterInfo(String),
}

/// Read-only access-token state for `account token status`. Carries no
/// secret values — only presence flags and the remaining validity.
#[derive(Debug, Clone, Copy)]
pub struct TokenStatus {
    /// Whether an access token is currently stored.
    pub has_access_token: bool,
    /// Seconds until the access token expires (negative = already
    /// expired); `None` when no access token (or no expiry) is stored.
    pub remaining_secs: Option<i64>,
    /// Whether a refresh token is stored. It is never removed via the CLI.
    pub has_refresh_token: bool,
}

/// API client for one account.
pub struct Client {
    http: reqwest::Client,
    auth: Arc<Mutex<Authenticator>>,
    signer: Option<Arc<RequestSigner>>,
    origin: AccountOrigin,
    account_locale: Locale,
    default_locale: Locale,
    customer_id: Option<String>,
    device_type: Option<String>,
    device_serial: Option<String>,
    api_base_override: Option<Url>,
    auth_base_override: Option<Url>,
}

/// Builder for [`Client`].
pub struct ClientBuilder {
    auth: Authenticator,
    country_code: Option<String>,
    api_base_override: Option<Url>,
    auth_base_override: Option<Url>,
}

impl ClientBuilder {
    /// Default marketplace for requests (the account's registration
    /// marketplace if unset). Each request can still override it.
    pub fn country_code(mut self, country_code: impl Into<String>) -> Self {
        self.country_code = Some(country_code.into());
        self
    }

    /// Overrides the Audible API base URL — for tests against a mock
    /// server only.
    pub fn api_base_override(mut self, url: Url) -> Self {
        self.api_base_override = Some(url);
        self
    }

    /// Overrides the auth API base URL (token refresh) — for tests
    /// against a mock server only.
    pub fn auth_base_override(mut self, url: Url) -> Self {
        self.auth_base_override = Some(url);
        self
    }

    /// Builds the client.
    pub fn build(self) -> Result<Client, ApiError> {
        let account_locale = *self.auth.locale();
        let default_locale = match &self.country_code {
            Some(code) => {
                locale::find(code).ok_or_else(|| ApiError::UnknownLocale(code.clone()))?
            }
            None => account_locale,
        };
        let signer = self.auth.signer().cloned();
        let origin = self.auth.origin();
        let customer_id = self.auth.customer_id().map(str::to_owned);
        let device_type = self.auth.device_type().map(str::to_owned);
        let device_serial = self.auth.device_serial().map(str::to_owned);

        Ok(Client {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()?,
            auth: Arc::new(Mutex::new(self.auth)),
            signer,
            origin,
            account_locale,
            default_locale,
            customer_id,
            device_type,
            device_serial,
            api_base_override: self.api_base_override,
            auth_base_override: self.auth_base_override,
        })
    }
}

impl Client {
    /// Starts building a client for the given account.
    pub fn builder(auth: Authenticator) -> ClientBuilder {
        ClientBuilder {
            auth,
            country_code: None,
            api_base_override: None,
            auth_base_override: None,
        }
    }

    /// Creates a client with default settings.
    pub fn new(auth: Authenticator) -> Result<Self, ApiError> {
        Self::builder(auth).build()
    }

    /// The account's Amazon customer id, if the auth data carries one.
    pub fn customer_id(&self) -> Option<&str> {
        self.customer_id.as_deref()
    }

    /// The registered device type, if the auth data carries one.
    pub fn device_type(&self) -> Option<&str> {
        self.device_type.as_deref()
    }

    /// The registered device serial number, if the auth data carries one.
    pub fn device_serial(&self) -> Option<&str> {
        self.device_serial.as_deref()
    }

    /// Starts a request for an API path (including an optional query
    /// string), e.g. `/1.0/library?num_results=50`.
    pub fn request(&self, method: Method, path: impl Into<String>) -> RequestBuilder<'_> {
        RequestBuilder {
            client: self,
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            auth: AuthMode::Auto,
            country_code: None,
            extra_headers: Vec::new(),
            absolute_url: None,
        }
    }

    /// Starts a request to a verbatim absolute URL, bypassing the API host
    /// builder and the foreign-host guard. This is the `api --foreign-host`
    /// escape hatch (e.g. to test website cookies against `www.amazon.<tld>`);
    /// the caller is responsible for the URL.
    pub fn request_absolute(&self, method: Method, url: Url) -> RequestBuilder<'_> {
        RequestBuilder {
            client: self,
            method,
            path: String::new(),
            query: Vec::new(),
            body: None,
            auth: AuthMode::Auto,
            country_code: None,
            extra_headers: Vec::new(),
            absolute_url: Some(url),
        }
    }

    /// Builds the auth headers for a request, resolving `Auto` to
    /// signing when the account has signing material, the access token
    /// otherwise. Shared by API requests and authenticated downloads.
    async fn auth_headers(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        requested: AuthMode,
        host: &str,
    ) -> Result<HeaderMap, ApiError> {
        let mode = match requested {
            AuthMode::Auto if self.signer.is_some() => AuthMode::Signing,
            AuthMode::Auto => AuthMode::Token,
            explicit => explicit,
        };

        let mut headers = HeaderMap::new();
        match mode {
            AuthMode::Auto => unreachable!("auto resolved above"),
            AuthMode::Signing => {
                let signer = self
                    .signer
                    .clone()
                    .ok_or(ApiError::AuthModeUnavailable(AuthMode::Signing))?;
                let method = method.to_owned();
                let path_and_query = path_and_query.to_owned();
                let body = body.to_vec();
                // RSA signing is CPU-bound; keep it off the executor.
                let signed = tokio::task::spawn_blocking(move || {
                    signer.sign_request(&method, &path_and_query, &body)
                })
                .await
                .expect("signing task must not panic");

                let mut token = HeaderValue::from_str(&signed.adp_token)
                    .map_err(|_| ApiError::AuthModeUnavailable(AuthMode::Signing))?;
                token.set_sensitive(true);
                headers.insert(HeaderName::from_static(HEADER_ADP_TOKEN), token);
                headers.insert(
                    HeaderName::from_static(HEADER_ADP_ALG),
                    HeaderValue::from_static(signed.alg),
                );
                let mut signature = HeaderValue::from_str(&signed.signature)
                    .map_err(|_| ApiError::AuthModeUnavailable(AuthMode::Signing))?;
                signature.set_sensitive(true);
                headers.insert(HeaderName::from_static(HEADER_ADP_SIGNATURE), signature);
            }
            AuthMode::Token => {
                // The Python reference sends `Authorization: Bearer` plus
                // `client-id: 0`, but the live API rejects that pair with
                // 400 "token does not correspond to the specified
                // Client-ID"; `x-amz-access-token` works without one.
                let token = self.ensure_fresh_access_token().await?;
                let mut value = HeaderValue::from_str(token.expose_secret())
                    .map_err(|_| ApiError::AuthModeUnavailable(AuthMode::Token))?;
                value.set_sensitive(true);
                headers.insert(HeaderName::from_static("x-amz-access-token"), value);
            }
            AuthMode::Cookies => {
                let cookie_line = self.ensure_fresh_cookies(host).await?;
                let mut value = HeaderValue::from_str(&cookie_line)
                    .map_err(|_| ApiError::AuthModeUnavailable(AuthMode::Cookies))?;
                value.set_sensitive(true);
                headers.insert(reqwest::header::COOKIE, value);
            }
        }
        Ok(headers)
    }

    /// Builds an authenticated GET for an arbitrary URL (a different host
    /// than the API — content delivery URLs from a license). Uses `Auto`
    /// auth; the caller adds a `Range` header and streams the response.
    /// This is internal download plumbing, not exposed to plugins.
    pub async fn authed_get(&self, url: &str) -> Result<reqwest::RequestBuilder, ApiError> {
        let parsed = Url::parse(url).map_err(|_| ApiError::InvalidPath(url.to_owned()))?;
        let path_and_query = match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_owned(),
        };
        let headers = self
            .auth_headers(
                "GET",
                &path_and_query,
                &[],
                AuthMode::Auto,
                parsed.host_str().unwrap_or_default(),
            )
            .await?;
        Ok(self.http.get(url).headers(headers))
    }

    /// Refreshes the access token if it is at or past its expiry and
    /// returns a valid token. Serialized per account: concurrent callers
    /// queue on the mutex and find the refreshed token.
    async fn ensure_fresh_access_token(&self) -> Result<SecretString, ApiError> {
        let mut auth = self.auth.lock().await;

        if !auth.access_token_expired() {
            tracing::debug!("access token still valid; no refresh needed");
            return auth
                .access_token()
                .cloned()
                .ok_or(ApiError::AuthModeUnavailable(AuthMode::Token));
        }

        tracing::debug!("access token expired; refreshing");
        self.perform_token_refresh(&mut auth).await
    }

    /// Forces an access-token refresh via the refresh token regardless of
    /// the current token's expiry, and writes the auth file back. Used by
    /// `account token refresh`.
    pub async fn force_refresh_access_token(&self) -> Result<SecretString, ApiError> {
        let mut auth = self.auth.lock().await;
        tracing::debug!("forcing access token refresh");
        self.perform_token_refresh(&mut auth).await
    }

    /// The actual refresh: exchanges the refresh token for a fresh access
    /// token, stores it and writes back. The caller holds the auth lock.
    async fn perform_token_refresh(
        &self,
        auth: &mut Authenticator,
    ) -> Result<SecretString, ApiError> {
        let refresh_token = auth
            .refresh_token()
            .cloned()
            .ok_or(ApiError::NoRefreshToken)?;

        let base = match &self.auth_base_override {
            Some(url) => url.as_str().trim_end_matches('/').to_owned(),
            None => self.origin.auth_url(&self.account_locale),
        };

        // Body and endpoint mirror audible.auth.refresh_access_token.
        let response = self
            .http
            .post(format!("{base}/auth/token"))
            .form(&[
                ("app_name", "Audible"),
                ("app_version", "3.56.2"),
                ("source_token", refresh_token.expose_secret()),
                ("requested_token_type", "access_token"),
                ("source_token_type", "refresh_token"),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(ApiError::TokenRefresh(response.status()));
        }

        let payload: serde_json::Value = response.json().await?;
        let access_token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or(ApiError::TokenRefreshResponse)?
            .to_owned();
        // expires_in arrives as a number or numeric string.
        let expires_in = match payload.get("expires_in") {
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            Some(serde_json::Value::String(s)) => s.parse().ok(),
            _ => None,
        }
        .ok_or(ApiError::TokenRefreshResponse)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs_f64();
        let token = SecretString::from(access_token);
        auth.apply_token_refresh(token.clone(), now + expires_in);
        tracing::info!("access token refreshed");

        // Merge only the refreshed token onto the on-disk state (A6): the
        // agent holds this `auth` across a session's lifetime, so a plain
        // whole-file save would roll back a concurrent CLI edit (a removed
        // cookie, freshly stored activation bytes). A failed write-back is
        // not fatal — the token lives in memory and the next run refreshes.
        if let Err(error) = auth.save_merged(crate::auth::MergeScope::BearerToken).await {
            tracing::warn!(%error, "could not write refreshed token back to the auth file");
        }

        Ok(token)
    }

    /// Deregisters this account's device with Amazon (`POST /auth/deregister`),
    /// so it disappears from "Manage Your Content and Devices". With
    /// `deregister_all`, other registrations that share this device's serial
    /// number are removed too (same account and serial — not every device of
    /// the account).
    ///
    /// Authenticated with `x-amz-access-token` (refreshed first if needed) —
    /// the account's token AuthMode, not an RFC-6750 `Authorization: Bearer`.
    /// On success the tokens are invalidated server-side, so the caller should
    /// discard the auth material afterwards. No secrets are logged.
    ///
    /// Returns the locally stored device name (the one Amazon assigned at
    /// registration), so the caller can report which device was removed.
    pub async fn deregister(&self, deregister_all: bool) -> Result<Option<String>, ApiError> {
        // Read the device name before the request (the auth file is deleted
        // afterwards); a short-lived lock, released before the token refresh.
        let device_name = {
            let auth = self.auth.lock().await;
            auth.device_name().map(str::to_owned)
        };
        let token = self.ensure_fresh_access_token().await?;
        let base = match &self.auth_base_override {
            Some(url) => url.as_str().trim_end_matches('/').to_owned(),
            None => self.origin.auth_url(&self.account_locale),
        };
        let url = format!("{base}/auth/deregister");
        let auth_domain = Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned))
            .unwrap_or_default();

        let mut access = HeaderValue::from_str(token.expose_secret())
            .map_err(|_| ApiError::AuthModeUnavailable(AuthMode::Token))?;
        access.set_sensitive(true);

        let response = self
            .http
            .post(&url)
            .header(HeaderName::from_static("x-amz-access-token"), access)
            .header("x-amzn-identity-auth-domain", auth_domain)
            .json(&serde_json::json!({
                "deregister_all_existing_accounts": deregister_all,
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ApiError::Deregister(response.status()));
        }
        Ok(device_name)
    }

    /// Read-only snapshot of the access-token state — no network, no
    /// secret values. Used by `account token status`.
    pub async fn token_status(&self) -> TokenStatus {
        let auth = self.auth.lock().await;
        let remaining_secs = match (auth.access_token().is_some(), auth.expires()) {
            (true, Some(expires)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before unix epoch")
                    .as_secs_f64();
                Some((expires - now) as i64)
            }
            _ => None,
        };
        TokenStatus {
            has_access_token: auth.access_token().is_some(),
            remaining_secs,
            has_refresh_token: auth.refresh_token().is_some(),
        }
    }

    /// Removes the stored access token (and its expiry), forcing a refresh
    /// on the next request, and writes the auth file back. The refresh
    /// token is deliberately kept: it mints new access tokens, drives the
    /// website-cookie exchange, and a device deregistration needs an access
    /// token (so the refresh token is required to obtain one). Used by
    /// `account token remove`.
    pub async fn clear_access_token(&self) -> Result<(), ApiError> {
        let mut auth = self.auth.lock().await;
        auth.clear_access_token();
        auth.save().await?;
        Ok(())
    }

    /// Refreshes website cookies for `country_code`'s marketplace by exchanging
    /// the refresh token, and stores them (multi-domain — other domains are
    /// kept), writing the auth file back. Returns the domains obtained and the
    /// raw response. Nothing secret is logged.
    pub async fn refresh_cookies(
        &self,
        country_code: &str,
    ) -> Result<(Vec<String>, serde_json::Value), ApiError> {
        let mut auth = self.auth.lock().await;
        self.perform_cookie_exchange(&mut auth, country_code).await
    }

    /// The actual cookie exchange: swaps the refresh token for fresh website
    /// cookies for `country_code`'s marketplace, records the response's
    /// `tokens.ttl` as a per-domain expiry (the real session lifetime), stores
    /// them (multi-domain, non-destructive) and writes the auth file back. The
    /// caller holds the auth lock. Nothing secret is logged.
    async fn perform_cookie_exchange(
        &self,
        auth: &mut Authenticator,
        country_code: &str,
    ) -> Result<(Vec<String>, serde_json::Value), ApiError> {
        let target_locale = locale::find(country_code)
            .ok_or_else(|| ApiError::UnknownLocale(country_code.to_owned()))?;
        let target = cookies::exchange_target(self.origin);
        let url = format!(
            "https://www.{target}.{}/ap/exchangetoken/cookies",
            target_locale.domain
        );

        let refresh_token = auth
            .refresh_token()
            .cloned()
            .ok_or(ApiError::NoRefreshToken)?;
        let body =
            cookies::exchange_body(refresh_token.expose_secret(), target, target_locale.domain);

        let response = self.http.post(url.as_str()).form(&body).send().await?;
        if !response.status().is_success() {
            return Err(ApiError::CookieExchange(response.status()));
        }
        let payload: serde_json::Value = response.json().await?;
        let by_domain = cookies::parse_exchange_response(&payload);
        // Record the exchange TTL (`response.tokens.ttl`, seconds) as an
        // absolute per-domain expiry; this drives the lazy re-exchange. Older
        // responses omit it — fall back to a finite default so freshly
        // exchanged cookies aren't re-exchanged on every request. The real
        // per-cookie `Expires` is kept as-is.
        let ttl_secs = cookies::parse_ttl(&payload).unwrap_or(cookies::DEFAULT_TTL_SECS);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs_f64();
        let ttl_expiry = now + ttl_secs as f64;
        let domains: Vec<String> = by_domain.keys().cloned().collect();
        for (domain, jar) in by_domain {
            auth.set_cookie_ttl(domain.clone(), ttl_expiry);
            auth.set_website_cookies(domain, jar);
        }
        // Merge only the exchanged domains onto the on-disk state (A6): a
        // whole-file save from this possibly-stale agent copy would roll
        // back a concurrent CLI edit to another field or another domain.
        // A failed write-back is not fatal — the cookies live in memory
        // and a later exchange fetches them again.
        if let Err(error) = auth
            .save_merged(crate::auth::MergeScope::Cookies(domains.clone()))
            .await
        {
            tracing::warn!(%error, "could not write exchanged cookies back to the auth file");
        }
        tracing::info!(?domains, "exchanged website cookies");
        Ok((domains, payload))
    }

    /// Returns a ready-to-send `Cookie` header for `host`, lazily exchanging
    /// fresh website cookies first when the stored ones are missing or their
    /// (TTL-bounded) session has lapsed. Mirrors
    /// [`Self::ensure_fresh_access_token`]: serialized per account, with a
    /// re-check under the lock so only one exchange happens under concurrency.
    /// Errors with `AuthModeUnavailable(Cookies)` when no cookies apply and the
    /// marketplace can't be derived from the host (e.g. an arbitrary
    /// `--foreign-host`).
    async fn ensure_fresh_cookies(&self, host: &str) -> Result<String, ApiError> {
        {
            let auth = self.auth.lock().await;
            if let CookieFreshness::Fresh(header) = auth.cookie_status(host) {
                return Ok(header);
            }
        }
        // Missing or stale: map the request host to a marketplace and exchange.
        let country_code = cookies::marketplace_domain_from_host(host)
            .and_then(locale::find_by_domain)
            .map(|locale| locale.country_code)
            .ok_or(ApiError::AuthModeUnavailable(AuthMode::Cookies))?;

        let mut auth = self.auth.lock().await;
        // Re-check under the lock: a concurrent caller may have refreshed.
        if let CookieFreshness::Fresh(header) = auth.cookie_status(host) {
            return Ok(header);
        }
        tracing::debug!(host, "website cookies missing or stale; exchanging");
        self.perform_cookie_exchange(&mut auth, country_code)
            .await?;
        match auth.cookie_status(host) {
            CookieFreshness::Fresh(header) => Ok(header),
            CookieFreshness::Stale | CookieFreshness::Absent => {
                Err(ApiError::AuthModeUnavailable(AuthMode::Cookies))
            }
        }
    }

    /// Summary of stored cookie domains for `account cookies show`:
    /// `(domain, cookie_count, expired_count, unknown_count, ttl_expiry)`. A
    /// cookie is *expired* only with an `Expires` in the past, and *unknown*
    /// when it carries no `Expires` at all (e.g. imported without an exchange)
    /// — those are not claimed valid. `ttl_expiry` is the domain's exchange
    /// session expiry (Unix epoch seconds), `None` when never exchanged.
    pub async fn cookie_summary(&self) -> Vec<(String, usize, usize, usize, Option<f64>)> {
        let auth = self.auth.lock().await;
        auth.website_cookies()
            .iter()
            .map(|(domain, jar)| {
                let expired = jar
                    .iter()
                    .filter(|cookie| cookie.expires.as_deref().is_some_and(cookies::is_expired))
                    .count();
                let unknown = jar.iter().filter(|cookie| cookie.expires.is_none()).count();
                (
                    domain.clone(),
                    jar.len(),
                    expired,
                    unknown,
                    auth.cookie_ttl(domain),
                )
            })
            .collect()
    }

    /// Removes stored website cookies — one domain, or all when `domain` is
    /// `None` — and writes the auth file back. Returns the removed domains.
    pub async fn remove_cookies(&self, domain: Option<&str>) -> Result<Vec<String>, ApiError> {
        let mut auth = self.auth.lock().await;
        let removed: Vec<String> = match domain {
            Some(domain) => {
                if auth.remove_website_cookies(domain) {
                    vec![domain.to_owned()]
                } else {
                    Vec::new()
                }
            }
            None => {
                let all: Vec<String> = auth.website_cookies().keys().cloned().collect();
                auth.clear_website_cookies();
                all
            }
        };
        if !removed.is_empty() {
            auth.save().await?;
        }
        Ok(removed)
    }
}

/// A request body together with its content type.
enum BodyContent {
    /// A JSON value, serialized and sent as `application/json`.
    Json(serde_json::Value),
    /// Raw bytes with a caller-supplied content type (e.g. XML). The exact
    /// bytes are signed and sent unchanged.
    Raw {
        bytes: Vec<u8>,
        content_type: HeaderValue,
    },
}

/// A single API request under construction.
pub struct RequestBuilder<'c> {
    client: &'c Client,
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    body: Option<BodyContent>,
    auth: AuthMode,
    country_code: Option<String>,
    extra_headers: Vec<(String, String)>,
    /// Set by `request_absolute` (the `api --foreign-host` escape hatch):
    /// send to this verbatim URL instead of building one from the API host.
    absolute_url: Option<Url>,
}

impl RequestBuilder<'_> {
    /// Selects the auth mode for this request (default: [`AuthMode::Auto`]).
    pub fn auth(mut self, mode: AuthMode) -> Self {
        self.auth = mode;
        self
    }

    /// Appends a query parameter.
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Sets a JSON body (sent as `application/json`).
    pub fn body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(BodyContent::Json(body));
        self
    }

    /// Sets a raw body with an explicit content type (e.g. `application/xml`).
    /// The exact bytes are signed and sent unchanged.
    pub fn raw_body(mut self, bytes: Vec<u8>, content_type: HeaderValue) -> Self {
        self.body = Some(BodyContent::Raw {
            bytes,
            content_type,
        });
        self
    }

    /// Adds a non-auth request header (e.g. the `X-ADP-*` app headers a
    /// license request expects). Auth headers are added automatically.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Sends the request to a different marketplace than the client's
    /// default.
    pub fn country_code(mut self, country_code: impl Into<String>) -> Self {
        self.country_code = Some(country_code.into());
        self
    }

    /// Applies auth and sends the request.
    pub async fn send(mut self) -> Result<reqwest::Response, ApiError> {
        let locale = match &self.country_code {
            Some(code) => {
                locale::find(code).ok_or_else(|| ApiError::UnknownLocale(code.clone()))?
            }
            None => self.client.default_locale,
        };
        let mut url = match self.absolute_url.take() {
            // The `api --foreign-host` escape hatch sends to a verbatim URL.
            Some(url) => url,
            None => {
                // Plain absolute paths only: plugins and callers can never
                // point the account's identity at a foreign host (§9).
                if !self.path.starts_with('/')
                    || self.path.starts_with("//")
                    || self.path.contains("://")
                {
                    return Err(ApiError::InvalidPath(self.path));
                }
                let base = match &self.client.api_base_override {
                    Some(url) => url.clone(),
                    None => Url::parse(&locale.audible_api_url()).expect("locale URLs are valid"),
                };
                base.join(&self.path)
                    .map_err(|_| ApiError::InvalidPath(self.path.clone()))?
            }
        };
        for (key, value) in &self.query {
            url.query_pairs_mut().append_pair(key, value);
        }

        // Serialize the body once: the signed bytes must be the sent bytes.
        let body: Option<(Vec<u8>, HeaderValue)> = match &self.body {
            Some(BodyContent::Json(json)) => Some((
                serde_json::to_vec(json).expect("JSON values always serialize"),
                HeaderValue::from_static("application/json"),
            )),
            Some(BodyContent::Raw {
                bytes,
                content_type,
            }) => Some((bytes.clone(), content_type.clone())),
            None => None,
        };
        let body_bytes: &[u8] = body.as_ref().map_or(&[], |(bytes, _)| bytes);

        let path_and_query = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };

        // App parity (AUD-29): a fresh per-request id the response echoes,
        // linking a log line or error to Amazon's server logs. Client-side
        // randomness, not a credential — safe to log.
        let request_id = new_request_id();

        // Never log query/header values: a query may carry tokens for
        // some endpoints and headers carry credentials.
        tracing::debug!(
            method = %self.method,
            path = url.path(),
            marketplace = locale.country_code,
            requested = ?self.auth,
            amzn_request_id = %request_id,
            "sending API request"
        );

        let mut headers = self
            .client
            .auth_headers(
                self.method.as_str(),
                &path_and_query,
                body_bytes,
                self.auth,
                url.host_str().unwrap_or_default(),
            )
            .await?;

        headers.insert(
            HeaderName::from_static("x-amzn-requestid"),
            HeaderValue::from_str(&request_id).expect("hex is a valid header value"),
        );

        for (name, value) in &self.extra_headers {
            match (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => return Err(ApiError::InvalidHeader(name.clone())),
            }
        }

        let method = self.method.clone();
        let path = url.path().to_owned();
        let mut request = self.client.http.request(self.method, url).headers(headers);
        if let Some((bytes, content_type)) = body {
            request = request.header(CONTENT_TYPE, content_type).body(bytes);
        }

        let started = std::time::Instant::now();
        let response = request.send().await?;
        tracing::info!(
            %method,
            path,
            status = %response.status(),
            version = ?response.version(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            amzn_request_id = %echoed_request_id(&response).unwrap_or(request_id),
            "API request finished"
        );
        Ok(response)
    }
}

/// A fresh `X-Amzn-RequestId` value in the official app's format: 20 random
/// bytes as 40 uppercase hex chars (verified from captured iOS traffic — not
/// a hyphenated UUID). The server echoes it on the response.
fn new_request_id() -> String {
    hex::encode_upper(rand::random::<[u8; 20]>())
}

/// The `x-amzn-requestid` a response carries (normally the echo of the id we
/// sent), if any — worth quoting in error messages so a failure can be
/// reported to Amazon with a concrete id.
pub(crate) fn echoed_request_id(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_matches_the_app_format() {
        let first = new_request_id();
        let second = new_request_id();
        // 20 random bytes as 40 uppercase hex chars, fresh per call.
        assert_eq!(first.len(), 40);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c))
        );
        assert_ne!(first, second);
    }
}
