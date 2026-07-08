//! `audible api` — authenticated raw calls against the Audible API.
//!
//! The account comes from `-a/--account`; the single target marketplace
//! from the request file or the global `-m/--marketplace` (which must
//! resolve to exactly one — `api` is single-host).
//!
//! A version-less path (`/library`) is normalized to `/1.0/library`; an
//! explicit version segment (`/1.0/…`, future `/2.0/…`) is kept as-is.
//! Auth-managed headers cannot be set via `--header`; the body may be JSON
//! (validated) or, with `--content-type`, any raw payload. Responses go to
//! stdout (or a file), headers can be included (curl `-i`) or dumped
//! separately (curl `-D`).

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::parser::ValueSource;
use clap::{Args, FromArgMatches};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Method, Url};

use crate::api::client::{AuthMode, RequestBuilder};
use crate::auth::signing::{HEADER_ADP_ALG, HEADER_ADP_SIGNATURE, HEADER_ADP_TOKEN};
use crate::config::ctx::Ctx;

mod request_file;
use request_file::{RequestFile, Templater};

/// `audible api`.
pub struct ApiCommand;

#[async_trait::async_trait]
impl super::Command for ApiCommand {
    fn name(&self) -> &'static str {
        "api"
    }

    fn clap(&self) -> clap::Command {
        ApiArgs::augment_args(
            clap::Command::new(self.name())
                .about("Send an authenticated request to the Audible API"),
        )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let args = ApiArgs::from_arg_matches(matches)?;
        // Whether --auth was given explicitly (it has a default, so a request
        // file's `auth` only applies when the user did not pass --auth).
        let auth_explicit = matches.value_source("auth") == Some(ValueSource::CommandLine);
        run(ctx, args, auth_explicit).await
    }
}

/// Send an authenticated request to the Audible API
//
// Field order sets the --help layout (AUD-18): the two positionals first,
// then options grouped by `help_heading` into Request / Input & output files
// / Behavior, ordered within each group from most to least common.
#[derive(Debug, Args)]
struct ApiArgs {
    /// HTTP method (GET, POST, DELETE, …)
    #[arg(required_unless_present_any = ["interactive", "request_file"])]
    method: Option<String>,

    /// API path, e.g. "/library" (the "/1.0" version prefix is added when
    /// omitted) or "/1.0/library?num_results=5"
    #[arg(required_unless_present_any = ["interactive", "request_file"])]
    path: Option<String>,

    // --- Request ---
    /// Auth mode for this request
    #[arg(long, default_value = "auto", value_parser = parse_auth_mode, help_heading = "Request")]
    auth: AuthMode,

    /// Additional query parameter, repeatable
    #[arg(long, value_name = "KEY=VALUE", help_heading = "Request")]
    query: Vec<String>,

    /// Template variable for {{name}} placeholders, repeatable
    #[arg(long = "var", value_name = "KEY=VALUE", help_heading = "Request")]
    var: Vec<String>,

    /// Additional request header, repeatable (e.g. -H "Accept: application/json")
    #[arg(
        short = 'H',
        long = "header",
        value_name = "NAME: VALUE",
        help_heading = "Request"
    )]
    header: Vec<String>,

    /// Content type for a raw (non-JSON) body, e.g. application/xml
    #[arg(long, value_name = "TYPE", help_heading = "Request")]
    content_type: Option<String>,

    /// JSON request body (a raw body when --content-type is set)
    #[arg(long, conflicts_with = "body_file", help_heading = "Request")]
    body: Option<String>,

    /// Read the request body from a file ("-" reads stdin)
    #[arg(long, value_name = "PATH", help_heading = "Request")]
    body_file: Option<String>,

    // --- Input & output files ---
    /// Load the request from a TOML request file (method/path then optional)
    #[arg(
        long = "request-file",
        value_name = "FILE",
        help_heading = "Input & output files"
    )]
    request_file: Option<PathBuf>,

    /// Save the composed request to a TOML file and exit (does not send)
    #[arg(
        long = "save-request",
        value_name = "FILE",
        help_heading = "Input & output files"
    )]
    save_request: Option<PathBuf>,

    /// Write the response to a file instead of stdout (the global -o selects
    /// the output FORMAT, so the file flag is long-only)
    #[arg(
        long = "output-file",
        value_name = "FILE",
        help_heading = "Input & output files"
    )]
    output_file: Option<PathBuf>,

    /// Write the response status line and headers to a file (curl -D)
    #[arg(
        short = 'D',
        long = "dump-header",
        value_name = "FILE",
        help_heading = "Input & output files"
    )]
    dump_header: Option<PathBuf>,

    // --- Behavior ---
    /// Compose the request interactively with guided prompts
    #[arg(short = 'I', long = "interactive", conflicts_with_all = ["method", "path", "request_file"], help_heading = "Behavior")]
    interactive: bool,

    /// Render the request and print it without sending
    #[arg(long = "dry-run", help_heading = "Behavior")]
    dry_run: bool,

    /// Include the response status line and headers in the output (curl -i)
    #[arg(short = 'i', long = "include", help_heading = "Behavior")]
    include_headers: bool,

    /// Treat the path as a full https URL and send it verbatim, to a host
    /// other than the Audible API (e.g. to test website cookies). Hidden.
    #[arg(long = "foreign-host", hide = true)]
    foreign_host: bool,
}

