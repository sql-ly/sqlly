//! Window chrome: n-level sidebar tree, pane dispatch, sidebar filter, root
//! render.
//!
//! The dialog owns its whole layout: a searchable n-level tree on the left, a
//! single pane's content on the right. It deliberately does *not* use the
//! gpui-component `Settings` element — that component is hard-wired to a
//! two-level page/group sidebar, renders a whole page's groups in a
//! virtualized `list` and "navigates" by scroll-to-item inside it (which
//! misses under virtualization), and can only search item titles/keywords, so
//! none of: deep hierarchy, per-node content, content search with in-place
//! highlighting, or restored scroll positions can be built on top of it.
//! The panes themselves still use gpui-component widgets (Input, Checkbox,
//! Select).

use super::*;
use crate::dialog::Dialog;
use crate::preferences::flatten_tree;
use crate::scrollbars::DraggableScrollbar;

impl PreferencesWindow {
    /// One pane's body, dispatched from its tree leaf. Also runs the pane's
    /// on-open side effects (lazy model loads) exactly like the old
    /// `select_pane` did. Called both for the visible pane and — with the
    /// text collector armed — by the search index pass over every pane.
    pub(super) fn render_pane_body(
        &mut self,
        pane: &PreferencesPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use PreferencesPane as P;
        match pane {
            P::Themes => self.render_themes(window, cx),
            P::Display => self.render_display(window, cx),
            P::Language => self.render_language(window, cx),
            P::EditorGeneral => self.render_editor_general(window, cx),
            P::EditorTabs => self.render_editor_tabs(cx),
            P::EditorFormatting => self.render_sql_formatting(window, cx),
            P::EditorIntelliSense => self.render_editor_intellisense(window, cx),
            P::EditorQueryHistory => self.render_query_history(cx),
            P::EditorExternalEditors => self.render_editor_external_editors(window, cx),
            P::EditorSnippets => self.render_editor_snippets(window, cx),
            P::DdlNavigation => {
                // Monitors can be plugged/unplugged while the app runs;
                // refresh the cached list when the pane renders empty.
                if self.monitors.is_empty() {
                    self.monitors = crate::ddl_navigation::detect_monitors(cx);
                }
                self.render_ddl_navigation(window, cx)
            }
            P::AiOnDevice => self.render_ai_on_device(cx),
            P::AiFim => self.render_ai_fim(window, cx),
            P::AiCloud => self.render_ai_cloud(window, cx),
            P::AiDecisionModel => self.render_ai_decision_model(cx),
            P::AiCodingAgentCli => self.render_ai_coding_agent_cli(window, cx),
            #[cfg(not(target_arch = "wasm32"))]
            P::AiMcp => div().child(self.mcp_settings.clone()).into_any_element(),
            #[cfg(target_arch = "wasm32")]
            P::AiMcp => self.hint(&crate::browser_preview::unavailable(
                &crate::localization::text("settings.layout.ai_mcp.feature_unavailable"),
            )),
            P::AiPlayground => self.render_ai_playground(),
            P::ResultsGeneral => self.render_results_general(window, cx),
            P::ResultsHistory => self.render_results_history(window, cx),
            P::ResultsDate => self.render_results_date(window, cx),
            P::ResultsBool => self.render_results_bool(window, cx),
            P::ResultsUuid => self.render_results_uuid(window, cx),
            P::ResultsInteger => self.render_results_integer(window, cx),
            P::ResultsDecimal => self.render_results_decimal(window, cx),
            P::ResultsOverrides => self.render_results_overrides(window, cx),
            P::ResultsMasking => self.render_results_masking(window, cx),
            // Export and its file-format options were separate groups on the
            // component's Results page; they share the Export pane here.
            P::ResultsExport => self.pane_container(vec![
                self.render_results_export(window, cx),
                self.section_header(&crate::localization::text(
                    "settings.layout.section_header.file_export_options",
                )),
                self.render_results_file_export(window, cx),
            ]),
            P::ResultsProtobufSchemas => self.render_protobuf_schemas(cx),
            P::Performance => self.render_performance(cx),
            P::Gestures => self.render_gestures(window, cx),
            P::DataModelServerExplorer => self.render_data_model(cx),
            P::IntelliSenseCache => self.render_intellisense_cache(window, cx),
            P::WorkspaceQueryResume => self.render_query_resume(window, cx),
            P::ScheduledDumps => self.render_scheduled_dumps(window, cx),
            P::WorkspaceICloudSync => self.render_icloud_sync(cx),
            P::Keybindings => {
                self.ensure_keybindings_model();
                self.render_keybindings(cx)
            }
            P::Advanced => self.render_advanced(window, cx),
            P::DebugLogging => self.render_debug_logging(window, cx),
        }
    }

