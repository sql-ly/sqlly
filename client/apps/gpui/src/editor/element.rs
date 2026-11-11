use super::*;
use sqlly_intel::intellisense::SignatureHelp;

pub struct EditorElement {
    pub(super) editor: Entity<SqlEditor>,
}

/// Overscan lines above/below the viewport to prevent pop-in during scroll.
pub(super) const EDITOR_OVERSCAN_LINES: usize = 8;
/// Bounded fallback when viewport height is unknown (first render).
pub(super) const EDITOR_FALLBACK_VISIBLE_LINES: usize = 200;
/// Blank lines of scrollable slack past the last line, so the end of a query
/// can be scrolled up off the bottom edge instead of being pinned there.
pub(super) const EDITOR_SCROLL_PAST_END_LINES: usize = 5;

/// One shaped line, tagged with both its buffer line index (for content:
/// line numbers, byte ranges) and its display index (for y positioning —
/// collapsed folds make the two diverge).
pub struct PaintLine {
    absolute_line: usize,
    display_line: usize,
    shaped: ShapedLine,
}

/// Ranged prepaint state: only the visible lines (+ overscan) are shaped.
/// `first_line` is the *display* index of `lines[0]`.
pub struct EditorPrepaintState {
    first_line: usize,
    lines: Vec<PaintLine>,
}

/// Compute the visible line range from scroll position and viewport height.
/// Returns a bounded range even when viewport height is zero/unknown.
pub(super) fn visible_line_range(
    line_count: usize,
    line_height: f32,
    scroll_y: f32,
    viewport_height: f32,
) -> Range<usize> {
    if line_count == 0 {
        return 0..0;
    }
    if line_height <= 0.0 || viewport_height <= 0.0 {
        let end = line_count.min(EDITOR_FALLBACK_VISIBLE_LINES);
        return 0..end.max(1);
    }
    let top = (-scroll_y).max(0.0);
    let bottom = top + viewport_height;
    let first = ((top / line_height).floor() as usize).saturating_sub(EDITOR_OVERSCAN_LINES);
    let last = ((bottom / line_height).ceil() as usize)
        .saturating_add(EDITOR_OVERSCAN_LINES)
        .saturating_add(1)
        .min(line_count);
    let first = first.min(line_count);
    let last = last.max(first + 1).min(line_count);
    first..last
}

fn paint_info_popup_frame(window: &mut Window, bounds: Bounds<Pixels>) {
    window.paint_quad(gpui::fill(bounds, Theme::popup_bg()));
    window.paint_quad(gpui::quad(
        bounds,
        gpui::Corners::default(),
        gpui::transparent_black(),
        gpui::Edges::all(px(1.0)),
        Theme::popup_border(),
        gpui::BorderStyle::Solid,
    ));
}

