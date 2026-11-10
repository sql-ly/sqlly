use super::*;

impl EntityInputHandler for SqlEditor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = self.offset_from_utf16(range.start);
        let end = self.offset_from_utf16(range.end);
        self.content.get(start..end).map(|s| s.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(sel) = self.selection() {
            let start = self.offset_to_utf16(sel.start);
            let end = self.offset_to_utf16(sel.end);
            Some(UTF16Selection {
                range: start..end,
                reversed: self
                    .selection_anchor
                    .map(|a| a > self.cursor)
                    .unwrap_or(false),
            })
        } else {
            let cursor_utf16 = self.offset_to_utf16(self.cursor);
            Some(UTF16Selection {
                range: cursor_utf16..cursor_utf16,
                reversed: false,
            })
        }
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let r = self.marked_range.as_ref()?;
        Some(self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // The preedit text stays in the buffer as committed text; only the
        // composition state (underline, undo grouping) ends.
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        // Vim Normal/Visual: printable keys are commands, never text. IME
        // composition commits (`marked_range` set) and host range edits
        // (`range` set) are not keystrokes and bypass the modal layer.
        // Commands are fed from `handle_key_down`; this is the safety net
        // that drops a native insertText / paste so it cannot land in the buffer.
        if self.vim.as_ref().is_some_and(|v| v.intercepts_text())
            && range.is_none()
            && self.marked_range.is_none()
        {
            return;
        }
        // Dot-segment completion: with the suggestion popup open, typing `.`
        // accepts the next dotted-path segment (`Acc|` → `Accounting.|`)
        // instead of inserting a literal dot. Never during IME composition or
        // for range-targeted host edits — those fall through unchanged.
        if text == "."
            && range.is_none()
            && self.marked_range.is_none()
            && self.try_advance_completion_to_dot(cx)
        {
            return;
        }
        // Feature 5: comma+space at end of line in SELECT column context →
        // ",\n{indent}". GPUI delivers typing as individual characters, so we
        // detect the space keystroke when the previous char is a comma. Never
        // during IME composition — the space may be a candidate-page key —
        // and never in multi-cursor mode, where the space fans out instead.
        if self.marked_range.is_none()
            && (self.try_automagic_comma_repair(text, range.as_ref(), cx)
                || self.try_automagic_connector_repair(text, range.as_ref(), cx))
        {
            return;
        }
        if self.marked_range.is_none()
            && self.extra_cursors.is_empty()
            && self.try_comma_space_expansion(text, range.as_ref(), cx)
        {
            return;
        }

        // Explicit range > active composition span > current selection. An
        // IME commit arrives with `range = None` and expects to replace the
        // preedit text, not the (empty) selection after it.
        let target = range
            .map(|r| self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end))
            .or_else(|| self.marked_range.clone());
        if let Some(target) = target {
            // Range-targeted replacements (IME commits, host edits) are
            // primary-only.
            self.clear_extra_cursors();
            let target = utf8_safe_edit_range(&self.content, target);
            let composing = self.marked_range.take().is_some();
            let mut new_content = String::with_capacity(self.content.len() + text.len());
            new_content.push_str(&self.content[..target.start]);
            new_content.push_str(text);
            new_content.push_str(&self.content[target.end..]);
            // A composition session snapshots once, when it starts.
            if !composing {
                self.push_delta(target.clone(), text.len());
            }
            self.content = new_content.into();
            self.invalidate_text_derived_caches();
            self.cursor = target.start + text.len();
            self.selection_anchor = None;
            self.after_text_change(cx);
        } else if !self.extra_cursors.is_empty() {
            // Multi-cursor typing: the keystroke lands at every caret.
            self.insert_at_all_cursors(text, cx);
        } else {
            self.replace_selection(text, cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_blocks_editing() {
            return;
        }
        // IME composition is primary-only.
        self.clear_extra_cursors();
        // Target: explicit range > active composition span > selection.
        let target = range
            .map(|r| self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end))
            .or_else(|| self.marked_range.clone())
            .or_else(|| self.selection())
            .unwrap_or(self.cursor..self.cursor);
        let target = utf8_safe_edit_range(&self.content, target);

        // One undo entry per composition session: snapshot when it starts,
        // not on every candidate update. Starting a composition also closes
        // the completion popup — its items were filtered against committed
        // text and would go stale under the preedit.
        if self.marked_range.is_none() {
            self.push_history();
            self.close_completions(cx);
        }
        let mut new_content = String::with_capacity(self.content.len() + new_text.len());
        new_content.push_str(&self.content[..target.start]);
        new_content.push_str(new_text);
        new_content.push_str(&self.content[target.end..]);
        self.content = new_content.into();
        self.invalidate_text_derived_caches();

        let marked_start = target.start;
        let marked_end = marked_start + new_text.len();
        self.marked_range = (!new_text.is_empty()).then_some(marked_start..marked_end);

        // Caret within the preedit: `new_selected_range` is UTF-16 relative
        // to `new_text`; default to the end of the composition.
        match new_selected_range {
            Some(sel) => {
                let start = marked_start + utf16_to_byte_within(new_text, sel.start);
                let end = marked_start + utf16_to_byte_within(new_text, sel.end);
                self.selection_anchor = (start != end).then_some(start);
                self.cursor = end;
            }
            None => {
                self.selection_anchor = None;
                self.cursor = marked_end;
            }
        }
        self.after_text_change(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let start = self.offset_from_utf16(range_utf16.start);
        let end = self.offset_from_utf16(range_utf16.end);
        let (line, col_start) = self.line_col_for_offset(start);
        let (_, col_end) = self.line_col_for_offset(end);
        if let Some((layout, tab_map)) = self.layout_for_line(line) {
            let x_start = layout.x_for_index(tab_map.to_display(col_start));
            let x_end = layout.x_for_index(tab_map.to_display(col_end));
            let y = element_bounds.origin.y + self.line_height * self.display_line_of(line) as f32;
            Some(Bounds {
                origin: Point::new(
                    element_bounds.origin.x + self.line_number_width + x_start,
                    y,
                ),
                size: gpui::size(x_end - x_start, self.line_height),
            })
        } else {
            None
        }
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let local_y = point.y - bounds.origin.y;
        let line = (local_y / self.line_height) as usize;
        let line = line.min(self.line_count().saturating_sub(1));
        let line_start = self.line_start_offset(line);
        let local_x = point.x - bounds.origin.x - self.line_number_width;
        if let Some((layout, tab_map)) = self.layout_for_line(line) {
            let col = layout.closest_index_for_x(local_x);
            let col = tab_map.to_content(col);
            Some(self.offset_to_utf16(line_start + col))
        } else {
            None
        }
    }
}
