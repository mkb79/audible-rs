//! `audible settings` — reusable settings bundles (`[settings.<name>]`).
//! A bundle holds per-value download options, bound to neither an account
//! nor a marketplace; `settings.default` is the fallback every other
//! bundle inherits unset fields from. All subcommands are pure config
//! operations; arbitrary keys can also be set via `audible config set
//! settings.<name>.<key>`.

use anyhow::{Result, bail};
use clap::Arg;

use crate::config::ctx::Ctx;
use crate::config::schema::validate_name;
use crate::config::write;
use crate::output::Output;

/// `audible settings`.
pub struct SettingsCommand;

/// Scalar per-value fields a bundle can carry, as
/// `(flag, config key, value name, help)`.
const SCALAR_FIELDS: [(&str, &str, &str, &str); 5] = [
    (
        "download-dir",
        "download_dir",
        "DIR",
        "Download directory used with this bundle",
    ),
    (
        "filename-mode",
        "filename_mode",
        "MODE",
        "Filename scheme: ascii, unicode, asin_ascii, asin_unicode or custom",
    ),
    (
        "filename-template",
        "filename_template",
        "TEMPLATE",
        "Custom filename template (used when filename_mode = custom; variables in `download --help`)",
    ),
    (
        "overwrite",
        "overwrite",
        "POLICY",
        "Overwrite policy for downloads: skip (default) or force",
    ),
    (
        "filename-max-length",
        "filename_max_length",
        "N",
        "Maximum filename length in bytes (0 = no limit)",
    ),
];

/// Multi-valued fields, written as TOML arrays (CLI value is CSV), as
/// `(flag, config key, value name, help)`.
const ARRAY_FIELDS: [(&str, &str, &str, &str); 2] = [
    (
        "cover-size",
        "cover_size",
        "PX,...",
        "Cover size(s) in px to download, comma-separated (e.g. 500,1215)",
    ),
    (
        "chapter-type",
        "chapter_type",
        "TYPE,...",
        "Chapter title layout(s): flat, tree or both, comma-separated",
    ),
];

#[async_trait::async_trait]
impl super::Command for SettingsCommand {
    fn name(&self) -> &'static str {
        "settings"
    }

    fn clap(&self) -> clap::Command {
        let name = |help: &'static str| {
            Arg::new("name")
                .required(true)
                .value_name("BUNDLE")
                .help(help)
        };
        let mut add = clap::Command::new("add")
            .about("Add a settings bundle (give at least one field)")
            .arg(name("Name of the new bundle"));
        for (flag, _, value_name, help) in SCALAR_FIELDS.iter().chain(ARRAY_FIELDS.iter()) {
            add = add.arg(
                Arg::new(*flag)
                    .long(*flag)
                    .value_name(*value_name)
                    .help(*help),
            );
        }
        clap::Command::new(self.name())
            .about("Manage reusable settings bundles")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(add)
            .subcommand(clap::Command::new("list").about("List settings bundles"))
            .subcommand(
                clap::Command::new("show")
                    .about("Show a settings bundle")
                    .arg(name("Bundle to show")),
            )
            .subcommand(
                clap::Command::new("remove")
                    .about("Remove a settings bundle")
                    .arg(name("Bundle to remove")),
            )
            .subcommand(
                clap::Command::new("set-default")
                    .about("Make a bundle the selected account's default_settings (uses -a)")
                    .arg(name("Bundle to make the account's default")),
            )
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        let name = |m: &clap::ArgMatches| m.get_one::<String>("name").expect("required").clone();
        match matches.subcommand() {
            Some(("add", sub)) => {
                add(ctx, sub)?;
                if ["filename-mode", "filename-template", "download-dir"]
                    .iter()
                    .any(|flag| sub.contains_id(flag))
                {
                    crate::commands::download::hint_reorganize(ctx).await;
                }
                Ok(())
            }
            Some(("list", _)) => list(ctx),
            Some(("show", sub)) => show(ctx, &name(sub)),
            Some(("remove", sub)) => remove(ctx, &name(sub)),
            Some(("set-default", sub)) => set_default(ctx, &name(sub)),
            _ => unreachable!("subcommand required"),
        }
    }
}