fn parse_auth_mode(s: &str) -> Result<AuthMode, String> {
    s.parse()
}

/// A resolved request body together with how it should be sent.
enum ResolvedBody {
    /// A validated JSON value (sent as `application/json`).
    Json(serde_json::Value),
    /// A raw payload with an explicit content type.
    Raw {
        bytes: Vec<u8>,
        content_type: HeaderValue,
    },
}

async fn run(ctx: &Ctx, args: ApiArgs, auth_explicit: bool) -> Result<()> {
    if args.interactive {
        return run_interactive(ctx, args).await;
    }

    // Optional request file; relative `body_file` paths resolve against its dir.
    let file = match args.request_file.as_deref() {
        Some(path) => Some(request_file::load(path).await?),
        None => None,
    };
    let file_dir = args
        .request_file
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_owned);

    let vars = request_file::collect_vars(file.as_ref(), &args.var)?;

    // Merge raw (pre-substitution) components, CLI over file.
    let method_raw = args
        .method
        .clone()
        .or_else(|| file.as_ref().and_then(|f| f.method.clone()))
        .ok_or_else(|| anyhow!("a method is required (positional or in the request file)"))?;
    let path_raw = args
        .path
        .clone()
        .or_else(|| file.as_ref().and_then(|f| f.path.clone()))
        .ok_or_else(|| anyhow!("a path is required (positional or in the request file)"))?;
    let auth = if auth_explicit {
        args.auth
    } else if let Some(raw) = file.as_ref().and_then(|f| f.auth.as_deref()) {
        raw.parse::<AuthMode>()
            .map_err(|error| anyhow!("invalid auth in the request file: {error}"))?
    } else {
        args.auth
    };
    let marketplace_raw = file.as_ref().and_then(|f| f.marketplace.clone());

    let mut query_raw = file
        .as_ref()
        .map(RequestFile::query_as_strings)
        .unwrap_or_default();
    query_raw.extend(args.query.iter().cloned());

    let mut header_raw = file
        .as_ref()
        .map(RequestFile::headers_as_strings)
        .unwrap_or_default();
    header_raw.extend(args.header.iter().cloned());

    // Any CLI body input replaces the file's body entirely.
    let cli_body = args.body.is_some() || args.body_file.is_some() || args.content_type.is_some();
    let (body_inline, body_file, content_type_raw) = if cli_body {
        (
            args.body.clone(),
            args.body_file.clone(),
            args.content_type.clone(),
        )
    } else if let Some(f) = file.as_ref() {
        let body_file = f
            .body_file
            .as_deref()
            .map(|p| resolve_body_path(p, file_dir.as_deref()));
        (f.body.clone(), body_file, f.content_type.clone())
    } else {
        (None, None, None)
    };

    // --save-request: persist the merged (pre-substitution) spec, do not send.
    if let Some(out) = args.save_request.as_deref() {
        let spec = RequestFile {
            method: Some(method_raw),
            path: Some(path_raw),
            auth: (auth != AuthMode::Auto).then(|| auth_mode_str(auth).to_owned()),
            marketplace: marketplace_raw,
            content_type: content_type_raw,
            body: body_inline,
            body_file,
            query: request_file::parse_query_strings(&query_raw)?,
            headers: request_file::parse_header_strings(&header_raw)?,
            vars: vars.clone(),
        };
        request_file::save(out, &spec).await?;
        eprintln!("wrote request to {}", out.display());
        return Ok(());
    }

    // Substitute {{var}} templates across all string fields.
    let mut tmpl = Templater::new(&vars);
    let method_s = tmpl.render(&method_raw);
    let path_s = tmpl.render(&path_raw);
    let query_s: Vec<String> = query_raw.iter().map(|q| tmpl.render(q)).collect();
    let header_s: Vec<String> = header_raw.iter().map(|h| tmpl.render(h)).collect();
    let content_type_s = content_type_raw.as_deref().map(|c| tmpl.render(c));
    let marketplace_s = marketplace_raw.as_deref().map(|m| tmpl.render(m));
    let body_bytes: Option<Vec<u8>> = match (&body_inline, &body_file) {
        (Some(text), _) => Some(tmpl.render(text).into_bytes()),
        (None, Some(path)) => {
            let raw = read_body_file(path).await?;
            // Template UTF-8 bodies; pass binary payloads through untouched.
            match String::from_utf8(raw) {
                Ok(text) => Some(tmpl.render(&text).into_bytes()),
                Err(err) => Some(err.into_bytes()),
            }
        }
        (None, None) => None,
    };
    tmpl.finish()?;

    // Convert to validated parts (shared with the flag-only request path).
    let method = parse_method(&method_s)?;
    let headers = header_s
        .iter()
        .map(|h| parse_header(h))
        .collect::<Result<Vec<_>>>()?;
    let body = finalize_body(body_bytes, content_type_s)?;

    // Resolve the target: under --foreign-host the path is a verbatim https URL
    // (no API host builder, no version prefix), to reach a non-Audible host
    // (e.g. www.amazon.<tld> for cookie testing) — the deliberate, hidden
    // relaxation of the no-foreign-host rule (§9); all auth modes stay allowed.
    // Otherwise it is an API path normalized to /1.0. Query merges into either.
    let (display_path, query_pairs, foreign_url) = if args.foreign_host {
        let url = Url::parse(&path_s).with_context(|| format!("invalid URL {path_s:?}"))?;
        if url.scheme() != "https" {
            bail!(
                "--foreign-host requires an https URL (got {:?})",
                url.scheme()
            );
        }
        tracing::warn!(
            host = url.host_str().unwrap_or("?"),
            "api --foreign-host: sending to a non-Audible host"
        );
        let query_pairs = merge_query(&path_s, &query_s)?;
        (path_s.clone(), query_pairs, Some(url))
    } else {
        let normalized = normalize_api_path(&path_s);
        let query_pairs = merge_query(&normalized, &query_s)?;
        (normalized, query_pairs, None)
    };

    if args.dry_run {
        print_dry_run(
            &method_s,
            &display_path,
            auth,
            &query_pairs,
            &headers,
            body.as_ref(),
            marketplace_s.as_deref(),
        );
        return Ok(());
    }

    let client = ctx.client().await?;
    let start = match foreign_url {
        Some(mut url) => {
            // Merged query pairs are appended by the builder; drop the URL's own.
            url.set_query(None);
            client.request_absolute(method, url)
        }
        None => client.request(method, path_without_query(&display_path)),
    };
    let mut request = apply_parts(start, auth, query_pairs, headers, body);
    // Marketplace: the request file's value wins; otherwise the global
    // `-m` (which must resolve to exactly one — `api` is single-host).
    // Foreign-host requests target a verbatim URL, so no marketplace.
    let marketplace = match marketplace_s {
        Some(market) => Some(market),
        None if !args.foreign_host => Some(ctx.marketplace_single()?),
        None => None,
    };
    if let Some(market) = marketplace {
        request = request.country_code(market);
    }
    let response = request.send().await?;
    emit_response(
        response,
        args.include_headers,
        args.output_file,
        args.dump_header,
    )
    .await
}

