//! Output rendering (D12): commands produce a structured [`Output`], a
//! central renderer turns it into `table | json | plain` (global
//! `--output` flag).
//!
//! Formats:
//! * `table` — human-readable aligned columns / key-value lines.
//! * `json` — machine-readable **envelope** (AUD-279), see below.
//! * `plain` — tab-separated rows without headers, for cut/awk pipes.
//!
//! # The `-o json` envelope (AUD-279)
//!
//! Every `-o json` run emits exactly **one** envelope on stdout:
//!
//! ```json
//! { "error": null, "warnings": [{"code": "…", "message": "…"}], "result": … }
//! ```
//!
//! * `result` holds what used to be the top-level payload (an array for
//!   tables, an object for key-value views, a server response verbatim
//!   for `api`); `null` on error — except where a command documents a
//!   failure payload (`api` keeps the server's error body in `result`).
//! * `warnings` is always present and empty when clean. Entries carry a
//!   **stable `code`** (the machine contract — consumers match on it,
//!   never on the message prose) and a human `message`. Raised via
//!   `Ctx::warn`, which also mirrors the message to stderr.
//! * `error` is `null` on success; on failure the dispatch boundary emits
//!   the envelope with `error.message` set and `result: null` — exit codes
//!   are unchanged (warnings → 0, error → 1). A run that succeeds without
//!   printing a payload (mutations) still emits the envelope with
//!   `result: null`, so **every** command answers `-o json`.
//!
//! The envelope exists **only here** — no command builds one by hand.
//! Deliberate passthrough exceptions (raw stdout, no envelope; each marks
//! itself via `Ctx::mark_raw_stdout`): `completions` (the shell script is
//! the artifact), `library export --csv` (explicit CSV), `api --dry-run` /
//! `--include` / `--dump-header` / `--output-file` and binary bodies
//! (wire-debug tools), `account cookies refresh --show-response` (raw
//! payload dump); `account export` writes files, and external plugin
//! invocations own their stdout entirely.

use std::str::FromStr;

use comfy_table::Table as ComfyTable;
use comfy_table::presets;

/// A warning-class note for the `-o json` envelope: something was
/// degraded or silently narrowed (a skipped title class, stale data …).
/// `code` is the stable machine contract; `message` the human wording
/// that `Ctx::warn` also mirrors to stderr. Not for explanations of a
/// normal empty result — those stay plain stderr hints (AUD-205).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Stable identifier, e.g. `lapsed_skipped` (snake_case, never
    /// reworded once shipped).
    pub code: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

/// Selected rendering format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable (default).
    #[default]
    Table,
    /// Machine-readable JSON.
    Json,
    /// Tab-separated values without headers.
    Plain,
}

impl FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Ok(OutputFormat::Table),
            "json" => Ok(OutputFormat::Json),
            "plain" => Ok(OutputFormat::Plain),
            other => Err(format!(
                "unknown output format {other:?} (expected table|json|plain)"
            )),
        }
    }
}

/// Structured command output.
#[derive(Debug)]
pub enum Output {
    /// Tabular data. Column keys are lowercase identifiers (`name`,
    /// `country_code`); the table renderer uppercases them for display,
    /// the JSON renderer uses them as object keys.
    Table {
        /// Column keys.
        columns: Vec<String>,
        /// Rows; each row has one cell per column.
        rows: Vec<Vec<String>>,
    },
    /// Ordered key-value pairs (detail/"show" views).
    KeyValue(Vec<(String, String)>),
    /// A raw JSON payload.
    Json(serde_json::Value),
    /// Pre-rendered text, emitted verbatim in every format — for views whose
    /// layout is custom (e.g. the sectioned `stats` report). The command
    /// itself chooses the layout per format (a JSON payload uses [`Output::Json`]).
    Text(String),
}

impl Output {
    /// Convenience constructor for tables.
    pub fn table<C: Into<String>>(columns: Vec<C>, rows: Vec<Vec<String>>) -> Self {
        Output::Table {
            columns: columns.into_iter().map(Into::into).collect(),
            rows,
        }
    }
}

