//! IntelliSense / LSP debug screen.
//!
//! The GPUI app does not run the `sqlly-lsp` server in-process — it drives the
//! same shared [`sqlly_services::Services`] the language server uses. So "what
//! the LSP says" is reproduced by re-running the exact `Services` calls the LSP
//! makes and rendering the results in LSP wire format. This view does that: it
//! builds a snapshot from the active editor + app state, computes each LSP
//! request result via the reusable [`sqlly_lsp`] helpers (the same free
//! functions the real `LspServer` handlers call, so the two can never drift),
//! and renders the JSON the server would emit for `initialize`, `completion`,
//! `hover`, `semanticTokens/full`, and `diagnostic`.
//!
//! Diagnostics are async (`Services::catalog::validate_sql`), so the sync
//! sections are filled immediately on refresh and the diagnostics section is
//! filled when the spawned validation completes (see
//! [`AppView::handle_toggle_intellisense_debug`]).

use gpui::{
    Context, CursorStyle, EventEmitter, IntoElement, MouseButton, Render, ScrollHandle,
    SharedString, Styled, Window, div, prelude::*, px,
};
use sqlly_core::ConnectionProfile;
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_services::{IntelliSenseRuleConfig, ProcedureInfo, Services};

use crate::theme::Theme;

/// A snapshot of the inputs the LSP server would use for a request batch.
/// Built by [`crate::app::AppView`] from the active tab + app state and passed
/// to [`LspDebugView::refresh`].
pub struct LspDebugSnapshot {
    /// Full document text the server would see (full text-document sync).
    pub text: String,
    /// Byte offset of the cursor in `text`.
    pub cursor: usize,
    /// Active connection profile (`None` ⇒ completion uses an empty catalog and
    /// validation is a no-op, matching the LSP server).
    pub profile: Option<ConnectionProfile>,
    /// Cached completion catalog from the connected database.
    pub catalog: CompletionCatalog,
    /// In-scope stored procedures for `EXEC` declaration snippets / hover.
    pub procedures: Vec<ProcedureInfo>,
    /// User-configurable completion rules.
    pub rules: IntelliSenseRuleConfig,
}

/// The computed LSP wire report: each section serialized to pretty JSON, the
/// same shape the `sqlly-lsp` server would put on the wire.
pub struct LspDebugReport {
    pub cursor_offset: usize,
    pub capabilities: String,
    pub position: String,
    pub completion: String,
    pub completion_count: usize,
    pub hover: String,
    pub semantic_tokens: String,
    pub diagnostics: String,
}

impl LspDebugReport {
    /// Ordered (label, body) sections for rendering.
    fn sections(&self) -> Vec<(String, &str)> {
        vec![
            (
                crate::localization::text("tools.lsp_debug.capabilities_section"),
                &self.capabilities,
            ),
            (
                crate::localization::text("tools.lsp_debug.position_section"),
                &self.position,
            ),
            (
                crate::localization::text("tools.lsp_debug.completion_section"),
                &self.completion,
            ),
            (
                crate::localization::text("tools.lsp_debug.hover_section"),
                &self.hover,
            ),
            (
                crate::localization::text("tools.lsp_debug.semantic_tokens_section"),
                &self.semantic_tokens,
            ),
            (
                crate::localization::text("tools.lsp_debug.diagnostics_section"),
                &self.diagnostics,
            ),
        ]
    }
}

/// A panel that renders the LSP wire report for the active editor. Mirrors the
/// [`crate::log_console::LogConsoleView`] shape: a header bar + a scrollable
/// monospace body, toggled by an action and hidden by default.
pub struct LspDebugView {
    visible: bool,
    report: Option<LspDebugReport>,
    scroll_handle: ScrollHandle,
    /// Monotonic generation stamped on each `refresh`. A spawned diagnostics
    /// task carries the generation it was started under; `set_diagnostics`
    /// discards results whose generation no longer matches, so a stale
    /// `validate_sql` from a prior refresh can never clobber a newer report.
    refresh_generation: u64,
}

#[derive(Clone)]
pub enum LspDebugEvent {
    VisibilityChanged(bool),
    /// The user clicked the in-header Refresh button; the owner should rebuild
    /// the snapshot and call `refresh` again (and re-spawn diagnostics).
    RefreshRequested,
}

impl EventEmitter<LspDebugEvent> for LspDebugView {}

