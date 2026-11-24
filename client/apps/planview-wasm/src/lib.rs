//! SQLly's execution-plan reader for the browser.
//!
//! The free online plan viewer at `sqlly.app/tools/query-plan-viewer` runs
//! this crate compiled to `wasm32-unknown-unknown`. It is a thin shell over
//! `sqlly_intel::plans` — the parsers, the findings engine, the operator
//! tree and the diagram layout the desktop app's plan pane uses — so a plan
//! pasted on the site reads exactly as it does in the app.
//!
//! Recognizing the engine and unwrapping what terminal clients put around a
//! plan live in `sqlly_intel::plans::import`, shared with the app's Open
//! Plan / Paste Plan. What the shell adds is the page's own vocabulary
//! (format ids, visitor-facing errors) and the one JSON document the page
//! renders (`doc`). Everything here except the `bindings` module is plain
//! Rust, unit-tested natively.

use std::cell::RefCell;

use serde_json::{Value, json};
use sqlly_intel::plans::{
    DEFAULT_MAX_COMPARISON_NODES, ExecutionPlan, ImportedPlan, PlanAnalysis, PlanConfidence,
    PlanContext, PlanEngine, PlanError, SchemaContext, analysis::resolve_dialect, batch_story,
    clean_plan_text, decode_plan_bytes, detect_plan_format, explain_plan_changes,
    findings::findings_for_plan_with_context, import_plan_document, import_plan_document_as,
    resolve_batch_aliases,
};

mod doc;
mod samples;

pub use doc::{DiagramRequest, warning_label};
pub use samples::{SAMPLES, Sample, samples_json};

/// An engine whose plans the viewer reads. The ids are what the page's
/// format picker sends; keep them in sync with
/// `QueryPlanViewerModel.Formats`. Which of the engine's output formats a
/// document is (text, JSON, XML, a client's table) is worked out from the
/// document itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    SqlServer,
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    DuckDb,
    ClickHouse,
    Oracle,
    CockroachDb,
    Trino,
    Spark,
}

impl Format {
    pub const ALL: [Format; 11] = [
        Format::SqlServer,
        Format::Postgres,
        Format::MySql,
        Format::MariaDb,
        Format::Sqlite,
        Format::DuckDb,
        Format::ClickHouse,
        Format::Oracle,
        Format::CockroachDb,
        Format::Trino,
        Format::Spark,
    ];

    /// Spelled out (the same ids as `PlanEngine::id`) because the site's
    /// tests read this function to check the page's picker against it.
    pub fn id(self) -> &'static str {
        match self {
            Format::SqlServer => "sqlserver",
            Format::Postgres => "postgresql",
            Format::MySql => "mysql",
            Format::MariaDb => "mariadb",
            Format::Sqlite => "sqlite",
            Format::DuckDb => "duckdb",
            Format::ClickHouse => "clickhouse",
            Format::Oracle => "oracle",
            Format::CockroachDb => "cockroachdb",
            Format::Trino => "trino",
            Format::Spark => "spark",
        }
    }

    /// The shared plan module's engine for this format.
    pub fn plan_engine(self) -> PlanEngine {
        match self {
            Format::SqlServer => PlanEngine::SqlServer,
            Format::Postgres => PlanEngine::Postgres,
            Format::MySql => PlanEngine::MySql,
            Format::MariaDb => PlanEngine::MariaDb,
            Format::Sqlite => PlanEngine::Sqlite,
            Format::DuckDb => PlanEngine::DuckDb,
            Format::ClickHouse => PlanEngine::ClickHouse,
            Format::Oracle => PlanEngine::Oracle,
            Format::CockroachDb => PlanEngine::CockroachDb,
            Format::Trino => PlanEngine::Trino,
            Format::Spark => PlanEngine::Spark,
        }
    }

    pub fn from_plan_engine(engine: PlanEngine) -> Format {
        match engine {
            PlanEngine::SqlServer => Format::SqlServer,
            PlanEngine::Postgres => Format::Postgres,
            PlanEngine::MySql => Format::MySql,
            PlanEngine::MariaDb => Format::MariaDb,
            PlanEngine::Sqlite => Format::Sqlite,
            PlanEngine::DuckDb => Format::DuckDb,
            PlanEngine::ClickHouse => Format::ClickHouse,
            PlanEngine::Oracle => Format::Oracle,
            PlanEngine::CockroachDb => Format::CockroachDb,
            PlanEngine::Trino => Format::Trino,
            PlanEngine::Spark => Format::Spark,
        }
    }

    pub fn from_id(id: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.id() == id.trim())
    }

    /// The engine, as the page names it.
    pub fn engine(self) -> &'static str {
        self.plan_engine().name()
    }
}

