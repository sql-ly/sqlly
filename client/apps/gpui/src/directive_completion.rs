//! IntelliSense for `-- sqlly …` directives: the family keyword, every
//! setting a family takes, and every value a setting accepts.
//!
//! The menu follows the caret through the directive:
//!
//! ```text
//! --|                          starter lines, one per family
//! -- sq|                       the namespace
//! -- sqlly |                   every family keyword
//! -- sqlly grid: |             that family's settings not yet on the line
//! -- sqlly grid: sort=|        the setting's values (here: result columns)
//! -- sqlly grid: sort=Total |  what may follow (here: asc / desc)
//! ```
//!
//! Recognition rides the shared lexer's rules ([`crate::sqlly_directive`]),
//! and the tests hold every offered key and value to the real parsers, so
//! the menu can only suggest text a directive honors. Column names come
//! from the tab's last run (`result_columns`, one list per result set);
//! each family writes them the way its parser reads them.
//!
//! Pure text scanning, line-local except for the first-line check: it runs
//! on every keystroke inside a directive comment.

use sqlly_intel::intellisense::{CompletionItem, CompletionKind, CompletionSource};

use crate::sqlly_directive::{Directive, classify, indexed_body, line_comment_start};

/// The menu for the caret position. `items` may be empty: the caret is in
/// a directive, so SQL completion must stay out of it, but there is nothing
/// to offer (free text, or the value is already complete).
pub struct DirectiveCompletions {
    pub items: Vec<CompletionItem>,
    /// Start of the text an accepted item replaces.
    pub replace_start: usize,
}

/// Completions for the directive comment under `cursor`; `None` when the
/// caret is not in one (ordinary comments and SQL fall through to the SQL
/// engine).
pub fn complete(
    content: &str,
    cursor: usize,
    result_columns: &[Vec<String>],
) -> Option<DirectiveCompletions> {
    let mut cursor = cursor.min(content.len());
    while cursor > 0 && !content.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let line_start = content[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let line_end = content[cursor..]
        .find(['\n', '\r'])
        .map_or(content.len(), |i| cursor + i);
    let line = &content[line_start..line_end];
    let comment_start = line_start + line_comment_start(line)?;
    if cursor < comment_start + 2 {
        return None;
    }
    let ctx = Ctx {
        content,
        cursor,
        line_end,
        comment_start,
        standalone: content[line_start..comment_start].trim().is_empty(),
        columns: result_columns,
    };
    let typed = &content[comment_start..cursor];
    let namespace = typed[2..].trim_start_matches(|c: char| c.is_ascii_whitespace());
    if namespace.is_empty() {
        // Starter lines only on a comment that is still just its dashes.
        return (ctx.standalone && ctx.tail().trim().is_empty()).then(|| ctx.starters());
    }
    if namespace.len() < NAMESPACE.len() {
        // One letter is any comment starting with "s"; two is an attempt.
        let partial = namespace.len() >= 2
            && NAMESPACE[..namespace.len()].eq_ignore_ascii_case(namespace)
            && ctx.tail().trim().is_empty();
        return partial.then(|| {
            ctx.whole_comment(vec![Cand {
                label: NAMESPACE.into(),
                insert: "-- sqlly ".into(),
                key: String::new(),
                kind: CompletionKind::Keyword,
                detail: "Start a SQLLY directive.".into(),
            }])
        });
    }
    let after_namespace = namespace
        .get(..NAMESPACE.len())
        .filter(|head| head.eq_ignore_ascii_case(NAMESPACE))
        .map(|_| &namespace[NAMESPACE.len()..])?;
    if !after_namespace.is_empty()
        && !after_namespace.starts_with(|c: char| c.is_ascii_whitespace())
    {
        return None;
    }
    let keyword = after_namespace.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let word_len = keyword
        .bytes()
        .take_while(u8::is_ascii_alphanumeric)
        .count();
    let (word, rest) = keyword.split_at(word_len);
    if rest.is_empty() {
        return Some(ctx.families(word));
    }
    let menu = ctx.family_body(&word.to_ascii_lowercase(), rest);
    Some(ctx.finish(menu))
}

const NAMESPACE: &str = "sqlly";

/// One candidate before filtering against what is already typed.
struct Cand {
    label: String,
    insert: String,
    /// The text the typed prefix is matched against (a column's bare name
    /// even when it is written quoted).
    key: String,
    kind: CompletionKind,
    detail: String,
}

impl Cand {
    fn new(text: &str, detail: &str, kind: CompletionKind) -> Self {
        Self {
            label: text.into(),
            insert: text.into(),
            key: text.into(),
            kind,
            detail: detail.into(),
        }
    }

    fn value(text: &str, detail: &str) -> Self {
        Self::new(text, detail, CompletionKind::Value)
    }
}

/// Candidates plus the typed text they complete. `filter` is what the
/// candidates are matched against; `replace_len` is how many bytes before
/// the caret an accepted candidate replaces (the filter, plus an opening
/// quote when the name is being typed quoted).
struct Menu<'a> {
    cands: Vec<Cand>,
    filter: &'a str,
    replace_len: usize,
    /// Example values for a free-form setting: shown while the value is
    /// still empty, never as a filter over what the user types.
    samples: bool,
}

impl<'a> Menu<'a> {
    fn new(cands: Vec<Cand>, filter: &'a str) -> Self {
        Self {
            cands,
            filter,
            replace_len: filter.len(),
            samples: false,
        }
    }

    fn empty() -> Self {
        Self::new(Vec::new(), "")
    }

    fn values(values: &[(&str, &str)], filter: &'a str) -> Self {
        Self::new(
            values
                .iter()
                .map(|(value, detail)| Cand::value(value, detail))
                .collect(),
            filter,
        )
    }

    fn samples(values: &[(&str, &str)], filter: &'a str) -> Self {
        Self {
            samples: true,
            ..Self::values(values, filter)
        }
    }
}

/// How a family writes a column name.
#[derive(Clone, Copy)]
enum NameStyle {
    /// As is; a name holding any of these characters cannot be written.
    Raw(&'static str),
    /// `grid:` / `map:` quoting ([`crate::grid_directive::quote_name`]).
    Quoted,
    /// A formula reference: bare letters/digits/`_`, else `"…"`.
    Formula,
}

impl NameStyle {
    fn write(self, name: &str) -> Option<String> {
        match self {
            Self::Raw(forbidden) => (!name.is_empty()
                && name.trim() == name
                && !name.contains(|c: char| forbidden.contains(c) || c == '\n' || c == '\r'))
            .then(|| name.to_string()),
            Self::Quoted => Some(crate::grid_directive::quote_name(name)),
            Self::Formula => Some(
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    name.to_string()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                },
            ),
        }
    }
}

/// The segment of a `;`-separated body the caret is in, and the settings
/// the rest of the line already holds.
struct Segment<'a> {
    /// The current segment up to the caret, leading whitespace dropped.
    text: &'a str,
    /// `(lowercased key, value)` of every other segment on the line.
    others: Vec<(String, &'a str)>,
}

impl Segment<'_> {
    fn has(&self, key: &str) -> bool {
        self.others.iter().any(|(k, _)| k == key)
    }

    fn has_any(&self, keys: &[&str]) -> bool {
        keys.iter().any(|key| self.has(key))
    }

    /// The 1-based result set another segment (`result:2`) points at.
    fn result(&self) -> Option<usize> {
        self.others
            .iter()
            .find(|(k, _)| k == "result")
            .and_then(|(_, v)| v.trim().parse().ok())
    }
}

/// A setting key: `(key, description, repeatable)`.
type Key = (&'static str, &'static str, bool);

