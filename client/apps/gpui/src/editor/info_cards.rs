//! Text layout for the editor's info popups — the hover card and the
//! signature-help card — as plain data: lines of colored, optionally bold
//! segments, already wrapped. The element only shapes and paints them, so
//! what a card says (and how it wraps) is testable without a window.

use gpui::Hsla;
use sqlly_intel::intellisense::{HoverInfo, RoutineKind, SignatureHelp};

use crate::theme::Theme;

/// Card text wraps at no more than this many characters (the editor font is
/// monospace); narrower editors wrap sooner — see [`wrap_columns`].
pub(super) const CARD_WRAP_COLUMNS: usize = 76;
/// Cards never wrap narrower than this, however small the editor.
const MIN_WRAP_COLUMNS: usize = 24;

/// Columns a card may use in an editor `available` pixels wide, given the
/// font's character width.
pub(super) fn wrap_columns(available: f32, char_width: f32) -> usize {
    if char_width <= 0.0 {
        return CARD_WRAP_COLUMNS;
    }
    ((available / char_width).floor() as usize).clamp(MIN_WRAP_COLUMNS, CARD_WRAP_COLUMNS)
}

/// At most `max` lines of a card: when it is taller, the last line that
/// fits becomes "…" so the reader knows there is more.
pub(super) fn fit_lines(mut lines: Vec<CardLine>, max: usize) -> Vec<CardLine> {
    let max = max.max(1);
    if lines.len() > max {
        lines.truncate(max - 1);
        lines.push(CardLine::new().push("…", dim(), false));
    }
    lines
}
/// Overloads listed on a function's hover card before "+N more forms".
const MAX_HOVER_SIGNATURES: usize = 4;
/// Parameters described on a function's hover card.
const MAX_HOVER_PARAMS: usize = 8;
/// Accepted words listed on the signature card before "…".
const MAX_SLOT_VALUES: usize = 14;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CardSegment {
    pub text: String,
    pub color: Hsla,
    pub bold: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct CardLine {
    pub segments: Vec<CardSegment>,
}

impl CardLine {
    fn new() -> Self {
        Self::default()
    }

    fn push(mut self, text: impl Into<String>, color: Hsla, bold: bool) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.segments.push(CardSegment { text, color, bold });
        }
        self
    }

    pub(super) fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Greedy word wrap at `width` characters; a word longer than the width gets
/// a line of its own rather than being split.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// `text` wrapped into lines of one color, each prefixed with `indent`.
fn wrapped(text: &str, color: Hsla, indent: &str, width: usize) -> Vec<CardLine> {
    wrap_words(text, width.saturating_sub(indent.len()).max(1))
        .into_iter()
        .map(|l| {
            CardLine::new()
                .push(indent, color, false)
                .push(l, color, false)
        })
        .collect()
}

/// A `label: text` block — `label` bold on the first line, `text` wrapped
/// and aligned under itself.
fn labeled(label: &str, text: &str, label_color: Hsla, color: Hsla, width: usize) -> Vec<CardLine> {
    let indent = " ".repeat(label.chars().count() + 2);
    let mut lines = Vec::new();
    for (i, part) in wrap_words(text, width.saturating_sub(indent.len()).max(1))
        .into_iter()
        .enumerate()
    {
        lines.push(if i == 0 {
            CardLine::new()
                .push(label, label_color, true)
                .push(": ", color, false)
                .push(part, color, false)
        } else {
            CardLine::new()
                .push(&indent, color, false)
                .push(part, color, false)
        });
    }
    if lines.is_empty() {
        lines.push(CardLine::new().push(label, label_color, true));
    }
    lines
}

fn dim() -> Hsla {
    Theme::text().opacity(0.7)
}

fn title(kind: &str, name: &str, color: Hsla, note: Option<&str>) -> CardLine {
    let mut line = CardLine::new()
        .push(format!("{kind} "), dim(), false)
        .push(name, color, true);
    if let Some(note) = note.filter(|n| !n.is_empty()) {
        line = line.push(format!("  · {note}"), dim(), false);
    }
    line
}

