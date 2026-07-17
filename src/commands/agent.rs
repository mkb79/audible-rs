//! `audible agent` — manage the long-lived session agent (M5, AUD-115):
//! `start` (detached daemon, `--foreground` to stay in the terminal),
//! `stop` (SIGTERM via the PID file), `status`. The daemon body runs as
//! the hidden `agent run` self-exec. The agent serves the same `/v1`
//! router as the plugin broker over a stable socket, holding unlocked
//! account sessions in memory.

use std::os::unix::process::CommandExt as _;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::ArgAction;

use crate::config::ctx::Ctx;
use crate::output::Output;
use crate::session::agent;

/// `audible agent`.
pub struct AgentCommand;

#[async_trait::async_trait]
impl super::Command for AgentCommand {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Manage the session agent (a resident process holding unlocked accounts)")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                clap::Command::new("start")
                    .about("Start the agent (detached; --foreground stays in the terminal)")
                    .arg(
                        clap::Arg::new("foreground")
                            .long("foreground")
                            .action(ArgAction::SetTrue)
                            .help("Run in the foreground (for systemd or debugging)"),
                    ),
            )
            .subcommand(clap::Command::new("stop").about("Stop the running agent"))
            .subcommand(clap::Command::new("status").about("Show whether the agent is running"))
            .subcommand(
                clap::Command::new("audit")
                    .about("Show the agent's request audit log")
                    .arg(
                        clap::Arg::new("tail")
                            .long("tail")
                            .value_name("N")
                            .value_parser(clap::value_parser!(usize))
                            .help("Only the last N entries"),
                    )
                    .arg(
                        clap::Arg::new("caller")
                            .long("caller")
                            .value_name("NAME")
                            .help("Only entries from this caller (token label or admin)"),
                    ),
            )
            .subcommand(
                clap::Command::new("token")
                    .about("Manage app tokens for external callers (web backends)")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("create")
                            .about("Create an app token (printed once)")
                            .arg(
                                clap::Arg::new("label")
                                    .long("label")
                                    .alias("name")
                                    .required(true)
                                    .value_name("LABEL")
                                    .help("Label (also the revoke key)"),
                            )
                            .arg(
                                clap::Arg::new("scopes")
                                    .long("scopes")
                                    .required(true)
                                    .value_name("SCOPES")
                                    .value_delimiter(',')
                                    // Derived from VALID_SCOPES so it cannot drift.
                                    .help(format!(
                                        "Granted scopes: {}",
                                        crate::plugins::VALID_SCOPES.join(",")
                                    )),
                            )
                            .arg(
                                clap::Arg::new("account")
                                    .long("account")
                                    .value_name("NAME")
                                    .help("Pin the token to one account (else request-chosen)"),
                            )
                            .arg(
                                clap::Arg::new("ttl")
                                    .long("ttl")
                                    .value_name("DURATION")
                                    .help("Expiry, e.g. 30d/12h (else never expires)"),
                            ),
                    )
                    .subcommand(
                        clap::Command::new("list").about("List app tokens (never the token)"),
                    )
                    .subcommand(
                        clap::Command::new("revoke")
                            .about("Revoke an app token by label")
                            .arg(
                                clap::Arg::new("label")
                                    .required(true)
                                    .value_name("LABEL")
                                    .help("Label of the token (see `agent token list`)"),
                            ),
                    ),
            )
            .subcommands(crate::commands::hosts::subcommands("agent callers"))
            .subcommand(
                // Hidden: the actual daemon body that `start` self-execs.
                clap::Command::new("run").hide(true),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        use crate::commands::hosts;
        match matches.subcommand() {
            Some(("start", start)) => start_agent(ctx, start.get_flag("foreground")).await,
            Some(("run", _)) => agent::serve(ctx, super::plugin::builtin_names()).await,
            Some(("stop", _)) => stop_agent(ctx),
            Some(("status", _)) => status(ctx).await,
            Some(("audit", audit)) => show_audit(
                ctx,
                audit.get_one::<usize>("tail").copied(),
                audit.get_one::<String>("caller").map(String::as_str),
            ),
            Some(("token", sub)) => match sub.subcommand() {
                Some(("create", create)) => token_create(ctx, create),
                Some(("list", _)) => token_list(ctx),
                Some(("revoke", revoke)) => {
                    token_revoke(ctx, revoke.get_one::<String>("label").expect("required"))
                }
                _ => unreachable!("subcommand required"),
            },
            // The agent's own host allowlist (`[session] allowed_hosts`,
            // AUD-124) — the running agent reads it fresh per external
            // call, so changes apply without a restart (AUD-121).
            Some(("allow-host", args)) => hosts::allow(
                ctx,
                "session",
                args.get_one::<String>("host").expect("required"),
                args.get_one::<String>("auth").expect("default"),
            ),
            Some(("list-hosts", _)) => {
                hosts::list(ctx, &ctx.config().session.allowed_hosts, "agent")
            }
            Some(("remove-host", args)) => hosts::remove(
                ctx,
                "session",
                args.get_one::<String>("host").expect("required"),
            ),
            _ => unreachable!("subcommand required"),
        }
    }
}

