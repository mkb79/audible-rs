//! CLI side of the agent (AUD-116): a tiny HTTP-over-unix-socket client
//! that talks to a running agent's `/v1` surface, reading the bootstrap
//! admin token from the 0600 `agent.token` file. Used by
//! `account unlock|lock` and `agent status`.

use anyhow::{Context as _, Result, bail};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;

use crate::config::ctx::Ctx;
use crate::session::agent;

/// One admin request to the running agent. Fails with a clear hint when
/// no agent is up. Returns `(status, json_body)`.
pub async fn admin_request(
    ctx: &Ctx,
    method: hyper::Method,
    path: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value)> {
    if !agent::is_running(ctx) {
        bail!("no agent is running — start it with `audible agent start`");
    }
    let token = std::fs::read_to_string(agent::token_path(ctx))
        .context("could not read the agent token (is the agent running?)")?
        .trim()
        .to_owned();
    let socket = agent::socket_path(ctx);

    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .with_context(|| format!("could not connect to {}", socket.display()))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .context("agent handshake failed")?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "agent")
        .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload)))
        .expect("static request parts are valid");
    let response = sender
        .send_request(request)
        .await
        .context("the agent request failed")?;
    let status = response.status();
    let bytes = response.into_body().collect().await?.to_bytes();
    let value: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Ok((status, value))
}
