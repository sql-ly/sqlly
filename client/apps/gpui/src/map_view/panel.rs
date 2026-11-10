//! The map window's Options panel: every `-- sqlly map:` setting as a
//! control. Each control edits a copy of the live [`MapDirective`] and
//! applies it, so the panel, the map, and the directive line it writes
//! never disagree.

use gpui::{ClipboardItem, Context, SharedString, Window, div, prelude::*, px};
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};

use super::directive::{
    BLUR_RANGE, HeatPalette, INTENSITY_RANGE, MapDirective, MapMode, OPACITY_RANGE, RADIUS_RANGE,
    directive_line,
};
use super::features::{MAX_CATEGORIES, MAX_FEATURES, MapSource};
use super::heatmap;
use super::view::{MapContent, MapView, feature_color};
use crate::localization::{format as tr_format, text as tr};
use crate::theme::Theme;

/// Feature-limit presets offered by the panel.
const LIMITS: [usize; 4] = [1_000, 5_000, 10_000, MAX_FEATURES];

/// The source settings a [`MapSource`] writes.
pub(crate) fn set_source(d: &mut MapDirective, source: Option<MapSource>, names: &[String]) {
    d.lat = None;
    d.lon = None;
    d.geom = None;
    match source {
        Some(MapSource::LonLat { lon, lat }) => {
            d.lon = names.get(lon).cloned();
            d.lat = names.get(lat).cloned();
        }
        Some(MapSource::Geometry { column }) => d.geom = names.get(column).cloned(),
        None => {}
    }
}

/// Toggle a tooltip column (case-insensitive), keeping order.
pub(crate) fn toggle_tooltip(d: &mut MapDirective, name: &str) {
    if let Some(i) = d.tooltip.iter().position(|n| n.eq_ignore_ascii_case(name)) {
        d.tooltip.remove(i);
    } else {
        d.tooltip.push(name.to_string());
    }
}

/// Step a numeric setting, clamped to its range.
pub(crate) fn step(value: Option<f64>, default: f64, delta: f64, (min, max): (f64, f64)) -> f64 {
    (((value.unwrap_or(default) + delta) * 1000.0).round() / 1000.0).clamp(min, max)
}

