//! Shared `/v1` HTTP router for the broker component's two lifetimes
//! (decision #7): the ephemeral per-plugin broker (`plugins::broker`) and
//! the long-lived agent (`session::agent`). Carrier per the maintainer's
//! Docker/Portainer model — HTTP/1.1 over a socket, versioned `/v1`,
//! `Authorization: Bearer <token>` per request, JSON errors. The two
//! lifetimes differ only in a [`Backend`]: how a token maps to scopes and
//! how a request's optional `account` maps to an unlocked [`Ctx`].
//! Responses never contain auth material; tokens are never logged.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use secrecy::SecretString;
use serde_json::{Value, json};

use crate::config::ctx::Ctx;

/// Longest request body the router accepts (API bodies are small JSON).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The result of authenticating a bearer token.
pub struct Auth {
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Account this token is pinned to, if any (AUD-117). When set, it
    /// **overrides** the request's `account` — a bound token can never
    /// reach another account.
    pub account: Option<String>,
    /// Who the token belongs to (label, or `admin`) — for the audit log.
    pub caller: String,
}

/// The request's selector headers (AUD-125): `X-Audible-Account`,
/// `X-Audible-Marketplace`, `X-Audible-Settings`. They apply to every
/// endpoint; a missing header means the server-config default. Body
/// selector fields no longer exist.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Selection {
    pub account: Option<String>,
    pub marketplace: Option<String>,
    pub settings: Option<String>,
}

/// Reads the selector headers, or a ready 400 for a malformed value.
#[allow(clippy::result_large_err)]
fn parse_selectors<B>(
    request: &Request<B>,
) -> std::result::Result<Selection, Box<Response<BoxedBody>>> {
    let header = |name: &str| -> std::result::Result<Option<String>, Box<Response<BoxedBody>>> {
        match request.headers().get(name) {
            None => Ok(None),
            Some(value) => match value.to_str() {
                Ok(text) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
                _ => Err(Box::new(reply(
                    StatusCode::BAD_REQUEST,
                    &json!({"error": format!("invalid {name} header")}),
                ))),
            },
        }
    };
    Ok(Selection {
        account: header("x-audible-account")?,
        marketplace: header("x-audible-marketplace")?,
        settings: header("x-audible-settings")?,
    })
}

/// Response extension carrying audit context (AUD-122): set on the
/// external-host path (`external:<host>`, attempted or served) so the
/// router can log which host the account credentials were aimed at.
/// Extensions never serialize — this stays internal.
#[derive(Clone)]
struct AuditDetail(String);

/// The AUD-123 gate: without selector permission (ephemeral broker) the
/// parsed headers are dropped wholesale — a plugin inherits the invoking
/// `-a/-m/-s` and cannot re-select.
fn effective_selection(selectable: bool, parsed: Selection) -> Selection {
    if selectable {
        parsed
    } else {
        Selection::default()
    }
}

/// Fail-closed account resolution (AUD-125): a bound token plus a
/// **different** requested account is refused with 403 — never silently
/// substituted. Returns the account to use (`None` = server default).
#[allow(clippy::result_large_err)]
fn effective_account<'s>(
    auth: &'s Auth,
    selection: &'s Selection,
) -> std::result::Result<Option<&'s str>, Box<Response<BoxedBody>>> {
    match (auth.account.as_deref(), selection.account.as_deref()) {
        (Some(bound), Some(requested)) if bound != requested => Err(Box::new(reply(
            StatusCode::FORBIDDEN,
            &json!({"error": format!("this token is bound to another account")}),
        ))),
        (bound, requested) => Ok(bound.or(requested)),
    }
}

/// The validated viewer account for the jobs GET routes (audit
/// 2026-07-17, B6): the same fail-closed selector contract as every
/// other route — a bound token with a foreign selector is 403, an
/// unknown account/marketplace/settings selector a 400 — instead of the
/// old unvalidated pass-through (unknown account → empty 200/404). An
/// unbound, selector-less caller keeps the unscoped view.
async fn job_viewer(
    backend: &dyn Backend,
    auth: &Auth,
    selection: &Selection,
) -> std::result::Result<Option<String>, Box<Response<BoxedBody>>> {
    if effective_account(auth, selection)?.is_none() {
        return Ok(None);
    }
    let ctx = select_session(backend, auth, selection).await?;
    let name = ctx.account_name().map_err(|error| {
        Box::new(reply(
            StatusCode::BAD_REQUEST,
            &json!({"error": format!("{error:#}")}),
        ))
    })?;
    Ok(Some(name))
}

/// Resolves and validates the request's session (AUD-125, fail-closed):
/// 403 when a bound token asks for another account, 400 for an unknown
/// account, a marketplace outside the account's marketplaces, or an
/// unknown settings bundle. Never silently substitutes — the caller gets
/// exactly what was requested or an error naming the selector.
async fn select_session(
    backend: &dyn Backend,
    auth: &Auth,
    selection: &Selection,
) -> std::result::Result<Arc<Ctx>, Box<Response<BoxedBody>>> {
    let bad =
        |message: String| Box::new(reply(StatusCode::BAD_REQUEST, &json!({ "error": message })));
    let account = effective_account(auth, selection)?;
    let ctx = backend
        .session(account)
        .await
        .map_err(|error| bad(format!("{error:#}")))?;
    // An unknown account may only surface once the ctx resolves it.
    ctx.account_name()
        .map_err(|error| bad(format!("{error:#}")))?;
    if let Some(marketplace) = selection.marketplace.as_deref() {
        // Validate against the account's marketplace axis (everything the
        // account is configured for), not the default selection.
        let allowed = &ctx
            .account()
            .map_err(|error| bad(format!("{error:#}")))?
            .marketplaces;
        for part in marketplace.split(',').map(str::trim) {
            if part != "all" && !allowed.iter().any(|cc| cc == part) {
                return Err(bad(format!(
                    "marketplace {part:?} is not configured for this account \
                     (allowed: {})",
                    allowed.join(", ")
                )));
            }
        }
    }
    // The reserved `default` bundle is always selectable, even when no
    // `[settings.default]` section exists (it resolves to code defaults)
    // — same rule as the CLI's `-s default`.
    if let Some(settings) = selection.settings.as_deref()
        && settings != crate::config::schema::DEFAULT_SETTINGS_NAME
        && !ctx.config().settings.contains_key(settings)
    {
        return Err(bad(format!("unknown settings bundle {settings:?}")));
    }
    Ok(ctx)
}

