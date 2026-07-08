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