impl MapView {
    /// Apply an edit to a copy of the live settings.
    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut MapDirective)) {
        let mut d = self.options.clone();
        f(&mut d);
        self.apply_options(d);
        cx.notify();
    }

    fn row(label: String, control: impl IntoElement) -> gpui::Div {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_col()
            .gap(px(s(3.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(label),
            )
            .child(control)
    }

    fn header(text: String) -> gpui::Div {
        let s = Theme::scaled;
        div()
            .pt(px(s(6.0)))
            .text_size(px(Theme::font_size_ratio(0.9)))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Theme::text())
            .child(text)
    }

    /// A dropdown choosing one of `choices` (`None` = the "auto"/"none"
    /// entry labelled `none_label`), writing through `set`.
    fn column_picker(
        &self,
        id: &'static str,
        current: Option<&str>,
        none_label: String,
        choices: Vec<String>,
        cx: &mut Context<Self>,
        set: fn(&mut MapDirective, Option<String>),
    ) -> impl IntoElement {
        let this = cx.entity();
        let label = current
            .map(str::to_string)
            .unwrap_or_else(|| none_label.clone());
        let current = current.map(str::to_ascii_lowercase);
        Button::new(id)
            .outline()
            .xsmall()
            .label(label)
            .dropdown_menu(move |menu, _window, _cx| {
                let this_none = this.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(none_label.clone())
                        .checked(current.is_none())
                        .on_click(move |_, _, app| {
                            this_none.update(app, |v, cx| v.edit(cx, |d| set(d, None)));
                        }),
                );
                for name in &choices {
                    let this = this.clone();
                    let value = name.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(current.as_deref() == Some(&name.to_ascii_lowercase()))
                            .on_click(move |_, _, app| {
                                let value = value.clone();
                                this.update(app, |v, cx| v.edit(cx, |d| set(d, Some(value))));
                            }),
                    );
                }
                menu
            })
    }

    fn stepper(
        &self,
        id: &'static str,
        text: String,
        cx: &mut Context<Self>,
        dec: fn(&mut MapDirective),
        inc: fn(&mut MapDirective),
    ) -> gpui::Div {
        let s = Theme::scaled;
        Self::group()
            .child(self.segment(
                SharedString::from(format!("{id}-dec")),
                "−".into(),
                false,
                cx,
                move |this, cx| this.edit(cx, dec),
            ))
            .child(
                div()
                    .px(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .child(text),
            )
            .child(self.segment(
                SharedString::from(format!("{id}-inc")),
                "+".into(),
                false,
                cx,
                move |this, cx| this.edit(cx, inc),
            ))
    }

    fn on_off(
        &self,
        id: &'static str,
        on_label: String,
        off_label: String,
        value: bool,
        cx: &mut Context<Self>,
        set: fn(&mut MapDirective, bool),
    ) -> gpui::Div {
        Self::group()
            .child(self.segment(
                SharedString::from(format!("{id}-on")),
                on_label,
                value,
                cx,
                move |this, cx| this.edit(cx, |d| set(d, true)),
            ))
            .child(self.segment(
                SharedString::from(format!("{id}-off")),
                off_label,
                !value,
                cx,
                move |this, cx| this.edit(cx, |d| set(d, false)),
            ))
    }

    fn legend(&self) -> Option<gpui::Div> {
        let s = Theme::scaled;
        let swatch = |color: gpui::Hsla| {
            div()
                .w(px(s(10.0)))
                .h(px(s(10.0)))
                .rounded(px(s(5.0)))
                .bg(color)
        };
        let item = |color, text: String| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .text_size(px(Theme::font_size_ratio(0.85)))
                .child(swatch(color))
                .child(text)
        };
        if let Some((min, max)) = self.data.color_range {
            use super::features::ColorKey;
            return Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(item(feature_color(ColorKey::Scale(0.0)), format!("{min}")))
                    .child(item(feature_color(ColorKey::Scale(1.0)), format!("{max}"))),
            );
        }
        if self.data.categories.is_empty() {
            return None;
        }
        use super::features::ColorKey;
        let mut list = div().flex().flex_col().gap(px(s(2.0)));
        for (i, name) in self.data.categories.iter().enumerate() {
            list = list.child(item(feature_color(ColorKey::Category(i)), name.clone()));
        }
        if self
            .data
            .features
            .iter()
            .any(|f| f.color == ColorKey::Category(MAX_CATEGORIES))
        {
            list = list.child(item(
                feature_color(ColorKey::Category(MAX_CATEGORIES)),
                tr("map_view.panel.other_category"),
            ));
        }
        Some(list)
    }

    pub(super) fn options_panel(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let d = &self.options;
        let names: Vec<String> = self.columns().iter().map(|c| c.name.clone()).collect();
        let has_rows = matches!(self.content, MapContent::Rows { .. });
        let mut body = div().flex().flex_col().gap(px(s(8.0))).p(px(s(12.0)));

        if has_rows {
            body = body.child(Self::header(tr("map_view.panel.data_header")));
            // Locations: auto, or one of the detected sources.
            let this = cx.entity();
            let sources = self.sources().to_vec();
            let columns = self.columns().to_vec();
            let explicit = d.lat.is_some() || d.geom.is_some();
            let current_label = if explicit {
                self.source.map(|s| s.label(&columns))
            } else {
                None
            };
            let auto_label = tr("map_view.panel.auto");
            let names_for_menu = names.clone();
            body = body.child(Self::row(
                tr("map_view.panel.locations"),
                Button::new("map-panel-source")
                    .outline()
                    .xsmall()
                    .label(current_label.clone().unwrap_or_else(|| auto_label.clone()))
                    .dropdown_menu(move |menu, _w, _cx| {
                        let t = this.clone();
                        let n = names_for_menu.clone();
                        let mut menu = menu.item(
                            PopupMenuItem::new(auto_label.clone())
                                .checked(!explicit)
                                .on_click(move |_, _, app| {
                                    let n = n.clone();
                                    t.update(app, |v, cx| v.edit(cx, |d| set_source(d, None, &n)));
                                }),
                        );
                        for source in &sources {
                            let t = this.clone();
                            let n = names_for_menu.clone();
                            let source = *source;
                            let label = source.label(&columns);
                            menu = menu.item(
                                PopupMenuItem::new(label.clone())
                                    .checked(current_label.as_deref() == Some(label.as_str()))
                                    .on_click(move |_, _, app| {
                                        let n = n.clone();
                                        t.update(app, |v, cx| {
                                            v.edit(cx, |d| set_source(d, Some(source), &n))
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            ));
            body = body.child(Self::row(
                tr("map_view.panel.label"),
                self.column_picker(
                    "map-panel-label",
                    d.label.as_deref(),
                    tr("map_view.panel.auto"),
                    names.clone(),
                    cx,
                    |d, v| d.label = v,
                ),
            ));
            // Tooltip columns: a checkable list.
            let this = cx.entity();
            let chosen: Vec<String> = d.tooltip.iter().map(|n| n.to_ascii_lowercase()).collect();
            let tooltip_label = if d.tooltip.is_empty() {
                tr("map_view.panel.none")
            } else {
                d.tooltip.join(", ")
            };
            let names_for_menu = names.clone();
            body = body.child(Self::row(
                tr("map_view.panel.tooltip"),
                Button::new("map-panel-tooltip")
                    .outline()
                    .xsmall()
                    .label(tooltip_label)
                    .dropdown_menu(move |menu, _w, _cx| {
                        let mut menu = menu;
                        for name in &names_for_menu {
                            let t = this.clone();
                            let value = name.clone();
                            menu = menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(chosen.contains(&name.to_ascii_lowercase()))
                                    .on_click(move |_, _, app| {
                                        let value = value.clone();
                                        t.update(app, |v, cx| {
                                            v.edit(cx, |d| toggle_tooltip(d, &value))
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            ));
            body = body
                .child(Self::row(
                    tr("map_view.panel.color"),
                    self.column_picker(
                        "map-panel-color",
                        d.color.as_deref(),
                        tr("map_view.panel.none"),
                        names.clone(),
                        cx,
                        |d, v| d.color = v,
                    ),
                ))
                .children(self.legend())
                .child(Self::row(
                    tr("map_view.panel.size"),
                    self.column_picker(
                        "map-panel-size",
                        d.size.as_deref(),
                        tr("map_view.panel.none"),
                        names.clone(),
                        cx,
                        |d, v| d.size = v,
                    ),
                ));
        }

        // Layers.
        body = body.child(Self::header(tr("map_view.panel.layers_header")));
        let mode = d.mode();
        let mut modes = Self::group();
        for (i, m) in MapMode::ALL.into_iter().enumerate() {
            let label = match m {
                MapMode::Points => tr("map_view.toolbar.points"),
                MapMode::Heatmap => tr("map_view.toolbar.heatmap"),
                MapMode::Both => tr("map_view.toolbar.both"),
            };
            modes = modes.child(self.segment(
                SharedString::from(format!("map-panel-mode-{i}")),
                label,
                m == mode,
                cx,
                move |this, cx| this.edit(cx, |d| d.mode = Some(m)),
            ));
        }
        body = body.child(Self::row(tr("map_view.panel.show"), modes));
        if mode.shows_points() {
            body = body.child(Self::row(
                tr("map_view.panel.cluster"),
                self.on_off(
                    "map-panel-cluster",
                    tr("map_view.panel.on"),
                    tr("map_view.panel.off"),
                    d.cluster == Some(true),
                    cx,
                    |d, v| d.cluster = Some(v),
                ),
            ));
        }
        if mode.shows_heat() {
            if has_rows {
                body = body.child(Self::row(
                    tr("map_view.panel.weight"),
                    self.column_picker(
                        "map-panel-weight",
                        d.weight.as_deref(),
                        tr("map_view.panel.none"),
                        names.clone(),
                        cx,
                        |d, v| d.weight = v,
                    ),
                ));
            }
            let radius = d.radius.unwrap_or(heatmap::DEFAULT_RADIUS_PX);
            body = body
                .child(Self::row(
                    tr("map_view.panel.radius"),
                    self.stepper(
                        "map-panel-radius",
                        tr_format(
                            "map_view.toolbar.radius",
                            &[("radius", radius.round() as i64)],
                        ),
                        cx,
                        |d| {
                            d.radius = Some(step(
                                d.radius,
                                heatmap::DEFAULT_RADIUS_PX,
                                -6.0,
                                RADIUS_RANGE,
                            ))
                        },
                        |d| {
                            d.radius = Some(step(
                                d.radius,
                                heatmap::DEFAULT_RADIUS_PX,
                                6.0,
                                RADIUS_RANGE,
                            ))
                        },
                    ),
                ))
                .child(Self::row(
                    tr("map_view.panel.blur"),
                    self.stepper(
                        "map-panel-blur",
                        tr_format(
                            "map_view.panel.pixels",
                            &[("value", d.blur.unwrap_or(0.0).round() as i64)],
                        ),
                        cx,
                        |d| d.blur = Some(step(d.blur, 0.0, -6.0, BLUR_RANGE)),
                        |d| d.blur = Some(step(d.blur, 0.0, 6.0, BLUR_RANGE)),
                    ),
                ))
                .child(Self::row(
                    tr("map_view.panel.intensity"),
                    self.stepper(
                        "map-panel-intensity",
                        format!("{}×", d.intensity.unwrap_or(1.0)),
                        cx,
                        |d| d.intensity = Some(step(d.intensity, 1.0, -0.25, INTENSITY_RANGE)),
                        |d| d.intensity = Some(step(d.intensity, 1.0, 0.25, INTENSITY_RANGE)),
                    ),
                ))
                .child(Self::row(
                    tr("map_view.panel.opacity"),
                    self.stepper(
                        "map-panel-opacity",
                        format!("{}%", (d.opacity.unwrap_or(0.8) * 100.0).round()),
                        cx,
                        |d| d.opacity = Some(step(d.opacity, 0.8, -0.1, OPACITY_RANGE)),
                        |d| d.opacity = Some(step(d.opacity, 0.8, 0.1, OPACITY_RANGE)),
                    ),
                ));
            let this = cx.entity();
            let current = d.palette.unwrap_or_default();
            body = body.child(Self::row(
                tr("map_view.panel.palette"),
                Button::new("map-panel-palette")
                    .outline()
                    .xsmall()
                    .label(palette_label(current))
                    .dropdown_menu(move |menu, _w, _cx| {
                        let mut menu = menu;
                        for p in HeatPalette::ALL {
                            let t = this.clone();
                            menu = menu.item(
                                PopupMenuItem::new(palette_label(p))
                                    .checked(p == current)
                                    .on_click(move |_, _, app| {
                                        t.update(app, |v, cx| v.edit(cx, |d| d.palette = Some(p)));
                                    }),
                            );
                        }
                        menu
                    }),
            ));
        }

        // Map behavior.
        body = body
            .child(Self::header(tr("map_view.panel.map_header")))
            .child(Self::row(
                tr("map_view.panel.basemap"),
                self.on_off(
                    "map-panel-basemap",
                    tr("map_view.panel.basemap_osm"),
                    tr("map_view.panel.none"),
                    d.basemap != Some(false),
                    cx,
                    |d, v| d.basemap = Some(v),
                ),
            ));
        if has_rows {
            body = body
                .child(Self::row(
                    tr("map_view.panel.click"),
                    self.on_off(
                        "map-panel-click",
                        tr("map_view.panel.click_select"),
                        tr("map_view.panel.none"),
                        d.click != Some(false),
                        cx,
                        |d, v| d.click = Some(v),
                    ),
                ))
                .child(Self::row(
                    tr("map_view.panel.open"),
                    self.on_off(
                        "map-panel-open",
                        tr("map_view.panel.open_auto"),
                        tr("map_view.panel.open_manual"),
                        d.open == Some(true),
                        cx,
                        |d, v| d.open = Some(v),
                    ),
                ));
            let this = cx.entity();
            let limit = d.feature_limit();
            body = body.child(Self::row(
                tr("map_view.panel.limit"),
                Button::new("map-panel-limit")
                    .outline()
                    .xsmall()
                    .label(limit.to_string())
                    .dropdown_menu(move |menu, _w, _cx| {
                        let mut menu = menu;
                        for n in LIMITS {
                            let t = this.clone();
                            menu = menu.item(
                                PopupMenuItem::new(n.to_string())
                                    .checked(n == limit)
                                    .on_click(move |_, _, app| {
                                        t.update(app, |v, cx| {
                                            v.edit(cx, |d| {
                                                d.limit = (n != MAX_FEATURES).then_some(n)
                                            })
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            ));
        }
        // Starting view: fit, or remember where the map is now.
        body = body.child(Self::row(
            tr("map_view.panel.start_view"),
            Self::group()
                .child(self.segment(
                    "map-panel-fit",
                    tr("map_view.panel.view_fit"),
                    d.center.is_none() && d.zoom.is_none(),
                    cx,
                    |this, cx| {
                        this.edit(cx, |d| {
                            d.fit = true;
                            d.center = None;
                            d.zoom = None;
                        })
                    },
                ))
                .child(self.segment(
                    "map-panel-here",
                    tr("map_view.panel.view_current"),
                    d.center.is_some(),
                    cx,
                    |this, cx| {
                        let (lat, lon) = this.current_center();
                        let zoom = (this.viewport.zoom * 10.0).round() / 10.0;
                        let mut d = this.options.clone();
                        d.fit = false;
                        d.center = Some((lat, lon));
                        d.zoom = Some(zoom);
                        // Keep the view where it is (no re-fit).
                        this.options = d;
                        cx.notify();
                    },
                )),
        ));

        // The directive itself.
        let line = directive_line(d);
        let copy_line = line.clone();
        let mut actions = div().flex().flex_row().gap(px(s(6.0))).child(self.segment(
            "map-panel-copy",
            tr("map_view.panel.copy_directive"),
            false,
            cx,
            move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_line.clone())),
        ));
        if let Some(write) = self.hooks.write.clone() {
            let settings = d.clone();
            actions = actions.child(
                crate::theme::accent_pill_button(
                    "map-panel-write",
                    tr("map_view.panel.write_to_query"),
                )
                .on_click(move |_, _, app| write(settings.clone(), app)),
            );
        }
        body = body
            .child(Self::header(tr("map_view.panel.directive_header")))
            .child(
                div()
                    .p(px(s(6.0)))
                    .rounded(px(s(4.0)))
                    .bg(Theme::input_bg())
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .child(line),
            )
            .child(actions);

        div()
            .id("map-options-panel")
            .w(px(s(290.0)))
            .h_full()
            .flex_none()
            .overflow_y_scroll()
            .bg(Theme::panel_bg())
            .border_l_1()
            .border_color(Theme::border())
            .child(body)
            .into_any_element()
    }
}

fn palette_label(p: HeatPalette) -> String {
    tr(match p {
        HeatPalette::Heat => "map_view.panel.palette_heat",
        HeatPalette::Fire => "map_view.panel.palette_fire",
        HeatPalette::Viridis => "map_view.panel.palette_viridis",
        HeatPalette::Ocean => "map_view.panel.palette_ocean",
        HeatPalette::Accent => "map_view.panel.palette_accent",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_edits_write_the_directive_the_parser_reads() {
        let names: Vec<String> = ["name", "lon", "lat", "shape"].map(String::from).to_vec();
        let mut d = MapDirective::default();
        set_source(&mut d, Some(MapSource::LonLat { lon: 1, lat: 2 }), &names);
        toggle_tooltip(&mut d, "name");
        toggle_tooltip(&mut d, "lat");
        toggle_tooltip(&mut d, "NAME");
        d.radius = Some(step(d.radius, 24.0, 6.0, RADIUS_RANGE));
        d.opacity = Some(step(Some(0.95), 0.8, 0.1, OPACITY_RANGE));
        assert_eq!(
            (d.lon.as_deref(), d.lat.as_deref()),
            (Some("lon"), Some("lat"))
        );
        assert_eq!(d.tooltip, vec!["lat".to_string()]);
        assert_eq!((d.radius, d.opacity), (Some(30.0), Some(1.0)));
        let parsed = super::super::directive::parse(&directive_line(&d));
        assert_eq!(parsed, vec![d.clone()]);
        set_source(&mut d, Some(MapSource::Geometry { column: 3 }), &names);
        assert_eq!((d.geom.as_deref(), d.lat.as_deref()), (Some("shape"), None));
        set_source(&mut d, None, &names);
        assert!(d.geom.is_none() && d.lat.is_none() && d.lon.is_none());
    }
}