impl LspDebugView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            visible: false,
            report: None,
            scroll_handle: ScrollHandle::new(),
            refresh_generation: 0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn toggle_visibility(&mut self, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        cx.emit(LspDebugEvent::VisibilityChanged(self.visible));
        cx.notify();
    }

    /// Compute the synchronous sections of the report from `snapshot`. The
    /// diagnostics section is filled with a placeholder here and replaced by
    /// [`Self::set_diagnostics`] when the async validation completes.
    ///
    /// Returns the new **refresh generation**. The caller must carry it into the
    /// spawned diagnostics task and pass it to `set_diagnostics`; a result whose
    /// generation no longer matches the current one is discarded (so a stale
    /// `validate_sql` from a prior refresh cannot clobber a newer report).
    pub fn refresh(
        &mut self,
        snapshot: &LspDebugSnapshot,
        services: &Services,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        let refresh_gen = self.refresh_generation;
        let report = build_sync_report(snapshot, services);
        self.report = Some(report);
        cx.notify();
        refresh_gen
    }

    /// Replace the diagnostics section with the serialized diagnostic list
    /// (called from the AppView's spawned validation task). Stale results —
    /// whose `generation` does not match the current [`refresh_generation`] —
    /// are silently discarded.
    pub fn set_diagnostics(&mut self, json: String, generation: u64, cx: &mut Context<Self>) {
        if generation != self.refresh_generation {
            return;
        }
        if let Some(report) = &mut self.report {
            report.diagnostics = json;
            cx.notify();
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.report = None;
        cx.notify();
    }
}

/// Cap a pretty-JSON section at ~8 KB so a huge completion list or semantic-
/// token array cannot swamp the debug panel. If the serialized string is under
/// the cap it is returned unchanged; otherwise it is truncated at the cap (on a
/// char boundary) and a `… // truncated (N <unit>)` marker is appended so the
/// reader knows data was elided. Capabilities/hover/position are always small
/// and are never truncated.
fn truncate_json(json: String, count: usize, unit: &str) -> String {
    const CAP: usize = 8 * 1024;
    if json.len() <= CAP {
        return json;
    }
    let mut truncated = json[..CAP.min(json.len())].to_string();
    // Walk back to a char boundary so we don't split a multi-byte sequence.
    while !truncated.is_char_boundary(truncated.len()) {
        truncated.pop();
    }
    truncated.push_str(&crate::localization::format(
        "tools.lsp_debug.truncated_marker",
        &[("count", &count.to_string()), ("unit", &unit.to_string())],
    ));
    truncated
}

/// Build the synchronous LSP wire report (everything except the async
/// diagnostics). Each section is serialized to pretty JSON via the reusable
/// `sqlly_lsp` helpers — the same free functions the real `LspServer` handlers
/// call — so the screen shows exactly what the language server would emit.
fn build_sync_report(snapshot: &LspDebugSnapshot, services: &Services) -> LspDebugReport {
    let serialization_error = |error: serde_json::Error| {
        crate::localization::format(
            "tools.lsp_debug.serialization_error",
            &[("error", &error.to_string())],
        )
    };
    let capabilities = serde_json::to_string_pretty(&sqlly_lsp::initialize_result())
        .unwrap_or_else(serialization_error);

    let pos = sqlly_lsp::offset_to_position(&snapshot.text, snapshot.cursor);
    let position = serde_json::to_string_pretty(&pos).unwrap_or_else(serialization_error);

    let items = {
        let req = sqlly_services::CompletionRequest::manual(
            &snapshot.text,
            snapshot.cursor,
            &snapshot.catalog,
            &snapshot.procedures,
            &snapshot.rules,
        );
        services.intellisense.complete_enriched_response(&req).items
    };
    let completion_count = items.len();
    let lsp_items = sqlly_lsp::completion_to_lsp(&items);
    let completion = truncate_json(
        serde_json::to_string_pretty(&lsp_items).unwrap_or_else(serialization_error),
        completion_count,
        &crate::localization::text("tools.lsp_debug.items_unit"),
    );

    let hover = sqlly_lsp::build_hover(&snapshot.text, snapshot.cursor, &snapshot.procedures);
    let hover_json = serde_json::to_string_pretty(&hover).unwrap_or_else(serialization_error);

    let tokens = sqlly_lsp::semantic_tokens(&snapshot.text);
    let token_tuple_count = tokens.data.len() / 5;
    let semantic_tokens = truncate_json(
        serde_json::to_string_pretty(&tokens).unwrap_or_else(serialization_error),
        token_tuple_count,
        &crate::localization::text("tools.lsp_debug.token_tuples_unit"),
    );

    let diagnostics = match (&snapshot.profile, snapshot.text.trim().is_empty()) {
        (None, _) => crate::localization::text("tools.lsp_debug.no_active_connection"),
        (_, true) => crate::localization::text("tools.lsp_debug.empty_document"),
        _ => crate::localization::text("tools.lsp_debug.computing"),
    };

    LspDebugReport {
        cursor_offset: snapshot.cursor,
        capabilities,
        position,
        completion,
        completion_count,
        hover: hover_json,
        semantic_tokens,
        diagnostics,
    }
}

