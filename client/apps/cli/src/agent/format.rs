use crate::commands;
use serde_json::{Value, json};
use sqlly_core::{CellValue, ResultColumn};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Csv,
    Tsv,
    Json,
    Jsonl,
    Md,
}

#[derive(Debug, Clone)]
pub struct ResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub rows_affected: Option<u64>,
    pub truncated: bool,
}

pub fn render(format: OutputFormat, sets: &[ResultSet]) -> String {
    match format {
        OutputFormat::Table => render_table(sets, false),
        OutputFormat::Md => render_table(sets, true),
        OutputFormat::Csv => render_delimited(sets, b','),
        OutputFormat::Tsv => render_delimited(sets, b'\t'),
        OutputFormat::Json => render_json(sets),
        OutputFormat::Jsonl => render_jsonl(sets),
    }
}

pub fn render_json_value(sets: &[ResultSet]) -> Value {
    json!({ "result_sets": sets.iter().map(result_set_json).collect::<Vec<_>>() })
}

fn render_table(sets: &[ResultSet], markdown: bool) -> String {
    use comfy_table::presets::{ASCII_MARKDOWN, UTF8_FULL_CONDENSED};
    use comfy_table::{Cell, ContentArrangement, Table};

    let mut out = String::new();
    for (idx, set) in sets.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let mut table = Table::new();
        table.load_style(if markdown {
            ASCII_MARKDOWN
        } else {
            UTF8_FULL_CONDENSED
        });
        table.set_content_arrangement(ContentArrangement::Disabled);
        table.set_header(set.columns.iter().map(|c| c.name.as_str()));
        for row in &set.rows {
            table.add_row(row.iter().map(|cell| {
                let text = commands::cell_display(cell);
                if markdown {
                    Cell::new(text.replace('|', "\\|"))
                } else {
                    Cell::new(text)
                }
            }));
        }
        out.push_str(&table.to_string());
        out.push('\n');
        out.push_str(&format!("({} rows)", set.rows.len()));
        if set.truncated {
            out.push_str(" (truncated at --limit)");
        }
        if let Some(affected) = set.rows_affected {
            out.push_str(&format!("\n({affected} rows affected)"));
        }
    }
    out
}

fn render_delimited(sets: &[ResultSet], delimiter: u8) -> String {
    let mut out = String::new();
    for (idx, set) in sets.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let mut writer = csv::WriterBuilder::new()
            .has_headers(true)
            .delimiter(delimiter)
            .from_writer(Vec::new());
        let headers: Vec<_> = set.columns.iter().map(|c| c.name.as_str()).collect();
        writer.write_record(&headers).expect("csv header");
        for row in &set.rows {
            let fields: Vec<String> = row.iter().map(csv_cell).collect();
            writer.write_record(&fields).expect("csv row");
        }
        out.push_str(&String::from_utf8(writer.into_inner().expect("csv bytes")).expect("utf8"));
        if set.truncated {
            out.push_str("(truncated at --limit)\n");
        }
    }
    out.trim_end().to_string()
}

fn render_json(sets: &[ResultSet]) -> String {
    serde_json::to_string_pretty(&render_json_value(sets)).expect("json")
}

fn render_jsonl(sets: &[ResultSet]) -> String {
    let Some(set) = sets.first() else {
        return String::new();
    };
    let names = dedup_column_names(&set.columns);
    set.rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (idx, cell) in row.iter().enumerate() {
                let key = names
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| format!("col_{idx}"));
                obj.insert(key, cell_json(cell));
            }
            serde_json::to_string(&Value::Object(obj)).expect("jsonl row")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Row-at-a-time renderer for the formats that need no look-ahead (CSV, TSV,
/// JSON Lines). Produces the same text as [`render`] for those formats, so
/// `query --limit 0` can export a result of any size in constant memory.
pub struct StreamWriter<W: std::io::Write> {
    sink: StreamSink<W>,
    /// Result-set index currently being written, if any.
    current: Option<usize>,
    sets_begun: usize,
    jsonl_names: Vec<String>,
}

enum StreamSink<W: std::io::Write> {
    Delimited { delimiter: u8, out: W },
    Lines(W),
}

/// One CSV/TSV record with the same quoting rules as `render_delimited`.
fn write_delimited<W: std::io::Write, I, T>(
    out: &mut W,
    delimiter: u8,
    fields: I,
) -> std::io::Result<()>
where
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .delimiter(delimiter)
        .buffer_capacity(1024)
        .from_writer(out);
    writer.write_record(fields)?;
    writer.flush()
}