/// Shape one card line into a paintable line, bolding the bold segments.
fn shape_card_line(
    window: &mut Window,
    line: &super::info_cards::CardLine,
    font: &Font,
    bold_font: &Font,
    font_size: Pixels,
) -> ShapedLine {
    let text = SharedString::from(line.text());
    let runs: Vec<TextRun> = if line.segments.is_empty() {
        vec![TextRun {
            len: 0,
            font: font.clone(),
            color: Theme::text(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }]
    } else {
        line.segments
            .iter()
            .map(|seg| TextRun {
                len: seg.text.len(),
                font: if seg.bold {
                    bold_font.clone()
                } else {
                    font.clone()
                },
                color: seg.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            })
            .collect()
    };
    window
        .text_system()
        .shape_line(text, font_size, &runs, None)
}

/// Advance width of one character of the (monospace) editor font at the
/// card's size.
fn card_char_width(window: &mut Window, font: &Font, font_size: Pixels) -> f32 {
    let probe = window.text_system().shape_line(
        SharedString::from("0000000000"),
        font_size,
        &[TextRun {
            len: 10,
            font: font.clone(),
            color: Theme::text(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    f32::from(probe.width) / 10.0
}

/// Runs for a signature label: the active parameter bold in full text color
/// (located by its recorded byte range), the rest dimmed.
fn signature_runs(help: &SignatureHelp, font: &Font, bold_font: &Font, dim: Hsla) -> Vec<TextRun> {
    let run = |len: usize, font: &Font, color: Hsla| TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let active = help
        .active_parameter
        .and_then(|i| help.parameter_ranges.get(i).copied())
        .filter(|&(start, end)| start < end && end <= help.label.len());
    let Some((start, end)) = active else {
        return vec![run(help.label.len(), font, dim)];
    };
    let mut runs = Vec::new();
    if start > 0 {
        runs.push(run(start, font, dim));
    }
    runs.push(run(end - start, bold_font, Theme::text()));
    if end < help.label.len() {
        runs.push(run(help.label.len() - end, font, dim));
    }
    runs
}

impl EditorElement {
    /// Paint the "to improve this, press F2" chip beneath (or above) the
    /// completion popup and stamp its bounds so a click opens the feedback
    /// dialog. Shared by the populated popup and the empty ("No suggestions")
    /// one — the empty state is exactly when suggestions are missing and
    /// feedback matters most, so the affordance must appear there too.
    #[allow(clippy::too_many_arguments)]
    fn paint_improve_hint(
        &self,
        popup_x: Pixels,
        popup_y: Pixels,
        popup_width: Pixels,
        popup_height: Pixels,
        cursor_y: Pixels,
        font: &Font,
        font_size: Pixels,
        line_height: Pixels,
        popup_alpha: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let label_text = SharedString::from(crate::localization::text(
            "editor.element.improve_hint_prefix",
        ));
        let label_len = label_text.len();
        let shaped_label = window.text_system().shape_line(
            label_text,
            font_size * 0.85,
            &[TextRun {
                len: label_len,
                font: font.clone(),
                color: Theme::text_dim().opacity(popup_alpha),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let key_text = SharedString::from("F2");
        let key_len = key_text.len();
        let shaped_key = window.text_system().shape_line(
            key_text,
            font_size * 0.8,
            &[TextRun {
                len: key_len,
                font: font.clone(),
                color: Theme::text().opacity(popup_alpha),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let hint_height = line_height * 0.9;
        let hint_width = popup_width;
        // popup_y > cursor_y means the popup sits below the caret line as
        // usual; otherwise it flipped above it.
        let flipped = popup_y < cursor_y;
        let hint_y = if flipped {
            popup_y - hint_height
        } else {
            popup_y + popup_height
        };
        let hint_bounds = Bounds {
            origin: Point::new(popup_x, hint_y),
            size: gpui::size(hint_width, hint_height),
        };
        window.paint_quad(gpui::fill(
            hint_bounds,
            Theme::popup_bg().opacity(popup_alpha),
        ));
        window.paint_quad(gpui::quad(
            hint_bounds,
            gpui::Corners::default(),
            gpui::transparent_black(),
            gpui::Edges::all(px(1.0)),
            Theme::popup_border().opacity(popup_alpha),
            gpui::BorderStyle::Solid,
        ));
        let _ = shaped_label.paint(
            Point::new(popup_x + px(6.0), hint_y),
            hint_height,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        // The F2 keycap: a small raised, rounded, bordered box.
        let cap_pad_x = px(5.0);
        let cap_bounds = Bounds {
            origin: Point::new(
                popup_x + px(6.0) + shaped_label.width + px(6.0),
                hint_y + px(2.0),
            ),
            size: gpui::size(shaped_key.width + cap_pad_x * 2.0, hint_height - px(4.0)),
        };
        window.paint_quad(gpui::quad(
            cap_bounds,
            gpui::Corners::all(px(3.0)),
            Theme::row_hover().opacity(popup_alpha),
            gpui::Edges::all(px(1.0)),
            Theme::text_dim().opacity(popup_alpha * 0.6),
            gpui::BorderStyle::Solid,
        ));
        let _ = shaped_key.paint(
            Point::new(cap_bounds.origin.x + cap_pad_x, cap_bounds.origin.y),
            cap_bounds.size.height,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        // Stamp the chip's bounds for `handle_mouse_down`'s hit-test: a click
        // here opens the dialog exactly like F2.
        self.editor
            .read(cx)
            .feedback_hint_bounds
            .set(Some(hint_bounds));
    }
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Refresh INSERT value labels before the width scan and prepaint's
        // line build: small buffers recompute synchronously so labels never
        // blank (and reflow the text under the caret) for the debounce after
        // each keystroke.
        self.editor
            .update(cx, |editor, _| editor.ensure_insert_hints_current());
        let editor = self.editor.read(cx);
        // Display lines, not buffer lines: collapsed folds shrink the
        // scrollable content height.
        let line_count = editor.display_line_count().max(1);
        // Pad the content with blank lines past the end so the last line can
        // be scrolled up away from the bottom edge (VS Code's "scroll beyond
        // last line"). Only the height grows — nothing paints down there.
        let height = editor.line_height * (line_count + EDITOR_SCROLL_PAST_END_LINES) as f32;

        // Content width: the widest (tab-expanded) line in em advances, so the
        // horizontal scroll range covers long lines instead of clipping them.
        // width stays at 100% via min-width so short content still fills the
        // viewport. Revision-keyed: the full-document scan runs once per
        // edit, not once per frame.
        let tab_width = editor.editor_prefs.spaces_per_tab() as usize;
        let max_chars = editor.max_display_cols(tab_width);
        let em = window
            .text_system()
            .shape_line(
                gpui::SharedString::from("M"),
                editor.font_size,
                &[gpui::TextRun {
                    len: 1,
                    font: editor.font.clone(),
                    color: gpui::black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        let gutter_width = SqlEditor::fitted_gutter_width(
            editor.editor_prefs.show_line_numbers,
            editor.line_count(),
            em,
        );
        let content_width = gutter_width + em * max_chars as f32 + px(24.0);

        let mut style = gpui::Style::default();
        style.size.width = gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0));
        style.min_size.width = gpui::Length::Definite(content_width.into());
        style.size.height = gpui::Length::Definite(height.into());
        // The scroll container is a flex column and gpui's default
        // flex_shrink is 1.0, so without these taffy shrinks this element to
        // exactly the viewport height — content never overflows, max_offset
        // stays 0, and the editor cannot scroll on either axis.
        style.min_size.height = gpui::Length::Definite(height.into());
        style.flex_shrink = 0.0;
        // Mouse hit-testing reads the stored width, so keep it in step with
        // what paints.
        if self.editor.read(cx).line_number_width != gutter_width {
            self.editor
                .update(cx, |editor, _| editor.line_number_width = gutter_width);
        }
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (
            font,
            font_size,
            tab_width,
            link_hover_range,
            marked_range,
            content,
            line_height,
            scroll_offset,
            viewport_bounds,
        ) = {
            let editor = self.editor.read(cx);
            (
                editor.font.clone(),
                editor.font_size,
                editor.editor_prefs.spaces_per_tab() as usize,
                editor.link_hover_range,
                editor.marked_range.clone(),
                editor.content.clone(),
                editor.line_height,
                editor.scroll_handle.offset(),
                editor.scroll_handle.bounds(),
            )
        };

        // A cursor inside a collapsed fold (via find navigation, undo, or any
        // programmatic move) reopens that fold before layout, so the caret is
        // never parked in hidden space.
        self.editor.update(cx, |editor, _| {
            editor.reveal_cursor_folds();
        });

        let line_index = self.editor.read(cx).line_index();
        let fold_map = self.editor.read(cx).fold_map_snapshot();
        let total_display_lines = fold_map.display_line_count(line_index.line_count());
        let vrange = visible_line_range(
            total_display_lines,
            line_height.into(),
            scroll_offset.y.into(),
            viewport_bounds.size.height.into(),
        );

        let mut paint_lines: Vec<PaintLine> = Vec::with_capacity(vrange.len());
        let mut layouts: Vec<ShapedLine> = Vec::with_capacity(vrange.len());
        let mut tab_maps: Vec<crate::tab_map::TabMap> = Vec::with_capacity(vrange.len());

        let dialect = self.editor.read(cx).database_type().family();
        // Per-line block-comment depth for the whole document, so a `/* … */`
        // opened above the viewport still paints as a comment here.
        let comment_states = self.editor.read(cx).comment_states();
        let syntax_bold = self.editor.read(cx).editor_prefs.syntax_bold;
        let semantic_names = self.editor.read(cx).semantic_names();
        let probe = ShapeProbe::current(&font, font_size, tab_width, dialect)
            .with_bold(syntax_bold, &semantic_names);
        self.editor
            .read(cx)
            .shape_cache
            .borrow_mut()
            .sync_probe(&probe);

        let hints = self.editor.read(cx).insert_hints_snapshot();

        for display_line in vrange.clone() {
            let absolute_line = fold_map
                .display_to_buffer(display_line)
                .min(line_index.line_count().saturating_sub(1));
            let line_start = line_index.line_start(absolute_line);
            let line_end = line_index.line_end(&content, absolute_line);
            let line_text = &content[line_start..line_end];

            let hint_lo = hints.partition_point(|h| h.offset < line_start);
            let hint_hi = hints.partition_point(|h| h.offset < line_end);
            let mut label_store = Vec::new();
            let mut inlay_cols = Vec::new();
            for hint in &hints[hint_lo..hint_hi] {
                let col = hint.offset - line_start;
                if col > line_text.len() || !line_text.is_char_boundary(col) {
                    continue;
                }
                inlay_cols.push(col);
                label_store.push(format!("{} ", hint.label));
            }
            let inlays: Vec<crate::tab_map::Inlay<'_>> = inlay_cols
                .iter()
                .zip(label_store.iter())
                .map(|(&content_col, text)| crate::tab_map::Inlay { content_col, text })
                .collect();
            // The key carries each label's column as well as its text: two
            // lines with equal text and equal labels at different value
            // positions must not share a shaped line / tab map.
            let comment_entry = comment_states
                .get(absolute_line)
                .copied()
                .unwrap_or_default();
            // The key carries the entry depth too: identical text highlights
            // differently inside and outside a block comment, so the two must
            // not share a cache slot.
            let cache_key_buf = (!label_store.is_empty() || comment_entry.is_inside()).then(|| {
                use std::fmt::Write as _;
                let mut key = String::with_capacity(line_text.len() + label_store.len() * 20);
                key.push_str(line_text);
                for (col, label) in inlay_cols.iter().zip(label_store.iter()) {
                    let _ = write!(key, "\u{0}{col}:{label}");
                }
                if comment_entry.is_inside() {
                    let _ = write!(key, "\u{0}c:{}", comment_entry.depth);
                }
                key
            });
            let cache_key = cache_key_buf.as_deref().unwrap_or(line_text);

            // Lines carrying a transient overlay (link hover, IME preedit)
            // are styled differently from their plain text and bypass the
            // cache; everything else shapes once per (text, probe).
            let has_overlay = link_hover_range.is_some_and(|(s, e)| s < line_end && e > line_start)
                || marked_range
                    .as_ref()
                    .is_some_and(|m| m.start < line_end && m.end > line_start);

            let cached = (!has_overlay)
                .then(|| self.editor.read(cx).shape_cache.borrow().get(cache_key))
                .flatten();

            let (map, shaped) = if let Some(hit) = cached {
                hit
            } else {
                let map = crate::tab_map::TabMap::with_inlays(line_text, tab_width, &inlays);
                let mut runs = SqlEditor::syntax_runs(
                    line_text,
                    &font,
                    dialect,
                    comment_entry,
                    &syntax_bold,
                    &semantic_names,
                );
                if let Some((link_start, link_end)) = link_hover_range
                    && link_start < line_end
                    && link_end > line_start
                {
                    runs = restyle_link_span(
                        runs,
                        link_start.max(line_start) - line_start,
                        link_end.min(line_end) - line_start,
                        &font,
                    );
                }
                // IME preedit: underline the in-progress composition span, as
                // every platform's native text field does.
                if let Some(marked) = &marked_range
                    && marked.start < line_end
                    && marked.end > line_start
                {
                    runs = underline_span(
                        runs,
                        marked.start.max(line_start) - line_start,
                        marked.end.min(line_end) - line_start,
                    );
                }
                let expanded = map.expand_runs(&runs);
                let shaped = window.text_system().shape_line(
                    SharedString::from(map.display().to_string()),
                    font_size,
                    &expanded,
                    None,
                );
                if !has_overlay {
                    self.editor
                        .read(cx)
                        .shape_cache
                        .borrow_mut()
                        .insert(cache_key, (map.clone(), shaped.clone()));
                }
                (map, shaped)
            };

            tab_maps.push(map);
            layouts.push(shaped.clone());
            paint_lines.push(PaintLine {
                absolute_line,
                display_line,
                shaped,
            });
        }

        self.editor.update(cx, |editor, _| {
            editor.last_layout = layouts;
            editor.last_layout_first_line = vrange.start;
            editor.last_tab_maps = std::rc::Rc::new(tab_maps);
            editor.last_bounds = Some(bounds);
        });
        // The thumbs were sized in `render` from the previous layout. The
        // editor is drawn from the frame cache, so nothing else would
        // re-render it to pick up a changed line count or viewport.
        let scrollbar_stale = {
            let editor = self.editor.read(cx);
            editor.scrollbar_geometry
                != (
                    editor.scroll_handle.bounds().size,
                    editor.scroll_handle.max_offset(),
                )
        };
        if scrollbar_stale {
            let editor = self.editor.clone();
            cx.defer(move |cx| editor.update(cx, |_, cx| cx.notify()));
        }

        EditorPrepaintState {
            first_line: vrange.start,
            lines: paint_lines,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layouts: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (
            line_height,
            line_number_width,
            show_line_numbers,
            focus_handle,
            selection,
            cursor,
            extra_cursors,
            font,
            font_size,
            content,
            completion_visible,
            completion_shown_at,
            completions,
            completion_selected,
            completion_scroll_offset,
            completion_replace_start,
            diagnostics,
            diag_by_line,
            diag_spans,
            hovered_diagnostic,
            hovered_sqlly_directive,
            hover_info,
            fold_preview,
            signature_help,
            fim_suggestion,
            scroll_viewport,
        ) = {
            let editor = self.editor.read(cx);
            (
                editor.line_height,
                editor.line_number_width,
                editor.editor_prefs.show_line_numbers,
                editor.focus_handle.clone(),
                editor.selection(),
                editor.cursor,
                editor.extra_cursors.clone(),
                editor.font.clone(),
                editor.font_size,
                editor.content.clone(),
                editor.completion_visible,
                editor.completion_shown_at,
                editor.completions.clone(),
                // The popup opens with no row selected — the highlight only
                // appears once the user navigates with Up/Down, matching the
                // Enter-inserts-a-newline-until-navigated key model.
                editor
                    .completion_user_navigated
                    .then_some(editor.completion_index),
                editor.completion_scroll_offset,
                editor.completion_replace_start,
                editor.diagnostics.clone(),
                editor.diag_by_line.clone(),
                editor.diag_spans.clone(),
                editor.hovered_diagnostic,
                editor.hovered_sqlly_directive,
                editor.hover_info.clone(),
                editor.fold_preview,
                editor.signature_help.clone(),
                editor
                    .fim_visible
                    .then(|| editor.fim_suggestion.clone())
                    .flatten(),
                editor.scroll_handle.bounds(),
            )
        };
        // Popups/tooltips clamp against the visible viewport (the scroll
        // container's bounds) — NOT `bounds`, which is the full scrolled
        // content and, for a short buffer, ends right below the last text
        // line. Clamping against content bounds pinned the completion popup
        // to the top of the editor instead of the caret.
        let viewport = if scroll_viewport.size.height > px(0.0) {
            scroll_viewport
        } else {
            bounds
        };
        let is_focused = focus_handle.is_focused(window);
        // The improve-hint chip re-stamps its bounds only while the popup is
        // painted; clear first so a dismissed popup can't leave a stale
        // click target behind.
        self.editor.read(cx).feedback_hint_bounds.set(None);
        self.editor.read(cx).fold_chip_bounds.borrow_mut().clear();
        let first_line = layouts.first_line;
        // One revision-keyed index for the whole paint pass — never rebuild
        // the newline table per painted line.
        let line_index = self.editor.read(cx).line_index();
        // ↻ chips for `--sqlly date:` markers, and a "Refresh dates" chip on
        // the first line of each statement that has any, by buffer line.
        #[allow(clippy::type_complexity)]
        let date_chips: std::collections::HashMap<
            usize,
            (Vec<usize>, Option<(usize, usize)>),
        > = {
            let editor = self.editor.read(cx);
            editor.date_chip_bounds.borrow_mut().clear();
            let mut map: std::collections::HashMap<usize, (Vec<usize>, Option<(usize, usize)>)> =
                std::collections::HashMap::new();
            if editor.show_date_refresh_buttons() {
                for marker in editor.date_markers().iter() {
                    let line = line_index.line_col_for_offset(marker.marker.start).0;
                    map.entry(line).or_default().0.push(marker.marker.start);
                }
                for (start, line, count) in editor.statements_with_date_markers() {
                    map.entry(line).or_default().1 = Some((start, count));
                }
            }
            map
        };
        // Fold display map for the pass: `dline` turns a buffer line into the
        // display index its content paints at (identity when nothing is
        // folded). Fold headers drive the gutter chevrons and the `⋯` chip.
        let fold_map = self.editor.read(cx).fold_map_snapshot();
        let dline = |line: usize| fold_map.buffer_to_display(line);
        let fold_headers: std::collections::HashMap<usize, bool> =
            self.editor.read(cx).with_folds(|f| {
                f.regions()
                    .iter()
                    .map(|r| (r.start_line, f.is_collapsed(r.start_line)))
                    .collect()
            });
        // Find-bar match highlights: the shared `Rc` avoids cloning up to
        // 10k ranges per frame; painted per visible line below.
        let find_matches = {
            let editor = self.editor.read(cx);
            editor
                .find_bar
                .as_ref()
                .filter(|bar| bar.visible && !bar.matches.is_empty())
                .map(|bar| (bar.matches.clone(), bar.current))
        };
        // Occurrence highlight: a quiet wash behind every other copy of the
        // selected text (VS Code style). `Rc`-shared like find matches, and
        // cached on the editor so this never rescans mid-paint.
        let occurrence_matches = self.editor.read(cx).occurrence_highlights();
        let tab_maps = self.editor.read(cx).last_tab_maps.clone();
        let inlay_tab_maps = tab_maps.clone();
        let dcol = move |rel: usize, col: usize| -> usize {
            tab_maps.get(rel).map_or(col, |m| m.to_display(col))
        };
        let (
            hovered_play_line,
            result_source,
            statement_first_lines,
            show_statement_frame,
            cursor_statement,
            last_mouse_position,
        ) = {
            let editor = self.editor.read(cx);
            (
                editor.hovered_play_line,
                editor.result_source().cloned(),
                editor
                    .statement_first_lines()
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>(),
                editor.editor_prefs.show_statement_frame,
                editor.statement_span_at_cursor(),
                editor.last_mouse_position,
            )
        };
        let gutter_hovered = last_mouse_position.is_some_and(|p| {
            let x = p.x - bounds.origin.x;
            let y = p.y - bounds.origin.y;
            x >= px(0.0) && x < line_number_width && y >= px(0.0) && y < bounds.size.height
        });

        // Paint line number background (only when the gutter is visible)
        if show_line_numbers {
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: bounds.origin,
                    size: gpui::size(line_number_width, bounds.size.height),
                },
                Theme::sidebar_bg(),
            ));
        }

        // Current-line wash in the text area (behind selection + glyphs).
        if is_focused {
            let (cursor_line, _) = line_index.line_col_for_offset(cursor);
            let y = bounds.origin.y + line_height * cursor_line as f32;
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: Point::new(bounds.origin.x + line_number_width, y),
                    size: gpui::size(bounds.size.width - line_number_width, line_height),
                },
                Theme::row_alt(),
            ));
        }

        if show_statement_frame
            && is_focused
            && let Some((start, end)) = cursor_statement
        {
            let (first, last) = super::statements::span_lines(
                &content,
                super::statements::StatementSpan { start, end },
                |o| line_index.line_col_for_offset(o).0,
            );
            if !fold_map.is_hidden(first) && !fold_map.is_hidden(last) {
                let first_d = fold_map.buffer_to_display(first);
                let last_d = fold_map.buffer_to_display(last);
                let fr = super::statements::frame_rect(
                    first_d,
                    last_d,
                    f32::from(line_height),
                    f32::from(line_number_width),
                    f32::from(bounds.size.width - line_number_width),
                );
                window.paint_quad(gpui::quad(
                    Bounds {
                        origin: Point::new(bounds.origin.x + px(fr.x), bounds.origin.y + px(fr.y)),
                        size: gpui::size(px(fr.width), px(fr.height)),
                    },
                    gpui::Corners::all(px(4.0)),
                    gpui::transparent_black(),
                    gpui::Edges::all(px(1.0)),
                    Theme::border(),
                    gpui::BorderStyle::Solid,
                ));
            }
        }

        // Paint line numbers and text (only visible lines)
        for paint_line in &layouts.lines {
            let line_idx = paint_line.absolute_line;
            let rel_idx = paint_line.display_line - first_line;
            let layout = &paint_line.shaped;
            let y = bounds.origin.y + line_height * paint_line.display_line as f32;

            // Line number + diagnostic gutter dot (only when the gutter is shown)
            if show_line_numbers {
                let line_num_str = self.editor.read(cx).line_number_label(line_idx + 1);
                let line_num_len = line_num_str.len();
                let line_num = if let Some(shaped) = self
                    .editor
                    .read(cx)
                    .shape_cache
                    .borrow()
                    .get_gutter(line_num_str.as_ref())
                {
                    shaped
                } else {
                    let shaped = window.text_system().shape_line(
                        line_num_str.clone(),
                        font_size,
                        &[TextRun {
                            len: line_num_len,
                            font: font.clone(),
                            color: Theme::text_dim(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    self.editor
                        .read(cx)
                        .shape_cache
                        .borrow_mut()
                        .insert_gutter(line_num_str.as_ref(), shaped.clone());
                    shaped
                };
                let _ = line_num.paint(
                    Point::new(
                        bounds.origin.x + px(8.0) + px(super::statements::PLAY_COLUMN_WIDTH),
                        y,
                    ),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );

                let outcome_first_line = result_source.as_ref().map(|src| {
                    super::statements::span_lines(
                        &content,
                        super::statements::StatementSpan {
                            start: src.span.0,
                            end: src.span.1,
                        },
                        |o| line_index.line_col_for_offset(o).0,
                    )
                    .0
                });
                let show_outcome_glyph = outcome_first_line == Some(line_idx);
                if statement_first_lines.contains(&line_idx)
                    && (gutter_hovered || show_outcome_glyph)
                {
                    let play = "\u{25B6}";
                    let color = if show_outcome_glyph {
                        match result_source.as_ref().map(|s| s.outcome) {
                            Some(super::RunOutcome::Succeeded) => Theme::success(),
                            Some(super::RunOutcome::Failed) => Theme::error(),
                            None => Theme::text_dim(),
                        }
                    } else if hovered_play_line == Some(paint_line.display_line) {
                        Theme::accent()
                    } else {
                        Theme::text_dim()
                    };
                    let glyph = window.text_system().shape_line(
                        SharedString::from(play),
                        font_size * 0.7,
                        &[TextRun {
                            len: play.len(),
                            font: font.clone(),
                            color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let _ = glyph.paint(
                        Point::new(bounds.origin.x + px(3.0), y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }

                // Paint diagnostic gutter dot for this line
                if let Some(top_diag) = diag_by_line
                    .get(&line_idx)
                    .and_then(|ids| ids.first())
                    .and_then(|&i| diagnostics.get(i))
                {
                    let dot_color = match top_diag.severity {
                        DiagnosticSeverity::Error => Theme::error(),
                        DiagnosticSeverity::Warning => Theme::warning(),
                        DiagnosticSeverity::Info => Theme::info(),
                        DiagnosticSeverity::RequireConfirmation => Theme::warning(),
                    };
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(
                                bounds.origin.x + line_number_width - px(12.0),
                                y + px(6.0),
                            ),
                            size: gpui::size(px(6.0), px(6.0)),
                        },
                        dot_color,
                    ));
                }

                // Fold chevron: ▸ when collapsed, ▾ when expandable. Painted
                // over the diagnostic-dot slot at the gutter's right edge —
                // the fold affordance is interactive, so it wins the pixels.
                if let Some(collapsed) = fold_headers.get(&line_idx) {
                    let glyph = if *collapsed { "\u{25B8}" } else { "\u{25BE}" };
                    let chevron = window.text_system().shape_line(
                        SharedString::from(glyph),
                        font_size * 0.8,
                        &[TextRun {
                            len: glyph.len(),
                            font: font.clone(),
                            color: if *collapsed {
                                Theme::accent()
                            } else {
                                Theme::text_dim()
                            },
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let _ = chevron.paint(
                        Point::new(bounds.origin.x + line_number_width - px(13.0), y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }

            // Paint find-match highlights for this line (under the selection
            // and text). Matches are sorted, so a binary search finds the
            // first candidate and the walk stops at the line's end — the
            // pass stays O(visible matches), not O(total matches), per line.
            if let Some((matches, current)) = &find_matches {
                let line_start = line_index.line_start(line_idx);
                let line_end = line_index.line_end(&content, line_idx);
                let mut i = matches.partition_point(|m| m.end <= line_start);
                while let Some(m) = matches.get(i) {
                    if m.start >= line_end {
                        break;
                    }
                    let col_start = m.start.max(line_start) - line_start;
                    let col_end = m.end.min(line_end) - line_start;
                    let x_start = layout.x_for_index(dcol(rel_idx, col_start));
                    let x_end = layout.x_for_index(dcol(rel_idx, col_end));
                    let color = if *current == Some(i) {
                        Theme::accent().opacity(0.45)
                    } else {
                        Theme::accent().opacity(0.18)
                    };
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(bounds.origin.x + line_number_width + x_start, y),
                            size: gpui::size(x_end - x_start, line_height),
                        },
                        color,
                    ));
                    i += 1;
                }
            }

            // Paint occurrence highlights for this line, with the same
            // binary-search walk as find matches (under the selection and
            // text, and quieter than a find match so the two never read
            // alike).
            if let Some(occurrences) = &occurrence_matches {
                let line_start = line_index.line_start(line_idx);
                let line_end = line_index.line_end(&content, line_idx);
                let mut i = occurrences.partition_point(|m| m.end <= line_start);
                while let Some(m) = occurrences.get(i) {
                    if m.start >= line_end {
                        break;
                    }
                    let col_start = m.start.max(line_start) - line_start;
                    let col_end = m.end.min(line_end) - line_start;
                    let x_start = layout.x_for_index(dcol(rel_idx, col_start));
                    let x_end = layout.x_for_index(dcol(rel_idx, col_end));
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(bounds.origin.x + line_number_width + x_start, y),
                            size: gpui::size(x_end - x_start, line_height),
                        },
                        Theme::text_dim().opacity(0.14),
                    ));
                    i += 1;
                }
            }

            // Paint selection for this line
            if let Some(sel) = &selection {
                let line_start = line_index.line_start(line_idx);
                let line_end = line_index.line_end(&content, line_idx);
                if sel.start < line_end && sel.end > line_start {
                    let sel_start = sel.start.max(line_start) - line_start;
                    let sel_end = sel.end.min(line_end) - line_start;
                    let x_start = layout.x_for_index(dcol(rel_idx, sel_start));
                    let x_end = layout.x_for_index(dcol(rel_idx, sel_end));
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(bounds.origin.x + line_number_width + x_start, y),
                            size: gpui::size(x_end - x_start, line_height),
                        },
                        Theme::selection(),
                    ));
                }
            }

            // Paint extra-cursor selections for this line (multi-cursor
            // mode), with the same selection color as the primary.
            if !extra_cursors.is_empty() {
                let line_start = line_index.line_start(line_idx);
                let line_end = line_index.line_end(&content, line_idx);
                for extra in &extra_cursors {
                    let Some(sel) = extra.selection() else {
                        continue;
                    };
                    if sel.start < line_end && sel.end > line_start {
                        let sel_start = sel.start.max(line_start) - line_start;
                        let sel_end = sel.end.min(line_end) - line_start;
                        let x_start = layout.x_for_index(dcol(rel_idx, sel_start));
                        let x_end = layout.x_for_index(dcol(rel_idx, sel_end));
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: Point::new(
                                    bounds.origin.x + line_number_width + x_start,
                                    y,
                                ),
                                size: gpui::size(x_end - x_start, line_height),
                            },
                            Theme::selection(),
                        ));
                    }
                }
            }

            // Ghost-label chips: a faint rounded wash behind each INSERT
            // value label so the decoration reads as clearly not part of the
            // query text. Spans are display-space byte ranges (no `dcol`
            // mapping) and include the trailing separator space; the chip
            // stops one byte short so it hugs the label itself.
            if let Some(map) = inlay_tab_maps.get(rel_idx) {
                for &(span_start, span_end) in map.inlay_spans() {
                    let x_start = layout.x_for_index(span_start);
                    let x_end = layout.x_for_index(span_end.saturating_sub(1));
                    if x_end <= x_start {
                        continue;
                    }
                    let pad = px(2.0);
                    let x0 = (bounds.origin.x + line_number_width + x_start - pad)
                        .max(bounds.origin.x + line_number_width);
                    let x1 = bounds.origin.x + line_number_width + x_end + pad;
                    window.paint_quad(gpui::quad(
                        Bounds {
                            origin: Point::new(x0, y + px(2.0)),
                            size: gpui::size(x1 - x0, line_height - px(4.0)),
                        },
                        gpui::Corners::all(px(3.0)),
                        Theme::text_dim().opacity(0.1),
                        gpui::Edges::all(px(0.0)),
                        gpui::transparent_black(),
                        gpui::BorderStyle::Solid,
                    ));
                }
            }

            // Paint text line
            let _ = layout.paint(
                Point::new(bounds.origin.x + line_number_width, y),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );

            // Collapsed fold: a dim `⋯` chip after the header text marks the
            // hidden lines (click the chevron or press the unfold shortcut
            // to reopen).
            if fold_headers.get(&line_idx) == Some(&true) {
                let chip_text = SharedString::from("\u{22EF}");
                let chip_len = chip_text.len();
                let shaped_chip = window.text_system().shape_line(
                    chip_text,
                    font_size,
                    &[TextRun {
                        len: chip_len,
                        font: font.clone(),
                        color: Theme::text_dim(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let chip_x = bounds.origin.x + line_number_width + layout.width + px(8.0);
                let quad_bounds = Bounds {
                    origin: Point::new(chip_x - px(3.0), y + px(2.0)),
                    size: gpui::size(shaped_chip.width + px(6.0), line_height - px(4.0)),
                };
                window.paint_quad(gpui::fill(quad_bounds, Theme::text_dim().opacity(0.12)));
                self.editor
                    .read(cx)
                    .fold_chip_bounds
                    .borrow_mut()
                    .push((line_idx, quad_bounds));
                let _ = shaped_chip.paint(
                    Point::new(chip_x, y),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }

            if let Some((markers, statement)) = date_chips.get(&line_idx) {
                let mut chips: Vec<(String, super::date_helper::DateChip)> = markers
                    .iter()
                    .map(|m| {
                        (
                            "\u{21BB}".to_string(),
                            super::date_helper::DateChip::One(*m),
                        )
                    })
                    .collect();
                if let Some((start, count)) = statement {
                    let label = if *count == 1 {
                        crate::localization::text("editor.element.refresh_date")
                    } else {
                        crate::localization::format(
                            "editor.element.refresh_dates",
                            &[("count", &count.to_string())],
                        )
                    };
                    chips.push((label, super::date_helper::DateChip::Statement(*start)));
                }
                let mut chip_x = bounds.origin.x + line_number_width + layout.width + px(10.0);
                let chip_font_size = font_size * 0.9;
                for (label, chip) in chips {
                    let len = label.len();
                    let shaped_chip = window.text_system().shape_line(
                        SharedString::from(label),
                        chip_font_size,
                        &[TextRun {
                            len,
                            font: font.clone(),
                            color: Theme::accent(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let chip_bounds = Bounds {
                        origin: Point::new(chip_x - px(4.0), y + px(2.0)),
                        size: gpui::size(shaped_chip.width + px(8.0), line_height - px(4.0)),
                    };
                    window.paint_quad(gpui::quad(
                        chip_bounds,
                        gpui::Corners::all(px(3.0)),
                        Theme::accent().opacity(0.12),
                        gpui::Edges::all(px(0.0)),
                        gpui::transparent_black(),
                        gpui::BorderStyle::Solid,
                    ));
                    self.editor
                        .read(cx)
                        .date_chip_bounds
                        .borrow_mut()
                        .push((chip_bounds, chip));
                    let _ = shaped_chip.paint(
                        Point::new(chip_x, y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    chip_x += shaped_chip.width + px(14.0);
                }
            }

            // Paint squiggly underlines for diagnostics on this line
            let line_start = line_index.line_start(line_idx);
            let line_end = line_index.line_end(&content, line_idx);
            for (i, &(span_start, span_end)) in diag_spans.iter().enumerate() {
                if line_idx < span_start || line_idx > span_end {
                    continue;
                }
                let Some(diag) = diagnostics.get(i) else {
                    continue;
                };
                if diag.start_byte >= diag.end_byte {
                    continue;
                }
                if diag.start_byte >= line_end || diag.end_byte <= line_start {
                    continue;
                }
                let col_start = diag.start_byte.max(line_start) - line_start;
                let col_end = diag.end_byte.min(line_end) - line_start;
                let x_start = layout.x_for_index(dcol(rel_idx, col_start));
                let x_end = layout.x_for_index(dcol(rel_idx, col_end));
                let squiggle_color = match diag.severity {
                    DiagnosticSeverity::Error => Theme::error(),
                    DiagnosticSeverity::Warning => Theme::warning(),
                    DiagnosticSeverity::Info => Theme::info(),
                    DiagnosticSeverity::RequireConfirmation => Theme::warning(),
                };
                let base_y = y + line_height - px(2.0);
                let mut sx = x_start;
                let mut toggle = false;
                while sx < x_end {
                    let seg_w = px(2.0).min(x_end - sx);
                    let y_off = if toggle { px(0.0) } else { px(2.0) };
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(
                                bounds.origin.x + line_number_width + sx,
                                base_y + y_off,
                            ),
                            size: gpui::size(seg_w, px(1.0)),
                        },
                        squiggle_color,
                    ));
                    sx += px(3.0);
                    toggle = !toggle;
                }
            }
        }

        // Paint cursor
        if is_focused {
            let (cursor_line, cursor_col) = line_index.line_col_for_offset(cursor);
            let rel_cursor = dline(cursor_line).saturating_sub(first_line);
            if let Some(pl) = layouts.lines.get(rel_cursor) {
                let layout = &pl.shaped;
                let x = layout.x_for_index(dcol(rel_cursor, cursor_col));
                let y = bounds.origin.y + line_height * dline(cursor_line) as f32;
                window.paint_quad(gpui::fill(
                    Bounds {
                        origin: Point::new(bounds.origin.x + line_number_width + x, y),
                        size: gpui::size(px(1.5), line_height),
                    },
                    Theme::cursor(),
                ));

                // Paint FIM ghost-text suggestion at the cursor. The suggestion
                // is rendered inline after the cursor in a dim color, mirroring
                // the Swift `Editor.showFIMSuggestion` ghost text.
                if let Some(fim) = &fim_suggestion {
                    // The suggestion may span multiple lines; paint the first
                    // line inline after the cursor and subsequent lines below.
                    let fim_lines: Vec<&str> = fim.split('\n').collect();
                    if let Some(first_line) = fim_lines.first() {
                        let ghost = window.text_system().shape_line(
                            SharedString::from(first_line.to_string()),
                            font_size,
                            &[TextRun {
                                len: first_line.chars().count(),
                                font: font.clone(),
                                color: Theme::text_dim(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let _ = ghost.paint(
                            Point::new(bounds.origin.x + line_number_width + x, y),
                            line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                    // Subsequent lines painted at the start of the text area.
                    for (i, line) in fim_lines.iter().skip(1).enumerate() {
                        let ghost = window.text_system().shape_line(
                            SharedString::from(line.to_string()),
                            font_size,
                            &[TextRun {
                                len: line.chars().count(),
                                font: font.clone(),
                                color: Theme::text_dim(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let y2 =
                            bounds.origin.y + line_height * (dline(cursor_line) + 1 + i) as f32;
                        let _ = ghost.paint(
                            Point::new(bounds.origin.x + line_number_width, y2),
                            line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                }
            }

            // Paint extra carets (multi-cursor mode): the same caret visual
            // as the primary, painted steady (no blink synchronization).
            for extra in &extra_cursors {
                let (eline, ecol) = line_index.line_col_for_offset(extra.cursor);
                let edisplay = dline(eline);
                let erel = edisplay.saturating_sub(first_line);
                let Some(pl) = layouts
                    .lines
                    .get(erel)
                    .filter(|pl| pl.display_line == edisplay)
                else {
                    continue; // off-screen
                };
                let ex = pl.shaped.x_for_index(dcol(erel, ecol));
                let ey = bounds.origin.y + line_height * edisplay as f32;
                window.paint_quad(gpui::fill(
                    Bounds {
                        origin: Point::new(bounds.origin.x + line_number_width + ex, ey),
                        size: gpui::size(px(1.5), line_height),
                    },
                    Theme::cursor(),
                ));
            }
        }

        // Signature help sits directly under the line being typed, and the
        // completion popup (when open) stacks below it, so neither covers the
        // code being typed nor the other. Laid out here, before the popup,
        // so the popup knows where to go; painted last, on top.
        // The natural-date helper's card takes the signature card's place
        // (opening the helper drops signature help, so they never compete).
        let date_card_lines = self.editor.read(cx).date_helper_card();
        let sig_card: Option<(Vec<ShapedLine>, Bounds<Pixels>, bool)> =
            if let Some(lines) = date_card_lines.as_ref() {
                let (cursor_line, cursor_col) =
                    line_index.line_col_for_offset(cursor.min(content.len()));
                let rel_cursor = dline(cursor_line).saturating_sub(first_line);
                let cursor_x = layouts
                    .lines
                    .get(rel_cursor)
                    .map(|pl| pl.shaped.x_for_index(dcol(rel_cursor, cursor_col)))
                    .unwrap_or(px(0.0));
                let cursor_y = bounds.origin.y + line_height * dline(cursor_line) as f32;
                let card_font = font_size * 0.92;
                let mut bold_font = font.clone();
                bold_font.weight = gpui::FontWeight::BOLD;
                let shaped: Vec<ShapedLine> = lines
                    .iter()
                    .map(|line| shape_card_line(window, line, &font, &bold_font, card_font))
                    .collect();
                let pad = px(8.0);
                let mut popup_w = px(0.0);
                for line in &shaped {
                    popup_w = popup_w.max(line.width);
                }
                popup_w += pad * 2.0;
                let popup_h = line_height * shaped.len() as f32 + px(8.0);
                let line_bottom = cursor_y + line_height;
                let room_below = viewport.origin.y + viewport.size.height - line_bottom;
                let below = popup_h <= room_below || room_below >= cursor_y - viewport.origin.y;
                let popup_x = (bounds.origin.x + line_number_width + cursor_x)
                    .min(viewport.origin.x + viewport.size.width - popup_w)
                    .max(viewport.origin.x);
                let popup_y = if below {
                    line_bottom
                } else {
                    (cursor_y - popup_h).max(viewport.origin.y)
                };
                Some((
                    shaped,
                    Bounds {
                        origin: Point::new(popup_x, popup_y),
                        size: gpui::size(popup_w, popup_h),
                    },
                    below,
                ))
            } else if let Some(help) = signature_help.as_ref() {
                let (cursor_line, cursor_col) =
                    line_index.line_col_for_offset(cursor.min(content.len()));
                let rel_cursor = dline(cursor_line).saturating_sub(first_line);
                let cursor_x = layouts
                    .lines
                    .get(rel_cursor)
                    .map(|pl| pl.shaped.x_for_index(dcol(rel_cursor, cursor_col)))
                    .unwrap_or(px(0.0));
                let cursor_y = bounds.origin.y + line_height * dline(cursor_line) as f32;
                let card_font = font_size * 0.92;
                let mut bold_font = font.clone();
                bold_font.weight = gpui::FontWeight::BOLD;
                let dim = Theme::text().opacity(0.7);
                let mut runs = signature_runs(help, &font, &bold_font, dim);
                let mut label_text = help.label.clone();
                if let Some(note) = super::info_cards::overload_note(help) {
                    runs.push(TextRun {
                        len: note.len(),
                        font: font.clone(),
                        color: dim,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                    label_text.push_str(&note);
                }
                let mut shaped = vec![window.text_system().shape_line(
                    SharedString::from(label_text),
                    card_font,
                    &runs,
                    None,
                )];
                let pad = px(8.0);
                let columns = super::info_cards::wrap_columns(
                    f32::from(viewport.size.width - pad * 2.0),
                    card_char_width(window, &font, card_font),
                );
                let detail = super::info_cards::signature_detail(help, columns);
                // Under the line when the whole card fits there (or there is
                // more room below than above), else above it; trimmed to fit.
                let line_bottom = cursor_y + line_height;
                let room_below = viewport.origin.y + viewport.size.height - line_bottom;
                let room_above = cursor_y - viewport.origin.y;
                let needed = line_height * (1 + detail.len()) as f32 + px(8.0);
                let below = needed <= room_below || room_below >= room_above;
                let room = if below { room_below } else { room_above };
                let max_detail = ((f32::from(room - px(8.0)) / f32::from(line_height)).floor()
                    as usize)
                    .saturating_sub(1);
                if max_detail > 0 {
                    for line in super::info_cards::fit_lines(detail, max_detail) {
                        shaped.push(shape_card_line(window, &line, &font, &bold_font, card_font));
                    }
                }
                let mut popup_w = px(0.0);
                for line in &shaped {
                    popup_w = popup_w.max(line.width);
                }
                popup_w += pad * 2.0;
                let popup_h = line_height * shaped.len() as f32 + px(8.0);
                let popup_x = (bounds.origin.x + line_number_width + cursor_x)
                    .min(viewport.origin.x + viewport.size.width - popup_w)
                    .max(viewport.origin.x);
                let popup_y = if below {
                    line_bottom
                } else {
                    (cursor_y - popup_h).max(viewport.origin.y)
                };
                Some((
                    shaped,
                    Bounds {
                        origin: Point::new(popup_x, popup_y),
                        size: gpui::size(popup_w, popup_h),
                    },
                    below,
                ))
            } else {
                None
            };
        // How far the completion popup must step past the signature card:
        // down when the card is under the line, up when it is above.
        let (sig_below, sig_above) = match &sig_card {
            Some((_, card, true)) => (card.size.height, px(0.0)),
            Some((_, card, false)) => (px(0.0), card.size.height),
            None => (px(0.0), px(0.0)),
        };

        // Paint completion popup
        if completion_visible && !completions.is_empty() {
            // Fade the popup in over ~120ms (ease-out cubic), matching the
            // app's opacity-only motion language. Hand-rolled because this
            // popup is custom-painted: the alpha multiplies into every color
            // below, and we ask for another frame until it lands at 1.0. The
            // extra frames last 120ms per popup appearance — the steady-state
            // hot path (typing with the popup up) paints exactly one frame
            // per keystroke, same as before.
            let popup_alpha = if cx.reduce_motion() {
                1.0
            } else {
                completion_shown_at
                    .map(|shown| (shown.elapsed().as_secs_f32() / 0.12).clamp(0.0, 1.0))
                    .unwrap_or(1.0)
            };
            let popup_alpha = 1.0 - (1.0 - popup_alpha).powi(3);
            if popup_alpha < 1.0 {
                window.request_animation_frame();
            }

            let (cursor_line, cursor_col) = line_index.line_col_for_offset(cursor);
            let rel_cursor = dline(cursor_line).saturating_sub(first_line);
            let cursor_x = layouts
                .lines
                .get(rel_cursor)
                .map(|pl| pl.shaped.x_for_index(dcol(rel_cursor, cursor_col)))
                .unwrap_or(px(0.0));
            let cursor_y = bounds.origin.y + line_height * dline(cursor_line) as f32;

            let popup_x = bounds.origin.x + line_number_width + cursor_x;
            let popup_y = cursor_y + line_height + sig_below;

            let max_visible = COMPLETION_MAX_VISIBLE;
            let visible_count = completions
                .len()
                .saturating_sub(completion_scroll_offset)
                .min(max_visible);

            // Compute the filter text the user typed (between the replacement
            // start and the cursor) so we can highlight matching portions of
            // each completion label.
            let filter_text: &str = if completion_replace_start <= cursor
                && completion_replace_start <= content.len()
                && cursor <= content.len()
            {
                &content[completion_replace_start..cursor]
            } else {
                ""
            };

            // Shape all visible labels + details upfront so we can measure the
            // widest one and reuse the shaped lines for painting (no double-shaping).
            // Each label is shaped with multiple TextRuns: matched characters
            // (case-insensitive subsequence match against filter_text) are
            // bold dark ink on a neon highlighter fill, unmatched characters
            // are dimmed.
            let shaped_labels: Vec<_> = completions
                .iter()
                .skip(completion_scroll_offset)
                .take(max_visible)
                .enumerate()
                .map(|(i, item)| {
                    let label = SharedString::from(item.label.clone());
                    let label_len = label.len();
                    // Keywords are plain text: the highlighter fill is for
                    // names (tables, columns, functions), where it shows which
                    // letters matched; on a keyword it is just noise.
                    let match_ranges = if item.kind == CompletionKind::Keyword {
                        Vec::new()
                    } else {
                        completion_match_ranges(filter_text, &item.label)
                    };

                    let is_selected = completion_selected == Some(i + completion_scroll_offset);
                    let matched_color = Theme::text().opacity(popup_alpha);
                    let unmatched_color = Theme::text_dim().opacity(popup_alpha);
                    // A column the query's GROUP BY still needs is drawn bold
                    // (its detail below is tinted too) so it reads as required.
                    let font = if item.detail == sqlly_intel::intellisense::REQUIRED_GROUP_BY_DETAIL
                    {
                        let mut bold = font.clone();
                        bold.weight = gpui::FontWeight::BOLD;
                        bold
                    } else {
                        font.clone()
                    };

                    // Matched characters read like a highlighter pen: neon
                    // fill, dark ink, bold.
                    let mut match_font = font.clone();
                    match_font.weight = gpui::FontWeight::BOLD;
                    let match_fill = Theme::match_highlight().opacity(popup_alpha);
                    let match_ink = Theme::match_highlight_text().opacity(popup_alpha);

                    let mut runs: Vec<TextRun> = Vec::new();
                    let mut prev_end = 0usize;
                    for (start, end) in &match_ranges {
                        if *start > prev_end {
                            runs.push(TextRun {
                                len: *start - prev_end,
                                font: font.clone(),
                                color: unmatched_color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            });
                        }
                        runs.push(TextRun {
                            len: *end - *start,
                            font: match_font.clone(),
                            color: match_ink,
                            background_color: Some(match_fill),
                            underline: None,
                            strikethrough: None,
                        });
                        prev_end = *end;
                    }
                    if prev_end < label_len {
                        runs.push(TextRun {
                            len: label_len - prev_end,
                            font: font.clone(),
                            color: unmatched_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        });
                    }
                    if runs.is_empty() {
                        runs.push(TextRun {
                            len: label_len,
                            font: font.clone(),
                            color: if is_selected {
                                matched_color
                            } else {
                                unmatched_color
                            },
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        });
                    }

                    window
                        .text_system()
                        .shape_line(label, font_size, &runs, None)
                })
                .collect();

            let shaped_details: Vec<_> = completions
                .iter()
                .skip(completion_scroll_offset)
                .take(max_visible)
                .map(|item| {
                    let detail_text = if item.kind == CompletionKind::Snippet {
                        let raw = if item.detail.is_empty() {
                            item.insert_text.as_str()
                        } else {
                            item.detail.as_str()
                        };
                        crate::snippets::strip_tab_stops(raw)
                    } else if item.detail.is_empty() {
                        String::new()
                    } else {
                        item.detail.clone()
                    };
                    if detail_text.is_empty() {
                        None
                    } else {
                        let required =
                            item.detail == sqlly_intel::intellisense::REQUIRED_GROUP_BY_DETAIL;
                        let detail = SharedString::from(if required {
                            format!("● {detail_text}")
                        } else {
                            detail_text
                        });
                        let detail_len = detail.len();
                        Some(window.text_system().shape_line(
                            detail,
                            font_size,
                            &[TextRun {
                                len: detail_len,
                                font: font.clone(),
                                color: if required {
                                    Theme::warning().opacity(popup_alpha)
                                } else {
                                    Theme::text_dim().opacity(popup_alpha)
                                },
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        ))
                    }
                })
                .collect();

            let max_label_width = shaped_labels
                .iter()
                .map(|l| l.width)
                .max()
                .unwrap_or(px(0.0));
            let max_detail_width = shaped_details
                .iter()
                .filter_map(|d| d.as_ref().map(|d| d.width))
                .max()
                .unwrap_or(px(0.0));

            // 20px left padding (kind dot + spacing) + label + 16px gap + detail + 10px right padding
            let popup_width = if max_detail_width > px(0.0) {
                (max_label_width + max_detail_width + px(46.0)).max(px(350.0))
            } else {
                (max_label_width + px(30.0)).max(px(350.0))
            };
            let popup_height = line_height * visible_count as f32;

            // Clamp to the visible viewport so the popup isn't cut off at
            // the right/bottom edge (same treatment as the diagnostic
            // tooltip below). If there is no room below the cursor line,
            // flip the popup above it.
            let popup_x = popup_x
                .min(viewport.origin.x + viewport.size.width - popup_width)
                .max(viewport.origin.x);
            let popup_y = if popup_y + popup_height > viewport.origin.y + viewport.size.height {
                (cursor_y - sig_above - popup_height).max(viewport.origin.y)
            } else {
                popup_y
            };

            // Stamp the popup rect + row height so mouse-down can map a click to
            // a row and the scroll-wheel handler knows when the pointer is over
            // the popup.
            self.editor
                .read(cx)
                .completion_popup_bounds
                .set(Some(Bounds {
                    origin: Point::new(popup_x, popup_y),
                    size: gpui::size(popup_width, popup_height),
                }));
            self.editor
                .read(cx)
                .completion_row_height
                .set(f32::from(line_height));

            // Background
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: Point::new(popup_x, popup_y),
                    size: gpui::size(popup_width, popup_height),
                },
                Theme::popup_bg().opacity(popup_alpha),
            ));

            // Border (use quad with border_widths)
            window.paint_quad(gpui::quad(
                Bounds {
                    origin: Point::new(popup_x, popup_y),
                    size: gpui::size(popup_width, popup_height),
                },
                gpui::Corners::default(),
                gpui::transparent_black(),
                gpui::Edges::all(px(1.0)),
                Theme::popup_border().opacity(popup_alpha),
                gpui::BorderStyle::Solid,
            ));

            // Items
            for (i, item) in completions
                .iter()
                .skip(completion_scroll_offset)
                .take(max_visible)
                .enumerate()
            {
                let item_y = popup_y + line_height * i as f32;

                // Highlight selected
                if completion_selected == Some(i + completion_scroll_offset) {
                    window.paint_quad(gpui::fill(
                        Bounds {
                            origin: Point::new(popup_x, item_y),
                            size: gpui::size(popup_width, line_height),
                        },
                        Theme::completion_selected().opacity(popup_alpha),
                    ));
                }

                // Kind indicator color
                let kind_color = match item.kind {
                    CompletionKind::Keyword => Theme::kind_keyword(),
                    CompletionKind::Table => Theme::kind_table(),
                    CompletionKind::Column => Theme::kind_column(),
                    CompletionKind::Function => Theme::kind_function(),
                    CompletionKind::Procedure => Theme::kind_procedure(),
                    CompletionKind::Database => Theme::kind_database(),
                    CompletionKind::Schema => Theme::kind_schema(),
                    CompletionKind::Snippet => Theme::kind_snippet(),
                    CompletionKind::Value => Theme::kind_default(),
                    CompletionKind::Join => Theme::kind_table(),
                    _ => Theme::kind_default(),
                };

                // Kind dot
                window.paint_quad(gpui::fill(
                    Bounds {
                        origin: Point::new(popup_x + px(6.0), item_y + px(6.0)),
                        size: gpui::size(px(8.0), px(8.0)),
                    },
                    kind_color.opacity(popup_alpha),
                ));

                // Label (reuse pre-shaped line): the highlighter fill behind
                // matched characters first, then the text over it.
                let _ = shaped_labels[i].paint_background(
                    Point::new(popup_x + px(20.0), item_y),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                let _ = shaped_labels[i].paint(
                    Point::new(popup_x + px(20.0), item_y),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );

                // Detail (reuse pre-shaped line, painted dimmed after label)
                if let Some(detail) = &shaped_details[i] {
                    let _ = detail.paint(
                        Point::new(popup_x + px(20.0) + max_label_width + px(16.0), item_y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }

            // Out-of-band hint attached outside the popup (below it, or above
            // when the popup flipped over the caret): F2 — or a click on the
            // chip itself — opens the "improve IntelliSense" feedback dialog
            // for this very popup.
            self.paint_improve_hint(
                popup_x,
                popup_y,
                popup_width,
                popup_height,
                cursor_y,
                &font,
                font_size,
                line_height,
                popup_alpha,
                window,
                cx,
            );
        } else if completion_visible {
            // The popup is open but there are no suggestions (a manual trigger,
            // or the user typed themselves out of every match). Show a
            // "No suggestions" placeholder so the popup is always available.
            let (cursor_line, cursor_col) = line_index.line_col_for_offset(cursor);
            let rel_cursor = dline(cursor_line).saturating_sub(first_line);
            let cursor_x = layouts
                .lines
                .get(rel_cursor)
                .map(|pl| pl.shaped.x_for_index(dcol(rel_cursor, cursor_col)))
                .unwrap_or(px(0.0));
            let cursor_y = bounds.origin.y + line_height * dline(cursor_line) as f32;

            let popup_width = px(350.0);
            let popup_height = line_height;
            let popup_x = (bounds.origin.x + line_number_width + cursor_x)
                .min(viewport.origin.x + viewport.size.width - popup_width)
                .max(viewport.origin.x);
            let popup_y_below = cursor_y + line_height + sig_below;
            let popup_y = if popup_y_below + popup_height > viewport.origin.y + viewport.size.height
            {
                (cursor_y - sig_above - popup_height).max(viewport.origin.y)
            } else {
                popup_y_below
            };
            let rect = Bounds {
                origin: Point::new(popup_x, popup_y),
                size: gpui::size(popup_width, popup_height),
            };

            window.paint_quad(gpui::fill(rect, Theme::popup_bg()));
            window.paint_quad(gpui::quad(
                rect,
                gpui::Corners::default(),
                gpui::transparent_black(),
                gpui::Edges::all(px(1.0)),
                Theme::popup_border(),
                gpui::BorderStyle::Solid,
            ));
            // Name F2 in the text itself, not only in the chip below — the
            // empty state is exactly when the user wants to report what was
            // missing, and the two elements read as one message this way.
            let text =
                SharedString::from(crate::localization::text("editor.element.no_suggestions"));
            let text_len = text.len();
            let shaped = window.text_system().shape_line(
                text,
                font_size,
                &[TextRun {
                    len: text_len,
                    font: font.clone(),
                    color: Theme::text_dim(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            let _ = shaped.paint(
                Point::new(popup_x + px(10.0), popup_y),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
            // Scroll over it is harmless; there are no rows to click.
            self.editor.read(cx).completion_popup_bounds.set(Some(rect));
            self.editor
                .read(cx)
                .completion_row_height
                .set(f32::from(line_height));
            // Even with nothing to suggest, offer the feedback affordance —
            // this is precisely when the user most wants to tell us what was
            // missing. F2 and a click on the chip both open the dialog.
            self.paint_improve_hint(
                popup_x,
                popup_y,
                popup_width,
                popup_height,
                cursor_y,
                &font,
                font_size,
                line_height,
                1.0,
                window,
                cx,
            );
        }
        if !completion_visible {
            // The popup isn't painted — drop any stale rect so a click/scroll
            // elsewhere never hits it.
            self.editor.read(cx).completion_popup_bounds.set(None);
        }

        // Paint diagnostic hover tooltip: the message, then (when the
        // diagnostic has them) a wrapped explainer and a "Fix this" link. The
        // popup and link bounds are stamped so the mouse handlers can keep the
        // popup up while the pointer is on it and apply the fix on a click;
        // both are cleared whenever no popup is painted.
        let mut diagnostic_popup_bounds = None;
        let mut diagnostic_fix_bounds = None;
        let mut diagnostic_directive_link_bounds = None;
        if let Some(diag_idx) = hovered_diagnostic
            && let Some(diag) = diagnostics.get(diag_idx)
        {
            let (diag_line, diag_col) = line_index.line_col_for_offset(diag.start_byte);
            let rel_diag = dline(diag_line).saturating_sub(first_line);
            if let Some(pl) = layouts.lines.get(rel_diag) {
                let layout = &pl.shaped;
                let x_start = layout.x_for_index(dcol(rel_diag, diag_col));
                let tooltip_y = bounds.origin.y + line_height * (dline(diag_line) + 1) as f32;
                let tooltip_x = bounds.origin.x + line_number_width + x_start;

                let shape = |text: String, color: Hsla, underline: bool| {
                    let text = SharedString::from(text);
                    let len = text.len();
                    window.text_system().shape_line(
                        text,
                        font_size,
                        &[TextRun {
                            len,
                            font: font.clone(),
                            color,
                            background_color: None,
                            underline: underline.then(|| gpui::UnderlineStyle {
                                thickness: px(1.0),
                                color: Some(color),
                                wavy: false,
                            }),
                            strikethrough: None,
                        }],
                        None,
                    )
                };
                // A problem with a `-- sqlly …` comment links to the
                // formatting-directives reference.
                let on_directive =
                    crate::sqlly_directive::is_directive_comment_at(&content, diag.start_byte);
                // A long message wraps only when there is more to say.
                let wrap_width = if diag.explainer.is_some() || diag.fix.is_some() || on_directive {
                    72
                } else {
                    usize::MAX
                };
                let mut lines: Vec<_> = group_by_lint::wrap_words(&diag.message, wrap_width)
                    .into_iter()
                    .map(|line| shape(line, Theme::text(), false))
                    .collect();
                if let Some(explainer) = &diag.explainer {
                    lines.extend(
                        group_by_lint::wrap_words(explainer, 72)
                            .into_iter()
                            .map(|line| shape(line, Theme::text_dim(), false)),
                    );
                }
                let fix_line = diag
                    .fix
                    .as_ref()
                    .map(|fix| shape(fix.label().to_string(), Theme::accent(), true));
                let directive_link = on_directive.then(|| {
                    shape(
                        crate::localization::text("editor.element.see_formatting_directives"),
                        Theme::accent(),
                        true,
                    )
                });
                let link_count =
                    usize::from(fix_line.is_some()) + usize::from(directive_link.is_some());

                let content_width = lines
                    .iter()
                    .chain(fix_line.iter())
                    .chain(directive_link.iter())
                    .map(|l| l.width)
                    .max()
                    .unwrap_or(px(0.0));
                let tooltip_width = content_width + px(16.0);
                let row_count = lines.len() + link_count;
                let gap = if link_count > 0 { px(6.0) } else { px(0.0) };
                let tooltip_height = line_height * row_count as f32 + px(4.0) + gap;

                let tooltip_x = tooltip_x
                    .min(viewport.origin.x + viewport.size.width - tooltip_width)
                    .max(viewport.origin.x);
                let tooltip_y =
                    tooltip_y.min(viewport.origin.y + viewport.size.height - tooltip_height);

                let accent_color = match diag.severity {
                    DiagnosticSeverity::Error => Theme::error(),
                    DiagnosticSeverity::Warning => Theme::warning(),
                    DiagnosticSeverity::Info => Theme::info(),
                    DiagnosticSeverity::RequireConfirmation => Theme::warning(),
                };

                let popup = Bounds {
                    origin: Point::new(tooltip_x, tooltip_y),
                    size: gpui::size(tooltip_width, tooltip_height),
                };
                window.paint_quad(gpui::fill(popup, Theme::popup_bg()));
                window.paint_quad(gpui::quad(
                    popup,
                    gpui::Corners::default(),
                    gpui::transparent_black(),
                    gpui::Edges::all(px(1.0)),
                    accent_color,
                    gpui::BorderStyle::Solid,
                ));
                let mut y = tooltip_y + px(2.0);
                for line in &lines {
                    let _ = line.paint(
                        Point::new(tooltip_x + px(8.0), y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    y += line_height;
                }
                if let Some(fix_line) = &fix_line {
                    y += gap;
                    let origin = Point::new(tooltip_x + px(8.0), y);
                    let _ = fix_line.paint(
                        origin,
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    let link = Bounds {
                        origin,
                        size: gpui::size(fix_line.width, line_height),
                    };
                    diagnostic_fix_bounds = Some(link);
                }
                if let Some(directive_link) = &directive_link {
                    if fix_line.is_some() {
                        y += line_height;
                    } else {
                        y += gap;
                    }
                    let origin = Point::new(tooltip_x + px(8.0), y);
                    let _ = directive_link.paint(
                        origin,
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    diagnostic_directive_link_bounds = Some(Bounds {
                        origin,
                        size: gpui::size(directive_link.width, line_height),
                    });
                }
                diagnostic_popup_bounds = Some(popup);
            }
        }
        {
            let editor = self.editor.read(cx);
            editor.diagnostic_popup_bounds.set(diagnostic_popup_bounds);
            editor.diagnostic_fix_bounds.set(diagnostic_fix_bounds);
            editor
                .diagnostic_directive_link_bounds
                .set(diagnostic_directive_link_bounds);
        }

        // Paint the `-- sqlly …` directive info popup and stamp its clickable
        // bounds. Cleared to `None` whenever no directive is hovered so a stale
        // click target can't linger.
        let mut directive_popup_bounds = None;
        // Never over a diagnostic popup: that one says what is wrong.
        if let Some((start, _end)) =
            hovered_sqlly_directive.filter(|_| diagnostic_popup_bounds.is_none())
        {
            let start = start.min(content.len());
            let (directive_line, dcolumn) = line_index.line_col_for_offset(start);
            let rel = dline(directive_line).saturating_sub(first_line);
            if let Some(pl) = layouts.lines.get(rel) {
                let x_start = pl.shaped.x_for_index(dcol(rel, dcolumn));
                let anchor_x = bounds.origin.x + line_number_width + x_start;
                let anchor_y = bounds.origin.y + line_height * (dline(directive_line) + 1) as f32;

                let line1 = SharedString::from(crate::localization::text(
                    "editor.element.directive_hover_explanation",
                ));
                let line2 = SharedString::from(crate::localization::text(
                    "editor.element.directive_hover_action",
                ));
                let len1 = line1.len();
                let len2 = line2.len();
                let shaped1 = window.text_system().shape_line(
                    line1,
                    font_size * 0.92,
                    &[TextRun {
                        len: len1,
                        font: font.clone(),
                        color: Theme::text(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let shaped2 = window.text_system().shape_line(
                    line2,
                    font_size * 0.92,
                    &[TextRun {
                        len: len2,
                        font: font.clone(),
                        color: Theme::accent(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let pad = px(8.0);
                let popup_w = shaped1.width.max(shaped2.width) + pad * 2.0;
                let popup_h = line_height * 2.0 + px(8.0);
                let popup_x = anchor_x
                    .min(viewport.origin.x + viewport.size.width - popup_w)
                    .max(viewport.origin.x);
                let popup_y = anchor_y.min(viewport.origin.y + viewport.size.height - popup_h);
                let popup_bounds = Bounds {
                    origin: Point::new(popup_x, popup_y),
                    size: gpui::size(popup_w, popup_h),
                };
                window.paint_quad(gpui::fill(popup_bounds, Theme::popup_bg()));
                window.paint_quad(gpui::quad(
                    popup_bounds,
                    gpui::Corners::default(),
                    gpui::transparent_black(),
                    gpui::Edges::all(px(1.0)),
                    Theme::accent(),
                    gpui::BorderStyle::Solid,
                ));
                let _ = shaped1.paint(
                    Point::new(popup_x + pad, popup_y + px(3.0)),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                let _ = shaped2.paint(
                    Point::new(popup_x + pad, popup_y + line_height + px(3.0)),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                directive_popup_bounds = Some(popup_bounds);
            }
        }
        self.editor
            .read(cx)
            .directive_hint_bounds
            .set(directive_popup_bounds);

        let mut hover_card_bounds = None;
        // A hover card (the pointer resting on a word) wins over signature
        // help (the caret inside a call): it is the more deliberate ask, and
        // the two would otherwise stack over each other.
        let paint_hover = hover_info.is_some()
            && fold_preview.is_none()
            && !completion_visible
            && hovered_diagnostic.is_none()
            && hovered_sqlly_directive.is_none();
        if paint_hover && let Some((info, (start, _end))) = hover_info.as_ref() {
            let start = (*start).min(content.len());
            let (hover_line, hover_col) = line_index.line_col_for_offset(start);
            let rel = dline(hover_line).saturating_sub(first_line);
            if let Some(pl) = layouts.lines.get(rel) {
                let x_start = pl.shaped.x_for_index(dcol(rel, hover_col));
                let anchor_x = bounds.origin.x + line_number_width + x_start;
                let anchor_y = bounds.origin.y + line_height * (dline(hover_line) + 1) as f32;
                let card_font = font_size * 0.92;
                let mut bold_font = font.clone();
                bold_font.weight = gpui::FontWeight::BOLD;
                let pad = px(8.0);
                let char_w = card_char_width(window, &font, card_font);
                let columns = super::info_cards::wrap_columns(
                    f32::from(viewport.size.width - pad * 2.0),
                    char_w,
                );
                // Below the word when the card fits there, else on whichever
                // side has more room, trimmed to what fits.
                let room_below = viewport.origin.y + viewport.size.height - anchor_y;
                let room_above = anchor_y - line_height - viewport.origin.y;
                let room = room_below.max(room_above);
                let max_lines =
                    (f32::from(room - px(8.0)) / f32::from(line_height)).floor() as usize;
                let lines = super::info_cards::hover_card(info, columns);
                let fits_below = line_height * lines.len() as f32 + px(8.0) <= room_below;
                let lines = super::info_cards::fit_lines(lines, max_lines);
                let shaped: Vec<ShapedLine> = lines
                    .iter()
                    .map(|line| shape_card_line(window, line, &font, &bold_font, card_font))
                    .collect();
                let mut popup_w = px(0.0);
                for line in &shaped {
                    popup_w = popup_w.max(line.width);
                }
                popup_w += pad * 2.0;
                let popup_h = line_height * shaped.len() as f32 + px(8.0);
                let popup_x = anchor_x
                    .min(viewport.origin.x + viewport.size.width - popup_w)
                    .max(viewport.origin.x);
                let popup_y = if fits_below || room_below >= room_above {
                    anchor_y
                } else {
                    (anchor_y - line_height - popup_h).max(viewport.origin.y)
                };
                let popup_bounds = Bounds {
                    origin: Point::new(popup_x, popup_y),
                    size: gpui::size(popup_w, popup_h),
                };
                paint_info_popup_frame(window, popup_bounds);
                for (i, line) in shaped.iter().enumerate() {
                    let _ = line.paint(
                        Point::new(popup_x + pad, popup_y + line_height * i as f32 + px(3.0)),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
                hover_card_bounds = Some(popup_bounds);
            }
        }
        self.editor
            .read(cx)
            .hover_card_bounds
            .set(hover_card_bounds);

        if let Some(line) = fold_preview
            && fold_headers.get(&line) == Some(&true)
        {
            let region = self.editor.read(cx).with_folds(|f| f.region_at(line));
            let chip = self
                .editor
                .read(cx)
                .fold_chip_bounds
                .borrow()
                .iter()
                .find(|(header, _)| *header == line)
                .map(|(_, bounds)| *bounds);
            if let (Some(region), Some(chip_bounds)) = (region, chip) {
                let (preview, remainder) = crate::folding::preview_lines(&content, region, 8);
                if !preview.is_empty() || remainder > 0 {
                    let pad = px(8.0);
                    let chip_x = chip_bounds.origin.x + px(3.0);
                    let line_y = chip_bounds.origin.y - px(2.0);
                    let mut shaped_lines: Vec<ShapedLine> = preview
                        .iter()
                        .map(|line| {
                            let ss = SharedString::from(line.clone());
                            let len = ss.len();
                            window.text_system().shape_line(
                                ss,
                                font_size,
                                &[TextRun {
                                    len,
                                    font: font.clone(),
                                    color: Theme::text(),
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                }],
                                None,
                            )
                        })
                        .collect();
                    if remainder > 0 {
                        let trailer = SharedString::from(crate::localization::format(
                            "editor.element.more_lines",
                            &[("count", &remainder.to_string())],
                        ));
                        let len = trailer.len();
                        shaped_lines.push(window.text_system().shape_line(
                            trailer,
                            font_size,
                            &[TextRun {
                                len,
                                font: font.clone(),
                                color: Theme::text_dim(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        ));
                    }
                    let mut popup_w = px(0.0);
                    for line in &shaped_lines {
                        popup_w = popup_w.max(line.width);
                    }
                    popup_w += pad * 2.0;
                    let popup_h = line_height * shaped_lines.len() as f32 + px(8.0);
                    let popup_x = chip_x
                        .min(viewport.origin.x + viewport.size.width - popup_w)
                        .max(viewport.origin.x);
                    let popup_y = (line_y + line_height)
                        .min(viewport.origin.y + viewport.size.height - popup_h);
                    let popup_bounds = Bounds {
                        origin: Point::new(popup_x, popup_y),
                        size: gpui::size(popup_w, popup_h),
                    };
                    paint_info_popup_frame(window, popup_bounds);
                    for (i, line) in shaped_lines.iter().enumerate() {
                        let _ = line.paint(
                            Point::new(popup_x + pad, popup_y + line_height * i as f32 + px(3.0)),
                            line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                }
            }
        }

        // A hover card (the more deliberate ask) replaces signature help.
        if let Some((shaped, popup_bounds, _)) = sig_card.filter(|_| hover_card_bounds.is_none()) {
            paint_info_popup_frame(window, popup_bounds);
            for (i, line) in shaped.iter().enumerate() {
                let _ = line.paint(
                    Point::new(
                        popup_bounds.origin.x + px(8.0),
                        popup_bounds.origin.y + line_height * i as f32 + px(3.0),
                    ),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        // Register input handler for IME
        window.handle_input(
            &focus_handle,
            gpui::ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
    }
}
