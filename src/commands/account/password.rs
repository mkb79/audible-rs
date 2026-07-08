//! `account password set|remove|source` — auth-file encryption and the
//! passphrase sources (prompt|env|command|file).

use anyhow::{Context as _, Result, bail};
use secrecy::{ExposeSecret, SecretString};

use crate::auth::authfile::KdfParams;
use crate::config::ctx::Ctx;
use crate::config::write;

use super::*;

/// `account password set`: encrypts a plain file or re-encrypts an
/// encrypted one with a new password (fresh salt and nonce either way).
pub(super) async fn password_set(ctx: &Ctx, name: &str) -> Result<()> {
    let (auth, path, state) = load_account_auth(ctx, name).await?;

    // AUDIBLE_NEW_AUTH_PASSWORD enables non-interactive use (scripts).
    let new_password = match std::env::var("AUDIBLE_NEW_AUTH_PASSWORD") {
        Ok(password) => SecretString::from(password),
        Err(_) => prompt_new_password()?,
    };
    // When the account reads its passphrase from the passwords file, keep a
    // copy to write back after the re-encryption (only exposed in that case).
    let write_back = matches!(
        ctx.config().accounts.get(name).map(|a| a.password_source),
        Some(crate::config::schema::PasswordSource::File)
    )
    .then(|| SecretString::from(new_password.expose_secret().to_owned()));

    auth.save_to(&path, Some(new_password), KdfParams::default())
        .await?;

    if let Some(new_password) = write_back {
        let account = ctx.config().accounts.get(name).expect("checked above");
        let pw_path = crate::config::passwords::resolve_path(ctx.config_dir(), account);
        crate::config::passwords::upsert(&pw_path, name, &new_password)?;
        eprintln!("updated the passwords entry for {name:?}");
    }

    if state == "plain" {
        eprintln!("auth file of {name:?} is now encrypted");
    } else {
        eprintln!("password of {name:?} changed");
    }
    Ok(())
}

/// `account password remove`: stores the auth file unencrypted.
pub(super) async fn password_remove(ctx: &Ctx, name: &str, yes: bool) -> Result<()> {
    let state = {
        let Some(account) = ctx.config().accounts.get(name) else {
            bail!("unknown account {name:?}");
        };
        auth_file_state(ctx, &account.auth_file)
    };
    if state == "plain" {
        bail!("auth file of {name:?} is already unencrypted");
    }

    eprintln!(
        "warning: this stores the auth file of {name:?} UNENCRYPTED — anyone \
         with file access can read your tokens"
    );
    if !crate::commands::prompt::confirm(yes, "Continue?")? {
        bail!("aborted");
    }

    let (auth, path, _) = load_account_auth(ctx, name).await?;
    auth.save_to(&path, None, KdfParams::default()).await?;

    // A now-plain file needs no passphrase: drop any passwords entry and
    // reset the source so the next load doesn't demand a missing entry.
    if let Some(account) = ctx.config().accounts.get(name)
        && account.password_source == crate::config::schema::PasswordSource::File
    {
        let pw_path = crate::config::passwords::resolve_path(ctx.config_dir(), account);
        crate::config::passwords::remove(&pw_path, name)?;
        let key = format!("accounts.{name}.password_source");
        write::edit_file(&ctx.config_file(), |content| {
            write::set(content, &key, "prompt")
        })?;
        eprintln!("removed the passwords entry for {name:?}; password_source reset to prompt");
    }

    eprintln!("auth file of {name:?} is now UNENCRYPTED");
    Ok(())
}

/// Removes a config key only if it is currently set — so switching password
/// modes can clear the now-irrelevant `password_command`/`password_file`
/// without erroring when it was never there.
fn clear_if_set(content: &str, key: &str) -> Result<String, crate::config::ConfigError> {
    if write::get(content, key)?.is_some() {
        write::unset(content, key)
    } else {
        Ok(content.to_owned())
    }
}