impl<W: std::io::Write> StreamWriter<W> {
    /// `None` for formats that must see the whole result first.
    pub fn new(format: OutputFormat, out: W) -> Option<Self> {
        let sink = match format {
            OutputFormat::Csv => StreamSink::Delimited {
                delimiter: b',',
                out,
            },
            OutputFormat::Tsv => StreamSink::Delimited {
                delimiter: b'\t',
                out,
            },
            OutputFormat::Jsonl => StreamSink::Lines(out),
            OutputFormat::Table | OutputFormat::Md | OutputFormat::Json => return None,
        };
        Some(Self {
            sink,
            current: None,
            sets_begun: 0,
            jsonl_names: Vec::new(),
        })
    }

    /// Start result set `index`. CSV/TSV write a blank separator line before
    /// every set after the first, then the header; JSON Lines keeps only the
    /// first set (as [`render`] does).
    pub fn begin_set(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        self.current = Some(index);
        match &mut self.sink {
            StreamSink::Delimited { delimiter, out } => {
                if self.sets_begun > 0 {
                    out.write_all(b"\n")?;
                }
                write_delimited(out, *delimiter, columns.iter().map(|c| c.name.as_str()))?;
            }
            StreamSink::Lines(_) => {
                if index == 0 {
                    self.jsonl_names = dedup_column_names(columns);
                }
            }
        }
        self.sets_begun += 1;
        Ok(())
    }

