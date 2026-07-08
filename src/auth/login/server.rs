//! `account login server` (AUD-60): a loopback reverse-proxy that serves
//! Amazon's sign-in through itself, so a real browser — including a phone via a
//! printed QR code — can complete the login while we capture the OAuth
//! authorization code. A convenience/fallback path on top of the scripted
//! login (AUD-59); it also clears the `br` redirect in a real browser.
//!
//! The user first lands on a small Amazon-styled **config page** (pick the
//! marketplace, device, account name, pre-merger username). On submit the
//! server builds the device/PKCE/authorize URL for that choice and redirects
//! the browser into the proxied Amazon sign-in.
//!
//! Design (mirroring the maintainer's `myaudible` prototype): one upstream
//! reqwest client holds the Amazon session (cookie jar + init cookies); the
//! browser stays "dumb" (its cookies are ignored, upstream `Set-Cookie` is
//! dropped). Served HTML is rewritten so navigation/forms stay on the proxy
//! while assets load from Amazon. As soon as an upstream response carries the
//! authorization code (incl. the `br` maplanding 302), it is captured. No
//! secrets are logged.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use regex::Regex;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

use crate::api::locale::{self, Locale};
use crate::auth::device::{Device, DeviceKind};

use super::internal::BROWSER_USER_AGENT;
use super::{LoginError, Pkce, authorize_url, generate_frc, username_login_supported};

/// Grace time after capture so the browser's success page flushes before the
/// listener is dropped.
const FLUSH_GRACE: Duration = Duration::from_millis(800);

/// Pre-fill values for the config page (from the CLI flags; all optional).
pub struct LoginDefaults {
    pub country_code: Option<String>,
    pub device: DeviceKind,
    pub username: bool,
    pub name: Option<String>,
    pub marketplaces: Option<String>,
    pub default_marketplaces: Option<String>,
    pub plain: bool,
}

/// The login choice + captured code, returned by [`LoginServer::run`].
pub struct ServerLogin {
    pub code: String,
    pub locale: Locale,
    pub device: Device,
    pub pkce: Pkce,
    pub with_username: bool,
    pub name: Option<String>,
    /// CSV of marketplaces the account owns books on (for later data commands).
    pub marketplaces: Option<String>,
    /// CSV subset the global `-m` defaults to when omitted later.
    pub default_marketplaces: Option<String>,
    /// Write the auth file unencrypted (no password).
    pub plain: bool,
}

/// The active sign-in session, created once the config form is submitted.
struct ActiveSession {
    upstream: reqwest::Client,
    /// `https://www.amazon.de` (or the Audible host for username login).
    amazon_base: String,
    locale: Locale,
    device: Device,
    pkce: Pkce,
    with_username: bool,
    name: Option<String>,
    marketplaces: Option<String>,
    default_marketplaces: Option<String>,
    plain: bool,
}

/// Shared proxy state (cheap to clone; the reqwest client is internally Arc'd).
struct ProxyState {
    /// The session path prefix on the proxy, e.g. `/9f3a…`.
    proxy_prefix: String,
    /// The captured authorization code is sent here.
    code_tx: mpsc::Sender<String>,
    /// Pre-fill values for the config page.
    defaults: LoginDefaults,
    /// Set after the config form is submitted.
    session: Mutex<Option<ActiveSession>>,
}

/// A bound login proxy. Build it with [`LoginServer::bind`], show the user the
/// [`LoginServer::landing_path`] under the bound port, then drive it with
/// [`LoginServer::run`].
pub struct LoginServer {
    listener: TcpListener,
    state: Arc<ProxyState>,
    code_rx: mpsc::Receiver<String>,
}

