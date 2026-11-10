//! Pure exporters for the relationships diagram: SVG, Mermaid `erDiagram`, and DBML.
//! The view decides the visible subgraph; these functions only format a model.

use std::fmt::Write as _;

use gpui::Hsla;

use crate::er_diagram::{ErGraph, ErLayout, rect_border_point};

pub struct ErSvgStyle {
    pub background: Hsla,
    pub node_bg: Hsla,
    pub node_border: Hsla,
    pub accent: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub font_size: f32,
    /// Node box height (the diagram's boxes grow with visible column rows).
    pub node_h: f32,
    /// Node box width, which the diagram lets the user widen or narrow.
    pub node_w: f32,
}

fn hsla_css(c: Hsla) -> String {
    format!(
        "hsl({:.0} {:.1}% {:.1}%)",
        c.h * 360.0,
        c.s * 100.0,
        c.l * 100.0
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// SVG at zoom 1.0, sized to the layout bounds. Mirrors `paint_er`: edges
/// (line + arrowhead polygon toward `to` + mid label) under node rects
/// (rx=4, 1px border) with a title line and a dim key-columns line.
pub fn er_svg(
    graph: &ErGraph,
    layout: &ErLayout,
    positions: &dyn Fn(usize) -> (f32, f32),
    style: &ErSvgStyle,
) -> String {
    let width = layout.width.max(1.0);
    let height = layout.height.max(1.0);
    let mut svg = String::new();
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.1}\" height=\"{height:.1}\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" font-family=\"sans-serif\" \
         style=\"background:{}\">",
        hsla_css(style.background)
    );

    let node_h = style.node_h;
    let node_w = style.node_w;
    let half = (node_w / 2.0, node_h / 2.0);
    let stroke = hsla_css(style.text_dim);
    let text = hsla_css(style.text);
    let text_dim = hsla_css(style.text_dim);
    let node_bg = hsla_css(style.node_bg);
    let node_border = hsla_css(style.node_border);
    let _ = style.accent;

    for edge in &graph.edges {
        if edge.from == edge.to {
            continue;
        }
        let (ax, ay) = positions(edge.from);
        let (bx, by) = positions(edge.to);
        let ca = (ax + half.0, ay + half.1);
        let cb = (bx + half.0, by + half.1);
        let start = rect_border_point(ca, half, cb);
        let end = rect_border_point(cb, half, ca);
        let _ = write!(
            svg,
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{stroke}\" stroke-width=\"1.5\" fill=\"none\"/>",
            start.0, start.1, end.0, end.1
        );
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1.0 {
            let (ux, uy) = (dx / len, dy / len);
            let base = (end.0 - ux * 9.0, end.1 - uy * 9.0);
            let (px_, py_) = (-uy, ux);
            let _ = write!(
                svg,
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"{stroke}\"/>",
                end.0,
                end.1,
                base.0 + px_ * 4.5,
                base.1 + py_ * 4.5,
                base.0 - px_ * 4.5,
                base.1 - py_ * 4.5
            );
        }
        let fs = style.font_size * 0.72;
        let mid = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
        let _ = write!(
            svg,
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"{fs:.1}\" text-anchor=\"middle\" fill=\"{text_dim}\">{}</text>",
            mid.0,
            mid.1 - fs * 1.2,
            xml_escape(&edge.label)
        );
    }

    for (i, node) in graph.nodes.iter().enumerate() {
        let (x, y) = positions(i);
        let _ = write!(
            svg,
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{node_w:.1}\" height=\"{node_h:.1}\" rx=\"4\" \
             fill=\"{node_bg}\" stroke=\"{node_border}\" stroke-width=\"1\"/>"
        );
        let title_fs = style.font_size * 0.9;
        let _ = write!(
            svg,
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"{title_fs:.1}\" fill=\"{text}\">{}</text>",
            x + 9.0,
            y + 8.0 + title_fs,
            xml_escape(&node.qualified())
        );
        if !node.columns_in_keys.is_empty() {
            let cols_fs = style.font_size * 0.74;
            let cols = node.columns_in_keys.join(", ");
            let _ = write!(
                svg,
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"{cols_fs:.1}\" fill=\"{text_dim}\">{}</text>",
                x + 9.0,
                y + 8.0 + title_fs * 1.5 + cols_fs,
                xml_escape(&cols)
            );
        }
    }

    svg.push_str("</svg>");
    svg
}

