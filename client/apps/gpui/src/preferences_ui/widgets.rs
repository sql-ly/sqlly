//! Shared render scaffolding: headings, rows, checkboxes, steppers, dropdowns.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ---- shared render helpers (mirror the Swift System Settings style) ----

    /// Widest a prose column gets. Hints used to have no cap at all, so the
    /// Gestures pane's ~460-character paragraph ran the full width of the pane
    /// at 12px — the one place a measure limit was actually needed was the one
    /// place it was missing. `intro` had `max_w(620)` on a ~587px pane, where it
    /// could never bind.
    pub(super) const PROSE_MAX_W: f32 = 520.;

    /// Formerly the pane's in-body title. Since the migration to the
    /// gpui-component `Settings` element, the pane name is supplied by the
    /// `SettingPage` / `SettingGroup` title in the sidebar and group header, so
    /// an in-body heading would duplicate it. Kept as a no-op (rather than
    /// deleting the ~22 call sites) so each pane body reads unchanged.
    pub(super) fn heading(&self, _text: &str) -> AnyElement {
        div().into_any_element()
    }

    pub(super) fn intro(&self, text: &str) -> AnyElement {
        div()
            .text_color(Theme::text_dim())
            .text_size(px(Theme::font_size()))
            .pb(px(Theme::scaled(16.)))
            .max_w(px(Theme::scaled(Self::PROSE_MAX_W)))
            .child(self.match_text(text))
            .into_any_element()
    }

    /// A section header owns the group beneath it, so it sits far from the
    /// previous group and close to its own children. The pane's uniform 4px gap
    /// used to put it 4px below the last control and 8px above its first — the
    /// grouping read backwards, which is why the long panes scanned as one
    /// undifferentiated wall. The big `pt` here is what buys the separation.
    pub(super) fn section_header(&self, title: &str) -> AnyElement {
        div()
            .text_color(Theme::text())
            .text_size(px(Theme::font_size()))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .pt(px(Theme::scaled(20.)))
            .pb(px(Theme::scaled(2.)))
            .child(self.match_text(title))
            .into_any_element()
    }

    pub(super) fn hint(&self, text: &str) -> AnyElement {
        div()
            .text_color(Theme::text_dim())
            .text_size(px(Theme::font_size_ratio(0.92)))
            .max_w(px(Theme::scaled(Self::PROSE_MAX_W)))
            .pb(px(Theme::scaled(2.)))
            .child(self.match_text(text))
            .into_any_element()
    }

    pub(super) fn warning(&self, text: &str) -> AnyElement {
        div()
            .text_color(Theme::warning())
            .text_size(px(Theme::font_size_ratio(0.92)))
            .max_w(px(Theme::scaled(Self::PROSE_MAX_W)))
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(self.match_text(text))
            .into_any_element()
    }

    pub(super) fn pane_container(&self, children: Vec<AnyElement>) -> AnyElement {
        // No outer padding: each pane body is now the single item of a
        // gpui-component `SettingGroup` `GroupBox`, which supplies the frame
        // and inset. The 4px inter-row gap is the pane's own responsibility.
        let mut c = div().flex().flex_col().gap(px(Theme::scaled(4.)));
        for ch in children {
            c = c.child(ch);
        }
        c.into_any_element()
    }

    /// A bordered, unfilled button.
    ///
    /// DESIGN.md allows the accent at most one primary action per view, and the
    /// dialog's Done already claims it. This pane used to paint an accent fill
    /// on "Fetch Models" once per provider, so a screen with one real primary
    /// showed four — which is the same as showing none, because nothing stood
    /// out. Everything inside a pane is secondary; the accent shows up here only
    /// as the focus ring.
    /// `id` is an `ElementId` rather than `&'static str` because per-row buttons
    /// (one Remove per column override) need ids derived from runtime data, and
    /// sibling elements sharing an id is a GPUI bug, not a style issue.
    pub(super) fn quiet_button<F>(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &str,
        cx: &mut Context<Self>,
        handler: F,
    ) -> AnyElement
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        // Shared because the mouse and keyboard paths both need it, and a
        // button that only answers to one of them is the bug this helper exists
        // to stop repeating.
        let handler = std::rc::Rc::new(handler);
        let on_click = handler.clone();
        div()
            .id(id)
            .px(px(Theme::scaled(12.)))
            .py(px(Theme::scaled(4.)))
            .my(px(Theme::scaled(2.)))
            .border_1()
            .border_color(Theme::border())
            .rounded(px(Theme::scaled(4.)))
            .bg(Theme::bg())
            .text_color(Theme::text())
            .text_size(px(Theme::font_size()))
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            // Sized to its label: callers put it in a `flex_row`, which is what
            // keeps it from stretching across the pane.
            .flex_none()
            .hover(|s| s.border_color(Theme::accent()))
            .child(self.match_text(label))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, window, cx| {
                    cx.stop_propagation();
                    on_click(this, window, cx);
                }),
            )
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "space" | " " | "enter" => handler(this, window, cx),
                    _ => {}
                }
            }))
            .focusable()
            .tab_stop(true)
            .focus(|s| s.border_color(Theme::accent()))
            .into_any_element()
    }

    /// A non-interactive value box used for sample previews (mirrors the
    /// dropdown's frame but reads only).
    pub(super) fn read_only_value(&self, text: &str) -> AnyElement {
        div()
            .min_w(px(Theme::scaled(220.)))
            .h(px(Theme::scaled(24.)))
            .px(px(Theme::scaled(8.)))
            .flex()
            .items_center()
            .border_1()
            .border_color(Theme::border())
            .rounded(px(Theme::scaled(4.)))
            .bg(Theme::panel_bg())
            .text_color(Theme::text())
            .text_size(px(Theme::font_size()))
            .child(self.match_text(text))
            .into_any_element()
    }

    /// A small bold sub-label that introduces a group of related controls
    /// within a section (e.g. the rows of the Organization section).
    pub(super) fn label_row(&self, text: &str) -> AnyElement {
        div()
            .pt(px(Theme::scaled(10.)))
            .pb(px(Theme::scaled(2.)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Theme::text())
            .child(self.match_text(text))
            .into_any_element()
    }

    /// A labeled text input plus, when the value does not parse, the complaint
    /// about it — rendered under the control it belongs to, indented to line up
    /// with it rather than with the label. Errors shown anywhere else make the
    /// user hunt for which of five numeric fields they fumbled.
    pub(super) fn input_row(&self, label: &str, id: &'static str) -> AnyElement {
        let input = self.prefs_input(id);
        let mut col = div()
            .flex()
            .flex_col()
            .child(self.labeled_row(label, input));
        if let Some(message) = self.field_errors.get(id) {
            col = col.child(
                div()
                    // Aligns the message with the input, not the label.
                    .pl(px(crate::form_controls::control_column_inset()))
                    .pb(px(Theme::scaled(4.)))
                    .child(self.warning(message)),
            );
        }
        col.into_any_element()
    }

    pub(super) fn labeled_row(&self, label: &str, control: AnyElement) -> AnyElement {
        crate::form_controls::labeled_row(self.match_text(label), control)
    }

    /// The `Input` element for a prefs text field created in the constructor.
    /// The field's value routing lives in its `InputEvent::Change`
    /// subscription (see the spec table in [`Self::new`]), so render sites
    /// only name the id.
    pub(super) fn prefs_input(&self, id: &'static str) -> AnyElement {
        let Some(state) = self.inputs.get(id) else {
            // Every id rendered is registered in the constructor; an unknown
            // id is a programming error, but an empty cell beats a panic in a
            // settings dialog.
            return div().into_any_element();
        };
        // `field` pins the themed text color: `Input` paints its value with
        // the inherited color, which defaults to black.
        let field = crate::form_controls::field(state.element());
        match self.input_value(id) {
            Some(value) => self.match_field(&value, field),
            None => field.into_any_element(),
        }
    }

    /// The sidebar search field, with an × that clears it. Clearing edits
    /// the text like a keystroke does, so the field's Change subscription
    /// restores the full tree.
    pub(super) fn search_box(&self) -> AnyElement {
        let Some(FieldInput::Single(state)) = self.inputs.get("prefs_search") else {
            return div().into_any_element();
        };
        div()
            .text_color(Theme::text())
            .child(gpui_component::input::Input::new(state).cleanable(true))
            .into_any_element()
    }

    /// `handler` is generic rather than a bare `fn` pointer so a pane can pass a
    /// closure that names its own store field inline. `fn` items still coerce,
    /// so the `Self::toggle_*` call sites are unaffected.
    pub(super) fn checkbox<F>(
        &self,
        id: &'static str,
        label: &str,
        value: bool,
        cx: &mut Context<Self>,
        handler: F,
    ) -> AnyElement
    where
        F: Fn(&mut Self, &mut Context<Self>) + 'static,
    {
        let this = cx.entity();
        // `checkbox_row` sizes the checkbox to its label (see its docs).
        // The gpui-component `Checkbox` themes itself from the global theme
        // (synced from the app palette at startup) and carries its own focus
        // and Space/Enter handling; `.tab_stop(true)` puts it in tab order.
        crate::form_controls::checkbox_row(
            Checkbox::new(id)
                .checked(value)
                // Label passed as a child (not `.label`) so search
                // matches inside it can be highlighted in place; the
                // accessibility label keeps the screen-reader name.
                .accessibility_label(label.to_string())
                .child(self.match_text(label))
                .tab_stop(true)
                .on_click(move |_new_checked: &bool, _window, app| {
                    // Borrow rather than move: `on_click` is `Fn` and may
                    // fire many times, so `handler` has to survive each
                    // call. The handlers flip the store field themselves,
                    // so the new checked state itself is not needed.
                    this.update(app, |this, cx| handler(this, cx));
                }),
        )
        .into_any_element()
    }

    /// A `checkbox` that can be disabled (greyed out, non-interactive) — used
    /// for the per-language export toggles while the Programming master
    /// switch is off.
    pub(super) fn checkbox_disabled<F>(
        &self,
        id: &'static str,
        label: &str,
        value: bool,
        disabled: bool,
        cx: &mut Context<Self>,
        handler: F,
    ) -> AnyElement
    where
        F: Fn(&mut Self, &mut Context<Self>) + 'static,
    {
        let this = cx.entity();
        crate::form_controls::checkbox_row(
            Checkbox::new(id)
                .checked(value)
                .accessibility_label(label.to_string())
                .child(self.match_text(label))
                .disabled(disabled)
                .tab_stop(!disabled)
                .on_click(move |_new_checked: &bool, _window, app| {
                    this.update(app, |this, cx| handler(this, cx));
                }),
        )
        .into_any_element()
    }

    /// A radio button row on the gpui-component `Radio` widget, which brings
    /// the focus ring, tab order, and Space/Enter toggling the old hand-built
    /// version had to spell out. The state stays controlled: `handler` writes
    /// the store field and notifies, exactly as before.
    pub(super) fn radio(
        &self,
        id: &'static str,
        label: &str,
        selected: bool,
        cx: &mut Context<Self>,
        handler: fn(&mut Self, &mut Context<Self>),
    ) -> AnyElement {
        let this = cx.entity();
        // Same reason as `checkbox`: the row wrapper keeps the widget sized to
        // its label instead of stretched across the pane.
        div()
            .flex()
            .flex_row()
            .py(px(Theme::scaled(4.)))
            .child(
                Radio::new(id)
                    .checked(selected)
                    // Label passed as a child (not `.label`) so search
                    // matches inside it can be highlighted in place; the
                    // accessibility label keeps the screen-reader name.
                    .accessibility_label(label.to_string())
                    .child(self.match_text(label))
                    .tab_stop(true)
                    .on_click(move |_new_checked: &bool, _window, app| {
                        // The handlers write the selected variant into the
                        // store themselves, so the requested value is unused.
                        this.update(app, handler);
                    }),
            )
            .into_any_element()
    }

    /// A house −/+ stepper, hand-built, so the keyboard path is spelled out
    /// by hand: these two glyph buttons are the only way to set font size,
    /// tab width, and decimal places, so leaving them mouse-only makes those
    /// settings unreachable without a pointer. Arrow keys drive it too, because
    /// that is what a stepper answers to on macOS.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stepper<Dec, Inc>(
        &self,
        id_dec: &'static str,
        id_inc: &'static str,
        value: i32,
        cx: &mut Context<Self>,
        handler_dec: Dec,
        handler_inc: Inc,
    ) -> AnyElement
    where
        Dec: Fn(&mut Self, &mut Context<Self>) + 'static,
        Inc: Fn(&mut Self, &mut Context<Self>) + 'static,
    {
        let handler_dec: StepperHandler = std::rc::Rc::new(handler_dec);
        let handler_inc: StepperHandler = std::rc::Rc::new(handler_inc);
        let button = |id: &'static str,
                      glyph: &'static str,
                      handler: StepperHandler,
                      cx: &mut Context<Self>| {
            let on_mouse = handler.clone();
            let on_key = handler;
            div()
                .w(px(Theme::scaled(24.)))
                .h(px(Theme::scaled(24.)))
                .border_1()
                .border_color(Theme::border())
                .rounded(px(Theme::scaled(3.)))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text())
                .text_size(px(Theme::font_size()))
                .child(glyph)
                .id(id)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| on_mouse(this, cx)),
                )
                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _window, cx| {
                    match ev.keystroke.key.as_str() {
                        "space" | " " | "enter" => on_key(this, cx),
                        _ => {}
                    }
                }))
                .focusable()
                .tab_stop(true)
                .focus(|s| s.border_color(Theme::accent()))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(4.)))
            .child(button(id_dec, "−", handler_dec, cx))
            .child(
                div()
                    .w(px(Theme::scaled(40.)))
                    .h(px(Theme::scaled(24.)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(Theme::text())
                    .text_size(px(Theme::font_size()))
                    .child(self.match_text(&value.to_string())),
            )
            .child(button(id_inc, "+", handler_inc, cx))
            .into_any_element()
    }

    /// A prefs text input created on first use, for per-column override fields
    /// whose ids are not known at window construction.
    pub(super) fn override_text_input<A>(
        &self,
        id: &'static str,
        value: &str,
        placeholder: String,
        apply: A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        A: Fn(&mut Self, String, &mut Context<Self>) + 'static,
    {
        let mut slots = self.override_text_slots.borrow_mut();
        if !slots.contains_key(id) {
            let apply_cell: std::rc::Rc<std::cell::RefCell<OverrideTextApply>> =
                std::rc::Rc::new(std::cell::RefCell::new(std::rc::Rc::new(apply)));
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.to_string())
            });
            let subscription = cx.subscribe_in(&state, window, {
                let apply_cell = apply_cell.clone();
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = state.read(cx).value().to_string();
                        let apply = apply_cell.borrow().clone();
                        apply(this, text, cx);
                    }
                }
            });
            slots.insert(
                id,
                OverrideTextSlot {
                    state,
                    apply: apply_cell,
                    _subscription: subscription,
                },
            );
        } else if let Some(slot) = slots.get_mut(id) {
            slot.apply.replace(std::rc::Rc::new(apply));
            if slot.state.read(cx).value() != value {
                slot.state.update(cx, |state, cx| {
                    state.set_value(value.to_string(), window, cx);
                });
            }
        }
        let state = slots.get(id).expect("slot inserted").state.clone();
        drop(slots);
        let field = crate::form_controls::field(gpui_component::input::Input::new(&state));
        self.match_field(value, field)
    }

    /// A real, selectable dropdown built on the gpui-component `Select`
    /// widget. The widget's `SelectState` owns the popup's open/close and
    /// keyboard-highlight state internally, so unlike the old stateless kit
    /// select there is no app-held open/highlight bookkeeping and no
    /// click-away overlay. Each option carries its display label, a selected
    /// marker, and an `on_select` handler that applies the choice.
    ///
    /// One `SelectState<Vec<String>>` of labels is created lazily per `id`
    /// (creation needs the window, which is why the render path threads it
    /// through). On `SelectEvent::Confirm` the chosen handler is looked up by
    /// the state's selected index, so duplicate labels stay unambiguous. Each
    /// render refreshes the slot's handler list, re-pushes the label set only
    /// when it changed, and re-asserts the store-derived selection.
    pub(super) fn select_dropdown(
        &self,
        id: &'static str,
        value: &str,
        options: Vec<DropdownOption>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.select_dropdown_with(id, value, options, None, window, cx)
    }

    /// [`select_dropdown`](Self::select_dropdown) with a search box in the
    /// menu, for lists too long to scan — the font pickers, whose options are
    /// every font installed on the machine.
    ///
    /// `search_placeholder` doubles as the opt-in: `None` is a plain dropdown.
    pub(super) fn searchable_dropdown(
        &self,
        id: &'static str,
        value: &str,
        options: Vec<DropdownOption>,
        search_placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.select_dropdown_with(id, value, options, Some(search_placeholder), window, cx)
    }

    fn select_dropdown_with(
        &self,
        id: &'static str,
        value: &str,
        options: Vec<DropdownOption>,
        search_placeholder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = value;
        let labels: Vec<String> = options.iter().map(|(label, _, _)| label.clone()).collect();
        // Option labels are pane content for search purposes (the selected
        // one is visible; the rest are one click away), so feed the index.
        for label in &labels {
            self.note_text(label);
        }
        let selected_index = options.iter().position(|(_, selected, _)| *selected);
        let handlers: Vec<DropdownHandler> = options.into_iter().map(|(_, _, on)| on).collect();

        let mut slots = self.select_slots.borrow_mut();
        if !slots.contains_key(id) {
            let handlers_cell: std::rc::Rc<std::cell::RefCell<Vec<DropdownHandler>>> =
                std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let searchable = search_placeholder.is_some();
            let state = cx.new(|cx| {
                SelectState::new(Vec::<String>::new(), None, window, cx).searchable(searchable)
            });
            let subscription = cx.subscribe_in(&state, window, {
                let handlers_cell = handlers_cell.clone();
                move |this: &mut Self, state, _event: &SelectEvent<Vec<String>>, window, cx| {
                    let Some(ix) = state.read(cx).selected_index(cx) else {
                        return;
                    };
                    // Clone out of the cell before invoking: the handler may
                    // re-render, which refreshes this very cell.
                    let handler = handlers_cell.borrow().get(ix.row).cloned();
                    if let Some(handler) = handler {
                        handler(this, window, cx);
                    }
                    cx.notify();
                }
            });
            slots.insert(
                id,
                SelectSlot {
                    state,
                    handlers: handlers_cell,
                    labels: Vec::new(),
                    _subscription: subscription,
                },
            );
        }
        let slot = slots.get_mut(id).expect("slot inserted above");
        *slot.handlers.borrow_mut() = handlers;
        if slot.labels != labels {
            slot.labels = labels.clone();
            slot.state
                .update(cx, |state, cx| state.set_items(labels, window, cx));
        }
        let desired = selected_index.map(IndexPath::new);
        if slot.state.read(cx).selected_index(cx) != desired {
            slot.state.update(cx, |state, cx| {
                state.set_selected_index(desired, window, cx)
            });
        }
        let state = slot.state.clone();
        drop(slots);

        let mut select = Select::new(&state).w_full();
        if let Some(placeholder) = search_placeholder {
            select = select.search_placeholder(placeholder);
        }
        crate::form_controls::field(select)
            .flex()
            .items_center()
            .into_any_element()
    }

    /// Build a labeled dropdown straight from an enum's variants. `label`
    /// renders each variant's display string; `apply` persists the chosen
    /// variant. Removes the per-pane option-vec boilerplate.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn enum_select<T, L, A>(
        &self,
        id: &'static str,
        current: T,
        variants: Vec<T>,
        label: L,
        apply: A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        T: PartialEq + Clone + 'static,
        L: Fn(&T) -> String,
        A: Fn(&mut Self, T, &mut Context<Self>) + Clone + 'static,
    {
        self.enum_select_in(
            id,
            current,
            variants,
            label,
            move |this, v, _window, cx| apply(this, v, cx),
            window,
            cx,
        )
    }

    /// Like [`Self::enum_select`], for `apply` logic that must push values
    /// into input widgets and therefore needs the window (e.g. a preset
    /// re-seeding the SQL numeric fields).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn enum_select_in<T, L, A>(
        &self,
        id: &'static str,
        current: T,
        variants: Vec<T>,
        label: L,
        apply: A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        T: PartialEq + Clone + 'static,
        L: Fn(&T) -> String,
        A: Fn(&mut Self, T, &mut Window, &mut Context<Self>) + Clone + 'static,
    {
        let options: Vec<DropdownOption> = variants
            .into_iter()
            .map(|v| {
                let selected = v == current;
                let lbl = label(&v);
                let apply = apply.clone();
                let on: DropdownHandler = std::rc::Rc::new(
                    move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                        apply(this, v.clone(), window, cx);
                    },
                );
                (lbl, selected, on)
            })
            .collect();
        self.select_dropdown(id, "", options, window, cx)
    }

    /// Convenience: a left/center/right alignment dropdown. `apply` writes the
    /// chosen alignment into whichever sub-format the caller targets.
    pub(super) fn alignment_select<A>(
        &self,
        id: &'static str,
        current: ResultTextAlignment,
        apply: A,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        A: Fn(&mut Self, ResultTextAlignment, &mut Context<Self>) + Clone + 'static,
    {
        self.enum_select(
            id,
            current,
            vec![
                ResultTextAlignment::Left,
                ResultTextAlignment::Center,
                ResultTextAlignment::Right,
            ],
            |a| alignment_label(a).to_string(),
            apply,
            window,
            cx,
        )
    }
}