/// The `result` value of the JSON envelope: what used to be the top-level
/// payload (tables → arrays of objects keyed by the column names,
/// key-value views → one object, raw JSON verbatim, text as a string).
fn result_value(output: &Output) -> serde_json::Value {
    match output {
        Output::Table { columns, rows } => rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .zip(row)
                    .map(|(column, cell)| (column.clone(), cell.clone().into()))
                    .collect::<serde_json::Map<String, serde_json::Value>>()
                    .into()
            })
            .collect::<Vec<serde_json::Value>>()
            .into(),
        Output::KeyValue(pairs) => serde_json::Value::Object(
            pairs
                .iter()
                .map(|(key, value)| (key.clone(), value.clone().into()))
                .collect(),
        ),
        Output::Json(value) => value.clone(),
        Output::Text(text) => text.clone().into(),
    }
}

/// Serializes the envelope — the one place its shape exists (AUD-279).
fn envelope(error: Option<&str>, warnings: &[Warning], result: serde_json::Value) -> String {
    let warnings: Vec<serde_json::Value> = warnings
        .iter()
        .map(|warning| serde_json::json!({"code": warning.code, "message": warning.message}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "error": error.map(|message| serde_json::json!({"message": message})),
        "warnings": warnings,
        "result": result,
    }))
    .expect("envelope values always serialize")
}