/// Applies already-validated parts onto a started request builder. Shared by
/// the flag, interactive and foreign-host paths so all behave identically.
fn apply_parts(
    request: RequestBuilder<'_>,
    auth: AuthMode,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Option<ResolvedBody>,
) -> RequestBuilder<'_> {
    let mut request = request.auth(auth);
    for (key, value) in query {
        request = request.query(key, value);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    match body {
        Some(ResolvedBody::Json(json)) => request = request.body(json),
        Some(ResolvedBody::Raw {
            bytes,
            content_type,
        }) => request = request.raw_body(bytes, content_type),
        None => {}
    }
    request
}

fn parse_method(raw: &str) -> Result<Method> {
    Method::from_bytes(raw.to_ascii_uppercase().as_bytes())
        .map_err(|_| anyhow!("invalid HTTP method {raw:?}"))
}

fn auth_mode_str(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::Auto => "auto",
        AuthMode::Signing => "signing",
        AuthMode::Token => "token",
        AuthMode::Cookies => "cookies",
    }
}

/// Resolves a request file's `body_file` against the file's own directory so a
/// request file and its payload travel together. Absolute paths and `-`
/// (stdin) are kept as-is.
fn resolve_body_path(body_file: &str, base: Option<&Path>) -> String {
    if body_file == "-" || Path::new(body_file).is_absolute() {
        return body_file.to_owned();
    }
    match base {
        Some(dir) => dir.join(body_file).to_string_lossy().into_owned(),
        None => body_file.to_owned(),
    }
}

