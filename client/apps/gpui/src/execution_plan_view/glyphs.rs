//! Small line glyphs for operator families and finding severities.
//!
//! Each glyph is a few polylines in a unit square, stored as static data and
//! stroked at whatever size the caller asks for. The diagram paints hundreds
//! of them per frame, so there is no allocation here beyond the path itself.
//! The shapes carry meaning on their own (a funnel filters, bars sort), so a
//! reader who cannot tell the heat colours apart still sees what a card is.

use gpui::{Bounds, Hsla, IntoElement, Pixels, Styled, Window, canvas, point, px};
use sqlly_services::plans::{FindingSeverity, OperatorCategory};

type Stroke = &'static [(f32, f32)];

/// A glyph: open polylines to stroke and closed polygons to fill.
pub(super) struct Glyph {
    strokes: &'static [Stroke],
    fills: &'static [Stroke],
}

const fn lines(strokes: &'static [Stroke]) -> Glyph {
    Glyph {
        strokes,
        fills: &[],
    }
}

/// The glyph for an operator family.
pub(super) fn category_glyph(category: OperatorCategory) -> Glyph {
    use OperatorCategory as C;
    match category {
        // Rows of a table, read top to bottom.
        C::Scan => lines(&[
            &[(0.1, 0.22), (0.9, 0.22)],
            &[(0.1, 0.5), (0.9, 0.5)],
            &[(0.1, 0.78), (0.9, 0.78)],
        ]),
        // One row picked out of the table.
        C::Seek => Glyph {
            strokes: &[
                &[(0.1, 0.2), (0.9, 0.2)],
                &[(0.1, 0.8), (0.9, 0.8)],
                &[(0.1, 0.5), (0.45, 0.5)],
            ],
            fills: &[&[(0.5, 0.3), (0.9, 0.5), (0.5, 0.7)]],
        },
        // Out to the table and back.
        C::Lookup => lines(&[
            &[(0.15, 0.2), (0.15, 0.75), (0.85, 0.75)],
            &[(0.62, 0.52), (0.85, 0.75), (0.62, 0.98)],
        ]),
        // A loop inside a loop.
        C::JoinNestedLoop => lines(&[
            &[(0.1, 0.1), (0.9, 0.1), (0.9, 0.9), (0.1, 0.9), (0.1, 0.1)],
            &[
                (0.35, 0.35),
                (0.65, 0.35),
                (0.65, 0.65),
                (0.35, 0.65),
                (0.35, 0.35),
            ],
        ]),
        // A hash sign.
        C::JoinHash => lines(&[
            &[(0.35, 0.1), (0.25, 0.9)],
            &[(0.75, 0.1), (0.65, 0.9)],
            &[(0.1, 0.35), (0.9, 0.35)],
            &[(0.1, 0.65), (0.9, 0.65)],
        ]),
        // Two ordered inputs zipped into one.
        C::JoinMerge => lines(&[
            &[(0.1, 0.15), (0.45, 0.5), (0.1, 0.85)],
            &[(0.45, 0.5), (0.9, 0.5)],
        ]),
        // Two inputs meeting.
        C::Join => lines(&[
            &[(0.1, 0.2), (0.6, 0.2), (0.6, 0.7), (0.1, 0.7), (0.1, 0.2)],
            &[(0.4, 0.4), (0.9, 0.4), (0.9, 0.9), (0.4, 0.9), (0.4, 0.4)],
        ]),
        // Bars in ascending order.
        C::Sort => lines(&[
            &[(0.1, 0.2), (0.4, 0.2)],
            &[(0.1, 0.5), (0.65, 0.5)],
            &[(0.1, 0.8), (0.9, 0.8)],
        ]),
        // A sigma.
        C::Aggregate => lines(&[&[
            (0.85, 0.15),
            (0.15, 0.15),
            (0.55, 0.5),
            (0.15, 0.85),
            (0.85, 0.85),
        ]]),
        // A frame sliding over rows.
        C::Window => lines(&[
            &[
                (0.1, 0.15),
                (0.9, 0.15),
                (0.9, 0.85),
                (0.1, 0.85),
                (0.1, 0.15),
            ],
            &[(0.1, 0.4), (0.9, 0.4)],
            &[(0.5, 0.4), (0.5, 0.85)],
        ]),
        // Rows set aside in a stack.
        C::Spool => lines(&[
            &[(0.1, 0.1), (0.9, 0.1), (0.9, 0.4), (0.1, 0.4), (0.1, 0.1)],
            &[(0.1, 0.6), (0.9, 0.6), (0.9, 0.9), (0.1, 0.9), (0.1, 0.6)],
        ]),
        // Streams running side by side.
        C::Parallelism => lines(&[
            &[(0.2, 0.1), (0.2, 0.9)],
            &[(0.5, 0.1), (0.5, 0.9)],
            &[(0.8, 0.1), (0.8, 0.9)],
        ]),
        // A funnel.
        C::Filter => lines(&[&[
            (0.1, 0.15),
            (0.9, 0.15),
            (0.58, 0.55),
            (0.58, 0.9),
            (0.42, 0.9),
            (0.42, 0.55),
            (0.1, 0.15),
        ]]),
        // A plus sign: something is worked out per row.
        C::Compute => lines(&[&[(0.5, 0.15), (0.5, 0.85)], &[(0.15, 0.5), (0.85, 0.5)]]),
        // A pencil stroke.
        C::Modify => lines(&[&[
            (0.15, 0.85),
            (0.3, 0.55),
            (0.75, 0.1),
            (0.9, 0.25),
            (0.45, 0.7),
            (0.15, 0.85),
        ]]),
        // An arrow leaving the box.
        C::Remote => lines(&[
            &[
                (0.6, 0.15),
                (0.1, 0.15),
                (0.1, 0.9),
                (0.85, 0.9),
                (0.85, 0.5),
            ],
            &[(0.45, 0.55), (0.9, 0.1)],
            &[(0.62, 0.1), (0.9, 0.1), (0.9, 0.38)],
        ]),
        // A union cup.
        C::SetOp => lines(&[&[
            (0.15, 0.1),
            (0.15, 0.65),
            (0.35, 0.9),
            (0.65, 0.9),
            (0.85, 0.65),
            (0.85, 0.1),
        ]]),
        // Rows stop at a line.
        C::Limit => lines(&[
            &[(0.1, 0.85), (0.9, 0.85)],
            &[(0.5, 0.1), (0.5, 0.65)],
            &[(0.28, 0.43), (0.5, 0.65), (0.72, 0.43)],
        ]),
        // Brackets around an inner query.
        C::Subquery => lines(&[
            &[(0.38, 0.1), (0.15, 0.1), (0.15, 0.9), (0.38, 0.9)],
            &[(0.62, 0.1), (0.85, 0.1), (0.85, 0.9), (0.62, 0.9)],
        ]),
        // A lambda.
        C::Function => lines(&[
            &[(0.2, 0.1), (0.35, 0.1), (0.8, 0.9)],
            &[(0.5, 0.42), (0.2, 0.9)],
        ]),
        // A single value.
        C::Constant => Glyph {
            strokes: &[],
            fills: &[&[(0.3, 0.3), (0.7, 0.3), (0.7, 0.7), (0.3, 0.7)]],
        },
        // Rows handed back to the client.
        C::Result => lines(&[
            &[(0.5, 0.9), (0.5, 0.15)],
            &[(0.25, 0.4), (0.5, 0.15), (0.75, 0.4)],
        ]),
        // A padlock.
        C::Lock => lines(&[
            &[
                (0.15, 0.45),
                (0.85, 0.45),
                (0.85, 0.9),
                (0.15, 0.9),
                (0.15, 0.45),
            ],
            &[
                (0.3, 0.45),
                (0.3, 0.2),
                (0.42, 0.1),
                (0.58, 0.1),
                (0.7, 0.2),
                (0.7, 0.45),
            ],
        ]),
        C::Other => lines(&[&[
            (0.5, 0.15),
            (0.85, 0.5),
            (0.5, 0.85),
            (0.15, 0.5),
            (0.5, 0.15),
        ]]),
    }
}