fn mermaid_ident(schema: &str, table: &str) -> String {
    let raw = if schema.is_empty() {
        table.to_string()
    } else {
        format!("{schema}_{table}")
    };
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Mermaid `erDiagram`. Entity names are `schema_table` (sanitised to
/// `[A-Za-z0-9_]`). Key columns are emitted as `string <col> PK`. Each
/// relationship is `<to> ||--o{ <from>` (referenced side exactly-one,
/// referencing side zero-or-many).
pub fn er_mermaid(graph: &ErGraph) -> String {
    let mut out = String::from("erDiagram\n");
    for node in &graph.nodes {
        let id = mermaid_ident(&node.schema, &node.table);
        let _ = writeln!(out, "    {id} {{");
        if node.columns_in_keys.is_empty() {
            let _ = writeln!(out, "        string _ PK");
        } else {
            for col in &node.columns_in_keys {
                let safe: String = col
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let _ = writeln!(out, "        string {safe} PK");
            }
        }
        let _ = writeln!(out, "    }}");
    }
    for edge in &graph.edges {
        let from = &graph.nodes[edge.from];
        let to = &graph.nodes[edge.to];
        let from_id = mermaid_ident(&from.schema, &from.table);
        let to_id = mermaid_ident(&to.schema, &to.table);
        let label = edge.label.replace('"', "'");
        let _ = writeln!(out, "    {to_id} ||--o{{ {from_id} : \"{label}\"");
    }
    out
}

fn dbml_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\\\""))
}

/// DBML. One `Table` per node listing key columns as `unknown [pk]`, then
/// one `Ref` per edge. Types are not exported (header comment).
pub fn er_dbml(graph: &ErGraph) -> String {
    let mut out = String::from("// Column types are not exported.\n");
    for node in &graph.nodes {
        let _ = writeln!(
            out,
            "Table {}.{} {{",
            dbml_ident(&node.schema),
            dbml_ident(&node.table)
        );
        for col in &node.columns_in_keys {
            let _ = writeln!(out, "  {} unknown [pk]", dbml_ident(col));
        }
        let _ = writeln!(out, "}}");
    }
    for edge in &graph.edges {
        let from = &graph.nodes[edge.from];
        let to = &graph.nodes[edge.to];
        if edge.pairs.is_empty() {
            continue;
        }
        let from_cols: Vec<String> = edge.pairs.iter().map(|(f, _)| dbml_ident(f)).collect();
        let to_cols: Vec<String> = edge.pairs.iter().map(|(_, t)| dbml_ident(t)).collect();
        let from_ref = dbml_ref(&from.schema, &from.table, &from_cols);
        let to_ref = dbml_ref(&to.schema, &to.table, &to_cols);
        let _ = writeln!(out, "Ref: {from_ref} > {to_ref}");
    }
    out
}