/// Prints the fully resolved request (`--dry-run`) to stdout instead of sending.
fn print_dry_run(
    method: &str,
    normalized_path: &str,
    auth: AuthMode,
    query: &[(String, String)],
    headers: &[(String, String)],
    body: Option<&ResolvedBody>,
    marketplace: Option<&str>,
) {
    let path = path_without_query(normalized_path);
    let full = if query.is_empty() {
        path.to_owned()
    } else {
        let pairs = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{path}?{pairs}")
    };
    println!("{} {full}", method.to_uppercase());
    println!("auth: {}", auth_mode_str(auth));
    if let Some(market) = marketplace {
        println!("marketplace: {market}");
    }
    for (name, value) in headers {
        println!("header: {name}: {value}");
    }
    match body {
        Some(ResolvedBody::Json(value)) => println!(
            "body (application/json):\n{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        ),
        Some(ResolvedBody::Raw {
            bytes,
            content_type,
        }) => match std::str::from_utf8(bytes) {
            Ok(text) => println!("body ({}):\n{text}", content_type.to_str().unwrap_or("?")),
            Err(_) => println!(
                "body ({}): <{} bytes>",
                content_type.to_str().unwrap_or("?"),
                bytes.len()
            ),
        },
        None => {}
    }
}

/// Builds a [`RequestFile`] from interactively composed parts (for saving).
fn request_file_from_parts(
    method: &str,
    normalized_path: &str,
    auth: AuthMode,
    query: &[(String, String)],
    headers: &[(String, String)],
    body: Option<&ResolvedBody>,
) -> Result<RequestFile> {
    let (content_type, body_text) = match body {
        Some(ResolvedBody::Json(value)) => (None, Some(serde_json::to_string(value)?)),
        Some(ResolvedBody::Raw {
            bytes,
            content_type,
        }) => {
            let text = String::from_utf8(bytes.clone())
                .map_err(|_| anyhow!("cannot save a binary body to a request file"))?;
            (
                Some(content_type.to_str().unwrap_or_default().to_owned()),
                Some(text),
            )
        }
        None => (None, None),
    };
    Ok(RequestFile {
        method: Some(method.to_uppercase()),
        path: Some(path_without_query(normalized_path).to_owned()),
        auth: (auth != AuthMode::Auto).then(|| auth_mode_str(auth).to_owned()),
        marketplace: None,
        content_type,
        body: body_text,
        body_file: None,
        query: query.iter().cloned().collect(),
        headers: headers.iter().cloned().collect(),
        vars: BTreeMap::new(),
    })
}

/// Normalizes an API path: a version-less path gets the `/1.0` prefix; an
/// explicit version segment (`1.0`, `2.0`, …) is left untouched. The query
/// string is preserved.
/// `pub(crate)`: the plugin broker (AUD-69) normalizes `api.request`
/// paths with exactly the CLI's rule.
pub(crate) fn normalize_api_path(path: &str) -> String {
    let (path_part, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let trimmed = path_part.trim_start_matches('/');
    let first_segment = trimmed.split('/').next().unwrap_or("");
    // A version looks like "1.0": only digits and dots, and at least one dot
    // (so a resource named "123" is not mistaken for a version).
    let is_version = first_segment.contains('.')
        && first_segment
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.');
    let normalized = if is_version {
        format!("/{trimmed}")
    } else {
        format!("/1.0/{trimmed}")
    };
    match query {
        Some(q) => format!("{normalized}?{q}"),
        None => normalized,
    }
}

fn path_without_query(path: &str) -> &str {
    path.split_once('?').map_or(path, |(p, _)| p)
}

/// True for headers owned by the auth layer (`--auth`); these must not be set
/// via `--header` or a user could break or leak the account's credentials.
fn is_auth_managed_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        HEADER_ADP_TOKEN
            | HEADER_ADP_ALG
            | HEADER_ADP_SIGNATURE
            | "x-amz-access-token"
            | "cookie"
            | "authorization"
    )
}

