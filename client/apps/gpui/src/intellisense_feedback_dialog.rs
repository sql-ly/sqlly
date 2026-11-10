//! "Improve IntelliSense" feedback dialog.
//!
//! Its own entity, hosted by `AppView` above the workbench, because the
//! dialog chrome positions itself in window coordinates: rendered inside the
//! query editor's subtree (where it used to live) the card was clipped to —
//! and mis-centered within — the editor pane. Hosted at the root it centers
//! over the whole window, and the title bar's pop-out affordance detaches it
//! into a standalone OS window (`windowed` presentation), matching the row
//! editor / data-transfer pattern.
//!
//! The capture itself still happens in the editor (it owns the completion
//! popup state); the editor emits `SqlEditorEvent::FeedbackRequested` with
//! the frozen snapshot and `AppView` opens this dialog. Submissions flow
//! back through [`FeedbackDialogEvent::Submitted`] to the app's background
//! upload queue.

use gpui::{
    Context, Entity, EventEmitter, Focusable as _, Font, FontWeight, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::{Selectable as _, Sizable as _};

use crate::dialog::{Dialog, DialogFrame};
use crate::feedback_tree::{
    CheckState, FeedbackTree, KindVisibility, TreeNodeKind, TreeSelection, required_explanation,
};
use crate::intellisense_feedback::FeedbackSnapshot;
use crate::scrollbars::DraggableScrollbar;
use crate::theme::Theme;
use crate::tooltip::AppTooltip as _;

pub enum FeedbackDialogEvent {
    /// The user submitted: the snapshot is already disclosure-filtered — only
    /// the data-model names they opted into are present at all. Boxed: the
    /// snapshot is large.
    Submitted {
        expected_text: String,
        snapshot: Box<FeedbackSnapshot>,
    },
    /// Closed without submitting (✕, Close, Escape).
    Dismissed,
    /// The dialog moved into its own OS window; the host should stop
    /// rendering the inline modal but keep listening for `Submitted`.
    PoppedOut,
}

pub struct IntelliSenseFeedbackDialog {
    /// The full captured context. Never sent as-is: filtered through `tree` +
    /// `selection` at submit time.
    snapshot: FeedbackSnapshot,
    /// Disclosure tree over `snapshot.catalog` (schemas, tables, columns,
    /// procedures, parameters, snippets).
    tree: FeedbackTree,
    selection: TreeSelection,
    /// Which object kinds the disclosure list shows. Seeded from the user's
    /// settings defaults; adjustable per report via the checkbox row. Purely
    /// list visibility — picking a table still sends all of its columns.
    kinds: KindVisibility,
    /// Opt-in: ship the multi-hop foreign-key chains rooted at the included
    /// tables. Off by default — chains grow the payload, and one hop already
    /// rides on every included table unconditionally.
    include_fk_chains: bool,
    /// Opt-in: replace the query's string literals and comments with
    /// `<redacted>` in the preview and the report. Off by default — the query
    /// is sent exactly as typed, since a literal's value is often what the
    /// completion problem depends on.
    mask_literals: bool,
    /// Manually expanded tree nodes; a live search auto-expands ancestors of
    /// matches on top of this.
    expanded: std::collections::HashSet<usize>,
    suggestion_count: usize,
    top_suggestions: Vec<String>,
    /// Search and note inputs, materialized on first render — the event
    /// handler that opens this dialog has no `Window`, and gpui-component
    /// input states need one (same idiom as the rename dialog).
    search: Option<Entity<InputState>>,
    input: Option<Entity<TextareaState>>,
    error: Option<String>,
    /// Set when the user tried to untick a locked (suggestion-backing) row:
    /// the dialog then explains why it stays in the report.
    required_notice: bool,
    /// The query, read-only: selectable and copyable like any text box.
    query_view: Option<Entity<TextareaState>>,
    /// Whether `query_view` currently shows the masked text (`None` before
    /// it is first filled), so a mask toggle refills it.
    query_view_masked: Option<bool>,
    /// The editor's monospace font, for the query preview.
    font: Font,
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and the native title bar owns close.
    windowed: bool,
    _search_subscription: Option<Subscription>,
}

impl EventEmitter<FeedbackDialogEvent> for IntelliSenseFeedbackDialog {}

impl IntelliSenseFeedbackDialog {
    pub fn new(
        snapshot: FeedbackSnapshot,
        include_all: bool,
        kinds: KindVisibility,
        font: Font,
    ) -> Self {
        let tree = FeedbackTree::build(&snapshot);
        // Start with the top level open when it is small enough to take in
        // at a glance; a big catalog starts collapsed and is searched.
        let expanded: std::collections::HashSet<usize> = if tree.roots.len() <= 16 {
            tree.roots.iter().copied().collect()
        } else {
            std::collections::HashSet::new()
        };
        let suggestion_count = snapshot.suggestions.len();
        let top_suggestions = snapshot
            .suggestions
            .iter()
            .take(3)
            .map(|s| s.label.clone())
            .collect();
        // A statement-scoped capture starts with the statement-referenced
        // objects checked — the model the user was working against is the
        // context a report needs, and it should not be dropped just because
        // the user did not re-tick it. Include-all captures need no seeding.
        let checked = if include_all {
            Default::default()
        } else {
            tree.default_selection(&snapshot)
        };
        Self {
            snapshot,
            tree,
            selection: TreeSelection {
                include_all,
                checked,
            },
            kinds,
            expanded,
            suggestion_count,
            top_suggestions,
            search: None,
            input: None,
            error: None,
            required_notice: false,
            query_view: None,
            query_view_masked: None,
            include_fk_chains: false,
            mask_literals: false,
            font,
            frame: DialogFrame::new(Theme::scaled(940.0), Theme::scaled(560.0)),
            windowed: false,
            _search_subscription: None,
        }
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            let search = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "feedback.intellisense.search_placeholder",
                ))
            });
            self._search_subscription =
                Some(cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                }));
            self.search = Some(search);
        }
        if self.input.is_none() {
            // Multi-line note: Enter inserts a newline, so there is no
            // submit-on-Enter subscription — the user submits with the button.
            let input = cx.new(|cx| {
                TextareaState::new(window, cx).auto_grow(3, 8).placeholder(
                    crate::localization::input_placeholder(
                        window,
                        cx,
                        "feedback.intellisense.expectation_placeholder",
                    ),
                )
            });
            // Focus once the textarea is on screen: focusing it before its
            // first frame never starts the caret blinking, so the box took
            // typing with no visible caret.
            let focus = input.focus_handle(cx);
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
            self.input = Some(input);
        }
        if self.query_view.is_none() {
            self.query_view = Some(cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 9)
                    .soft_wrap(false)
            }));
        }
    }

    /// Fill the read-only query box with the previewed SQL (again whenever
    /// masking flips) and park its cursor where the editor's caret was.
    fn sync_query_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.query_view_masked == Some(self.mask_literals) {
            return;
        }
        let Some(view) = self.query_view.clone() else {
            return;
        };
        let (before, after) = self.preview_sql();
        let caret = before.len();
        let text = format!("{before}{after}");
        view.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(caret..caret, cx);
        });
        self.query_view_masked = Some(self.mask_literals);
    }

    fn toggle_check(&mut self, node: usize, cx: &mut Context<Self>) {
        // A refused toggle is a locked row: explain instead of silently
        // ignoring the click.
        self.required_notice = !self.tree.toggle(&mut self.selection, node);
        cx.notify();
    }

    fn toggle_expand(&mut self, node: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&node) {
            self.expanded.insert(node);
        }
        cx.notify();
    }

    fn toggle_include_all(&mut self, cx: &mut Context<Self>) {
        self.selection.include_all = !self.selection.include_all;
        // Unticking "Include all" with nothing individually picked would leave
        // an empty model; land on the statement-referenced seed instead.
        if !self.selection.include_all && self.selection.checked.is_empty() {
            self.selection.checked = self.tree.default_selection(&self.snapshot);
        }
        cx.notify();
    }

    /// Flip one kind-visibility toggle for this report (settings defaults are
    /// untouched).
    fn toggle_kind(&mut self, key: &'static str, cx: &mut Context<Self>) {
        let kinds = &mut self.kinds;
        match key {
            "procedures" => kinds.procedures = !kinds.procedures,
            "views" => kinds.views = !kinds.views,
            "functions" => kinds.functions = !kinds.functions,
            "tables" => kinds.tables = !kinds.tables,
            "columns" => kinds.columns = !kinds.columns,
            "types" => kinds.types = !kinds.types,
            _ => {}
        }
        cx.notify();
    }

    fn toggle_fk_chains(&mut self, cx: &mut Context<Self>) {
        self.include_fk_chains = !self.include_fk_chains;
        cx.notify();
    }

    fn toggle_mask_literals(&mut self, cx: &mut Context<Self>) {
        self.mask_literals = !self.mask_literals;
        cx.notify();
    }

    /// The query as previewed and sent, split at the caret: exactly as typed
    /// unless the user chose to mask literals.
    fn preview_sql(&self) -> (&str, &str) {
        if self.mask_literals {
            (
                &self.snapshot.sql_before_cursor,
                &self.snapshot.sql_after_cursor,
            )
        } else {
            let raw = &self.snapshot.unredacted_sql;
            (&raw.before_cursor, &raw.after_cursor)
        }
    }

    /// Validates the note, filters the context through the disclosure tree
    /// *here* — only the data-model names the user opted into are serialized
    /// at all — then hands the submission to the host, which uploads it in
    /// the background (with retries) and reports in the status bar.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.input else {
            return;
        };
        let expected = input.read(cx).value().trim().to_string();
        if expected.is_empty() {
            self.error = Some(crate::localization::text(
                "feedback.intellisense.expectation_required",
            ));
            cx.notify();
            return;
        }
        let mut filtered = self.tree.filter_snapshot(&self.snapshot, &self.selection);
        if !self.mask_literals {
            filtered.reveal_literals();
        }
        if self.include_fk_chains {
            // Only chains rooted at a table that is actually in the report.
            let included: std::collections::HashSet<(String, String)> = filtered
                .catalog
                .objects
                .iter()
                .map(|o| {
                    (
                        o.schema.as_deref().unwrap_or("").to_ascii_lowercase(),
                        o.name.to_ascii_lowercase(),
                    )
                })
                .collect();
            filtered.fk_chains.retain(|chain| {
                chain.first().is_some_and(|hop| {
                    included.contains(&(
                        hop.from_schema.to_ascii_lowercase(),
                        hop.from_table.to_ascii_lowercase(),
                    ))
                })
            });
        } else {
            filtered.fk_chains.clear();
        }
        cx.emit(FeedbackDialogEvent::Submitted {
            expected_text: expected,
            snapshot: Box::new(filtered),
        });
        if self.windowed {
            // Deferred so the queued `Submitted` reaches the host first — a
            // popped-out dialog is owned only by its window, and GPUI drops a
            // released entity's subscribers before delivering its events.
            window.defer(cx, |window, _| window.remove_window());
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FeedbackDialogEvent::Dismissed);
        if self.windowed {
            window.remove_window();
        }
    }

    /// Move this dialog into its own OS window (same entity, `windowed`
    /// presentation). The host drops the inline overlay on `PoppedOut`; its
    /// detached subscription keeps routing `Submitted`.
    fn pop_out(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        // Opening the window renders this entity into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "feedback.intellisense.dialog_title",
                    Theme::scaled(960.0),
                    Theme::scaled(620.0),
                ),
                None,
                cx,
                move |_, cx| {
                    // Flip to the windowed presentation before the new
                    // window's first render.
                    build_entity.update(cx, |dialog, _| dialog.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                entity.update(cx, |_, cx| cx.emit(FeedbackDialogEvent::PoppedOut));
            }
        });
    }
}

impl Render for IntelliSenseFeedbackDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_inputs(window, cx);
        self.sync_query_view(window, cx);

        let s = Theme::scaled;
        let small = px(Theme::font_size_ratio(0.85));
        let tiny = px(Theme::font_size_ratio(0.75));
        let include_all = self.selection.include_all;

        let summary = if self.top_suggestions.is_empty() {
            crate::localization::format(
                "feedback.intellisense.suggestions_summary",
                &[("count", &self.suggestion_count.to_string())],
            )
        } else {
            crate::localization::format(
                "feedback.intellisense.suggestions_summary_top",
                &[
                    ("count", &self.suggestion_count.to_string()),
                    ("top", &self.top_suggestions.join(", ")),
                ],
            )
        };
        let error_line = self.error.as_ref().map(|error| {
            div()
                .text_size(small)
                .text_color(Theme::error())
                .child(error.clone())
        });

        // The three-state checkbox glyph: accent+check = included with
        // everything below it, grey+dash = name only (a descendant is
        // ticked), empty = not sent.
        let check_glyph = |state: CheckState, locked: bool| {
            let base = div()
                .flex_none()
                .w(px(s(14.0)))
                .h(px(s(14.0)))
                .rounded_sm()
                .border_1()
                .flex()
                .items_center()
                .justify_center()
                // Fixed glyph: the tick is drawn to fit its checkbox.
                .text_size(px(s(10.0)));
            match state {
                // Locked rows keep the checked look, muted, so they read as
                // "included, not yours to change".
                CheckState::Checked if locked => base
                    .border_color(Theme::accent().opacity(0.55))
                    .bg(Theme::accent().opacity(0.55))
                    .text_color(Theme::ink_on(Theme::accent()))
                    .child("✓"),
                CheckState::Checked => base
                    .border_color(Theme::accent())
                    .bg(Theme::accent())
                    .text_color(Theme::ink_on(Theme::accent()))
                    .child("✓"),
                CheckState::Implied => base
                    .border_color(Theme::text_dim().opacity(0.5))
                    .bg(Theme::text_dim().opacity(0.35))
                    .text_color(Theme::ink_on(Theme::text_dim()))
                    .child("–"),
                CheckState::Unchecked => base.border_color(Theme::border()),
            }
        };

        // ---- Left pane: search + include-all + disclosure tree ----
        let search_query = self
            .search
            .as_ref()
            .map(|search| search.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let search_filter = if include_all {
            None
        } else {
            self.tree.search(&search_query)
        };
        let implied = self.tree.implied_set(&self.selection);

        /// Rendering cap: a search for a single letter over a huge model
        /// can match thousands of nodes; everything past the cap is
        /// summarized in one trailing row.
        const MAX_TREE_ROWS: usize = 400;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut overflow = 0usize;
        let mut stack: Vec<usize> = self.tree.roots.iter().rev().copied().collect();
        while let Some(index) = stack.pop() {
            let node = &self.tree.nodes[index];
            if let Some(filter) = &search_filter
                && !filter.visible.contains(&index)
            {
                continue;
            }
            // Kind visibility: a hidden object kind drops the row and its
            // subtree from the list; hidden columns drop just the column
            // rows. Include-all shows everything (the full catalog is sent).
            if !include_all {
                // Objects the suggestions came from always stay listed: they
                // are sent regardless, so the user must be able to see them.
                let hidden = match &node.kind {
                    TreeNodeKind::Object { .. } => {
                        !self.kinds.shows_kind(node.detail) && !self.tree.is_required(index)
                    }
                    TreeNodeKind::Column { .. } => !self.kinds.columns,
                    _ => false,
                };
                if hidden {
                    continue;
                }
            }
            let auto_expand = search_filter
                .as_ref()
                .is_some_and(|f| f.auto_expand.contains(&index));
            let is_expanded = auto_expand || self.expanded.contains(&index);
            let has_children = !node.children.is_empty();
            if is_expanded && has_children {
                for &child in node.children.iter().rev() {
                    stack.push(child);
                }
            }
            if rows.len() >= MAX_TREE_ROWS {
                overflow += 1;
                continue;
            }

            let state = self.tree.check_state(&self.selection, &implied, index);
            let locked = !include_all && self.tree.is_required(index);

            let disclosure = if has_children {
                let arrow = div()
                    .flex_none()
                    .w(px(s(14.0)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(Theme::text_dim())
                    .child(crate::icons::disclosure_chevron(is_expanded));
                if include_all {
                    arrow.into_any_element()
                } else {
                    arrow
                        .id(("fb-exp", index))
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                this.toggle_expand(index, cx);
                            }),
                        )
                        .into_any_element()
                }
            } else {
                div().flex_none().w(px(s(14.0))).into_any_element()
            };

            let checkbox = {
                let glyph = check_glyph(state, locked);
                if include_all {
                    glyph.into_any_element()
                } else {
                    glyph
                        .id(("fb-check", index))
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                this.toggle_check(index, cx);
                            }),
                        )
                        .into_any_element()
                }
            };

            // Clicking the name expands containers and ticks leaves.
            let label = {
                let text = div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(node.label.clone());
                if include_all {
                    text.into_any_element()
                } else {
                    text.id(("fb-label", index))
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, _window, cx| {
                                if has_children {
                                    this.toggle_expand(index, cx);
                                } else {
                                    this.toggle_check(index, cx);
                                }
                            }),
                        )
                        .into_any_element()
                }
            };

            let detail = (!node.detail.is_empty()).then(|| {
                div()
                    .flex_none()
                    .text_size(tiny)
                    .text_color(Theme::text_dim())
                    .child(node.detail)
            });
            // Tag the locked root (the object or snippet itself, not each of
            // its columns) so the reason is visible without hovering.
            let required_tag = (locked && self.tree.required.contains(&index)).then(|| {
                div()
                    .flex_none()
                    .text_size(tiny)
                    .text_color(Theme::accent())
                    .child(crate::localization::text(
                        "feedback.intellisense.required_badge",
                    ))
            });

            rows.push(
                div()
                    .id(("fb-node", index))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(s(8.0) + node.depth as f32 * s(14.0)))
                    .pr_2()
                    .py(px(s(1.0)))
                    .text_size(small)
                    .when(!include_all, |d| {
                        d.hover(|style| style.bg(Theme::row_hover()))
                    })
                    .when(locked, |d| d.app_tooltip_wrapping(required_explanation()))
                    .child(disclosure)
                    .child(checkbox)
                    .child(label)
                    .children(detail)
                    .children(required_tag)
                    .into_any_element(),
            );
        }
        if overflow > 0 {
            rows.push(
                div()
                    .pl(px(s(8.0)))
                    .py_1()
                    .text_size(small)
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "feedback.intellisense.search_overflow",
                        &[("count", &overflow.to_string())],
                    ))
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .p_2()
                    .text_size(small)
                    .text_color(Theme::text_dim())
                    .child(if search_query.is_empty() {
                        crate::localization::text("feedback.intellisense.no_data_model")
                    } else {
                        crate::localization::format(
                            "feedback.intellisense.no_search_matches",
                            &[("query", &search_query)],
                        )
                    })
                    .into_any_element(),
            );
        }

        let search_box = match &self.search {
            Some(search) if !include_all => {
                div().flex_1().child(Input::new(search)).into_any_element()
            }
            // Greyed placeholder while "include all" owns the choice.
            _ => div()
                .flex_1()
                .h(px(s(26.0)))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(Theme::border())
                .text_size(small)
                .text_color(Theme::text_dim().opacity(0.5))
                .child(crate::localization::text(
                    "feedback.intellisense.disabled_search_placeholder",
                ))
                .into_any_element(),
        };

        // Stock gpui-component checkbox: themed, focusable, Space/Enter
        // toggling for free (the tree rows keep the bespoke tri-state glyph,
        // which the stock control cannot express).
        let this = cx.entity();
        let include_all_toggle = div().flex_none().child(
            Checkbox::new("fb-include-all")
                .checked(include_all)
                .accessibility_label(crate::localization::text(
                    "feedback.intellisense.include_all_accessibility",
                ))
                .child(div().text_size(small).child(crate::localization::text(
                    "feedback.intellisense.include_all_label",
                )))
                .tab_stop(true)
                .on_click({
                    let this = this.clone();
                    move |_checked: &bool, _window, app| {
                        this.update(app, |this, cx| this.toggle_include_all(cx));
                    }
                }),
        );

        // Per-kind visibility toggles: what the list shows, not what a picked
        // table sends (a picked table always ships all of its columns).
        let kinds = self.kinds;
        let kind_toggles: Vec<gpui::AnyElement> = [
            (
                "procedures",
                crate::localization::text("feedback.intellisense.kind_procedures"),
                kinds.procedures,
            ),
            (
                "views",
                crate::localization::text("feedback.intellisense.kind_views"),
                kinds.views,
            ),
            (
                "functions",
                crate::localization::text("feedback.intellisense.kind_functions"),
                kinds.functions,
            ),
            (
                "tables",
                crate::localization::text("feedback.intellisense.kind_tables"),
                kinds.tables,
            ),
            (
                "columns",
                crate::localization::text("feedback.intellisense.kind_columns"),
                kinds.columns,
            ),
            (
                "types",
                crate::localization::text("feedback.intellisense.kind_types"),
                kinds.types,
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (key, label, on))| {
            div()
                .flex_none()
                .child(
                    Checkbox::new(("fb-kind", i))
                        .checked(on)
                        .disabled(include_all)
                        .accessibility_label(kind_toggle_accessibility_label(&label))
                        .child(div().text_size(tiny).child(label))
                        .tab_stop(!include_all)
                        .on_click({
                            let this = this.clone();
                            move |_checked: &bool, _window, app| {
                                this.update(app, |this, cx| this.toggle_kind(key, cx));
                            }
                        }),
                )
                .into_any_element()
        })
        .collect();

        // Opt-in relationship chains: multi-hop FK paths rooted at the
        // included tables. Independent of include-all (it widens what rides
        // with whatever set of tables the report ends up containing).
        let fk_chains_available = !self.snapshot.fk_chains.is_empty();
        let fk_chain_toggle = fk_chains_available.then(|| {
            div().flex_none().child(
                Checkbox::new("fb-fk-chains")
                    .checked(self.include_fk_chains)
                    .accessibility_label(crate::localization::text(
                        "feedback.intellisense.fk_chains_accessibility",
                    ))
                    .child(div().text_size(tiny).child(crate::localization::text(
                        "feedback.intellisense.fk_chains_label",
                    )))
                    .tab_stop(true)
                    .on_click({
                        let this = this.clone();
                        move |_checked: &bool, _window, app| {
                            this.update(app, |this, cx| this.toggle_fk_chains(cx));
                        }
                    }),
            )
        });

        let tree_pane = div()
            .flex_none()
            .w(px(s(340.0)))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(search_box)
                    .child(include_all_toggle),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .gap_y_1()
                    .children(
                        kind_toggles.into_iter().map(|toggle| {
                            div().when(include_all, |d| d.opacity(0.45)).child(toggle)
                        }),
                    )
                    .children(fk_chain_toggle),
            )
            .child(
                div()
                    .id("fb-tree")
                    .flex_1()
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(Theme::border())
                    .py_1()
                    .when(include_all, |d| d.opacity(0.45))
                    .children(rows),
            )
            .child(
                div()
                    .text_size(tiny)
                    .text_color(Theme::text_dim())
                    .child(if include_all {
                        crate::localization::text("feedback.intellisense.include_all_summary")
                    } else {
                        selection_summary(self.selection.checked.len(), self.tree.required.len())
                    }),
            )
            .when(self.required_notice && !include_all, |d| {
                d.child(
                    div()
                        .id("fb-required-notice")
                        .text_size(tiny)
                        .text_color(Theme::accent())
                        .child(required_explanation()),
                )
            });

        // ---- Right pane: the query (with caret), disclosure notes,
        // the expectation input, and the buttons. ----
        let (sql_before, _) = self.preview_sql();
        let caret_line = sql_before.matches('\n').count() + 1;
        let caret_column = sql_before.rsplit('\n').next().unwrap_or("").chars().count() + 1;

        let note = |text: &str| {
            div()
                .flex()
                .w_full()
                .min_w(px(0.0))
                .gap_1()
                .text_size(small)
                .text_color(Theme::text_dim())
                .child(div().flex_none().child("•"))
                .child(div().flex_1().min_w(px(0.0)).child(text.to_string()))
        };

        // The query, read-only: select and copy any part of it. Its cursor
        // starts where the editor's caret was.
        let query_pane = div()
            .flex_none()
            .font_family(self.font.family.clone())
            .text_size(small)
            .children(
                self.query_view
                    .as_ref()
                    .map(|view| Textarea::new(view).readonly(true)),
            );

        // Mask/unmask the literals — only offered when the query has
        // literals or comments to mask.
        let literals_toggle = self.snapshot.has_redacted_literals().then(|| {
            Button::new("fb-literals")
                .xsmall()
                .ghost()
                .compact()
                .label(if self.mask_literals {
                    crate::localization::text("feedback.intellisense.unmask_literals_button")
                } else {
                    crate::localization::text("feedback.intellisense.mask_literals_button")
                })
                .selected(self.mask_literals)
                .tooltip(if self.mask_literals {
                    crate::localization::text("feedback.intellisense.unmask_literals_tooltip")
                } else {
                    crate::localization::text("feedback.intellisense.mask_literals_tooltip")
                })
                .on_click(cx.listener(|this, _, _window, cx| this.toggle_mask_literals(cx)))
        });

        // The whole pane scrolls so the note box, placed right under the
        // query, stays reachable on a short screen; the disclosure notes
        // follow it.
        let right_pane = div()
            .id("fb-right")
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(crate::localization::format(
                                "feedback.intellisense.query_heading",
                                &[
                                    ("line", &caret_line.to_string()),
                                    ("column", &caret_column.to_string()),
                                ],
                            )),
                    )
                    .children(literals_toggle),
            )
            .child(query_pane)
            .children(
                self.input
                    .as_ref()
                    .map(|input| div().flex_none().child(Textarea::new(input))),
            )
            .children(error_line)
            .child(
                div()
                    .text_size(small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(crate::localization::text(
                        "feedback.intellisense.disclosure_heading",
                    )),
            )
            .child(note(&crate::localization::text(
                "feedback.intellisense.disclosure_capture",
            )))
            .child(note(crate::settings_snapshot::DISCLOSURE))
            .child(note(&crate::localization::text(
                "feedback.intellisense.disclosure_names",
            )))
            .child(note(&if self.mask_literals {
                crate::localization::text("feedback.intellisense.disclosure_masked")
            } else {
                crate::localization::text("feedback.intellisense.disclosure_unmasked")
            }))
            .child(note(&crate::localization::text(
                "feedback.intellisense.disclosure_never",
            )));

        // The two-pane disclosure layout manages its own scrolling, so
        // the chrome body is bare; this wrapper carries the old card's
        // padding and, crucially, the Escape action that gpui-component
        // inputs (the search box and the textarea) emit while focused.
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .text_color(Theme::text())
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Escape, window, cx| {
                    this.cancel(window, cx);
                }),
            )
            .child(
                div()
                    .text_size(small)
                    .text_color(Theme::text_dim())
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(tree_pane)
                    .child(right_pane),
            )
            .into_any_element();
        let submit_button = Button::new("intellisense-feedback-submit")
            .label(crate::localization::text(
                "feedback.intellisense.submit_button",
            ))
            .primary()
            .on_click(cx.listener(|this, _, window, cx| {
                this.submit(window, cx);
            }))
            .into_any_element();

        let mut dialog = Dialog::new("intellisense-feedback-dialog", |this: &mut Self| {
            &mut this.frame
        })
        .title(crate::localization::text(
            "feedback.intellisense.dialog_title",
        ))
        .bare_body()
        .dismiss_on_scrim_click(false)
        .windowed(self.windowed)
        .on_close(|this, window, cx| this.cancel(window, cx))
        .child(body)
        .button(submit_button);
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog
            .build(self, window, cx)
            // Keeps a scrollbar drag tracking once the pointer leaves the
            // narrow track (the query-preview scrollbars).
            .scrollbar_drag_area()
    }
}