struct Ctx<'a> {
    content: &'a str,
    cursor: usize,
    line_end: usize,
    comment_start: usize,
    /// The comment is the whole line (every family but `name`/`date`
    /// needs that).
    standalone: bool,
    columns: &'a [Vec<String>],
}

impl<'a> Ctx<'a> {
    /// The rest of the line after the caret.
    fn tail(&self) -> &'a str {
        &self.content[self.cursor..self.line_end]
    }

    /// `typed` (a slice ending at the caret) extended to the end of the line.
    fn to_line_end(&self, typed: &str) -> &'a str {
        &self.content[self.cursor - typed.len()..self.line_end]
    }

    /// Whether this is the first non-empty line, where `view:` and the
    /// canvas shape are honored (a lock block's opener line reads as blank).
    fn first_line(&self) -> bool {
        let unwrapped = crate::sqlly_directive::unwrap_lock_blocks(self.content);
        let line_start = self.content[..self.comment_start]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        unwrapped
            .get(..line_start)
            .is_some_and(|before| before.trim().is_empty())
    }

    // ── Assembly ───────────────────────────────────────────────────────

    fn item(&self, index: usize, cand: Cand, span: (usize, usize)) -> CompletionItem {
        CompletionItem {
            label: cand.label,
            insert_text: cand.insert,
            source: match cand.kind {
                CompletionKind::Column => CompletionSource::LiveMetadata,
                CompletionKind::Snippet => CompletionSource::Snippet,
                _ => CompletionSource::Builtin,
            },
            kind: cand.kind,
            score: 10_000 - index as i32,
            detail: cand.detail,
            replace_span: Some(span),
        }
    }

    /// Items that rewrite the comment from its dashes to the caret.
    fn whole_comment(&self, cands: Vec<Cand>) -> DirectiveCompletions {
        let span = (self.comment_start, self.cursor);
        DirectiveCompletions {
            items: cands
                .into_iter()
                .enumerate()
                .map(|(index, cand)| self.item(index, cand, span))
                .collect(),
            replace_start: self.comment_start,
        }
    }

    /// Filter a menu against what is typed and anchor it at the caret. An
    /// accepted item also replaces the rest of the word the caret is in,
    /// so completing inside an existing value does not leave its tail.
    fn finish(&self, menu: Menu<'_>) -> DirectiveCompletions {
        let replace_start = self.cursor - menu.replace_len.min(self.cursor);
        let replace_end = self.cursor + word_len(self.tail());
        let filter = menu.filter.to_lowercase();
        let written = &self.content[replace_start..self.cursor];
        let mut leading = Vec::new();
        let mut containing = Vec::new();
        if !(menu.samples && !filter.is_empty()) {
            for cand in menu.cands {
                // What is already written in full is not a suggestion.
                if replace_end == self.cursor && written.eq_ignore_ascii_case(&cand.insert) {
                    continue;
                }
                let key = cand.key.to_lowercase();
                if key.starts_with(&filter) {
                    leading.push(cand);
                } else if cand.kind == CompletionKind::Column && key.contains(&filter) {
                    containing.push(cand);
                }
            }
        }
        leading.append(&mut containing);
        DirectiveCompletions {
            items: leading
                .into_iter()
                .enumerate()
                .map(|(index, cand)| self.item(index, cand, (replace_start, replace_end)))
                .collect(),
            replace_start,
        }
    }

    // ── Result columns ─────────────────────────────────────────────────

    /// Column names of result set `result` (1-based), or of every result
    /// set when the directive names none, without case-insensitive repeats.
    fn column_names(&self, result: Option<usize>) -> Vec<&'a str> {
        let sets: &[Vec<String>] = match result {
            Some(n) => n
                .checked_sub(1)
                .and_then(|i| self.columns.get(i))
                .map_or(&[], std::slice::from_ref),
            None => self.columns,
        };
        let mut names: Vec<&str> = Vec::new();
        for name in sets.iter().flatten() {
            if !names.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
                names.push(name);
            }
        }
        names
    }

    /// Column candidates written in `style`, each followed by `suffix`,
    /// leaving out the names in `taken`.
    fn columns(
        &self,
        result: Option<usize>,
        style: NameStyle,
        suffix: &str,
        taken: &[String],
    ) -> Vec<Cand> {
        self.column_names(result)
            .into_iter()
            .filter(|name| !taken.iter().any(|t| t.eq_ignore_ascii_case(name)))
            .filter_map(|name| {
                Some(Cand {
                    label: name.to_string(),
                    insert: format!("{}{suffix}", style.write(name)?),
                    key: name.to_string(),
                    kind: CompletionKind::Column,
                    detail: "Result column".into(),
                })
            })
            .collect()
    }

    /// A column menu for a name that may be typed with an opening quote.
    fn column_menu<'t>(
        &self,
        typed: &'t str,
        result: Option<usize>,
        style: NameStyle,
        suffix: &str,
        taken: &[String],
    ) -> Menu<'t> {
        Menu {
            cands: self.columns(result, style, suffix, taken),
            filter: typed.strip_prefix('"').unwrap_or(typed),
            replace_len: typed.len(),
            samples: false,
        }
    }

    /// One candidate per result set of the last run (three when the query
    /// has not run), each followed by `suffix`.
    fn result_numbers(&self, suffix: &str) -> Vec<Cand> {
        let count = if self.columns.is_empty() {
            3
        } else {
            self.columns.len()
        };
        (1..=count)
            .map(|n| {
                let detail = match self.columns.get(n - 1) {
                    Some(columns) if !columns.is_empty() => {
                        let shown: Vec<&str> = columns.iter().take(4).map(String::as_str).collect();
                        let more = if columns.len() > shown.len() {
                            ", …"
                        } else {
                            ""
                        };
                        format!("Result set {n}: {}{more}", shown.join(", "))
                    }
                    _ => format!("Result set {n}"),
                };
                Cand {
                    label: n.to_string(),
                    insert: format!("{n}{suffix}"),
                    key: n.to_string(),
                    kind: CompletionKind::Value,
                    detail,
                }
            })
            .collect()
    }

    // ── Starters, namespace and family keyword ─────────────────────────

    fn starters(&self) -> DirectiveCompletions {
        let first_line = self.first_line();
        let cands = STARTERS
            .iter()
            .filter(|starter| first_line || !starter.first_line_only)
            .map(|starter| Cand {
                label: starter.label.into(),
                insert: match starter.first_line_insert {
                    Some(insert) if first_line => insert.into(),
                    _ => starter.insert.into(),
                },
                key: String::new(),
                kind: CompletionKind::Snippet,
                detail: starter.detail.into(),
            })
            .collect();
        self.whole_comment(cands)
    }

    /// The family keywords starting with `word`. An accepted keyword
    /// rewrites the comment from its dashes in the canonical spelling, so
    /// `--sqlly fo` cannot complete into the `--sqlly format:` form the
    /// format parser ignores.
    fn families(&self, word: &str) -> DirectiveCompletions {
        let first_line = self.first_line();
        let tail = self.tail();
        let tail_word = word_len(tail);
        // Completing the keyword of a directive that already has a body
        // replaces just the keyword.
        let keyword_only = !tail[tail_word..].trim().is_empty();
        let word = word.to_ascii_lowercase();
        let span = (self.comment_start, self.cursor + tail_word);
        let items = FAMILIES
            .iter()
            .filter(|family| family.label.starts_with(&word))
            .filter(|family| self.standalone || family.trailing)
            .filter(|family| first_line || !family.first_line_only)
            .map(|family| {
                let insert = if keyword_only {
                    family.insert.split([':', ' ']).next().unwrap_or_default()
                } else {
                    family.insert
                };
                Cand {
                    label: family.label.into(),
                    insert: format!("-- sqlly {insert}"),
                    key: String::new(),
                    kind: CompletionKind::Keyword,
                    detail: family.detail.into(),
                }
            })
            .enumerate()
            .map(|(index, cand)| self.item(index, cand, span))
            .collect();
        DirectiveCompletions {
            items,
            replace_start: self.comment_start,
        }
    }

    /// The menu inside family `word`; `rest` is everything typed after the
    /// keyword (starting with its colon, index or space).
    fn family_body<'t>(&self, word: &str, rest: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        let colon_body = rest.strip_prefix(':');
        match (word, colon_body) {
            ("format", Some(body)) => self.format(body),
            ("view", Some(body)) => enum_menu(VIEWS, body),
            ("pivot", Some(body)) => self.pivot(body),
            ("chart", Some(body)) => self.chart(body),
            ("conditional", Some(body)) => self.conditional(body),
            ("column", Some(body)) => self.computed_column(body),
            ("layout", Some(body)) => enum_menu(LAYOUTS, body),
            ("focus", Some(body)) => Menu::new(self.result_numbers(""), body.trim_start()),
            ("result", Some(body)) => self.placement(body),
            ("param", Some(body)) => self.param(body),
            ("date", Some(body)) => {
                let detail =
                    crate::localization::text("editor.format_directives.docs.natural_date_title");
                Menu::new(
                    sqlly_intel::natural_date::example_phrases()
                        .iter()
                        .map(|phrase| Cand::value(phrase, &detail))
                        .collect(),
                    body.trim_start(),
                )
            }
            ("lines" | "grid" | "map" | "name", _) => {
                let Some((index, body)) = indexed_body(rest) else {
                    return self.result_index(rest);
                };
                match word {
                    "lines" => Menu::samples(LINES, body.trim_start()),
                    "grid" => self.grid(body, index),
                    "map" => self.map(body, index),
                    // A result name is free text.
                    _ => Menu::empty(),
                }
            }
            // `set <name> = <value>` is free text, and anything else is
            // not a directive the menu knows.
            _ => Menu::empty(),
        }
    }

    /// After `grid ` / `map ` / `lines ` / `name `: the result set number.
    fn result_index<'t>(&self, rest: &'t str) -> Menu<'t> {
        let digits = rest.trim_start();
        if digits.len() == rest.len() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Menu::empty();
        }
        Menu::new(self.result_numbers(": "), digits)
    }

    // ── `;`-separated key/value families ───────────────────────────────

    /// The caret's segment of `body` and the settings around it.
    /// `separator` splits a key from its value; `quoted` families keep `;`
    /// inside double quotes.
    fn segment<'t>(&self, body: &'t str, separator: char, quoted: bool) -> Segment<'t>
    where
        'a: 't,
    {
        let typed = split_top(body, ';', quoted);
        let current = typed.len() - 1;
        let others = split_top(self.to_line_end(body), ';', quoted)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| *index != current)
            .filter_map(|(_, segment)| {
                let segment = segment.trim();
                if segment.is_empty() {
                    return None;
                }
                let (key, value) = segment.split_once(separator).unwrap_or((segment, ""));
                Some((key.trim().to_ascii_lowercase(), value))
            })
            .collect();
        Segment {
            text: typed[current].trim_start(),
            others,
        }
    }

    // ── format ─────────────────────────────────────────────────────────

    fn format<'t>(&self, body: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, '=', false);
        let Some((key, value)) = segment.text.split_once('=') else {
            // The parser reads `column=` from the first setting only.
            if !segment.has("column") {
                return key_menu(&segment, '=', &[FORMAT_COLUMN]);
            }
            let kinds: Vec<&str> = FORMAT_KINDS.iter().map(|(key, ..)| *key).collect();
            let mut keys: Vec<Key> = Vec::new();
            if !segment.has_any(&kinds) {
                keys.extend_from_slice(FORMAT_KINDS);
            }
            if segment.has("number") {
                keys.extend_from_slice(FORMAT_NUMBER_OPTIONS);
            }
            keys.extend_from_slice(FORMAT_OPTIONS);
            return key_menu(&segment, '=', &keys);
        };
        // Format settings are read exactly as written: lowercase keys, no
        // space around `=`.
        match key {
            "column" => Menu::new(self.columns(None, NameStyle::Raw(";"), "", &[]), value),
            "number" => Menu::samples(DECIMALS, value),
            "date" => Menu::samples(DATE_PATTERNS, value),
            "boolean" => Menu::samples(BOOLEAN_LABELS, value),
            "binary" => Menu::values(BINARY_FORMATS, value),
            "text" => Menu::values(TEXT_CASES, value),
            "mask" => Menu::samples(MASKS, value),
            "epoch" => Menu::values(EPOCH_UNITS, value),
            "thousands" => Menu::values(TRUE_FALSE, value),
            "style" => Menu::values(NUMBER_STYLES, value),
            "width" => Menu::samples(WIDTHS, value),
            "zone" => zone_menu(value),
            // `json=` and `template=` are free text.
            _ => Menu::empty(),
        }
    }

    // ── conditional ────────────────────────────────────────────────────

    fn conditional<'t>(&self, body: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, '=', false);
        let Some((key, value)) = segment.text.split_once('=') else {
            // A test with its style, or one whole-column scale/bar: once
            // the line holds either kind, the other is a conflict.
            let rule = segment.has_any(&["when", "text", "fill", "bold"]);
            let whole_column = segment.has_any(&["scale", "bar"]);
            let keys: Vec<Key> = CONDITIONAL_KEYS
                .iter()
                .filter(|(key, ..)| match *key {
                    "column" => true,
                    "scale" | "bar" => !rule && !whole_column,
                    _ => !whole_column,
                })
                .copied()
                .collect();
            return key_menu(&segment, '=', &keys);
        };
        let value = value.trim_start();
        match key.trim().to_ascii_lowercase().as_str() {
            "column" => Menu::new(self.columns(None, NameStyle::Raw(";"), "", &[]), value),
            "when" => Menu::new(
                CONDITIONS
                    .iter()
                    .map(|(label, insert, detail)| Cand {
                        label: (*label).into(),
                        insert: (*insert).into(),
                        key: (*insert).into(),
                        kind: CompletionKind::Value,
                        detail: (*detail).into(),
                    })
                    .collect(),
                value,
            ),
            "text" | "fill" | "bar" => Menu::samples(COLORS, value),
            "scale" => Menu::samples(COLOR_SCALES, value),
            _ => Menu::empty(),
        }
    }

    // ── pivot ──────────────────────────────────────────────────────────

    fn pivot<'t>(&self, body: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, ':', false);
        let Some((key, value)) = segment.text.split_once(':') else {
            return key_menu(&segment, ':', PIVOT_KEYS);
        };
        let result = segment.result();
        let value = value.trim_start();
        // Pivot names are written bare, so its separators cannot be part
        // of one; a number would read as the retired column index.
        let style = NameStyle::Raw(",;:");
        match key.trim().to_ascii_lowercase().as_str() {
            "row" | "column" => self.name_list(value, result, style, false),
            "value" => match value.rsplit_once(':') {
                Some((_, aggregate)) => Menu::values(PIVOT_AGGREGATES, aggregate.trim_start()),
                None => Menu::new(self.columns(result, style, ":", &[]), value),
            },
            "format" => match value.split_once(':') {
                Some((_, options)) => number_format_options(options),
                None => Menu::new(self.columns(result, style, ":", &[]), value),
            },
            "valueformat" => number_format_options(value),
            "result" => Menu::new(self.result_numbers(""), value),
            _ => Menu::empty(),
        }
    }

    /// A comma-separated list of column names: the element being typed,
    /// without the names already listed.
    fn name_list<'t>(
        &self,
        value: &'t str,
        result: Option<usize>,
        style: NameStyle,
        quoted: bool,
    ) -> Menu<'t> {
        let mut elements = split_top(value, ',', quoted);
        let current = elements.pop().unwrap_or_default().trim_start();
        let taken: Vec<String> = elements
            .iter()
            .map(|element| unquote(element.trim()))
            .collect();
        self.column_menu(current, result, style, "", &taken)
    }

    // ── chart ──────────────────────────────────────────────────────────

    fn chart<'t>(&self, body: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, ':', false);
        let Some((key, value)) = segment.text.split_once(':') else {
            // `values` and `value` are one setting.
            let keys: Vec<Key> = CHART_KEYS
                .iter()
                .filter(|(key, ..)| *key != "values" || !segment.has("value"))
                .copied()
                .collect();
            return key_menu(&segment, ':', &keys);
        };
        let result = segment.result();
        let value = value.trim_start();
        let style = NameStyle::Raw(",;:");
        match key.trim().to_ascii_lowercase().as_str() {
            "kind" => Menu::values(CHART_KINDS, value),
            "label" => Menu::new(self.columns(result, style, "", &[]), value),
            "values" | "value" => self.name_list(value, result, style, false),
            "aggregate" => Menu::values(CHART_AGGREGATES, value),
            "order" => Menu::values(CHART_ORDERS, value),
            "top" => Menu::samples(TOP_COUNTS, value),
            "navigate" => Menu::values(TRUE_FALSE, value),
            "result" => Menu::new(self.result_numbers(""), value),
            _ => Menu::empty(),
        }
    }

    // ── grid ───────────────────────────────────────────────────────────

    fn grid<'t>(&self, body: &'t str, index: Option<usize>) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, '=', true);
        let Some((key, value)) = segment.text.split_once('=') else {
            return key_menu(&segment, '=', GRID_KEYS);
        };
        let value = value.trim_start();
        let style = NameStyle::Quoted;
        match key.trim().to_ascii_lowercase().as_str() {
            "order" | "hide" => self.name_list(value, index, style, true),
            "freeze" => Menu::samples(FREEZE_COUNTS, value),
            "sort" => {
                let key = split_top(value, ',', true)
                    .pop()
                    .unwrap_or_default()
                    .trim_start();
                match after_name(key) {
                    None => self.column_menu(key, index, style, "", &[]),
                    Some(direction) => Menu::values(SORT_DIRECTIONS, direction),
                }
            }
            "group" => self.column_menu(value, index, style, "", &[]),
            "filter" => match after_name(value) {
                None => self.column_menu(value, index, style, " ", &[]),
                // A whole operator and its space: what follows is the
                // value (`is ` still becomes `is not` by typing on).
                Some(operator)
                    if operator.ends_with(char::is_whitespace)
                        && FILTER_OPERATORS
                            .iter()
                            .any(|(known, _)| known.eq_ignore_ascii_case(operator.trim_end())) =>
                {
                    Menu::empty()
                }
                Some(operator) => Menu::new(
                    FILTER_OPERATORS
                        .iter()
                        .map(|(operator, detail)| Cand {
                            label: (*operator).into(),
                            insert: format!("{operator} "),
                            key: (*operator).into(),
                            kind: CompletionKind::Keyword,
                            detail: (*detail).into(),
                        })
                        .collect(),
                    operator,
                ),
            },
            "label" => {
                let parts = split_top(value, ':', true);
                match parts.as_slice() {
                    [column] => self.column_menu(column, index, style, ":", &[]),
                    [_, display] => {
                        let display = display.trim_start();
                        let mut menu = self.column_menu(display, index, style, "", &[]);
                        menu.cands.insert(
                            0,
                            Cand::value("off", "Show the stored value, with no label."),
                        );
                        menu
                    }
                    _ => Menu::empty(),
                }
            }
            _ => Menu::empty(),
        }
    }

    // ── map ────────────────────────────────────────────────────────────

    fn map<'t>(&self, body: &'t str, index: Option<usize>) -> Menu<'t>
    where
        'a: 't,
    {
        let segment = self.segment(body, '=', true);
        let Some((key, value)) = segment.text.split_once('=') else {
            return key_menu(&segment, '=', MAP_KEYS);
        };
        let value = value.trim_start();
        let style = NameStyle::Quoted;
        match key.trim().to_ascii_lowercase().as_str() {
            "lat" | "lon" | "geom" | "label" | "color" | "size" | "weight" => {
                self.column_menu(value, index, style, "", &[])
            }
            "tooltip" => self.name_list(value, index, style, true),
            "srid" => Menu::samples(SRIDS, value),
            "mode" => Menu::values(MAP_MODES, value),
            "radius" => Menu::samples(MAP_RADII, value),
            "blur" => Menu::samples(MAP_BLURS, value),
            "intensity" => Menu::samples(MAP_INTENSITIES, value),
            "opacity" => Menu::samples(MAP_OPACITIES, value),
            "palette" => Menu::values(MAP_PALETTES, value),
            "cluster" => Menu::values(ON_OFF, value),
            "basemap" => Menu::values(MAP_BASEMAPS, value),
            "center" => Menu::samples(MAP_CENTERS, value),
            "zoom" => Menu::samples(MAP_ZOOMS, value),
            "fit" => Menu::values(MAP_FITS, value),
            "limit" => Menu::samples(MAP_LIMITS, value),
            "click" => Menu::values(MAP_CLICKS, value),
            "open" => Menu::values(MAP_OPENS, value),
            _ => Menu::empty(),
        }
    }

    // ── column (computed) ──────────────────────────────────────────────

    fn computed_column<'t>(&self, body: &'t str) -> Menu<'t>
    where
        'a: 't,
    {
        // `<Name> = <formula>[; after=<column>][; result=<n>]`: the name is
        // free text.
        let Some((_, rest)) = body.split_once('=') else {
            return Menu::empty();
        };
        let result = formula_options(self.to_line_end(rest))
            .into_iter()
            .skip(1)
            .find_map(|option| {
                let (key, value) = option.split_once('=')?;
                key.trim()
                    .eq_ignore_ascii_case("result")
                    .then(|| value.trim().parse().ok())?
            });
        let parts = formula_options(rest);
        let current = parts[parts.len() - 1];
        if parts.len() == 1 {
            return self.formula(current, result);
        }
        let option = current.trim_start();
        let Some((key, value)) = option.split_once('=') else {
            let used: Vec<String> = parts[1..parts.len() - 1]
                .iter()
                .filter_map(|part| part.split_once('='))
                .map(|(key, _)| key.trim().to_ascii_lowercase())
                .collect();
            let segment = Segment {
                text: option,
                others: used.into_iter().map(|key| (key, "")).collect(),
            };
            return key_menu(&segment, '=', COMPUTED_OPTIONS);
        };
        let value = value.trim_start();
        match key.trim().to_ascii_lowercase().as_str() {
            "after" => Menu::new(self.columns(result, NameStyle::Raw(";\""), "", &[]), value),
            "result" => Menu::new(self.result_numbers(""), value),
            _ => Menu::empty(),
        }
    }

    /// Inside a formula: `!` references a column, `@` a `-- sqlly set`
    /// variable, and a bare word starts a function name.
    fn formula<'t>(&self, source: &'t str, result: Option<usize>) -> Menu<'t> {
        // `!"Order To` — a quoted reference still open at the caret.
        if let Some(open) = source.rfind("!\"")
            && !source[open + 2..].contains('"')
        {
            return Menu {
                cands: self.columns(result, NameStyle::Formula, "", &[]),
                filter: &source[open + 2..],
                replace_len: source.len() - open - 1,
                samples: false,
            };
        }
        // Inside a text literal nothing is a name.
        if source.matches('"').count() % 2 == 1 {
            return Menu::empty();
        }
        let word_start = source
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map_or(source.len(), |(index, _)| index);
        let (before, word) = source.split_at(word_start);
        if before.ends_with('!') {
            return Menu::new(self.columns(result, NameStyle::Formula, "", &[]), word);
        }
        if before.ends_with('@') {
            let variables = self
                .content
                .lines()
                .filter_map(|line| {
                    let line = line.trim_start();
                    match classify(line)? {
                        Directive::Set { body } => {
                            let (name, _) = body.split_once('=')?;
                            Some(Cand::new(
                                name.trim(),
                                "Variable from a set directive",
                                CompletionKind::Parameter,
                            ))
                        }
                        _ => None,
                    }
                })
                .collect();
            return Menu::new(variables, word);
        }
        if !word.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return Menu::empty();
        }
        let detail =
            crate::localization::text("editor.format_directives.completion_worksheet_function");
        Menu::new(
            crate::formula::FUNCTIONS
                .iter()
                .map(|name| Cand {
                    label: (*name).into(),
                    insert: format!("{name}("),
                    key: (*name).into(),
                    kind: CompletionKind::Function,
                    detail: detail.clone(),
                })
                .collect(),
            word,
        )
    }

    // ── result (canvas placement) ──────────────────────────────────────

    /// Every cell of the canvas the first line declares (2x2 when it
    /// declares none), as `<row>|<column>`.
    fn placement<'t>(&self, body: &'t str) -> Menu<'t> {
        let max = crate::result_layout::MAX_CANVAS_SIZE;
        let unwrapped = crate::sqlly_directive::unwrap_lock_blocks(self.content);
        let (rows, columns) = unwrapped
            .lines()
            .find(|line| !line.trim().is_empty())
            .and_then(|line| match classify(line.trim_start())? {
                Directive::Canvas { rows, columns } => Some((rows, columns)),
                _ => None,
            })
            .filter(|(rows, columns)| (1..=max).contains(rows) && (1..=max).contains(columns))
            .unwrap_or((2, 2));
        let cells = (1..=rows)
            .flat_map(|row| (1..=columns).map(move |column| (row, column)))
            .map(|(row, column)| {
                Cand::value(
                    &format!("{row}|{column}"),
                    &format!("Row {row}, column {column} of the canvas"),
                )
            })
            .collect();
        Menu::new(cells, body.trim_start())
    }

    // ── param ──────────────────────────────────────────────────────────

    /// `@name <type> [= default]`: the query's undeclared `@parameters`,
    /// then the types, then the defaults a type has words for.
    fn param<'t>(&self, body: &'t str) -> Menu<'t> {
        let body = body.trim_start();
        let Some(name_end) = body.find(|c: char| c.is_ascii_whitespace()) else {
            return Menu::new(self.undeclared_parameters(), body);
        };
        let after_name = body[name_end..].trim_start();
        let Some((ty, default)) = after_name.split_once('=') else {
            // A finished type has nothing after it but `= default`.
            if after_name.contains(|c: char| c.is_ascii_whitespace()) {
                return Menu::empty();
            }
            return Menu::values(PARAM_TYPES, after_name);
        };
        let mut defaults = vec![("NULL", "No value until one is entered.")];
        if matches!(
            ty.trim().to_ascii_lowercase().as_str(),
            "bool" | "boolean" | "bit"
        ) {
            defaults.extend([("true", ""), ("false", "")]);
        }
        Menu::values(&defaults, default.trim_start())
    }

    /// `@name` tokens in the buffer no `param:` line declares yet.
    fn undeclared_parameters(&self) -> Vec<Cand> {
        let declared: Vec<String> = crate::query_params::parse_param_directives(self.content)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let bytes = self.content.as_bytes();
        let mut names: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < bytes.len() && names.len() < MAX_PARAMETER_NAMES {
            if bytes[i] != b'@' {
                i += 1;
                continue;
            }
            let start = i;
            i += 1;
            let len = bytes[i..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                .count();
            i += len;
            // `@@ROWCOUNT` is a system variable and `a@b` an address.
            let glued = start > 0
                && (bytes[start - 1] == b'@'
                    || bytes[start - 1].is_ascii_alphanumeric()
                    || bytes[start - 1] == b'_');
            if len == 0 || glued || bytes[start + 1].is_ascii_digit() {
                continue;
            }
            let name = &self.content[start..i];
            // The token on the directive line itself is the one being typed.
            let own = start >= self.comment_start && start < self.line_end;
            if !own
                && !declared.iter().any(|d| d.eq_ignore_ascii_case(name))
                && !names.iter().any(|n| n.eq_ignore_ascii_case(name))
            {
                names.push(name);
            }
        }
        names
            .into_iter()
            .map(|name| {
                Cand::new(
                    name,
                    "Parameter used in this query",
                    CompletionKind::Parameter,
                )
            })
            .collect()
    }
}

