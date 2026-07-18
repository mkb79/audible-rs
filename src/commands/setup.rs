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

/// The two chapter layouts combine to exactly three sensible answers, so the
/// question is a menu rather than free text.
const CHAPTER_CHOICES: [&str; 3] = ["tree", "flat", "tree,flat"];

/// The stored chapter layouts expressed as one of [`CHAPTER_CHOICES`].
///
/// Anything else falls back to `tree` — including `both`, which an earlier
/// wizard happily wrote because its label ("tree, flat, or both comma-separated")
/// read as if `both` were a value, while `parse_chapter_types` only ever
/// accepted `tree`/`flat`. Without this, a stored value the menu does not offer
/// would make an empty answer re-ask forever.
fn chapter_default(stored: Option<&[String]>) -> String {
    let stored = stored.unwrap_or(&[]);
    let has = |name: &str| {
        stored
            .iter()
            .any(|value| value.trim().eq_ignore_ascii_case(name))
    };
    match (has("tree"), has("flat")) {
        (true, true) => "tree,flat".to_owned(),
        (false, true) => "flat".to_owned(),
        _ => "tree".to_owned(),
    }
}

/// The first item that is not a cover size, with the reason — or `None` when
/// all are fine. The answer is a **list**, so each item is checked on its own:
/// `500,900` is two sizes, not one unparsable number. The rule itself is the
/// config's ([`crate::config::schema::validate_cover_size`]), so the wizard
/// cannot drift from what `config set` accepts.
fn invalid_cover_size(sizes: &[String]) -> Option<String> {
    sizes
        .iter()
        .find_map(|size| crate::config::schema::validate_cover_size(size).err())
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
        &crate::commands::enum_str(&default.overwrite).unwrap_or_else(|| "skip".to_owned()),
    )?;

    let include_podcasts = ask_yes_no(
        "Include podcasts in downloads (a show ASIN downloads all its episodes)",
        default.include_podcasts.unwrap_or(true),
    )?;

    // Verified here, like the template and the numbers — an unchecked answer
    // would otherwise sit in the config until a download failed on it. (The
    // config write validates too; this just fails at the question rather than
    // at the end of the wizard.)
    let cover_size = loop {
        let value = ask(
            "Default cover size(s): px, or `native` for each title's largest (comma-separated)",
            &default
                .cover_size
                .as_deref()
                .map(|v| v.join(","))
                .unwrap_or_else(|| "500".to_owned()),
        )?;
        let sizes = crate::commands::split_csv(&value);
        if sizes.is_empty() {
            eprintln!("pick at least one size");
        } else if let Some(reason) = invalid_cover_size(&sizes) {
            eprintln!("{reason}");
        } else {
            break value;
        }
    };

    let chapter_type = ask_choice(
        "Chapter title layout(s)",
        &CHAPTER_CHOICES,
        &chapter_default(default.chapter_type.as_deref()),
    )?;

    section("File names");

    let filename_mode = ask_choice(
        "Filename mode",
        &["ascii", "unicode", "asin_ascii", "asin_unicode", "custom"],
        &crate::commands::enum_str(&default.filename_mode).unwrap_or_else(|| "ascii".to_owned()),
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
                &crate::commands::enum_str(&default.decrypt_backend)
                    .unwrap_or_else(|| "auto".to_owned()),
            )?),
        )
    } else {
        (None, None)
    };

    section("Library database");

    let auto_sync = ask_choice(
        "Library auto-sync before local reads",
        &["delta", "none"],
        &crate::commands::enum_str(&Some(config.db.auto_sync)).expect("auto_sync always set"),
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

    let cover_sizes = crate::commands::split_csv(&cover_size);
    let chapter_types = crate::commands::split_csv(&chapter_type);
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

    /// The chapter question is a menu, so its prefill has to *be* one of the
    /// choices — otherwise an empty answer returns a value the menu rejects and
    /// the question re-asks forever. `both` is the real case: an earlier wizard
    /// wrote it from a label that read as if it were a value, while only
    /// `tree`/`flat` were ever valid.
    #[test]
    fn chapter_default_is_always_one_of_the_offered_choices() {
        let stored =
            |values: &[&str]| -> Vec<String> { values.iter().map(|v| (*v).to_owned()).collect() };
        assert_eq!(chapter_default(Some(&stored(&["tree"]))), "tree");
        assert_eq!(chapter_default(Some(&stored(&["flat"]))), "flat");
        assert_eq!(
            chapter_default(Some(&stored(&["tree", "flat"]))),
            "tree,flat"
        );
        // Order and case must not matter.
        assert_eq!(
            chapter_default(Some(&stored(&["flat", "Tree"]))),
            "tree,flat"
        );
        // Garbage a previous run may have written, and the unset case.
        assert_eq!(chapter_default(Some(&stored(&["both"]))), "tree");
        assert_eq!(chapter_default(Some(&stored(&[]))), "tree");
        assert_eq!(chapter_default(None), "tree");

        for values in [&["tree"][..], &["flat"][..], &["both"][..], &[][..]] {
            let picked = chapter_default(Some(&stored(values)));
            assert!(
                CHAPTER_CHOICES.contains(&picked.as_str()),
                "{picked:?} is not offered by the menu"
            );
        }
    }

    /// Cover sizes are a **list**, so the check is per item — `500,900` is two
    /// sizes, not one unparsable number. (Validating the raw answer as a single
    /// integer would reject every multi-size setup, which the config, the
    /// `--cover-size` flag and `settings set` all support.)
    #[test]
    fn cover_sizes_accept_a_comma_separated_list() {
        let sizes = |value: &str| crate::commands::split_csv(value);

        assert!(invalid_cover_size(&sizes("500")).is_none());
        assert!(invalid_cover_size(&sizes("500,900")).is_none());
        assert!(
            invalid_cover_size(&sizes("500, 900 ,1215")).is_none(),
            "surrounding spaces are trimmed away"
        );

        // `native` — the master itself — is a size like any other, alone or mixed.
        assert!(invalid_cover_size(&sizes("native")).is_none());
        assert!(invalid_cover_size(&sizes("500,native")).is_none());

        // Only the offending item is reported, and it is named.
        let reason = invalid_cover_size(&sizes("500,large")).expect("\"large\" is not a size");
        assert!(reason.contains("large"), "names the culprit: {reason}");
        assert!(
            invalid_cover_size(&sizes("0")).is_some(),
            "0 px is not a size"
        );
        assert!(invalid_cover_size(&sizes("-5")).is_some());

        // Above the typo guard the message has to point at what was meant.
        let reason = invalid_cover_size(&sizes("5000")).expect("above the cap");
        assert!(reason.contains("native"), "points at native: {reason}");

        // An empty answer splits to nothing; the caller handles that case.
        assert!(sizes("").is_empty());
        assert!(sizes(" , ").is_empty());
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