/// The line under the disclosure list describing what the selection sends.
fn selection_summary(picked: usize, required: usize) -> String {
    let picked_text = picked.to_string();
    let required_text = required.to_string();
    let picked_text = picked_text.as_str();
    let required_text = required_text.as_str();
    let (key, arguments): (&str, Vec<(&str, &str)>) = match (picked, required) {
        (0, 0) => ("feedback.intellisense.selection_none", Vec::new()),
        (0, 1) => (
            "feedback.intellisense.selection_required_one",
            vec![("required", required_text)],
        ),
        (0, _) => (
            "feedback.intellisense.selection_required_many",
            vec![("required", required_text)],
        ),
        (1, 0) => (
            "feedback.intellisense.selection_picked_one",
            vec![("picked", picked_text)],
        ),
        (_, 0) => (
            "feedback.intellisense.selection_picked_many",
            vec![("picked", picked_text)],
        ),
        (1, _) => (
            "feedback.intellisense.selection_both_one",
            vec![("picked", picked_text), ("required", required_text)],
        ),
        (_, _) => (
            "feedback.intellisense.selection_both_many",
            vec![("picked", picked_text), ("required", required_text)],
        ),
    };
    crate::localization::format(key, &arguments)
}

/// What a screen reader announces for a per-kind visibility toggle. The
/// checkbox role announces the checked/unchecked state itself; the label must
/// carry the kind and what toggling it does, not just the bare word next to
/// the box. Space/Enter toggling and tab focus come from the stock checkbox.
fn kind_toggle_accessibility_label(kind: &str) -> String {
    crate::localization::format(
        "feedback.intellisense.kind_toggle_accessibility",
        &[("kind", &kind.to_lowercase())],
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::intellisense_feedback::capture_with_full_catalog;
    use sqlly_intel::intellisense::CompletionCatalog;
    use sqlly_services::IntelliSenseRuleConfig;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn sample_snapshot() -> FeedbackSnapshot {
        capture_with_full_catalog(
            "SELECT * FROM dbo.",
            18,
            &[],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
            true,
        )
    }

    #[gpui::test]
    fn submit_requires_a_note_then_emits(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            IntelliSenseFeedbackDialog::new(
                sample_snapshot(),
                false,
                KindVisibility::default(),
                gpui::font("monospace"),
            )
        });
        let submitted: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let log = submitted.clone();
            cx.subscribe(&entity, move |_, _, event: &FeedbackDialogEvent, _| {
                if let FeedbackDialogEvent::Submitted { expected_text, .. } = event {
                    log.borrow_mut().push(expected_text.clone());
                }
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            dialog.ensure_inputs(window, cx);
            dialog.submit(window, cx);
            assert!(dialog.error.is_some(), "empty note must be rejected");
            dialog.input.as_ref().unwrap().update(cx, |state, cx| {
                state.set_value("offer the Id column", window, cx)
            });
            dialog.submit(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(submitted.borrow().as_slice(), ["offer the Id column"]);
    }

    /// The query is sent exactly as typed until the user masks literals.
    #[gpui::test]
    fn mask_literals_redacts_what_is_sent(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let sql = "SELECT * FROM dbo.Users WHERE Name = 'Ada' AND ";
        let snapshot = capture_with_full_catalog(
            sql,
            sql.len(),
            &[],
            sql.len(),
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
            true,
        );
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            IntelliSenseFeedbackDialog::new(
                snapshot,
                true,
                KindVisibility::default(),
                gpui::font("monospace"),
            )
        });
        let sent: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let log = sent.clone();
            cx.subscribe(&entity, move |_, _, event: &FeedbackDialogEvent, _| {
                if let FeedbackDialogEvent::Submitted { snapshot, .. } = event {
                    log.borrow_mut().push(snapshot.sql_before_cursor.clone());
                }
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            assert_eq!(dialog.preview_sql().0, sql);
            dialog.ensure_inputs(window, cx);
            dialog
                .input
                .as_ref()
                .unwrap()
                .update(cx, |state, cx| state.set_value("offer columns", window, cx));
            dialog.submit(window, cx);
            dialog.toggle_mask_literals(cx);
            assert!(!dialog.preview_sql().0.contains("'Ada'"));
            dialog.submit(window, cx);
        });
        cx.run_until_parked();
        let sent = sent.borrow();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sql, "sent as typed by default");
        assert!(!sent[1].contains("'Ada'"), "masked literals are redacted");
    }

    #[gpui::test]
    fn cancel_emits_dismissed(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            IntelliSenseFeedbackDialog::new(
                sample_snapshot(),
                true,
                KindVisibility::default(),
                gpui::font("monospace"),
            )
        });
        let dismissed = Rc::new(RefCell::new(0usize));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let log = dismissed.clone();
            cx.subscribe(&entity, move |_, _, event: &FeedbackDialogEvent, _| {
                if matches!(event, FeedbackDialogEvent::Dismissed) {
                    *log.borrow_mut() += 1;
                }
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            assert!(dialog.selection.include_all);
            dialog.cancel(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(*dismissed.borrow(), 1);
    }

    /// The per-kind toggles must announce their kind and purpose — a screen
    /// reader hearing six bare checkboxes named "Views", "Tables", … has no
    /// way to know what checking one does.
    #[test]
    fn kind_toggles_announce_their_kind_and_purpose() {
        assert_eq!(
            kind_toggle_accessibility_label("Procedures"),
            "Show procedures in the object list"
        );
        assert_eq!(
            kind_toggle_accessibility_label("Types"),
            "Show types in the object list"
        );
    }

    /// A statement-scoped dialog opens with the referenced objects checked
    /// (include-all captures skip the seeding).
    #[gpui::test]
    fn scoped_dialog_seeds_the_referenced_selection(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            IntelliSenseFeedbackDialog::new(
                sample_snapshot(),
                false,
                KindVisibility::default(),
                gpui::font("monospace"),
            )
        });
        view.update(cx, |dialog, _| {
            assert!(!dialog.selection.include_all);
            assert_eq!(
                dialog.selection.checked,
                dialog.tree.default_selection(&dialog.snapshot),
                "the statement-referenced objects start checked"
            );
        });
    }

    #[test]
    fn selection_summary_counts_required_items() {
        assert_eq!(
            selection_summary(0, 0),
            "Nothing ticked: no data-model names will be sent."
        );
        assert_eq!(
            selection_summary(0, 2),
            "2 required items from the suggestions will be sent."
        );
        assert_eq!(
            selection_summary(1, 1),
            "1 item ticked plus 1 required — greyed boxes send the name only."
        );
    }

    /// Clicking a locked row explains itself instead of unticking, and the
    /// submission still carries the suggestion-backing object.
    #[gpui::test]
    fn locked_row_click_explains_and_submission_keeps_it(cx: &mut gpui::TestAppContext) {
        use sqlly_intel::intellisense::{
            CompletionItem, CompletionKind, CompletionObject, CompletionSource,
        };
        ensure_components_initialized(cx);
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["dbo".to_string()],
            objects: vec![
                CompletionObject::table("dbo", "Users", vec!["Email".to_string()]),
                CompletionObject::table("dbo", "Other", vec!["Id".to_string()]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture_with_full_catalog(
            "SELECT ",
            7,
            &[CompletionItem {
                label: "Email".to_string(),
                insert_text: "Email".to_string(),
                kind: CompletionKind::Column,
                source: CompletionSource::LiveMetadata,
                score: 70,
                detail: String::new(),
                replace_span: None,
            }],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        let (view, cx) = cx.add_window_view(|_window, _cx| {
            IntelliSenseFeedbackDialog::new(
                snapshot,
                false,
                KindVisibility::default(),
                gpui::font("monospace"),
            )
        });
        let submitted: Rc<RefCell<Vec<FeedbackSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        view.update(cx, |_, cx| {
            let entity = cx.entity();
            let log = submitted.clone();
            cx.subscribe(&entity, move |_, _, event: &FeedbackDialogEvent, _| {
                if let FeedbackDialogEvent::Submitted { snapshot, .. } = event {
                    log.borrow_mut().push((**snapshot).clone());
                }
            })
            .detach();
        });
        view.update_in(cx, |dialog, window, cx| {
            let users = dialog
                .tree
                .nodes
                .iter()
                .position(|n| n.label == "Users")
                .unwrap();
            assert!(dialog.tree.is_required(users));
            dialog.toggle_check(users, cx);
            assert!(dialog.required_notice, "the click is explained");
            // Explicitly drop every tick: the required table still ships.
            dialog.selection.checked.clear();
            dialog.ensure_inputs(window, cx);
            dialog
                .input
                .as_ref()
                .unwrap()
                .update(cx, |state, cx| state.set_value("offer Email", window, cx));
            dialog.submit(window, cx);
        });
        cx.run_until_parked();
        let submitted = submitted.borrow();
        assert_eq!(submitted.len(), 1);
        let names: Vec<&str> = submitted[0]
            .catalog
            .objects
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, ["Users"]);
    }
}
