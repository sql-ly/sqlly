//! What a Markdown cell's editor offers in place of SQL IntelliSense: the
//! Markdown snippets (the slash menu and Ctrl+Space share one list, built
//! by `ui::slash_items`), the values Markdown text can show (`{{formula}}`
//! and `{{param.x}}`, with their current values and sensitive ones hidden),
//! picture attachments after `](attachment:`, code-fence languages, hover
//! cards with a value and its state, and gentle diagnostics for names the
//! notebook does not have. Completion and hover scan the caret's line only;
//! the diagnostics read the cell's own text once per pass.

use sqlly_intel::intellisense::{CompletionItem, CompletionKind, HoverInfo};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::notebook::SourceLanguage;
use sqlly_services::notebook::cells::attachment::{MARKDOWN_SCHEME, markdown_attachment_refs};
use sqlly_services::notebook::references::{RefTarget, find_references};

use super::intel::{
    ParamEntry, ReferenceContext, ReferenceEntry, SnippetScope, is_ref_char, item, open_reference,
    rank, reference_at, reference_end,
};
use crate::directive_completion::DirectiveCompletions;

/// Languages a code fence completes to, most used first.
pub const FENCE_LANGUAGES: &[&str] = &[
    "sql",
    "json",
    "text",
    "csv",
    "python",
    "bash",
    "yaml",
    "markdown",
    "javascript",
    "xml",
    "html",
    "diff",
];