/// The behavior distinguishing the two lifetimes.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Authenticates a bearer token to its scopes (+ optional account
    /// binding), or `None` when unauthenticated. Ephemeral: one token,
    /// the manifest scopes, no binding. Agent: admin token or app-token
    /// store.
    fn authenticate(&self, token: &str) -> Option<Auth>;

    /// The unlocked context for a request's optional `account` selector.
    /// Ephemeral: always the invoking `Ctx` (ignores `account`). Agent:
    /// the session map (lazily unlocking via `password_source`).
    async fn session(&self, account: Option<&str>) -> Result<Arc<Ctx>>;

    /// Whether callers may select account/marketplace per request
    /// (AUD-123). The ephemeral broker refuses — a plugin runs strictly
    /// under the invoking `-a/-m/-s` and cannot re-select through the
    /// broker. The agent allows it (serving several accounts is its
    /// point).
    fn allows_selectors(&self) -> bool {
        false
    }

    /// The binary `/v1/invoke` self-execs (the running CLI).
    fn invoke_exe(&self) -> PathBuf;

    /// Names of the built-in commands `/v1/invoke`/`/v1/jobs` may
    /// self-exec. Supplied by the composition root (which owns the CLI
    /// registry) — the shared router itself must never reach upward into
    /// the commands layer (audit 2026-07-17, E1).
    fn builtin_names(&self) -> &[String];

    /// External hosts this lifetime may reach (AUD-124): the ephemeral
    /// broker reads `[plugins] allowed_hosts`, the agent `[session]
    /// allowed_hosts` (fresh from disk, AUD-121). Default: none —
    /// deny-by-default.
    fn allowed_hosts(&self) -> Vec<crate::config::schema::AllowedHost> {
        Vec::new()
    }

    /// The CLI command that approves a host for this lifetime — used in
    /// the refusal hint.
    fn allow_host_command(&self) -> &'static str {
        "plugin allow-host"
    }

    // --- admin surface (`/v1/agent/*`) — agent lifetime only -----------

    /// Whether the token may use the admin endpoints (unlock/lock/status).
    /// The ephemeral broker has no admin surface.
    fn is_admin(&self, _token: &str) -> bool {
        false
    }

    /// Unlocks an account into the session map with an explicit
    /// passphrase; returns the resolved account name (AUD-116).
    async fn unlock(&self, _account: Option<&str>, _password: SecretString) -> Result<String> {
        anyhow::bail!("unlock is only available in agent mode")
    }

    /// Locks account(s); returns how many sessions were dropped.
    async fn lock(&self, _account: Option<&str>, _all: bool) -> Result<usize> {
        anyhow::bail!("lock is only available in agent mode")
    }

    /// Snapshot of the agent's state (unlocked accounts, counts).
    async fn agent_status(&self) -> Result<Value> {
        anyhow::bail!("status is only available in agent mode")
    }

    /// Records one served request. Agent-only; the ephemeral broker (a
    /// user's own CLI run) does not audit. `detail` carries extra
    /// context for security-relevant calls (AUD-122), e.g. the external
    /// target host.
    fn audit(
        &self,
        _caller: &str,
        _method: &str,
        _path: &str,
        _status: u16,
        _detail: Option<&str>,
    ) {
    }

    // --- jobs (`/v1/jobs`) — agent lifetime only -----------------------

    /// Starts an async built-in invocation; returns its job id (AUD-119).
    /// `account` is the resolved account name (job ownership, AUD-125);
    /// `marketplace`/`settings` become the child's `-m`/`-s`.
    async fn start_job(
        &self,
        _argv: Vec<String>,
        _output: String,
        _account: String,
        _marketplace: Option<String>,
        _settings: Option<String>,
    ) -> Result<String> {
        anyhow::bail!("jobs are only available in agent mode")
    }

    /// Status of one job (`None` when the id is unknown — or owned by a
    /// different account than `viewer`, which reads as unknown to that
    /// caller; AUD-125).
    async fn job_status(&self, _id: &str, _viewer: Option<&str>) -> Option<Value> {
        None
    }

    /// All known jobs (id, account, state), filtered to `viewer`'s
    /// account when set (AUD-125).
    async fn list_jobs(&self, _viewer: Option<&str>) -> Value {
        json!({ "jobs": [] })
    }
}

type BoxedBody = Full<Bytes>;

/// Serves `/v1` on an accepted stream until the peer closes it. `trusted`
/// = the connection came in over the local unix socket; untrusted (TCP)
/// connections cannot use the admin endpoints (AUD-119).
pub async fn serve_connection<I>(io: I, backend: Arc<dyn Backend>, trusted: bool)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let service = service_fn(move |request| handle(request, Arc::clone(&backend), trusted));
    if let Err(error) = hyper::server::conn::http1::Builder::new()
        // Tolerate raw clients that shut their write side before reading.
        .half_close(true)
        .serve_connection(io, service)
        .await
    {
        tracing::debug!(%error, "rpc connection error");
    }
}

/// Accept loop over a bound `UnixListener` (trusted); runs until aborted.
#[cfg(unix)]
pub async fn serve_unix(listener: tokio::net::UnixListener, backend: Arc<dyn Backend>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let backend = Arc::clone(&backend);
                tokio::spawn(
                    async move { serve_connection(TokioIo::new(stream), backend, true).await },
                );
            }
            Err(error) => tracing::debug!(%error, "rpc accept failed"),
        }
    }
}

