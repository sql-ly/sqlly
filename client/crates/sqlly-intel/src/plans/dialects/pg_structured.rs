//! PostgreSQL `EXPLAIN (FORMAT YAML)` and `EXPLAIN (FORMAT XML)`. Both carry
//! exactly the keys of `FORMAT JSON`, so each is decoded into the same
//! value and handed to the JSON walker in `pg`.

use std::borrow::Cow;

use serde_json::{Map, Value};

use super::pg::plans_from_pg_value;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::ExecutionPlan;

const MAX_DEPTH: usize = 512;

/// Parse `EXPLAIN (FORMAT YAML)` output.
pub fn parse_pg_explain_yaml(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let doc = unwrap_psql_cell(doc);
    let lines: Vec<YamlLine> = doc
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let text = line.trim_start_matches(' ');
            YamlLine {
                indent: line.len() - text.len(),
                text: text.trim_end(),
            }
        })
        .collect();
    if lines.is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    let mut position = 0;
    let value = yaml_block(&lines, &mut position, lines[0].indent, 0);
    plans_from_pg_value(&value, "postgresql-yaml").map_err(|_| {
        PlanError::InvalidStructure(
            "PostgreSQL EXPLAIN YAML: no \"Plan:\" entry found. Expected the output of EXPLAIN (FORMAT YAML), which starts with \"- Plan:\".".into(),
        )
    })
}

/// True when `doc` is PostgreSQL's YAML EXPLAIN: it starts with `- Plan:`
/// and the plan's first key is `Node Type`.
pub fn looks_like_pg_explain_yaml(doc: &str) -> bool {
    let doc = unwrap_psql_cell(doc);
    let mut lines = doc.lines().map(str::trim).filter(|line| !line.is_empty());
    matches!(lines.next(), Some("- Plan:" | "Plan:"))
        && lines
            .next()
            .is_some_and(|line| line.starts_with("Node Type:"))
}

struct YamlLine<'a> {
    indent: usize,
    text: &'a str,
}

fn yaml_scalar(text: &str) -> Value {
    let text = text.trim();
    if text.starts_with('"') {
        // PostgreSQL writes YAML strings with JSON escaping.
        if let Ok(value) = serde_json::from_str::<String>(text) {
            return Value::String(value);
        }
        return Value::String(text.trim_matches('"').to_string());
    }
    match text {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "" | "~" | "null" => return Value::Null,
        _ => {}
    }
    if let Ok(number) = text.parse::<f64>()
        && number.is_finite()
        && let Some(number) = serde_json::Number::from_f64(number)
    {
        return Value::Number(number);
    }
    Value::String(text.to_string())
}

/// `key: value` or `key:` -> (key, value text).
fn yaml_entry(text: &str) -> Option<(&str, &str)> {
    if text.starts_with('"') {
        return None;
    }
    if let Some(at) = text.find(": ") {
        return Some((&text[..at], text[at + 2..].trim()));
    }
    text.strip_suffix(':').map(|key| (key, ""))
}

/// Read the block that starts at `position` and is indented by `indent`.
fn yaml_block(lines: &[YamlLine], position: &mut usize, indent: usize, depth: usize) -> Value {
    let Some(first) = lines.get(*position) else {
        return Value::Null;
    };
    if depth > MAX_DEPTH {
        // Skip what is nested too deep to be a plan.
        while lines
            .get(*position)
            .is_some_and(|line| line.indent >= indent)
        {
            *position += 1;
        }
        return Value::Null;
    }
    if first.text.starts_with("- ") || first.text == "-" {
        let mut items = Vec::new();
        while let Some(line) = lines.get(*position) {
            if line.indent != indent || !(line.text.starts_with("- ") || line.text == "-") {
                break;
            }
            let rest = line.text[1..].trim_start();
            let inner_indent = indent + (line.text.len() - rest.len());
            if yaml_entry(rest).is_some() {
                // A mapping whose first entry shares the dash's line.
                let mut object = Map::new();
                yaml_mapping_entry(lines, position, rest, inner_indent, depth, &mut object);
                yaml_mapping(lines, position, inner_indent, depth, &mut object);
                items.push(Value::Object(object));
            } else {
                *position += 1;
                items.push(yaml_scalar(rest));
            }
        }
        return Value::Array(items);
    }
    let mut object = Map::new();
    yaml_mapping(lines, position, indent, depth, &mut object);
    if object.is_empty() {
        // Not a mapping after all: consume the line so the caller advances.
        *position += 1;
        return yaml_scalar(first.text);
    }
    Value::Object(object)
}

fn yaml_mapping(
    lines: &[YamlLine],
    position: &mut usize,
    indent: usize,
    depth: usize,
    object: &mut Map<String, Value>,
) {
    while let Some(line) = lines.get(*position) {
        if line.indent != indent || line.text.starts_with("- ") || yaml_entry(line.text).is_none() {
            break;
        }
        yaml_mapping_entry(lines, position, line.text, indent, depth, object);
    }
}