/// Characters of a value shown in a completion's detail before "…".
const MAX_PREVIEW_CHARS: usize = 40;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn preview(value: &str) -> String {
    let mut chars = value.chars();
    let head: String = chars.by_ref().take(MAX_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn cell_detail(c: &ReferenceEntry) -> String {
    match (&c.markdown_value, &c.value_note) {
        (Some(value), None) => crate::localization::format(
            "notebook.markdown_editor.intel.cell_value",
            &[("kind", &c.kind_label), ("value", &preview(value))],
        ),
        (Some(value), Some(note)) => crate::localization::format(
            "notebook.markdown_editor.intel.cell_value_note",
            &[
                ("kind", &c.kind_label),
                ("value", &preview(value)),
                ("note", note),
            ],
        ),
        (None, note) => crate::localization::format(
            "notebook.markdown_editor.intel.cell_note",
            &[
                ("kind", &c.kind_label),
                (
                    "note",
                    &note
                        .clone()
                        .unwrap_or_else(|| t("notebook.markdown_editor.intel.hover_no_value")),
                ),
            ],
        ),
    }
}

fn param_detail(p: &ParamEntry) -> String {
    if p.sensitive {
        return crate::localization::format(
            "notebook.markdown_editor.intel.param_hidden",
            &[("type", &p.type_label)],
        );
    }
    match &p.value {
        Some(value) => crate::localization::format(
            "notebook.markdown_editor.intel.param_value",
            &[("type", &p.type_label), ("value", &preview(value))],
        ),
        None => crate::localization::format(
            "notebook.markdown_editor.intel.param_unset",
            &[("type", &p.type_label)],
        ),
    }
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |p| p + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..].find('\n').map_or(text.len(), |p| cursor + p)
}

/// Whether the line holding `cursor` starts inside a fenced code block.
fn in_fence(text: &str, cursor: usize) -> bool {
    crate::editor::markdown_fence_states(&text[..line_end(text, cursor)])
        .last()
        .copied()
        .unwrap_or(false)
}

fn references(
    ctx: &ReferenceContext,
    text: &str,
    open: usize,
    typed: &str,
    cursor: usize,
) -> Vec<CompletionItem> {
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
                        param_detail(p),
                        score,
                        span,
                    ));
                }
            }
        }
    } else {
        for c in ctx.cells.iter().filter(|c| c.shows_in_markdown) {
            if let Some(score) = rank(&c.token, typed) {
                items.push(item(
                    c.token.clone(),
                    format!("{{{{{}}}}}", c.token),
                    CompletionKind::Value,
                    cell_detail(c),
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
    items
}

/// The start of an image destination the caret is typing (`![alt](att|`),
/// with the text typed so far, when it is (a prefix of) `attachment:…`.
fn image_destination(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let start = line_start(text, cursor);
    let before = &text[start..cursor];
    let open = before.rfind("](")?;
    let typed = before[open + 2..].trim_start_matches('<');
    let dest = cursor - typed.len();
    if typed.contains([' ', ')', '>', '"']) || !before[..open].contains("![") {
        return None;
    }
    let close = before[..open].rfind("![")?;
    if before[close..open].contains(']') {
        return None;
    }
    let head = typed.get(..typed.len().min(MARKDOWN_SCHEME.len()))?;
    MARKDOWN_SCHEME
        .get(..head.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(head))
        .then_some((dest, typed))
}

fn attachments(
    ctx: &ReferenceContext,
    text: &str,
    dest: usize,
    typed: &str,
    cursor: usize,
) -> Vec<CompletionItem> {
    let name = typed.get(MARKDOWN_SCHEME.len()..).unwrap_or("");
    let rest = &text[cursor..line_end(text, cursor)];
    let word: usize = rest
        .chars()
        .take_while(|c| !matches!(c, ')' | ' ' | '>' | '"'))
        .map(char::len_utf8)
        .sum();
    let mut items: Vec<CompletionItem> = ctx
        .attachments
        .iter()
        .filter(|a| a.picture)
        .filter_map(|a| {
            let score = rank(&a.token, name)?;
            Some(item(
                a.token.clone(),
                format!("{MARKDOWN_SCHEME}{}", a.token),
                CompletionKind::Value,
                crate::localization::format(
                    "notebook.markdown_editor.intel.attachment_picture",
                    &[(
                        "file_name",
                        if a.file_name.is_empty() {
                            &a.token
                        } else {
                            &a.file_name
                        },
                    )],
                ),
                score,
                (dest, cursor + word),
            ))
        })
        .collect();
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items
}

/// `(fence start, typed language)` when the caret ends an opening fence.
fn fence_language(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let start = line_start(text, cursor);
    if line_end(text, cursor) != cursor {
        return None;
    }
    let line = &text[start..cursor];
    let (c, run) = crate::editor::markdown_fence_marker(line)?;
    if c != b'`' || in_fence(text, cursor) {
        return None;
    }
    let indent = line.len() - line.trim_start_matches(' ').len();
    let typed = &line[indent + run..];
    typed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '+')
        .then_some((start + indent, typed))
}

fn fences(text: &str, at: usize, typed: &str, cursor: usize) -> Vec<CompletionItem> {
    let ticks = &text[at..cursor][..text[at..cursor].len() - typed.len()];
    let detail = t("notebook.markdown_editor.intel.fence_language");
    FENCE_LANGUAGES
        .iter()
        .enumerate()
        .filter_map(|(n, lang)| {
            let score = rank(lang, typed)? * 100 - n as i32;
            Some(item(
                (*lang).to_string(),
                format!("{ticks}{lang}\n$0\n{ticks}"),
                CompletionKind::Snippet,
                detail.clone(),
                score,
                (at, cursor),
            ))
        })
        .collect()
}

fn snippets(
    ctx: &ReferenceContext,
    typed: &str,
    span: (usize, usize),
    scopes: &[SnippetScope],
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = ctx
        .slash
        .iter()
        .enumerate()
        .filter(|(_, s)| scopes.contains(&s.scope))
        .filter_map(|(n, s)| {
            let score = rank(&s.label, typed.trim())?;
            Some(item(
                s.label.clone(),
                s.insert.clone(),
                CompletionKind::Snippet,
                s.detail.clone(),
                score * 100 - n as i32,
                span,
            ))
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.score));
    items
}

/// Completions for the caret in a Markdown cell: values and parameters in
/// an open `{{`, pictures in an image's `attachment:` destination, the
/// language of an opening code fence, the slash menu at the start of a
/// line, and (`manual`, Ctrl+Space) the Markdown snippets for the word
/// before the caret. `None` when none of these applies.
pub fn completions(
    ctx: &ReferenceContext,
    text: &str,
    cursor: usize,
    manual: bool,
) -> Option<DirectiveCompletions> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let done = |items: Vec<CompletionItem>, replace_start: usize| {
        Some(DirectiveCompletions {
            items,
            replace_start,
        })
    };
    if let Some((open, typed)) = open_reference(text, cursor) {
        return done(references(ctx, text, open, typed, cursor), open);
    }
    if let Some((dest, typed)) = image_destination(text, cursor) {
        return done(attachments(ctx, text, dest, typed, cursor), dest);
    }
    if let Some((at, typed)) = fence_language(text, cursor) {
        return done(fences(text, at, typed, cursor), at);
    }
    let start = line_start(text, cursor);
    let line = &text[start..cursor];
    let indent = line.len() - line.trim_start().len();
    if let Some(typed) = line.trim_start().strip_prefix('/')
        && !in_fence(text, cursor)
    {
        if !typed.chars().all(|c| c.is_alphanumeric() || c == ' ') || typed.len() > 24 {
            return None;
        }
        let slash = start + indent;
        let all = [
            SnippetScope::Block,
            SnippetScope::Inline,
            SnippetScope::Reference,
            SnippetScope::Cell,
        ];
        return done(snippets(ctx, typed, (slash, cursor), &all), slash);
    }
    if !manual || in_fence(text, cursor) {
        return None;
    }
    let word: usize = line
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric())
        .map(char::len_utf8)
        .sum();
    let word_start = cursor - word;
    let at_line_start = word_start == start + indent;
    let scopes: &[SnippetScope] = if at_line_start {
        &[
            SnippetScope::Block,
            SnippetScope::Inline,
            SnippetScope::Reference,
        ]
    } else {
        &[SnippetScope::Inline, SnippetScope::Reference]
    };
    done(
        snippets(ctx, &text[word_start..cursor], (word_start, cursor), scopes),
        word_start,
    )
}

/// The card for the `{{…}}` under `offset`: a formula's kind, value and
/// state, a parameter's type and value (hidden when sensitive), or, for a
/// cell Markdown can't show, why the reference stays as written.
pub fn hover(
    ctx: &ReferenceContext,
    text: &str,
    offset: usize,
) -> Option<(HoverInfo, (usize, usize))> {
    let (start, end, inner) = reference_at(text, offset)?;
    let card = |kind_label: String, name: String, details: Vec<String>| {
        Some((
            HoverInfo::Relation {
                kind_label,
                name,
                details: details.into_iter().filter(|d| !d.is_empty()).collect(),
                columns: Vec::new(),
                more_columns: 0,
            },
            (start, end),
        ))
    };
    if let Some(param) = inner
        .strip_prefix("param.")
        .or_else(|| inner.strip_prefix("PARAM."))
    {
        let name = param.split('.').next().unwrap_or(param);
        let p = ctx
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))?;
        let value = if p.sensitive {
            t("notebook.markdown_editor.intel.hover_hidden")
        } else {
            match &p.value {
                Some(v) => crate::localization::format(
                    "notebook.markdown_editor.intel.hover_value",
                    &[("value", &preview(v))],
                ),
                None => t("notebook.markdown_editor.intel.hover_unset"),
            }
        };
        return card(
            ctx.param_kind_label.clone(),
            p.name.clone(),
            vec![p.type_label.clone(), value],
        );
    }
    let entry = ctx.entry(inner)?;
    if !entry.shows_in_markdown {
        return card(
            entry.kind_label.clone(),
            entry.token.clone(),
            vec![
                entry.state_label.clone(),
                t("notebook.markdown_editor.intel.hover_not_shown"),
            ],
        );
    }
    let value = match &entry.markdown_value {
        Some(v) => crate::localization::format(
            "notebook.markdown_editor.intel.hover_value",
            &[("value", &preview(v))],
        ),
        None => t("notebook.markdown_editor.intel.hover_no_value"),
    };
    card(
        entry.kind_label.clone(),
        entry.token.clone(),
        vec![value, entry.value_note.clone().unwrap_or_default()],
    )
}

