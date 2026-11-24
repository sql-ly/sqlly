//! English text of every finding string, next to the rule that uses it.
//!
//! Each rule file exports a `TEMPLATES` table of `(short key, English text,
//! what the string is)`. The full catalog key is
//! `services.plans.findings.<short key>`; the translation catalogs carry the
//! same keys with the same `{placeholders}`. [`tr`] renders the English
//! fallback and attaches the arguments, so the UI can render the translated
//! template with the same values.

use sqlly_core::DisplayMessage;
use std::collections::HashMap;
use std::sync::OnceLock;

/// `(short key, English template, description for translators)`.
pub(crate) type Template = (&'static str, &'static str, &'static str);

/// Argument list of a finding string.
pub(crate) type Args = Vec<(&'static str, String)>;

const PREFIX: &str = "services.plans.findings.";

/// Strings shared by every rule.
const COMMON: &[Template] = &[
    (
        "common.detail",
        "Operator: {operator} (node {node_id})\n{evidence}",
        "technical detail of a finding tied to one operator; {evidence} is a list of measured values, one per line",
    ),
    (
        "common.detail_statement",
        "Whole statement\n{evidence}",
        "technical detail of a finding about the whole statement; {evidence} is a list of measured values, one per line",
    ),
    (
        "impact.time",
        "About {percent} of this statement's run time ({time}) is spent in this part of the plan.",
        "estimated impact line under a finding, from measured time",
    ),
    (
        "impact.cost",
        "About {percent} of this statement's estimated cost is in this part of the plan.",
        "estimated impact line under a finding, from the optimizer's cost estimate",
    ),
    (
        "impact.planned_time",
        "The steps planned from this estimate take about {percent} of this statement's run time ({time}).",
        "estimated impact line under a row-estimate finding, from measured time: the share is that of the joins, sorts and aggregates above the misestimated step",
    ),
    (
        "impact.planned_cost",
        "The steps planned from this estimate carry about {percent} of this statement's estimated cost.",
        "estimated impact line under a row-estimate finding, from the optimizer's cost estimate: the share is that of the joins, sorts and aggregates above the misestimated step",
    ),
    (
        "impact.optimizer",
        "The optimizer estimates this would cut the statement's cost by about {percent}.",
        "estimated impact line under a missing-index finding, from the optimizer's own impact figure",
    ),
    (
        "common.note.alias",
        "The plan shows {table} as it is named in the query, which may be an alias. Replace it with the real table name before running the statement.",
        "note under a finding whose SQL uses a table name that may be a query alias",
    ),
    (
        "common.note.write_cost",
        "Every index has a cost: each INSERT, UPDATE and DELETE on {table} has to maintain it, and it takes disk space.",
        "note under a finding that suggests a new index",
    ),
    (
        "common.note.same_index",
        "Another finding suggests an index on {table} ({index_columns}) that serves this one too. Create that index once.",
        "note under a finding whose index suggestion was merged into another finding's; {index_columns} is that index's key column list",
    ),
    (
        "common.note.second_index",
        "Another finding suggests a different index on {table}. Each index adds write cost, so create the one for the more expensive finding first and capture the plan again.",
        "note under a finding that suggests a second, different index on the same table",
    ),
    (
        "common.note.extends_index",
        "The existing index {index} is a prefix of this one. Once the new index is in place the old one is redundant and can be dropped.",
        "note under an index suggestion that extends an index already in the schema",
    ),
    (
        "common.note.rebuilds_index",
        "This rebuilds the existing index {index} with the extra columns in one step, so no second index is added. The rebuild takes time and locks on a large table.",
        "note under an index suggestion that rebuilds an existing index with more columns",
    ),
    (
        "common.note.needs_schema",
        "With the table definitions supplied, this advice could check the indexes that already exist and name the exact columns.",
        "note under a finding saying what supplying the schema would add",
    ),
    (
        "common.note.estimated_only",
        "This plan has estimates only. Capture an executed plan to confirm the row counts before changing anything.",
        "note under a finding produced from an estimated plan",
    ),
    (
        "common.action.index_exists",
        "{table} already has an index that fits this: {index}. The plan does not use it here, so another index would not help. Check that statistics are current and that the compared values have the column's type.",
        "action shown instead of a CREATE INDEX when the supplied schema already has that index; {index} is its name and key columns",
    ),
    (
        "common.alt.drop_index",
        "After creating the new index, drop the one it replaces",
        "label of an alternative SQL statement that drops a now-redundant index",
    ),
    (
        "evidence.table",
        "Table: {value}",
        "evidence line: table name",
    ),
    (
        "evidence.index",
        "Index: {value}",
        "evidence line: index name",
    ),
    (
        "evidence.filter",
        "Filter: {value}",
        "evidence line: the predicate applied after rows are read",
    ),
    (
        "evidence.bitmap_filter",
        "Bitmap filter from a join, on: {value}",
        "evidence line: SQL Server PROBE bitmap filter pushed down to a read by a join; {value} is the column list",
    ),
    (
        "evidence.index_condition",
        "Index condition: {value}",
        "evidence line: the predicate the index satisfies",
    ),
    (
        "evidence.join_condition",
        "Join condition: {value}",
        "evidence line: the join predicate",
    ),
    (
        "evidence.rows_read",
        "Rows read: {value}",
        "evidence line: rows read from storage",
    ),
    (
        "evidence.rows_returned",
        "Rows returned: {value}",
        "evidence line: rows the operator produced",
    ),
    (
        "evidence.rows_removed",
        "Rows removed by filter: {value}",
        "evidence line: rows read and then discarded",
    ),
    (
        "evidence.estimated_rows",
        "Estimated rows: {value}",
        "evidence line: the optimizer's row estimate",
    ),
    (
        "evidence.actual_rows",
        "Actual rows: {value}",
        "evidence line: measured row count",
    ),
    (
        "evidence.executions",
        "Executions: {value}",
        "evidence line: how many times the operator ran",
    ),
    (
        "evidence.time",
        "Time: {value}",
        "evidence line: measured time including inputs",
    ),
    (
        "evidence.own_time",
        "Own time: {value}",
        "evidence line: measured time of the operator alone",
    ),
    (
        "evidence.cost_share",
        "Share of estimated cost: {value}",
        "evidence line: share of the optimizer's cost estimate",
    ),
    (
        "evidence.sort_keys",
        "Sort keys: {value}",
        "evidence line: columns a sort orders by",
    ),
    (
        "evidence.sort_method",
        "Sort method: {value}",
        "evidence line: sort algorithm the engine reports",
    ),
    (
        "evidence.disk",
        "Written to disk: {value}",
        "evidence line: size of sort or hash work written to disk",
    ),
    (
        "evidence.memory",
        "Memory used: {value}",
        "evidence line: memory the operator used",
    ),
    (
        "evidence.hash_batches",
        "Hash batches: {value}",
        "evidence line: number of batches a hash was split into",
    ),
    (
        "evidence.heap_fetches",
        "Heap fetches: {value}",
        "evidence line: PostgreSQL index-only scan heap fetches",
    ),
    (
        "evidence.heap_blocks",
        "Heap blocks: {value}",
        "evidence line: PostgreSQL bitmap heap scan exact and lossy block counts",
    ),
    (
        "evidence.workers",
        "Workers: {value}",
        "evidence line: parallel workers planned and launched",
    ),
    (
        "evidence.threads",
        "Rows per thread: {value}",
        "evidence line: rows each parallel thread processed",
    ),
    (
        "evidence.partitions",
        "Partitions: {value}",
        "evidence line: partitions scanned out of total",
    ),
    (
        "evidence.parts",
        "Parts and granules: {value}",
        "evidence line: ClickHouse parts and granules read",
    ),
    (
        "evidence.grant",
        "Memory grant: {value}",
        "evidence line: SQL Server memory grant sizes",
    ),
    (
        "evidence.parameter",
        "Parameter: {value}",
        "evidence line: a parameter with its compiled and runtime values",
    ),
    (
        "evidence.statistics",
        "Statistics: {value}",
        "evidence line: a statistics object and how stale it is",
    ),
    (
        "evidence.columns",
        "Columns: {value}",
        "evidence line: a list of column names",
    ),
    (
        "evidence.output_columns",
        "Columns returned: {value}",
        "evidence line: number or list of columns the operator returns",
    ),
    (
        "evidence.reason",
        "Reason given by the engine: {value}",
        "evidence line: a reason code the engine printed in the plan",
    ),
    (
        "evidence.planning_time",
        "Planning time: {value}",
        "evidence line: time the optimizer took",
    ),
    (
        "evidence.execution_time",
        "Execution time: {value}",
        "evidence line: time the statement took to run",
    ),
    (
        "evidence.jit_time",
        "JIT time: {value}",
        "evidence line: PostgreSQL JIT compilation time",
    ),
    (
        "evidence.udf_time",
        "Time in scalar functions: {value}",
        "evidence line: time spent inside scalar user-defined functions",
    ),
    (
        "evidence.existing_index",
        "Existing index: {value}",
        "evidence line: an index from the supplied schema",
    ),
    (
        "evidence.types",
        "Declared types: {value}",
        "evidence line: declared column types from the supplied schema",
    ),
    (
        "evidence.warning",
        "Plan warning: {value}",
        "evidence line: a warning the engine attached to the plan",
    ),
    (
        "evidence.missing_index_impact",
        "Plan warning: missing index, estimated cost reduction {value}",
        "evidence line: the engine's missing-index warning; {value} is the share of the statement's estimated cost the engine expects the index to save, such as 66.8%",
    ),
];

/// Every table of templates, in catalog order.
pub(crate) fn all_templates() -> Vec<&'static Template> {
    [
        COMMON,
        super::rules_access::TEMPLATES,
        super::rules_estimates::TEMPLATES,
        super::rules_memory::TEMPLATES,
        super::rules_join::TEMPLATES,
        super::rules_predicate::TEMPLATES,
        super::rules_misc::TEMPLATES,
        super::rules_schema::TEMPLATES,
        super::rules_engines::TEMPLATES,
    ]
    .into_iter()
    .flatten()
    .collect()
}

struct Entry {
    key: &'static str,
    english: &'static str,
}

fn table() -> &'static HashMap<&'static str, Entry> {
    static TABLE: OnceLock<HashMap<&'static str, Entry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        all_templates()
            .into_iter()
            .map(|(short, english, _)| {
                // The catalog key lives for the program: it is built once per
                // template and handed to `DisplayMessage` as `&'static str`.
                let key: &'static str = Box::leak(format!("{PREFIX}{short}").into_boxed_str());
                (*short, Entry { key, english })
            })
            .collect()
    })
}