/// `agent start` — `--foreground` runs the daemon here; otherwise spawn a
/// detached `agent run` in its own process group with output redirected
/// to the agent log, then wait for the socket to appear.
async fn start_agent(ctx: &Ctx, foreground: bool) -> Result<()> {
    if agent::is_running(ctx) {
        bail!("the agent is already running");
    }
    if foreground {
        return agent::serve(ctx, super::plugin::builtin_names()).await;
    }

    let exe = std::env::current_exe().context("could not resolve the own binary")?;
    let log_path = agent::pid_path(ctx).with_file_name("agent.log");
    crate::session::create_private_dir(log_path.parent().expect("agent dir"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("could not open {}", log_path.display()))?;
    let log_err = log.try_clone()?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("agent")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    // Detach into a new session so the daemon outlives this shell.
    // SAFETY: setsid in the pre-exec hook only detaches the child.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn().context("could not spawn the agent")?;

    // Wait for the socket (or an early exit) — up to ~3s.
    let socket = agent::socket_path(ctx);
    for _ in 0..30 {
        if socket.exists() {
            eprintln!(
                "agent started (pid {}), socket {}",
                child.id(),
                socket.display()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "the agent did not come up within 3s — see the log at {}",
        log_path.display()
    );
}

/// `agent audit` — render the request audit log (optionally tailed and
/// filtered by caller).
fn show_audit(ctx: &Ctx, tail: Option<usize>, caller: Option<&str>) -> Result<()> {
    let log = crate::session::audit::AuditLog::new(ctx.config_dir());
    let mut entries = log.read();
    if let Some(caller) = caller {
        entries.retain(|entry| entry.caller == caller);
    }
    if let Some(n) = tail
        && entries.len() > n
    {
        entries.drain(..entries.len() - n);
    }
    if entries.is_empty() {
        eprintln!("no audit entries");
        return Ok(());
    }
    let rows = entries
        .into_iter()
        .map(|entry| {
            vec![
                entry.time,
                entry.caller,
                entry.method,
                entry.path,
                entry.status.to_string(),
                entry.detail.unwrap_or_default(),
            ]
        })
        .collect();
    ctx.print(&Output::table(
        vec!["time", "caller", "method", "path", "status", "detail"],
        rows,
    ));
    Ok(())
}

/// `agent token create` — mint an app token, store its hash, print it once.
fn token_create(ctx: &Ctx, args: &clap::ArgMatches) -> Result<()> {
    let store = crate::session::tokens::TokenStore::new(ctx.config_dir());
    let scopes: Vec<String> = args
        .get_many::<String>("scopes")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    let account = args.get_one::<String>("account").cloned();
    let ttl = args
        .get_one::<String>("ttl")
        .map(|value| crate::config::schema::parse_duration(value))
        .transpose()
        .context("invalid --ttl")?;
    let token = store.create(
        args.get_one::<String>("label").expect("required"),
        scopes,
        account,
        ttl,
    )?;
    // The one and only time the plaintext is shown.
    println!("{token}");
    eprintln!(
        "token created; store it now — it is not recoverable. \
         Callers send it as `Authorization: Bearer <token>`."
    );
    Ok(())
}

/// `agent token list` — labels/scopes/binding/expiry, never the token.
fn token_list(ctx: &Ctx) -> Result<()> {
    let store = crate::session::tokens::TokenStore::new(ctx.config_dir());
    let records = store.load()?;
    if records.is_empty() {
        eprintln!("no app tokens (create one with `audible agent token create`)");
        return Ok(());
    }
    let rows = records
        .into_iter()
        .map(|record| {
            vec![
                record.label,
                record.scopes.join(","),
                record.account.unwrap_or_else(|| "-".to_owned()),
                record.expires.unwrap_or_else(|| "never".to_owned()),
                record.created,
            ]
        })
        .collect();
    ctx.print(&Output::table(
        vec!["label", "scopes", "account", "expires", "created"],
        rows,
    ));
    Ok(())
}

/// `agent token revoke <label>`.
fn token_revoke(ctx: &Ctx, label: &str) -> Result<()> {
    let store = crate::session::tokens::TokenStore::new(ctx.config_dir());
    if store.revoke(label)? {
        eprintln!("revoked token {label:?}");
    } else {
        bail!("no token named {label:?}");
    }
    Ok(())
}

/// `agent stop` — SIGTERM the running daemon.
fn stop_agent(ctx: &Ctx) -> Result<()> {
    let pid = agent::stop(ctx)?;
    eprintln!("signalled the agent (pid {pid}) to stop");
    Ok(())
}

/// `agent status` — running/stopped, socket, and (when running) the
/// unlocked accounts and session count from the live agent.
async fn status(ctx: &Ctx) -> Result<()> {
    let running = agent::is_running(ctx);
    let mut rows = vec![
        vec![
            "status".to_owned(),
            if running { "running" } else { "stopped" }.to_owned(),
        ],
        vec![
            "pid".to_owned(),
            agent::read_pid(ctx)
                .filter(|_| running)
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ],
        vec![
            "socket".to_owned(),
            agent::socket_path(ctx).display().to_string(),
        ],
    ];
    if running
        && let Ok((_, body)) =
            crate::session::client::admin_request(ctx, hyper::Method::GET, "/v1/agent/status", None)
                .await
    {
        let unlocked = body
            .get("unlocked_accounts")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_owned());
        rows.push(vec!["unlocked accounts".to_owned(), unlocked]);
    }
    ctx.print(&Output::table(vec!["field", "value"], rows));
    Ok(())
}
