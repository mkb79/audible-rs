//! `audible account` — account management: `import`/`login` (bring an
//! account over or register a fresh one) and `logout` (deregister the
//! device with Amazon and delete it locally), plus `list`, `remove`,
//! `set-default`, `rename`, `marketplaces`, `password`, `cookies`, `token`,
//! `activation-bytes`, `widevine`, `export` and `unlock`/`lock`. This
//! module holds the clap tree, dispatch and the shared registration tail;
//! the subcommand bodies live in the sibling files (login/manage/
//! credentials/password).

use anyhow::{Context as _, Result, bail};
use clap::{Args, FromArgMatches};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::activation::ActivationMethod;
use crate::auth::AccountOrigin;
use crate::auth::Authenticator;
use crate::auth::authfile::KdfParams;
use crate::config::ctx::Ctx;
use crate::config::schema::validate_name;
use crate::config::write;

mod login;
mod manage;
mod material;
mod password;
mod status;

use login::{LoginArgs, ServerArgs, login, login_server};
use manage::{
    ExportArgs, LogoutArgs, RemoveArgs, export, list, lock, logout, marketplaces_add,
    marketplaces_remove, marketplaces_set_default, remove, rename, set_default, unlock,
};
use material::{
    activation_bytes, cookies_refresh, cookies_remove, cookies_status, parse_activation_method,
    token_refresh, token_remove, token_status, widevine_fetch, widevine_set,
};
use password::{password_remove, password_set, password_source};
use status::status;

/// `audible account`.
pub struct AccountCommand;

