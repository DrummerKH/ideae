//! A result written out, row by row: the same code serves a copy of the rows
//! in the table and a whole result streamed to a file past the table's limit.

use std::io::{self, Write};

use super::{config::Driver, value::Cell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Markdown,
    Insert,
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Csv, Format::Tsv, Format::Json, Format::Markdown, Format::Insert];

    pub fn name(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::Markdown => "Markdown",
            Format::Insert => "SQL INSERT",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Markdown => "md",
            Format::Insert => "sql",
        }
    }
}

/// Writes a header, rows, and an end.
pub struct Writer<W: Write> {
    out: W,
    format: Format,
    columns: Vec<String>,
    table: String,
    driver: Driver,
    rows: u64,
}

fn delimited(text: &str, separator: char) -> String {
    if text.contains([separator, '"', '\n', '\r']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
}

pub fn sql_value(cell: &Cell, driver: Driver) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Int(_) | Cell::UInt(_) | Cell::Decimal(_) | Cell::Float(_) => cell.shown().into_owned(),
        Cell::Text(text) => driver.string(text),
        // `0x…`, and `''` for none: a bare `0x` is not a literal.
        Cell::Bytes(bytes) if bytes.is_empty() => "''".into(),
        // PostgreSQL's `bytea` literal.
        Cell::Bytes(_) if driver == Driver::Postgres => format!("'\\x{}'", cell.copied().trim_start_matches("0x")),
        Cell::Bytes(_) => cell.copied().into_owned(),
    }
}

fn json_value(cell: &Cell) -> serde_json::Value {
    use serde_json::Value;
    match cell {
        Cell::Null => Value::Null,
        Cell::Int(n) => Value::from(*n),
        Cell::UInt(n) => Value::from(*n),
        // Exact: a JSON number would be read as a float by most programs.
        Cell::Decimal(text) | Cell::Text(text) => Value::from(text.as_ref()),
        Cell::Float(text) => text.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map_or_else(|| Value::from(text.as_ref()), Value::Number),
        Cell::Bytes(_) => Value::from(cell.copied().as_ref()),
    }
}

impl<W: Write> Writer<W> {
    /// `table` names the target of an `INSERT`.
    pub fn start(mut out: W, format: Format, columns: Vec<String>, table: &str, driver: Driver) -> io::Result<Self> {
        match format {
            Format::Csv => writeln!(out, "{}", columns.iter().map(|c| delimited(c, ',')).collect::<Vec<_>>().join(","))?,
            Format::Tsv => writeln!(out, "{}", columns.iter().map(|c| delimited(c, '\t')).collect::<Vec<_>>().join("\t"))?,
            Format::Json => write!(out, "[")?,
            Format::Markdown => {
                writeln!(out, "| {} |", columns.iter().map(|c| markdown(c)).collect::<Vec<_>>().join(" | "))?;
                writeln!(out, "|{}", " --- |".repeat(columns.len()))?;
            }
            Format::Insert => {}
        }
        Ok(Writer { out, format, columns, table: table.to_string(), driver, rows: 0 })
    }

    pub fn row(&mut self, row: &[Cell]) -> io::Result<()> {
        match self.format {
            Format::Csv => writeln!(self.out, "{}", row.iter().map(|c| if c.is_null() { String::new() } else { delimited(&c.copied(), ',') }).collect::<Vec<_>>().join(","))?,
            Format::Tsv => writeln!(self.out, "{}", row.iter().map(|c| delimited(&c.copied(), '\t')).collect::<Vec<_>>().join("\t"))?,
            Format::Json => {
                let object: serde_json::Map<String, serde_json::Value> = self.columns.iter().cloned().zip(row.iter().map(json_value)).collect();
                write!(self.out, "{}\n  {}", if self.rows == 0 { "" } else { "," }, serde_json::Value::Object(object))?;
            }
            Format::Markdown => writeln!(self.out, "| {} |", row.iter().map(|c| markdown(&c.copied())).collect::<Vec<_>>().join(" | "))?,
            Format::Insert => {
                let names = self.columns.iter().map(|c| self.driver.quoted(c)).collect::<Vec<_>>().join(", ");
                let values = row.iter().map(|c| sql_value(c, self.driver)).collect::<Vec<_>>().join(", ");
                writeln!(self.out, "INSERT INTO {} ({names}) VALUES ({values});", self.driver.quoted(&self.table))?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// How many rows were written.
    pub fn finish(mut self) -> io::Result<u64> {
        if self.format == Format::Json {
            writeln!(self.out, "{}]", if self.rows == 0 { "" } else { "\n" })?;
        }
        self.out.flush()?;
        Ok(self.rows)
    }
}

fn markdown(text: &str) -> String {
    text.replace('\\', "\\\\").replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Rows as text, for the clipboard.
pub fn to_string<'a>(format: Format, columns: Vec<String>, table: &str, driver: Driver, rows: impl Iterator<Item = &'a [Cell]>) -> String {
    let mut out = Vec::new();
    let written = (|| -> io::Result<()> {
        let mut writer = Writer::start(&mut out, format, columns, table, driver)?;
        for row in rows {
            writer.row(row)?;
        }
        writer.finish().map(drop)
    })();
    debug_assert!(written.is_ok());
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Vec<Cell>> {
        vec![
            vec![Cell::Int(1), Cell::Decimal("10.50".into()), Cell::Text("it's, \"a\"\nline".into())],
            vec![Cell::UInt(u64::MAX), Cell::Null, Cell::Bytes(vec![0xab].into())],
        ]
    }

    fn written(format: Format) -> String {
        let rows = rows();
        to_string(format, vec!["id".into(), "amount".into(), "note".into()], "orders", Driver::MySql, rows.iter().map(Vec::as_slice))
    }

    #[test]
    fn csv_quotes_what_needs_it_and_null_is_an_empty_field() {
        assert_eq!(written(Format::Csv), "id,amount,note\n1,10.50,\"it's, \"\"a\"\"\nline\"\n18446744073709551615,,0xab\n");
    }

    #[test]
    fn json_keeps_big_integers_and_decimals_exact() {
        let value: serde_json::Value = serde_json::from_str(&written(Format::Json)).unwrap();
        assert_eq!(value[0]["amount"], "10.50");
        assert_eq!(value[1]["id"].as_u64(), Some(u64::MAX));
        assert!(value[1]["amount"].is_null());
    }

    #[test]
    fn insert_statements_escape_their_values() {
        let text = written(Format::Insert);
        assert!(text.starts_with("INSERT INTO `orders` (`id`, `amount`, `note`) VALUES (1, 10.50, 'it''s, \"a\"\\nline');\n"));
        assert!(text.contains("VALUES (18446744073709551615, NULL, 0xab);"));
    }

    #[test]
    fn markdown_keeps_a_row_on_its_line() {
        let rows = [vec![Cell::Text("a|b\nc".into())]];
        let text = to_string(Format::Markdown, vec!["x".into()], "t", Driver::MySql, rows.iter().map(Vec::as_slice));
        assert_eq!(text, "| x |\n| --- |\n| a\\|b c |\n");
    }

    #[test]
    fn an_empty_result_is_still_valid_json() {
        let text = to_string(Format::Json, vec!["x".into()], "t", Driver::MySql, std::iter::empty());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), serde_json::json!([]));
    }
}