/// Accept loop over a bound `TcpListener` (untrusted — no admin routes).
pub async fn serve_tcp(listener: tokio::net::TcpListener, backend: Arc<dyn Backend>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let backend = Arc::clone(&backend);
                tokio::spawn(async move {
                    serve_connection(TokioIo::new(stream), backend, false).await
                });
            }
            Err(error) => tracing::debug!(%error, "rpc accept failed"),
        }
    }
}

async fn handle(
    request: Request<hyper::body::Incoming>,
    backend: Arc<dyn Backend>,
    trusted: bool,
) -> Result<Response<BoxedBody>, std::convert::Infallible> {
    let response = route(request, backend.as_ref(), trusted)
        .await
        .unwrap_or_else(|error| {
            // Command errors surface verbatim; client errors are
            // sanitized at their source, so they carry no secrets.
            reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": format!("{error:#}")}),
            )
        });
    Ok(response)
}

/// Caller name audited for requests that never authenticated (AUD-134);
/// filterable like any label via `agent audit --caller unauthenticated`.
const UNAUTHENTICATED: &str = "unauthenticated";

/// Audit detail for a rejected bearer: a non-reversible fingerprint of
/// the presented token — per the security policy never the token itself,
/// but enough that repeated probing with the same wrong token
/// correlates. `no token` when the header was missing entirely.
fn rejected_token_detail(token: Option<&str>) -> String {
    use sha2::Digest as _;
    match token {
        Some(token) => {
            let hash = sha2::Sha256::digest(token.as_bytes());
            format!("token-sha256:{}", hex::encode(&hash[..6]))
        }
        None => "no token".to_owned(),
    }
}

async fn route(
    request: Request<hyper::body::Incoming>,
    backend: &dyn Backend,
    trusted: bool,
) -> Result<Response<BoxedBody>> {
    let token = bearer(&request).map(str::to_owned);
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    // One audit entry per request — refusals included (AUD-134), so a
    // revoked web-backend token or someone probing the TCP listener is
    // visible in `agent audit`. Agent-only: the ephemeral broker's
    // `audit` is a no-op. Deliberately no rate limit: on the opt-in TCP
    // listener the 401 volume is attacker-controlled, but the entries
    // are small, append-only, and exactly what the log exists for.
    let audit = |caller: &str, status: StatusCode, detail: Option<&str>| {
        backend.audit(caller, method.as_str(), &path, status.as_u16(), detail);
    };

    // Admin surface (`/v1/agent/*`): local (unix socket) only, gated by
    // the admin token. Never reachable over TCP (AUD-119).
    if let Some(rest) = path.strip_prefix("/v1/agent/") {
        if !trusted {
            let response = reply(
                StatusCode::FORBIDDEN,
                &json!({"error": "admin endpoints are only available over the local socket"}),
            );
            audit(
                UNAUTHENTICATED,
                response.status(),
                Some("admin endpoint over tcp"),
            );
            return Ok(response);
        }
        if !token
            .as_deref()
            .is_some_and(|token| backend.is_admin(token))
        {
            let response = reply(
                StatusCode::UNAUTHORIZED,
                &json!({"error": "admin token required"}),
            );
            audit(
                UNAUTHENTICATED,
                response.status(),
                Some(&rejected_token_detail(token.as_deref())),
            );
            return Ok(response);
        }
        // Errors become the 500 reply here (instead of in `handle`) so
        // the admin call is audited with its real outcome.
        let response = match admin_route(&method, rest, request, backend).await {
            Ok(response) => response,
            Err(error) => reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": format!("{error:#}")}),
            ),
        };
        audit("admin", response.status(), None);
        return Ok(response);
    }

    let Some(auth) = token
        .as_deref()
        .and_then(|token| backend.authenticate(token))
    else {
        let response = reply(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "missing or wrong bearer token"}),
        );
        audit(
            UNAUTHENTICATED,
            response.status(),
            Some(&rejected_token_detail(token.as_deref())),
        );
        return Ok(response);
    };
    // Selector headers (AUD-125), gated by AUD-123: the ephemeral broker
    // drops them wholesale, the agent validates fail-closed downstream.
    let selection = match parse_selectors(&request) {
        Ok(parsed) => effective_selection(backend.allows_selectors(), parsed),
        Err(bad) => {
            audit(&auth.caller, bad.status(), None);
            return Ok(*bad);
        }
    };
    // Dispatch, then audit with the reply's status. Errors become the
    // 500 reply here so failed requests are audited too.
    let response =
        match dispatch(&method, &path, request, backend, &auth, &selection, trusted).await {
            Ok(response) => response,
            Err(error) => reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({"error": format!("{error:#}")}),
            ),
        };
    let detail = response.extensions().get::<AuditDetail>().cloned();
    audit(
        &auth.caller,
        response.status(),
        detail.as_ref().map(|detail| detail.0.as_str()),
    );
    Ok(response)
}

