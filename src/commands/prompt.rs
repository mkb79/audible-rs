//! Shared terminal prompts (stderr): free-text with default, numbered
//! choice menus, y/N confirmation and secret input. Kept UI-only — no
//! command logic — so every command prompts consistently.

use anyhow::{Result, bail};

/// Prompts on stderr with a prefilled default (empty input keeps it).
pub(crate) fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let term = console::Term::stderr();
    term.write_str(&format!("{label} [{default}]: "))?;
    let input = term.read_line()?;
    let trimmed = input.trim();
    Ok(if trimmed.is_empty() {
        default.to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// Prompts until the input matches one of the allowed choices.
/// Presents `choices` as a numbered menu and returns the chosen value. The user
/// may enter the number or the literal value; an empty line keeps `default`.
/// Deliberately a plain numbered list (no TTY-only widget) so `setup` still
/// works when stdin is piped.
pub(crate) fn prompt_choice(label: &str, choices: &[&str], default: &str) -> Result<String> {
    let default_pos = choices.iter().position(|choice| *choice == default);
    loop {
        eprintln!("{label}:");
        for (index, choice) in choices.iter().enumerate() {
            let marker = if Some(index) == default_pos {
                "  (default)"
            } else {
                ""
            };
            eprintln!("  {}) {choice}{marker}", index + 1);
        }
        let picked = prompt_with_default("Select (number or value)", default)?;
        if let Some(choice) = picked
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| choices.get(index))
        {
            return Ok((*choice).to_owned());
        }
        if choices.contains(&picked.as_str()) {
            return Ok(picked);
        }
        eprintln!(
            "please pick a number 1-{} or one of: {}",
            choices.len(),
            choices.join(", ")
        );
        // The re-ask reprints the whole menu, so separate it from the failed
        // attempt the same way the caller separates one question from the next.
        eprintln!();
    }
}

/// Asks a `y/N` confirmation for a destructive action (prompt on stderr).
/// `yes` — the command's `--yes` flag — skips the question. Fails on a
/// non-interactive stderr so nothing destructive runs without consent.
pub(crate) fn confirm(yes: bool, question: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    let term = console::Term::stderr();
    if !term.is_term() {
        bail!("this needs confirmation; re-run with --yes in a non-interactive shell");
    }
    term.write_str(&format!("{question} [y/N]: "))?;
    let answer = term.read_line()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Prompts (stderr) for a secret line without echo (e.g. the Amazon password).
pub(crate) fn prompt_secret(label: &str) -> Result<String> {
    let term = console::Term::stderr();
    term.write_str(&format!("{label}: "))?;
    let secret = term.read_secure_line()?;
    if secret.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(secret)
}

/// Prompts on stderr and requires a non-empty line (used for the redirect URL).
pub(crate) fn prompt_required(label: &str) -> Result<String> {
    let term = console::Term::stderr();
    loop {
        term.write_str(&format!("{label}: "))?;
        let line = term.read_line()?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
}

/// The shared many-candidates `--title` picker (audit 2026-07-17, C4 —
/// the library and catalog resolvers carried drifted copies). On a TTY:
/// a multi-select over `labels` (space toggles, `a` all, enter confirms)
/// that reports a one-line "selected N of M" summary instead of
/// dialoguer's echo; returns the chosen indices (empty = nothing
/// selected, already reported). Off a TTY: an error listing at most 20
/// candidates plus `hint` — a single podcast show can expand to hundreds
/// of rows, so the dump is always capped.
///
/// `prompt_label`/`noun` word the two surfaces ("Matches"/"titles",
/// "Catalog matches"/"catalog titles").
pub(crate) fn pick_many(
    prompt_label: &str,
    noun: &str,
    query: &str,
    labels: &[String],
    hint: &str,
) -> Result<Vec<usize>> {
    if !console::Term::stderr().is_term() {
        const MAX_LISTED: usize = 20;
        let mut listing: Vec<String> = labels
            .iter()
            .take(MAX_LISTED)
            .map(|label| format!("  {label}"))
            .collect();
        if labels.len() > MAX_LISTED {
            listing.push(format!("  … and {} more", labels.len() - MAX_LISTED));
        }
        bail!(
            "{} {noun} match {query:?}{hint}:\n{}",
            labels.len(),
            listing.join("\n"),
        );
    }
    // `report(false)`: the default echoes the whole chosen list back as
    // one long line — we clear the picker and print a concise summary.
    let selection = dialoguer::MultiSelect::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!(
            "{prompt_label} for {query:?} — space toggles · a all · enter confirms"
        ))
        .items(labels)
        .report(false)
        .interact_on(&console::Term::stderr())?;
    if selection.is_empty() {
        eprintln!("no titles selected for {query:?}");
    } else {
        eprintln!(
            "selected {} of {} for {query:?}",
            selection.len(),
            labels.len()
        );
    }
    Ok(selection)
}