#[async_trait::async_trait]
impl super::Command for AccountCommand {
    fn name(&self) -> &'static str {
        "account"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Manage accounts")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(ImportArgs::augment_args(
                clap::Command::new("import")
                    .about("Convert a legacy Python auth file and register it as an account"),
            ))
            .subcommand(
                // `about`/`long_about` are set after `augment_args` so they win
                // over the args struct's doc comment.
                LoginArgs::augment_args(clap::Command::new("login"))
                    .about(
                        "Register a new account via a live sign-in (scripted by default; \
                         --external for the browser flow; pick the marketplace with -m <cc>)",
                    )
                    .long_about(
                        "Register a new account by signing in live. Default: the scripted \
                         (internal) login that prompts for email + password here; --external \
                         prints a sign-in URL for your own browser instead.\n\n\
                         A sign-in registers ONE device on a SINGLE marketplace — the same \
                         Audible account (user_id) is shared across all marketplaces, so \
                         registering per marketplace is unnecessary.\n\n\
                         -m <cc> picks that single registration marketplace (required). \
                         --marketplaces lists every marketplace you own audiobooks on; it is \
                         saved on the account for later data commands (e.g. `library sync -m \
                         all`) and adds no extra registrations. --default-marketplaces is the \
                         subset that -m defaults to when omitted later. Both default to the \
                         registration marketplace.",
                    )
                    .subcommand(
                        ServerArgs::augment_args(clap::Command::new("server"))
                            .about(
                                "Log in through a local browser-proxy server \
                                 (headless-friendly; open the printed URL or scan the QR \
                                 with a phone)",
                            )
                            .long_about(
                                "Log in from a real browser through a local reverse proxy \
                                 (headless-friendly; scan the printed QR from a phone). A \
                                 small config page shown first lets you pick marketplace, \
                                 device, account name and the unprotected-file option.\n\n\
                                 A sign-in registers ONE device on a SINGLE marketplace — the \
                                 same Audible account (user_id) is shared across all \
                                 marketplaces.\n\n\
                                 -m <cc> pre-selects the marketplace on that config page (you \
                                 confirm or change it there). --marketplaces lists every \
                                 marketplace you own audiobooks on; it is saved on the account \
                                 for later data commands and adds no extra registrations. \
                                 --default-marketplaces is the subset that -m defaults to when \
                                 omitted later. Both default to the chosen marketplace.",
                            ),
                    ),
            )
            .subcommand(LogoutArgs::augment_args(
                clap::Command::new("logout").about(
                    "Deregister the selected account's device with Amazon, then \
                     delete it locally (config entry + auth file)",
                ),
            ))
            .subcommand(clap::Command::new("list").about("List accounts"))
            .subcommand(
                clap::Command::new("status")
                    .about("Show the account's membership status")
                    .long_about(
                        "Show the Audible membership status for the selected \
                         marketplace(s): plan, whether it renews or was cancelled, the \
                         next-bill / end date, member-since date and account segment. \
                         Read-only; one row per marketplace.",
                    ),
            )
            .subcommand(
                // about/long_about set AFTER augment_args so the ExportArgs
                // struct doc comment does not override them.
                ExportArgs::augment_args(clap::Command::new("export"))
                    .about("Export the selected account's auth material to a file (uses -a)")
                    .long_about(
                        "Export the selected account's auth material to a file (uses -a).\n\n\
                         Formats:\n  \
                         encrypted  new-format envelope, freshly encrypted (default)\n  \
                         plain      new-format JSON, UNENCRYPTED\n  \
                         python     legacy audible/audible-cli layout, UNENCRYPTED — \
                         usable with the Python tools; cookies are flattened to the \
                         account marketplace's domain",
                    ),
            )
            .subcommand(RemoveArgs::augment_args(
                clap::Command::new("remove")
                    .about("Remove an account from the config (does not deregister the device)"),
            ))
            .subcommand(
                clap::Command::new("set-default")
                    .about("Make an account the default (top-level default_account)")
                    .arg(
                        clap::Arg::new("name")
                            .required(true)
                            .value_name("ACCOUNT")
                            .help("Account to make the default"),
                    ),
            )
            .subcommand(
                clap::Command::new("rename")
                    .about(
                        "Rename an account in the config (repoints default_account; \
                         the auth file is left in place)",
                    )
                    .arg(
                        clap::Arg::new("old")
                            .required(true)
                            .value_name("OLD")
                            .help("Current account name"),
                    )
                    .arg(
                        clap::Arg::new("new")
                            .required(true)
                            .value_name("NEW")
                            .help("New account name"),
                    ),
            )
            // Session-agent pair, after the config-editing commands.
            .subcommand(
                clap::Command::new("unlock")
                    .about("Unlock an account into the running session agent (uses -a/--account)")
                    .long_about(
                        "Hand an account's decrypted session to the running agent so it \
                         serves requests without the passphrase. The passphrase comes from \
                         the account's password_source, or is prompted for. Needs \
                         `audible agent start` first.",
                    ),
            )
            .subcommand(
                clap::Command::new("lock")
                    .about("Lock account session(s) held by the agent (uses -a/--account)")
                    .arg(
                        clap::Arg::new("all")
                            .long("all")
                            .action(clap::ArgAction::SetTrue)
                            .help("Lock every account, not just the selected one"),
                    ),
            )
            .subcommand(
                clap::Command::new("marketplaces")
                    .about("Manage an account's marketplace set (uses -a/--account)")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("add")
                            .about("Add a marketplace to the account")
                            .arg(
                                clap::Arg::new("cc")
                                    .required(true)
                                    .value_name("CC")
                                    .help("Marketplace country code (e.g. de, us, uk)"),
                            )
                            .arg(
                                clap::Arg::new("default")
                                    .long("default")
                                    .action(clap::ArgAction::SetTrue)
                                    .help("Also add it to the account's default marketplace set (used when -m is omitted)"),
                            ),
                    )
                    .subcommand(
                        clap::Command::new("remove")
                            .about("Remove a marketplace from the account")
                            .arg(
                                clap::Arg::new("cc")
                                    .required(true)
                                    .value_name("CC")
                                    .help("Marketplace country code (e.g. de, us, uk)"),
                            ),
                    )
                    .subcommand(
                        clap::Command::new("set-default")
                            .about("Set the account's default marketplace set")
                            .arg(
                                clap::Arg::new("csv")
                                    .required(true)
                                    .value_name("CC,...")
                                    .help("Comma-separated country codes (subset of the account's marketplaces)"),
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("password")
                    .about("Manage the auth file password (uses -a/--account)")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("set")
                            .about("Encrypt the auth file or change its password"),
                    )
                    .subcommand(
                        clap::Command::new("remove")
                            .about("Store the auth file UNENCRYPTED (not recommended)")
                            .arg(crate::commands::yes_arg()),
                    )
                    .subcommand(
                        clap::Command::new("source")
                            .about(
                                "Choose where the auth-file passphrase comes from \
                                 (prompt|env|command|file)",
                            )
                            .long_about(
                                "Choose where the auth-file passphrase comes from.\n\n\
                                 Modes:\n  \
                                 prompt   ask interactively (default; nothing stored)\n  \
                                 env      AUDIBLE_AUTH_PASSWORD[_<NAME>]\n  \
                                 command  stdout of --command is the passphrase\n  \
                                 file     read from the passwords file (--file or \
                                 <config_dir>/passwords), keyed by account\n\n\
                                 'file' writes the current passphrase into the passwords \
                                 file (0600) and requires an already-encrypted auth file. \
                                 Switching away from 'file' removes the entry.",
                            )
                            .arg(
                                clap::Arg::new("mode")
                                    .required(true)
                                    .value_name("MODE")
                                    .value_parser(["prompt", "env", "command", "file"])
                                    .help("Where the passphrase comes from"),
                            )
                            .arg(
                                clap::Arg::new("command")
                                    .long("command")
                                    .value_name("CMD")
                                    .help("Command whose stdout is the passphrase (mode=command)"),
                            )
                            .arg(
                                clap::Arg::new("file")
                                    .long("file")
                                    .value_name("PATH")
                                    .help(
                                        "Passwords file (mode=file; default \
                                         <config_dir>/passwords)",
                                    ),
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("cookies")
                    .about("Inspect and refresh domain-bound website cookies")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("status").about("Show per-domain cookie status"),
                    )
                    .subcommand(
                        clap::Command::new("refresh")
                            .about(
                                "Refresh website cookies for the selected marketplace(s) (kept alongside existing domains; use -m)",
                            )
                            .arg(
                                clap::Arg::new("show-response")
                                    .long("show-response")
                                    .action(clap::ArgAction::SetTrue)
                                    .help("Print the raw refresh response (contains cookie values)"),
                            ),
                    )
                    .subcommand(
                        clap::Command::new("remove")
                            .about("Remove stored website cookies")
                            .arg(
                                clap::Arg::new("domain")
                                    .long("domain")
                                    .value_name("DOMAIN")
                                    .help("Cookie domain to remove (e.g. .amazon.de)"),
                            )
                            .arg(
                                clap::Arg::new("all")
                                    .long("all")
                                    .action(clap::ArgAction::SetTrue)
                                    .help("Remove cookies for all domains"),
                            )
                            .group(
                                clap::ArgGroup::new("target")
                                    .args(["domain", "all"])
                                    .required(true),
                            ),
                    ),
            )
            .subcommand(
                clap::Command::new("token")
                    .about("Inspect and manage the account's access token")
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("status")
                            .about("Show the access token's remaining validity"),
                    )
                    .subcommand(
                        clap::Command::new("refresh")
                            .about("Force an access-token refresh via the refresh token"),
                    )
                    .subcommand(clap::Command::new("remove").about(
                        "Remove the stored access token (forces a refresh on next use; \
                         the refresh token is kept)",
                    )),
            )
            .subcommand(
                clap::Command::new("activation-bytes")
                    .about(
                        "Show or fetch the account's activation bytes (legacy .aax \
                         decryption key; aaxc uses a per-title key/iv instead)",
                    )
                    .arg(
                        clap::Arg::new("fetch")
                            .long("fetch")
                            .action(clap::ArgAction::SetTrue)
                            .help(
                                "Fetch the activation bytes from Audible and store them in \
                                 the auth file (even if one is already stored)",
                            ),
                    )
                    .arg(
                        clap::Arg::new("method")
                            .long("method")
                            .value_name("METHOD")
                            .value_parser(parse_activation_method)
                            .requires("fetch")
                            .help("Force the fetch method: signing or cookies (default: auto)"),
                    ),
            )
            .subcommand(
                clap::Command::new("widevine")
                    .about(
                        "Manage the account's Widevine CDM (.wvd) for the Widevine/DASH \
                         download path (needs an Android-registered account)",
                    )
                    .subcommand_required(true)
                    .arg_required_else_help(true)
                    .subcommand(
                        clap::Command::new("fetch")
                            .about(
                                "Fetch a CDM from a provider URL, save it and set widevine_cdm",
                            )
                            .arg(
                                clap::Arg::new("url")
                                    .required(true)
                                    .value_name("URL")
                                    .help("CDM provider endpoint (e.g. a community AudibleCdm service)"),
                            ),
                    )
                    .subcommand(
                        clap::Command::new("set")
                            .about("Use an existing local .wvd file (BYO): set widevine_cdm")
                            .arg(
                                clap::Arg::new("path")
                                    .required(true)
                                    .value_name("PATH")
                                    .help("Path to the local .wvd file"),
                            ),
                    ),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let name = |m: &clap::ArgMatches| m.get_one::<String>("name").expect("required").clone();
        match matches.subcommand() {
            Some(("import", sub)) => import(ctx, ImportArgs::from_arg_matches(sub)?).await,
            Some(("login", sub)) => match sub.subcommand() {
                Some(("server", server)) => {
                    login_server(ctx, ServerArgs::from_arg_matches(server)?).await
                }
                _ => login(ctx, LoginArgs::from_arg_matches(sub)?).await,
            },
            Some(("logout", sub)) => logout(ctx, LogoutArgs::from_arg_matches(sub)?).await,
            Some(("list", _)) => list(ctx),
            Some(("status", _)) => status(ctx).await,
            Some(("export", sub)) => export(ctx, ExportArgs::from_arg_matches(sub)?).await,
            Some(("remove", sub)) => remove(ctx, RemoveArgs::from_arg_matches(sub)?),
            Some(("set-default", sub)) => set_default(ctx, &name(sub)),
            Some(("unlock", _)) => unlock(ctx).await,
            Some(("lock", sub)) => lock(ctx, sub.get_flag("all")).await,
            Some(("rename", sub)) => rename(
                ctx,
                sub.get_one::<String>("old").expect("required"),
                sub.get_one::<String>("new").expect("required"),
            ),
            Some(("marketplaces", sub)) => match sub.subcommand() {
                Some(("add", sub)) => marketplaces_add(
                    ctx,
                    sub.get_one::<String>("cc").expect("required"),
                    sub.get_flag("default"),
                ),
                Some(("remove", sub)) => {
                    marketplaces_remove(ctx, sub.get_one::<String>("cc").expect("required"))
                }
                Some(("set-default", sub)) => {
                    marketplaces_set_default(ctx, sub.get_one::<String>("csv").expect("required"))
                }
                _ => unreachable!("subcommand required"),
            },
            Some(("password", sub)) => match sub.subcommand() {
                Some(("set", _)) => password_set(ctx, &ctx.account_name()?).await,
                Some(("remove", sub)) => {
                    password_remove(ctx, &ctx.account_name()?, sub.get_flag("yes")).await
                }
                Some(("source", sub)) => {
                    password_source(
                        ctx,
                        &ctx.account_name()?,
                        sub.get_one::<String>("mode").expect("required"),
                        sub.get_one::<String>("command").map(String::as_str),
                        sub.get_one::<String>("file").map(String::as_str),
                    )
                    .await
                }
                _ => unreachable!("subcommand required"),
            },
            Some(("cookies", sub)) => match sub.subcommand() {
                Some(("status", _)) => cookies_status(ctx).await,
                Some(("refresh", sub)) => cookies_refresh(ctx, sub.get_flag("show-response")).await,
                Some(("remove", sub)) => {
                    cookies_remove(ctx, sub.get_one::<String>("domain").map(String::as_str)).await
                }
                _ => unreachable!("subcommand required"),
            },
            Some(("token", sub)) => match sub.subcommand() {
                Some(("status", _)) => token_status(ctx).await,
                Some(("refresh", _)) => token_refresh(ctx).await,
                Some(("remove", _)) => token_remove(ctx).await,
                _ => unreachable!("subcommand required"),
            },
            Some(("widevine", sub)) => match sub.subcommand() {
                Some(("fetch", f)) => {
                    widevine_fetch(ctx, f.get_one::<String>("url").expect("required")).await
                }
                Some(("set", s)) => {
                    widevine_set(ctx, s.get_one::<String>("path").expect("required")).await
                }
                _ => unreachable!("subcommand required"),
            },
            Some(("activation-bytes", sub)) => {
                activation_bytes(
                    ctx,
                    sub.get_flag("fetch"),
                    sub.get_one::<ActivationMethod>("method")
                        .copied()
                        .unwrap_or_default(),
                )
                .await
            }
            _ => unreachable!("subcommand required"),
        }
    }
}

