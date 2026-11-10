//! Result layout plan — parsed SQL comment-driven result layout (`-- sqlly 4x2`,
//! `-- sqlly result: row|column`). Pure scanner + placement model.
//!
//! Port of `SQLLYExports/QueryResultLayoutPlan.swift`. No I/O, no UI — fully
//! unit-testable.

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResultLayoutDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    /// 1-based SQL line for problems found statically by [`parse`] (these
    /// surface as editor squiggles via `sqlly_directive::directive_diagnostics`);
    /// `None` for runtime problems only known once the result sets arrive
    /// ([`QueryResultLayoutSpec::assigning_result_sets`]).
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

impl QueryResultLayoutDiagnostic {
    pub fn warning(message: impl Into<String>, line: Option<usize>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            line,
        }
    }
    pub fn error(message: impl Into<String>, line: Option<usize>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            line,
        }
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResultPlacement {
    pub statement_index: i32,
    pub result_set_index: Option<usize>,
    pub row_start: usize,
    pub row_end: usize,
    pub column_start: usize,
    pub column_end: usize,
}

impl QueryResultPlacement {
    pub fn new(
        statement_index: i32,
        result_set_index: Option<usize>,
        row_start: usize,
        row_end: usize,
        column_start: usize,
        column_end: usize,
    ) -> Self {
        Self {
            statement_index,
            result_set_index,
            row_start,
            row_end,
            column_start,
            column_end,
        }
    }

    pub fn row_span(&self) -> usize {
        self.row_end - self.row_start + 1
    }
    pub fn column_span(&self) -> usize {
        self.column_end - self.column_start + 1
    }
    #[allow(dead_code)]
    pub fn is_spanned(&self) -> bool {
        self.row_span() > 1 || self.column_span() > 1
    }

    pub fn overlaps(&self, other: &QueryResultPlacement) -> bool {
        !(self.row_end < other.row_start
            || self.row_start > other.row_end
            || self.column_end < other.column_start
            || self.column_start > other.column_end)
    }
}

// ---------------------------------------------------------------------------
// Layout spec
// ---------------------------------------------------------------------------

/// Maximum canvas size in each dimension.
pub const MAX_CANVAS_SIZE: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResultLayoutSpec {
    pub rows: usize,
    pub columns: usize,
    pub placements: Vec<QueryResultPlacement>,
    pub diagnostics: Vec<QueryResultLayoutDiagnostic>,
}

