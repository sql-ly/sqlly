//! Layout decisions every renderer applies the same way: what renders
//! inline, what moves to an appendix, and the metadata lines of the cover.

use super::doc::{Block, CodeBlock, Placement, Provenance, ReportDoc, walk};
use super::labels::ReportLabels;

/// Whether `block` renders where it sits.
pub fn inline(block: &Block, doc: &ReportDoc) -> bool {
    match block {
        Block::Code(c) => doc.meta.layout.sql == Placement::Inline && !c.source.is_empty(),
        Block::Provenance(_) => doc.meta.layout.provenance == Placement::Inline,
        _ => true,
    }
}

pub fn appendix_sql(doc: &ReportDoc) -> Vec<&CodeBlock> {
    let mut out = Vec::new();
    if doc.meta.layout.sql == Placement::Appendix {
        walk(&doc.blocks, &mut |b| {
            if let Block::Code(c) = b
                && !c.source.is_empty()
            {
                out.push(c);
            }
        });
    }
    out
}

pub fn appendix_provenance(doc: &ReportDoc) -> Vec<&Provenance> {
    let mut out = Vec::new();
    if doc.meta.layout.provenance == Placement::Appendix {
        walk(&doc.blocks, &mut |b| {
            if let Block::Provenance(p) = b {
                out.push(p);
            }
        });
    }
    out
}

/// `(label, value)` lines for the cover, in a fixed order.
pub fn meta_lines(doc: &ReportDoc, labels: &ReportLabels) -> Vec<(String, String)> {
    let m = &doc.meta;
    let mut out = Vec::new();
    if let Some(a) = m.author.as_deref().filter(|a| !a.is_empty()) {
        out.push((labels.author.clone(), a.to_string()));
    }
    if let Some(g) = m.generated_at.as_deref().filter(|g| !g.is_empty()) {
        out.push((labels.generated.clone(), g.to_string()));
    }
    if let Some(n) = m.notebook_id.as_deref().filter(|n| !n.is_empty()) {
        out.push((labels.notebook.clone(), n.to_string()));
    }
    if let Some(v) = m.notebook_version.as_deref().filter(|v| !v.is_empty()) {
        out.push((labels.version.clone(), v.to_string()));
    }
    for c in &m.connections {
        let mut parts = vec![c.label.clone()];
        if let Some(db) = c.database.as_deref().filter(|d| !d.is_empty()) {
            parts.push(db.to_string());
        }
        if let Some(engine) = c.engine.as_deref().filter(|e| !e.is_empty()) {
            parts.push(engine.to_string());
        }
        match (
            c.environment.as_deref().filter(|e| !e.is_empty()),
            c.production,
        ) {
            (Some(env), _) => parts.push(env.to_string()),
            (None, true) => parts.push(labels.production.clone()),
            _ => {}
        }
        out.push((labels.connections.clone(), parts.join(" · ")));
    }
    for p in m.parameters.iter().filter(|p| !p.sensitive) {
        out.push((
            labels.parameters.clone(),
            format!("{} = {}", p.name, p.value),
        ));
    }
    if m.data_withheld {
        out.push((labels.status.clone(), labels.data_withheld.clone()));
    }
    out
}

/// Provenance fields as `(label, value)` pairs.
pub fn provenance_lines(p: &Provenance, labels: &ReportLabels) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |label: &str, value: &Option<String>| {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            out.push((label.to_string(), v.to_string()));
        }
    };
    push(&labels.cell, &p.cell_id);
    push(&labels.run, &p.run_id);
    push(&labels.connection, &p.connection);
    push(&labels.database, &p.database);
    push(&labels.environment, &p.environment);
    push(&labels.extracted, &p.extracted_at);
    push(&labels.sql_hash, &p.sql_hash);
    if let Some(n) = p.row_count {
        out.push((labels.rows.clone(), n.to_string()));
    }
    out
}