/// Renders the output in the requested format (no trailing newline).
/// `warnings` reach only the `json` envelope — their human mirror went to
/// stderr the moment `Ctx::warn` raised them, in every format.
pub fn render(output: &Output, format: OutputFormat, warnings: &[Warning]) -> String {
    match (output, format) {
        // A header with no rows says nothing — the command's own stderr hint
        // explains the empty result (AUD-205). Only the human format is
        // suppressed: `Json` keeps rendering the envelope with `result: []`,
        // which consumers need.
        (Output::Table { rows, .. }, OutputFormat::Table) if rows.is_empty() => String::new(),
        (Output::Table { columns, rows }, OutputFormat::Table) => {
            let mut table = ComfyTable::new();
            table.load_preset(presets::NOTHING);
            table.set_header(columns.iter().map(|c| c.to_uppercase()));
            for row in rows {
                table.add_row(row.clone());
            }
            table.to_string()
        }
        (Output::Table { rows, .. }, OutputFormat::Plain) => rows
            .iter()
            .map(|row| row.join("\t"))
            .collect::<Vec<_>>()
            .join("\n"),

        (Output::KeyValue(pairs), OutputFormat::Table) => {
            let width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            pairs
                .iter()
                .map(|(key, value)| format!("{key:<width$}  {value}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        (Output::KeyValue(pairs), OutputFormat::Plain) => pairs
            .iter()
            .map(|(key, value)| format!("{key}\t{value}"))
            .collect::<Vec<_>>()
            .join("\n"),

        // The human format displays a raw JSON payload pretty-printed and
        // unwrapped — the envelope belongs to `-o json` alone.
        (Output::Json(value), OutputFormat::Table) => {
            serde_json::to_string_pretty(value).expect("values always serialize")
        }
        (Output::Json(value), OutputFormat::Plain) => value.to_string(),
        (Output::Text(text), OutputFormat::Table | OutputFormat::Plain) => text.clone(),

        (output, OutputFormat::Json) => envelope(None, warnings, result_value(output)),
    }
}

/// Renders to stdout.
pub fn print(output: &Output, format: OutputFormat, warnings: &[Warning]) {
    let rendered = render(output, format, warnings);
    // Nothing to render (an empty table, or `plain` with no rows) prints
    // nothing at all — not a lone header, not a blank line.
    if rendered.is_empty() {
        return;
    }
    println!("{rendered}");
}

/// Emits an envelope directly (outside the payload renderer) — the one
/// home of the `-o json` shape for the boundary cases: the failure
/// envelope (`error` set), the empty success envelope of a command
/// without a payload (`result: null`), and `api`'s documented
/// failure-with-body (`error` set, `result` = the server's error body).
/// Callers gate on the format and the printed-flag; exit codes stay
/// untouched.
pub fn print_envelope(error: Option<&str>, warnings: &[Warning], result: serde_json::Value) {
    println!("{}", envelope(error, warnings, result));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_table() -> Output {
        Output::table(
            vec!["name", "marketplace"],
            vec![
                vec!["alice-de".into(), "de".into()],
                vec!["alice-us".into(), "us".into()],
            ],
        )
    }

    #[test]
    fn table_format_uppercases_headers() {
        let rendered = render(&sample_table(), OutputFormat::Table, &[]);
        assert!(rendered.contains("NAME"));
        assert!(rendered.contains("MARKETPLACE"));
        assert!(rendered.contains("alice-de"));
    }

    /// The `-o json` envelope (AUD-279): constant `error`/`warnings`/`data`
    /// keys; the table payload sits under `data`, keyed by column names.
    #[test]
    fn json_format_wraps_the_payload_in_the_envelope() {
        let rendered = render(&sample_table(), OutputFormat::Json, &[]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert!(value["error"].is_null());
        assert_eq!(value["warnings"], serde_json::json!([]));
        assert_eq!(value["result"][0]["name"], "alice-de");
        assert_eq!(value["result"][1]["marketplace"], "us");
    }

    /// Warnings reach the envelope as `{code, message}` — the code is the
    /// stable machine contract, the message the human wording.
    #[test]
    fn json_envelope_carries_warnings() {
        let warnings = [Warning {
            code: "lapsed_skipped",
            message: "skipped 3 title(s) …".into(),
        }];
        let rendered = render(&sample_table(), OutputFormat::Json, &warnings);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["warnings"][0]["code"], "lapsed_skipped");
        assert!(
            value["warnings"][0]["message"]
                .as_str()
                .unwrap()
                .contains("skipped")
        );
        assert!(value["error"].is_null());
    }

    /// The failure envelope: `error.message` set, `result: null`, recorded
    /// warnings preserved — same constant keys as success.
    #[test]
    fn error_envelope_sets_error_and_nulls_data() {
        let warnings = [Warning {
            code: "archived_skipped",
            message: "skipped 1 archived title(s)".into(),
        }];
        let rendered = envelope(Some("license denied: no reason given"), &warnings, {
            serde_json::Value::Null
        });
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["error"]["message"], "license denied: no reason given");
        assert!(value["result"].is_null());
        assert_eq!(value["warnings"][0]["code"], "archived_skipped");
    }

    #[test]
    fn plain_format_is_tab_separated_without_headers() {
        let rendered = render(&sample_table(), OutputFormat::Plain, &[]);
        assert_eq!(rendered, "alice-de\tde\nalice-us\tus");
    }

    /// An empty table renders nothing for the human formats — a lone header
    /// says nothing (AUD-205) — but `json` must keep emitting the envelope
    /// with `data: []`, which consumers depend on.
    #[test]
    fn empty_table_is_silent_except_in_json() {
        let empty = Output::table(vec!["name", "marketplace"], Vec::new());
        assert_eq!(render(&empty, OutputFormat::Table, &[]), "");
        assert_eq!(render(&empty, OutputFormat::Plain, &[]), "");
        let value: serde_json::Value =
            serde_json::from_str(&render(&empty, OutputFormat::Json, &[])).unwrap();
        assert_eq!(value["result"], serde_json::json!([]));
        assert!(value["error"].is_null());
    }

    #[test]
    fn key_value_formats() {
        let output = Output::KeyValue(vec![
            ("name".into(), "alice-de".into()),
            ("account".into(), "alice".into()),
        ]);
        assert!(render(&output, OutputFormat::Table, &[]).contains("name     alice-de"));
        let json: serde_json::Value =
            serde_json::from_str(&render(&output, OutputFormat::Json, &[])).unwrap();
        assert_eq!(json["result"]["account"], "alice");
        assert_eq!(
            render(&output, OutputFormat::Plain, &[]),
            "name\talice-de\naccount\talice"
        );
    }

    /// A raw JSON payload stays unwrapped in the human format (`table`
    /// pretty-prints it) — the envelope belongs to `-o json` alone.
    #[test]
    fn raw_json_payload_is_unwrapped_outside_json_format() {
        let output = Output::Json(serde_json::json!({"format": "export", "version": 1}));
        let table = render(&output, OutputFormat::Table, &[]);
        let value: serde_json::Value = serde_json::from_str(&table).unwrap();
        assert_eq!(value["format"], "export");

        let json = render(&output, OutputFormat::Json, &[]);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["result"]["format"], "export");
    }

    #[test]
    fn output_format_parses() {
        assert_eq!(
            "table".parse::<OutputFormat>().unwrap(),
            OutputFormat::Table
        );
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert!("xml".parse::<OutputFormat>().is_err());
    }
}