/// Parses a `Name: value` header, rejecting auth-managed names and
/// `content-type` (which is owned by the body / `--content-type`).
fn parse_header(raw: &str) -> Result<(String, String)> {
    let (name, value) = raw
        .split_once(':')
        .with_context(|| format!("header {raw:?} is not \"Name: value\""))?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        bail!("header {raw:?} has an empty name");
    }
    if is_auth_managed_header(name) {
        bail!(
            "header {name:?} is managed by --auth and cannot be set with --header; \
             use --auth signing|token|cookies instead"
        );
    }
    if name.eq_ignore_ascii_case("content-type") {
        bail!("set the body content type with --content-type, not --header");
    }
    HeaderName::from_bytes(name.as_bytes())
        .with_context(|| format!("invalid header name {name:?}"))?;
    HeaderValue::from_str(value).with_context(|| format!("invalid value for header {name:?}"))?;
    Ok((name.to_owned(), value.to_owned()))
}

/// Merges the path's query string and `--query` parameters into a single,
/// order-preserving list. Identical repeats collapse; a key appearing twice
/// with different values is an error.
fn merge_query(path: &str, extra: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some((_, query)) = path.split_once('?') {
        for segment in query.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
            pairs.push((key.to_owned(), value.to_owned()));
        }
    }
    for raw in extra {
        let (key, value) = raw
            .split_once('=')
            .with_context(|| format!("query parameter {raw:?} is not KEY=VALUE"))?;
        pairs.push((key.to_owned(), value.to_owned()));
    }

    // Keep every pair in order so repeated keys are sent as repeated params
    // (the library endpoint requires `asins=A&asins=B`; CSV is rejected there).
    // Only an exact key+value duplicate is dropped.
    let mut result: Vec<(String, String)> = Vec::new();
    for pair in pairs {
        if !result.contains(&pair) {
            result.push(pair);
        }
    }
    Ok(result)
}

/// Final body decision: bytes become JSON or a raw payload (per content type);
/// no bytes means no body, unless a content type was set (then it is an error).
fn finalize_body(
    bytes: Option<Vec<u8>>,
    content_type: Option<String>,
) -> Result<Option<ResolvedBody>> {
    match bytes {
        Some(bytes) => Ok(Some(classify_body(bytes, content_type)?)),
        None => {
            if content_type.is_some() {
                bail!("a content type was set but there is no body");
            }
            Ok(None)
        }
    }
}

/// Turns raw body bytes into a [`ResolvedBody`]: a content type means a raw
/// payload sent as-is; without one the body must be valid JSON.
fn classify_body(bytes: Vec<u8>, content_type: Option<String>) -> Result<ResolvedBody> {
    match content_type {
        Some(ct) => {
            let content_type = HeaderValue::from_str(&ct)
                .with_context(|| format!("invalid content type {ct:?}"))?;
            Ok(ResolvedBody::Raw {
                bytes,
                content_type,
            })
        }
        None => {
            let json: serde_json::Value = serde_json::from_slice(&bytes)
                .context("body is not valid JSON; set --content-type for a raw body")?;
            Ok(ResolvedBody::Json(json))
        }
    }
}

async fn read_body_file(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        // stdin may be a pipe; read it off the async executor.
        tokio::task::spawn_blocking(|| {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            Ok::<_, std::io::Error>(buf)
        })
        .await
        .expect("stdin read task must not panic")
        .context("could not read body from stdin")
    } else {
        tokio::fs::read(path)
            .await
            .with_context(|| format!("could not read body file {path:?}"))
    }
}

