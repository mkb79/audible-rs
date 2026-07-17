//! Account bookkeeping: `list`, `remove`, `rename`, `set-default`,
//! `marketplaces`, `export`, `logout` and the agent-facing `unlock`/`lock`.

use anyhow::{Context as _, Result, bail};
use clap::Args;
#[cfg(unix)]
use secrecy::ExposeSecret;
use secrecy::SecretString;

use crate::auth::authfile::{self, KdfParams, Protection};
use crate::config::ctx::Ctx;
use crate::config::schema::validate_name;
use crate::config::write;

use super::*;

/// Deregister the selected account's device with Amazon, then delete it
/// locally (config entry + auth file)
#[derive(Debug, Args)]
pub(super) struct LogoutArgs {
    /// Also deregister other registrations that share this device's serial
    /// number (Amazon's `deregister_all_existing_accounts`) — same account
    /// and serial, not every device of the account; usually there is just
    /// this one
    #[arg(long)]
    all: bool,

    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    yes: bool,
}

/// Remove an account from the config (does not deregister the device)
#[derive(Debug, Args)]
pub(super) struct RemoveArgs {
    /// Account to remove
    name: String,

    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    yes: bool,

    /// Also delete the account's auth file from disk
    #[arg(long)]
    delete_auth_file: bool,
}

pub(super) fn list(ctx: &Ctx) -> Result<()> {
    let config = ctx.config();
    if config.accounts.is_empty() {
        eprintln!(
            "no accounts configured — register one with `audible account login` \
             (or import a legacy auth file with `audible account import <file>`)"
        );
    }
    let default_account = config.default_account.as_deref();
    let mut rows = Vec::new();
    for (name, account) in &config.accounts {
        let password_source = serde_json::to_value(account.password_source)?
            .as_str()
            .unwrap_or_default()
            .to_owned();
        rows.push(vec![
            name.clone(),
            auth_file_state(ctx, &account.auth_file).to_owned(),
            password_source,
            account.marketplaces.join(","),
            account.default_marketplaces.join(","),
            if default_account == Some(name.as_str()) {
                "*"
            } else {
                ""
            }
            .to_owned(),
        ]);
    }
    ctx.print(&crate::output::Output::table(
        vec![
            "name",
            "auth_file",
            "password_source",
            "marketplaces",
            "default_mp",
            "default",
        ],
        rows,
    ));
    Ok(())
}

/// `account set-default <name>`: makes an account the top-level default.
pub(super) fn set_default(ctx: &Ctx, name: &str) -> Result<()> {
    if !ctx.config().accounts.contains_key(name) {
        bail!("unknown account {name:?}");
    }
    write::edit_file(&ctx.config_file(), |content| {
        write::set(content, "default_account", name)
    })?;
    eprintln!("default account is now {name:?}");
    Ok(())
}

/// `account rename <old> <new>`: renames an account in the config and
/// repoints `default_account` if it referenced the old name. The auth
/// file stays where it is — the renamed entry keeps pointing at it.
pub(super) fn rename(ctx: &Ctx, old: &str, new: &str) -> Result<()> {
    if !ctx.config().accounts.contains_key(old) {
        bail!("unknown account {old:?}");
    }
    validate_name(new)?;
    if old == new {
        bail!("account is already named {new:?}");
    }
    if ctx.config().accounts.contains_key(new) {
        bail!("account {new:?} already exists");
    }
    let was_default = ctx.config().default_account.as_deref() == Some(old);

    write::edit_file(&ctx.config_file(), |content| {
        write::rename_account(content, old, new)
    })?;

    eprintln!("renamed account {old:?} to {new:?}");
    if was_default {
        eprintln!("default_account now points to {new:?}");
    }
    Ok(())
}