/// Undo what clients wrap around a plan (see
/// `sqlly_intel::plans::clean_plan_text`).
pub fn clean_input(text: &str) -> String {
    clean_plan_text(text)
}

/// Decode the bytes of a plan file: UTF-8, or UTF-16 with or without a byte
/// order mark (SSMS saves `.sqlplan` as UTF-16).
pub fn decode_bytes(bytes: &[u8]) -> String {
    decode_plan_bytes(bytes)
}

/// What the pasted text is, as one of the viewer's formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    Format(Format),
    /// Recognizable, but not a plan the viewer reads (SQLite bytecode, a
    /// ClickHouse syntax tree). The message says what it is and what to
    /// run instead.
    Unsupported(String),
    Unknown,
}

/// Recognize the engine that produced `text` (already [`clean_input`]ed).
pub fn detect(text: &str) -> Detected {
    if let Some(found) = detect_plan_format(text) {
        return Detected::Format(Format::from_plan_engine(found.format.engine()));
    }
    // Not a plan format: the importer's error tells a recognizable
    // non-plan (it names what the text looks like) from plain noise.
    match import_plan_document(text) {
        Err(PlanError::Import(error)) if error.format_label.is_some() => {
            Detected::Unsupported(error.to_string())
        }
        _ => Detected::Unknown,
    }
}

/// Why a plan could not be read, in the pieces the page lays out: what the
/// text looks like, where reading stopped, and how to capture a plan that
/// reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadError {
    /// The whole explanation as one paragraph.
    pub message: String,
    /// What the text looks like (`PostgreSQL EXPLAIN (text)`).
    pub label: Option<String>,
    /// What went wrong, as a sentence.
    pub detail: Option<String>,
    /// 1-based position in the cleaned text where reading stopped.
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// How to capture a plan the viewer reads.
    pub hint: Option<String>,
    /// The engine the text looks like, when one was recognized.
    pub format: Option<Format>,
}

impl ReadError {
    fn plain(message: impl Into<String>) -> Box<Self> {
        Box::new(Self {
            message: message.into(),
            ..Self::default()
        })
    }

    fn from_plan_error(error: &PlanError) -> Box<Self> {
        let message = error.message().fallback().to_string();
        let PlanError::Import(import) = error else {
            return Self::plain(message);
        };
        let detail = import.detail.trim().trim_end_matches('.');
        let mut chars = detail.chars();
        let detail = chars
            .next()
            .map(|first| format!("{}{}.", first.to_uppercase(), chars.as_str()));
        Box::new(Self {
            message,
            label: import.format_label.clone(),
            detail,
            line: import.line,
            column: import.column,
            hint: import.hint.clone(),
            format: import
                .format_id
                .and_then(sqlly_intel::plans::PlanFormat::from_id)
                .map(|format| Format::from_plan_engine(format.engine())),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "message": self.message,
            "label": self.label,
            "detail": self.detail,
            "line": self.line,
            "column": self.column,
            "hint": self.hint,
            "format": self.format.map(Format::id),
            "engine": self.format.map(Format::engine),
        })
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Read `text` as a plan from `format`'s engine, or from whichever engine
/// it looks like when `format` is `None`.
pub fn import(text: &str, format: Option<Format>) -> Result<ImportedPlan, Box<ReadError>> {
    let result = match format {
        Some(format) => import_plan_document_as(format.plan_engine(), text),
        None => import_plan_document(text),
    };
    let imported = result.map_err(|e| ReadError::from_plan_error(&e))?;
    if imported.plans.iter().all(|p| p.nodes.is_empty()) {
        return Err(Box::new(ReadError {
            label: Some(imported.format.label().to_string()),
            format: Some(Format::from_plan_engine(imported.format.engine())),
            ..*ReadError::plain(format!(
                "The {} plan has no operators to show. If it came from a batch, the statements that ran may have produced no plan (SET and DECLARE do not).",
                imported.format.engine().name()
            ))
        }));
    }
    Ok(imported)
}

/// Parse `text` as `format` into one plan per statement.
pub fn parse(text: &str, format: Format) -> Result<Vec<ExecutionPlan>, Box<ReadError>> {
    import(text, Some(format)).map(|imported| imported.plans)
}

/// What the visitor supplied besides the plan.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// A [`Format::id`], or empty / `"auto"` to recognize the engine.
    pub format: String,
    /// Table definitions: `CREATE TABLE` / `CREATE INDEX` statements, and
    /// optionally `table: rows` lines.
    pub schema: String,
    /// The SQL the plan is for.
    pub query: String,
    /// How to lay out the first diagram.
    pub diagram: DiagramRequest,
}

impl Options {
    pub fn format(format: &str) -> Self {
        Self {
            format: format.to_string(),
            ..Self::default()
        }
    }