impl QueryResultLayoutSpec {
    pub fn none() -> Self {
        Self {
            rows: 0,
            columns: 0,
            placements: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn has_layout(&self) -> bool {
        self.rows > 0 && self.columns > 0
    }

    pub fn is_valid(&self) -> bool {
        self.has_layout()
            && !self
                .diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error)
    }

    /// Problems only known once result sets arrive (a statement returned
    /// more sets than the board has open cells). Static problems carry a
    /// line and are reported in the editor instead.
    pub fn runtime_diagnostics(&self) -> impl Iterator<Item = &QueryResultLayoutDiagnostic> {
        self.diagnostics.iter().filter(|d| d.line.is_none())
    }

    pub fn placement_at(&self, row: usize, column: usize) -> Option<&QueryResultPlacement> {
        self.placements.iter().find(|p| {
            row >= p.row_start
                && row <= p.row_end
                && column >= p.column_start
                && column <= p.column_end
        })
    }

    pub fn next_open_cell(&self) -> Option<(usize, usize)> {
        self.next_open_cell_occupied(&self.placements)
    }

    pub fn next_open_cell_occupied(
        &self,
        occupied: &[QueryResultPlacement],
    ) -> Option<(usize, usize)> {
        for row in 0..self.rows {
            for col in 0..self.columns {
                let blocked = occupied.iter().any(|p| {
                    row >= p.row_start
                        && row <= p.row_end
                        && col >= p.column_start
                        && col <= p.column_end
                });
                if !blocked {
                    return Some((row, col));
                }
            }
        }
        None
    }

    /// Assigns result-set indexes to parsed statement placements and places any
    /// extra result sets in the next available cells.
    pub fn assigning_result_sets(&self, result_set_count: usize) -> QueryResultLayoutSpec {
        if !self.has_layout() {
            return self.clone();
        }
        let mut resolved = self.clone();
        resolved.placements = self
            .placements
            .iter()
            .map(|p| {
                let mut copy = p.clone();
                if copy.statement_index >= 0 && (copy.statement_index as usize) < result_set_count {
                    copy.result_set_index = Some(copy.statement_index as usize);
                }
                copy
            })
            .collect();

        let mut assigned: std::collections::BTreeSet<usize> = resolved
            .placements
            .iter()
            .filter_map(|p| p.result_set_index)
            .collect();

        for rs_idx in 0..result_set_count {
            if assigned.contains(&rs_idx) {
                continue;
            }
            match resolved.next_open_cell_occupied(&resolved.placements) {
                Some((row, col)) => {
                    resolved.placements.push(QueryResultPlacement::new(
                        rs_idx as i32,
                        Some(rs_idx),
                        row,
                        row,
                        col,
                        col,
                    ));
                    assigned.insert(rs_idx);
                }
                None => {
                    resolved
                        .diagnostics
                        .push(QueryResultLayoutDiagnostic::warning(
                            layout_text(
                                "results.layout.no_open_cell_for_result",
                                &[("result_number", &(rs_idx + 1).to_string())],
                            ),
                            None,
                        ));
                }
            }
        }
        resolved
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

fn layout_text(key: &'static str, arguments: &[(&str, &str)]) -> String {
    crate::localization::format(key, arguments)
}

/// Pure, SQL-aware scanner for the result-layout comment grammar.
pub fn parse(sql: &str) -> QueryResultLayoutSpec {
    parse_with_limits(sql, MAX_CANVAS_SIZE, MAX_CANVAS_SIZE)
}

pub fn parse_with_limits(sql: &str, max_rows: usize, max_columns: usize) -> QueryResultLayoutSpec {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let normalized = sql.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let Some((first_index, first_line)) =
        lines.iter().enumerate().find(|(_, l)| !l.trim().is_empty())
    else {
        return QueryResultLayoutSpec::none();
    };
    // 1-based, like every other diagnostic line: the canvas directive is the
    // first non-empty line, which need not be line 1.
    let canvas_line = first_index + 1;

    let Some(canvas) = parse_canvas(first_line) else {
        return QueryResultLayoutSpec::none();
    };
    let mut diagnostics = Vec::new();

    if canvas.0 == 0 || canvas.1 == 0 {
        diagnostics.push(QueryResultLayoutDiagnostic::error(
            layout_text("results.layout.nonpositive_canvas", &[]),
            Some(canvas_line),
        ));
        return QueryResultLayoutSpec {
            rows: 0,
            columns: 0,
            placements: Vec::new(),
            diagnostics,
        };
    }
    if canvas.0 > max_rows || canvas.1 > max_columns {
        diagnostics.push(QueryResultLayoutDiagnostic::error(
            layout_text(
                "results.layout.canvas_too_large",
                &[
                    ("rows", &canvas.0.to_string()),
                    ("columns", &canvas.1.to_string()),
                    ("maximum_rows", &max_rows.to_string()),
                    ("maximum_columns", &max_columns.to_string()),
                ],
            ),
            Some(canvas_line),
        ));
        return QueryResultLayoutSpec {
            rows: canvas.0,
            columns: canvas.1,
            placements: Vec::new(),
            diagnostics,
        };
    }

    let statements = scan_result_producing_statements(&lines);
    let mut placements: Vec<QueryResultPlacement> = Vec::new();
    let mut statement_had_placement: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();

    for (stmt_idx, statement) in statements.iter().enumerate() {
        let Some(marker) = &statement.placement else {
            continue;
        };
        let (placement, error) = parse_placement(&marker.value, stmt_idx as i32);
        if let Some(p) = placement {
            if p.row_start >= canvas.0
                || p.column_start >= canvas.1
                || p.row_end >= canvas.0
                || p.column_end >= canvas.1
            {
                diagnostics.push(QueryResultLayoutDiagnostic::warning(
                    layout_text(
                        "results.layout.placement_outside_canvas",
                        &[
                            ("placement", &marker.value),
                            ("rows", &canvas.0.to_string()),
                            ("columns", &canvas.1.to_string()),
                        ],
                    ),
                    Some(marker.line),
                ));
            } else if let Some(conflict) = placements.iter().find(|existing| existing.overlaps(&p))
            {
                diagnostics.push(QueryResultLayoutDiagnostic::warning(
                    layout_text(
                        "results.layout.placement_overlap",
                        &[
                            ("placement", &marker.value),
                            ("conflicting_placement", &format_placement(conflict)),
                        ],
                    ),
                    Some(marker.line),
                ));
            } else {
                placements.push(p);
                statement_had_placement.insert(stmt_idx);
            }
        } else if let Some(msg) = error {
            diagnostics.push(QueryResultLayoutDiagnostic::warning(msg, Some(marker.line)));
        }
    }

    // Auto-place statements without explicit placement.
    for (stmt_idx, stmt) in statements.iter().enumerate() {
        if statement_had_placement.contains(&stmt_idx) {
            continue;
        }
        let temp_spec = QueryResultLayoutSpec {
            rows: canvas.0,
            columns: canvas.1,
            placements: placements.clone(),
            diagnostics: Vec::new(),
        };
        match temp_spec.next_open_cell() {
            Some((row, col)) => {
                placements.push(QueryResultPlacement::new(
                    stmt_idx as i32,
                    None,
                    row,
                    row,
                    col,
                    col,
                ));
            }
            None => {
                diagnostics.push(QueryResultLayoutDiagnostic::warning(
                    layout_text(
                        "results.layout.no_open_cell_for_statement",
                        &[("statement_number", &(stmt_idx + 1).to_string())],
                    ),
                    // On the statement's own placement directive when it
                    // has one (it overlapped or fell outside the board), so
                    // the squiggle lands on the comment to fix.
                    Some(
                        stmt.placement
                            .as_ref()
                            .map_or(stmt.start_line, |marker| marker.line),
                    ),
                ));
            }
        }
    }

    // Dangling placement markers.
    for marker in dangling_placement_markers(&lines, &statements) {
        diagnostics.push(QueryResultLayoutDiagnostic::warning(
            layout_text(
                "results.layout.dangling_placement",
                &[("placement", &marker.value)],
            ),
            Some(marker.line),
        ));
    }

    // Sort placements.
    placements.sort_by(|a, b| {
        a.statement_index
            .cmp(&b.statement_index)
            .then(a.row_start.cmp(&b.row_start))
            .then(a.column_start.cmp(&b.column_start))
    });

    QueryResultLayoutSpec {
        rows: canvas.0,
        columns: canvas.1,
        placements,
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// Layout directives: `-- sqlly layout:` and `-- sqlly lines:`
// ---------------------------------------------------------------------------

/// The result layout a script asks for with `-- sqlly layout:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedLayout {
    Stacked,
    Tabbed,
    Canvas,
}

/// Largest row allocation `-- sqlly lines:` accepts.
pub const MAX_LINES: usize = 500;

/// What the script's `-- sqlly layout:` / `-- sqlly lines:` comments ask
/// for. Both are honored on any line whose only content is the comment; the
/// first `layout:` wins and a later `lines:` for the same target replaces an
/// earlier one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutDirectives {
    pub layout: Option<RequestedLayout>,
    /// Rows for every result set without an allocation of its own.
    pub default_lines: Option<usize>,
    /// `(1-based result-set index, rows)` from `lines <n>: <rows>`.
    pub lines: Vec<(usize, usize)>,
    /// 1-based result set to show by default (`-- sqlly focus: <n>`); a
    /// later valid focus overrides an earlier one.
    pub focus: Option<usize>,
}

impl LayoutDirectives {
    /// 0-based slot of the result set `-- sqlly focus:` asks to show first.
    pub fn focus_slot(&self) -> Option<usize> {
        self.focus.map(|n| n - 1)
    }

    /// Rows allotted to the result set in 0-based `slot`, if any.
    pub fn lines_for(&self, slot: usize) -> Option<usize> {
        self.lines
            .iter()
            .find(|(index, _)| *index == slot + 1)
            .map(|(_, rows)| *rows)
            .or(self.default_lines)
    }
}

/// Largest result-set number `-- sqlly focus:` accepts.
pub const MAX_FOCUS: usize = 10_000;

/// A 1-based result-set number from 1 to [`MAX_FOCUS`], digits only.
pub fn parse_focus_value(value: &str) -> Option<usize> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = value.parse().ok()?;
    (1..=MAX_FOCUS).contains(&n).then_some(n)
}

/// One line of `sql` whose only content is a `--` comment (outside strings
/// and block comments) — the lines placement-style directives are honored
/// on. Offsets are byte positions in `sql`; `end` excludes the terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneComment {
    pub start: usize,
    pub end: usize,
    /// The comment text after its `--`.
    pub body: String,
}

/// Every whole-line `--` comment in `sql`, in document order, using the
/// same lexical state the directive parsers use.
pub fn standalone_comments(sql: &str) -> Vec<StandaloneComment> {
    // Lock-block directives are whole-line comments too (same offsets).
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let mut state = LineAnalysisState::default();
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for raw in sql.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let analysis = analyze_line(line, &mut state);
        if analysis.code.trim().is_empty()
            && let Some(body) = analysis.line_comment_body
        {
            out.push(StandaloneComment {
                start: line_start,
                end: line_start + line.len(),
                body,
            });
        }
        line_start += raw.len() + 1;
    }
    out
}