/// A problem with a reference in Markdown text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub start: usize,
    pub end: usize,
    pub message: String,
    /// A name the notebook does not have (else a reference that only stays
    /// as written).
    pub unknown: bool,
}

/// Every reference in `text` that will not show a value: names the
/// notebook does not have, parameter fields that do not exist, cells that
/// are not formulas, and picture destinations naming no picture attachment.
pub fn problems(ctx: &ReferenceContext, text: &str) -> Vec<Problem> {
    let mut out = Vec::new();
    if text.contains("{{") {
        for r in find_references(text, SourceLanguage::Markdown, DatabaseType::default()) {
            let problem = match &r.target {
                RefTarget::Cell(token) => match ctx.entry(token) {
                    None => Some((
                        crate::localization::format(
                            "notebook.markdown_editor.diag.unknown_reference",
                            &[("name", token)],
                        ),
                        true,
                    )),
                    Some(e) if !e.shows_in_markdown => Some((
                        crate::localization::format(
                            "notebook.markdown_editor.diag.not_shown",
                            &[("name", &e.token)],
                        ),
                        false,
                    )),
                    Some(_) => None,
                },
                RefTarget::Param { name, field } => {
                    match ctx
                        .params
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(name))
                    {
                        None => Some((
                            crate::localization::format(
                                "notebook.markdown_editor.diag.unknown_parameter",
                                &[("name", name)],
                            ),
                            true,
                        )),
                        Some(p)
                            if field.as_ref().is_some_and(|field| {
                                !p.fields.iter().any(|f| f.eq_ignore_ascii_case(field))
                            }) =>
                        {
                            Some((
                                crate::localization::format(
                                    "notebook.markdown_editor.diag.unknown_field",
                                    &[
                                        ("name", p.name.as_str()),
                                        ("field", field.as_deref().unwrap_or_default()),
                                    ],
                                ),
                                true,
                            ))
                        }
                        Some(_) => None,
                    }
                }
            };
            if let Some((message, unknown)) = problem {
                out.push(Problem {
                    start: r.start,
                    end: r.end,
                    message,
                    unknown,
                });
            }
        }
    }
    if text.contains("](") {
        let named = markdown_attachment_refs(text);
        for name in &named {
            let (message, unknown) = match ctx
                .attachments
                .iter()
                .find(|a| a.token == *name || a.id == *name)
            {
                None => (
                    crate::localization::format(
                        "notebook.markdown_editor.diag.unknown_attachment",
                        &[("name", name)],
                    ),
                    true,
                ),
                Some(a) if !a.picture => (
                    crate::localization::format(
                        "notebook.markdown_editor.diag.not_a_picture",
                        &[("name", &a.token)],
                    ),
                    false,
                ),
                Some(_) => continue,
            };
            let needle = format!("{MARKDOWN_SCHEME}{name}");
            let mut from = 0;
            while let Some(at) = text[from..].find(&needle) {
                let start = from + at;
                let end = start + needle.len();
                from = end;
                if text[end..].chars().next().is_some_and(is_ref_char) {
                    continue;
                }
                out.push(Problem {
                    start,
                    end,
                    message: message.clone(),
                    unknown,
                });
            }
        }
    }
    out.sort_by_key(|p| p.start);
    out
}