impl LoginServer {
    /// Binds the proxy. No Amazon session is created yet — that happens when
    /// the user submits the config page.
    pub async fn bind(addr: SocketAddr, defaults: LoginDefaults) -> Result<Self, LoginError> {
        // Unguessable session token: the proxy only serves under this path.
        let mut token = [0u8; 16];
        OsRng.fill_bytes(&mut token);
        let proxy_prefix = format!("/{}", hex::encode(token));

        let (code_tx, code_rx) = mpsc::channel(1);
        let state = Arc::new(ProxyState {
            proxy_prefix,
            code_tx,
            defaults,
            session: Mutex::new(None),
        });
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            state,
            code_rx,
        })
    }

    /// The actually bound port (relevant when binding to port 0).
    pub fn local_port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(0)
    }

    /// The path to open in the browser — the config page.
    pub fn landing_path(&self) -> String {
        format!("{}/", self.state.proxy_prefix)
    }

    /// Serves the proxy until the login completes (code captured) or the
    /// timeout elapses; returns the chosen config + captured code.
    pub async fn run(self, timeout: Duration) -> Result<ServerLogin, LoginError> {
        let LoginServer {
            listener,
            state,
            mut code_rx,
        } = self;

        // The accept loop runs in the background; per-connection tasks are
        // detached so an in-flight success page still flushes after we stop.
        let accept_state = Arc::clone(&state);
        let accept = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::debug!(%error, "login server accept failed");
                        continue;
                    }
                };
                let state = Arc::clone(&accept_state);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, Arc::clone(&state)));
                    if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%error, "login server connection error");
                    }
                });
            }
        });

        let captured = tokio::select! {
            captured = code_rx.recv() => {
                // Let the browser's success page flush before we tear down.
                tokio::time::sleep(FLUSH_GRACE).await;
                captured.ok_or(LoginError::Cancelled)
            }
            _ = tokio::time::sleep(timeout) => Err(LoginError::LoginFailed(
                "no sign-in completed within the time limit".to_owned(),
            )),
        };
        accept.abort();

        let code = captured?;
        let session = state
            .session
            .lock()
            .await
            .take()
            .ok_or_else(|| LoginError::LoginFailed("internal: no active session".to_owned()))?;
        Ok(ServerLogin {
            code,
            locale: session.locale,
            device: session.device,
            pkce: session.pkce,
            with_username: session.with_username,
            name: session.name,
            marketplaces: session.marketplaces,
            default_marketplaces: session.default_marketplaces,
            plain: session.plain,
        })
    }
}

