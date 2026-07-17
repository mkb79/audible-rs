//! Ephemeral plugin broker (AUD-69): the short-lived half of the broker
//! component (decision #7). Binds a private unix socket for the duration
//! of one plugin invocation and serves the shared `/v1` router
//! ([`crate::session::rpc`]) with a fixed identity — the invoking `Ctx`,
//! already unlocked because the user just ran the command. The manifest
//! scopes are the single token's grant. Socket + dir are removed when the
//! plugin exits; the token is never logged.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use crate::config::ctx::Ctx;
use crate::session::rpc::{self, Auth, Backend};

/// A running broker: socket + token handed to exactly one plugin.
pub struct Broker {
    /// Socket path, exported as `AUDIBLE_SOCKET`.
    pub socket_path: PathBuf,
    /// Per-invocation bearer token, exported as `AUDIBLE_BROKER_TOKEN`.
    pub token: String,
    /// Private 0700 directory holding the socket; removed on shutdown.
    dir: PathBuf,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Broker {
    /// Binds a fresh socket in a private random-named 0700 directory and
    /// starts serving with the given manifest scopes. `builtins` names the
    /// commands `/v1/invoke` may self-exec — passed down from the
    /// composition root (main.rs owns the registry; this layer must not
    /// reach up for it).
    pub async fn start(ctx: Arc<Ctx>, scopes: Vec<String>, builtins: Vec<String>) -> Result<Self> {
        let invoke_exe = std::env::current_exe().context("could not resolve the own binary")?;
        Self::start_with_exe(ctx, scopes, builtins, invoke_exe).await
    }

    /// [`Self::start`] with the `/v1/invoke` binary explicit — tests use
    /// a stub, because `current_exe` is the test runner there.
    async fn start_with_exe(
        ctx: Arc<Ctx>,
        scopes: Vec<String>,
        builtins: Vec<String>,
        invoke_exe: PathBuf,
    ) -> Result<Self> {
        let dir = crate::session::runtime_dir(&ctx).join(format!(
            "audible-broker-{}",
            hex::encode(rand::random::<[u8; 8]>())
        ));
        crate::session::create_private_dir(&dir)?;
        let socket_path = dir.join("broker.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path)
            .with_context(|| format!("could not bind {}", socket_path.display()))?;
        let token = hex::encode(rand::random::<[u8; 32]>());

        let backend: Arc<dyn Backend> = Arc::new(Ephemeral {
            ctx,
            token: token.clone(),
            scopes,
            invoke_exe,
            builtins,
        });
        let accept_task = tokio::spawn(rpc::serve_unix(listener, backend));

        Ok(Self {
            socket_path,
            token,
            dir,
            accept_task,
        })
    }

    /// Stops serving and removes socket and directory.
    pub async fn shutdown(self) {
        self.accept_task.abort();
        let _ = self.accept_task.await;
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Ephemeral identity: one token, one already-unlocked context.
struct Ephemeral {
    ctx: Arc<Ctx>,
    token: String,
    scopes: Vec<String>,
    invoke_exe: PathBuf,
    /// Built-in command names for `/v1/invoke`, from the composition root.
    builtins: Vec<String>,
}

#[async_trait::async_trait]
impl Backend for Ephemeral {
    fn authenticate(&self, token: &str) -> Option<Auth> {
        (token == self.token).then(|| Auth {
            scopes: self.scopes.clone(),
            account: None,
            caller: "plugin".to_owned(),
        })
    }

    async fn session(&self, _account: Option<&str>) -> Result<Arc<Ctx>> {
        // A plugin runs under the invoking account; the `account`
        // selector is an agent-mode feature (session map).
        Ok(Arc::clone(&self.ctx))
    }

    fn invoke_exe(&self) -> PathBuf {
        self.invoke_exe.clone()
    }

    fn builtin_names(&self) -> &[String] {
        &self.builtins
    }

    // Plugins use the plugin allowlist (AUD-124); the invoking process is
    // short-lived, so the cached config is current enough.
    fn allowed_hosts(&self) -> Vec<crate::config::schema::AllowedHost> {
        self.ctx.config().plugins.allowed_hosts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    async fn http(socket: &std::path::Path, request: &str) -> String {
        let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn request(method: &str, path: &str, token: Option<&str>) -> String {
        let auth = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        format!("{method} {path} HTTP/1.1\r\nHost: broker\r\n{auth}Connection: close\r\n\r\n")
    }

    fn post(path: &str, token: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: broker\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn test_broker(scopes: &[&str]) -> (Broker, tempfile::TempDir) {
        test_broker_with(scopes, None).await
    }

    async fn test_broker_with(
        scopes: &[&str],
        invoke_exe: Option<PathBuf>,
    ) -> (Broker, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "version = 1\ndefault_account = \"smoke\"\n\n\
                 [accounts.smoke]\nauth_file = \"smoke.auth\"\n\
                 marketplaces = [\"de\", \"us\"]\ndefault_marketplaces = [\"de\"]\n\n\
                 [session]\nsocket_dir = {:?}\n",
                tmp.path().join("run")
            ),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        let ctx = Arc::new(Ctx::with_dir(config_dir, Default::default()).unwrap());
        let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        let broker = match invoke_exe {
            Some(exe) => Broker::start_with_exe(ctx, scopes, vec!["library".to_owned()], exe)
                .await
                .unwrap(),
            None => Broker::start(ctx, scopes, vec!["library".to_owned()])
                .await
                .unwrap(),
        };
        (broker, tmp)
    }

    #[tokio::test]
    async fn auth_scopes_and_routes() {
        let (broker, _tmp) = test_broker(&["config"]).await;
        let socket = broker.socket_path.clone();

        let response = http(&socket, &request("GET", "/v1/accounts", None)).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        let response = http(&socket, &request("GET", "/v1/accounts", Some("nope"))).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");

        let response = http(
            &socket,
            &request("GET", "/v1/accounts", Some(&broker.token)),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"smoke\""), "{response}");
        assert!(!response.contains("auth_file"), "{response}");
        // Selector discovery (AUD-126): the effective default bundle per
        // account and the settings list, with the implicit `default`
        // present even though the fixture configures no bundle at all.
        assert!(
            response.contains("\"default_settings\":\"default\""),
            "{response}"
        );
        assert!(
            response.contains("\"settings\":[\"default\"]"),
            "{response}"
        );

        let response = http(
            &socket,
            &request("GET", "/v1/config/resolved", Some(&broker.token)),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"download_dir\""), "{response}");

        // `api` scope not granted → 403.
        let response = http(
            &socket,
            &request("POST", "/v1/api/request", Some(&broker.token)),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");

        let response = http(&socket, &request("GET", "/v1/nope", Some(&broker.token))).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let dir = broker.dir.clone();
        broker.shutdown().await;
        assert!(!socket.exists());
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn invoke_endpoint_runs_builtins_via_self_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("fake-audible");
        std::fs::write(&stub, "#!/bin/sh\necho \"ARGS:$@\"\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (broker, _tmp) = test_broker_with(&["invoke"], Some(stub)).await;

        let response = http(
            &broker.socket_path,
            &post(
                "/v1/invoke",
                &broker.token,
                r#"{"argv": ["library", "list", "--limit", "5"]}"#,
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.contains("ARGS:-a smoke -s default -o json library list --limit 5"),
            "{response}"
        );

        let response = http(
            &broker.socket_path,
            &post("/v1/invoke", &broker.token, r#"{"argv": ["demo"]}"#),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("not a built-in"), "{response}");

        let response = http(
            &broker.socket_path,
            &post("/v1/invoke", &broker.token, r#"{}"#),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");

        broker.shutdown().await;
    }

    /// The Python SDK (AUD-70) speaks the protocol end-to-end.
    #[tokio::test]
    async fn python_sdk_speaks_the_broker_protocol() {
        if super::super::python3().is_none() {
            eprintln!("skipping: no python3 on PATH");
            return;
        }
        let (broker, _tmp) = test_broker(&["config"]).await;
        let script = r#"
from audible_plugin_sdk import Broker, BrokerError
broker = Broker()
accounts = broker.accounts()
assert accounts[0]["name"] == "smoke", accounts
resolved = broker.config_resolved()
assert "download_dir" in resolved, resolved
try:
    broker.api_request("/1.0/library")
    raise SystemExit("expected a 403")
except BrokerError as error:
    assert error.status == 403, error.status
print("ok")
"#;
        let output = tokio::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .env("AUDIBLE_SOCKET", &broker.socket_path)
            .env("AUDIBLE_BROKER_TOKEN", &broker.token)
            .env(
                "PYTHONPATH",
                concat!(env!("CARGO_MANIFEST_DIR"), "/sdk/python"),
            )
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
        broker.shutdown().await;
    }
}