#[cfg(test)]
mod tests {
    use super::super::intel::{AttachmentEntry, SlashItem};
    use super::*;

    fn ctx() -> ReferenceContext {
        ReferenceContext {
            cells: vec![
                ReferenceEntry {
                    token: "avg_order".into(),
                    id: "c_avg".into(),
                    kind_label: "Formula".into(),
                    state_label: "Fresh".into(),
                    shows_in_markdown: true,
                    markdown_value: Some("42.50".into()),
                    ..Default::default()
                },
                ReferenceEntry {
                    token: "growth".into(),
                    id: "c_growth".into(),
                    kind_label: "Formula".into(),
                    state_label: "Stale".into(),
                    shows_in_markdown: true,
                    markdown_value: Some("3%".into()),
                    value_note: Some("Stale".into()),
                    ..Default::default()
                },
                ReferenceEntry {
                    token: "orders".into(),
                    id: "c_orders".into(),
                    kind_label: "SQL".into(),
                    state_label: "Fresh".into(),
                    ..Default::default()
                },
            ],
            params: vec![
                ParamEntry {
                    name: "region".into(),
                    type_label: "text".into(),
                    value: Some("West".into()),
                    ..Default::default()
                },
                ParamEntry {
                    name: "api_key".into(),
                    type_label: "text".into(),
                    sensitive: true,
                    ..Default::default()
                },
                ParamEntry {
                    name: "period".into(),
                    type_label: "date_range".into(),
                    fields: vec!["start".into(), "end".into()],
                    value: Some("2026-01-01 – 2026-01-31".into()),
                    ..Default::default()
                },
            ],
            slash: vec![
                SlashItem {
                    label: "Heading 1".into(),
                    insert: "# ".into(),
                    ..Default::default()
                },
                SlashItem {
                    label: "Bold".into(),
                    insert: "**${1:text}**".into(),
                    scope: SnippetScope::Inline,
                    ..Default::default()
                },
                SlashItem {
                    label: "New SQL cell below".into(),
                    scope: SnippetScope::Cell,
                    ..Default::default()
                },
            ],
            attachments: vec![
                AttachmentEntry {
                    token: "trend".into(),
                    id: "c_trend".into(),
                    file_name: "trend.png".into(),
                    picture: true,
                },
                AttachmentEntry {
                    token: "sales_csv".into(),
                    id: "c_csv".into(),
                    file_name: "sales.csv".into(),
                    picture: false,
                },
            ],
            param_kind_label: "Parameter".into(),
            ..Default::default()
        }
    }