/// The scope-gated data routes. Split out so [`route`] can audit the
/// resulting status for every served request.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    method: &Method,
    path: &str,
    request: Request<hyper::body::Incoming>,
    backend: &dyn Backend,
    auth: &Auth,
    selection: &Selection,
    trusted: bool,
) -> Result<Response<BoxedBody>> {
    let has = |scope: &str| auth.scopes.iter().any(|granted| granted == scope);
    let deny = |scope: &str| {
        reply(
            StatusCode::FORBIDDEN,
            &json!({"error": format!("scope {scope:?} not granted")}),
        )
    };
    match (method, path) {
        (&Method::POST, "/v1/api/request") => {
            if !has("api") {
                return Ok(deny("api"));
            }
            let body = read_json_body(request).await?;
            api_request(backend, body, auth, selection, has("hosts"), trusted).await
        }
        (&Method::GET, "/v1/config/resolved") => {
            if !has("config") {
                return Ok(deny("config"));
            }
            config_resolved(backend, auth, selection).await
        }
        (&Method::GET, "/v1/accounts") => {
            if !has("config") {
                return Ok(deny("config"));
            }
            accounts(backend).await
        }
        (&Method::POST, "/v1/invoke") => {
            if !has("invoke") {
                return Ok(deny("invoke"));
            }
            let body = read_json_body(request).await?;
            invoke_command(backend, body, auth, selection).await
        }
        (&Method::POST, "/v1/jobs") => {
            if !has("invoke") {
                return Ok(deny("invoke"));
            }
            let body = read_json_body(request).await?;
            start_job(backend, body, auth, selection).await
        }
        (&Method::GET, "/v1/jobs") => {
            if !has("invoke") {
                return Ok(deny("invoke"));
            }
            let viewer = match job_viewer(backend, auth, selection).await {
                Ok(viewer) => viewer,
                Err(refusal) => return Ok(*refusal),
            };
            Ok(reply(
                StatusCode::OK,
                &backend.list_jobs(viewer.as_deref()).await,
            ))
        }
        (&Method::GET, jobs_id) if jobs_id.starts_with("/v1/jobs/") => {
            if !has("invoke") {
                return Ok(deny("invoke"));
            }
            let viewer = match job_viewer(backend, auth, selection).await {
                Ok(viewer) => viewer,
                Err(refusal) => return Ok(*refusal),
            };
            let viewer = viewer.as_deref();
            let id = &jobs_id["/v1/jobs/".len()..];
            match backend.job_status(id, viewer).await {
                Some(status) => Ok(reply(StatusCode::OK, &status)),
                None => Ok(reply(
                    StatusCode::NOT_FOUND,
                    &json!({"error": format!("no job {id:?}")}),
                )),
            }
        }
        _ => Ok(reply(
            StatusCode::NOT_FOUND,
            &json!({"error": format!("no route {method} {path}")}),
        )),
    }
}

/// Dispatch for the admin routes (`/v1/agent/<rest>`); the caller has
/// already verified the admin token.
async fn admin_route(
    method: &Method,
    rest: &str,
    request: Request<hyper::body::Incoming>,
    backend: &dyn Backend,
) -> Result<Response<BoxedBody>> {
    match (method, rest) {
        (&Method::POST, "unlock") => {
            let body = read_json_body(request).await?;
            let Some(passphrase) = body.get("passphrase").and_then(Value::as_str) else {
                return Ok(reply(
                    StatusCode::BAD_REQUEST,
                    &json!({"error": "missing \"passphrase\""}),
                ));
            };
            let account = body.get("account").and_then(Value::as_str);
            match backend
                .unlock(account, SecretString::from(passphrase.to_owned()))
                .await
            {
                Ok(name) => Ok(reply(StatusCode::OK, &json!({"unlocked": name}))),
                Err(error) => Ok(reply(
                    StatusCode::BAD_REQUEST,
                    &json!({"error": format!("{error:#}")}),
                )),
            }
        }
        (&Method::POST, "lock") => {
            let body = read_json_body(request).await?;
            let account = body.get("account").and_then(Value::as_str);
            let all = body.get("all").and_then(Value::as_bool).unwrap_or(false);
            let dropped = backend.lock(account, all).await?;
            Ok(reply(StatusCode::OK, &json!({"locked": dropped})))
        }
        (&Method::GET, "status") => {
            let status = backend.agent_status().await?;
            Ok(reply(StatusCode::OK, &status))
        }
        _ => Ok(reply(
            StatusCode::NOT_FOUND,
            &json!({"error": format!("no admin route {method} /v1/agent/{rest}")}),
        )),
    }
}

