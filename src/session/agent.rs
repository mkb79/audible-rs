//! Long-lived session agent (M5, AUD-115): the persistent half of the
//! broker component (decision #7). Binds a stable unix socket and serves
//! the shared `/v1` router ([`super::rpc`]) with a **session map** —
//! `account → unlocked Ctx`, filled lazily on first use via the account's
//! `password_source` (so keyctl/command accounts come up headless;
//! `prompt` accounts stay locked until `account unlock`, AUD-116). Idle
//! sessions are evicted after `[session].idle_timeout`. Auth is a
//! per-start bootstrap admin token (a 0600 file the local CLI reads);
//! scoped app tokens replace it in AUD-117.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use tokio::sync::RwLock;

use crate::config::ctx::{Ctx, Selectors};
use crate::session::rpc::{self, Auth, Backend};
use crate::session::tokens::TokenStore;

/// Directory holding the agent's socket, PID and token files.
fn agent_dir(ctx: &Ctx) -> PathBuf {
    super::runtime_dir(ctx).join("audible")
}

/// Stable socket path the CLI and external callers connect to.
pub fn socket_path(ctx: &Ctx) -> PathBuf {
    agent_dir(ctx).join("agent.sock")
}

/// PID file of the running daemon.
pub fn pid_path(ctx: &Ctx) -> PathBuf {
    agent_dir(ctx).join("agent.pid")
}

/// 0600 file holding the bootstrap admin token (local CLI reads it).
pub fn token_path(ctx: &Ctx) -> PathBuf {
    agent_dir(ctx).join("agent.token")
}

/// One unlocked account session and its last-use instant (idle eviction).
struct Session {
    ctx: Arc<Ctx>,
    last_used: Instant,
}