/// Convert a legacy Python auth file and register it as an account
#[derive(Debug, Args)]
struct ImportArgs {
    /// Legacy Python auth file (plain, json- or bytes-encrypted)
    file: std::path::PathBuf,

    /// Account name (default: asked interactively, derived from the
    /// file name)
    #[arg(long)]
    name: Option<String>,

    /// Marketplaces this account may use (comma-separated country codes;
    /// default: the marketplace the account was registered in).
    #[arg(long, value_name = "CC,...")]
    marketplaces: Option<String>,

    /// Default marketplace set used when -m/--marketplace is omitted
    /// (comma-separated; must be a subset of --marketplaces; default: the
    /// first --marketplaces entry).
    #[arg(long, value_name = "CC,...")]
    default_marketplaces: Option<String>,

    /// Write the new auth file unencrypted — anyone with file access can
    /// read your tokens; not recommended
    #[arg(long)]
    plain: bool,

    /// Overwrite an existing auth file and config entry
    #[arg(long)]
    force: bool,
}

async fn import(ctx: &Ctx, args: ImportArgs) -> Result<()> {
    // The input password (for encrypted files) comes from
    // AUDIBLE_AUTH_PASSWORD or a prompt inside load_import_input.
    let auth = super::load_import_input(&args.file).await?;
    let plain = args.plain;
    let (_, target) = finalize_account(
        ctx,
        Registration {
            auth,
            name: args.name,
            default_name: derive_name(&args.file),
            marketplaces: args.marketplaces,
            default_marketplaces: args.default_marketplaces,
            plain,
            force: args.force,
        },
    )
    .await?;
    println!(
        "Imported {} -> {} ({}). The original file is unchanged.",
        args.file.display(),
        target.display(),
        if plain { "plain" } else { "encrypted" },
    );
    Ok(())
}

