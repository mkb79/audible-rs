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

/// A section header. The wizard is otherwise one dense wall of prompts, and the
/// numbered `prompt_choice` menus make it worse by butting straight against the
/// next label.
fn section(title: &str) {
    eprintln!("\n── {title} ──");
}

/// The wizard's prompts, each preceded by a blank line. The spacing lives here
/// rather than in [`prompt_with_default`]/[`prompt_choice`], which `account`
/// shares.
fn ask(label: &str, default: &str) -> Result<String> {
    eprintln!();
    prompt_with_default(label, default)
}

fn ask_choice(label: &str, choices: &[&str], default: &str) -> Result<String> {
    eprintln!();
    prompt_choice(label, choices, default)
}

fn ask_yes_no(label: &str, default: bool) -> Result<bool> {
    let default = if default { "yes" } else { "no" };
    Ok(ask_choice(label, &["yes", "no"], default)? == "yes")
}

fn setup(ctx: &Ctx) -> Result<()> {
    let term = console::Term::stderr();
    if !term.is_term() {
        bail!("setup is interactive and needs a terminal (use `audible config set` in scripts)");
    }

    eprintln!("audible setup — default settings bundle");
    eprintln!("config file: {}", ctx.config_file().display());

    let config = ctx.config();
    let default = config.settings.get("default").cloned().unwrap_or_default();

    section("Downloads");

    let download_dir = ask(
        "Download directory",
        &default
            .download_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "~/Audible".to_owned()),
    )?;

    let overwrite = ask_choice(
        "Re-download already fetched artifacts",
        &["skip", "force"],
        &enum_str(&default.overwrite)?.unwrap_or_else(|| "skip".to_owned()),
    )?;

    let include_podcasts = ask_yes_no(
        "Include podcasts in downloads (a show ASIN downloads all its episodes)",
        default.include_podcasts.unwrap_or(true),
    )?;

    let cover_size = ask(
        "Default cover size(s) in px (comma-separated)",
        &default
            .cover_size
            .as_deref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "500".to_owned()),
    )?;

    let chapter_type = ask(
        "Chapter title layout(s) (tree, flat, or both comma-separated)",
        &default
            .chapter_type
            .as_deref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "tree".to_owned()),
    )?;

    section("File names");

    let filename_mode = ask_choice(
        "Filename mode",
        &["ascii", "unicode", "asin_ascii", "asin_unicode", "custom"],
        &enum_str(&default.filename_mode)?.unwrap_or_else(|| "ascii".to_owned()),
    )?;

    // Custom mode needs a template; ask for it and verify it right away so an
    // invalid template can never be written to the config.
    let filename_template = if filename_mode == "custom" {
        let value = loop {
            let value = ask(
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

    let filename_max_length = loop {
        let value = ask(
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

    section("Decryption");

    // Decrypt (AUD-27): surfaced here so users decide once. Default keeps the
    // source aaxc (non-destructive; big libraries can opt into removal).
    let decrypt = ask_yes_no(
        "Decrypt downloads to a playable m4b",
        default.decrypt.unwrap_or(false),
    )?;
    // Both only mean anything with decrypt on. `None` = not asked, and the key
    // is then dropped from the config below.
    let (decrypt_keep_source, decrypt_backend) = if decrypt {
        (
            Some(ask_yes_no(
                "Keep the source aaxc after decrypt (no = delete it)",
                default.decrypt_keep_source.unwrap_or(true),
            )?),
            Some(ask_choice(
                "Decrypt tool",
                &["auto", "aaxclean", "ffmpeg"],
                &enum_str(&default.decrypt_backend)?.unwrap_or_else(|| "auto".to_owned()),
            )?),
        )
    } else {
        (None, None)
    };

    section("Library database");

    let auto_sync = ask_choice(
        "Library auto-sync before local reads",
        &["delta", "none"],
        &enum_str(&Some(config.db.auto_sync))?.expect("auto_sync always set"),
    )?;
    // Without auto-sync there is no age to compare against.
    let sync_max_age = if auto_sync == "none" {
        None
    } else {
        Some(loop {
            let value = ask("Sync max age (e.g. 6h, 30m)", &config.db.sync_max_age)?;
            match parse_duration(&value) {
                Ok(_) => break value,
                Err(error) => eprintln!("{error}"),
            }
        })
    };

    let record_changes = ask_yes_no(
        "Record library changes (added/changed/removed) for `library changes`",
        config.db.record_changes,
    )?;
    // Nothing recorded, nothing to retain.
    let change_retention_days = if record_changes {
        Some(loop {
            let value = ask(
                "Keep change history for how many days (0 = forever)",
                &config.db.change_retention_days.to_string(),
            )?;
            match value.trim().parse::<u32>() {
                Ok(_) => break value.trim().to_owned(),
                Err(_) => eprintln!("enter a whole number of days (e.g. 90)"),
            }
        })
    } else {
        None
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
    let bool_str = |value: bool| if value { "true" } else { "false" };
    let decrypt_value = bool_str(decrypt);
    let include_podcasts_value = bool_str(include_podcasts);
    let record_changes_value = bool_str(record_changes);
    let decrypt_keep_source_value = decrypt_keep_source.map(bool_str);

    // Every question we skipped: not asked ⇒ the key must not survive in the
    // config either. `setup` is repeatable, so a previous run's
    // `decrypt_keep_source` has to go once decrypt is switched off — dead
    // config is exactly what bites when the feature is switched back on and
    // nobody remembers the old answer (AUD-204).
    let conditional: [(&str, Option<&str>); 5] = [
        (
            "settings.default.filename_template",
            filename_template.as_deref(),
        ),
        (
            "settings.default.decrypt_keep_source",
            decrypt_keep_source_value,
        ),
        (
            "settings.default.decrypt_backend",
            decrypt_backend.as_deref(),
        ),
        ("db.sync_max_age", sync_max_age.as_deref()),
        ("db.change_retention_days", change_retention_days.as_deref()),
    ];

    let fields: [(&str, &str); 8] = [
        ("settings.default.download_dir", download_dir.as_str()),
        ("settings.default.filename_mode", filename_mode.as_str()),
        ("settings.default.overwrite", overwrite.as_str()),
        (
            "settings.default.filename_max_length",
            filename_max_length.as_str(),
        ),
        ("settings.default.decrypt", decrypt_value),
        ("settings.default.include_podcasts", include_podcasts_value),
        ("db.auto_sync", auto_sync.as_str()),
        ("db.record_changes", record_changes_value),
    ];

    write::edit_file(&ctx.config_file(), |content| {
        apply_answers(content, &fields, &conditional, &cover_sizes, &chapter_types)
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

/// Applies the wizard's answers to the config text: the always-present fields,
/// the two arrays, and the conditional keys — written when the question was
/// asked, **removed** when it was skipped.
///
/// The removal is what makes `setup` repeatable (AUD-204): a run with
/// `decrypt = no` has to take a previous run's `decrypt_keep_source` back out,
/// so the file only ever holds settings that are in effect. Split out from the
/// prompts so the rule is testable without a terminal.
fn apply_answers(
    content: &str,
    fields: &[(&str, &str)],
    conditional: &[(&str, Option<&str>)],
    cover_sizes: &[String],
    chapter_types: &[String],
) -> Result<String, crate::config::ConfigError> {
    let mut all: Vec<(&str, &str)> = fields.to_vec();
    for (key, value) in conditional {
        if let Some(value) = value {
            all.push((key, value));
        }
    }
    let content = write::set_many(content, &all)?;
    let content = write::set_array(&content, "settings.default.cover_size", cover_sizes)?;
    let mut content = write::set_array(&content, "settings.default.chapter_type", chapter_types)?;
    for (key, value) in conditional {
        // `unset` fails on an absent key, so only drop what is really there —
        // a skipped question is the norm, not an error.
        if value.is_none() && write::get(&content, key)?.is_some() {
            content = write::unset(&content, key)?;
        }
    }
    Ok(content)
}

/// Snake-case string form of a config enum (matches the TOML values).
fn enum_str<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>> {
    Ok(match value {
        Some(value) => serde_json::to_value(value)?.as_str().map(|s| s.to_owned()),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COVER: [&str; 1] = ["500"];
    const CHAPTER: [&str; 1] = ["tree"];

    fn arrays() -> (Vec<String>, Vec<String>) {
        (
            COVER.iter().map(|s| (*s).to_owned()).collect(),
            CHAPTER.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    /// `setup` is repeatable, so a follow-up that is no longer asked must be
    /// taken back out of the config (AUD-204): switching decrypt off has to drop
    /// `decrypt_keep_source`/`decrypt_backend`, leaving `auto_sync` drops the
    /// max age, and so on. Dead config is what bites when the feature is turned
    /// back on and nobody remembers the old answer.
    #[test]
    fn skipped_follow_ups_are_removed_from_the_config() {
        let before = r#"version = 1

[settings.default]
decrypt = true
decrypt_keep_source = false
decrypt_backend = "ffmpeg"
filename_mode = "custom"
filename_template = "%fulltitle%"

[db]
auto_sync = "delta"
sync_max_age = "6h"
record_changes = true
change_retention_days = 90
"#;
        // A second run that turns everything off — so nothing dependent is asked.
        let fields = [
            ("settings.default.decrypt", "false"),
            ("settings.default.filename_mode", "ascii"),
            ("db.auto_sync", "none"),
            ("db.record_changes", "false"),
        ];
        let conditional: [(&str, Option<&str>); 5] = [
            ("settings.default.filename_template", None),
            ("settings.default.decrypt_keep_source", None),
            ("settings.default.decrypt_backend", None),
            ("db.sync_max_age", None),
            ("db.change_retention_days", None),
        ];
        let (cover, chapter) = arrays();
        let after = apply_answers(before, &fields, &conditional, &cover, &chapter).unwrap();

        for key in [
            "decrypt_keep_source",
            "decrypt_backend",
            "filename_template",
            "sync_max_age",
            "change_retention_days",
        ] {
            assert!(!after.contains(key), "{key} must be gone:\n{after}");
        }
        // The parents themselves are still recorded with their new answer.
        assert!(after.contains("decrypt = false"));
        assert!(after.contains("auto_sync = \"none\""));

        // Running again over the already-clean config must not fail: `unset`
        // rejects an absent key, and by then these keys are long gone.
        let twice = apply_answers(&after, &fields, &conditional, &cover, &chapter)
            .expect("removing an absent key is a no-op, not an error");
        assert_eq!(twice, after, "a repeated run changes nothing further");
    }

    /// The counterpart: a question that *was* asked writes its key.
    #[test]
    fn asked_follow_ups_are_written() {
        let (cover, chapter) = arrays();
        let fields = [("settings.default.decrypt", "true")];
        let conditional: [(&str, Option<&str>); 2] = [
            ("settings.default.decrypt_keep_source", Some("false")),
            ("settings.default.decrypt_backend", Some("aaxclean")),
        ];
        let after =
            apply_answers("version = 1\n", &fields, &conditional, &cover, &chapter).unwrap();
        assert!(after.contains("decrypt_keep_source = false"));
        assert!(after.contains("decrypt_backend = \"aaxclean\""));
    }
}