/// `account password source <NAME> <MODE>`: choose where the auth-file
/// passphrase comes from. `file` writes the current passphrase into the
/// passwords file (requires an encrypted auth file); switching away removes
/// the entry.
pub(super) async fn password_source(
    ctx: &Ctx,
    name: &str,
    mode: &str,
    command: Option<&str>,
    file: Option<&str>,
) -> Result<()> {
    use crate::config::passwords;
    use crate::config::schema::PasswordSource;

    let Some(account) = ctx.config().accounts.get(name) else {
        bail!("unknown account {name:?}");
    };
    let was_file = account.password_source == PasswordSource::File;
    let old_pw_path = passwords::resolve_path(ctx.config_dir(), account);
    let auth_path = if account.auth_file.is_absolute() {
        account.auth_file.clone()
    } else {
        ctx.config_dir().join(&account.auth_file)
    };
    let source_key = format!("accounts.{name}.password_source");
    let command_key = format!("accounts.{name}.password_command");
    let file_key = format!("accounts.{name}.password_file");

    match mode {
        "prompt" | "env" => {
            write::edit_file(&ctx.config_file(), |content| {
                let content = write::set(content, &source_key, mode)?;
                let content = clear_if_set(&content, &command_key)?;
                clear_if_set(&content, &file_key)
            })?;
            if was_file {
                passwords::remove(&old_pw_path, name)?;
            }
            eprintln!("account {name:?}: password_source = {mode}");
        }
        "command" => {
            let Some(command) = command else {
                bail!("mode 'command' requires --command <CMD>");
            };
            // Verify before persisting: the command must yield a passphrase
            // that opens the auth file.
            let pass = crate::config::ctx::run_password_command(command).await?;
            crate::auth::Authenticator::load_file(&auth_path, Some(pass))
                .await
                .context("the command did not produce a working passphrase")?;

            // source + command are only valid together, so set them in one
            // step; a stale password_file is cleared.
            write::edit_file(&ctx.config_file(), |content| {
                let content = write::set_many(
                    content,
                    &[(&source_key, "command"), (&command_key, command)],
                )?;
                clear_if_set(&content, &file_key)
            })?;
            if was_file {
                passwords::remove(&old_pw_path, name)?;
            }
            eprintln!("account {name:?}: password_source = command (verified)");
        }
        "file" => {
            // The passphrase that currently opens the auth file (requires an
            // encrypted file).
            let passphrase = current_passphrase(ctx, name, account, &auth_path).await?;

            let target = match file {
                Some(path) => crate::naming::expand_tilde(std::path::Path::new(path)),
                None => passwords::default_path(ctx.config_dir()),
            };
            passwords::upsert(&target, name, &passphrase)?;

            write::edit_file(&ctx.config_file(), |content| {
                let content = write::set(content, &source_key, "file")?;
                let content = match file {
                    Some(path) => write::set(&content, &file_key, path)?,
                    None => clear_if_set(&content, &file_key)?,
                };
                clear_if_set(&content, &command_key)
            })?;
            // Relocating to a different file: drop the entry from the old one.
            if was_file && old_pw_path != target {
                passwords::remove(&old_pw_path, name)?;
            }
            eprintln!(
                "account {name:?}: password_source = file ({})",
                target.display()
            );
        }
        _ => unreachable!("the mode value_parser restricts this"),
    }
    Ok(())
}

/// The passphrase that currently opens `auth_path`, for moving an account
/// onto `password_source = "file"`. Requires an encrypted file: uses the
/// account's current source non-interactively, otherwise prompts.
async fn current_passphrase(
    ctx: &Ctx,
    name: &str,
    account: &crate::config::schema::Account,
    auth_path: &std::path::Path,
) -> Result<SecretString> {
    use crate::auth::Authenticator;

    // Non-interactive first: the account's configured source, else the env
    // fallback the other maintenance commands honor.
    let candidate =
        match crate::config::ctx::resolve_password(ctx.config_dir(), name, account).await? {
            Some(pass) => Some(pass),
            None => env_current_password(name),
        };
    if let Some(pass) = candidate {
        Authenticator::load_file(
            auth_path,
            Some(SecretString::from(pass.expose_secret().to_owned())),
        )
        .await
        .context("the account's current passphrase did not open the auth file")?;
        return Ok(pass);
    }

    // `prompt` source with no env override: an unencrypted file has no
    // passphrase to store.
    match Authenticator::load_file(auth_path, None).await {
        Ok(_) => bail!(
            "auth file of {name:?} is unencrypted — encrypt it first with \
             `account password set {name}` before using password_source = file"
        ),
        Err(error) if crate::commands::password_required(&error) => {
            let term = console::Term::stderr();
            term.write_str(&format!("Current password for account {name:?}: "))?;
            let pass = SecretString::from(term.read_secure_line()?);
            Authenticator::load_file(
                auth_path,
                Some(SecretString::from(pass.expose_secret().to_owned())),
            )
            .await
            .context("could not open the auth file with this password")?;
            Ok(pass)
        }
        Err(error) => {
            Err(anyhow::Error::new(error)
                .context(format!("could not load {}", auth_path.display())))
        }
    }
}