fn bearer(request: &Request<hyper::body::Incoming>) -> Option<&str> {
    request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

async fn read_json_body(request: Request<hyper::body::Incoming>) -> Result<Value> {
    let body = http_body_util::Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map_err(|error| anyhow::anyhow!("could not read the request body: {error}"))?
        .to_bytes();
    serde_json::from_slice(&body).context("request body is not valid JSON")
}

/// `POST /v1/api/request` — one Audible API call through the selected
/// account's client. Paths only (normalized like `audible api`); a caller
/// can never address a foreign host.
async fn api_request(
    backend: &dyn Backend,
    body: Value,
    auth: &Auth,
    selection: &Selection,
    has_hosts: bool,
    trusted: bool,
) -> Result<Response<BoxedBody>> {
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    let Some(path) = text("path") else {
        return Ok(reply(
            StatusCode::BAD_REQUEST,
            &json!({"error": "missing \"path\""}),
        ));
    };
    let external = path.contains("://");
    if !path.starts_with('/') && !external {
        return Ok(reply(
            StatusCode::BAD_REQUEST,
            &json!({"error": "\"path\" must be an API path like /1.0/library or an https:// URL"}),
        ));
    }
    let method = match text("method")
        .unwrap_or_else(|| "GET".to_owned())
        .to_ascii_uppercase()
        .parse::<reqwest::Method>()
    {
        Ok(method) => method,
        Err(_) => {
            return Ok(reply(
                StatusCode::BAD_REQUEST,
                &json!({"error": "invalid \"method\""}),
            ));
        }
    };
    // Fail-closed selector resolution (AUD-125): 403/400 instead of
    // silent substitution.
    let ctx = match select_session(backend, auth, selection).await {
        Ok(ctx) => ctx,
        Err(refusal) => return Ok(*refusal),
    };
    // One API call, one marketplace — csv/all is an invoke concept.
    if let Some(marketplace) = selection.marketplace.as_deref()
        && (marketplace.contains(',') || marketplace == "all")
    {
        return Ok(reply(
            StatusCode::BAD_REQUEST,
            &json!({"error": "api requests take exactly one marketplace"}),
        ));
    }
    let client = ctx.client().await?;

    // Audit context (AUD-122): every external attempt — refused or
    // served — records its target host, so the log shows where the
    // account credentials were aimed, not just "api/request".
    let audit_detail = external
        .then(|| reqwest::Url::parse(&path).ok())
        .flatten()
        .and_then(|url| {
            url.host_str()
                .map(|host| AuditDetail(format!("external:{host}")))
        });
    let tag = |mut response: Response<BoxedBody>| {
        if let Some(detail) = audit_detail.clone() {
            response.extensions_mut().insert(detail);
        }
        response
    };

    // External-host path (AUD-120): allowlisted hosts, per-host auth,
    // gated by the `hosts` scope and — over TCP — the opt-in flag.
    let mut api = if external {
        match external_request(backend, &ctx, client, &method, &path, has_hosts, trusted) {
            Ok(request) => request,
            Err(refusal) => return Ok(tag(*refusal)),
        }
    } else {
        let marketplace = match selection.marketplace.clone() {
            Some(marketplace) => marketplace,
            None => ctx.marketplace_single()?,
        };
        let normalized = crate::api::normalize_api_path(&path);
        client
            .request(method, normalized)
            .country_code(&marketplace)
    };

    if let Some(query) = body.get("query").and_then(Value::as_object) {
        for (key, value) in query {
            let value = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            api = api.query(key, value);
        }
    }
    if let Some(request_body) = body.get("body")
        && !request_body.is_null()
    {
        api = api.body(request_body.clone());
    }

    let response = api.send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;
    let payload: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Ok(tag(reply(
        StatusCode::OK,
        &json!({"status": status, "body": payload}),
    )))
}

/// Builds an external-host request against a user-approved host with its
/// configured auth (AUD-120), or returns a ready refusal response. Every
/// guard is deny-by-default: `hosts` scope, HTTPS, TCP opt-in, exact
/// allowlist match.
#[allow(clippy::result_large_err)]
fn external_request<'c>(
    backend: &dyn Backend,
    ctx: &Ctx,
    client: &'c crate::api::client::Client,
    method: &reqwest::Method,
    url: &str,
    has_hosts: bool,
    trusted: bool,
) -> std::result::Result<crate::api::client::RequestBuilder<'c>, Box<Response<BoxedBody>>> {
    let refuse =
        |status: StatusCode, message: String| Box::new(reply(status, &json!({ "error": message })));
    if !has_hosts {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "external-host calls need the \"hosts\" scope".to_owned(),
        ));
    }
    if !trusted && !ctx.config().session.allow_external_over_tcp {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "external-host calls are disabled over TCP (set [session] \
             allow_external_over_tcp = true to allow)"
                .to_owned(),
        ));
    }
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err(refuse(
                StatusCode::BAD_REQUEST,
                format!("invalid URL {url:?}"),
            ));
        }
    };
    if parsed.scheme() != "https" {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "external-host calls must use https".to_owned(),
        ));
    }
    let host = parsed.host_str().unwrap_or_default().to_owned();
    // The allowlist is per lifetime (AUD-124): the ephemeral broker
    // reads `[plugins] allowed_hosts`, the agent `[session]
    // allowed_hosts` — a host approved for local plugins is not thereby
    // approved for network callers, and vice versa.
    let Some(entry) = backend
        .allowed_hosts()
        .into_iter()
        .find(|entry| entry.host == host)
    else {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            format!(
                "host {host:?} is not allowed — approve it with \
                 `audible {} {host}`",
                backend.allow_host_command()
            ),
        ));
    };
    let mode = entry
        .auth
        .parse::<crate::api::client::AuthMode>()
        .unwrap_or(crate::api::client::AuthMode::Signing);
    Ok(client.request_absolute(method.clone(), parsed).auth(mode))
}