/// Writes the response. The body goes to stdout (or the `--output-file`); with
/// `-i` the status line and headers are prepended to that stream; `-D` writes
/// the head to its own file. Pretty-prints JSON only for an interactive
/// stdout; pipes and files get the raw bytes (binary-safe).
async fn emit_response(
    response: reqwest::Response,
    include_headers: bool,
    output: Option<PathBuf>,
    dump_header: Option<PathBuf>,
) -> Result<()> {
    let status = response.status();
    let mut head = format!("{:?} {status}\n", response.version());
    for (name, value) in response.headers() {
        head.push_str(name.as_str());
        head.push_str(": ");
        head.push_str(value.to_str().unwrap_or("<binary>"));
        head.push('\n');
    }

    eprintln!("HTTP {status}");

    if let Some(path) = &dump_header {
        tokio::fs::write(path, &head)
            .await
            .with_context(|| format!("could not write headers to {}", path.display()))?;
    }

    let body = response
        .bytes()
        .await
        .context("could not read response body")?;

    let to_tty = output.is_none() && std::io::stdout().is_terminal();
    let body_out: Vec<u8> = if to_tty {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(value) => serde_json::to_vec_pretty(&value)?,
            Err(_) => body.to_vec(),
        }
    } else {
        body.to_vec()
    };
    let binary_to_tty = to_tty && std::str::from_utf8(&body_out).is_err();

    let mut payload = Vec::new();
    if include_headers {
        payload.extend_from_slice(head.as_bytes());
        payload.push(b'\n');
    }
    if binary_to_tty {
        eprintln!(
            "(binary response, {} bytes; use --output-file FILE to save it)",
            body.len()
        );
    } else {
        payload.extend_from_slice(&body_out);
    }

    match &output {
        Some(path) => {
            tokio::fs::write(path, &payload)
                .await
                .with_context(|| format!("could not write response to {}", path.display()))?;
            eprintln!("wrote {} bytes to {}", payload.len(), path.display());
        }
        None => {
            if to_tty && !binary_to_tty {
                payload.push(b'\n');
            }
            let mut out = std::io::stdout().lock();
            out.write_all(&payload)
                .context("could not write response to stdout")?;
            out.flush().ok();
        }
    }

    if !status.is_success() {
        bail!("request failed with HTTP status {status}");
    }
    Ok(())
}

// --- Interactive composer ---------------------------------------------------