/// Builds the upstream client: the Amazon session jar seeded with the init
/// cookies that suppress most captchas. Redirects are **not** followed — they
/// are passed through to the browser (see [`proxy`]) so its address bar stays
/// in sync and Amazon's multi-step pages keep resolving relative links.
fn build_upstream_client(amazon_base: &str) -> Result<reqwest::Client, LoginError> {
    let jar = Arc::new(reqwest::cookie::Jar::default());
    if let Ok(base) = reqwest::Url::parse(&format!("{amazon_base}/")) {
        for cookie in init_cookies() {
            jar.add_cookie_str(&cookie, &base);
        }
    }
    Ok(reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .user_agent(BROWSER_USER_AGENT)
        .cookie_provider(jar)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// `frc` / `map-md` / `amzn-app-id` — the device-context cookies the app sets
/// to reduce captchas (mirrors the reference's `build_init_cookies`).
fn init_cookies() -> Vec<String> {
    let frc = generate_frc();
    let map_md = STANDARD
        .encode(
            serde_json::json!({
                "device_user_dictionary": [],
                "device_registration_data": { "software_version": "35602678" },
                "app_identifier": { "app_version": "3.56.2", "bundle_id": "com.audible.iphone" },
            })
            .to_string(),
        )
        .trim_end_matches('=')
        .to_owned();
    vec![
        format!("frc={frc}; Path=/"),
        format!("map-md={map_md}; Path=/"),
        "amzn-app-id=MAPiOSLib/6.0/ToHideRetailLink; Path=/".to_owned(),
    ]
}

/// Per-connection request handler: config page, form submit, proxy, or 404.
async fn handle(
    req: Request<Incoming>,
    state: Arc<ProxyState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(route(req, &state).await.unwrap_or_else(|error| {
        tracing::debug!(%error, "login proxy error");
        html_response(StatusCode::BAD_GATEWAY, BAD_GATEWAY_PAGE)
    }))
}

async fn route(
    req: Request<Incoming>,
    state: &ProxyState,
) -> Result<Response<Full<Bytes>>, LoginError> {
    let (parts, body) = req.into_parts();

    // Only serve under the unguessable session prefix.
    let Some(rest) = parts.uri.path().strip_prefix(&state.proxy_prefix) else {
        return Ok(html_response(StatusCode::NOT_FOUND, NOT_FOUND_PAGE));
    };

    // The config page and its submit endpoint.
    if parts.method == Method::GET && (rest.is_empty() || rest == "/") {
        return Ok(html_owned(StatusCode::OK, landing_html(state, None)));
    }
    if parts.method == Method::POST && rest == "/__start" {
        return start_session(body, state).await;
    }

    // Everything else is proxied to Amazon — once a session exists.
    let active = {
        let guard = state.session.lock().await;
        guard
            .as_ref()
            .map(|session| (session.upstream.clone(), session.amazon_base.clone()))
    };
    let Some((upstream, amazon_base)) = active else {
        // Not configured yet — send the browser back to the config page.
        return Ok(redirect_response(&format!("{}/", state.proxy_prefix)));
    };

    proxy(parts, body, state, &upstream, &amazon_base).await
}

/// Handles the config-form submit: build the device/PKCE/authorize URL for the
/// chosen marketplace, store the session, and redirect into the proxied login.
async fn start_session(
    body: Incoming,
    state: &ProxyState,
) -> Result<Response<Full<Bytes>>, LoginError> {
    let bytes = body
        .collect()
        .await
        .map_err(|error| LoginError::LoginFailed(format!("reading the form: {error}")))?
        .to_bytes();

    let mut marketplace = state.defaults.country_code.clone().unwrap_or_default();
    let mut device = state.defaults.device;
    let mut name = state.defaults.name.clone();
    let mut marketplaces = state.defaults.marketplaces.clone();
    let mut default_marketplaces = state.defaults.default_marketplaces.clone();
    // Checkboxes are absent when unchecked, so the submitted form is the source
    // of truth (the CLI default only pre-checks the box).
    let mut username = false;
    let mut plain = false;
    let trimmed_opt = |value: std::borrow::Cow<'_, str>| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    for (key, value) in url::form_urlencoded::parse(&bytes) {
        match key.as_ref() {
            "marketplace" => marketplace = value.into_owned(),
            "device" => device = value.parse().unwrap_or(device),
            "username" => username = true,
            "plain" => plain = true,
            "name" => name = trimmed_opt(value),
            "marketplaces" => marketplaces = trimmed_opt(value),
            "default_marketplaces" => default_marketplaces = trimmed_opt(value),
            _ => {}
        }
    }

    let Some(locale) = locale::find(&marketplace) else {
        return Ok(html_owned(
            StatusCode::OK,
            landing_html(state, Some("Please choose a marketplace.")),
        ));
    };
    if username && !username_login_supported(&locale) {
        return Ok(html_owned(
            StatusCode::OK,
            landing_html(
                state,
                Some("Audible-username login is only available for DE, US and UK."),
            ),
        ));
    }

    let device_obj = Device::generate(device);
    let pkce = Pkce::generate();
    let authorize = authorize_url(&device_obj, &pkce, &locale, username);
    let url = reqwest::Url::parse(&authorize).map_err(|_| LoginError::InvalidRedirect)?;
    let amazon_base = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or("www.amazon.com")
    );
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let upstream = build_upstream_client(&amazon_base)?;

    *state.session.lock().await = Some(ActiveSession {
        upstream,
        amazon_base,
        locale,
        device: device_obj,
        pkce,
        with_username: username,
        name,
        marketplaces,
        default_marketplaces,
        plain,
    });

    Ok(redirect_response(&format!(
        "{}{path_and_query}",
        state.proxy_prefix
    )))
}

