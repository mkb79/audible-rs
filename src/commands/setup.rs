//! `audible setup` — guided wizard for the default settings bundle
//! (`[settings.default]`) and the `[db]` section. Configures the
//! installation, not the accounts; repeatable, with the current values
//! as prefilled defaults. Writes go through the same validated path as
//! `config set`.

use anyhow::{Result, bail};

use crate::config::ctx::Ctx;
use crate::config::schema::parse_duration;
use crate::config::write;

use super::prompt::{prompt_choice, prompt_with_default};

/// `audible setup`.
pub struct SetupCommand;

#[async_trait::async_trait]
impl super::Command for SetupCommand {
    fn name(&self) -> &'static str {
        "setup"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name()).about("Configure installation-wide defaults interactively")
    }

    async fn run(&self, ctx: &Ctx, _matches: &clap::ArgMatches) -> Result<()> {
        setup(ctx)?;
        // Naming/dir may have changed → offer to migrate existing downloads.
        crate::commands::download::hint_reorganize(ctx).await;
        Ok(())
    }
}

fn setup(ctx: &Ctx) -> Result<()> {
    let term = console::Term::stderr();
    if !term.is_term() {
        bail!("setup is interactive and needs a terminal (use `audible config set` in scripts)");
    }

    eprintln!("audible setup — default settings bundle");
    eprintln!("config file: {}\n", ctx.config_file().display());

    let config = ctx.config();
    let default = config.settings.get("default").cloned().unwrap_or_default();

    let download_dir = prompt_with_default(
        "Download directory",
        &default
            .download_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "~/Audible".to_owned()),
    )?;

    let filename_mode = prompt_choice(
        "Filename mode",
        &["ascii", "unicode", "asin_ascii", "asin_unicode", "custom"],
        &enum_str(&default.filename_mode)?.unwrap_or_else(|| "ascii".to_owned()),
    )?;

    // Custom mode needs a template; ask for it and verify it right away so an
    // invalid template can never be written to the config.
    let filename_template = if filename_mode == "custom" {
        let value = loop {
            let value = prompt_with_default(
                "Filename template (e.g. %publication%/%fulltitle%)",
                default
                    .filename_template
                    .as_deref()
                    .unwrap_or("%publication%/%fulltitle%"),
            )?;
            if value.trim().is_empty() {
                eprintln!("the template must not be empty");
            } else if let Err(reason) = crate::config::filename_template::validate(&value) {
                eprintln!("invalid template: {reason}");
            } else {
                break value;
            }
        };
        Some(value)
    } else {
        None
    };

    let overwrite = prompt_choice(
        "Re-download already fetched artifacts",
        &["skip", "force"],
        &enum_str(&default.overwrite)?.unwrap_or_else(|| "skip".to_owned()),
    )?;

    let cover_size = prompt_with_default(
        "Default cover size(s) in px (comma-separated)",
        &default
            .cover_size
            .as_deref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "500".to_owned()),
    )?;

    let chapter_type = prompt_with_default(
        "Chapter title layout(s) (tree, flat, or both comma-separated)",
        &default
            .chapter_type
            .as_deref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "tree".to_owned()),
    )?;

    let filename_max_length = loop {
        let value = prompt_with_default(
            "Max file name length in bytes (0 = no limit)",
            &default
                .filename_max_length
                .unwrap_or(crate::config::resolve::DEFAULT_FILENAME_MAX_LENGTH)
                .to_string(),
        )?;
        match value.parse::<usize>() {
            Ok(_) => break value,
            Err(_) => eprintln!("please enter a whole number"),
        }
    };

    // Decrypt (AUD-27): surfaced here so users decide once. Default keeps the
    // source aaxc (non-destructive; big libraries can opt into removal).
    let decrypt = prompt_choice(
        "Decrypt downloads to a playable m4b",
        &["no", "yes"],
        if default.decrypt.unwrap_or(false) {
            "yes"
        } else {
            "no"
        },
    )? == "yes";
    let decrypt_keep_source = prompt_choice(
        "Keep the source aaxc after decrypt (no = delete it)",
        &["yes", "no"],
        if default.decrypt_keep_source.unwrap_or(true) {
            "yes"
        } else {
            "no"
        },
    )? == "yes";
    let decrypt_backend = prompt_choice(
        "Decrypt tool",
        &["auto", "aaxclean", "ffmpeg"],
        &enum_str(&default.decrypt_backend)?.unwrap_or_else(|| "auto".to_owned()),
    )?;

    let auto_sync = prompt_choice(
        "Library auto-sync before local reads",
        &["delta", "none"],
        &enum_str(&Some(config.db.auto_sync))?.expect("auto_sync always set"),
    )?;

    let sync_max_age = loop {
        let value = prompt_with_default("Sync max age (e.g. 6h, 30m)", &config.db.sync_max_age)?;
        match parse_duration(&value) {
            Ok(_) => break value,
            Err(error) => eprintln!("{error}"),
        }
    };

    let record_changes = prompt_choice(
        "Record library changes (added/changed/removed) for `library changes`",
        &["yes", "no"],
        if config.db.record_changes {
            "yes"
        } else {
            "no"
        },
    )?;
    let record_changes = if record_changes == "yes" {
        "true"
    } else {
        "false"
    };
    let change_retention_days = loop {
        let value = prompt_with_default(
            "Keep change history for how many days (0 = forever)",
            &config.db.change_retention_days.to_string(),
        )?;
        match value.trim().parse::<u32>() {
            Ok(_) => break value.trim().to_owned(),
            Err(_) => eprintln!("enter a whole number of days (e.g. 90)"),
        }
    };

    let split_csv = |value: &str| -> Vec<String> {
        value
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let cover_sizes = split_csv(&cover_size);
    let chapter_types = split_csv(&chapter_type);
    let decrypt = if decrypt { "true" } else { "false" };
    let decrypt_keep_source = if decrypt_keep_source { "true" } else { "false" };
    write::edit_file(&ctx.config_file(), |content| {
        let mut fields: Vec<(&str, &str)> = vec![
            ("settings.default.download_dir", download_dir.as_str()),
            ("settings.default.filename_mode", filename_mode.as_str()),
            ("settings.default.overwrite", overwrite.as_str()),
            (
                "settings.default.filename_max_length",
                filename_max_length.as_str(),
            ),
            ("settings.default.decrypt", decrypt),
            ("settings.default.decrypt_keep_source", decrypt_keep_source),
            ("settings.default.decrypt_backend", decrypt_backend.as_str()),
            ("db.auto_sync", auto_sync.as_str()),
            ("db.sync_max_age", sync_max_age.as_str()),
            ("db.record_changes", record_changes),
            ("db.change_retention_days", change_retention_days.as_str()),
        ];
        // Only written for the custom mode (verified above).
        if let Some(template) = filename_template.as_deref() {
            fields.push(("settings.default.filename_template", template));
        }
        let content = write::set_many(content, &fields)?;
        let content = write::set_array(&content, "settings.default.cover_size", &cover_sizes)?;
        write::set_array(&content, "settings.default.chapter_type", &chapter_types)
    })?;

    eprintln!("\nDefaults saved.");
    if config.accounts.is_empty() {
        eprintln!(
            "Next: register an account with `audible account login` \
             (or import a legacy Python auth file with `audible account import <file>`)."
        );
    }
    Ok(())
}

/// Snake-case string form of a config enum (matches the TOML values).
fn enum_str<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>> {
    Ok(match value {
        Some(value) => serde_json::to_value(value)?.as_str().map(|s| s.to_owned()),
        None => None,
    })
}