    /// The visible rows of the sidebar tree. While a search is active every
    /// folder is treated as expanded, so all matches are visible; otherwise
    /// the user's expansion state applies.
    fn sidebar_rows(&self) -> Vec<(usize, String, String, Option<PreferencesPane>)> {
        let searching = !self.search_query.trim().is_empty();
        if searching {
            let mut all = HashSet::new();
            fn collect_folders(node: &PreferencesTreeNode, out: &mut HashSet<String>) {
                if node.is_folder() {
                    out.insert(node.id.clone());
                }
                for child in &node.children {
                    collect_folders(child, out);
                }
            }
            for node in &self.filtered_tree {
                collect_folders(node, &mut all);
            }
            flatten_tree(&self.filtered_tree, &all)
        } else {
            flatten_tree(&self.filtered_tree, &self.expanded)
        }
    }

    /// Sidebar rows. Rendered as *direct* children of the scroll container so
    /// `ScrollHandle::scroll_to_item` can reveal the selected row exactly —
    /// this is what makes "clear the search and the selection scrolls back
    /// into view" reliable.
    fn render_sidebar_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let searching = !self.search_query.trim().is_empty();
        let flat = self.sidebar_rows();

        if self.sidebar_reveal_pending.get() {
            self.sidebar_reveal_pending.set(false);
            if let Some(ix) = flat
                .iter()
                .position(|(_, _, _, pane)| pane.as_ref() == Some(&self.selected_pane))
            {
                self.sidebar_scroll.scroll_to_item(ix);
            }
        }

        flat.into_iter()
            .enumerate()
            .map(|(row_idx, (depth, id, title, pane))| {
                let indent = px(Theme::scaled(8. + depth as f32 * 16.));
                let is_folder = pane.is_none();
                let is_selected = pane.as_ref() == Some(&self.selected_pane);
                let is_expanded = is_folder && (searching || self.expanded.contains(&id));

                let id_for_handler = id.clone();
                let pane_for_handler = pane.clone();

                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(4.)))
                    .py(px(Theme::scaled(4.)))
                    .px(px(Theme::scaled(8.)))
                    .ml(indent)
                    .mr(px(Theme::scaled(4.)))
                    .rounded(px(Theme::scaled(4.)))
                    .cursor_pointer()
                    .when(is_selected, |d| d.bg(Theme::accent().alpha(0.18)))
                    .id(("pref_node", row_idx))
                    .on_mouse_down(gpui::MouseButton::Left, {
                        let id = id_for_handler;
                        let pane = pane_for_handler;
                        cx.listener(move |this, _ev, _window, cx| {
                            if let Some(p) = pane.as_ref() {
                                this.select_pane(p.clone(), cx);
                            } else if this.search_query.trim().is_empty() {
                                // While searching, folders are force-expanded;
                                // a toggle would appear to do nothing.
                                this.toggle_expand(&id, cx);
                            }
                        })
                    })
                    .child(
                        // The glyph is small; the hit target comes from the
                        // row, not from inflating the type.
                        div()
                            .w(px(Theme::scaled(16.)))
                            .flex_none()
                            // Fixed glyph: the icon column keeps one width.
                            .text_size(px(Theme::scaled(10.)))
                            .line_height(px(Theme::scaled(13.)))
                            .flex()
                            .items_center()
                            .text_color(Theme::text_dim())
                            .when(is_folder, |d| {
                                d.child(if is_expanded { "▾" } else { "▸" })
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(if is_selected {
                                Theme::accent()
                            } else {
                                Theme::text()
                            })
                            .font_weight(if is_folder {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .child(title.clone()),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Breadcrumb above the pane body: folder path plus the pane's own title.
    fn render_breadcrumb(&self) -> AnyElement {
        let mut parts =
            preferences::tree_path_to(&self.tree, &self.selected_pane).unwrap_or_default();
        parts.push(self.selected_pane.display_title().to_string());

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(6.)))
            .px(px(Theme::scaled(20.)))
            .h(px(Theme::scaled(38.)))
            .flex_none()
            .border_b_1()
            .border_color(Theme::border());
        let last = parts.len().saturating_sub(1);
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                row = row.child(
                    div()
                        .text_color(Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .child("›"),
                );
            }
            row = row.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .when(i == last, |d| {
                        d.font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                    })
                    .when(i != last, |d| d.text_color(Theme::text_dim()))
                    .child(part),
            );
        }
        row.into_any_element()
    }
}

impl Render for PreferencesWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Claim focus on first paint. Without this the dialog opens with focus
        // still on whatever was behind it, so ⌘, then Esc would type an escape
        // into the SQL editor instead of closing Preferences.
        if !self.did_initial_focus {
            self.did_initial_focus = true;
            self.previous_focus = window.focused(cx);
            window.focus(&self.focus_handle, cx);
        }

        // Per-frame search bookkeeping: only the first match may carry the
        // scroll-to marker, and a pending query (re)filters the tree here,
        // where the window needed by the index pass is available. This may
        // switch `selected_pane` to the first matching pane.
        self.pane_match_claimed.set(false);
        self.refresh_search(window, cx);

