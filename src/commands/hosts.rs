//! Shared external-host allowlist plumbing for `plugin …-host` and
//! `agent …-host` (AUD-124). The two consumers keep **separate trust
//! domains** — `[plugins] allowed_hosts` vs `[session] allowed_hosts` —
//! and only share the CLI shape: the same subcommand trio, approval
//! writer and table rendering, parameterized by the config table.

use anyhow::Result;
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::config::schema::AllowedHost;
use crate::config::write;
use crate::output::Output;

/// The `allow-host`/`list-hosts`/`remove-host` subcommand trio, defined
/// once for both consumers; `consumer` names who the approval applies
/// to in the help texts (e.g. "agent callers").
pub(crate) fn subcommands(consumer: &str) -> [clap::Command; 3] {
    [
        clap::Command::new("allow-host")
            .about(format!("Approve an external host for {consumer}"))
            .arg(
                Arg::new("host")
                    .required(true)
                    .value_name("HOST")
                    .help("External HTTPS host (e.g. api.example.com)"),
            )
            .arg(
                Arg::new("auth")
                    .long("auth")
                    .value_name("MODE")
                    .value_parser(["auto", "signing", "token"])
                    .default_value("signing")
                    .help("Auth mode used against this host"),
            ),
        clap::Command::new("list-hosts")
            .about(format!("List external hosts approved for {consumer}")),
        clap::Command::new("remove-host")
            .about(format!("Remove an external host approved for {consumer}"))
            .arg(
                Arg::new("host")
                    .required(true)
                    .value_name("HOST")
                    .help("Host to remove from the allowlist"),
            ),
    ]
}

/// Approves `host` in the given config `table` (`plugins` or `session`).
pub(crate) fn allow(ctx: &Ctx, table: &str, host: &str, auth: &str) -> Result<()> {
    write::edit_file(&ctx.config_file(), |content| {
        write::add_allowed_host(content, table, host, auth)
    })?;
    eprintln!("approved external host {host:?} (auth: {auth})");
    if auth == "token" {
        eprintln!(
            "warning: `token` sends your account's x-amz-access-token to this host — \
             prefer `signing` unless the host requires it"
        );
    }
    Ok(())
}

/// Renders an allowlist; `noun` is the CLI noun for the hint.
pub(crate) fn list(ctx: &Ctx, hosts: &[AllowedHost], noun: &str) -> Result<()> {
    if hosts.is_empty() {
        eprintln!("no approved external hosts (add one with `audible {noun} allow-host <host>`)");
        return Ok(());
    }
    ctx.print(&Output::table(
        vec!["host", "auth"],
        hosts
            .iter()
            .map(|entry| vec![entry.host.clone(), entry.auth.clone()])
            .collect(),
    ));
    Ok(())
}

/// Removes `host` from the given config `table`.
pub(crate) fn remove(ctx: &Ctx, table: &str, host: &str) -> Result<()> {
    write::edit_file(&ctx.config_file(), |content| {
        write::remove_allowed_host(content, table, host)
    })?;
    eprintln!("removed external host {host:?}");
    Ok(())
}
