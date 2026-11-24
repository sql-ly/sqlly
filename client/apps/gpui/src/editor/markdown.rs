//! Markdown mode, for notebook Markdown cells. The same editor with the SQL
//! machinery switched off (completion engine, hover and signature cards,
//! lints and INSERT hints, formatting, statement spans, SQL folds, value
//! lookups, date helper, go to definition, AI ghost text) and Markdown in
//! its place: Markdown colouring, the notebook's Markdown completions and
//! hover cards, reference diagnostics, list continuation and nesting, and
//! paired brackets, backticks and emphasis marks. Everything here looks at
//! the caret's line, so it stays cheap per keystroke.

use super::*;

/// What the editor's text is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditorLanguage {
    #[default]
    Sql,
    Markdown,
}

/// A list item (or quote line) that Enter continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListLine<'a> {
    pub indent: &'a str,
    /// The marker as the next line writes it (`- `, `3. `, `- [ ] `, `> `).
    pub next: String,
    /// Byte offset in the line where the item's text starts.
    pub content_start: usize,
}

/// The list item, task or quote `line` holds, if any.
pub(crate) fn list_line(line: &str) -> Option<ListLine<'_>> {
    use super::markdown_syntax::{list_marker_len, task_box_len};
    let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    let (indent, rest) = line.split_at(indent_len);
    if let Some(quote) = rest.strip_prefix('>') {
        let space = usize::from(quote.starts_with(' '));
        return Some(ListLine {
            indent,
            next: "> ".into(),
            content_start: indent_len + 1 + space,
        });
    }
    let len = list_marker_len(rest)?;
    let marker = &rest[..len];
    let after = &rest[len..];
    let spaces = after.len() - after.trim_start_matches(' ').len();
    let mut content_start = indent_len + len + spaces;
    let mut next = match marker.as_bytes()[0] {
        b'0'..=b'9' => {
            let n: u64 = marker[..len - 1].parse().unwrap_or(0);
            format!("{}{} ", n.saturating_add(1), &marker[len - 1..])
        }
        _ => format!("{marker} "),
    };
    if let Some(task) = task_box_len(&line[content_start..]) {
        next.push_str("[ ] ");
        content_start += task;
        content_start += usize::from(line[content_start..].starts_with(' '));
    }
    Some(ListLine {
        indent,
        next,
        content_start: content_start.min(line.len()),
    })
}

/// What Enter does at byte `col` of `line`: `Some((replace, text, caret))`
/// edits the line (`replace` is a byte range in the line, `caret` the
/// caret's offset in the line afterwards), `None` is a plain line break.
pub(crate) fn enter_edit(
    line: &str,
    col: usize,
    unit: &str,
) -> Option<(Range<usize>, String, usize)> {
    let item = list_line(line)?;
    if col < item.content_start {
        return None;
    }
    if line[item.content_start..].trim().is_empty() {
        // An empty item ends the list (a nested one moves out a level).
        return Some(if item.indent.is_empty() {
            (0..line.len(), String::new(), 0)
        } else {
            let outdent = item
                .indent
                .strip_prefix(unit)
                .or_else(|| item.indent.strip_prefix('\t'))
                .unwrap_or("");
            let text = format!("{outdent}{}", &line[item.indent.len()..]);
            let caret = text.len();
            (0..line.len(), text, caret)
        });
    }
    let text = format!("\n{}{}", item.indent, item.next);
    let caret = col + text.len();
    Some((col..col, text, caret))
}

fn closer(c: char) -> Option<char> {
    Some(match c {
        '[' => ']',
        '(' => ')',
        '`' => '`',
        '*' => '*',
        '_' => '_',
        _ => return None,
    })
}

fn opens_after(prev: Option<char>) -> bool {
    prev.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\''))
}