/// The mark for a finding severity. The three differ in shape as well as in
/// colour: a filled triangle, an outlined triangle, an outlined circle-ish
/// octagon.
pub(super) fn severity_glyph(severity: FindingSeverity) -> Glyph {
    match severity {
        FindingSeverity::Critical => Glyph {
            strokes: &[],
            fills: &[&[(0.5, 0.05), (0.98, 0.92), (0.02, 0.92)]],
        },
        FindingSeverity::Warning => lines(&[
            &[(0.5, 0.1), (0.94, 0.9), (0.06, 0.9), (0.5, 0.1)],
            &[(0.5, 0.4), (0.5, 0.68)],
        ]),
        FindingSeverity::Info => lines(&[
            &[
                (0.32, 0.08),
                (0.68, 0.08),
                (0.92, 0.32),
                (0.92, 0.68),
                (0.68, 0.92),
                (0.32, 0.92),
                (0.08, 0.68),
                (0.08, 0.32),
                (0.32, 0.08),
            ],
            &[(0.5, 0.45), (0.5, 0.72)],
        ]),
    }
}

/// A chevron for fold toggles drawn on a canvas: pointing down when the
/// subtree is open, right when it is folded.
pub(super) fn fold_glyph(folded: bool) -> Glyph {
    if folded {
        lines(&[&[(0.35, 0.15), (0.7, 0.5), (0.35, 0.85)]])
    } else {
        lines(&[&[(0.15, 0.35), (0.5, 0.7), (0.85, 0.35)]])
    }
}

impl Glyph {
    /// Stroke the glyph into `bounds`.
    pub(super) fn paint(&self, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
        let (x, y) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if w < 3.0 || h < 3.0 {
            return;
        }
        let at = |p: &(f32, f32)| point(px(x + p.0 * w), px(y + p.1 * h));
        let weight = (w / 9.0).clamp(1.0, 2.0);
        for stroke in self.strokes {
            let Some((first, rest)) = stroke.split_first() else {
                continue;
            };
            let mut path = gpui::PathBuilder::stroke(px(weight));
            path.move_to(at(first));
            for p in rest {
                path.line_to(at(p));
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
        for fill in self.fills {
            let Some((first, rest)) = fill.split_first() else {
                continue;
            };
            let mut path = gpui::PathBuilder::fill();
            path.move_to(at(first));
            for p in rest {
                path.line_to(at(p));
            }
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    }

    /// The glyph as an element of `size` pixels, for use in ordinary layout.
    pub(super) fn element(self, size: f32, color: Hsla) -> impl IntoElement {
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| self.paint(bounds, color, window),
        )
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
    }
}