/// Proxies one browser request to Amazon, capturing the code or rewriting HTML.
async fn proxy(
    parts: hyper::http::request::Parts,
    body: Incoming,
    state: &ProxyState,
    upstream: &reqwest::Client,
    amazon_base: &str,
) -> Result<Response<Full<Bytes>>, LoginError> {
    // The browser navigated to maplanding carrying the code — capture before
    // replaying (also covers `br`, whose maplanding 302s onward).
    if let Some(code) = code_from_query(parts.uri.query()) {
        let _ = state.code_tx.try_send(code);
        return Ok(html_response(StatusCode::OK, SUCCESS_PAGE));
    }

    let rest = parts
        .uri
        .path()
        .strip_prefix(&state.proxy_prefix)
        .unwrap_or("/");
    let rest = if rest.is_empty() { "/" } else { rest };
    let mut upstream_url = format!("{amazon_base}{rest}");
    if let Some(query) = parts.uri.query() {
        upstream_url.push('?');
        upstream_url.push_str(query);
    }

    let headers = forward_headers(&parts.headers);
    let body = body
        .collect()
        .await
        .map_err(|error| LoginError::LoginFailed(format!("reading the browser request: {error}")))?
        .to_bytes();

    let mut request = upstream
        .request(parts.method, &upstream_url)
        .headers(headers);
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }
    let response = request.send().await?;
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);

    // Pass redirects THROUGH to the browser (Location rewritten onto the proxy)
    // so its address bar stays in sync and relative links keep resolving against
    // the right path — flattening redirects here breaks Amazon's multi-step
    // pages (e.g. the OTP method choice), which 302 across paths.
    if status.is_redirection() {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let Some(location) = location else {
            return Ok(Response::builder()
                .status(status)
                .body(Full::new(Bytes::new()))
                .expect("valid response"));
        };
        // The redirect may carry the authorization code straight away.
        if let Some(code) = location
            .split_once('?')
            .and_then(|(_, query)| code_from_query(Some(query)))
        {
            let _ = state.code_tx.try_send(code);
            return Ok(html_response(StatusCode::OK, SUCCESS_PAGE));
        }
        let location = rewrite_location(&location, amazon_base, &state.proxy_prefix);
        return Ok(Response::builder()
            .status(status)
            .header(hyper::header::LOCATION, location)
            .body(Full::new(Bytes::new()))
            .expect("valid response"));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    // reqwest has already decompressed the body, so we don't forward an encoding.
    let bytes = response.bytes().await?;

    let out = if content_type.contains("text/html") {
        let html = String::from_utf8_lossy(&bytes);
        Bytes::from(rewrite_html(&html, amazon_base, &state.proxy_prefix).into_bytes())
    } else {
        bytes
    };

    Ok(Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .body(Full::new(out))
        .expect("valid response"))
}