/// Builds the message for `short` (a key without the
/// `services.plans.findings.` prefix). Only the arguments the template uses
/// are attached.
pub(crate) fn tr(short: &str, args: &[(&'static str, String)]) -> DisplayMessage {
    let Some(entry) = table().get(short) else {
        debug_assert!(false, "no finding template for {short}");
        return DisplayMessage::raw(short.to_string());
    };
    let mut text = entry.english.to_string();
    let mut used: Vec<&(&'static str, String)> = Vec::new();
    for arg in args {
        let placeholder = format!("{{{}}}", arg.0);
        if entry.english.contains(&placeholder) && !used.iter().any(|seen| seen.0 == arg.0) {
            used.push(arg);
        }
    }
    // Substitute in one pass over the template so a value that itself
    // contains `{name}` is never substituted again.
    let mut out = String::with_capacity(text.len() + 32);
    let mut rest = text.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match used.iter().find(|arg| arg.0 == name) {
                    Some(arg) => out.push_str(&arg.1),
                    None => {
                        debug_assert!(false, "finding template {short} is missing {{{name}}}");
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    text = out;
    let mut message = DisplayMessage::new(entry.key, text);
    for arg in used {
        message = message.with_arg(arg.0, arg.1.clone());
    }
    message
}

/// Title-cased kind for the translators' `location` text
/// (`table_scan` -> `Table Scan`).
#[cfg(test)]
pub(crate) fn location(short: &str, what: &str) -> String {
    let segment = short.split('.').next().unwrap_or(short);
    let title = match segment {
        "common" | "evidence" | "impact" => "All findings".to_string(),
        other => other
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    format!("Execution Plan → Findings → {title}: {what}.")
}

/// Every finding string as a catalog entry `(key, English value, location)`,
/// for the plan catalog checks in `plans::phrases`.
#[cfg(test)]
pub(crate) fn catalog_entries() -> Vec<(String, String, String)> {
    all_templates()
        .into_iter()
        .map(|(short, english, what)| {
            (
                format!("{PREFIX}{short}"),
                english.to_string(),
                location(short, what),
            )
        })
        .collect()
}