/// Inputs to [`finalize_account`], the shared tail of `import` and `login`.
pub(super) struct Registration {
    pub(super) auth: Authenticator,
    pub(super) name: Option<String>,
    pub(super) default_name: String,
    pub(super) marketplaces: Option<String>,
    pub(super) default_marketplaces: Option<String>,
    pub(super) plain: bool,
    pub(super) force: bool,
}

/// Saves the auth envelope and registers the account in the config — the part
/// `import` and `login` share. Resolves the marketplace axis (AUD-39), writes
/// the (optionally encrypted) auth file, updates `config.toml`, prints the
/// registration summary and a merge hint for pre-merger accounts. Returns the
/// resolved account name and the auth-file path.
pub(super) async fn finalize_account(
    ctx: &Ctx,
    reg: Registration,
) -> Result<(String, std::path::PathBuf)> {
    let name = match reg.name {
        Some(name) => name,
        None => prompt_with_default("Account name", &reg.default_name)?,
    };
    validate_name(&name)?;
    if ctx.config().accounts.contains_key(&name) && !reg.force {
        bail!("account {name:?} already exists (use --force to replace it)");
    }

    let auth_file_name = format!("{name}.auth");
    let target = ctx.config_dir().join(&auth_file_name);
    if target.exists() && !reg.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            target.display()
        );
    }

    // Resolve the marketplace axis up front (before any disk write) so an
    // invalid `--marketplaces`/`--default-marketplaces` fails cleanly. The
    // available set defaults to the registration marketplace; the default set
    // defaults to the first available marketplace and must be a subset.
    let marketplaces = match &reg.marketplaces {
        Some(raw) => parse_marketplaces(raw)?,
        None => vec![reg.auth.locale().country_code.to_owned()],
    };
    let default_marketplaces = match &reg.default_marketplaces {
        Some(raw) => {
            let set = parse_marketplaces(raw)?;
            for cc in &set {
                if !marketplaces.contains(cc) {
                    bail!(
                        "--default-marketplaces entry {cc:?} is not in --marketplaces ({})",
                        marketplaces.join(",")
                    );
                }
            }
            set
        }
        None => vec![marketplaces[0].clone()],
    };

    let password = if reg.plain {
        eprintln!("warning: writing the auth file UNENCRYPTED (--plain)");
        None
    } else {
        Some(prompt_new_password()?)
    };

    crate::fsutil::create_private_dir(ctx.config_dir())
        .with_context(|| format!("could not create {}", ctx.config_dir().display()))?;
    reg.auth
        .save_to(&target, password, KdfParams::default())
        .await?;

    // Register the account in the config; every step is schema-validated
    // before it reaches disk.
    let make_default = ctx.config().default_account.is_none();
    let auth_file_key = format!("accounts.{name}.auth_file");
    let marketplaces_key = format!("accounts.{name}.marketplaces");
    let default_marketplaces_key = format!("accounts.{name}.default_marketplaces");
    write::edit_file(&ctx.config_file(), |content| {
        let content = write::set(content, &auth_file_key, &auth_file_name)?;
        let content = write::set_array(&content, &marketplaces_key, &marketplaces)?;
        let content = write::set_array(&content, &default_marketplaces_key, &default_marketplaces)?;
        if make_default {
            write::set(&content, "default_account", &name)
        } else {
            Ok(content)
        }
    })?;

    println!(
        "Registered account {name:?} (marketplaces {}; default {}){}.",
        marketplaces.join(", "),
        default_marketplaces.join(", "),
        if make_default {
            ", set as default account"
        } else {
            ""
        }
    );
    if reg.auth.origin() == AccountOrigin::AudibleLegacy {
        println!(
            "\nNote: this is a pre-merger Audible account (it signs in with \
             an Audible username instead of an Amazon identity). You can \
             merge it with an Amazon account; see\n\
             https://help.audible.ca/s/article/merge-audible-and-amazon-accounts"
        );
    }
    Ok((name, target))
}