    /// Read the page's options object. A bare string is a format id.
    pub fn from_json(options: &Value) -> Self {
        if let Some(format) = options.as_str() {
            return Self::format(format);
        }
        let text = |key: &str| {
            options
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            format: text("format"),
            schema: text("schema"),
            query: text("query"),
            diagram: DiagramRequest::from_json(options),
        }
    }
}

/// The plans of the last successful [`analyze`], kept so the page can ask
/// for a new diagram layout (collapse a subtree, turn the tree, colour by
/// another metric) without sending the plan again.
struct Session {
    plans: Vec<ExecutionPlan>,
    analyses: Vec<PlanAnalysis>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

fn picked_format(format_id: &str) -> Result<Option<Format>, Box<ReadError>> {
    match format_id.trim() {
        "" | "auto" => Ok(None),
        id => Format::from_id(id)
            .map(Some)
            .ok_or_else(|| ReadError::plain(format!("unknown plan format \"{id}\""))),
    }
}

/// Plans with their statement's place in the document.
type NumberedPlans = Vec<(usize, ExecutionPlan)>;

/// The statements of `text` that have operators, with aliases untouched.
fn read_statements(
    text: &str,
    picked: Option<Format>,
) -> Result<(ImportedPlan, NumberedPlans), Box<ReadError>> {
    let mut imported = import(text, picked)?;
    let plans = std::mem::take(&mut imported.plans)
        .into_iter()
        .enumerate()
        .filter(|(_, plan)| !plan.nodes.is_empty())
        .collect();
    Ok((imported, plans))
}

/// Analyze `text` and return the page's JSON document.
///
/// `format` in the result is the engine the plan was read as. It differs
/// from a picked format only when the text is certainly another engine's
/// plan, in which case `detected` is true.
pub fn analyze(text: &str, options: &Options) -> Result<Value, Box<ReadError>> {
    let cleaned = clean_input(text);
    if cleaned.is_empty() {
        return Err(ReadError::plain(
            "Paste an execution plan, open a plan file, or pick a sample.",
        ));
    }
    let picked = picked_format(&options.format)?;
    let (imported, numbered) = read_statements(text, picked)?;
    let format = Format::from_plan_engine(imported.format.engine());
    let detection = detect_plan_format(&cleaned).filter(|d| d.format == imported.format);
    let total_statements = numbered.len();
    let (indexes, mut plans): (Vec<usize>, Vec<ExecutionPlan>) = numbered.into_iter().unzip();
    let dialect = resolve_dialect(&plans[0]);

    let schema = (!options.schema.trim().is_empty())
        .then(|| SchemaContext::from_ddl(&options.schema, dialect));
    let usable_schema = schema.as_ref().filter(|s| !s.is_empty());
    let query = Some(options.query.as_str()).filter(|q| !q.trim().is_empty());

    // What the same plans say with the statement text but no table
    // definitions, to show which findings the definitions changed.
    let without_schema = usable_schema.map(|_| {
        let mut base = plans.clone();
        let context = PlanContext::new().with_query_text(query);
        resolve_batch_aliases(&mut base, &context);
        base.iter()
            .map(|plan| findings_for_plan_with_context(plan, &context))
            .collect::<Vec<_>>()
    });

    let context = PlanContext::new()
        .with_schema(usable_schema)
        .with_query_text(query);
    let aliases = resolve_batch_aliases(&mut plans, &context);
    let batch = batch_story(&plans);

    let statements: Vec<Value> = batch
        .statements
        .iter()
        .enumerate()
        .map(|(position, statement)| {
            let plan = &plans[position];
            let findings = findings_for_plan_with_context(plan, &context);
            doc::statement_json(&doc::StatementInput {
                index: indexes[position],
                plan,
                analysis: &statement.analysis,
                story: &statement.story,
                share: statement.share,
                findings: &findings,
                without_schema: without_schema
                    .as_ref()
                    .map(|lists| lists[position].as_slice()),
                aliases: aliases.get(position).map(Vec::as_slice).unwrap_or_default(),
                query_text: if total_statements == 1 {
                    &options.query
                } else {
                    ""
                },
                diagram: &options.diagram,
            })
        })
        .collect();

    let result = json!({
        "format": format.id(),
        "engine": format.engine(),
        "sourceFormat": imported.format.id(),
        "sourceFormatLabel": imported.format.label(),
        "detected": picked != Some(format),
        "detection": detection.map(|d| json!({
            "confidence": match d.confidence {
                PlanConfidence::Certain => "certain",
                PlanConfidence::Likely => "likely",
                PlanConfidence::Guess => "guess",
            },
            "evidence": d.evidence,
        })),
        "printsAliases": sqlly_intel::plans::dialect_prints_aliases(dialect),
        "aliasesResolved": aliases.iter().map(Vec::len).sum::<usize>(),
        "schema": schema.as_ref().map(|s| doc::schema_json(s, dialect)),
        "headline": batch.headline.as_ref().map(|h| h.fallback().to_string()),
        "statements": statements,
    });

    let analyses = batch.statements.into_iter().map(|s| s.analysis).collect();
    SESSION.with(|session| {
        *session.borrow_mut() = Some(Session { plans, analyses });
    });
    Ok(result)
}

/// Lay the diagram of statement `position` (its place in the last
/// [`analyze`] result's `statements`) out again.
pub fn layout_diagram(position: usize, request: &DiagramRequest) -> Option<Value> {
    SESSION.with(|session| {
        let session = session.borrow();
        let session = session.as_ref()?;
        let plan = session.plans.get(position)?;
        let analysis = session.analyses.get(position)?;
        Some(doc::diagram_json(plan, analysis, request))
    })
}

/// Explain what changed between two captures of the same statements:
/// `before` is the earlier plan, `after` the one being viewed.
pub fn compare(
    before: &str,
    after: &str,
    format_id: &str,
    query: &str,
) -> Result<Value, Box<ReadError>> {
    if clean_input(before).is_empty() {
        return Err(ReadError::plain(
            "Paste the earlier plan to compare against.",
        ));
    }
    let picked = picked_format(format_id)?;
    let context = PlanContext::new().with_query_text(Some(query));
    let read = |text: &str| -> Result<(Format, Vec<ExecutionPlan>), Box<ReadError>> {
        let (imported, numbered) = read_statements(text, picked)?;
        let mut plans: Vec<ExecutionPlan> = numbered.into_iter().map(|(_, plan)| plan).collect();
        resolve_batch_aliases(&mut plans, &context);
        Ok((Format::from_plan_engine(imported.format.engine()), plans))
    };
    let (before_format, before_plans) = read(before)?;
    let (after_format, after_plans) = read(after)?;
    if before_format != after_format {
        return Err(ReadError::plain(format!(
            "The earlier plan is from {} and the current one is from {}. Compare two plans of the same statement from the same engine.",
            before_format.engine(),
            after_format.engine()
        )));
    }
    let report = explain_plan_changes(&before_plans, &after_plans, DEFAULT_MAX_COMPARISON_NODES)
        .map_err(|e| ReadError::plain(format!("{}.", e.message().fallback())))?;
    Ok(doc::changes_json(&report))
}

/// How to capture a plan the viewer reads, per engine.
pub fn capture_help_json() -> Value {
    Value::Array(
        Format::ALL
            .into_iter()
            .map(|format| {
                json!({
                    "format": format.id(),
                    "engine": format.engine(),
                    "hint": format.plan_engine().capture_hint(),
                })
            })
            .collect(),
    )
}

fn envelope(result: Result<Value, Box<ReadError>>) -> Value {
    match result {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("ok".to_string(), Value::Bool(true));
            }
            value
        }
        Err(error) => json!({ "ok": false, "error": error.to_json() }),
    }
}