async fn run_interactive(ctx: &Ctx, args: ApiArgs) -> Result<()> {
    let term = console::Term::stderr();
    if !term.is_term() {
        bail!("--interactive needs a terminal");
    }
    let theme = dialoguer::theme::ColorfulTheme::default();
    eprintln!("Compose an API request — Ctrl-C to abort\n");

    let methods = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "custom…"];
    let method_idx = dialoguer::Select::with_theme(&theme)
        .with_prompt("Method")
        .items(methods)
        .default(0)
        .interact_on(&term)?;
    let method_str = if methods[method_idx] == "custom…" {
        dialoguer::Input::<String>::with_theme(&theme)
            .with_prompt("Custom method")
            .interact_on(&term)?
    } else {
        methods[method_idx].to_owned()
    };
    let method = parse_method(&method_str)?;

    let raw_path: String = dialoguer::Input::with_theme(&theme)
        .with_prompt("Path (e.g. /library)")
        .validate_with(|input: &String| -> Result<(), &str> {
            if input.contains("://") {
                Err("enter an API path, not a full URL")
            } else {
                Ok(())
            }
        })
        .interact_on(&term)?;
    let normalized = normalize_api_path(&raw_path);

    let auth_modes = ["auto", "signing", "token", "cookies"];
    let auth_idx = dialoguer::Select::with_theme(&theme)
        .with_prompt("Auth mode")
        .items(auth_modes)
        .default(0)
        .interact_on(&term)?;
    let auth: AuthMode = auth_modes[auth_idx].parse().expect("known auth mode");

    let mut query_extra: Vec<String> = Vec::new();
    loop {
        let entry: String = dialoguer::Input::with_theme(&theme)
            .with_prompt("Add query key=value (empty to finish)")
            .allow_empty(true)
            .interact_on(&term)?;
        if entry.trim().is_empty() {
            break;
        }
        let mut trial = query_extra.clone();
        trial.push(entry.clone());
        match merge_query(&normalized, &trial) {
            Ok(_) => query_extra.push(entry),
            Err(error) => eprintln!("  {error}"),
        }
    }
    let query_pairs = merge_query(&normalized, &query_extra)?;

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let entry: String = dialoguer::Input::with_theme(&theme)
            .with_prompt("Add header Name: value (empty to finish)")
            .allow_empty(true)
            .interact_on(&term)?;
        if entry.trim().is_empty() {
            break;
        }
        match parse_header(&entry) {
            Ok(header) => headers.push(header),
            Err(error) => eprintln!("  {error}"),
        }
    }

    let mut body: Option<ResolvedBody> = None;
    if dialoguer::Confirm::with_theme(&theme)
        .with_prompt("Add a request body?")
        .default(false)
        .interact_on(&term)?
    {
        let sources = ["edit inline ($EDITOR)", "from file"];
        let source_idx = dialoguer::Select::with_theme(&theme)
            .with_prompt("Body source")
            .items(sources)
            .default(0)
            .interact_on(&term)?;
        let bytes = if source_idx == 0 {
            dialoguer::Editor::new()
                .edit("")?
                .unwrap_or_default()
                .into_bytes()
        } else {
            let path: String = dialoguer::Input::with_theme(&theme)
                .with_prompt("Body file path (- for stdin)")
                .interact_on(&term)?;
            read_body_file(&path).await?
        };
        let content_type: String = dialoguer::Input::with_theme(&theme)
            .with_prompt("Content-Type (empty = JSON)")
            .allow_empty(true)
            .interact_on(&term)?;
        let content_type = (!content_type.trim().is_empty()).then_some(content_type);
        body = Some(classify_body(bytes, content_type)?);
    }

    eprintln!("\nRequest:");
    eprintln!("  {} {}", method_str.to_uppercase(), normalized);
    eprintln!("  auth: {}", auth_modes[auth_idx]);
    for (key, value) in &query_pairs {
        eprintln!("  query: {key}={value}");
    }
    for (name, value) in &headers {
        eprintln!("  header: {name}: {value}");
    }
    match &body {
        Some(ResolvedBody::Json(_)) => eprintln!("  body: JSON"),
        Some(ResolvedBody::Raw {
            bytes,
            content_type,
        }) => eprintln!(
            "  body: {} ({} bytes)",
            content_type.to_str().unwrap_or("?"),
            bytes.len()
        ),
        None => {}
    }
    if args.dry_run {
        print_dry_run(
            &method_str,
            &normalized,
            auth,
            &query_pairs,
            &headers,
            body.as_ref(),
            None,
        );
        return Ok(());
    }
    if let Some(out) = args.save_request.as_deref() {
        let spec = request_file_from_parts(
            &method_str,
            &normalized,
            auth,
            &query_pairs,
            &headers,
            body.as_ref(),
        )?;
        request_file::save(out, &spec).await?;
        eprintln!("wrote request to {}", out.display());
        return Ok(());
    }
    if dialoguer::Confirm::with_theme(&theme)
        .with_prompt("Save this request to a file?")
        .default(false)
        .interact_on(&term)?
    {
        let out: String = dialoguer::Input::with_theme(&theme)
            .with_prompt("File path")
            .interact_on(&term)?;
        let spec = request_file_from_parts(
            &method_str,
            &normalized,
            auth,
            &query_pairs,
            &headers,
            body.as_ref(),
        )?;
        request_file::save(Path::new(&out), &spec).await?;
        eprintln!("saved request to {out}");
    }

    if !dialoguer::Confirm::with_theme(&theme)
        .with_prompt("Send this request?")
        .default(true)
        .interact_on(&term)?
    {
        eprintln!("aborted");
        return Ok(());
    }
    eprintln!();

    let client = ctx.client().await?;
    let marketplace = ctx.marketplace_single()?;
    let request = apply_parts(
        client
            .request(method, path_without_query(&normalized))
            .country_code(marketplace),
        auth,
        query_pairs,
        headers,
        body,
    );
    let response = request.send().await?;
    emit_response(
        response,
        args.include_headers,
        args.output_file,
        args.dump_header,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_default_version() {
        assert_eq!(normalize_api_path("/library"), "/1.0/library");
        assert_eq!(normalize_api_path("/wishlist"), "/1.0/wishlist");
        // Missing leading slash is tolerated.
        assert_eq!(normalize_api_path("library"), "/1.0/library");
    }

    #[test]
    fn normalize_keeps_explicit_version() {
        assert_eq!(normalize_api_path("/1.0/library"), "/1.0/library");
        assert_eq!(normalize_api_path("/2.0/foo"), "/2.0/foo");
    }

    #[test]
    fn normalize_preserves_query() {
        assert_eq!(
            normalize_api_path("/library?num_results=5"),
            "/1.0/library?num_results=5"
        );
        assert_eq!(
            normalize_api_path("/1.0/library?a=b&c=d"),
            "/1.0/library?a=b&c=d"
        );
    }

    #[test]
    fn header_accepts_normal() {
        let (name, value) = parse_header("Accept: application/json").unwrap();
        assert_eq!(name, "Accept");
        assert_eq!(value, "application/json");
    }

    #[test]
    fn header_rejects_auth_managed() {
        assert!(parse_header("x-amz-access-token: secret").is_err());
        assert!(parse_header("Cookie: a=b").is_err());
        assert!(parse_header("x-adp-token: t").is_err());
        assert!(parse_header("Authorization: Bearer x").is_err());
    }

    #[test]
    fn header_rejects_content_type() {
        assert!(parse_header("Content-Type: application/xml").is_err());
    }

    #[test]
    fn header_rejects_malformed() {
        assert!(parse_header("no-colon").is_err());
        assert!(parse_header(": empty-name").is_err());
    }

    #[test]
    fn query_merges_path_and_extra() {
        let merged = merge_query("/1.0/library?a=1", &["b=2".to_owned()]).unwrap();
        assert_eq!(
            merged,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn query_collapses_identical_repeat() {
        let merged = merge_query("/x?a=1", &["a=1".to_owned()]).unwrap();
        assert_eq!(merged, vec![("a".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn query_keeps_repeated_keys() {
        // Repeated keys with different values are preserved in order and sent
        // as repeated params (e.g. `asins=A&asins=B`), not collapsed or rejected.
        let repeated = vec![
            ("a".to_owned(), "1".to_owned()),
            ("a".to_owned(), "2".to_owned()),
        ];
        assert_eq!(
            merge_query("/x?a=1", &["a=2".to_owned()]).unwrap(),
            repeated
        );
        assert_eq!(
            merge_query("/x", &["a=1".to_owned(), "a=2".to_owned()]).unwrap(),
            repeated
        );
        // The real motivating case: the library endpoint's repeated asins.
        assert_eq!(
            merge_query("/1.0/library?asins=A&asins=B", &[]).unwrap(),
            vec![
                ("asins".to_owned(), "A".to_owned()),
                ("asins".to_owned(), "B".to_owned())
            ]
        );
    }

    #[test]
    fn body_inline_json_ok() {
        let resolved = finalize_body(Some(b"{\"x\":1}".to_vec()), None).unwrap();
        assert!(matches!(resolved, Some(ResolvedBody::Json(_))));
    }

    #[test]
    fn body_invalid_json_without_content_type_errs() {
        assert!(finalize_body(Some(b"<xml/>".to_vec()), None).is_err());
    }

    #[test]
    fn body_raw_with_content_type_ok() {
        let resolved =
            finalize_body(Some(b"<xml/>".to_vec()), Some("application/xml".to_owned())).unwrap();
        assert!(matches!(resolved, Some(ResolvedBody::Raw { .. })));
    }

    #[test]
    fn body_content_type_without_body_errs() {
        assert!(finalize_body(None, Some("application/xml".to_owned())).is_err());
    }

    #[test]
    fn body_absent_is_none() {
        assert!(finalize_body(None, None).unwrap().is_none());
    }

    #[test]
    fn body_path_resolves_relative_to_file_dir() {
        let base = Path::new("/cfg/reqs");
        assert_eq!(
            resolve_body_path("payload.xml", Some(base)),
            "/cfg/reqs/payload.xml"
        );
        assert_eq!(resolve_body_path("/abs/p.xml", Some(base)), "/abs/p.xml");
        assert_eq!(resolve_body_path("-", Some(base)), "-");
        assert_eq!(resolve_body_path("rel.xml", None), "rel.xml");
    }

    #[test]
    fn auth_mode_str_round_trips() {
        for label in ["auto", "signing", "token", "cookies"] {
            let mode: AuthMode = label.parse().unwrap();
            assert_eq!(auth_mode_str(mode), label);
        }
    }
}
