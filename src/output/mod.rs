//! Output rendering (D12): commands produce a structured [`Output`], a
//! central renderer turns it into `table | json | plain` (global
//! `--output` flag). Progress display (indicatif) follows with the
//! download work in M3.
//!
//! Formats:
//! * `table` — human-readable aligned columns / key-value lines.
//! * `json` — machine-readable; tables become arrays of objects keyed
//!   by the column names.
//! * `plain` — tab-separated rows without headers, for cut/awk pipes.

use std::str::FromStr;

use comfy_table::Table as ComfyTable;
use comfy_table::presets;

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

/// Renders the output in the requested format (no trailing newline).
pub fn render(output: &Output, format: OutputFormat) -> String {
    match (output, format) {
        (Output::Table { columns, rows }, OutputFormat::Table) => {
            let mut table = ComfyTable::new();
            table.load_preset(presets::NOTHING);
            table.set_header(columns.iter().map(|c| c.to_uppercase()));
            for row in rows {
                table.add_row(row.clone());
            }
            table.to_string()
        }
        (Output::Table { columns, rows }, OutputFormat::Json) => {
            let array: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .zip(row)
                        .map(|(column, cell)| (column.clone(), cell.clone().into()))
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                        .into()
                })
                .collect();
            serde_json::to_string_pretty(&array).expect("strings always serialize")
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
        (Output::KeyValue(pairs), OutputFormat::Json) => {
            let object: serde_json::Map<String, serde_json::Value> = pairs
                .iter()
                .map(|(key, value)| (key.clone(), value.clone().into()))
                .collect();
            serde_json::to_string_pretty(&serde_json::Value::Object(object))
                .expect("strings always serialize")
        }
        (Output::KeyValue(pairs), OutputFormat::Plain) => pairs
            .iter()
            .map(|(key, value)| format!("{key}\t{value}"))
            .collect::<Vec<_>>()
            .join("\n"),

        (Output::Json(value), OutputFormat::Plain) => value.to_string(),
        (Output::Json(value), _) => {
            serde_json::to_string_pretty(value).expect("values always serialize")
        }

        (Output::Text(text), _) => text.clone(),
    }
}

/// Renders to stdout.
pub fn print(output: &Output, format: OutputFormat) {
    println!("{}", render(output, format));
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
        let rendered = render(&sample_table(), OutputFormat::Table);
        assert!(rendered.contains("NAME"));
        assert!(rendered.contains("MARKETPLACE"));
        assert!(rendered.contains("alice-de"));
    }

    #[test]
    fn json_format_uses_column_keys() {
        let rendered = render(&sample_table(), OutputFormat::Json);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value[0]["name"], "alice-de");
        assert_eq!(value[1]["marketplace"], "us");
    }

    #[test]
    fn plain_format_is_tab_separated_without_headers() {
        let rendered = render(&sample_table(), OutputFormat::Plain);
        assert_eq!(rendered, "alice-de\tde\nalice-us\tus");
    }

    #[test]
    fn key_value_formats() {
        let output = Output::KeyValue(vec![
            ("name".into(), "alice-de".into()),
            ("account".into(), "alice".into()),
        ]);
        assert!(render(&output, OutputFormat::Table).contains("name     alice-de"));
        let json: serde_json::Value =
            serde_json::from_str(&render(&output, OutputFormat::Json)).unwrap();
        assert_eq!(json["account"], "alice");
        assert_eq!(
            render(&output, OutputFormat::Plain),
            "name\talice-de\naccount\talice"
        );
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