/// State of an async job (AUD-119).
enum Job {
    Running,
    Done {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

/// One job plus its owning account (AUD-125): status/listing is scoped —
/// a caller bound or selected to another account does not see it.
struct JobEntry {
    account: String,
    state: Job,
}

/// Shared job table (async self-exec results kept in memory).
type Jobs = Arc<RwLock<HashMap<String, JobEntry>>>;

/// The agent's shared state: the session map plus the config template
/// every per-account `Ctx` is built from.
struct AgentBackend {
    config_dir: PathBuf,
    admin_token: String,
    invoke_exe: PathBuf,
    idle_timeout: Duration,
    sessions: RwLock<HashMap<String, Session>>,
    /// Persisted app tokens (AUD-117); reloaded from disk on change.
    tokens: TokenStore,
    /// Append-only audit log of served requests (AUD-118).
    audit: crate::session::audit::AuditLog,
    /// Async jobs (AUD-119); in-memory, cleared on restart.
    jobs: Jobs,
}

#[async_trait::async_trait]
impl Backend for AgentBackend {
    // The agent serves several accounts — callers may select per request
    // (unlike a plugin's ephemeral broker, AUD-123).
    fn allows_selectors(&self) -> bool {
        true
    }

    fn authenticate(&self, token: &str) -> Option<Auth> {
        // The bootstrap admin token grants every scope, unbound (local
        // CLI convenience). App tokens carry their own scopes + binding.
        if token == self.admin_token {
            return Some(Auth {
                scopes: crate::plugins::VALID_SCOPES
                    .iter()
                    .map(|scope| (*scope).to_owned())
                    .collect(),
                account: None,
                caller: "admin".to_owned(),
            });
        }
        self.tokens
            .lookup(token)
            .map(|(scopes, account, label)| Auth {
                scopes,
                account,
                caller: label,
            })
    }

    async fn session(&self, account: Option<&str>) -> Result<Arc<Ctx>> {
        // A per-account Ctx (its own client cell = its own unlocked
        // session); the account name is resolved through the same rules
        // as the CLI so `None` picks default_account / the sole account.
        let probe = self.build_ctx(account)?;
        let name = probe.account_name()?;

        if let Some(session) = self.sessions.read().await.get(&name) {
            return Ok(Arc::clone(&session.ctx));
        }
        let mut sessions = self.sessions.write().await;
        // Re-check: another task may have inserted it meanwhile.
        if let Some(session) = sessions.get_mut(&name) {
            session.last_used = Instant::now();
            return Ok(Arc::clone(&session.ctx));
        }
        let ctx = Arc::new(self.build_ctx(Some(&name))?);
        sessions.insert(
            name,
            Session {
                ctx: Arc::clone(&ctx),
                last_used: Instant::now(),
            },
        );
        Ok(ctx)
    }

    fn invoke_exe(&self) -> PathBuf {
        self.invoke_exe.clone()
    }

    /// The agent's own allowlist, `[session] allowed_hosts` (AUD-124) —
    /// read fresh from disk on every external call so `agent allow-host`
    /// takes effect without a restart (AUD-121). External calls are rare;
    /// a failed reload refuses all hosts (fail-closed).
    fn allowed_hosts(&self) -> Vec<crate::config::schema::AllowedHost> {
        let path = self.config_dir.join("config.toml");
        match crate::config::schema::Config::load(&path) {
            Ok(config) => config.session.allowed_hosts,
            Err(error) => {
                tracing::warn!(%error, "could not reload the config; refusing external hosts");
                Vec::new()
            }
        }
    }

    fn allow_host_command(&self) -> &'static str {
        "agent allow-host"
    }

    fn is_admin(&self, token: &str) -> bool {
        token == self.admin_token
    }

    async fn unlock(
        &self,
        account: Option<&str>,
        password: secrecy::SecretString,
    ) -> Result<String> {
        // A fresh Ctx so a wrong passphrase does not disturb an existing
        // session; on success it replaces any lazily-opened one.
        let ctx = self.build_ctx(account)?;
        let name = ctx.account_name()?;
        ctx.unlock_with_password(password).await?;
        self.sessions.write().await.insert(
            name.clone(),
            Session {
                ctx: Arc::new(ctx),
                last_used: Instant::now(),
            },
        );
        Ok(name)
    }

    async fn lock(&self, account: Option<&str>, all: bool) -> Result<usize> {
        let mut sessions = self.sessions.write().await;
        if all {
            let count = sessions.len();
            sessions.clear();
            return Ok(count);
        }
        let name = self.build_ctx(account)?.account_name()?;
        Ok(sessions.remove(&name).is_some() as usize)
    }

    async fn agent_status(&self) -> Result<serde_json::Value> {
        let sessions = self.sessions.read().await;
        let mut unlocked: Vec<&String> = sessions.keys().collect();
        unlocked.sort();
        Ok(serde_json::json!({
            "unlocked_accounts": unlocked,
            "session_count": sessions.len(),
        }))
    }

    fn audit(&self, caller: &str, method: &str, path: &str, status: u16, detail: Option<&str>) {
        self.audit.append(&crate::session::audit::AuditEntry {
            time: crate::db::now_iso_utc(),
            caller: caller.to_owned(),
            method: method.to_owned(),
            path: path.to_owned(),
            status,
            detail: detail.map(str::to_owned),
        });
    }

    async fn start_job(
        &self,
        argv: Vec<String>,
        output: String,
        account: String,
        marketplace: Option<String>,
        settings: Option<String>,
    ) -> Result<String> {
        // Resolve the session up front so a bad account fails the
        // request, not silently the job. `account` is the resolved name
        // (the router validated the selectors, AUD-125).
        let ctx = self.session(Some(&account)).await?;
        let parts = rpc::invoke_argv(
            &ctx,
            &output,
            &argv,
            marketplace.as_deref(),
            settings.as_deref(),
        )
        .map_err(|refusal| anyhow::anyhow!("invalid job selectors: {:?}", refusal.status()))?;
        let exe = self.invoke_exe.clone();
        let id = hex::encode(rand::random::<[u8; 8]>());

        self.jobs.write().await.insert(
            id.clone(),
            JobEntry {
                account: account.clone(),
                state: Job::Running,
            },
        );
        let jobs = Arc::clone(&self.jobs);
        let job_id = id.clone();
        tokio::spawn(async move {
            let result = tokio::process::Command::new(&exe)
                .args(&parts)
                .stdin(std::process::Stdio::null())
                .output()
                .await;
            let state = match result {
                Ok(output) => Job::Done {
                    code: output.status.code().unwrap_or(1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                },
                Err(error) => Job::Done {
                    code: -1,
                    stdout: String::new(),
                    stderr: format!("could not run the job: {error}"),
                },
            };
            jobs.write()
                .await
                .insert(job_id, JobEntry { account, state });
        });
        Ok(id)
    }

    async fn job_status(&self, id: &str, viewer: Option<&str>) -> Option<serde_json::Value> {
        let jobs = self.jobs.read().await;
        let entry = jobs.get(id)?;
        // Another account's job reads as unknown to this caller.
        if viewer.is_some_and(|viewer| viewer != entry.account) {
            return None;
        }
        Some(match &entry.state {
            Job::Running => serde_json::json!({ "state": "running", "account": entry.account }),
            Job::Done {
                code,
                stdout,
                stderr,
            } => serde_json::json!({
                "state": "done",
                "account": entry.account,
                "code": code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        })
    }

    async fn list_jobs(&self, viewer: Option<&str>) -> serde_json::Value {
        let jobs = self.jobs.read().await;
        let list: Vec<serde_json::Value> = jobs
            .iter()
            .filter(|(_, entry)| viewer.is_none_or(|viewer| viewer == entry.account))
            .map(|(id, entry)| {
                let state = match entry.state {
                    Job::Running => "running",
                    Job::Done { .. } => "done",
                };
                serde_json::json!({ "job_id": id, "account": entry.account, "state": state })
            })
            .collect();
        serde_json::json!({ "jobs": list })
    }
}

impl AgentBackend {
    /// A fresh `Ctx` bound to `account` (the session-map identity). The
    /// client cell is unset — unlock happens lazily on first `client()`
    /// via the account's `password_source`.
    fn build_ctx(&self, account: Option<&str>) -> Result<Ctx> {
        Ctx::with_dir(
            self.config_dir.clone(),
            Selectors {
                account: account.map(str::to_owned),
                ..Default::default()
            },
        )
    }

    /// Drops sessions idle longer than the timeout.
    async fn evict_idle(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|name, session| {
            let keep = now.duration_since(session.last_used) < self.idle_timeout;
            if !keep {
                tracing::info!(account = name, "evicting idle session");
            }
            keep
        });
    }
}

/// Runs the agent in the foreground until SIGTERM/SIGINT: binds the
/// socket, writes the PID and token files, serves `/v1`, and cleans up.
/// This is the daemon body (`agent run`); `agent start` self-execs it
/// detached.
pub async fn serve(ctx: &Ctx) -> Result<()> {
    let dir = agent_dir(ctx);
    super::create_private_dir(&dir)?;

    if is_running(ctx) {
        bail!(
            "an agent is already running (pid file {})",
            pid_path(ctx).display()
        );
    }

    let socket_path = socket_path(ctx);
    let _ = std::fs::remove_file(&socket_path); // stale socket of a crashed run
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("could not bind {}", socket_path.display()))?;

    let admin_token = hex::encode(rand::random::<[u8; 32]>());
    write_private(&token_path(ctx), admin_token.as_bytes())?;
    write_private(&pid_path(ctx), std::process::id().to_string().as_bytes())?;

    let idle_timeout = crate::config::schema::parse_duration(&ctx.config().session.idle_timeout)
        .unwrap_or_else(|_| Duration::from_secs(900));
    let backend = Arc::new(AgentBackend {
        config_dir: ctx.config_dir().to_owned(),
        admin_token,
        invoke_exe: std::env::current_exe().context("could not resolve the own binary")?,
        idle_timeout,
        sessions: RwLock::new(HashMap::new()),
        tokens: TokenStore::new(ctx.config_dir()),
        audit: crate::session::audit::AuditLog::new(ctx.config_dir()),
        jobs: Arc::new(RwLock::new(HashMap::new())),
    });

    // Idle-eviction sweeper.
    let sweeper_backend = Arc::clone(&backend);
    let sweeper = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            sweeper_backend.evict_idle().await;
        }
    });

    let serve_backend: Arc<dyn Backend> = backend;
    let server = tokio::spawn(rpc::serve_unix(listener, Arc::clone(&serve_backend)));

    // Opt-in TCP listener (AUD-119): same router, untrusted — no admin
    // routes over the network; app tokens only.
    let tcp_server = match ctx.config().session.listen.as_deref() {
        Some(addr) => {
            let tcp = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("could not bind the TCP listener at {addr}"))?;
            tracing::warn!(
                %addr,
                "agent TCP listener enabled — reachable by any local process; \
                 expose beyond localhost only behind your own TLS/reverse proxy"
            );
            Some(tokio::spawn(rpc::serve_tcp(tcp, serve_backend)))
        }
        None => None,
    };

    tracing::info!(socket = %socket_path.display(), "agent started");
    wait_for_shutdown().await;
    tracing::info!("agent stopping");

    server.abort();
    if let Some(tcp_server) = tcp_server {
        tcp_server.abort();
    }
    sweeper.abort();
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(pid_path(ctx));
    let _ = std::fs::remove_file(token_path(ctx));
    Ok(())
}