/// Forwards the browser's request headers to Amazon, dropping ones that must be
/// recomputed or that would leak the proxy/browser transport state.
fn forward_headers(incoming: &hyper::HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in incoming {
        let drop = matches!(
            name.as_str(),
            "host"
                | "cookie"
                | "accept-encoding"
                | "content-length"
                | "connection"
                | "proxy-connection"
                | "upgrade-insecure-requests"
                | "te"
        );
        if drop {
            continue;
        }
        if let (Ok(header_name), Ok(header_value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(header_name, header_value);
        }
    }
    out
}

/// Rewrites served HTML so navigation/forms stay on the proxy while assets load
/// from Amazon directly. Ported from the reference's `rewrite_html`.
fn rewrite_html(html: &str, amazon_base: &str, proxy_prefix: &str) -> String {
    static SCHEMELESS: OnceLock<Regex> = OnceLock::new();
    static REL_ASSET: OnceLock<Regex> = OnceLock::new();
    static REL_NAV: OnceLock<Regex> = OnceLock::new();
    static UEDATA: OnceLock<Regex> = OnceLock::new();

    let schemeless = SCHEMELESS
        .get_or_init(|| Regex::new(r#"(?i)((?:src|style)=['"]?)//"#).expect("valid regex"));
    let rel_asset =
        REL_ASSET.get_or_init(|| Regex::new(r#"(?i)((?:src|style)=['"]?)/"#).expect("valid regex"));
    let rel_nav = REL_NAV.get_or_init(|| {
        Regex::new(r#"(?i)((?:href|action|data-refresh-url)=['"]?)/"#).expect("valid regex")
    });
    let uedata = UEDATA.get_or_init(|| Regex::new(r"/ap/uedata").expect("valid regex"));

    // Order matters: scheme-less (`//host`) before single-slash (`/path`).
    let out = schemeless.replace_all(html, "${1}https://");
    let out = rel_asset.replace_all(&out, format!("${{1}}{amazon_base}/"));
    let out = rel_nav.replace_all(&out, format!("${{1}}{proxy_prefix}/"));
    // Absolute Amazon nav links -> the proxy.
    let abs_nav = Regex::new(&format!(
        r#"(?i)((?:href|action)=['"]?){}"#,
        regex::escape(amazon_base)
    ))
    .expect("valid regex");
    let out = abs_nav.replace_all(&out, format!("${{1}}{proxy_prefix}"));
    let out = uedata.replace_all(&out, format!("{amazon_base}/ap/uedata"));
    out.into_owned()
}

/// Extracts `openid.oa2.authorization_code` from a query string, if present.
fn code_from_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "openid.oa2.authorization_code")
        .map(|(_, value)| value.into_owned())
        .filter(|code| !code.is_empty())
}

/// Rewrites a redirect `Location` onto the proxy: an absolute Amazon URL or a
/// root-relative path is moved under the session prefix; anything else (a
/// path-relative target, or a foreign host) is left for the browser to resolve.
fn rewrite_location(location: &str, amazon_base: &str, proxy_prefix: &str) -> String {
    if let Some(rest) = location.strip_prefix(amazon_base) {
        format!("{proxy_prefix}{rest}")
    } else if location.starts_with('/') && !location.starts_with("//") {
        format!("{proxy_prefix}{location}")
    } else {
        location.to_owned()
    }
}

/// The Amazon-styled config page (marketplace / device / name / pre-merger).
fn landing_html(state: &ProxyState, error: Option<&str>) -> String {
    let defaults = &state.defaults;
    let default_cc = defaults.country_code.as_deref().unwrap_or("de");
    let mut options = String::new();
    for entry in locale::LOCALES {
        let selected = if entry.country_code.eq_ignore_ascii_case(default_cc) {
            " selected"
        } else {
            ""
        };
        options.push_str(&format!(
            "<option value=\"{cc}\"{selected}>{cc_up} — audible.{dom}</option>",
            cc = entry.country_code,
            cc_up = entry.country_code.to_uppercase(),
            dom = entry.domain,
        ));
    }
    let iphone = if matches!(defaults.device, DeviceKind::IPhone) {
        " selected"
    } else {
        ""
    };
    let android = if matches!(defaults.device, DeviceKind::Android) {
        " selected"
    } else {
        ""
    };
    let checked = if defaults.username { " checked" } else { "" };
    let plain_checked = if defaults.plain { " checked" } else { "" };
    let name = html_escape(defaults.name.as_deref().unwrap_or(""));
    let marketplaces = html_escape(defaults.marketplaces.as_deref().unwrap_or(""));
    let default_marketplaces = html_escape(defaults.default_marketplaces.as_deref().unwrap_or(""));
    let error_block = match error {
        Some(message) => format!("<div class=\"err\">{}</div>", html_escape(message)),
        None => String::new(),
    };
    let action = format!("{}/__start", state.proxy_prefix);

    format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Audible — sign in</title>
<style>
 body{{background:#fff;font-family:"Amazon Ember",Arial,sans-serif;color:#111;margin:0;padding:24px}}
 .card{{max-width:340px;margin:0 auto;border:1px solid #ddd;border-radius:8px;padding:20px}}
 h1{{font-size:26px;font-weight:400;margin:0 0 14px}}
 .brand{{color:#f8991c;font-weight:700;letter-spacing:.5px}}
 .fld{{position:relative;display:block;font-weight:700;font-size:13px;margin:12px 0 4px}}
 select,input[type=text]{{width:100%;box-sizing:border-box;padding:7px;border:1px solid #888c8c;border-radius:3px;font-size:15px}}
 .row{{position:relative;display:flex;align-items:center;gap:8px;margin-top:14px}}
 .row input{{width:auto}}
 .cbl{{margin:0;font-weight:400;font-size:14px}}
 .hint{{color:#565959;font-size:12px;margin:4px 0 0}}
 .help{{display:inline-flex;align-items:center;justify-content:center;width:15px;height:15px;
   border:1px solid #999;border-radius:50%;font-size:10px;font-weight:700;color:#555;
   cursor:help;outline:none;vertical-align:middle;margin-left:4px}}
 .tip{{display:none;position:absolute;left:0;top:100%;width:100%;box-sizing:border-box;z-index:20;
   margin-top:4px;background:#232f3e;color:#fff;font-size:12px;font-weight:400;line-height:1.45;
   padding:8px 10px;border-radius:6px;box-shadow:0 3px 10px rgba(0,0,0,.35)}}
 .help:hover ~ .tip,.help:focus ~ .tip{{display:block}}
 button{{margin-top:18px;width:100%;padding:9px;background:linear-gradient(#f7dfa5,#f0c14b);
   border:1px solid #a88734;border-radius:8px;font-size:15px;cursor:pointer}}
 button:hover{{background:linear-gradient(#f5d78e,#eeb933)}}
 .err{{background:#fff3f3;border:1px solid #d9534f;color:#c40000;padding:8px;border-radius:4px;font-size:13px;margin-bottom:12px}}
</style></head>
<body><div class="card">
 <h1><span class="brand">audible</span> sign in</h1>
 {error_block}
 <form method="post" action="{action}">
  <label class="fld" for="marketplace">Marketplace <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">Which Audible marketplace to register this device on. One registration works for your whole account, so pick the store you mainly use.</span></label>
  <select id="marketplace" name="marketplace">{options}</select>

  <label class="fld" for="device">Device <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">The device profile to register as. iPhone is the default; Android matters for Widevine DRM and Spatial/Atmos audio.</span></label>
  <select id="device" name="device">
   <option value="iphone"{iphone}>iPhone</option>
   <option value="android"{android}>Android</option>
  </select>

  <label class="fld" for="name">Account name <span class="hint">(optional)</span> <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">A short name for this account in your local config (e.g. 'main' or 'us'). Defaults to the marketplace code.</span></label>
  <input id="name" type="text" name="name" value="{name}" placeholder="defaults to the marketplace code">

  <label class="fld" for="marketplaces">Marketplaces <span class="hint">(optional)</span> <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">Every marketplace you own audiobooks on, comma-separated (e.g. us,uk,de). Saved for later 'library sync'/'list'; it adds no extra device registrations. Empty = just the registration marketplace.</span></label>
  <input id="marketplaces" type="text" name="marketplaces" value="{marketplaces}" placeholder="e.g. us,uk,de">

  <label class="fld" for="default_marketplaces">Default marketplaces <span class="hint">(optional)</span> <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">Which of the marketplaces above the global -m defaults to when omitted on later commands (a comma-separated subset). Empty = the registration marketplace.</span></label>
  <input id="default_marketplaces" type="text" name="default_marketplaces" value="{default_marketplaces}" placeholder="subset of Marketplaces">

  <div class="row"><input id="username" type="checkbox" name="username" value="1"{checked}>
   <label for="username" class="cbl">Pre-merger Audible username login</label>
   <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">Sign in with an old Audible username instead of an Amazon email. Only for accounts predating the Amazon merger; DE, US and UK only.</span></div>

  <div class="row"><input id="plain" type="checkbox" name="plain" value="1"{plain_checked}>
   <label for="plain" class="cbl">Store the auth file unprotected (no password)</label>
   <span class="help" tabindex="0" aria-label="help">?</span><span class="tip">Store the auth file without a password (no encryption). Convenient, but not recommended — anyone with the file can use your account.</span></div>

  <button type="submit">Continue to sign-in</button>
 </form>
</div></body></html>"##
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html_response(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    html_owned(status, body.to_owned())
}

fn html_owned(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body.into_bytes())))
        .expect("valid response")
}

fn redirect_response(location: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(hyper::header::LOCATION, location)
        .body(Full::new(Bytes::new()))
        .expect("valid response")
}

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=utf-8>\
<title>Login complete</title></head>\
<body style=\"font-family:sans-serif;text-align:center;padding-top:3rem\">\
<h2>&#9989; Login complete</h2>\
<p>You can close this tab and return to the terminal.</p></body></html>";

const NOT_FOUND_PAGE: &str = "<!doctype html><html><body><h2>404</h2>\
<p>Open the sign-in URL printed in the terminal.</p></body></html>";

const BAD_GATEWAY_PAGE: &str = "<!doctype html><html><body><h2>Upstream error</h2>\
<p>Could not reach Amazon. Try again, or use <code>account login --external</code>.</p></body></html>";

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(defaults: LoginDefaults) -> ProxyState {
        let (code_tx, _code_rx) = mpsc::channel(1);
        ProxyState {
            proxy_prefix: "/tok".to_owned(),
            code_tx,
            defaults,
            session: Mutex::new(None),
        }
    }

    #[test]
    fn landing_page_lists_marketplaces_and_honours_defaults() {
        let state = test_state(LoginDefaults {
            country_code: Some("us".to_owned()),
            device: DeviceKind::Android,
            username: true,
            name: Some("my-acct".to_owned()),
            marketplaces: Some("us,uk".to_owned()),
            default_marketplaces: None,
            plain: true,
        });
        let html = landing_html(&state, None);
        assert!(html.contains(r#"action="/tok/__start""#), "{html}");
        // The marketplaces field is offered and pre-filled; every option has a help tip.
        assert!(
            html.contains(r#"name="marketplaces" value="us,uk""#),
            "{html}"
        );
        assert!(html.contains(r#"name="default_marketplaces""#), "{html}");
        assert!(html.contains(r#"class="help""#), "{html}");
        assert!(html.contains(r#"class="tip""#), "{html}");
        // Every marketplace is offered, with the default pre-selected.
        assert!(html.contains(r#"<option value="de""#), "{html}");
        assert!(html.contains(r#"<option value="us" selected"#), "{html}");
        // Device + username + name + plain defaults are reflected.
        assert!(
            html.contains(r#"<option value="android" selected"#),
            "{html}"
        );
        assert!(
            html.contains("checkbox\" name=\"username\" value=\"1\" checked"),
            "{html}"
        );
        assert!(
            html.contains("checkbox\" name=\"plain\" value=\"1\" checked"),
            "{html}"
        );
        assert!(html.contains(r#"value="my-acct""#), "{html}");
    }

    #[test]
    fn landing_page_shows_and_escapes_errors() {
        let state = test_state(LoginDefaults {
            country_code: None,
            device: DeviceKind::IPhone,
            username: false,
            name: None,
            marketplaces: None,
            default_marketplaces: None,
            plain: false,
        });
        let html = landing_html(&state, Some("bad <x> & \"y\""));
        assert!(html.contains("bad &lt;x&gt; &amp; &quot;y&quot;"), "{html}");
        // No default name -> empty value.
        assert!(html.contains(r#"name="name" value="""#), "{html}");
    }

    #[test]
    fn rewrite_keeps_nav_on_proxy_and_assets_on_amazon() {
        let html = concat!(
            r#"<a href="/ap/signin">x</a>"#,
            r#"<form action="/ap/signin"></form>"#,
            r#"<img src="/img/logo.png">"#,
            r#"<x style="/css/a.css">"#,
            r#"<script src="//m.media-amazon.com/x.js"></script>"#,
            r#"<a href="https://www.amazon.de/gp/help">help</a>"#,
        );
        let out = rewrite_html(html, "https://www.amazon.de", "/tok");
        assert!(out.contains(r#"href="/tok/ap/signin""#), "{out}");
        assert!(out.contains(r#"action="/tok/ap/signin""#), "{out}");
        assert!(
            out.contains(r#"src="https://www.amazon.de/img/logo.png""#),
            "{out}"
        );
        assert!(
            out.contains(r#"style="https://www.amazon.de/css/a.css""#),
            "{out}"
        );
        assert!(
            out.contains(r#"src="https://m.media-amazon.com/x.js""#),
            "{out}"
        );
        assert!(out.contains(r#"href="/tok/gp/help""#), "{out}");
    }

    #[test]
    fn rewrite_points_uedata_at_amazon() {
        let out = rewrite_html(
            r#"<script>var u="/ap/uedata?x=1";</script>"#,
            "https://www.amazon.de",
            "/tok",
        );
        assert!(out.contains("https://www.amazon.de/ap/uedata"), "{out}");
    }

    #[test]
    fn captures_code_from_query() {
        assert_eq!(
            code_from_query(Some("a=1&openid.oa2.authorization_code=ABC&b=2")).as_deref(),
            Some("ABC")
        );
        assert_eq!(code_from_query(Some("a=1")), None);
        assert_eq!(code_from_query(None), None);
    }

    #[test]
    fn rewrites_redirect_locations_onto_the_proxy() {
        let base = "https://www.amazon.de";
        // Absolute same-host and root-relative targets move under the prefix.
        assert_eq!(
            rewrite_location("https://www.amazon.de/ap/cvf?x=1", base, "/tok"),
            "/tok/ap/cvf?x=1"
        );
        assert_eq!(rewrite_location("/ap/mfa", base, "/tok"), "/tok/ap/mfa");
        // Scheme-relative and path-relative targets are left for the browser.
        assert_eq!(rewrite_location("//cdn/x", base, "/tok"), "//cdn/x");
        assert_eq!(rewrite_location("verify?y=2", base, "/tok"), "verify?y=2");
    }
}