/// Parses a comma-separated marketplace list: lowercased, validated
/// against the known marketplaces, de-duplicated, order preserved.
pub(super) fn parse_marketplaces(raw: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let cc = token.to_ascii_lowercase();
        if crate::api::locale::find(&cc).is_none() {
            bail!("unknown marketplace {cc:?} (e.g. de, us, uk, fr, ca, it, au, in, jp, es, br)");
        }
        if !out.contains(&cc) {
            out.push(cc);
        }
    }
    if out.is_empty() {
        bail!("no marketplace given");
    }
    Ok(out)
}

/// Diagnoses the on-disk state of an auth file without exposing any of
/// its content (only the format marker is inspected).
pub(super) fn auth_file_state(ctx: &Ctx, auth_file: &std::path::Path) -> &'static str {
    let path = if auth_file.is_absolute() {
        auth_file.to_path_buf()
    } else {
        ctx.config_dir().join(auth_file)
    };
    let Ok(content) = std::fs::read(&path) else {
        return "MISSING";
    };
    match serde_json::from_slice::<serde_json::Value>(&content) {
        Ok(value) if value.get("ciphertext").is_some() => "encrypted",
        Ok(value) if value.get("data").is_some() => "plain",
        _ => "INVALID",
    }
}

/// Non-interactive current-password fallback for the maintenance commands:
/// `AUDIBLE_AUTH_PASSWORD_<NAME>` (uppercased, `-`→`_`), then
/// `AUDIBLE_AUTH_PASSWORD`.
pub(super) fn env_current_password(name: &str) -> Option<SecretString> {
    let specific = format!(
        "AUDIBLE_AUTH_PASSWORD_{}",
        name.to_ascii_uppercase().replace('-', "_")
    );
    std::env::var(&specific)
        .or_else(|_| std::env::var("AUDIBLE_AUTH_PASSWORD"))
        .ok()
        .map(SecretString::from)
}