/// `POST /v1/invoke` — runs one **built-in** command via self-exec and
/// returns its captured output (AUD-114). Built-ins only (recursion
/// guard); the child inherits the resolved `-a`/`-s` (plus raw `-m`) of
/// the request's account, so the caller sees exactly that account's view.
async fn invoke_command(
    backend: &dyn Backend,
    body: Value,
    auth: &Auth,
    selection: &Selection,
) -> Result<Response<BoxedBody>> {
    let mut child = match build_invoke(backend, &body, auth, selection).await {
        Ok(child) => child,
        Err(bad) => return Ok(*bad),
    };
    let output = child
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("could not run the invoke self-exec")?;
    Ok(reply(
        StatusCode::OK,
        &json!({
            "code": output.status.code().unwrap_or(1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }),
    ))
}

/// `POST /v1/jobs` — starts a built-in invocation asynchronously and
/// returns a job id to poll (AUD-119). Same validation and account rules
/// as `/v1/invoke`; the work runs in the agent, not blocking the request.
async fn start_job(
    backend: &dyn Backend,
    body: Value,
    auth: &Auth,
    selection: &Selection,
) -> Result<Response<BoxedBody>> {
    let (argv, output) = match parse_invoke(&body, backend.builtin_names()) {
        Ok(parts) => parts,
        Err(bad) => return Ok(*bad),
    };
    // Fail-closed selector resolution, and the resolved account name
    // becomes the job's owner (AUD-125).
    let ctx = match select_session(backend, auth, selection).await {
        Ok(ctx) => ctx,
        Err(refusal) => return Ok(*refusal),
    };
    let id = backend
        .start_job(
            argv,
            output,
            ctx.account_name()?,
            selection.marketplace.clone(),
            selection.settings.clone(),
        )
        .await?;
    Ok(reply(StatusCode::ACCEPTED, &json!({ "job_id": id })))
}

/// Parses + validates an invoke/job body into `(argv, output)`, or a
/// ready 400 response. Shared by `/v1/invoke` and `/v1/jobs`. Selectors
/// come from headers, never the body (AUD-125). `builtins` comes from the
/// backend ([`Backend::builtin_names`]).
fn parse_invoke(
    body: &Value,
    builtins: &[String],
) -> std::result::Result<(Vec<String>, String), Box<Response<BoxedBody>>> {
    let bad =
        |message: String| Box::new(reply(StatusCode::BAD_REQUEST, &json!({ "error": message })));
    let Some(raw_argv) = body.get("argv").and_then(Value::as_array) else {
        return Err(bad("missing \"argv\" (array of strings)".to_owned()));
    };
    // Fail-closed (B6): a non-string element is a 400, never silently
    // dropped — `[42, "library"]` used to run `library`.
    let mut argv: Vec<String> = Vec::with_capacity(raw_argv.len());
    for value in raw_argv {
        match value.as_str() {
            Some(text) => argv.push(text.to_owned()),
            None => {
                return Err(bad(format!(
                    "\"argv\" must be an array of strings (found {value})"
                )));
            }
        }
    }
    let Some(command_name) = argv.first() else {
        return Err(bad("\"argv\" must name a built-in command".to_owned()));
    };
    if !builtins.iter().any(|builtin| builtin == command_name) {
        return Err(bad(format!(
            "{command_name:?} is not a built-in command (plugins cannot invoke plugins)"
        )));
    }
    let output = body.get("output").and_then(Value::as_str).unwrap_or("json");
    if !["json", "table", "plain"].contains(&output) {
        return Err(bad("\"output\" must be json, table or plain".to_owned()));
    }
    Ok((argv, output.to_owned()))
}

/// Builds the self-exec command for an invoke body, or a ready 4xx.
async fn build_invoke(
    backend: &dyn Backend,
    body: &Value,
    auth: &Auth,
    selection: &Selection,
) -> std::result::Result<tokio::process::Command, Box<Response<BoxedBody>>> {
    let (argv, output) = parse_invoke(body, backend.builtin_names())?;
    let ctx = select_session(backend, auth, selection).await?;
    let mut child = tokio::process::Command::new(backend.invoke_exe());
    child.args(invoke_argv(
        &ctx,
        &output,
        &argv,
        selection.marketplace.as_deref(),
        selection.settings.as_deref(),
    )?);
    Ok(child)
}

/// Builds the `-a/-s/-o[/-m]` prefix + argv for a resolved context.
/// `marketplace`/`settings` are validated selector overrides (AUD-125);
/// without them the context's own selection applies. Boxed error to keep
/// the ready-response callers uniform.
#[allow(clippy::result_large_err)]
pub fn invoke_argv(
    ctx: &Ctx,
    output: &str,
    argv: &[String],
    marketplace: Option<&str>,
    settings: Option<&str>,
) -> std::result::Result<Vec<std::ffi::OsString>, Box<Response<BoxedBody>>> {
    let bad = |error: anyhow::Error| {
        Box::new(reply(
            StatusCode::BAD_REQUEST,
            &json!({"error": format!("{error:#}")}),
        ))
    };
    let settings = match settings {
        Some(settings) => settings.to_owned(),
        None => ctx.settings_name().map_err(bad)?,
    };
    let mut parts: Vec<std::ffi::OsString> = vec![
        "-a".into(),
        ctx.account_name().map_err(bad)?.into(),
        "-s".into(),
        settings.into(),
        "-o".into(),
        output.into(),
    ];
    if let Some(marketplace) = marketplace
        .map(str::to_owned)
        .or_else(|| ctx.marketplace_selector())
    {
        parts.push("-m".into());
        parts.push(marketplace.into());
    }
    parts.extend(argv.iter().map(std::ffi::OsString::from));
    Ok(parts)
}

/// `GET /v1/config/resolved` — the effective settings view of the
/// selected account. Hand-picked secret-free fields.
async fn config_resolved(
    backend: &dyn Backend,
    auth: &Auth,
    selection: &Selection,
) -> Result<Response<BoxedBody>> {
    let ctx = match select_session(backend, auth, selection).await {
        Ok(ctx) => ctx,
        Err(refusal) => return Ok(*refusal),
    };
    // The settings header selects the reported bundle (validated above).
    let settings_name = match selection.settings.clone() {
        Some(settings) => settings,
        None => ctx.settings_name()?,
    };
    let view = crate::config::resolve::SettingsView::resolve(ctx.config(), &settings_name)?;
    let payload = json!({
        "account": ctx.account_name()?,
        "settings": settings_name,
        "marketplaces": ctx.marketplaces()?,
        "download_dir": crate::naming::download_dir_for(&view),
        "filename_mode": format!("{:?}", view.filename_mode(None, None)).to_lowercase(),
        "filename_template": view.filename_template(),
        "filename_max_length": view.filename_max_length(None, None),
        "decrypt": view.decrypt(),
        "cover_size": view.cover_size(None, None),
        "chapter_type": view.chapter_type(None, None),
    });
    Ok(reply(StatusCode::OK, &payload))
}

/// `GET /v1/accounts` — the discovery call for the selector headers
/// (AUD-126): every account with its marketplace axis and effective
/// default bundle, plus all selectable settings-bundle names. Names
/// only, never values.
async fn accounts(backend: &dyn Backend) -> Result<Response<BoxedBody>> {
    use crate::config::schema::DEFAULT_SETTINGS_NAME;
    // Any session works; the config is identical across them.
    let ctx = backend.session(None).await?;
    let config = ctx.config();
    let default = config.default_account.as_deref();
    let list: Vec<Value> = config
        .accounts
        .iter()
        .map(|(name, account)| {
            json!({
                "name": name,
                "marketplaces": account.marketplaces,
                "default_marketplaces": account.default_marketplaces,
                "is_default": Some(name.as_str()) == default,
                // What applies without an X-Audible-Settings header.
                "default_settings": account
                    .default_settings
                    .as_deref()
                    .unwrap_or(DEFAULT_SETTINGS_NAME),
            })
        })
        .collect();
    // All valid X-Audible-Settings values: the configured bundles plus
    // the always-selectable implicit `default` (BTreeMap keys sort).
    let mut settings: Vec<&str> = config.settings.keys().map(String::as_str).collect();
    if !config.settings.contains_key(DEFAULT_SETTINGS_NAME) {
        settings.insert(0, DEFAULT_SETTINGS_NAME);
    }
    Ok(reply(
        StatusCode::OK,
        &json!({"accounts": list, "settings": settings}),
    ))
}

fn reply(status: StatusCode, payload: &Value) -> Response<BoxedBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload.to_string())))
        .expect("static response parts are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built-ins the test backends expose to `/v1/invoke` (the trait
    /// returns a slice, so the list needs a stable home).
    static TEST_BUILTINS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

    /// A selectable backend over a two-account fixture config (no auth
    /// material — `select_session` never touches `client()`).
    struct TestBackend {
        config_dir: PathBuf,
    }

    #[async_trait::async_trait]
    impl Backend for TestBackend {
        fn authenticate(&self, _token: &str) -> Option<Auth> {
            None
        }
        fn allows_selectors(&self) -> bool {
            true
        }
        async fn session(&self, account: Option<&str>) -> Result<Arc<Ctx>> {
            Ok(Arc::new(Ctx::with_dir(
                self.config_dir.clone(),
                crate::config::ctx::Selectors {
                    account: account.map(str::to_owned),
                    ..Default::default()
                },
            )?))
        }
        fn invoke_exe(&self) -> PathBuf {
            PathBuf::from("/bin/false")
        }
        fn builtin_names(&self) -> &[String] {
            TEST_BUILTINS.get_or_init(|| vec!["library".to_owned()])
        }
    }

    fn fixture_backend() -> (TestBackend, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "version = 1\ndefault_account = \"smoke\"\n\n\
             [accounts.smoke]\nauth_file = \"smoke.auth\"\n\
             marketplaces = [\"de\", \"us\"]\ndefault_marketplaces = [\"de\"]\n\n\
             [settings.fast]\n",
        )
        .unwrap();
        let backend = TestBackend {
            config_dir: tmp.path().to_path_buf(),
        };
        (backend, tmp)
    }

    fn auth(bound: Option<&str>) -> Auth {
        Auth {
            scopes: vec!["api".into()],
            account: bound.map(str::to_owned),
            caller: "test".into(),
        }
    }

    fn select(
        account: Option<&str>,
        marketplace: Option<&str>,
        settings: Option<&str>,
    ) -> Selection {
        Selection {
            account: account.map(str::to_owned),
            marketplace: marketplace.map(str::to_owned),
            settings: settings.map(str::to_owned),
        }
    }

    /// AUD-123: without selector permission the parsed headers are
    /// dropped wholesale — the invoking context wins.
    #[test]
    fn selection_is_dropped_without_permission() {
        let parsed = select(Some("other"), Some("us"), Some("fast"));
        assert_eq!(
            effective_selection(false, parsed.clone()),
            Selection::default()
        );
        assert_eq!(effective_selection(true, parsed.clone()), parsed);
    }

    /// B6: `argv` elements must all be strings — a non-string used to be
    /// silently dropped, so `[42, "library"]` ran `library`.
    #[test]
    fn invoke_argv_rejects_non_strings() {
        let builtins = vec!["library".to_owned()];
        let refused = parse_invoke(&json!({"argv": [42, "library"]}), &builtins)
            .expect_err("non-string argv element must refuse");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        let (argv, output) = parse_invoke(&json!({"argv": ["library", "list"]}), &builtins)
            .expect("all-string argv parses");
        assert_eq!(argv, ["library", "list"]);
        assert_eq!(output, "json");
    }

    /// B6: the jobs GET routes validate their selector like every other
    /// route — unknown account = 400, not an empty 200; bound mismatch =
    /// 403; selector-less unbound callers keep the unscoped view.
    #[tokio::test]
    async fn job_viewer_fails_closed() {
        let (backend, _tmp) = fixture_backend();
        let refused = job_viewer(&backend, &auth(None), &select(Some("nope"), None, None))
            .await
            .expect_err("unknown account must refuse");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        let refused = job_viewer(
            &backend,
            &auth(Some("smoke")),
            &select(Some("other"), None, None),
        )
        .await
        .expect_err("bound mismatch must refuse");
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        // Selector-less, unbound: unscoped view (None), no validation hit.
        assert_eq!(
            job_viewer(&backend, &auth(None), &select(None, None, None))
                .await
                .expect("unscoped view resolves"),
            None
        );
        // A valid selector resolves to the account name.
        assert_eq!(
            job_viewer(&backend, &auth(None), &select(Some("smoke"), None, None))
                .await
                .expect("valid selector resolves")
                .as_deref(),
            Some("smoke")
        );
    }

    /// AUD-125 fail-closed matrix: 403 for a bound token asking for
    /// another account; 400 for unknown account/marketplace/settings;
    /// valid selectors resolve.
    #[tokio::test]
    async fn select_session_fails_closed() {
        let (backend, _tmp) = fixture_backend();
        let status = |refusal: Box<Response<BoxedBody>>| refusal.status();

        // Bound token + different account → 403, never substitution.
        let refused = select_session(
            &backend,
            &auth(Some("smoke")),
            &select(Some("other"), None, None),
        )
        .await
        .err()
        .expect("bound mismatch must refuse");
        assert_eq!(status(refused), StatusCode::FORBIDDEN);

        // Bound token + matching or absent account → ok.
        for account in [Some("smoke"), None] {
            assert!(
                select_session(&backend, &auth(Some("smoke")), &select(account, None, None))
                    .await
                    .is_ok(),
                "binding resolves"
            );
        }

        // Unknown account → 400.
        let refused = select_session(&backend, &auth(None), &select(Some("nope"), None, None))
            .await
            .err()
            .expect("unknown account must refuse");
        assert_eq!(status(refused), StatusCode::BAD_REQUEST);

        // Marketplace outside the account's list → 400; configured → ok.
        let refused = select_session(&backend, &auth(None), &select(None, Some("jp"), None))
            .await
            .err()
            .expect("foreign marketplace must refuse");
        assert_eq!(status(refused), StatusCode::BAD_REQUEST);
        assert!(
            select_session(&backend, &auth(None), &select(None, Some("us"), None))
                .await
                .is_ok(),
            "configured marketplace resolves"
        );
        assert!(
            select_session(&backend, &auth(None), &select(None, Some("de,us"), None))
                .await
                .is_ok(),
            "csv of configured marketplaces resolves"
        );

        // Unknown settings bundle → 400; existing → ok.
        let refused = select_session(&backend, &auth(None), &select(None, None, Some("nope")))
            .await
            .err()
            .expect("unknown settings must refuse");
        assert_eq!(status(refused), StatusCode::BAD_REQUEST);
        // The reserved `default` is always selectable — the fixture has
        // no [settings.default] section (AUD-126 fix).
        assert!(
            select_session(&backend, &auth(None), &select(None, None, Some("default")))
                .await
                .is_ok(),
            "reserved default resolves without a [settings.default] section"
        );
        assert!(
            select_session(&backend, &auth(None), &select(None, None, Some("fast")))
                .await
                .is_ok(),
            "existing settings resolves"
        );
    }

    /// Selector overrides land in the child's `-m`/`-s`; without them the
    /// context defaults apply.
    #[tokio::test]
    async fn invoke_argv_honors_selector_overrides() {
        let (backend, _tmp) = fixture_backend();
        let ctx = backend.session(None).await.unwrap();
        let argv = vec!["library".to_owned()];

        let parts = invoke_argv(&ctx, "json", &argv, Some("us"), Some("fast")).unwrap();
        let parts: Vec<String> = parts
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            parts,
            [
                "-a", "smoke", "-s", "fast", "-o", "json", "-m", "us", "library"
            ]
        );

        let parts = invoke_argv(&ctx, "json", &argv, None, None).unwrap();
        let parts: Vec<String> = parts
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        // No -m: the fixture ctx has no marketplace selector of its own.
        assert_eq!(
            parts,
            ["-a", "smoke", "-s", "default", "-o", "json", "library"]
        );
    }

    /// One recorded audit call: `(caller, method, path, status, detail)`.
    type AuditRecord = (String, String, String, u16, Option<String>);

    /// A backend that records every audit call (AUD-134); admin token is
    /// `"admintok"`, bearer auth always fails (like the agent facing an
    /// unknown app token).
    struct AuditingBackend {
        records: std::sync::Mutex<Vec<AuditRecord>>,
    }

    #[async_trait::async_trait]
    impl Backend for AuditingBackend {
        fn authenticate(&self, _token: &str) -> Option<Auth> {
            None
        }
        async fn session(&self, _account: Option<&str>) -> Result<Arc<Ctx>> {
            anyhow::bail!("not used")
        }
        fn invoke_exe(&self) -> PathBuf {
            PathBuf::from("/bin/false")
        }
        fn builtin_names(&self) -> &[String] {
            TEST_BUILTINS.get_or_init(|| vec!["library".to_owned()])
        }
        fn is_admin(&self, token: &str) -> bool {
            token == "admintok"
        }
        async fn agent_status(&self) -> Result<Value> {
            Ok(json!({"unlocked_accounts": []}))
        }
        fn audit(&self, caller: &str, method: &str, path: &str, status: u16, detail: Option<&str>) {
            self.records.lock().unwrap().push((
                caller.to_owned(),
                method.to_owned(),
                path.to_owned(),
                status,
                detail.map(str::to_owned),
            ));
        }
    }

    /// Serves one raw HTTP/1.1 request against the router and returns the
    /// status line; `trusted` mirrors unix-socket (true) vs TCP (false).
    async fn one_request(backend: Arc<AuditingBackend>, trusted: bool, raw: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut client, server) = tokio::io::duplex(8192);
        let serve = tokio::spawn(serve_connection(TokioIo::new(server), backend, trusted));
        client.write_all(raw.as_bytes()).await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        serve.abort();
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    /// AUD-134: refusals and the admin surface land in the audit log —
    /// with a token fingerprint (never the token) for rejected bearers.
    #[tokio::test]
    async fn refusals_and_admin_calls_are_audited() {
        let backend = Arc::new(AuditingBackend {
            records: std::sync::Mutex::new(Vec::new()),
        });

        // Admin over TCP → 403, audited as unauthenticated.
        let status = one_request(
            Arc::clone(&backend),
            false,
            "GET /v1/agent/status HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert!(status.contains("403"), "{status}");

        // Wrong admin token over the socket → 401 with a fingerprint.
        let status = one_request(
            Arc::clone(&backend),
            true,
            "GET /v1/agent/status HTTP/1.1\r\nhost: x\r\n\
             authorization: Bearer wrong\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert!(status.contains("401"), "{status}");

        // Correct admin token → 200, audited as admin.
        let status = one_request(
            Arc::clone(&backend),
            true,
            "GET /v1/agent/status HTTP/1.1\r\nhost: x\r\n\
             authorization: Bearer admintok\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert!(status.contains("200"), "{status}");

        // Data path without any token → 401, audited as "no token".
        let status = one_request(
            Arc::clone(&backend),
            false,
            "GET /v1/accounts HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert!(status.contains("401"), "{status}");

        let records = backend.records.lock().unwrap();
        assert_eq!(records.len(), 4, "{records:?}");

        assert_eq!(records[0].0, UNAUTHENTICATED);
        assert_eq!(records[0].3, 403);
        assert_eq!(records[0].4.as_deref(), Some("admin endpoint over tcp"));

        assert_eq!(records[1].0, UNAUTHENTICATED);
        assert_eq!(records[1].3, 401);
        let detail = records[1].4.as_deref().unwrap();
        assert!(detail.starts_with("token-sha256:"), "{detail}");
        assert!(!detail.contains("wrong"), "never the token itself");

        assert_eq!(records[2].0, "admin");
        assert_eq!(records[2].2, "/v1/agent/status");
        assert_eq!(records[2].3, 200);

        assert_eq!(records[3].0, UNAUTHENTICATED);
        assert_eq!(records[3].3, 401);
        assert_eq!(records[3].4.as_deref(), Some("no token"));
    }

    /// The fingerprint is stable per token, differs across tokens, and
    /// never contains token material.
    #[test]
    fn rejected_token_detail_is_a_fingerprint() {
        assert_eq!(rejected_token_detail(None), "no token");
        let a = rejected_token_detail(Some("secret-token-a"));
        let b = rejected_token_detail(Some("secret-token-b"));
        assert_eq!(a, rejected_token_detail(Some("secret-token-a")));
        assert_ne!(a, b);
        assert!(a.starts_with("token-sha256:"), "{a}");
        assert_eq!(a.len(), "token-sha256:".len() + 12, "6 bytes hex");
        assert!(!a.contains("secret"), "{a}");
    }
}
