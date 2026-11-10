//! Canvas painter for the execution-plan Diagram mode. Pure paint: the view
//! owns pan/selection state and resolves theme colors on the render pass,
//! same split as `er_diagram.rs`.

use gpui::{
    App, Bounds, Corners, Edges, Hsla, PaintQuad, Pixels, Point, TextAlign, TextRun, Window, point,
    px, size,
};
use sqlly_services::plans::{DIAGRAM_NODE_H, DIAGRAM_NODE_W, PlanDiagramLayout};

use crate::execution_plan_view::truncate;
use crate::theme::Theme;

#[derive(Clone)]
pub(crate) struct DiagramPaint {
    pub layout: PlanDiagramLayout,
    pub selected: Option<i32>,
    pub pan: (f32, f32),
    pub background: Hsla,
    pub node_bg: Hsla,
    pub node_border: Hsla,
    pub accent: Hsla,
    pub warning: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub font_size: f32,
}

pub(crate) fn node_at(
    layout: &PlanDiagramLayout,
    pan: (f32, f32),
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<i32> {
    let lx = f32::from(position.x - bounds.origin.x) - pan.0;
    let ly = f32::from(position.y - bounds.origin.y) - pan.1;
    layout.nodes.iter().rev().find_map(|node| {
        if lx >= node.x
            && lx <= node.x + DIAGRAM_NODE_W
            && ly >= node.y
            && ly <= node.y + DIAGRAM_NODE_H
        {
            Some(node.node_id)
        } else {
            None
        }
    })
}

pub(crate) fn paint_plan_diagram(
    p: &DiagramPaint,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    window.paint_quad(PaintQuad {
        bounds,
        background: p.background.into(),
        border_color: gpui::transparent_black(),
        border_widths: Default::default(),
        corner_radii: Default::default(),
        border_style: Default::default(),
    });
    if p.layout.nodes.is_empty() {
        return;
    }

    let ox = f32::from(bounds.origin.x) + p.pan.0;
    let oy = f32::from(bounds.origin.y) + p.pan.1;
    let text_system = window.text_system().clone();
    let font = Theme::ui_font();
    let line_height = |fs: f32| px(fs * 1.4);
    let shape = |text: &str, color: Hsla, fs: f32| {
        let run = TextRun {
            len: text.len(),
            color,
            font: font.clone(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        text_system.shape_line(text.to_owned().into(), px(fs), &[run], None)
    };
    let by_id: std::collections::HashMap<i32, &sqlly_services::plans::PlanDiagramNode> =
        p.layout.nodes.iter().map(|n| (n.node_id, n)).collect();

    for edge in &p.layout.edges {
        let Some(parent) = by_id.get(&edge.from_node) else {
            continue;
        };
        let Some(child) = by_id.get(&edge.to_node) else {
            continue;
        };
        let start = (
            ox + parent.x + DIAGRAM_NODE_W / 2.0,
            oy + parent.y + DIAGRAM_NODE_H,
        );
        let end = (ox + child.x + DIAGRAM_NODE_W / 2.0, oy + child.y);
        let mut stroke = gpui::PathBuilder::stroke(px(1.5));
        stroke.move_to(point(px(start.0), px(start.1)));
        stroke.line_to(point(px(end.0), px(end.1)));
        if let Ok(path) = stroke.build() {
            window.paint_path(path, p.text_dim);
        }
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1.0 {
            let (ux, uy) = (dx / len, dy / len);
            let base = (end.0 - ux * 9.0, end.1 - uy * 9.0);
            let (px_, py_) = (-uy, ux);
            let mut head = gpui::PathBuilder::fill();
            head.move_to(point(px(end.0), px(end.1)));
            head.line_to(point(px(base.0 + px_ * 4.5), px(base.1 + py_ * 4.5)));
            head.line_to(point(px(base.0 - px_ * 4.5), px(base.1 - py_ * 4.5)));
            head.close();
            if let Ok(path) = head.build() {
                window.paint_path(path, p.text_dim);
            }
        }
    }

    for node in &p.layout.nodes {
        let x = ox + node.x;
        let y = oy + node.y;
        let selected = p.selected == Some(node.node_id);
        let border = if selected { p.accent } else { p.node_border };
        window.paint_quad(PaintQuad {
            bounds: Bounds {
                origin: point(px(x), px(y)),
                size: size(px(DIAGRAM_NODE_W), px(DIAGRAM_NODE_H)),
            },
            background: p.node_bg.into(),
            border_color: border,
            border_widths: Edges::all(px(1.0)),
            corner_radii: Corners::all(px(0.0)),
            border_style: Default::default(),
        });
        let bar_w = (node.cost_percent as f32 / 100.0 * DIAGRAM_NODE_W).clamp(0.0, DIAGRAM_NODE_W);
        if bar_w > 0.0 {
            let bar_h = 3.0;
            let bar_color = if node.warning_count > 0 {
                p.warning
            } else {
                p.accent
            };
            window.paint_quad(PaintQuad {
                bounds: Bounds {
                    origin: point(px(x), px(y + DIAGRAM_NODE_H - bar_h)),
                    size: size(px(bar_w), px(bar_h)),
                },
                background: bar_color.into(),
                border_color: gpui::transparent_black(),
                border_widths: Default::default(),
                corner_radii: Default::default(),
                border_style: Default::default(),
            });
        }
        let title_fs = p.font_size * 0.9;
        let title = shape(&truncate(&node.title, 20), p.text, title_fs);
        let _ = title.paint(
            Point {
                x: px(x + 8.0),
                y: px(y + 7.0),
            },
            line_height(title_fs),
            TextAlign::Left,
            None,
            window,
            cx,
        );
        let estimated = sqlly_services::plans::human_count(node.estimated_rows);
        let cost = format!("{:.0}", node.cost_percent);
        let subtitle = if let Some(actual) = node.actual_rows {
            crate::localization::format(
                "query.execution_plan.diagram_actual_subtitle",
                &[
                    ("cost_percent", &cost),
                    ("estimated_rows", &estimated),
                    ("actual_rows", &sqlly_services::plans::human_count(actual)),
                ],
            )
        } else {
            crate::localization::format(
                "query.execution_plan.diagram_estimated_subtitle",
                &[("cost_percent", &cost), ("estimated_rows", &estimated)],
            )
        };
        let cols_fs = p.font_size * 0.74;
        let cols = shape(&truncate(&subtitle, 26), p.text_dim, cols_fs);
        let _ = cols.paint(
            Point {
                x: px(x + 8.0),
                y: px(y + 7.0 + title_fs * 1.5),
            },
            line_height(cols_fs),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::plans::PlanDiagramNode;

    #[test]
    fn node_at_maps_window_coordinates_through_pan() {
        let layout = PlanDiagramLayout {
            nodes: vec![PlanDiagramNode {
                node_id: 1,
                title: "Scan".into(),
                cost_percent: 50.0,
                estimated_rows: 4.0,
                actual_rows: None,
                warning_count: 0,
                x: 24.0,
                y: 24.0,
            }],
            edges: Vec::new(),
            width: 216.0,
            height: 104.0,
        };
        let bounds = Bounds {
            origin: point(px(100.0), px(100.0)),
            size: size(px(800.0), px(480.0)),
        };
        let inside = point(px(130.0), px(130.0));
        assert_eq!(node_at(&layout, (0.0, 0.0), bounds, inside), Some(1));
        assert_eq!(
            node_at(&layout, (-(DIAGRAM_NODE_W + 1.0), 0.0), bounds, inside),
            None
        );
    }
}
