//! Small building blocks the analysis cell views share: labelled inputs,
//! pickers, chips, read-only grids and SVG charts.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, div, img, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;

use crate::theme::Theme;

pub(crate) fn t(key: &str) -> String {
    crate::localization::text(key)
}

pub(crate) fn caption(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.82)))
        .text_color(Theme::text_dim())
        .child(text.into())
}

pub(crate) fn body_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.9)))
        .text_color(Theme::text())
        .child(text.into())
}

pub(crate) fn chip(text: impl Into<SharedString>, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .px(px(Theme::scaled(6.0)))
        .rounded(px(Theme::scaled(3.0)))
        .border_1()
        .border_color(color)
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(color)
        .child(text.into())
}

pub(crate) fn button(id: impl Into<SharedString>, label: String) -> Button {
    Button::new(gpui::ElementId::Name(id.into()))
        .ghost()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

pub(crate) fn checkbox(id: impl Into<SharedString>, label: String, checked: bool) -> Checkbox {
    Checkbox::new(gpui::ElementId::Name(id.into()))
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
        .checked(checked)
}

pub(crate) fn primary_button(id: impl Into<SharedString>, label: String) -> Button {
    Button::new(gpui::ElementId::Name(id.into()))
        .primary()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

pub(crate) type OnChoose = Rc<dyn Fn(String, &mut App)>;

/// A dropdown button listing `choices` (`(value, label)`); the current value
/// is checked.
pub(crate) fn picker(
    id: impl Into<SharedString>,
    shown: String,
    choices: Vec<(String, String)>,
    current: Option<String>,
    on_choose: OnChoose,
) -> impl IntoElement {
    button(id, shown).dropdown_menu(move |mut menu, _, _| {
        for (value, label) in choices.clone() {
            let choose = on_choose.clone();
            let checked = current.as_deref() == Some(value.as_str());
            menu = menu.item(
                PopupMenuItem::new(label)
                    .checked(checked)
                    .on_click(move |_, _, app| choose(value.clone(), app)),
            );
        }
        menu
    })
}

/// A labelled row: `label` then the control.
pub(crate) fn field(label: String, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::scaled(6.0)))
        .child(caption(label).flex_none())
        .child(control)
}

/// A text input that reports its committed value (Enter or blur) to
/// `on_commit`.
pub(crate) fn input<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    placeholder: &'static str,
    value: &str,
    on_commit: impl Fn(&mut V, String, &mut Context<V>) + 'static,
) -> Entity<InputState> {
    let state = cx.new(|cx| {
        InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
            window,
            cx,
            placeholder,
        ))
    });
    if !value.is_empty() {
        let value = value.to_string();
        state.update(cx, |s, cx| s.set_value(value, window, cx));
    }
    cx.subscribe(&state, move |this, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
            let value = input.read(cx).value().to_string();
            on_commit(this, value, cx);
        }
    })
    .detach();
    state
}

/// A read-only results grid over `rows`.
pub(crate) fn grid(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    cx: &mut App,
) -> Option<Entity<SqllyDataTable>> {
    crate::tool_dialog::build_result_grid(
        &crate::tool_dialog::ToolQuerySet {
            columns: columns.to_vec(),
            rows: rows.to_vec(),
        },
        cx,
    )
}

pub(crate) fn grid_box(grid: Option<Entity<SqllyDataTable>>, height: f32) -> AnyElement {
    match grid {
        Some(grid) => div()
            .h(px(Theme::scaled(height)))
            .w_full()
            .border_1()
            .border_color(Theme::border())
            .child(grid)
            .into_any_element(),
        None => div().into_any_element(),
    }
}

/// Whether the app is drawing a dark theme, for SVG chart colours.
pub(crate) fn dark() -> bool {
    crate::theme::relative_luminance(Theme::bg()) < 0.4
}

/// An SVG document as an image element.
pub(crate) fn svg_image(svg: String, width: f32, height: f32) -> AnyElement {
    let image = Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Svg,
        svg.into_bytes(),
    ));
    div()
        .w(px(Theme::scaled(width)))
        .h(px(Theme::scaled(height)))
        .child(img(image).size_full())
        .into_any_element()
}

/// A compact bar chart of `(label, value)` pairs through the report SVG
/// renderer (the same drawing every export uses).
pub(crate) fn bar_chart_svg(id: &str, bars: &[(String, f64)], width: f32, height: f32) -> String {
    use sqlly_services::report::chart::{ChartKind, ChartSpec};
    use sqlly_services::report::doc::{TableBlock, TableColumn};
    let table = TableBlock::new(
        vec![
            TableColumn::new("bucket", "nvarchar"),
            TableColumn::new("count", "float"),
        ],
        bars.iter()
            .map(|(l, v)| vec![CellValue::String(l.clone()), CellValue::F64(*v)])
            .collect(),
    );
    let spec = ChartSpec::new(ChartKind::Bar).x("bucket").y("count");
    let theme = if dark() {
        &sqlly_services::report::theme::DARK
    } else {
        &sqlly_services::report::theme::LIGHT
    };
    let labels = sqlly_services::report::labels::ReportLabels::default();
    sqlly_services::report::svg::chart_svg(
        &spec,
        &table,
        sqlly_services::report::svg::ChartOptions::new(theme, &labels).size(width, height),
        id,
    )
}

/// A number for display: integers without decimals, others to 4
/// significant places.
pub(crate) fn number(value: f64) -> String {
    if !value.is_finite() {
        return "—".into();
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let magnitude = value.abs();
    if magnitude >= 1000.0 {
        format!("{value:.1}")
    } else if magnitude >= 1.0 {
        format!("{value:.3}")
    } else {
        format!("{value:.4}")
    }
}

pub(crate) fn percent(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

pub(crate) fn section(title: String) -> gpui::Div {
    div()
        .pt(px(Theme::scaled(4.0)))
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text())
        .child(title)
}

/// The body every analysis cell draws: its error line (if any) above the
/// cell's own view.
pub(crate) fn cell_body(cell: &crate::notebook::cells::CellRenderCx) -> AnyElement {
    use crate::notebook::view::CellStatus;
    use gpui::prelude::FluentBuilder as _;
    let s = Theme::scaled;
    if cell.collapsed {
        return div()
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .child(caption(t("notebook.analysis.collapsed")))
            .into_any_element();
    }
    let failed = matches!(cell.state, CellStatus::Failed | CellStatus::Blocked);
    div()
        .flex()
        .flex_col()
        .w_full()
        .when_some(cell.message.clone(), |d, message| {
            d.child(
                div()
                    .px(px(s(10.0)))
                    .pt(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(if failed {
                        Theme::error()
                    } else {
                        Theme::text_dim()
                    })
                    .child(message),
            )
        })
        .children(cell.analysis.clone().map(|v| v.into_any_element()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_svg_is_deterministic_and_scriptless() {
        let bars = vec![("0–10".to_string(), 3.0), ("10–20".to_string(), 7.0)];
        let a = bar_chart_svg("h", &bars, 220.0, 80.0);
        assert_eq!(a, bar_chart_svg("h", &bars, 220.0, 80.0));
        assert!(a.starts_with("<svg") && !a.contains("<script"));
        assert_eq!(number(3.0), "3");
        assert_eq!(number(0.123456), "0.1235");
        assert_eq!(percent(0.25), "25.0%");
    }
}