/// [`analyze`] as the page calls it: options as a JSON string, and a result
/// that is always a JSON object with `ok` (`error` holds a [`ReadError`]
/// when it is false).
pub fn analyze_json(text: &str, options: &str) -> String {
    let options = serde_json::from_str::<Value>(options)
        .map(|value| Options::from_json(&value))
        .unwrap_or_else(|_| Options::format(options));
    envelope(analyze(text, &options)).to_string()
}

/// [`compare`] with the same envelope as [`analyze_json`], under `changes`.
pub fn compare_json(before: &str, after: &str, format_id: &str, query: &str) -> String {
    envelope(compare(before, after, format_id, query).map(|changes| json!({ "changes": changes })))
        .to_string()
}

/// The JavaScript surface, generated by wasm-bindgen. Present only on wasm.
#[cfg(target_arch = "wasm32")]
mod bindings {
    use wasm_bindgen::prelude::*;

    /// `analyzePlan(text, options) -> string` (JSON). `options` is a JSON
    /// object `{ format, schema, query, orientation, metric, collapsed }`.
    /// The result always parses: `{ ok: true, ... }` or
    /// `{ ok: false, error: { message, label, detail, line, column, hint } }`.
    #[wasm_bindgen(js_name = analyzePlan)]
    pub fn analyze_plan(text: &str, options: &str) -> String {
        super::analyze_json(text, options)
    }