/// Every line of the hover card for `info`, title first.
pub(super) fn hover_card(info: &HoverInfo, width: usize) -> Vec<CardLine> {
    match info {
        HoverInfo::Table {
            qualified_name,
            is_view,
            columns,
            more_columns,
        } => {
            let (kind, color) = if *is_view {
                ("VIEW", Theme::kind_view())
            } else {
                ("TABLE", Theme::kind_table())
            };
            let mut lines = vec![title(kind, qualified_name, color, None)];
            for col in columns {
                let mut detail = String::new();
                if !col.data_type.is_empty() {
                    detail.push_str(&col.data_type);
                }
                match col.nullable {
                    Some(true) => detail.push_str("  NULL"),
                    Some(false) => detail.push_str("  NOT NULL"),
                    None => {}
                }
                if col.is_primary_key {
                    detail.push_str("  PK");
                }
                lines.push(CardLine::new().push(&col.name, Theme::text(), false).push(
                    format!("  {}", detail.trim_start()),
                    dim(),
                    false,
                ));
            }
            if *more_columns > 0 {
                lines.push(CardLine::new().push(
                    crate::localization::format(
                        "editor.info_cards.more_columns",
                        &[("count", &more_columns.to_string())],
                    ),
                    dim(),
                    false,
                ));
            }
            lines
        }
        HoverInfo::Column {
            table_qualified_name,
            column,
        } => {
            let mut parts = Vec::new();
            if !column.data_type.is_empty() {
                parts.push(column.data_type.clone());
            }
            match column.nullable {
                Some(true) => parts.push("NULL".into()),
                Some(false) => parts.push("NOT NULL".into()),
                None => {}
            }
            parts.push(crate::localization::format(
                "editor.info_cards.column_in_table",
                &[("table", table_qualified_name)],
            ));
            vec![
                title("COLUMN", &column.name, Theme::kind_column(), None),
                CardLine::new().push(parts.join("  "), dim(), false),
            ]
        }
        HoverInfo::Routine {
            qualified_name,
            kind,
            parameters,
            returns,
        } => {
            let (kind_label, color) = match kind {
                RoutineKind::Procedure => ("PROCEDURE", Theme::kind_procedure()),
                RoutineKind::Function => ("FUNCTION", Theme::function()),
            };
            let mut lines = vec![title(kind_label, qualified_name, color, None)];
            for p in parameters {
                let mut detail = p.data_type.clone();
                if p.is_output {
                    detail.push_str(" OUTPUT");
                }
                lines.push(
                    CardLine::new()
                        .push(&p.name, Theme::variable(), false)
                        .push(format!("  {}", detail.trim()), dim(), false),
                );
            }
            if let Some(ret) = returns {
                lines.push(CardLine::new().push(
                    crate::localization::format(
                        "editor.info_cards.routine_returns",
                        &[("return_type", ret)],
                    ),
                    dim(),
                    false,
                ));
            }
            lines
        }
        HoverInfo::Builtin(function) => {
            let mut lines = vec![title(
                "FUNCTION",
                &function.name,
                Theme::function(),
                Some(&function.category),
            )];
            for (index, signature) in function
                .signatures
                .iter()
                .enumerate()
                .take(MAX_HOVER_SIGNATURES)
            {
                let (label, _) = function.label(index);
                let args = &label[function.name.len()..];
                lines.push(
                    CardLine::new()
                        .push(&function.name, Theme::function(), false)
                        .push(args, Theme::text(), false)
                        .push(format!("  → {}", signature.returns), dim(), false),
                );
            }
            let extra = function
                .signatures
                .len()
                .saturating_sub(MAX_HOVER_SIGNATURES);
            if extra > 0 {
                lines.push(CardLine::new().push(
                    crate::localization::format(
                        "editor.info_cards.more_forms",
                        &[("count", &extra.to_string())],
                    ),
                    dim(),
                    false,
                ));
            }
            lines.extend(wrapped(&function.summary, Theme::text(), "", width));
            if let Some(first) = function.signatures.first() {
                for p in first.params.iter().take(MAX_HOVER_PARAMS) {
                    let text = if p.ty.is_empty() {
                        p.doc.clone()
                    } else {
                        format!("{} — {}", p.ty, p.doc)
                    };
                    lines.extend(labeled(&p.name, &text, Theme::variable(), dim(), width));
                }
            }
            if let Some(example) = function.example.as_deref().filter(|e| !e.is_empty()) {
                lines.push(
                    CardLine::new()
                        .push(
                            crate::localization::text("editor.info_cards.example_label"),
                            dim(),
                            false,
                        )
                        .push(example, Theme::text(), false),
                );
            }
            lines
        }
        HoverInfo::Constant { doc, slot } => {
            word_card("CONSTANT", doc, Theme::constant(), slot.as_deref(), width)
        }
        HoverInfo::DataType(doc) => word_card("TYPE", doc, Theme::type_name(), None, width),
        HoverInfo::Command(doc) => {
            // The safety badges ride on the title line so a destructive or
            // blocking command warns before Run does.
            let badges = doc.badge_text();
            let mut note = doc.group.clone();
            if !badges.is_empty() {
                note = format!("{note}  ·  {badges}");
            }
            if let Some(since) = doc.deprecated_since.as_deref() {
                note = crate::localization::format(
                    "editor.info_cards.deprecated_since",
                    &[("note", &note), ("since", &since.to_string())],
                );
            }
            let mut lines = vec![title("COMMAND", &doc.name, Theme::function(), Some(&note))];
            lines.extend(wrapped(&doc.syntax, Theme::text(), "", width));
            lines.extend(wrapped(&doc.summary, dim(), "", width));
            let mut facts = Vec::new();
            if let Some(since) = doc.since.as_deref().filter(|s| !s.is_empty()) {
                facts.push(crate::localization::format(
                    "editor.info_cards.since",
                    &[("version", since)],
                ));
            }
            if let Some(complexity) = doc.complexity.as_deref().filter(|c| !c.is_empty()) {
                facts.push(complexity.to_string());
            }
            if !facts.is_empty() {
                lines.extend(wrapped(&facts.join("  ·  "), dim(), "", width));
            }
            for badge in &doc.badges {
                if let sqlly_intel::redis_complete::CommandBadge::Blocking(reason) = badge {
                    lines.extend(labeled(
                        &crate::localization::text("editor.info_cards.blocking_label"),
                        reason,
                        Theme::variable(),
                        dim(),
                        width,
                    ));
                }
            }
            lines
        }
        HoverInfo::SystemVariable(doc) => word_card(
            "VARIABLE",
            doc,
            Theme::variable(),
            Some(&crate::localization::text(
                "editor.info_cards.system_variable_note",
            )),
            width,
        ),
    }
}