    fn labels(c: &Option<DirectiveCompletions>) -> Vec<&str> {
        c.as_ref()
            .map(|c| c.items.iter().map(|i| i.label.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn braces_offer_only_what_markdown_shows_with_values() {
        let text = "Average {{";
        let c = completions(&ctx(), text, text.len(), false);
        assert_eq!(labels(&c), vec!["avg_order", "growth", "param."]);
        let items = &c.unwrap().items;
        assert!(items[0].detail.contains("42.50"), "{}", items[0].detail);
        assert!(items[1].detail.contains("Stale"), "{}", items[1].detail);
        assert_eq!(items[0].insert_text, "{{avg_order}}");
    }

    #[test]
    fn param_completions_hide_sensitive_values() {
        let text = "For {{param.";
        let c = completions(&ctx(), text, text.len(), false).unwrap();
        let by: Vec<(&str, &str)> = c
            .items
            .iter()
            .map(|i| (i.label.as_str(), i.detail.as_str()))
            .collect();
        assert!(
            by.iter()
                .any(|(l, d)| *l == "param.region" && d.contains("West"))
        );
        let key = by.iter().find(|(l, _)| *l == "param.api_key").unwrap();
        assert!(!key.1.contains("West"), "{}", key.1);
        assert!(by.iter().any(|(l, _)| *l == "param.period.start"));
    }

    #[test]
    fn picture_destinations_list_picture_attachments() {
        for text in ["![Trend](", "![Trend](att", "![Trend](attachment:tr"] {
            let c = completions(&ctx(), text, text.len(), false);
            assert_eq!(labels(&c), vec!["trend"], "{text}");
            let item = &c.unwrap().items[0];
            assert_eq!(item.insert_text, "attachment:trend");
            assert_eq!(item.replace_span, Some((9, text.len())));
        }
        assert!(completions(&ctx(), "[link](att", 10, false).is_none());
        assert!(completions(&ctx(), "![x](https", 10, false).is_none());
    }

    #[test]
    fn opening_fences_complete_languages_and_closing_ones_do_not() {
        let text = "```s";
        let c = completions(&ctx(), text, text.len(), false).unwrap();
        assert_eq!(c.items[0].label, "sql");
        assert_eq!(c.items[0].insert_text, "```sql\n$0\n```");
        let text = "```sql\nSELECT 1\n```";
        assert!(completions(&ctx(), text, text.len(), false).is_none());
    }

    #[test]
    fn slash_and_manual_snippets_share_one_list() {
        let c = completions(&ctx(), "/bo", 3, false);
        assert_eq!(labels(&c), vec!["Bold"]);
        let c = completions(&ctx(), "/", 1, false);
        assert_eq!(labels(&c).len(), 3, "the slash menu has every item");
        assert!(
            completions(&ctx(), "He", 2, false).is_none(),
            "automatic: quiet"
        );
        let c = completions(&ctx(), "He", 2, true);
        assert_eq!(labels(&c), vec!["Heading 1"]);
        let c = completions(&ctx(), "some text b", 11, true);
        assert_eq!(labels(&c), vec!["Bold"], "mid-line: inline snippets only");
        assert!(
            completions(&ctx(), "```\n/he", 7, false).is_none(),
            "not in code"
        );
    }

    #[test]
    fn hover_shows_value_state_and_masks_sensitive() {
        let text = "{{growth}} {{param.api_key}} {{orders}}";
        let Some((HoverInfo::Relation { details, .. }, span)) = hover(&ctx(), text, 3) else {
            panic!("card");
        };
        assert_eq!(span, (0, 10));
        assert!(details.iter().any(|d| d.contains("3%")));
        assert!(details.iter().any(|d| d == "Stale"));
        let Some((HoverInfo::Relation { details, .. }, _)) = hover(&ctx(), text, 14) else {
            panic!("card");
        };
        assert_eq!(details.len(), 2);
        let Some((HoverInfo::Relation { details, .. }, _)) = hover(&ctx(), text, 32) else {
            panic!("card");
        };
        assert!(details.len() == 2, "{details:?}");
    }

    #[test]
    fn problems_flag_unknown_names_gently() {
        let text = "{{avg_order}} {{nope}} {{param.zone}} {{param.region.start}} {{orders}}\n\
                    `{{in_code}}` ![a](attachment:trend) ![b](attachment:gone) ![c](attachment:sales_csv)";
        let found: Vec<(&str, bool)> = problems(&ctx(), text)
            .into_iter()
            .map(|p| (&text[p.start..p.end], p.unknown))
            .collect();
        assert_eq!(
            found,
            vec![
                ("{{nope}}", true),
                ("{{param.zone}}", true),
                ("{{param.region.start}}", true),
                ("{{orders}}", false),
                ("attachment:gone", true),
                ("attachment:sales_csv", false),
            ]
        );
        assert!(problems(&ctx(), "{{param.period.start}} plain").is_empty());
    }
}