    pub fn row(&mut self, index: usize, row: &[CellValue]) -> std::io::Result<()> {
        match &mut self.sink {
            StreamSink::Delimited { delimiter, out } => {
                write_delimited(out, *delimiter, row.iter().map(csv_cell))?;
            }
            StreamSink::Lines(out) => {
                if index != 0 {
                    return Ok(());
                }
                let mut obj = serde_json::Map::new();
                for (idx, cell) in row.iter().enumerate() {
                    let key = self
                        .jsonl_names
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| format!("col_{idx}"));
                    obj.insert(key, cell_json(cell));
                }
                serde_json::to_writer(&mut *out, &Value::Object(obj))?;
                out.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    /// Close the current set; `truncated` appends the `--limit` note CSV/TSV
    /// carry in buffered mode.
    pub fn end_set(&mut self, truncated: bool) -> std::io::Result<()> {
        self.current = None;
        if let StreamSink::Delimited { out, .. } = &mut self.sink
            && truncated
        {
            out.write_all(b"(truncated at --limit)\n")?;
        }
        Ok(())
    }

    pub fn current_set(&self) -> Option<usize> {
        self.current
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        match &mut self.sink {
            StreamSink::Delimited { out, .. } | StreamSink::Lines(out) => out.flush(),
        }
    }
}

fn result_set_json(set: &ResultSet) -> Value {
    json!({
        "columns": set.columns.iter().map(|c| json!({
            "name": c.name,
            "data_type": c.data_type,
            "nullable": c.nullable,
        })).collect::<Vec<_>>(),
        "rows": set.rows.iter().map(|row| row.iter().map(cell_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "rows_affected": set.rows_affected,
        "truncated": set.truncated,
    })
}

fn cell_json(cell: &CellValue) -> Value {
    match cell {
        CellValue::Null => Value::Null,
        CellValue::Bool(b) => json!(b),
        CellValue::I64(n) => json!(n),
        CellValue::F64(x) if x.is_finite() => json!(x),
        CellValue::F64(x) => Value::String(x.to_string()),
        CellValue::String(s) => Value::String(s.clone()),
        CellValue::Bytes(_) => Value::String(commands::cell_display(cell)),
    }
}

/// One CSV field's text before the writer quotes it. Text cells go through
/// the shared formula guard so `=1+1` cannot run when the output is opened
/// in a spreadsheet; numbers, booleans and binary values are written as-is.
fn csv_cell(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => String::new(),
        CellValue::String(text) => sqlly_core::formula_guard::guard_text(text).into_owned(),
        other => commands::cell_display(other),
    }
}

fn dedup_column_names(columns: &[ResultColumn]) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    columns
        .iter()
        .map(|c| {
            let base = c.name.clone();
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base}_{count}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_set() -> ResultSet {
        ResultSet {
            columns: vec![
                ResultColumn {
                    name: "a".into(),
                    data_type: "int".into(),
                    nullable: true,
                },
                ResultColumn {
                    name: "b".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
            ],
            rows: vec![
                vec![CellValue::I64(1), CellValue::Null],
                vec![CellValue::I64(2), CellValue::String("héllo 🐘".into())],
            ],
            rows_affected: None,
            truncated: false,
        }
    }

    #[test]
    fn table_renders_null_and_unicode() {
        let text = render(OutputFormat::Table, &[sample_set()]);
        assert!(text.contains("NULL"));
        assert!(text.contains("héllo 🐘"));
        assert_eq!(text.lines().filter(|l| l.contains('─')).count(), 2);
    }

    #[test]
    fn csv_quotes_commas_and_newlines() {
        let set = ResultSet {
            columns: vec![ResultColumn {
                name: "x".into(),
                data_type: "text".into(),
                nullable: true,
            }],
            rows: vec![
                vec![CellValue::String("a,b".into())],
                vec![CellValue::String("line\n2".into())],
            ],
            rows_affected: None,
            truncated: false,
        };
        let text = render(OutputFormat::Csv, &[set]);
        assert!(text.contains("\"a,b\""));
        assert!(text.contains("\"line\n2\""));
    }

    #[test]
    fn csv_null_is_empty_field() {
        let text = render(OutputFormat::Csv, &[sample_set()]);
        assert!(text.starts_with("a,b\n1,"));
    }

    #[test]
    fn tsv_separates_with_tabs() {
        let text = render(OutputFormat::Tsv, &[sample_set()]);
        assert!(text.starts_with("a\tb\n1\t\n2\t"));
        assert!(!text.contains(','));
    }

    #[test]
    fn csv_guards_spreadsheet_formulas_but_not_numbers() {
        let set = ResultSet {
            columns: vec![
                ResultColumn {
                    name: "x".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
                ResultColumn {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: true,
                },
            ],
            rows: vec![
                vec![CellValue::String("=1+1".into()), CellValue::I64(-5)],
                vec![CellValue::String("-12.5".into()), CellValue::F64(-0.5)],
                vec![CellValue::String("@cmd".into()), CellValue::Bool(true)],
            ],
            rows_affected: None,
            truncated: false,
        };
        let text = render(OutputFormat::Csv, &[set]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "'=1+1,-5");
        assert_eq!(lines[2], "-12.5,-0.5");
        assert_eq!(lines[3], "'@cmd,true");
    }

    #[test]
    fn json_types_are_preserved() {
        let value = render_json_value(&[sample_set()]);
        assert_eq!(value["result_sets"][0]["rows"][0][1], Value::Null);
        assert_eq!(value["result_sets"][0]["rows"][1][0], json!(2));
    }

    #[test]
    fn jsonl_one_object_per_row_with_dedup_columns() {
        let set = ResultSet {
            columns: vec![
                ResultColumn {
                    name: "x".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "x".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
            ],
            rows: vec![vec![CellValue::I64(1), CellValue::I64(2)]],
            rows_affected: None,
            truncated: false,
        };
        let text = render(OutputFormat::Jsonl, &[set]);
        let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(line.get("x").is_some());
        assert!(line.get("x_2").is_some());
        assert_eq!(text.lines().count(), 1);
    }

    fn stream(format: OutputFormat, sets: &[ResultSet]) -> String {
        let mut out = Vec::new();
        let mut writer = StreamWriter::new(format, &mut out).expect("streamable");
        for (index, set) in sets.iter().enumerate() {
            writer.begin_set(index, &set.columns).unwrap();
            for row in &set.rows {
                writer.row(index, row).unwrap();
            }
            writer.end_set(set.truncated).unwrap();
        }
        writer.finish().unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn streamed_output_matches_buffered_render() {
        let mut second = sample_set();
        second.truncated = true;
        second.rows.push(vec![
            CellValue::I64(3),
            CellValue::String("=cmd, \"quoted\"\nline".into()),
        ]);
        let sets = [sample_set(), second];
        for format in [OutputFormat::Csv, OutputFormat::Tsv, OutputFormat::Jsonl] {
            let streamed = stream(format, &sets);
            // `emit` prints buffered text with one trailing newline.
            let buffered = format!("{}\n", render(format, &sets));
            assert_eq!(streamed, buffered, "{format:?}");
        }
        assert!(StreamWriter::new(OutputFormat::Table, Vec::new()).is_none());
        assert!(StreamWriter::new(OutputFormat::Json, Vec::new()).is_none());
    }

    #[test]
    fn md_escapes_pipes() {
        let set = ResultSet {
            columns: vec![ResultColumn {
                name: "x".into(),
                data_type: "text".into(),
                nullable: true,
            }],
            rows: vec![vec![CellValue::String("a|b".into())]],
            rows_affected: None,
            truncated: false,
        };
        let text = render(OutputFormat::Md, &[set]);
        assert!(text.contains("a\\|b"));
        assert!(text.starts_with('|'));
    }

    #[test]
    fn multiple_result_sets_are_separated() {
        let text = render(OutputFormat::Table, &[sample_set(), sample_set()]);
        assert!(text.matches("(2 rows)").count() >= 2);
    }

    #[test]
    fn truncated_flag_rendered() {
        let mut set = sample_set();
        set.truncated = true;
        let text = render(OutputFormat::Table, &[set]);
        assert!(text.contains("(truncated at --limit)"));
    }
}