fn word_card(
    kind: &str,
    doc: &sqlly_intel::intellisense::WordDoc,
    color: Hsla,
    note: Option<&str>,
    width: usize,
) -> Vec<CardLine> {
    let mut lines = vec![title(kind, &doc.name, color, note)];
    if let Some(syntax) = doc.syntax.as_deref().filter(|s| !s.is_empty()) {
        lines.extend(wrapped(syntax, Theme::text(), "", width));
    }
    lines.extend(wrapped(&doc.doc, dim(), "", width));
    lines
}

/// The signature card's lines after the signature label itself: the active
/// argument's meaning, the fixed words it accepts, and what the function
/// does.
pub(super) fn signature_detail(help: &SignatureHelp, width: usize) -> Vec<CardLine> {
    let mut lines = Vec::new();
    if let Some(i) = help.active_parameter
        && let Some(doc) = help.parameter_docs.get(i).filter(|d| !d.is_empty())
    {
        lines.extend(labeled(
            &help.parameters[i],
            doc,
            Theme::variable(),
            Theme::text(),
            width,
        ));
    }
    if !help.active_values.is_empty() {
        let mut values = help
            .active_values
            .iter()
            .take(MAX_SLOT_VALUES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if help.active_values.len() > MAX_SLOT_VALUES {
            values.push_str(", …");
        }
        lines.extend(labeled(
            &crate::localization::text("editor.info_cards.accepts_label"),
            &values,
            dim(),
            Theme::constant(),
            width,
        ));
    }
    if let Some(summary) = help.summary.as_deref().filter(|s| !s.is_empty()) {
        lines.extend(wrapped(summary, dim(), "", width));
    }
    lines
}

/// Suffix after the signature label when the function has several forms.
pub(super) fn overload_note(help: &SignatureHelp) -> Option<String> {
    (help.overload_count > 1).then(|| {
        crate::localization::format(
            "editor.info_cards.overload_note",
            &[
                ("current", &(help.overload_index + 1).to_string()),
                ("total", &help.overload_count.to_string()),
            ],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::intellisense::{BuiltinFunction, FunctionParam, FunctionSignature, WordDoc};

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = wrap_words("the quick brown fox jumps over the lazy dog", 10);
        assert_eq!(
            lines,
            ["the quick", "brown fox", "jumps over", "the lazy", "dog"]
        );
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        // A word wider than the line keeps a line to itself, unsplit.
        assert_eq!(
            wrap_words("a supercalifragilistic b", 6),
            ["a", "supercalifragilistic", "b"]
        );
        assert!(wrap_words("   ", 10).is_empty());
    }

    fn dateadd() -> BuiltinFunction {
        BuiltinFunction {
            name: "DATEADD".into(),
            category: "Date and time".into(),
            summary: "Adds a number of date-part units to a date.".into(),
            signatures: vec![FunctionSignature {
                params: vec![
                    FunctionParam {
                        name: "datepart".into(),
                        ty: "date part".into(),
                        doc: "The unit to add.".into(),
                        optional: false,
                        values: vec!["year".into(), "day".into()],
                    },
                    FunctionParam {
                        name: "number".into(),
                        ty: "int".into(),
                        doc: "How many units.".into(),
                        optional: false,
                        values: vec![],
                    },
                ],
                returns: "datetime".into(),
                variadic: false,
            }],
            example: Some("DATEADD(day, 7, GETDATE())".into()),
            niladic: false,
        }
    }

    #[test]
    fn builtin_card_shows_signature_summary_params_and_example() {
        let text: Vec<String> =
            hover_card(&HoverInfo::Builtin(Box::new(dateadd())), CARD_WRAP_COLUMNS)
                .iter()
                .map(CardLine::text)
                .collect();
        assert_eq!(text[0], "FUNCTION DATEADD  · Date and time");
        assert_eq!(text[1], "DATEADD(datepart, number)  → datetime");
        assert!(text.contains(&"Adds a number of date-part units to a date.".to_string()));
        assert!(text.contains(&"datepart: date part — The unit to add.".to_string()));
        assert_eq!(text.last().unwrap(), "Example: DATEADD(day, 7, GETDATE())");
    }

    #[test]
    fn type_card_shows_syntax_then_doc() {
        let doc = WordDoc {
            name: "varchar".into(),
            doc: "Variable-length text.".into(),
            syntax: Some("varchar(n | max)".into()),
        };
        let text: Vec<String> = hover_card(&HoverInfo::DataType(doc), CARD_WRAP_COLUMNS)
            .iter()
            .map(CardLine::text)
            .collect();
        assert_eq!(
            text,
            ["TYPE varchar", "varchar(n | max)", "Variable-length text."]
        );
    }

    #[test]
    fn signature_detail_explains_the_active_argument() {
        let help = SignatureHelp {
            label: "DATEADD(datepart, number)".into(),
            parameters: vec!["datepart".into(), "number".into()],
            parameter_ranges: vec![(8, 16), (18, 24)],
            parameter_docs: vec![
                "date part — The unit to add.".into(),
                "int — How many.".into(),
            ],
            active_parameter: Some(0),
            kind: RoutineKind::Function,
            open_paren: 7,
            summary: Some("Adds units.".into()),
            overload_index: 0,
            overload_count: 2,
            active_values: vec!["year".into(), "day".into()],
        };
        let text: Vec<String> = signature_detail(&help, CARD_WRAP_COLUMNS)
            .iter()
            .map(CardLine::text)
            .collect();
        assert_eq!(
            text,
            [
                "datepart: date part — The unit to add.",
                "accepts: year, day",
                "Adds units."
            ]
        );
        assert_eq!(overload_note(&help).as_deref(), Some("  (1 of 2 forms)"));
    }

    #[test]
    fn long_docs_wrap_under_their_label() {
        let long = "word ".repeat(40);
        let lines = labeled(
            "expression",
            long.trim(),
            Theme::text(),
            Theme::text(),
            CARD_WRAP_COLUMNS,
        );
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(
                line.text().chars().count() <= CARD_WRAP_COLUMNS,
                "{}",
                line.text()
            );
        }
        assert!(
            lines[1]
                .text()
                .starts_with(&" ".repeat("expression".len() + 2))
        );
    }

    #[test]
    fn narrow_editors_wrap_sooner_and_tall_cards_are_trimmed() {
        assert_eq!(wrap_columns(10_000.0, 8.0), CARD_WRAP_COLUMNS);
        assert_eq!(wrap_columns(400.0, 10.0), 40);
        assert_eq!(wrap_columns(50.0, 10.0), MIN_WRAP_COLUMNS);
        // The summary (prose) wraps to the width; signature lines do not.
        let narrow: Vec<String> = hover_card(&HoverInfo::Builtin(Box::new(dateadd())), 30)
            .iter()
            .map(CardLine::text)
            .collect();
        assert!(
            narrow.contains(&"Adds a number of date-part".to_string()),
            "{narrow:?}"
        );
        assert!(
            narrow.contains(&"units to a date.".to_string()),
            "{narrow:?}"
        );
        let lines: Vec<CardLine> = (0..10)
            .map(|i| CardLine::new().push(i.to_string(), dim(), false))
            .collect();
        let fitted = fit_lines(lines.clone(), 4);
        assert_eq!(fitted.len(), 4);
        assert_eq!(fitted[3].text(), "…");
        assert_eq!(fit_lines(lines, 20).len(), 10);
    }
}