/// `stacked`, `tabbed` (or `tabs`), `canvas` — case-insensitive, trimmed.
pub fn parse_requested_layout(value: &str) -> Option<RequestedLayout> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("stacked") {
        Some(RequestedLayout::Stacked)
    } else if value.eq_ignore_ascii_case("tabbed") || value.eq_ignore_ascii_case("tabs") {
        Some(RequestedLayout::Tabbed)
    } else if value.eq_ignore_ascii_case("canvas") {
        Some(RequestedLayout::Canvas)
    } else {
        None
    }
}

/// A row count from 1 to [`MAX_LINES`], digits only.
pub fn parse_lines_value(value: &str) -> Option<usize> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rows: usize = value.parse().ok()?;
    (1..=MAX_LINES).contains(&rows).then_some(rows)
}

/// Scan `sql` for its layout directives. Comments inside strings or block
/// comments are not directives, and a directive after code on its line is
/// ignored (as every placement-style directive is).
pub fn parse_layout_directives(sql: &str) -> LayoutDirectives {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let normalized = sql.replace("\r\n", "\n").replace('\r', "\n");
    let mut state = LineAnalysisState::default();
    let mut out = LayoutDirectives::default();
    for line in normalized.split('\n') {
        let analysis = analyze_line(line, &mut state);
        if !analysis.code.trim().is_empty() {
            continue;
        }
        let Some(body) = analysis.line_comment_body.as_deref() else {
            continue;
        };
        match crate::sqlly_directive::classify_body(body) {
            Some(crate::sqlly_directive::Directive::Layout { body }) => {
                if out.layout.is_none() {
                    out.layout = parse_requested_layout(body);
                }
            }
            Some(crate::sqlly_directive::Directive::Focus { body }) => {
                if let Some(n) = parse_focus_value(body) {
                    out.focus = Some(n);
                }
            }
            Some(crate::sqlly_directive::Directive::Lines { index, body }) => {
                let Some(rows) = parse_lines_value(body) else {
                    continue;
                };
                match index {
                    None => out.default_lines = Some(rows),
                    Some(index) => {
                        if let Some(entry) = out.lines.iter_mut().find(|(i, _)| *i == index) {
                            entry.1 = rows;
                        } else {
                            out.lines.push((index, rows));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Parser internals
// ---------------------------------------------------------------------------

struct PlacementMarker {
    value: String,
    line: usize,
}

struct Statement {
    #[allow(dead_code)]
    text: String,
    start_line: usize,
    placement: Option<PlacementMarker>,
}

fn parse_canvas(line: &str) -> Option<(usize, usize)> {
    match crate::sqlly_directive::classify(line.trim())? {
        crate::sqlly_directive::Directive::Canvas { rows, columns } => Some((rows, columns)),
        _ => None,
    }
}

fn parse_placement(
    value: &str,
    statement_index: i32,
) -> (Option<QueryResultPlacement>, Option<String>) {
    let parts: Vec<&str> = value.split('|').map(|p| p.trim()).collect();
    if parts.len() != 2 {
        return (
            None,
            Some(layout_text(
                "results.layout.malformed_placement_separator",
                &[("placement", value)],
            )),
        );
    }
    let Some(row) = parse_range(parts[0]) else {
        return (
            None,
            Some(layout_text(
                "results.layout.malformed_placement_ranges",
                &[("placement", value)],
            )),
        );
    };
    let Some(column) = parse_range(parts[1]) else {
        return (
            None,
            Some(layout_text(
                "results.layout.malformed_placement_ranges",
                &[("placement", value)],
            )),
        );
    };
    if row.1 < row.0 || column.1 < column.0 {
        return (
            None,
            Some(layout_text(
                "results.layout.reversed_placement_range",
                &[("placement", value)],
            )),
        );
    }
    (
        Some(QueryResultPlacement::new(
            statement_index,
            None,
            row.0 - 1,
            row.1 - 1,
            column.0 - 1,
            column.1 - 1,
        )),
        None,
    )
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let pieces: Vec<&str> = value.split('-').map(|p| p.trim()).collect();
    if pieces.is_empty() || pieces.len() > 2 {
        return None;
    }
    let start: usize = pieces[0].parse().ok()?;
    if start == 0 {
        return None;
    }
    if pieces.len() == 1 {
        return Some((start, start));
    }
    let end: usize = pieces[1].parse().ok()?;
    if end == 0 {
        return None;
    }
    Some((start, end))
}

fn scan_result_producing_statements(lines: &[&str]) -> Vec<Statement> {
    let mut state = LineAnalysisState::default();
    let mut current = String::new();
    let mut current_start_line: Option<usize> = None;
    let mut pending_placement: Option<PlacementMarker> = None;
    let mut statements: Vec<Statement> = Vec::new();

    for (zero_based_line, line) in lines.iter().enumerate() {
        let line_number = zero_based_line + 1;
        let analysis = analyze_line(line, &mut state);
        let trimmed_code = analysis.code.trim();

        if trimmed_code.is_empty()
            && let Some(marker_value) =
                parse_placement_comment(analysis.line_comment_body.as_deref())
        {
            // Flush any pending statement before binding this placement to the next.
            flush_statement(
                &mut current,
                &mut current_start_line,
                &mut pending_placement,
                &mut statements,
            );
            pending_placement = Some(PlacementMarker {
                value: marker_value,
                line: line_number,
            });
            continue;
        }

        if trimmed_code.eq_ignore_ascii_case("GO") {
            flush_statement(
                &mut current,
                &mut current_start_line,
                &mut pending_placement,
                &mut statements,
            );
            continue;
        }

        let mut segment = String::new();
        for ch in analysis.code.chars() {
            if ch == ';' {
                append_code(&mut current, &mut current_start_line, &segment, line_number);
                flush_statement(
                    &mut current,
                    &mut current_start_line,
                    &mut pending_placement,
                    &mut statements,
                );
                segment.clear();
            } else {
                segment.push(ch);
            }
        }
        if !segment.is_empty() {
            segment.push('\n');
            append_code(&mut current, &mut current_start_line, &segment, line_number);
        }
    }
    flush_statement(
        &mut current,
        &mut current_start_line,
        &mut pending_placement,
        &mut statements,
    );
    statements
}

fn append_code(
    current: &mut String,
    current_start_line: &mut Option<usize>,
    code: &str,
    line: usize,
) {
    if current_start_line.is_none() && !code.trim().is_empty() {
        *current_start_line = Some(line);
    }
    current.push_str(code);
}

fn flush_statement(
    current: &mut String,
    current_start_line: &mut Option<usize>,
    pending_placement: &mut Option<PlacementMarker>,
    statements: &mut Vec<Statement>,
) {
    let text = current.trim().to_string();
    current.clear();
    let start = current_start_line.take();
    if text.is_empty() {
        return;
    }
    if is_result_producing(&text) {
        statements.push(Statement {
            text,
            start_line: start.unwrap_or(1),
            placement: pending_placement.take(),
        });
    } else {
        pending_placement.take();
    }
}

fn dangling_placement_markers(lines: &[&str], statements: &[Statement]) -> Vec<PlacementMarker> {
    let mut markers = Vec::new();
    let mut state = LineAnalysisState::default();
    for (zero_based_line, line) in lines.iter().enumerate() {
        let analysis = analyze_line(line, &mut state);
        if !analysis.code.trim().is_empty() {
            continue;
        }
        let Some(value) = parse_placement_comment(analysis.line_comment_body.as_deref()) else {
            continue;
        };
        let marker = PlacementMarker {
            value: value.clone(),
            line: zero_based_line + 1,
        };
        let was_used = statements.iter().any(|s| {
            s.placement
                .as_ref()
                .map(|p| p.value == value)
                .unwrap_or(false)
        });
        if !was_used {
            markers.push(marker);
        }
    }
    markers
}

/// `body` is the comment text after its `--` (from `analyze_line`).
fn parse_placement_comment(body: Option<&str>) -> Option<String> {
    match crate::sqlly_directive::classify_body(body?)? {
        crate::sqlly_directive::Directive::Result { body } => Some(body.trim().to_string()),
        _ => None,
    }
}

fn is_result_producing(statement: &str) -> bool {
    let first = statement
        .split_whitespace()
        .next()
        .map(|s| s.to_ascii_lowercase());
    matches!(
        first.as_deref(),
        Some("select")
            | Some("with")
            | Some("exec")
            | Some("execute")
            | Some("values")
            | Some("table")
    )
}

fn format_placement(p: &QueryResultPlacement) -> String {
    format!(
        "{}-{}|{}-{}",
        p.row_start + 1,
        p.row_end + 1,
        p.column_start + 1,
        p.column_end + 1
    )
}

#[derive(Default)]
struct LineAnalysisState {
    in_block_comment: bool,
    in_string: bool,
    in_bracket_identifier: bool,
}

struct LineAnalysis {
    code: String,
    line_comment_body: Option<String>,
}

fn analyze_line(line: &str, state: &mut LineAnalysisState) -> LineAnalysis {
    let chars: Vec<char> = line.chars().collect();
    let mut code = String::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        let next = if index + 1 < chars.len() {
            Some(chars[index + 1])
        } else {
            None
        };

        if state.in_block_comment {
            if ch == '*' && next == Some('/') {
                state.in_block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        if state.in_string {
            if ch == '\'' && next == Some('\'') {
                code.push_str("  ");
                index += 2;
            } else if ch == '\'' {
                state.in_string = false;
                code.push(' ');
                index += 1;
            } else {
                code.push(if ch == '\t' { '\t' } else { ' ' });
                index += 1;
            }
            continue;
        }

        if state.in_bracket_identifier {
            if ch == ']' {
                state.in_bracket_identifier = false;
            }
            code.push(if ch == ';' { ' ' } else { ch });
            index += 1;
            continue;
        }

        if ch == '\'' {
            state.in_string = true;
            code.push(' ');
            index += 1;
        } else if ch == '[' {
            state.in_bracket_identifier = true;
            code.push(ch);
            index += 1;
        } else if ch == '/' && next == Some('*') {
            state.in_block_comment = true;
            index += 2;
        } else if ch == '-' && next == Some('-') {
            let body_start = index + 2;
            let body = if body_start < chars.len() {
                chars[body_start..].iter().collect()
            } else {
                String::new()
            };
            return LineAnalysis {
                code,
                line_comment_body: Some(body),
            };
        } else {
            code.push(ch);
            index += 1;
        }
    }
    LineAnalysis {
        code,
        line_comment_body: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_directives_parse_layout_default_and_indexed_lines() {
        let sql = "-- sqlly layout: Stacked\n-- sqlly lines: 8\nSELECT 1; -- sqlly lines 1: 99\n-- sqlly lines 2: 20\n-- sqlly lines 2: 30\n/* -- sqlly layout: canvas */\nSELECT 2;";
        let d = parse_layout_directives(sql);
        assert_eq!(d.layout, Some(RequestedLayout::Stacked));
        assert_eq!(d.default_lines, Some(8));
        // The trailing comment after code is ignored; the later indexed line
        // overrides the earlier one.
        assert_eq!(d.lines, vec![(2, 30)]);
        assert_eq!(d.lines_for(0), Some(8));
        assert_eq!(d.lines_for(1), Some(30));
        assert_eq!(d.lines_for(5), Some(8));
        assert_eq!(parse_layout_directives("SELECT 1").lines_for(0), None);
    }

    #[test]
    fn focus_directive_parses_and_later_wins() {
        let d =
            parse_layout_directives("-- sqlly focus: 2\nSELECT 1;\n-- sqlly focus: 3\nSELECT 2;");
        assert_eq!(d.focus, Some(3));
        assert_eq!(d.focus_slot(), Some(2));
        // Invalid values never clobber a valid one; trailing comments and
        // comments inside strings are not directives.
        let d = parse_layout_directives(
            "-- sqlly focus: 2\n-- sqlly focus: 0\n-- sqlly focus: x\nSELECT 1 -- sqlly focus: 4\nSELECT '\n-- sqlly focus: 5\n';",
        );
        assert_eq!(d.focus, Some(2));
        assert_eq!(parse_layout_directives("SELECT 1").focus_slot(), None);
        assert_eq!(parse_focus_value(" 7 "), Some(7));
        assert_eq!(parse_focus_value("0"), None);
        assert_eq!(parse_focus_value("-1"), None);
    }

    #[test]
    fn standalone_comments_report_offsets_and_skip_code_and_strings() {
        let sql = "-- a\nSELECT 1 -- b\n  -- c\nSELECT '\n-- d\n';\n/*\n-- e\n*/";
        let found = standalone_comments(sql);
        let bodies: Vec<&str> = found.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec![" a", " c"]);
        assert_eq!(&sql[found[1].start..found[1].end], "  -- c");
    }

    #[test]
    fn layout_directives_first_layout_wins_and_values_are_validated() {
        let d = parse_layout_directives(
            "-- sqlly layout: tabs\n-- sqlly layout: canvas\n-- sqlly lines: 0\n-- sqlly lines: 9999\n-- sqlly lines 3: x\nSELECT 1",
        );
        assert_eq!(d.layout, Some(RequestedLayout::Tabbed));
        assert_eq!(d.default_lines, None);
        assert!(d.lines.is_empty());
        assert_eq!(
            parse_requested_layout(" CANVAS "),
            Some(RequestedLayout::Canvas)
        );
        assert_eq!(parse_requested_layout("grid"), None);
        assert_eq!(parse_lines_value(" 12 "), Some(12));
        assert_eq!(parse_lines_value("-1"), None);
    }

    #[test]
    fn placement_overlaps_basic() {
        let a = QueryResultPlacement::new(0, None, 0, 1, 0, 1);
        let b = QueryResultPlacement::new(1, None, 1, 2, 1, 2);
        assert!(a.overlaps(&b));
        let c = QueryResultPlacement::new(2, None, 2, 3, 0, 1);
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn placement_spans() {
        let p = QueryResultPlacement::new(0, None, 0, 2, 0, 3);
        assert_eq!(p.row_span(), 3);
        assert_eq!(p.column_span(), 4);
        assert!(p.is_spanned());
        let q = QueryResultPlacement::new(0, None, 0, 0, 0, 0);
        assert!(!q.is_spanned());
    }

    #[test]
    fn spec_none_is_empty() {
        let s = QueryResultLayoutSpec::none();
        assert!(!s.has_layout());
        assert!(!s.is_valid());
    }

    #[test]
    fn spec_has_layout() {
        let s = QueryResultLayoutSpec {
            rows: 2,
            columns: 3,
            placements: Vec::new(),
            diagnostics: Vec::new(),
        };
        assert!(s.has_layout());
        assert!(s.is_valid());
    }

    #[test]
    fn spec_placement_at_finds_occupied() {
        let p = QueryResultPlacement::new(0, None, 0, 1, 0, 1);
        let s = QueryResultLayoutSpec {
            rows: 4,
            columns: 4,
            placements: vec![p],
            diagnostics: Vec::new(),
        };
        assert!(s.placement_at(0, 0).is_some());
        assert!(s.placement_at(1, 1).is_some());
        assert!(s.placement_at(2, 2).is_none());
    }

    #[test]
    fn spec_next_open_cell() {
        let p = QueryResultPlacement::new(0, None, 0, 0, 0, 0);
        let s = QueryResultLayoutSpec {
            rows: 2,
            columns: 2,
            placements: vec![p],
            diagnostics: Vec::new(),
        };
        assert_eq!(s.next_open_cell(), Some((0, 1)));
    }

    #[test]
    fn spec_next_open_cell_full() {
        let p1 = QueryResultPlacement::new(0, None, 0, 0, 0, 0);
        let p2 = QueryResultPlacement::new(1, None, 0, 0, 1, 1);
        let p3 = QueryResultPlacement::new(2, None, 1, 1, 0, 1);
        let s = QueryResultLayoutSpec {
            rows: 2,
            columns: 2,
            placements: vec![p1, p2, p3],
            diagnostics: Vec::new(),
        };
        assert_eq!(s.next_open_cell(), None);
    }

    #[test]
    fn parse_empty_sql_returns_none() {
        let s = parse("");
        assert!(!s.has_layout());
    }

    #[test]
    fn parse_no_canvas_returns_none() {
        let s = parse("SELECT 1");
        assert!(!s.has_layout());
    }

    #[test]
    fn parse_canvas_dimensions() {
        let s = parse("-- sqlly 4x2\nSELECT 1");
        assert_eq!(s.rows, 4);
        assert_eq!(s.columns, 2);
        assert!(s.is_valid());
    }

    #[test]
    fn parse_canvas_tolerates_any_whitespace_after_the_namespace() {
        // The shared directive lexer accepts any ASCII whitespace between
        // `sqlly` and the shape — the same rule every other family uses.
        let s = parse("-- sqlly\t4x2\nSELECT 1");
        assert_eq!(s.rows, 4);
        assert_eq!(s.columns, 2);
    }

    #[test]
    fn parse_canvas_requires_plain_digits() {
        // A signed number is not a canvas anywhere — the lexer, the hover,
        // and this parser agree by construction.
        assert!(!parse("-- sqlly +4x2\nSELECT 1").has_layout());
    }

    #[test]
    fn parse_canvas_with_placement() {
        let sql = "-- sqlly 2x2\n-- sqlly result: 1|1\nSELECT 1\n-- sqlly result: 1|2\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.rows, 2);
        assert_eq!(s.columns, 2);
        assert_eq!(s.placements.len(), 2);
        // First placement: row 1-1, col 1-1 (0-indexed: 0,0)
        assert_eq!(s.placements[0].row_start, 0);
        assert_eq!(s.placements[0].column_start, 0);
        // Second placement: row 1-1, col 2-2 (0-indexed: 0,1)
        assert_eq!(s.placements[1].row_start, 0);
        assert_eq!(s.placements[1].column_start, 1);
    }

    #[test]
    fn parse_auto_places_without_marker() {
        let sql = "-- sqlly 2x2\nSELECT 1;\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.placements.len(), 2);
        // First at (0,0), second at (0,1)
        assert_eq!(s.placements[0].row_start, 0);
        assert_eq!(s.placements[0].column_start, 0);
        assert_eq!(s.placements[1].row_start, 0);
        assert_eq!(s.placements[1].column_start, 1);
    }

    #[test]
    fn parse_spanned_placement() {
        let sql = "-- sqlly 2x2\n-- sqlly result: 1-2|1\nSELECT 1\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.placements[0].row_span(), 2);
        assert!(s.placements[0].is_spanned());
    }

    #[test]
    fn parse_zero_dimensions_error() {
        let s = parse("-- sqlly 0x2\nSELECT 1");
        assert!(!s.is_valid());
        assert!(s.diagnostics.iter().any(|d| d.message.contains("positive")));
    }

    #[test]
    fn parse_oversized_canvas_error() {
        let s = parse_with_limits("-- sqlly 16x2\nSELECT 1", 8, 8);
        assert!(!s.is_valid());
        assert!(
            s.diagnostics
                .iter()
                .any(|d| d.message.contains("exceeds maximum"))
        );
    }

    #[test]
    fn parse_placement_outside_canvas_warning() {
        let sql = "-- sqlly 2x2\n-- sqlly result: 5|5\nSELECT 1";
        let s = parse(sql);
        assert!(
            s.diagnostics
                .iter()
                .any(|d| d.message.contains("outside canvas"))
        );
    }

    #[test]
    fn parse_overlapping_placement_warning() {
        let sql =
            "-- sqlly 2x2\n-- sqlly result: 1-2|1-2\nSELECT 1\n-- sqlly result: 1|1\nSELECT 2";
        let s = parse(sql);
        assert!(s.diagnostics.iter().any(|d| d.message.contains("overlaps")));
    }

    #[test]
    fn parse_dangling_placement_marker_warning() {
        let sql = "-- sqlly 2x2\n-- sqlly result: 1|1\n-- nothing here";
        let s = parse(sql);
        assert!(s.diagnostics.iter().any(|d| {
            d.message
                .contains("not followed by a result-producing statement")
        }));
    }

    #[test]
    fn parse_malformed_placement_warning() {
        let sql = "-- sqlly 2x2\n-- sqlly result: abc\nSELECT 1";
        let s = parse(sql);
        assert!(
            s.diagnostics
                .iter()
                .any(|d| d.message.contains("malformed"))
        );
    }

    #[test]
    fn parse_go_separator() {
        let sql = "-- sqlly 2x2\nSELECT 1\nGO\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.placements.len(), 2);
    }

    #[test]
    fn parse_semicolon_separator() {
        let sql = "-- sqlly 2x2\nSELECT 1;\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.placements.len(), 2);
    }

    #[test]
    fn parse_result_producing_detection() {
        assert!(is_result_producing("SELECT 1"));
        assert!(is_result_producing("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_result_producing("EXEC sp_help"));
        assert!(is_result_producing("EXECUTE sp_help"));
        assert!(is_result_producing("VALUES (1, 2)"));
        assert!(!is_result_producing("INSERT INTO t VALUES (1)"));
        assert!(!is_result_producing("UPDATE t SET x = 1"));
        assert!(!is_result_producing("DELETE FROM t"));
    }

    #[test]
    fn parse_range_single() {
        assert_eq!(parse_range("1"), Some((1, 1)));
        assert_eq!(parse_range("3"), Some((3, 3)));
    }

    #[test]
    fn parse_range_multi() {
        assert_eq!(parse_range("1-3"), Some((1, 3)));
        assert_eq!(parse_range("2-4"), Some((2, 4)));
    }

    #[test]
    fn parse_range_invalid() {
        assert_eq!(parse_range("0"), None);
        assert_eq!(parse_range("abc"), None);
        assert_eq!(parse_range(""), None);
        assert_eq!(parse_range("1-2-3"), None);
    }

    #[test]
    fn parse_placement_malformed() {
        let (p, err) = parse_placement("abc", 0);
        assert!(p.is_none());
        assert!(err.unwrap().contains("malformed"));
    }

    #[test]
    fn parse_canvas_basic() {
        assert_eq!(parse_canvas("-- sqlly 4x2"), Some((4, 2)));
        assert_eq!(parse_canvas("-- sqlly 2x2"), Some((2, 2)));
        assert_eq!(parse_canvas("SELECT 1"), None);
        assert_eq!(parse_canvas("-- abc"), None);
    }

    #[test]
    fn assigning_result_sets_assigns_indexes() {
        let s = QueryResultLayoutSpec {
            rows: 2,
            columns: 2,
            placements: vec![
                QueryResultPlacement::new(0, None, 0, 0, 0, 0),
                QueryResultPlacement::new(1, None, 0, 0, 1, 1),
            ],
            diagnostics: Vec::new(),
        };
        let resolved = s.assigning_result_sets(2);
        assert_eq!(resolved.placements[0].result_set_index, Some(0));
        assert_eq!(resolved.placements[1].result_set_index, Some(1));
    }

    #[test]
    fn assigning_result_sets_auto_places_extras() {
        let s = QueryResultLayoutSpec {
            rows: 2,
            columns: 2,
            placements: vec![QueryResultPlacement::new(0, None, 0, 0, 0, 0)],
            diagnostics: Vec::new(),
        };
        let resolved = s.assigning_result_sets(3);
        assert_eq!(resolved.placements.len(), 3);
        assert_eq!(resolved.placements[0].result_set_index, Some(0));
        // Extras should have result_set_index set.
        assert!(resolved.placements[1].result_set_index.is_some());
        assert!(resolved.placements[2].result_set_index.is_some());
    }

    #[test]
    fn parse_skips_strings_and_comments() {
        let sql = "-- sqlly 2x2\n-- This is a comment\nSELECT '-- not a comment', 1\n-- sqlly result: 1|1\nSELECT 2";
        let s = parse(sql);
        // Should parse two result-producing statements
        assert_eq!(s.placements.len(), 2);
    }

    #[test]
    fn parse_crlf_line_endings() {
        let sql = "-- sqlly 2x2\r\nSELECT 1;\r\nSELECT 2";
        let s = parse(sql);
        assert_eq!(s.rows, 2);
        assert_eq!(s.columns, 2);
        assert_eq!(s.placements.len(), 2);
    }
}