const MAX_PARAMETER_NAMES: usize = 50;

/// Most zone names shown at once; typing narrows the rest.
const MAX_ZONES: usize = 40;

fn enum_menu<'t>(values: &[(&str, &str)], body: &'t str) -> Menu<'t> {
    Menu::values(values, body.trim_start())
}

/// The settings in `keys` the line does not hold yet (repeatable ones
/// always), each written with its `separator`; a key with no value
/// (`bold`) is written bare.
fn key_menu<'t>(segment: &Segment<'t>, separator: char, keys: &[Key]) -> Menu<'t> {
    let cands = keys
        .iter()
        .filter(|(key, _, repeatable)| *repeatable || !segment.has(key))
        .map(|(key, detail, _)| {
            let text = if BARE_KEYS.contains(key) {
                (*key).to_string()
            } else {
                format!("{key}{separator}")
            };
            Cand::new(&text, detail, CompletionKind::Keyword)
        })
        .collect();
    Menu::new(cands, segment.text)
}

/// The `align=…,decimals=…` option list of a pivot `format:` /
/// `valueformat:` setting.
fn number_format_options(options: &str) -> Menu<'_> {
    let mut elements: Vec<&str> = options.split(',').collect();
    let current = elements.pop().unwrap_or_default().trim_start();
    let Some((key, value)) = current.split_once('=') else {
        let used: Vec<String> = elements
            .iter()
            .filter_map(|element| element.split_once('='))
            .map(|(key, _)| key.trim().to_ascii_lowercase())
            .collect();
        let mut cands: Vec<Cand> = PIVOT_FORMAT_OPTIONS
            .iter()
            .filter(|(key, _)| !used.iter().any(|u| u == key))
            .map(|(key, detail)| Cand::new(&format!("{key}="), detail, CompletionKind::Keyword))
            .collect();
        // `default` stands alone: it is the whole option list.
        if elements.iter().all(|element| element.trim().is_empty()) {
            cands.push(Cand::value("default", "The standard number format."));
        }
        return Menu::new(cands, current);
    };
    let value = value.trim_start();
    match key.trim().to_ascii_lowercase().as_str() {
        "align" => Menu::values(ALIGNMENTS, value),
        "decimals" => Menu::samples(DECIMALS, value),
        "red" | "parens" | "thousands" => Menu::values(TRUE_FALSE, value),
        _ => Menu::empty(),
    }
}