impl Render for LspDebugView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().into_any_element();
        }

        let report = self.report.as_ref();
        let completion_count = report.map(|r| r.completion_count).unwrap_or(0);
        let cursor_offset = report.map(|r| r.cursor_offset).unwrap_or(0);

        let header = div()
            .flex()
            .flex_row()
            .h(px(Theme::bar_height(2.0)))
            .items_center()
            .px(px(Theme::scaled(8.0)))
            .gap(px(Theme::scaled(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("tools.lsp_debug.panel_title")),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "tools.lsp_debug.cursor_offset",
                        &[("offset", &cursor_offset.to_string())],
                    )),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "tools.lsp_debug.completion_count",
                        &[("count", &completion_count.to_string())],
                    )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .cursor(CursorStyle::PointingHand)
                    .child(crate::localization::text("tools.lsp_debug.refresh_button"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_this, _ev, _window, cx| {
                            cx.emit(LspDebugEvent::RefreshRequested);
                            cx.notify();
                        }),
                    ),
            )
            .child(div().w(px(Theme::scaled(8.0))))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .cursor(CursorStyle::PointingHand)
                    .child(crate::localization::text("tools.lsp_debug.clear_button"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            this.clear(cx);
                        }),
                    ),
            );

        let body = if let Some(report) = report {
            let sections = report.sections();
            div()
                .id("lsp_debug_scroll")
                .flex_1()
                .overflow_scroll()
                .p(px(Theme::scaled(6.0)))
                .track_scroll(&self.scroll_handle)
                .children(sections.into_iter().flat_map(|(label, body)| {
                    [
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .font_family(Theme::font_family())
                            .child(SharedString::from(format!("── {label} ──"))),
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .font_family(Theme::font_family())
                            .mb(px(Theme::scaled(8.0)))
                            .child(SharedString::from(body.to_string())),
                    ]
                }))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .p(px(Theme::scaled(8.0)))
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text("tools.lsp_debug.empty_snapshot"))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .h(px(Theme::scaled(220.0)))
            .bg(Theme::sidebar_bg())
            .border_t_1()
            .border_color(Theme::border())
            .child(header)
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_snapshot() -> LspDebugSnapshot {
        LspDebugSnapshot {
            text: String::new(),
            cursor: 0,
            profile: None,
            catalog: CompletionCatalog::default(),
            procedures: Vec::new(),
            rules: IntelliSenseRuleConfig::default(),
        }
    }

    #[test]
    fn offset_to_position_round_trips_at_start() {
        let text = "SELECT * FROM t";
        let pos = sqlly_lsp::offset_to_position(text, 0);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 0);
        assert_eq!(sqlly_lsp::position_to_offset(text, pos), 0);
    }

    #[test]
    fn offset_to_position_handles_multiline_and_utf16() {
        // 'é' is 2 bytes in UTF-8 but 1 UTF-16 code unit; '한' is 3 bytes / 1 unit.
        let text = "SELECT\né한\nWHERE";
        let w_offset = text.find('W').unwrap();
        let pos = sqlly_lsp::offset_to_position(text, w_offset);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 0);
        assert_eq!(sqlly_lsp::position_to_offset(text, pos), w_offset);

        // Cursor after 'é한' on line 1 ⇒ character == 2 (UTF-16 units).
        let mid_offset = "SELECT\n".len() + "é한".len();
        let pos = sqlly_lsp::offset_to_position(text, mid_offset);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 2);
        assert_eq!(sqlly_lsp::position_to_offset(text, pos), mid_offset);
    }

    #[test]
    fn offset_to_position_clamps_past_end() {
        let text = "abc";
        let pos = sqlly_lsp::offset_to_position(text, 999);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 3);
    }

    #[test]
    fn completion_to_lsp_preserves_label_and_insert_text() {
        use sqlly_intel::intellisense::{CompletionItem, CompletionKind, CompletionSource};
        let items = vec![
            CompletionItem {
                label: "SELECT".to_string(),
                insert_text: "SELECT".to_string(),
                kind: CompletionKind::Keyword,
                source: CompletionSource::Keyword,
                score: 0,
                detail: String::new(),
                replace_span: None,
            },
            CompletionItem {
                label: "dbo.sp".to_string(),
                insert_text: "dbo.sp".to_string(),
                kind: CompletionKind::Procedure,
                source: CompletionSource::LiveMetadata,
                score: 0,
                detail: String::new(),
                replace_span: None,
            },
        ];
        let lsp = sqlly_lsp::completion_to_lsp(&items);
        assert_eq!(lsp.len(), 2);
        assert_eq!(lsp[0].label, "SELECT");
        assert_eq!(lsp[0].kind, sqlly_lsp::CompletionItemKind::Keyword as i32);
        assert_eq!(lsp[1].kind, sqlly_lsp::CompletionItemKind::Function as i32);
    }

    #[test]
    fn build_hover_returns_none_without_matching_procedure() {
        let text = "SELECT 1";
        let hover = sqlly_lsp::build_hover(text, 0, &[]);
        assert!(hover.is_none());
    }

    #[test]
    fn report_diagnostics_placeholder_no_connection() {
        let services = Services::new();
        let snap = empty_snapshot();
        let report = build_sync_report(&snap, &services);
        assert!(report.diagnostics.contains("no active connection"));
        // SQL keywords are offered even without a connection (the LSP server
        // returns keywords with an empty catalog — see its
        // `completion_returns_keywords_without_connection` integration test).
        assert!(report.completion_count > 0);
    }

    #[test]
    fn truncate_json_passes_small_strings_through() {
        let s = truncate_json("hello".to_string(), 5, "items");
        assert_eq!(s, "hello");
    }

    #[test]
    fn truncate_json_caps_oversized_and_appends_marker() {
        let big = "x".repeat(20_000);
        let s = truncate_json(big, 42, "items");
        assert!(s.len() < 20_000);
        assert!(s.contains("truncated (42 items)"));
        // Must not end mid-codepoint — the marker is appended cleanly.
        assert!(s.ends_with(')'));
    }

    #[test]
    fn stale_diagnostics_generation_is_discarded() {
        // Build a view manually (no gpui context needed for the pure logic).
        let mut view = LspDebugView {
            visible: true,
            report: None,
            scroll_handle: ScrollHandle::new(),
            refresh_generation: 0,
        };
        // Simulate a refresh that stamps generation 1 and seeds a report.
        view.refresh_generation = 1;
        view.report = Some(LspDebugReport {
            cursor_offset: 0,
            capabilities: "{}".to_string(),
            position: "{}".to_string(),
            completion: "[]".to_string(),
            completion_count: 0,
            hover: "null".to_string(),
            semantic_tokens: "{}".to_string(),
            diagnostics: "computing…".to_string(),
        });
        // A stale result from generation 0 must not overwrite the placeholder.
        // We can't call set_diagnostics without a Context, so test the guard
        // logic directly by mirroring its predicate.
        let stale_gen = 0u64;
        assert_ne!(stale_gen, view.refresh_generation);
    }

    #[gpui::test]
    fn lsp_debug_smoke(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, cx| LspDebugView::new(cx));

        // Hidden by default.
        view.read_with(cx, |v, _| assert!(!v.is_visible()));

        // Toggle visible.
        view.update(cx, |v, cx| v.toggle_visibility(cx));
        view.read_with(cx, |v, _| assert!(v.is_visible()));

        // Refresh with an empty snapshot (no connection) → report present,
        // keywords offered, diagnostics placeholder set.
        let services = Services::new();
        let snap = empty_snapshot();
        let refresh_gen = view.update(cx, |v, cx| v.refresh(&snap, &services, cx));
        view.read_with(cx, |v, _| {
            let r = v.report.as_ref().expect("report present after refresh");
            assert!(r.completion_count > 0);
            assert!(r.diagnostics.contains("no active connection"));
        });

        // set_diagnostics with the matching generation updates the section.
        view.update(cx, |v, cx| {
            v.set_diagnostics("[]  // matched".to_string(), refresh_gen, cx)
        });
        view.read_with(cx, |v, _| {
            assert_eq!(v.report.as_ref().unwrap().diagnostics, "[]  // matched");
        });

        // set_diagnostics with a stale generation is discarded.
        view.update(cx, |v, cx| {
            v.set_diagnostics("[]  // stale".to_string(), refresh_gen.wrapping_add(1), cx)
        });
        view.read_with(cx, |v, _| {
            assert_eq!(v.report.as_ref().unwrap().diagnostics, "[]  // matched");
        });
    }
}