    /// `layoutDiagram(statement, options) -> string` (JSON, or "" when no
    /// plan is loaded): the diagram of one statement of the last analyzed
    /// plan, laid out for `{ orientation, metric, collapsed }`.
    #[wasm_bindgen(js_name = layoutDiagram)]
    pub fn layout_diagram(statement: usize, options: &str) -> String {
        let options = serde_json::from_str(options).unwrap_or(serde_json::Value::Null);
        super::layout_diagram(statement, &super::DiagramRequest::from_json(&options))
            .map(|value| value.to_string())
            .unwrap_or_default()
    }

    /// `comparePlans(before, after, format, query) -> string` (JSON).
    #[wasm_bindgen(js_name = comparePlans)]
    pub fn compare_plans(before: &str, after: &str, format: &str, query: &str) -> String {
        super::compare_json(before, after, format, query)
    }

    /// `decodePlanBytes(bytes) -> string`: the text of a plan file, UTF-8
    /// or UTF-16.
    #[wasm_bindgen(js_name = decodePlanBytes)]
    pub fn decode_plan_bytes(bytes: &[u8]) -> String {
        super::decode_bytes(bytes)
    }

    /// `detectPlanFormat(text) -> string`: a format id, or "" when unknown.
    #[wasm_bindgen(js_name = detectPlanFormat)]
    pub fn detect_plan_format(text: &str) -> String {
        match super::detect(&super::clean_input(text)) {
            super::Detected::Format(f) => f.id().to_string(),
            _ => String::new(),
        }
    }

    /// `samplePlans() -> string`: JSON array of the bundled real plans.
    #[wasm_bindgen(js_name = samplePlans)]
    pub fn sample_plans() -> String {
        super::samples_json().to_string()
    }

    /// `captureHelp() -> string`: JSON array of per-engine capture advice.
    #[wasm_bindgen(js_name = captureHelp)]
    pub fn capture_help() -> String {
        super::capture_help_json().to_string()
    }

    /// Version of this wrapper crate, for bug reports.
    #[wasm_bindgen(js_name = viewerVersion)]
    pub fn viewer_version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

#[cfg(test)]
mod tests;