fn closes_before(next: Option<char>) -> bool {
    next.is_none_or(|c| {
        c.is_whitespace() || matches!(c, ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?')
    })
}

/// What typing `c` between `before` and `after` (the rest of the line)
/// does: `Some((text, caret))` inserts `text` with the caret `caret` bytes
/// into it, `Some(("", 1))` steps over the closer already there, `None`
/// types `c` as usual.
pub(crate) fn pair_edit(before: &str, after: &str, c: char) -> Option<(String, usize)> {
    let prev = before.chars().next_back();
    let next = after.chars().next();
    if matches!(c, ']' | ')' | '`' | '*' | '_') && next == Some(c) {
        // `*|*` + `*` opens bold (`**|**`) rather than stepping out.
        let opening_bold = c == '*'
            && prev == Some('*')
            && opens_after(before[..before.len() - 1].chars().next_back());
        if !opening_bold {
            return Some((String::new(), 1));
        }
        return Some(("**".into(), 1));
    }
    let close = closer(c)?;
    // Inside an open `{{…}}` or inline code, marks are literal text.
    let in_reference = before
        .rfind("{{")
        .is_some_and(|open| !before[open..].contains("}}"));
    let in_code = c != '`' && before.bytes().filter(|b| *b == b'`').count() % 2 == 1;
    if !closes_before(next) || in_reference || in_code {
        return None;
    }
    let at_line_start = before.trim_start_matches([' ', '\t']).is_empty();
    let pairs = match c {
        '[' | '(' => true,
        '`' => prev != Some('`'),
        // A `*` starting a line is a bullet; `_` inside a word is text.
        _ => !at_line_start && opens_after(prev),
    };
    pairs.then(|| (format!("{c}{close}"), c.len_utf8()))
}

impl SqlEditor {
    /// Switch between SQL and Markdown. Clears what the other language left
    /// behind (diagnostics, hints, cards, caches keyed by the text).
    pub fn set_language(&mut self, language: EditorLanguage, cx: &mut Context<Self>) {
        if self.language == language {
            return;
        }
        self.language = language;
        self.completion_catalog = self.with_notebook_references(self.base_catalog.clone());
        self.statement_cache.borrow_mut().0 = None;
        *self.comment_state_cache.borrow_mut() = None;
        self.folds.borrow_mut().0 = None;
        self.insert_hints = std::rc::Rc::new(Vec::new());
        self.insert_hints_version = self.insert_hints_version.wrapping_add(1);
        self.signature_help = None;
        self.date_helper = None;
        self.close_completions(cx);
        self.clear_hover_ui();
        self.apply_lint_diagnostics(Vec::new(), cx);
        self.schedule_insert_hints(cx);
        self.forward_to_shared_view(cx, move |view, cx| view.set_language(language, cx));
        cx.notify();
    }

    pub fn language(&self) -> EditorLanguage {
        self.language
    }

    pub(super) fn is_markdown(&self) -> bool {
        self.language == EditorLanguage::Markdown
    }

    /// Wrap an action handler so it does nothing in Markdown mode (the SQL
    /// tools: formatting, comments, value lookups, validation, AI ghost
    /// text, IntelliSense feedback, identifier qualification).
    pub(super) fn sql_only<A: 'static>(
        handler: fn(&mut Self, &A, &mut Window, &mut Context<Self>),
    ) -> impl Fn(&mut Self, &A, &mut Window, &mut Context<Self>) {
        move |editor, action, window, cx| {
            if !editor.is_markdown() {
                handler(editor, action, window, cx);
            }
        }
    }

    /// Per-line fence state as comment states (depth 1 inside a fence), so
    /// the renderer's cross-line context and shape-cache keys work as they
    /// do for SQL block comments.
    pub(super) fn markdown_line_states(&self) -> Vec<crate::sql_syntax::CommentState> {
        markdown_syntax::fence_states(&self.content)
            .into_iter()
            .map(|inside| crate::sql_syntax::CommentState {
                depth: u16::from(inside),
            })
            .collect()
    }

    pub(super) fn markdown_completions(
        &self,
        manual: bool,
    ) -> Option<crate::directive_completion::DirectiveCompletions> {
        let refs = self.notebook_refs.as_ref()?;
        crate::notebook::editing::markdown::completions(refs, &self.content, self.cursor, manual)
    }

    pub(super) fn markdown_hover(&self, offset: usize) -> Option<(HoverInfo, (usize, usize))> {
        let refs = self.notebook_refs.as_ref()?;
        crate::notebook::editing::markdown::hover(refs, &self.content, offset)
    }

    /// Recompute the reference diagnostics (names the notebook does not
    /// have) from the cell's text and the notebook's current names.
    pub(super) fn refresh_markdown_diagnostics(&mut self, cx: &mut Context<Self>) {
        let problems = match &self.notebook_refs {
            Some(refs) => crate::notebook::editing::markdown::problems(refs, &self.content),
            None => Vec::new(),
        };
        let lints = problems
            .into_iter()
            .map(|p| ValidationDiagnostic {
                severity: if p.unknown {
                    DiagnosticSeverity::Warning
                } else {
                    DiagnosticSeverity::Info
                },
                source: DiagnosticSource::Lint,
                line: self.content[..p.start.min(self.content.len())]
                    .matches('\n')
                    .count(),
                identifier: None,
                start_byte: p.start,
                end_byte: p.end,
                message: p.message,
                explainer: None,
                fix: None,
            })
            .collect();
        self.apply_lint_diagnostics(lints, cx);
    }

    /// Enter in Markdown: continue the list, task list or quote the caret is
    /// in (an empty item ends it); otherwise a line break that keeps the
    /// line's indentation.
    pub(super) fn markdown_enter(&mut self, cx: &mut Context<Self>) {
        self.clear_extra_cursors();
        if !self.has_selection() {
            let (line, col) = self.line_col_for_offset(self.cursor);
            let start = self.line_start_offset(line);
            let end = self.line_end_offset(line);
            let unit = self.editor_prefs.indent_unit();
            if let Some((range, text, caret)) = enter_edit(&self.content[start..end], col, &unit) {
                self.selection_anchor = Some(start + range.start);
                self.cursor = start + range.end;
                self.replace_selection(&text, cx);
                self.cursor = (start + caret).min(self.content.len());
                cx.notify();
                return;
            }
        }
        self.insert_newline(cx);
    }

    /// Tab / Shift+Tab with the caret on a list item: nest or un-nest the
    /// whole item, keeping the caret on the same text. `false` when the
    /// caret is not on one.
    pub(super) fn markdown_indent_item(&mut self, outdent: bool, cx: &mut Context<Self>) -> bool {
        if self.has_selection() || !self.extra_cursors.is_empty() {
            return false;
        }
        let (line, _) = self.line_col_for_offset(self.cursor);
        let start = self.line_start_offset(line);
        let end = self.line_end_offset(line);
        let text = &self.content[start..end];
        if list_line(text).is_none_or(|item| item.next == "> ") {
            return false;
        }
        let unit = self.editor_prefs.indent_unit();
        let (range, insert) = if outdent {
            let leading = text.len() - text.trim_start_matches([' ', '\t']).len();
            let width = if text.starts_with(unit.as_str()) {
                unit.len()
            } else if text.starts_with('\t') {
                1
            } else {
                leading.min(unit.len())
            };
            if width == 0 {
                return true;
            }
            (start..start + width, String::new())
        } else {
            (start..start, unit)
        };
        let caret = self.cursor;
        let delta = insert.len() as isize - range.len() as isize;
        self.selection_anchor = Some(range.start);
        self.cursor = range.end;
        self.replace_selection(&insert, cx);
        self.cursor = caret
            .saturating_add_signed(delta)
            .max(start)
            .min(self.content.len());
        self.retire_automatic_completion(cx);
        cx.notify();
        true
    }

    /// Typing in Markdown: wrap a one-line selection in `[ ]`, `( )`, `` ` ``,
    /// `*` or `_` (keeping it selected, so a second `*` makes it bold), pair
    /// an opening bracket, backtick or emphasis mark, and step over a closer
    /// that is already there. `false` lets the text through.
    pub(super) fn markdown_type(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return false;
        };
        if let Some(selection) = self.selection() {
            let Some(close) = closer(c) else {
                return false;
            };
            if self.content[selection.clone()].contains('\n') {
                return false;
            }
            let inner = self.content[selection.clone()].to_string();
            self.replace_selection(&format!("{c}{inner}{close}"), cx);
            self.selection_anchor = Some(selection.start + c.len_utf8());
            self.cursor = selection.start + c.len_utf8() + inner.len();
            cx.notify();
            return true;
        }
        let (line, _) = self.line_col_for_offset(self.cursor);
        let start = self.line_start_offset(line);
        let end = self.line_end_offset(line);
        let before = &self.content[start..self.cursor];
        let after = &self.content[self.cursor..end];
        let Some((insert, caret)) = pair_edit(before, after, c) else {
            return false;
        };
        let at = self.cursor;
        if insert.is_empty() {
            self.cursor = at + caret;
            self.goal_column = None;
            self.close_completions(cx);
            cx.notify();
            return true;
        }
        self.replace_selection(&insert, cx);
        self.cursor = at + caret;
        cx.notify();
        true
    }

    /// Backspace between an empty pair (`(|)`, `[|]`, `` `|` ``, `*|*`,
    /// `_|_`) removes both halves.
    pub(super) fn markdown_backspace(&mut self, cx: &mut Context<Self>) -> bool {
        if self.has_selection() || !self.extra_cursors.is_empty() || self.cursor == 0 {
            return false;
        }
        let prev = self.content[..self.cursor].chars().next_back();
        let next = self.content[self.cursor..].chars().next();
        let (Some(prev), Some(next)) = (prev, next) else {
            return false;
        };
        if closer(prev) != Some(next) {
            return false;
        }
        self.selection_anchor = Some(self.cursor - prev.len_utf8());
        self.cursor += next.len_utf8();
        self.replace_selection("", cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_continues_lists_tasks_and_quotes() {
        let at_end = |line: &str| enter_edit(line, line.len(), "  ").map(|(_, t, _)| t);
        assert_eq!(at_end("- milk").as_deref(), Some("\n- "));
        assert_eq!(at_end("  * eggs").as_deref(), Some("\n  * "));
        assert_eq!(at_end("9. nine").as_deref(), Some("\n10. "));
        assert_eq!(at_end("1) one").as_deref(), Some("\n2) "));
        assert_eq!(at_end("- [x] done").as_deref(), Some("\n- [ ] "));
        assert_eq!(at_end("> quoted").as_deref(), Some("\n> "));
        assert_eq!(at_end("plain"), None);
        assert_eq!(at_end("-not a list"), None);
        assert_eq!(
            enter_edit("- milk", 1, "  "),
            None,
            "caret inside the marker"
        );
    }

    #[test]
    fn enter_on_an_empty_item_ends_or_outdents_it() {
        assert_eq!(enter_edit("- ", 2, "  "), Some((0..2, String::new(), 0)));
        assert_eq!(
            enter_edit("- [ ] ", 6, "  "),
            Some((0..6, String::new(), 0))
        );
        assert_eq!(
            enter_edit("    - ", 6, "  "),
            Some((0..6, "  - ".into(), 4)),
            "a nested empty item moves out a level"
        );
    }

    #[test]
    fn pairs_open_close_and_step_over() {
        assert_eq!(pair_edit("see ", "", '['), Some(("[]".into(), 1)));
        assert_eq!(pair_edit("call", " x", '('), Some(("()".into(), 1)));
        assert_eq!(pair_edit("a ", "", '`'), Some(("``".into(), 1)));
        assert_eq!(pair_edit("a `", "`", '`'), Some((String::new(), 1)));
        assert_eq!(pair_edit("``", "", '`'), None, "the third tick of a fence");
        assert_eq!(pair_edit("a ", "", '*'), Some(("**".into(), 1)));
        assert_eq!(pair_edit("a *", "*", '*'), Some(("**".into(), 1)), "bold");
        assert_eq!(pair_edit("a **b*", "*", '*'), Some((String::new(), 1)));
        assert_eq!(pair_edit("", "", '*'), None, "a bullet");
        assert_eq!(pair_edit("  ", "", '*'), None, "an indented bullet");
        assert_eq!(pair_edit("snake", "", '_'), None, "inside a word");
        assert_eq!(pair_edit("a ", "word", '['), None, "before a word");
        assert_eq!(pair_edit("[x", "]", ']'), Some((String::new(), 1)));
        assert_eq!(pair_edit("a", "", 'b'), None);
        assert_eq!(pair_edit("{{", "}}", '_'), None, "inside a reference");
        assert_eq!(pair_edit("a `x ", "`", '*'), None, "inside inline code");
    }
}