pub(super) async fn load_account_auth(
    ctx: &Ctx,
    name: &str,
) -> Result<(crate::auth::Authenticator, std::path::PathBuf, &'static str)> {
    use crate::auth::Authenticator;

    let Some(account) = ctx.config().accounts.get(name) else {
        bail!("unknown account {name:?}");
    };
    let path = if account.auth_file.is_absolute() {
        account.auth_file.clone()
    } else {
        ctx.config_dir().join(&account.auth_file)
    };
    let state = auth_file_state(ctx, &account.auth_file);

    // Honor the account's password_source (prompt | env | command | file),
    // the same resolver the data commands use; only `prompt` falls back to
    // an interactive prompt if the file turns out to be encrypted.
    let resolved = crate::config::ctx::resolve_password(ctx.config_dir(), name, account).await?;
    let prompt_allowed = account.password_source == crate::config::schema::PasswordSource::Prompt;

    // For a prompt-source account, keep the historical non-interactive
    // convenience of the maintenance commands: AUDIBLE_AUTH_PASSWORD[_<NAME>]
    // is tried before the interactive prompt.
    let resolved = match resolved {
        some @ Some(_) => some,
        None if prompt_allowed => env_current_password(name),
        none => none,
    };

    let auth = match Authenticator::load_file(&path, resolved).await {
        Ok(auth) => auth,
        Err(error) if super::password_required(&error) && prompt_allowed => {
            let term = console::Term::stderr();
            term.write_str(&format!("Current password for account {name:?}: "))?;
            let password = SecretString::from(term.read_secure_line()?);
            Authenticator::load_file(&path, Some(password))
                .await
                .context("could not open the auth file with this password")?
        }
        Err(error) => {
            return Err(
                anyhow::Error::new(error).context(format!("could not load {}", path.display()))
            );
        }
    };
    Ok((auth, path, state))
}

/// Default account name from the legacy file name (`alice.json` →
/// `alice`), mapped onto the allowed name alphabet.
fn derive_name(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let derived: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if derived.is_empty() {
        "main".to_owned()
    } else {
        derived
    }
}

use super::prompt::{prompt_required, prompt_secret, prompt_with_default};

/// Prompts twice for the password protecting the converted file.
pub(super) fn prompt_new_password() -> Result<SecretString> {
    let term = console::Term::stderr();
    loop {
        term.write_str("New password for the converted auth file: ")?;
        let first = Zeroizing::new(term.read_secure_line()?);
        if first.is_empty() {
            term.write_line("Password must not be empty (use --plain for an unencrypted file).")?;
            continue;
        }
        term.write_str("Repeat password: ")?;
        let second = Zeroizing::new(term.read_secure_line()?);
        if *first == *second {
            return Ok(SecretString::from(first.to_string()));
        }
        term.write_line("Passwords do not match; try again.")?;
    }
}

/// The Android device type — Widevine is only granted to it (AUD-56).
const ANDROID_DEVICE_TYPE: &str = "A10KISP2GWF0E4";