        let sidebar_rows = self.render_sidebar_rows(cx);
        let breadcrumb = self.render_breadcrumb();
        let pane_body = self.render_pane_body(&self.selected_pane.clone(), window, cx);

        // Right-pane scrollbar geometry: the track is ALWAYS visible; the
        // thumb only renders when the content actually overflows (a track
        // with no thumb reads as a disabled scrollbar).
        let scrollbar_w = 15.0_f32;
        let viewport_h = f32::from(self.pane_scroll.bounds().size.height);
        let max_scroll = f32::from(self.pane_scroll.max_offset().y);
        let offset_y = f32::from(self.pane_scroll.offset().y);
        let (pane_scrollable, thumb_h, thumb_y) = if max_scroll > 0.0 && viewport_h > 0.0 {
            let content_total = max_scroll + viewport_h;
            let thumb_h = ((viewport_h * viewport_h) / content_total).max(24.0);
            let pct = (-offset_y / max_scroll).clamp(0.0, 1.0);
            let thumb_y = pct * (viewport_h - thumb_h);
            (true, thumb_h, thumb_y)
        } else {
            (false, 0.0, 0.0)
        };

        let focus_handle = self.focus_handle.clone();

        // Sidebar: search on top, tree below.
        let sidebar = div()
            .w(px(Theme::scaled(250.)))
            .h_full()
            .flex_none()
            .bg(Theme::sidebar_bg())
            .border_r_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .child(
                div()
                    .p(px(Theme::scaled(12.)))
                    .border_b_1()
                    .border_color(Theme::border())
                    // The search text routing lives in this input's Change
                    // subscription (see the constructor).
                    .child(self.search_box()),
            )
            .child(
                div()
                    .flex_1()
                    .py(px(Theme::scaled(4.)))
                    .id("prefs_sidebar_scroll")
                    .overflow_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .children(sidebar_rows),
            );

        // Detail: breadcrumb header + scrollable body.
        let detail = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(breadcrumb)
            .child(
                div()
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w_full()
                            .h_full()
                            .id("prefs_pane_scroll")
                            .overflow_scroll()
                            .track_scroll(&self.pane_scroll)
                            .child(
                                div()
                                    .p(px(Theme::scaled(20.)))
                                    .pr(px(Theme::scaled(20. + scrollbar_w)))
                                    .child(pane_body),
                            ),
                    )
                    .child(
                        // Scrollbar track + thumb. Pressing the track jumps
                        // there; holding drags.
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(scrollbar_w))
                            .h_full()
                            .bg(Theme::sidebar_bg().alpha(0.5))
                            .when(pane_scrollable, |track| {
                                track
                                    .scrollbar_track(&self.pane_scroll, gpui::Axis::Vertical)
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(thumb_y))
                                            .right_0()
                                            .w(px(scrollbar_w))
                                            .h(px(thumb_h))
                                            .rounded(px(Theme::scaled(4.)))
                                            .bg(Theme::text_dim().opacity(0.5)),
                                    )
                            }),
                    ),
            );

        // Sidebar + detail. The unified chrome owns the scrim, card, title
        // bar, and footer; this two-column split manages its own scrolling
        // and edge-to-edge layout, hence `.bare_body()` below.
        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            // Keeps a scrollbar drag tracking once the pointer leaves the
            // narrow track.
            .scrollbar_drag_area()
            .child(sidebar)
            .child(detail);

        Dialog::new("prefs_dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("settings.layout.title.settings"))
            .easy_close(true)
            // The dialog holds the window's focus, so every key the
            // Preferences window responds to arrives at the chrome's card.
            .track_focus(&focus_handle)
            .key_context(PREFS_KEY_CONTEXT)
            .bare_body()
            .on_close(|this, window, cx| this.close(window, cx))
            .child(body)
            .build(self, window, cx)
            // Esc goes through the action system, not `on_key_down`: the
            // global `escape` -> `CancelTabSwitcher` binding in main.rs
            // consumes the keystroke before any raw listener runs, so the
            // scoped binding dispatches `ClosePreferences` instead. The
            // listener rides on the chrome's overlay, an ancestor of the
            // focused card, so it stays on the dispatch path. See
            // `PREFS_KEY_CONTEXT`.
            .on_action(cx.listener(|this, _: &ClosePreferences, window, cx| {
                this.close(window, cx);
            }))
            // Raw key-down still carries the keybinding-capture flow, which
            // needs the literal keystroke rather than an action. Bound keys
            // never reach it (bindings win); unbound keystrokes bubble past
            // the card's chrome handler (which only acts on escape/enter)
            // up to this listener.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.keybinding_capture.is_some() {
                    this.handle_keybinding_key_down(event, window, cx);
                    cx.stop_propagation();
                }
            }))
    }
}