/// Whether a live agent process owns the PID file.
pub fn is_running(ctx: &Ctx) -> bool {
    read_pid(ctx).is_some_and(process_alive)
}

/// The PID recorded in the PID file (whether alive or not).
pub fn read_pid(ctx: &Ctx) -> Option<i32> {
    let text = std::fs::read_to_string(pid_path(ctx)).ok()?;
    text.trim().parse::<i32>().ok()
}

/// Sends a signal to a PID; `0` only checks existence.
fn signal(pid: i32, sig: i32) -> bool {
    // SAFETY: kill with a valid signal number has no memory effects.
    unsafe { libc::kill(pid, sig) == 0 }
}

fn process_alive(pid: i32) -> bool {
    signal(pid, 0)
}

/// Asks a running agent to stop (SIGTERM). Returns the signalled PID.
pub fn stop(ctx: &Ctx) -> Result<i32> {
    let Some(pid) = read_pid(ctx).filter(|pid| process_alive(*pid)) else {
        bail!("no agent is running");
    };
    if !signal(pid, libc::SIGTERM) {
        bail!("could not signal the agent (pid {pid})");
    }
    Ok(pid)
}

/// Blocks until SIGTERM or SIGINT arrives.
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal as unix_signal};
    let mut term = unix_signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = unix_signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Writes a file with 0600 permissions (owner read/write only).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(tmp: &Path) -> Ctx {
        let config_dir = tmp.join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "version = 1\ndefault_account = \"a\"\n\n\
                 [accounts.a]\nauth_file = \"a.auth\"\nmarketplaces=[\"de\"]\n\
                 default_marketplaces=[\"de\"]\n\n[accounts.b]\nauth_file=\"b.auth\"\n\
                 marketplaces=[\"us\"]\ndefault_marketplaces=[\"us\"]\n\n\
                 [session]\nsocket_dir = {:?}\n",
                tmp.join("run")
            ),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.join("run")).unwrap();
        Ctx::with_dir(config_dir, Default::default()).unwrap()
    }

    fn backend(ctx: &Ctx, idle: Duration) -> AgentBackend {
        AgentBackend {
            config_dir: ctx.config_dir().to_owned(),
            admin_token: "T".into(),
            invoke_exe: PathBuf::from("/bin/true"),
            idle_timeout: idle,
            sessions: RwLock::new(HashMap::new()),
            tokens: TokenStore::new(ctx.config_dir()),
            audit: crate::session::audit::AuditLog::new(ctx.config_dir()),
            jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn session_map_resolves_accounts_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let backend = backend(&ctx, Duration::from_secs(900));

        // Default account resolves; a second call returns the same Ctx.
        let a1 = backend.session(None).await.unwrap();
        let a2 = backend.session(Some("a")).await.unwrap();
        assert!(Arc::ptr_eq(&a1, &a2));
        assert_eq!(a1.account_name().unwrap(), "a");

        // A different account is a different session.
        let b = backend.session(Some("b")).await.unwrap();
        assert!(!Arc::ptr_eq(&a1, &b));
        assert_eq!(backend.sessions.read().await.len(), 2);
    }

    #[tokio::test]
    async fn idle_sessions_are_evicted() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let backend = backend(&ctx, Duration::from_millis(20));
        backend.session(Some("a")).await.unwrap();
        assert_eq!(backend.sessions.read().await.len(), 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        backend.evict_idle().await;
        assert_eq!(backend.sessions.read().await.len(), 0);
    }

    #[tokio::test]
    async fn jobs_run_async_and_report_status() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        // /bin/true exits 0 with no output — enough to exercise the
        // running → done lifecycle without touching auth.
        let backend = backend(&ctx, Duration::from_secs(900));
        let id = backend
            .start_job(
                vec!["library".into()],
                "json".into(),
                "a".into(),
                None,
                None,
            )
            .await
            .unwrap();
        // Poll until the spawned job finishes.
        let mut status = None;
        for _ in 0..100 {
            let snapshot = backend.job_status(&id, None).await.unwrap();
            if snapshot["state"] == "done" {
                status = Some(snapshot);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = status.expect("job finished");
        assert_eq!(status["code"], 0);
        assert_eq!(status["account"], "a");
        let list = backend.list_jobs(None).await;
        assert!(
            list["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|job| job["job_id"] == id.as_str())
        );
        // Job scoping (AUD-125): another account's viewer sees nothing.
        assert!(backend.job_status(&id, Some("b")).await.is_none());
        assert!(backend.job_status(&id, Some("a")).await.is_some());
        let filtered = backend.list_jobs(Some("b")).await;
        assert!(filtered["jobs"].as_array().unwrap().is_empty());
        assert!(backend.job_status("nonexistent", None).await.is_none());
    }

    #[test]
    fn authenticate_only_the_admin_token() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path());
        let backend = backend(&ctx, Duration::from_secs(1));
        assert!(backend.authenticate("wrong").is_none());
        let auth = backend.authenticate("T").unwrap();
        assert!(
            auth.scopes.contains(&"api".to_owned()) && auth.scopes.contains(&"invoke".to_owned())
        );
        assert!(auth.account.is_none());

        // An app token authenticates with its own scopes + binding.
        let token = backend
            .tokens
            .create("web", vec!["api".into()], Some("b".into()), None)
            .unwrap();
        let auth = backend.authenticate(&token).unwrap();
        assert_eq!(auth.scopes, ["api"]);
        assert_eq!(auth.account.as_deref(), Some("b"));
    }
}