/// `account marketplaces add <cc>`: adds a marketplace to the resolved
/// account's set.
pub(super) fn marketplaces_add(ctx: &Ctx, cc: &str, set_default: bool) -> Result<()> {
    let name = ctx.account_name()?;
    let cc = cc.to_ascii_lowercase();
    let account = ctx.account()?;
    let already_listed = account.marketplaces.contains(&cc);
    let already_default = account.default_marketplaces.contains(&cc);
    let mut marketplaces = account.marketplaces.clone();
    let mut default_marketplaces = account.default_marketplaces.clone();

    // Already in the available set: only the default set could still change.
    if already_listed {
        if set_default && !already_default {
            default_marketplaces.push(cc.clone());
            write::edit_file(&ctx.config_file(), |content| {
                write::set_array(
                    content,
                    &format!("accounts.{name}.default_marketplaces"),
                    &default_marketplaces,
                )
            })?;
            eprintln!("added marketplace {cc:?} to the default set of account {name:?}");
        } else {
            eprintln!("account {name:?} already lists marketplace {cc:?}");
            if !already_default {
                print_default_marketplace_hint(&cc);
            }
        }
        return Ok(());
    }

    // Brand-new marketplace: extend `marketplaces`, and `default_marketplaces`
    // too when --default is given (it must stay a subset, so write it after).
    marketplaces.push(cc.clone());
    if set_default {
        default_marketplaces.push(cc.clone());
    }
    write::edit_file(&ctx.config_file(), |content| {
        let content = write::set_array(
            content,
            &format!("accounts.{name}.marketplaces"),
            &marketplaces,
        )?;
        if set_default {
            write::set_array(
                &content,
                &format!("accounts.{name}.default_marketplaces"),
                &default_marketplaces,
            )
        } else {
            Ok(content)
        }
    })?;

    if set_default {
        eprintln!("added marketplace {cc:?} to account {name:?} (and to its default set)");
    } else {
        eprintln!("added marketplace {cc:?} to account {name:?}");
        print_default_marketplace_hint(&cc);
    }
    Ok(())
}

/// Hints that a marketplace outside the account's default set is skipped
/// by commands run without `-m`, and how to include it.
fn print_default_marketplace_hint(cc: &str) {
    eprintln!(
        "note: {cc:?} is not in the account's default marketplaces — commands without \
         -m/--marketplace will not include it. Add it with \
         `audible account marketplaces add {cc} --default` or \
         `audible account marketplaces set-default <csv>`."
    );
}

/// `account marketplaces remove <cc>`: removes a marketplace from the
/// resolved account's set (and from its default set).
pub(super) fn marketplaces_remove(ctx: &Ctx, cc: &str) -> Result<()> {
    let name = ctx.account_name()?;
    let cc = cc.to_ascii_lowercase();
    let account = ctx.account()?;
    if !account.marketplaces.contains(&cc) {
        bail!("account {name:?} does not list marketplace {cc:?}");
    }
    let marketplaces: Vec<String> = account
        .marketplaces
        .iter()
        .filter(|m| **m != cc)
        .cloned()
        .collect();
    let default_marketplaces: Vec<String> = account
        .default_marketplaces
        .iter()
        .filter(|m| **m != cc)
        .cloned()
        .collect();
    write::edit_file(&ctx.config_file(), |content| {
        // Shrink the default set first so it stays a subset throughout.
        let content = write::set_array(
            content,
            &format!("accounts.{name}.default_marketplaces"),
            &default_marketplaces,
        )?;
        write::set_array(
            &content,
            &format!("accounts.{name}.marketplaces"),
            &marketplaces,
        )
    })?;
    eprintln!("removed marketplace {cc:?} from account {name:?}");
    Ok(())
}

/// `account marketplaces set-default <csv>`: sets the resolved account's
/// default marketplace set (must be a subset of its marketplaces).
pub(super) fn marketplaces_set_default(ctx: &Ctx, csv: &str) -> Result<()> {
    let name = ctx.account_name()?;
    let set = parse_marketplaces(csv)?;
    write::edit_file(&ctx.config_file(), |content| {
        write::set_array(
            content,
            &format!("accounts.{name}.default_marketplaces"),
            &set,
        )
    })?;
    eprintln!(
        "default marketplaces of account {name:?} are now {}",
        set.join(", ")
    );
    Ok(())
}