fn add(ctx: &Ctx, sub: &clap::ArgMatches) -> Result<()> {
    let name = sub.get_one::<String>("name").expect("required");
    validate_name(name)?;
    if ctx.config().settings.contains_key(name) {
        bail!("settings bundle {name:?} already exists");
    }

    // Scalar fields go through set_many; multi-valued fields are split on
    // commas and written as TOML arrays.
    let scalars: Vec<(String, String)> = SCALAR_FIELDS
        .iter()
        .filter_map(|(flag, key, _, _)| {
            sub.get_one::<String>(flag)
                .map(|value| (format!("settings.{name}.{key}"), value.clone()))
        })
        .collect();
    let arrays: Vec<(String, Vec<String>)> = ARRAY_FIELDS
        .iter()
        .filter_map(|(flag, key, _, _)| {
            sub.get_one::<String>(flag).map(|value| {
                // Same CSV rule as `setup` and the other flags — one home
                // (D3); both feed the same `settings.<name>.<key>`.
                let items = crate::commands::split_csv(value);
                (format!("settings.{name}.{key}"), items)
            })
        })
        .collect();
    if scalars.is_empty() && arrays.is_empty() {
        bail!(
            "give at least one field (e.g. --download-dir), or use \
             `audible config set settings.{name}.<key> <value>`"
        );
    }
    write::edit_file(&ctx.config_file(), |content| {
        let entries: Vec<(&str, &str)> = scalars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut content = write::set_many(content, &entries)?;
        for (key, items) in &arrays {
            content = write::set_array(&content, key, items)?;
        }
        Ok(content)
    })?;
    eprintln!("added settings bundle {name:?}");
    Ok(())
}

fn list(ctx: &Ctx) -> Result<()> {
    let config = ctx.config();
    if config.settings.is_empty() {
        eprintln!("no settings bundles configured — add one with `audible settings add <name>`");
    }
    let rows = config
        .settings
        .iter()
        .map(|(name, settings)| {
            vec![
                name.clone(),
                settings
                    .download_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default(),
                crate::commands::enum_str(&settings.filename_mode).unwrap_or_default(),
                crate::commands::enum_str(&settings.overwrite).unwrap_or_default(),
                settings.cover_size.as_deref().unwrap_or_default().join(","),
                settings
                    .chapter_type
                    .as_deref()
                    .unwrap_or_default()
                    .join(","),
            ]
        })
        .collect();
    ctx.print(&Output::table(
        vec![
            "name",
            "download_dir",
            "filename_mode",
            "overwrite",
            "cover_size",
            "chapter_type",
        ],
        rows,
    ));
    Ok(())
}

fn show(ctx: &Ctx, name: &str) -> Result<()> {
    let Some(settings) = ctx.config().settings.get(name) else {
        bail!("unknown settings bundle {name:?}");
    };
    let mut pairs = vec![("name".to_owned(), name.to_owned())];
    if let Some(dir) = &settings.download_dir {
        pairs.push(("download_dir".to_owned(), dir.display().to_string()));
    }
    let filename_mode = crate::commands::enum_str(&settings.filename_mode).unwrap_or_default();
    if !filename_mode.is_empty() {
        pairs.push(("filename_mode".to_owned(), filename_mode));
    }
    if let Some(template) = &settings.filename_template {
        pairs.push(("filename_template".to_owned(), template.clone()));
    }
    let overwrite = crate::commands::enum_str(&settings.overwrite).unwrap_or_default();
    if !overwrite.is_empty() {
        pairs.push(("overwrite".to_owned(), overwrite));
    }
    if let Some(sizes) = &settings.cover_size {
        pairs.push(("cover_size".to_owned(), sizes.join(",")));
    }
    if let Some(chapters) = &settings.chapter_type {
        pairs.push(("chapter_type".to_owned(), chapters.join(",")));
    }
    if let Some(max) = settings.filename_max_length {
        pairs.push(("filename_max_length".to_owned(), max.to_string()));
    }
    ctx.print(&Output::KeyValue(pairs));
    Ok(())
}

fn remove(ctx: &Ctx, name: &str) -> Result<()> {
    if !ctx.config().settings.contains_key(name) {
        bail!("unknown settings bundle {name:?}");
    }
    write::edit_file(&ctx.config_file(), |content| {
        write::unset(content, &format!("settings.{name}"))
    })?;
    eprintln!("removed settings bundle {name:?}");
    Ok(())
}

fn set_default(ctx: &Ctx, name: &str) -> Result<()> {
    if !ctx.config().settings.contains_key(name) {
        bail!("unknown settings bundle {name:?}");
    }
    let account = ctx.account_name()?;
    write::edit_file(&ctx.config_file(), |content| {
        write::set(
            content,
            &format!("accounts.{account}.default_settings"),
            name,
        )
    })?;
    eprintln!("default settings of account {account:?} is now {name:?}");
    Ok(())
}