fn dbml_ref(schema: &str, table: &str, cols: &[String]) -> String {
    let qt = format!("{}.{}", dbml_ident(schema), dbml_ident(table));
    if cols.len() == 1 {
        format!("{qt}.{}", cols[0])
    } else {
        format!("{qt}.({})", cols.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er_diagram::NODE_W;
    use crate::er_diagram::{build_graph, layout_grid};
    use sqlly_services::ForeignKeyRow;

    fn fk(
        constraint: &str,
        from_schema: &str,
        from_table: &str,
        from_column: &str,
        to_schema: &str,
        to_table: &str,
        to_column: &str,
    ) -> ForeignKeyRow {
        ForeignKeyRow {
            constraint_name: constraint.to_string(),
            from_schema: from_schema.to_string(),
            from_table: from_table.to_string(),
            from_column: from_column.to_string(),
            to_schema: to_schema.to_string(),
            to_table: to_table.to_string(),
            to_column: to_column.to_string(),
        }
    }

    fn fixture() -> ErGraph {
        build_graph(&[
            fk(
                "fk_orders_user",
                "public",
                "orders",
                "user_id",
                "public",
                "users",
                "id",
            ),
            fk(
                "fk_items_order",
                "public",
                "order_items",
                "order_id",
                "public",
                "orders",
                "id",
            ),
        ])
    }

    fn style() -> ErSvgStyle {
        ErSvgStyle {
            background: Hsla {
                h: 0.0,
                s: 0.0,
                l: 1.0,
                a: 1.0,
            },
            node_bg: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.96,
                a: 1.0,
            },
            node_border: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.7,
                a: 1.0,
            },
            accent: Hsla {
                h: 0.6,
                s: 0.6,
                l: 0.5,
                a: 1.0,
            },
            text: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.1,
                a: 1.0,
            },
            text_dim: Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.45,
                a: 1.0,
            },
            font_size: 13.0,
            node_h: 58.0,
            node_w: crate::er_diagram::NODE_W,
        }
    }

    #[test]
    fn mermaid_golden() {
        let graph = fixture();
        const EXPECTED: &str = "\
erDiagram
    public_orders {
        string user_id PK
        string id PK
    }
    public_order_items {
        string order_id PK
    }
    public_users {
        string id PK
    }
    public_users ||--o{ public_orders : \"fk_orders_user (user_id → id)\"
    public_orders ||--o{ public_order_items : \"fk_items_order (order_id → id)\"
";
        assert_eq!(er_mermaid(&graph), EXPECTED);
    }

    #[test]
    fn dbml_golden() {
        let graph = fixture();
        const EXPECTED: &str = "\
// Column types are not exported.
Table \"public\".\"orders\" {
  \"user_id\" unknown [pk]
  \"id\" unknown [pk]
}
Table \"public\".\"order_items\" {
  \"order_id\" unknown [pk]
}
Table \"public\".\"users\" {
  \"id\" unknown [pk]
}
Ref: \"public\".\"orders\".\"user_id\" > \"public\".\"users\".\"id\"
Ref: \"public\".\"order_items\".\"order_id\" > \"public\".\"orders\".\"id\"
";
        assert_eq!(er_dbml(&graph), EXPECTED);
    }

    #[test]
    fn svg_golden_structure() {
        let graph = fixture();
        let layout = layout_grid(graph.nodes.len(), NODE_W, 58.0);
        let svg = er_svg(&graph, &layout, &|i| layout.positions[i], &style());
        assert!(
            svg.starts_with("<svg xmlns="),
            "svg should start with xmlns tag, got {}",
            &svg[..svg.len().min(80)]
        );
        assert_eq!(svg.matches("<rect").count(), 3, "expected 3 node rects");
        let lines = svg.matches("<line").count() + svg.matches("<path").count();
        assert_eq!(lines, 2, "expected 2 edges");
        assert_eq!(svg.matches("<polygon").count(), 2, "expected 2 arrowheads");
        assert!(svg.contains("public.orders"));
        assert!(svg.contains("public.users"));
        assert!(svg.contains("public.order_items"));
    }

    #[test]
    fn mermaid_sanitises_names() {
        let graph = build_graph(&[fk(
            "fk",
            "my-schema",
            "odd table",
            "x",
            "my-schema",
            "other",
            "id",
        )]);
        let text = er_mermaid(&graph);
        assert!(text.contains("my_schema_odd_table"), "{text}");
        assert!(text.contains("my_schema_other"), "{text}");
        assert!(!text.contains("my-schema"));
        assert!(!text.contains("odd table"));
    }

    #[test]
    fn dbml_quotes_names() {
        let graph = build_graph(&[fk(
            "fk",
            "my schema",
            "odd-table",
            "user id",
            "my schema",
            "users",
            "id",
        )]);
        let text = er_dbml(&graph);
        assert!(text.contains("Table \"my schema\".\"odd-table\""), "{text}");
        assert!(text.contains("\"user id\" unknown [pk]"), "{text}");
        assert!(text.contains("Ref:"), "{text}");
    }
}