pub(super) fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<()> {
    let name = &args.name;
    let Some(account) = ctx.config().accounts.get(name) else {
        bail!("unknown account {name:?}");
    };
    let auth_file = account.auth_file.clone();
    let pw_path = crate::config::passwords::resolve_path(ctx.config_dir(), account);

    let clears_default = ctx.config().default_account.as_deref() == Some(name.as_str());

    eprintln!("This removes account {name:?} from the config.");
    if clears_default {
        eprintln!("default_account points here and will be cleared.");
    }
    eprintln!(
        "The device registration stays active — use `account logout` to also \
         deregister it with Amazon."
    );
    if args.delete_auth_file {
        eprintln!(
            "warning: deleting the auth file makes a later deregistration via \
             this CLI impossible — the auth material is the device identity. \
             You can still remove the device on the Audible/Amazon website \
             (account settings -> devices):\n\
             https://help.audible.com/s/article/remove-devices-from-account"
        );
    }

    if !crate::commands::prompt::confirm(args.yes, "Continue?")? {
        bail!("aborted");
    }

    write::edit_file(&ctx.config_file(), |content| {
        // Order matters for the schema gate: clear default_account first,
        // then remove the account table.
        let content = if clears_default {
            write::unset(content, "default_account")?
        } else {
            content.to_owned()
        };
        write::unset(&content, &format!("accounts.{name}"))
    })?;
    crate::config::passwords::remove(&pw_path, name)?;
    eprintln!("removed account {name:?}");

    let auth_path = if auth_file.is_absolute() {
        auth_file
    } else {
        ctx.config_dir().join(auth_file)
    };
    if args.delete_auth_file {
        match std::fs::remove_file(&auth_path) {
            Ok(()) => eprintln!("deleted {}", auth_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else {
        eprintln!(
            "auth file kept at {} (remove it with --delete-auth-file)",
            auth_path.display()
        );
    }
    Ok(())
}

/// `account logout`: deregisters the selected account's device with Amazon
/// (so it disappears from "Manage Your Content and Devices"), then deletes
/// the account locally — its config entry and auth file. The deregistration
/// invalidates the tokens server-side, so the local material is useless
/// afterwards; deletion is therefore part of the operation. Local deletion
/// only happens once the deregistration has succeeded, so a failure leaves
/// the auth material intact for a retry.
pub(super) async fn logout(ctx: &Ctx, args: LogoutArgs) -> Result<()> {
    let name = ctx.account_name()?;
    let Some(account) = ctx.config().accounts.get(&name) else {
        bail!("unknown account {name:?}");
    };
    let auth_file = account.auth_file.clone();
    let pw_path = crate::config::passwords::resolve_path(ctx.config_dir(), account);
    let clears_default = ctx.config().default_account.as_deref() == Some(name.as_str());

    if args.all {
        eprintln!(
            "This deregisters account {name:?}'s device — and any other registration \
             sharing its serial number — with Amazon, then deletes it locally \
             (config entry + auth file)."
        );
    } else {
        eprintln!(
            "This deregisters account {name:?}'s device with Amazon, then deletes \
             the account locally (config entry + auth file)."
        );
    }
    if clears_default {
        eprintln!("default_account points here and will be cleared.");
    }

    if !crate::commands::prompt::confirm(args.yes, "Continue?")? {
        bail!("aborted");
    }

    // Deregister with Amazon first (x-amz-access-token, refreshed if needed).
    let client = ctx.client().await?;
    let device_name = client
        .deregister(args.all)
        .await
        .context("deregistration failed — the account was left untouched")?;
    match &device_name {
        Some(name) => eprintln!("deregistered device {name:?} with Amazon"),
        None => eprintln!("deregistered the device with Amazon"),
    }

    // Then delete locally: clear default_account first (the schema gate),
    // drop the account table, then remove the auth file.
    write::edit_file(&ctx.config_file(), |content| {
        let content = if clears_default {
            write::unset(content, "default_account")?
        } else {
            content.to_owned()
        };
        write::unset(&content, &format!("accounts.{name}"))
    })?;
    crate::config::passwords::remove(&pw_path, &name)?;
    eprintln!("removed account {name:?} from the config");

    let auth_path = if auth_file.is_absolute() {
        auth_file
    } else {
        ctx.config_dir().join(auth_file)
    };
    match std::fs::remove_file(&auth_path) {
        Ok(()) => eprintln!("deleted {}", auth_path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Loads an account's auth file for a password operation. The current
/// password comes from `AUDIBLE_AUTH_PASSWORD[_<NAME>]` or a prompt.
/// Returns the authenticator, the file path and the on-disk state.
/// `account export`: write the selected account's auth material to a file.
#[derive(Debug, Args)]
pub(super) struct ExportArgs {
    /// Destination file (required — secrets are never written to stdout)
    #[arg(long, value_name = "FILE")]
    out: std::path::PathBuf,

    /// encrypted: new-format envelope (password via prompt or
    /// AUDIBLE_NEW_AUTH_PASSWORD); plain: unencrypted new format;
    /// python: legacy audible/audible-cli layout (unencrypted)
    #[arg(long, default_value = "encrypted", value_parser = ["encrypted", "plain", "python"])]
    format: String,

    /// Overwrite an existing destination file
    #[arg(long)]
    force: bool,
}

pub(super) async fn export(ctx: &Ctx, args: ExportArgs) -> Result<()> {
    let name = ctx.account_name()?;
    if args.out.exists() && !args.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            args.out.display()
        );
    }
    let (auth, _path, _state) = load_account_auth(ctx, &name).await?;

    let content = match args.format.as_str() {
        "encrypted" => {
            // Fresh password for the export copy (independent of the live
            // auth file's password); env enables non-interactive use.
            let password = match std::env::var("AUDIBLE_NEW_AUTH_PASSWORD") {
                Ok(password) => SecretString::from(password),
                Err(_) => prompt_new_password()?,
            };
            // Argon2id off the async runtime (E5) — authfile's own
            // contract, honored everywhere else.
            let value = auth.export_value();
            tokio::task::spawn_blocking(move || {
                authfile::write(
                    &value,
                    Protection::Encrypted(KdfParams::default()),
                    Some(&password),
                )
            })
            .await
            .expect("blocking export task must not panic")?
        }
        "plain" => {
            eprintln!(
                "warning: writing UNENCRYPTED auth material (tokens, device key) — \
                 keep the file safe or delete it after use"
            );
            authfile::write(&auth.export_value(), Protection::Plain, None)?
        }
        _ => {
            let (legacy, dropped_domains) = auth.export_legacy_value();
            eprintln!(
                "warning: writing UNENCRYPTED auth material (tokens, device key) — \
                 keep the file safe or delete it after use"
            );
            if !dropped_domains.is_empty() {
                eprintln!(
                    "note: the Python format keeps one flat cookie set — cookies for \
                     {} were not exported",
                    dropped_domains.join(", ")
                );
            }
            serde_json::to_string_pretty(&legacy)?
        }
    };

    write_export_file(&args.out, content.as_bytes(), args.force)
        .with_context(|| format!("could not write {}", args.out.display()))?;
    eprintln!(
        "exported account {name:?} as {} to {}",
        args.format,
        args.out.display()
    );
    Ok(())
}

/// Writes export content with owner-only permissions (`0o600`) on Unix; on
/// Windows the mode is a no-op — the exported auth file rests on user-profile
/// isolation, not an ACL (AUD-198). Without `force` the create fails when the
/// file already exists (`create_new`, no TOCTOU window); with it, permissions
/// are tightened afterwards too.
fn write_export_file(path: &std::path::Path, content: &[u8], force: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content)?;
    file.flush()
}

/// `account unlock` — resolve the selected account's passphrase (from
/// `password_source`, else prompt) and hand it to the running agent so
/// the session is served without the passphrase (AUD-116).
///
/// The session agent is Unix-only for now (see AUD-193); on Windows both
/// `unlock` and `lock` report that cleanly instead of failing to build.
#[cfg(not(unix))]
pub(super) async fn unlock(_ctx: &Ctx) -> Result<()> {
    anyhow::bail!("the session agent is not available on Windows yet (see AUD-193)")
}

#[cfg(unix)]
pub(super) async fn unlock(ctx: &Ctx) -> Result<()> {
    let account_name = ctx.account_name()?;
    let account = crate::config::resolve::account(ctx.config(), &account_name)?;
    let password =
        match crate::config::ctx::resolve_password(ctx.config_dir(), &account_name, account).await?
        {
            Some(password) => password,
            None => SecretString::from(crate::commands::prompt::prompt_secret(&format!(
                "Passphrase for account {account_name:?}"
            ))?),
        };
    let (status, body) = crate::session::client::admin_request(
        ctx,
        hyper::Method::POST,
        "/v1/agent/unlock",
        Some(serde_json::json!({
            "account": account_name,
            "passphrase": password.expose_secret(),
        })),
    )
    .await?;
    if status.is_success() {
        eprintln!("unlocked account {account_name:?} in the agent");
        Ok(())
    } else {
        bail!(
            "unlock failed: {}",
            body.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("error")
        );
    }
}

/// `account lock` — drop the selected account's session in the agent (or
/// every session with `--all`).
#[cfg(not(unix))]
pub(super) async fn lock(_ctx: &Ctx, _all: bool) -> Result<()> {
    anyhow::bail!("the session agent is not available on Windows yet (see AUD-193)")
}

#[cfg(unix)]
pub(super) async fn lock(ctx: &Ctx, all: bool) -> Result<()> {
    let body = if all {
        serde_json::json!({ "all": true })
    } else {
        serde_json::json!({ "account": ctx.account_name()? })
    };
    let (status, reply) = crate::session::client::admin_request(
        ctx,
        hyper::Method::POST,
        "/v1/agent/lock",
        Some(body),
    )
    .await?;
    if status.is_success() {
        let count = reply.get("locked").and_then(|v| v.as_u64()).unwrap_or(0);
        eprintln!("locked {count} session(s)");
        Ok(())
    } else {
        bail!(
            "lock failed: {}",
            reply
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("error")
        );
    }
}
