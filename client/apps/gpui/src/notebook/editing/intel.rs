//! IntelliSense over referenced cells. The notebook hands each cell editor
//! a [`ReferenceContext`] (the other cells' names, kinds, states, columns
//! and values, the parameters, the attachments, and for Markdown cells the
//! slash menu); a SQL cell's editor asks this module for `{{…}}`
//! completions and hover cards, and runs its ordinary SQL completion over
//! text where every `{{x}}` is masked as a same-length identifier that the
//! merged catalog knows as a table with the cell's columns, so
//! `FROM {{orders}} o` then `o.` offers the columns of `orders`. Markdown
//! cells get their own completions from [`super::markdown`]. Line-local
//! scans only: nothing here parses the document.

use sqlly_intel::intellisense::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionObject, CompletionSource,
    HoverColumn, HoverInfo,
};

use crate::directive_completion::DirectiveCompletions;

/// Columns listed on a hover card before "… and N more".
pub const MAX_HOVER_COLUMNS: usize = 12;

/// Another cell a `{{…}}` can name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceEntry {
    /// What `{{…}}` holds for it: the cell's name, else its id.
    pub token: String,
    pub id: String,
    pub kind_label: String,
    pub state_label: String,
    /// `(name, type)`; the type is empty when only the query is known.
    pub columns: Vec<(String, String)>,
    /// Columns come from the cell's last output (else from its query).
    pub columns_from_output: bool,
    /// Markdown text shows this cell's value for `{{token}}` (a formula).
    pub shows_in_markdown: bool,
    /// The value Markdown shows, when there is one.
    pub markdown_value: Option<String>,
    /// Why the value may not be current (stale, partial input, not run).
    pub value_note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParamEntry {
    pub name: String,
    pub type_label: String,
    /// Extra `{{param.name.field}}` forms (`start`, `end` of a date range).
    pub fields: Vec<String>,
    /// The current value as Markdown shows it; `None` when unset. Never
    /// filled for a sensitive parameter.
    pub value: Option<String>,
    pub sensitive: bool,
}

/// An attachment cell a Markdown picture can name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentEntry {
    pub token: String,
    pub id: String,
    pub file_name: String,
    /// A PNG or JPEG picture (the only attachments Markdown shows).
    pub picture: bool,
}

/// Where a Markdown snippet is offered besides the slash menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SnippetScope {
    /// Starts a line: headings, lists, tables, fences, quotes, dividers.
    #[default]
    Block,
    /// Anywhere in a line: emphasis, code, links, pictures.
    Inline,
    /// Opens a `{{…}}` reference.
    Reference,
    /// Slash menu only: adds a cell.
    Cell,
}

/// What a slash-menu item does besides inserting its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashAction {
    /// Insert a cell of this kind below the Markdown cell.
    InsertCell(&'static str),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlashItem {
    pub label: String,
    pub detail: String,
    /// The text written, with `${1:…}` tab stops where it has some.
    pub insert: String,
    pub action: Option<SlashAction>,
    pub scope: SnippetScope,
}

/// Everything one cell editor's notebook completions draw on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceContext {
    pub cells: Vec<ReferenceEntry>,
    pub params: Vec<ParamEntry>,
    /// Empty for SQL cells (no slash menu there).
    pub slash: Vec<SlashItem>,
    /// Empty for SQL cells.
    pub attachments: Vec<AttachmentEntry>,
    pub param_kind_label: String,
    pub from_output_note: String,
    pub from_query_note: String,
}

impl ReferenceContext {
    pub(super) fn entry(&self, token: &str) -> Option<&ReferenceEntry> {
        self.cells
            .iter()
            .find(|c| c.token.eq_ignore_ascii_case(token))
            .or_else(|| self.cells.iter().find(|c| c.id == token))
    }

    /// The action of the slash item labelled `label`.
    pub fn slash_action(&self, label: &str) -> Option<SlashAction> {
        self.slash
            .iter()
            .find(|s| s.label == label)
            .and_then(|s| s.action)
    }
}

pub(super) fn is_ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// Start of an unclosed `{{` on the caret's line, with the text typed after it.
pub(super) fn open_reference(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let before = &text[line_start..cursor];
    let open = before.rfind("{{")?;
    let typed = &before[open + 2..];
    typed
        .chars()
        .all(is_ref_char)
        .then_some((line_start + open, typed))
}