/// `zone=`: the fixed modes, plus the IANA zones matching what is typed.
fn zone_menu(value: &str) -> Menu<'_> {
    let mut menu = Menu::values(ZONE_MODES, value);
    let typed = value.to_lowercase();
    if !typed.is_empty() {
        let mut containing = Vec::new();
        for zone in chrono_tz::TZ_VARIANTS {
            let name = zone.name();
            let lower = name.to_lowercase();
            if lower.starts_with(&typed) {
                menu.cands.push(Cand::value(name, "IANA time zone"));
            } else if lower.contains(&typed) {
                // Matched by city (`york` → America/New_York): key it on
                // the typed text so the prefix filter keeps it.
                containing.push(Cand {
                    key: format!("{typed}{name}"),
                    ..Cand::value(name, "IANA time zone")
                });
            }
        }
        menu.cands.append(&mut containing);
        menu.cands.truncate(ZONE_MODES.len() + MAX_ZONES);
    }
    menu
}

/// Split on `separator`, outside double quotes when `quoted`. Always at
/// least one part.
fn split_top(text: &str, separator: char, quoted: bool) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        if c == '"' && quoted {
            in_quotes = !in_quotes;
        } else if c == separator && !in_quotes {
            parts.push(&text[start..index]);
            start = index + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

/// The `;`-separated parts of a computed column's text after its `=`: the
/// formula, then its options. A `;` inside quotes or parentheses belongs to
/// the formula (the same split `computed_columns` makes).
fn formula_options(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quotes = false;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '(' if !in_quotes => depth += 1,
            ')' if !in_quotes => depth -= 1,
            ';' if !in_quotes && depth <= 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// What follows a finished column name at the start of `text` — a bare
/// name ends at whitespace, a quoted one at its closing quote and the
/// whitespace after it. `None` while the name is still being typed.
fn after_name(text: &str) -> Option<&str> {
    let end = if text.starts_with('"') {
        let mut chars = text.char_indices().skip(1).peekable();
        let mut close = None;
        while let Some((index, c)) = chars.next() {
            if c != '"' {
                continue;
            }
            // `""` is a literal quote inside the name.
            if chars.peek().is_some_and(|(_, next)| *next == '"') {
                chars.next();
                continue;
            }
            close = Some(index + 1);
            break;
        }
        close?
    } else {
        text.find(char::is_whitespace)?
    };
    let rest = &text[end..];
    rest.starts_with(char::is_whitespace)
        .then(|| rest.trim_start())
}

/// A list element as a bare name, for telling which columns are taken.
fn unquote(element: &str) -> String {
    match element
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        Some(inner) => inner.replace("\"\"", "\""),
        None => element.trim_matches(['[', ']', '`']).to_string(),
    }
}

/// Length of the word (letters, digits, `_`) at the start of `text`.
fn word_len(text: &str) -> usize {
    text.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum()
}

// ── Vocabulary ─────────────────────────────────────────────────────────

/// A whole-line starter offered on a bare `--`.
struct Starter {
    label: &'static str,
    insert: &'static str,
    /// What to write instead on the first non-empty line (where a `view:`
    /// line can lead).
    first_line_insert: Option<&'static str>,
    first_line_only: bool,
    detail: &'static str,
}

const fn starter(label: &'static str, insert: &'static str, detail: &'static str) -> Starter {
    Starter {
        label,
        insert,
        first_line_insert: None,
        first_line_only: false,
        detail,
    }
}

const STARTERS: &[Starter] = &[
    Starter {
        first_line_insert: Some(
            "-- sqlly view:pivot\n-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum",
        ),
        ..starter(
            "Pivot result view",
            "-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum",
            "Show the interactive pivot tab; separate groups with semicolons and fields with commas.",
        )
    },
    Starter {
        first_line_only: true,
        ..starter(
            "Grid result view",
            "-- sqlly view:grid",
            "Show the standard result grid.",
        )
    },
    starter(
        "Column format",
        "-- sqlly format: column=Amount; number=2; thousands=true",
        "Override result formatting by column name.",
    ),
    starter(
        "Result set name",
        "-- sqlly name 1: Result name",
        "Name a result set by its one-based position.",
    ),
    Starter {
        first_line_only: true,
        ..starter(
            "Stacked result layout",
            "-- sqlly 2x2\n-- sqlly result: 1|1",
            "Define a result canvas and place the following result-producing statement.",
        )
    },
    Starter {
        first_line_insert: Some(
            "-- sqlly view:chart\n-- sqlly chart: kind:bar; label:Region; values:Revenue",
        ),
        ..starter(
            "Chart",
            "-- sqlly chart: kind:bar; label:Region; values:Revenue",
            "Chart the result: a label column and one or more value columns.",
        )
    },
    starter(
        "Grid columns",
        "-- sqlly grid: sort=Total desc; freeze=1",
        "Order, hide, freeze, sort, group, filter or label the grid's columns.",
    ),
    starter(
        "Conditional formatting",
        "-- sqlly conditional: column=Status; when=is Failed; fill=#F8696B",
        "Style the cells of a column that pass a test.",
    ),
    starter(
        "Computed column",
        "-- sqlly column: Total = !Quantity * !Price",
        "Add a column calculated from the row's other columns.",
    ),
    starter(
        "Map",
        "-- sqlly map: lat=Latitude; lon=Longitude; mode=points",
        "Plot the result's coordinates on a map.",
    ),
    starter(
        "Query parameter",
        "-- sqlly param: @customer_id int",
        "Declare a typed parameter the run prompts for.",
    ),
    starter(
        "Variable",
        "-- sqlly set region = West",
        "Set a ${name} variable for this query.",
    ),
    starter(
        "Result layout",
        "-- sqlly layout: stacked",
        "Show the results stacked, tabbed or on a canvas.",
    ),
    starter(
        "Result height",
        "-- sqlly lines: 10",
        "Rows each stacked result shows before it scrolls.",
    ),
    starter(
        "Default result",
        "-- sqlly focus: 2",
        "The result set shown first.",
    ),
];

/// A family keyword as the menu after `-- sqlly ` offers it.
struct Family {
    /// Shown, and matched against the typed keyword.
    label: &'static str,
    /// Written after `-- sqlly `.
    insert: &'static str,
    detail: &'static str,
    /// Honored only on the first non-empty line.
    first_line_only: bool,
    /// Honored after SQL code on the same line.
    trailing: bool,
}

const fn family(label: &'static str, insert: &'static str, detail: &'static str) -> Family {
    Family {
        label,
        insert,
        detail,
        first_line_only: false,
        trailing: false,
    }
}

const fn canvas(shape: &'static str, detail: &'static str) -> Family {
    Family {
        first_line_only: true,
        ..family(shape, shape, detail)
    }
}

const FAMILIES: &[Family] = &[
    family(
        "format:",
        "format: ",
        "Format how one result column is displayed.",
    ),
    Family {
        first_line_only: true,
        ..family("view:", "view:", "The result view that opens first.")
    },
    family(
        "pivot:",
        "pivot: ",
        "Rows, columns and measure of the pivot view.",
    ),
    family(
        "chart:",
        "chart: ",
        "Kind, label and values of the chart view.",
    ),
    family(
        "grid:",
        "grid: ",
        "Order, hide, freeze, sort, group, filter and label grid columns.",
    ),
    family("grid <n>:", "grid 1: ", "Grid settings for one result set."),
    family(
        "conditional:",
        "conditional: ",
        "Style the cells of a column that pass a test.",
    ),
    family(
        "column:",
        "column: ",
        "Add a column calculated by a formula.",
    ),
    family("map:", "map: ", "Plot the result on a map."),
    family("map <n>:", "map 1: ", "Map settings for one result set."),
    Family {
        trailing: true,
        ..family("name <n>:", "name 1: ", "Name a result set.")
    },
    family(
        "layout:",
        "layout: ",
        "Show the results stacked, tabbed or on a canvas.",
    ),
    family(
        "lines:",
        "lines: ",
        "Rows each stacked result shows before it scrolls.",
    ),
    family(
        "lines <n>:",
        "lines 1: ",
        "Rows one stacked result shows before it scrolls.",
    ),
    family("focus:", "focus: ", "The result set shown first."),
    family(
        "result:",
        "result: ",
        "Place the next statement's result on the canvas.",
    ),
    family("param:", "param: ", "Declare a typed query parameter."),
    family("set", "set ", "Set a ${name} variable for this query."),
    Family {
        trailing: true,
        ..family(
            "date:",
            "date:",
            "The phrase a date literal was written from.",
        )
    },
    canvas("1x2", "A canvas of one row and two columns."),
    canvas("2x1", "A canvas of two rows and one column."),
    canvas("2x2", "A canvas of two rows and two columns."),
    canvas("2x3", "A canvas of two rows and three columns."),
    canvas("3x2", "A canvas of three rows and two columns."),
    canvas("3x3", "A canvas of three rows and three columns."),
];

/// Settings written without a value.
const BARE_KEYS: &[&str] = &["bold"];

const TRUE_FALSE: &[(&str, &str)] = &[("true", ""), ("false", "")];
const ON_OFF: &[(&str, &str)] = &[("on", ""), ("off", "")];

const VIEWS: &[(&str, &str)] = &[
    ("grid", "The standard result grid."),
    ("pivot", "The interactive pivot tab."),
    ("chart", "The chart tab."),
    ("map", "The map tab."),
];

const LAYOUTS: &[(&str, &str)] = &[
    ("stacked", "Every result visible, one above the next."),
    ("tabbed", "One result at a time, on tabs."),
    ("canvas", "Results placed on a board of cells."),
];

const LINES: &[(&str, &str)] = &[
    ("5", "Rows shown before the result scrolls"),
    ("10", ""),
    ("15", ""),
    ("20", ""),
    ("30", ""),
];

const DECIMALS: &[(&str, &str)] = &[
    ("0", "Decimal places"),
    ("1", ""),
    ("2", ""),
    ("3", ""),
    ("4", ""),
];

const FORMAT_COLUMN: Key = ("column", "The result column this rule applies to.", false);

/// The display rules; a line carries one.
const FORMAT_KINDS: &[Key] = &[
    ("number", "A number with fixed decimal places.", false),
    ("date", "A date or time by strftime pattern.", false),
    ("boolean", "Text for true and for false.", false),
    (
        "binary",
        "Binary values as hex, base64, UUID or ULID.",
        false,
    ),
    ("text", "Change the casing of text.", false),
    ("json", "Show the value at a path inside JSON.", false),
    ("template", "Build the cell text from a template.", false),
    (
        "mask",
        "Hide all but the leading and trailing characters.",
        false,
    ),
    ("epoch", "Read a number as a Unix timestamp.", false),
];

const FORMAT_NUMBER_OPTIONS: &[Key] = &[
    ("thousands", "Thousands separators for the number.", false),
    (
        "style",
        "Show the number as fixed, percent or bytes.",
        false,
    ),
];

const FORMAT_OPTIONS: &[Key] = &[
    ("width", "Column width in font-relative units.", false),
    ("zone", "Time zone a date or time is shown in.", false),
];

const DATE_PATTERNS: &[(&str, &str)] = &[
    ("%Y-%m-%d", "2026-03-09"),
    ("%Y-%m-%d %H:%M:%S", "2026-03-09 14:05:00"),
    ("%d %b %Y", "09 Mar 2026"),
    ("%m/%d/%Y", "03/09/2026"),
    ("%b %d, %Y %I:%M %p", "Mar 09, 2026 02:05 PM"),
    ("%H:%M", "14:05"),
];

const BOOLEAN_LABELS: &[(&str, &str)] = &[
    ("Yes/No", "Text for true / text for false"),
    ("True/False", ""),
    ("On/Off", ""),
    ("Y/N", ""),
];

const BINARY_FORMATS: &[(&str, &str)] = &[
    ("hex", "Hexadecimal digits."),
    ("base64", "Base64 text."),
    ("uuid", "16-byte values as a UUID."),
    ("ulid", "16-byte values as a ULID."),
];

const TEXT_CASES: &[(&str, &str)] = &[
    ("upper", "UPPER CASE"),
    ("lower", "lower case"),
    ("title", "Title Case"),
    ("none", "As stored"),
];

const MASKS: &[(&str, &str)] = &[
    ("0/4", "Keep the last four characters"),
    ("2/2", "Keep the first two and last two"),
    ("4/0", "Keep the first four characters"),
];

const EPOCH_UNITS: &[(&str, &str)] = &[
    ("auto", "Seconds or milliseconds, by size."),
    ("seconds", "Seconds since 1970."),
    ("milliseconds", "Milliseconds since 1970."),
    ("s", "Seconds since 1970."),
    ("ms", "Milliseconds since 1970."),
];

const NUMBER_STYLES: &[(&str, &str)] = &[
    ("fixed", "Plain fixed-point number."),
    ("percent", "A fraction as a percentage."),
    ("bytes", "A byte count as KB, MB, GB."),
];

const WIDTHS: &[(&str, &str)] = &[
    ("20dp", "1dp is a quarter of the grid font size"),
    ("40dp", ""),
    ("60dp", ""),
    ("80dp", ""),
    ("120dp", ""),
];

const ZONE_MODES: &[(&str, &str)] = &[
    ("utc", "Always UTC."),
    ("local", "This computer's time zone."),
    ("stored", "As the server returned it."),
];

const CONDITIONAL_KEYS: &[Key] = &[
    ("column", "The result column the rule styles.", false),
    ("when", "The test a cell must pass.", false),
    ("text", "Text color for cells that pass.", false),
    ("fill", "Background color for cells that pass.", false),
    ("bold", "Bold text for cells that pass.", false),
    ("scale", "Shade the whole column between two colors.", false),
    ("bar", "Draw a data bar in every cell.", false),
];

/// `when=` tests: `(label, inserted text, description)`.
const CONDITIONS: &[(&str, &str, &str)] = &[
    ("null", "null", "The cell is NULL."),
    ("not null", "not null", "The cell has a value."),
    ("> <number>", "> ", "Greater than a number."),
    ("< <number>", "< ", "Less than a number."),
    (
        "between <a> and <b>",
        "between 0 and 100",
        "Between two numbers.",
    ),
    ("is <text>", "is ", "Exactly this text."),
    ("contains <text>", "contains ", "Contains this text."),
];

const COLORS: &[(&str, &str)] = &[
    ("#F8696B", "Red"),
    ("#FFEB84", "Yellow"),
    ("#63BE7B", "Green"),
    ("#5B9BD5", "Blue"),
    ("#B00020", "Dark red"),
    ("#1B7F3B", "Dark green"),
    ("#FFFFFF", "White"),
    ("#000000", "Black"),
];

const COLOR_SCALES: &[(&str, &str)] = &[
    ("#63BE7B..#F8696B", "Green (low) to red (high)"),
    ("#F8696B..#63BE7B", "Red (low) to green (high)"),
    ("#FFFFFF..#5B9BD5", "White (low) to blue (high)"),
];

const PIVOT_KEYS: &[Key] = &[
    ("row", "Columns that group the rows.", false),
    ("column", "Columns that spread across the top.", false),
    ("value", "The measured column and its aggregate.", false),
    ("format", "Number format for one field.", true),
    ("valueformat", "Number format for the measure.", false),
    ("result", "The result set to pivot.", false),
];

const PIVOT_AGGREGATES: &[(&str, &str)] = &[
    ("sum", "Total of the values."),
    ("count", "Number of rows."),
    ("avg", "Average of the values."),
    ("min", "Smallest value."),
    ("max", "Largest value."),
    ("first", "First value met."),
];

const PIVOT_FORMAT_OPTIONS: &[(&str, &str)] = &[
    ("align", "Left, center or right."),
    ("decimals", "Decimal places."),
    ("thousands", "Thousands separators."),
    ("red", "Negative numbers in red."),
    ("parens", "Negative numbers in parentheses."),
];

const ALIGNMENTS: &[(&str, &str)] = &[("left", ""), ("center", ""), ("right", "")];

const CHART_KEYS: &[Key] = &[
    ("kind", "The chart type (required).", false),
    ("label", "The column that names each point.", false),
    ("values", "The columns that are plotted.", false),
    ("aggregate", "How rows sharing a label combine.", false),
    ("order", "The order of the categories.", false),
    ("top", "Keep only the largest categories.", false),
    ("navigate", "Clicking a point selects its grid row.", false),
    ("result", "The result set to chart.", false),
];

const CHART_KINDS: &[(&str, &str)] = &[
    ("bar", ""),
    ("line", ""),
    ("area", ""),
    ("scatter", ""),
    ("histogram", ""),
    ("pie", ""),
    ("donut", ""),
];

const CHART_AGGREGATES: &[(&str, &str)] = &[
    ("none", "Plot every row."),
    ("sum", "Total per label."),
    ("average", "Average per label."),
    ("minimum", "Smallest per label."),
    ("maximum", "Largest per label."),
    ("count", "Rows per label."),
];

const CHART_ORDERS: &[(&str, &str)] = &[
    ("source", "As the query returned them."),
    ("category", "By label, ascending."),
    ("value", "By value, largest first."),
];

const TOP_COUNTS: &[(&str, &str)] = &[("5", "Categories kept"), ("10", ""), ("20", ""), ("50", "")];

const GRID_KEYS: &[Key] = &[
    ("order", "Columns in display order.", false),
    ("hide", "Columns to hide.", false),
    ("freeze", "Leading columns kept in view.", false),
    ("sort", "Columns to sort by, each asc or desc.", false),
    ("group", "The column to group rows by.", false),
    ("filter", "Keep rows whose column passes a test.", true),
    ("label", "Show another column's text for a column.", true),
];

const FREEZE_COUNTS: &[(&str, &str)] =
    &[("1", "Leading columns kept in view"), ("2", ""), ("3", "")];

const SORT_DIRECTIONS: &[(&str, &str)] = &[("asc", "Smallest first."), ("desc", "Largest first.")];

const FILTER_OPERATORS: &[(&str, &str)] = &[
    ("contains", "Text contains."),
    ("not contains", "Text does not contain."),
    ("begins with", "Text starts with."),
    ("ends with", "Text ends with."),
    ("is", "Text equals."),
    ("is not", "Text differs from."),
    ("matches", "Text matches a pattern."),
    ("=", "Number equals."),
    ("!=", "Number differs from."),
    (">", "Number greater than."),
    (">=", "Number at least."),
    ("<", "Number less than."),
    ("<=", "Number at most."),
    ("between", "Number between a and b."),
    ("not between", "Number outside a and b."),
    ("in", "One of the values, separated by |."),
];

const MAP_KEYS: &[Key] = &[
    ("lat", "The latitude column.", false),
    ("lon", "The longitude column.", false),
    ("geom", "A geometry or geography column.", false),
    ("srid", "The coordinate system of the geometry.", false),
    ("label", "The column that names each point.", false),
    ("tooltip", "Columns shown when a point is hovered.", false),
    ("mode", "Points, a heatmap, or both.", false),
    ("color", "The column that colors the points.", false),
    ("size", "The column that sizes the points.", false),
    ("weight", "The column that weights the heatmap.", false),
    ("radius", "Heatmap radius in pixels.", false),
    ("blur", "Heatmap blur in pixels.", false),
    ("intensity", "Heatmap intensity.", false),
    ("opacity", "Layer opacity, 0 to 1.", false),
    ("palette", "The heatmap's colors.", false),
    ("cluster", "Group nearby points.", false),
    ("basemap", "The background map.", false),
    ("center", "Starting center as latitude,longitude.", false),
    ("zoom", "Starting zoom level.", false),
    ("fit", "Start zoomed to the data.", false),
    ("limit", "Most rows plotted.", false),
    ("click", "What clicking a point does.", false),
    ("open", "Whether the map opens after a run.", false),
];

const SRIDS: &[(&str, &str)] = &[("4326", "WGS 84 (GPS)"), ("3857", "Web Mercator")];

const MAP_MODES: &[(&str, &str)] = &[
    ("points", "One marker per row."),
    ("heatmap", "Density shading."),
    ("both", "Markers over the heatmap."),
];

const MAP_RADII: &[(&str, &str)] = &[("10", "Pixels, 6 to 120"), ("20", ""), ("40", "")];
const MAP_BLURS: &[(&str, &str)] = &[("10", "Pixels, 0 to 200"), ("15", ""), ("30", "")];
const MAP_INTENSITIES: &[(&str, &str)] = &[("0.5", "0 to 10"), ("1", ""), ("2", "")];
const MAP_OPACITIES: &[(&str, &str)] = &[("0.25", "0 to 1"), ("0.5", ""), ("0.75", ""), ("1", "")];

const MAP_PALETTES: &[(&str, &str)] = &[
    ("heat", ""),
    ("fire", ""),
    ("viridis", ""),
    ("ocean", ""),
    ("accent", "The theme's accent color."),
];

const MAP_BASEMAPS: &[(&str, &str)] = &[
    ("osm", "OpenStreetMap tiles."),
    ("none", "No background map."),
];

const MAP_CENTERS: &[(&str, &str)] = &[("0,0", "latitude,longitude")];
const MAP_ZOOMS: &[(&str, &str)] = &[
    ("2", "0 (world) to 19 (street)"),
    ("5", ""),
    ("10", ""),
    ("15", ""),
];
const MAP_FITS: &[(&str, &str)] = &[("data", "Zoom to the plotted rows.")];
const MAP_LIMITS: &[(&str, &str)] = &[("1000", "Most rows plotted"), ("10000", ""), ("50000", "")];

const MAP_CLICKS: &[(&str, &str)] = &[
    ("select", "Select the point's grid row."),
    ("none", "Do nothing."),
];

const MAP_OPENS: &[(&str, &str)] = &[
    ("auto", "Open the map when the query finishes."),
    ("manual", "Open it from the result's toolbar."),
];

const COMPUTED_OPTIONS: &[Key] = &[
    ("after", "The column this one is placed after.", false),
    ("result", "The result set the column is added to.", false),
];

const PARAM_TYPES: &[(&str, &str)] = &[
    ("auto", "Typed by the value entered."),
    ("text", ""),
    ("int", ""),
    ("decimal", ""),
    ("bool", ""),
    ("date", ""),
    ("datetime", ""),
    ("time", ""),
    ("uuid", ""),
    ("json", ""),
    ("binary", ""),
];

#[cfg(test)]
mod tests;