/// Consume one `key: value` entry whose text is `text` on the current line.
fn yaml_mapping_entry(
    lines: &[YamlLine],
    position: &mut usize,
    text: &str,
    indent: usize,
    depth: usize,
    object: &mut Map<String, Value>,
) {
    *position += 1;
    let Some((key, value)) = yaml_entry(text) else {
        return;
    };
    let value = if value.is_empty() {
        match lines.get(*position) {
            // Nested block: deeper, or a list at the same indent.
            Some(next)
                if next.indent > indent
                    || (next.indent == indent && next.text.starts_with("- ")) =>
            {
                let nested_indent = next.indent;
                let nested = yaml_block(lines, position, nested_indent, depth + 1);
                // Drop anything left at a deeper indent that did not parse.
                while lines
                    .get(*position)
                    .is_some_and(|line| line.indent > indent)
                {
                    *position += 1;
                }
                nested
            }
            _ => Value::Null,
        }
    } else {
        yaml_scalar(value)
    };
    object.insert(key.to_string(), value);
}

/// Parse `EXPLAIN (FORMAT XML)` output.
pub fn parse_pg_explain_xml(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let doc = unwrap_psql_cell(doc);
    let xml = roxmltree::Document::parse(doc.trim())
        .map_err(|e| PlanError::XmlParse(format!("PostgreSQL EXPLAIN XML: {e}")))?;
    let root = xml.root_element();
    if root.tag_name().name() != "explain" {
        return Err(PlanError::InvalidStructure(format!(
            "PostgreSQL EXPLAIN XML: the root element is <{}>, expected <explain>.",
            root.tag_name().name()
        )));
    }
    let queries: Vec<Value> = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "Query")
        .map(|query| xml_object(query, 0))
        .collect();
    plans_from_pg_value(&Value::Array(queries), "postgresql-xml").map_err(|_| {
        PlanError::InvalidStructure(
            "PostgreSQL EXPLAIN XML: no <Plan> element found under <explain><Query>.".into(),
        )
    })
}

/// True when `doc` is PostgreSQL's XML EXPLAIN (root `<explain>` in the
/// `http://www.postgresql.org/2009/explain` namespace).
pub fn looks_like_pg_explain_xml(doc: &str) -> bool {
    let doc = unwrap_psql_cell(doc);
    let head: String = doc.trim_start().chars().take(400).collect();
    head.contains("<explain") && head.contains("postgresql.org/2009/explain")
}

fn xml_object(element: roxmltree::Node, depth: usize) -> Value {
    let mut object = Map::new();
    for child in element.children().filter(roxmltree::Node::is_element) {
        let key = child.tag_name().name().replace('-', " ");
        object.insert(key, xml_value(child, depth + 1));
    }
    Value::Object(object)
}

fn xml_value(element: roxmltree::Node, depth: usize) -> Value {
    let children: Vec<roxmltree::Node> = element
        .children()
        .filter(roxmltree::Node::is_element)
        .collect();
    if children.is_empty() {
        return Value::String(element.text().unwrap_or_default().trim().to_string());
    }
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    let first = children[0].tag_name().name();
    let is_list = match element.tag_name().name() {
        "Plans" | "Workers" | "Triggers" | "Target-Tables" | "Grouping-Sets" | "Group-Keys" => true,
        _ => first == "Item",
    };
    if is_list {
        return Value::Array(
            children
                .into_iter()
                .map(|child| {
                    if child.children().any(|node| node.is_element()) {
                        xml_object(child, depth + 1)
                    } else {
                        Value::String(child.text().unwrap_or_default().trim().to_string())
                    }
                })
                .collect(),
        );
    }
    xml_object(element, depth)
}

/// Undo psql's aligned output around a single-cell result: the `QUERY PLAN`
/// header, the rule under it, the ` +` that marks a line break inside the
/// cell, the `(1 row)` footer and the one-space left margin.
pub(crate) fn unwrap_psql_cell(doc: &str) -> Cow<'_, str> {
    let mut lines: Vec<&str> = doc.lines().map(|line| line.trim_end()).collect();
    let is_rule = |line: &str| {
        let line = line.trim();
        line.len() >= 3 && line.chars().all(|c| matches!(c, '-' | '+'))
    };
    let is_footer = |line: &str| {
        let line = line.trim();
        line.strip_prefix('(')
            .and_then(|l| l.strip_suffix(" rows)").or_else(|| l.strip_suffix(" row)")))
            .is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
    };
    let header = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .filter(|&at| {
            lines[at].trim() == "QUERY PLAN" && lines.get(at + 1).is_some_and(|l| is_rule(l))
        });
    let mut changed = false;
    if let Some(at) = header {
        lines.drain(..at + 2);
        changed = true;
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.last().is_some_and(|line| is_footer(line)) {
        lines.pop();
        changed = true;
    }
    let content: Vec<usize> = (0..lines.len())
        .filter(|&at| !lines[at].trim().is_empty())
        .collect();
    let continued = content.len() >= 2
        && content[..content.len() - 1]
            .iter()
            .all(|&at| lines[at].ends_with('+'));
    if !changed && !continued {
        return Cow::Borrowed(doc);
    }
    let margin = content
        .iter()
        .map(|&at| lines[at].len() - lines[at].trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(doc.len());
    for (at, line) in lines.iter().enumerate() {
        let mut line = *line;
        if continued && at != *content.last().unwrap_or(&0) {
            line = line.strip_suffix('+').unwrap_or(line).trim_end();
        }
        out.push_str(line.get(margin..).unwrap_or(line.trim_start()));
        out.push('\n');
    }
    Cow::Owned(out)
}