/// End of the reference text after the caret (rest of the word and `}}`).
pub(super) fn reference_end(text: &str, cursor: usize) -> usize {
    let rest = &text[cursor..];
    let word: usize = rest
        .chars()
        .take_while(|c| is_ref_char(*c))
        .map(char::len_utf8)
        .sum();
    if rest[word..].starts_with("}}") {
        cursor + word + 2
    } else {
        cursor
    }
}

pub(super) fn item(
    label: String,
    insert: String,
    kind: CompletionKind,
    detail: String,
    score: i32,
    span: (usize, usize),
) -> CompletionItem {
    CompletionItem {
        label,
        insert_text: insert,
        kind,
        source: CompletionSource::Inferred,
        score,
        detail,
        replace_span: Some(span),
    }
}

pub(super) fn rank(candidate: &str, typed: &str) -> Option<i32> {
    if typed.is_empty() {
        return Some(100);
    }
    let lower = candidate.to_ascii_lowercase();
    let typed = typed.to_ascii_lowercase();
    if lower.starts_with(&typed) {
        Some(200)
    } else if lower.contains(&typed) {
        Some(100)
    } else {
        None
    }
}

/// Notebook completions at `cursor`: cell names and `param.` inside an
/// open `{{`, or the slash menu at the start of a Markdown line. `None`
/// when the caret is in neither (the SQL engine takes over).
pub fn completions(
    ctx: &ReferenceContext,
    text: &str,
    cursor: usize,
) -> Option<DirectiveCompletions> {
    if let Some((open, typed)) = open_reference(text, cursor) {
        let span = (open, reference_end(text, cursor));
        let mut items = Vec::new();
        if let Some(rest) = typed
            .strip_prefix("param.")
            .or_else(|| typed.strip_prefix("PARAM."))
        {
            for p in &ctx.params {
                let forms = std::iter::once(p.name.clone())
                    .chain(p.fields.iter().map(|f| format!("{}.{f}", p.name)));
                for form in forms {
                    if let Some(score) = rank(&form, rest) {
                        items.push(item(
                            format!("param.{form}"),
                            format!("{{{{param.{form}}}}}"),
                            CompletionKind::Parameter,
                            p.type_label.clone(),
                            score,
                            span,
                        ));
                    }
                }
            }
        } else {
            for c in &ctx.cells {
                if let Some(score) = rank(&c.token, typed) {
                    let detail = if c.state_label.is_empty() {
                        c.kind_label.clone()
                    } else {
                        format!("{} · {}", c.kind_label, c.state_label)
                    };
                    items.push(item(
                        c.token.clone(),
                        format!("{{{{{}}}}}", c.token),
                        CompletionKind::Table,
                        detail,
                        score,
                        span,
                    ));
                }
            }
            if !ctx.params.is_empty() && rank("param", typed).is_some() {
                items.push(item(
                    "param.".into(),
                    "{{param.".into(),
                    CompletionKind::Parameter,
                    ctx.param_kind_label.clone(),
                    50,
                    (open, cursor),
                ));
            }
        }
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        return Some(DirectiveCompletions {
            items,
            replace_start: open,
        });
    }
    if ctx.slash.is_empty() {
        return None;
    }
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let line_start = text[..cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line = &text[line_start..cursor];
    let indent = line.len() - line.trim_start().len();
    let typed = line.trim_start().strip_prefix('/')?;
    if !typed.chars().all(|c| c.is_alphanumeric() || c == ' ') || typed.len() > 24 {
        return None;
    }
    let slash = line_start + indent;
    let mut items: Vec<CompletionItem> = ctx
        .slash
        .iter()
        .enumerate()
        .filter_map(|(n, s)| {
            let score = rank(&s.label, typed.trim())?;
            Some(item(
                s.label.clone(),
                s.insert.clone(),
                CompletionKind::Snippet,
                s.detail.clone(),
                score * 100 - n as i32,
                (slash, cursor),
            ))
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.score));
    Some(DirectiveCompletions {
        items,
        replace_start: slash,
    })
}

/// `sql` with every `{{x}}` / `{{param.x}}` rewritten byte-for-byte as an
/// identifier (`__x__`, `__param_x__`), so offsets do not move.
pub fn mask(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let inner: usize = after
            .chars()
            .take_while(|c| is_ref_char(*c))
            .map(char::len_utf8)
            .sum();
        if inner > 0 && after[inner..].starts_with("}}") {
            out.push_str("__");
            out.push_str(&after[..inner].replace('.', "_"));
            out.push_str("__");
            rest = &after[inner + 2..];
        } else {
            out.push_str("{{");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn masked(token: &str) -> String {
    format!("__{token}__")
}

/// `base` plus one table per referenceable cell, named as [`mask`] writes
/// it, holding the cell's columns.
pub fn merged_catalog(base: &CompletionCatalog, ctx: &ReferenceContext) -> CompletionCatalog {
    let mut catalog = base.clone();
    let schema = base.schemas.first().cloned();
    for c in &ctx.cells {
        let mut object = CompletionObject::table(
            schema.clone().unwrap_or_default(),
            masked(&c.token),
            c.columns.iter().map(|(n, _)| n.clone()).collect(),
        );
        object.schema = schema.clone();
        catalog.objects.push(object);
    }
    catalog
}

/// Give items the engine produced for masked references their `{{x}}` form.
pub fn unmask_items(ctx: &ReferenceContext, items: &mut [CompletionItem]) {
    for item in items {
        let Some(token) = item
            .label
            .strip_prefix("__")
            .and_then(|l| l.strip_suffix("__"))
        else {
            continue;
        };
        if let Some(entry) = ctx.entry(token) {
            item.label = format!("{{{{{}}}}}", entry.token);
            item.insert_text = item.label.clone();
            item.detail = entry.kind_label.clone();
        }
    }
}

/// The `{{…}}` under `offset`, as (start, end, inner text).
pub(super) fn reference_at(text: &str, offset: usize) -> Option<(usize, usize, &str)> {
    let offset = offset.min(text.len());
    if !text.is_char_boundary(offset) {
        return None;
    }
    let line_start = text[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line_end = text[offset..]
        .find('\n')
        .map(|p| offset + p)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let mut from = 0;
    while let Some(open) = line[from..].find("{{") {
        let start = from + open;
        let Some(close) = line[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + close + 2;
        if (line_start + start..line_start + end).contains(&offset) {
            let inner = &line[start + 2..start + 2 + close];
            return inner.chars().all(is_ref_char).then_some((
                line_start + start,
                line_start + end,
                inner,
            ));
        }
        from = end;
    }
    None
}

/// The card for the `{{…}}` under `offset`: the cell's kind, name, state
/// and columns, or the parameter's type.
pub fn hover(
    ctx: &ReferenceContext,
    text: &str,
    offset: usize,
) -> Option<(HoverInfo, (usize, usize))> {
    let (start, end, inner) = reference_at(text, offset)?;
    if let Some(param) = inner.strip_prefix("param.") {
        let name = param.split('.').next().unwrap_or(param);
        let p = ctx
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))?;
        return Some((
            HoverInfo::Relation {
                kind_label: ctx.param_kind_label.clone(),
                name: p.name.clone(),
                details: vec![p.type_label.clone()],
                columns: Vec::new(),
                more_columns: 0,
            },
            (start, end),
        ));
    }
    let entry = ctx.entry(inner)?;
    let mut details = vec![entry.state_label.clone()];
    if !entry.columns.is_empty() {
        details.push(if entry.columns_from_output {
            ctx.from_output_note.clone()
        } else {
            ctx.from_query_note.clone()
        });
    }
    details.retain(|d| !d.is_empty());
    let columns = entry
        .columns
        .iter()
        .take(MAX_HOVER_COLUMNS)
        .map(|(name, ty)| HoverColumn {
            name: name.clone(),
            data_type: ty.clone(),
            nullable: None,
            is_primary_key: false,
        })
        .collect();
    Some((
        HoverInfo::Relation {
            kind_label: entry.kind_label.clone(),
            name: entry.token.clone(),
            details,
            columns,
            more_columns: entry.columns.len().saturating_sub(MAX_HOVER_COLUMNS),
        },
        (start, end),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ReferenceContext {
        ReferenceContext {
            cells: vec![
                ReferenceEntry {
                    token: "orders".into(),
                    id: "c_orders".into(),
                    kind_label: "SQL".into(),
                    state_label: "Fresh".into(),
                    columns: vec![
                        ("order_id".into(), "int".into()),
                        ("amount".into(), "decimal".into()),
                    ],
                    columns_from_output: true,
                    ..Default::default()
                },
                ReferenceEntry {
                    token: "customers".into(),
                    id: "c_cust".into(),
                    kind_label: "SQL".into(),
                    state_label: "Not run".into(),
                    columns: vec![
                        ("customer_id".into(), String::new()),
                        ("name".into(), String::new()),
                    ],
                    columns_from_output: false,
                    ..Default::default()
                },
            ],
            params: vec![ParamEntry {
                name: "region".into(),
                type_label: "Text".into(),
                ..Default::default()
            }],
            slash: vec![
                SlashItem {
                    label: "Heading 1".into(),
                    detail: String::new(),
                    insert: "# ".into(),
                    ..Default::default()
                },
                SlashItem {
                    label: "SQL cell below".into(),
                    detail: String::new(),
                    insert: String::new(),
                    action: Some(SlashAction::InsertCell("sql")),
                    scope: SnippetScope::Cell,
                },
            ],
            param_kind_label: "Parameter".into(),
            from_output_note: "from its last result".into(),
            from_query_note: "from its query".into(),
            ..Default::default()
        }
    }

    fn labels(c: &DirectiveCompletions) -> Vec<&str> {
        c.items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn open_braces_complete_cell_names_and_params() {
        let text = "select * from {{or";
        let c = completions(&ctx(), text, text.len()).unwrap();
        assert_eq!(labels(&c), vec!["orders"]);
        assert_eq!(c.items[0].insert_text, "{{orders}}");
        assert_eq!(c.items[0].replace_span, Some((14, text.len())));
        let text = "where region = {{param.re";
        let c = completions(&ctx(), text, text.len()).unwrap();
        assert_eq!(labels(&c), vec!["param.region"]);
        let text = "from {{}}";
        let c = completions(&ctx(), text, 7).unwrap();
        assert_eq!(c.items[0].replace_span, Some((5, 9)));
        assert!(labels(&c).contains(&"param."));
        assert!(completions(&ctx(), "from {{orders}} o", 17).is_none());
    }

    #[test]
    fn slash_menu_only_at_line_start_in_markdown() {
        let c = completions(&ctx(), "Intro\n/hea", 10).unwrap();
        assert_eq!(labels(&c), vec!["Heading 1"]);
        assert_eq!(c.items[0].replace_span, Some((6, 10)));
        assert!(completions(&ctx(), "a /hea", 6).is_none());
        let sql = ReferenceContext {
            slash: Vec::new(),
            ..ctx()
        };
        assert!(completions(&sql, "/hea", 4).is_none());
        assert_eq!(
            ctx().slash_action("SQL cell below"),
            Some(SlashAction::InsertCell("sql"))
        );
    }

    #[test]
    fn masking_keeps_offsets_and_aliases_complete_columns() {
        let sql = "select o. from {{orders}} o join {{param.region}} r";
        let m = mask(sql);
        assert_eq!(m.len(), sql.len());
        assert!(m.contains("__orders__ o"));
        assert!(m.contains("__param_region__"));
        let catalog = merged_catalog(&CompletionCatalog::default(), &ctx());
        let cursor = "select o.".len();
        let items = sqlly_intel::intellisense::complete(&m, cursor, &catalog);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.iter().any(|n| n.ends_with("order_id")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("amount")), "{names:?}");
    }

    #[test]
    fn table_suggestions_come_back_as_references() {
        let mut items = vec![CompletionItem {
            label: "__customers__".into(),
            insert_text: "__customers__".into(),
            kind: CompletionKind::Table,
            source: CompletionSource::LiveMetadata,
            score: 1,
            detail: String::new(),
            replace_span: None,
        }];
        unmask_items(&ctx(), &mut items);
        assert_eq!(items[0].label, "{{customers}}");
        assert_eq!(items[0].insert_text, "{{customers}}");
    }

    #[test]
    fn hover_describes_the_referenced_cell() {
        let text = "select * from {{orders}} o";
        let (info, span) = hover(&ctx(), text, 17).unwrap();
        assert_eq!(span, (14, 24));
        let HoverInfo::Relation {
            name,
            details,
            columns,
            ..
        } = info
        else {
            panic!("relation card");
        };
        assert_eq!(name, "orders");
        assert_eq!(
            details,
            vec!["Fresh".to_string(), "from its last result".to_string()]
        );
        assert_eq!(columns.len(), 2);
        assert!(hover(&ctx(), text, 3).is_none());
        let (info, _) = hover(&ctx(), "x = {{param.region}}", 8).unwrap();
        assert!(matches!(info, HoverInfo::Relation { name, .. } if name == "region"));
    }
}
