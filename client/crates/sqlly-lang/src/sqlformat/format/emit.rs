//! The formatter core: walks statements, clauses, and nodes, driving the
//! writer while applying every layout, casing, and quoting option.

use super::Options;
use super::casing::{
    CAT_CLAUSES, CAT_DDL, CAT_DML, CAT_JOINS, CAT_SET_OPS, CAT_TRANSACTION, CAT_WORD_OPERATORS,
    TYPE_WORDS, apply_keyword_case, pascal_case_identifier, plain_identifier_ok, quote_plain,
    reserved_needs_quotes, safe_to_unquote, word_in,
};
use super::segment::{
    BatchSeparator, Clause, ClauseKind, Statement, control_comment_state, find_alias_pos,
    find_case_end, segment_clauses, split_commas, split_statements_items,
};
use super::tree::{Item, Node, ParenKind, build_tree, word_text};
use super::writer::{AlignTag, LastTok, Writer, push_multiline, reindent_block_comment};
use crate::sqlformat::prefs::{
    BooleanOperatorPlacement, CaseExpressionStyle, ClauseBodyPlacement, CommaStyle, CommentStyle,
    CteStyle, DdlStyle, DmlStyle, FunctionArgStyle, FunctionCase, IdentifierCase,
    IdentifierQuoting, InListStyle, JoinLayout, KeywordCase, MajorClausePlacement,
    OnClausePlacement, OrderGroupLayout, SelectListStyle, SemicolonStyle, SubqueryStyle,
    ValuesStyle,
};
use crate::sqllex::{CommentShape, IdentQuote, TokenKind};

/// Where a word sits, for casing decisions.
#[derive(Clone, Copy, PartialEq)]
enum WordRole {
    Keyword,
    Function,
    DataType,
    Identifier,
}

#[derive(Clone, Copy)]
struct Ctx {
    /// Indent (units) for clause keywords of the current query block.
    clause_indent: usize,
    /// Extra columns for aligned subqueries.
    clause_extra: usize,
    /// Statement is top-level (blank-line-before-clauses applies).
    top_level: bool,
}

impl Ctx {
    fn cont_indent(&self, o: &Options) -> usize {
        self.clause_indent + cont_units(o)
    }
}

/// Continuation indent in units: `continuation_indent_size` is expressed in
/// spaces; convert to whole indent units (at least 1).
fn cont_units(o: &Options) -> usize {
    let unit = o.indent_unit_width().max(1);
    (o.p.continuation_indent_size() as usize)
        .div_ceil(unit)
        .max(1)
}

pub(super) struct Formatter<'a, 'o, 'p> {
    src: &'a str,
    o: &'o Options<'p>,
    w: Writer<'o, 'p>,
    /// `formatter:off` state.
    enabled: bool,
    /// Depth of CREATE TABLE bodies — unquoted idents matching a type word
    /// get data-type casing there.
    ddl_body_depth: usize,
    /// The previous emitted token was `::` — the next word is a cast target.
    after_cast_op: bool,
    /// The previous emitted token was `.` — the next word is the member of
    /// a qualified name (`o.type`), an identifier whatever it is spelled.
    after_dot: bool,
}

impl<'a, 'o, 'p> Formatter<'a, 'o, 'p> {
    pub(super) fn new(src: &'a str, o: &'o Options<'p>) -> Self {
        Self {
            src,
            o,
            w: Writer::new(o),
            enabled: true,
            ddl_body_depth: 0,
            after_cast_op: false,
            after_dot: false,
        }
    }

    pub(super) fn run(mut self, items: &[Item<'a>]) -> String {
        // `GO` batch separators are T-SQL (other dialects keep `go` a word);
        // a lone `/` line ends a PL/SQL block in Oracle scripts.
        let statements = split_statements_items(items, self.o.dialect);
        let multi = statements.len() > 1;
        for (si, stmt) in statements.iter().enumerate() {
            if stmt.items.is_empty() && stmt.semicolon.is_none() && stmt.separator.is_none() {
                continue;
            }
            if si > 0 {
                let observed = stmt
                    .items
                    .first()
                    .or(stmt.semicolon.as_ref())
                    .or(stmt.separator.as_ref().map(|s| &s.go))
                    .map(|i| i.newlines_before)
                    .unwrap_or(1);
                let blanks = observed.saturating_sub(1) as usize;
                let max = self.o.p.max_consecutive_blank_lines() as usize;
                self.w.newline(0, 0);
                self.w.blank_lines(blanks.min(max));
            }
            self.emit_statement(stmt, multi);
        }
        self.w.finish()
    }

    // -- statement level ---------------------------------------------------

    fn emit_statement(&mut self, stmt: &Statement<'a>, multi_statement: bool) {
        let has_body = !stmt.items.is_empty() || stmt.semicolon.is_some();
        if has_body {
            self.emit_statement_body(stmt, multi_statement);
        }
        if let Some(sep) = &stmt.separator {
            self.emit_batch_separator(sep, has_body);
        }
    }

    /// A `GO` batch separator always gets its own line — never joined onto
    /// the statement before it or folded into a one-line rendering.
    fn emit_batch_separator(&mut self, sep: &BatchSeparator<'a>, after_body: bool) {
        let verbatim = !self.enabled;
        self.w.newline(0, 0);
        if after_body {
            let blanks = (sep.go.newlines_before.saturating_sub(1) as usize)
                .min(self.o.p.max_consecutive_blank_lines() as usize);
            self.w.blank_lines(blanks);
        }
        let go = if verbatim {
            sep.go.tok.text.to_string()
        } else {
            self.cased_word(&sep.go, WordRole::Keyword)
        };
        self.w.push(&go);
        if let Some(count) = &sep.count {
            self.w.space();
            self.w.push(count.tok.text);
        }
        for c in &sep.trailing_comments {
            self.w.space();
            push_multiline(&mut self.w, c.tok.text.trim_end());
            if self.o.p.respect_control_comments
                && let Some(state) =
                    control_comment_state(c.tok.text, &self.o.p.control_comment_prefix)
            {
                self.enabled = state;
            }
        }
        self.w.last = LastTok::Operand;
    }

    fn emit_statement_body(&mut self, stmt: &Statement<'a>, multi_statement: bool) {
        // Control-comment toggles in the statement's *leading* comments take
        // effect for this statement; toggles later in it (trailing comments)
        // affect the following statements.
        let first_code = stmt
            .items
            .iter()
            .position(|i| !matches!(i.tok.kind, TokenKind::Comment(_)))
            .unwrap_or(stmt.items.len());
        if self.o.p.respect_control_comments {
            for item in &stmt.items[..first_code] {
                if let Some(state) =
                    control_comment_state(item.tok.text, &self.o.p.control_comment_prefix)
                {
                    self.enabled = state;
                }
            }
        }
        let started_enabled = self.enabled;
        if self.o.p.respect_control_comments {
            for item in &stmt.items[first_code..] {
                if let TokenKind::Comment(_) = item.tok.kind
                    && let Some(state) =
                        control_comment_state(item.tok.text, &self.o.p.control_comment_prefix)
                {
                    self.enabled = state;
                }
            }
        }
        if !started_enabled {
            // Verbatim: the original slice, trailing spaces trimmed per line.
            let slice = &self.src[stmt.src_start..stmt.src_end];
            self.w.newline(0, 0);
            for (i, line) in slice.split('\n').enumerate() {
                if i > 0 {
                    self.w.newline(0, 0);
                }
                self.w.push(line.trim_end());
            }
            self.w.last = LastTok::Operand;
            return;
        }

        let nodes = build_tree(&stmt.items);
        // Whole-statement one-liner shortcuts.
        let shortcut_limit = if !multi_statement && self.o.p.keep_short_queries_on_one_line {
            Some(self.o.p.short_query_max_length().max(0) as usize)
        } else if self.o.p.major_clause_placement == MajorClausePlacement::CompactWhenShort {
            Some(self.o.max_line())
        } else {
            None
        };
        // `dml_style` on INSERT/UPDATE/DELETE/MERGE: Compact puts the whole
        // statement on one line whenever it fits the line length; Expanded
        // asks for one column/value per line, so it opts out of one-liners.
        let compact_limit = match self.o.p.dml_style {
            _ if !is_dml_statement(&nodes) => shortcut_limit,
            DmlStyle::Standard => shortcut_limit,
            DmlStyle::Compact => Some(shortcut_limit.unwrap_or(0).max(self.o.max_line())),
            DmlStyle::Expanded => None,
        };
        if let Some(limit) = compact_limit
            && let Some(compact) = self.render_compact(&nodes)
            && compact.chars().count() + usize::from(stmt.semicolon.is_some()) <= limit
        {
            self.w.newline(0, 0);
            self.w.push(&compact);
            if stmt.semicolon.is_some() {
                self.w.join();
                self.w.push(";");
            }
            return;
        }

        let ctx = Ctx {
            clause_indent: 0,
            clause_extra: 0,
            top_level: true,
        };
        self.w.newline(0, 0);
        self.emit_query_block(&nodes, &ctx);

        if stmt.semicolon.is_some() {
            match self.o.p.semicolon_style {
                SemicolonStyle::EndOfStatement => {
                    self.w.join();
                    self.w.push(";");
                }
                SemicolonStyle::OwnLine => {
                    self.w.newline(0, 0);
                    self.w.push(";");
                }
            }
        }
    }

    /// Render a node list as a single line (casing and spacing applied, no
    /// layout). `None` when a line comment makes a one-liner impossible.
    fn render_compact(&mut self, nodes: &[Node<'a>]) -> Option<String> {
        if nodes.iter().any(|n| n.contains_line_comment()) {
            return None;
        }
        let saved_lines = std::mem::take(&mut self.w.lines);
        let saved_cur = std::mem::take(&mut self.w.cur);
        let saved_started = self.w.started;
        let saved_pending = self.w.pending_space;
        let saved_join = self.w.force_join;
        let saved_blanks = self.w.pending_blank_lines;
        let saved_last = self.w.last;
        self.w.started = false;
        self.w.pending_space = false;
        self.w.force_join = false;
        self.w.pending_blank_lines = 0;
        self.w.last = LastTok::None;

        let ctx = Ctx {
            clause_indent: 0,
            clause_extra: 0,
            top_level: false,
        };
        self.emit_flow_compact(nodes, &ctx);
        let ok = self.w.lines.is_empty();
        let text = std::mem::take(&mut self.w.cur.text);

        self.w.lines = saved_lines;
        self.w.cur = saved_cur;
        self.w.started = saved_started;
        self.w.pending_space = saved_pending;
        self.w.force_join = saved_join;
        self.w.pending_blank_lines = saved_blanks;
        self.w.last = saved_last;

        if ok { Some(text) } else { None }
    }

    // -- query block (a list of clauses) ------------------------------------

    fn emit_query_block(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let clauses = segment_clauses(nodes);
        let preserve_breaks =
            self.o.p.major_clause_placement == MajorClausePlacement::PreserveExisting;
        self.w.new_scope();
        for (ci, clause) in clauses.iter().enumerate() {
            if ci > 0 {
                let break_here = if preserve_breaks {
                    clause.nodes[0].newlines_before() > 0
                } else {
                    true
                };
                if break_here {
                    if self.o.p.blank_line_before_major_clauses && ctx.top_level {
                        self.w.newline(ctx.clause_indent, ctx.clause_extra);
                        self.w.blank_lines(1);
                    }
                    if clause.kind == ClauseKind::MergeWhen && self.o.p.merge_section_separation {
                        self.w.newline(ctx.clause_indent, ctx.clause_extra);
                        self.w.blank_lines(1);
                    }
                    self.w.newline(ctx.clause_indent, ctx.clause_extra);
                } else {
                    self.w.space();
                }
            }
            self.emit_clause(clause, ctx);
        }
    }

    fn emit_clause(&mut self, clause: &Clause<'a, '_>, ctx: &Ctx) {
        match clause.kind {
            ClauseKind::Select => self.emit_select_clause(clause.nodes, ctx),
            ClauseKind::GroupBy | ClauseKind::OrderBy => {
                self.emit_order_group_clause(clause.nodes, ctx)
            }
            ClauseKind::Where | ClauseKind::Having => self.emit_condition_clause(clause.nodes, ctx),
            ClauseKind::From => self.emit_from_clause(clause.nodes, ctx),
            ClauseKind::UpdateSet => self.emit_update_set_clause(clause.nodes, ctx),
            ClauseKind::MergeWhen => self.emit_merge_when_clause(clause.nodes, ctx),
            ClauseKind::Values => self.emit_values_clause(clause.nodes, ctx),
            ClauseKind::With => self.emit_with_clause(clause.nodes, ctx),
            _ => self.emit_flow(clause.nodes, ctx),
        }
    }

    // -- SELECT --------------------------------------------------------------

    fn emit_select_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        // `SELECT [DISTINCT|ALL|TOP n]`, then the column list.
        let mut i = 0;
        while i < nodes.len() {
            if nodes[i].is_comment() {
                self.emit_node(&nodes[i], ctx);
                i += 1;
                continue;
            }
            let Some(w) = (match &nodes[i] {
                Node::Tok(item) => word_text(&item.tok).map(|w| w.to_ascii_lowercase()),
                _ => None,
            }) else {
                break;
            };
            match w.as_str() {
                "select" | "distinct" | "all" => {
                    self.emit_node(&nodes[i], ctx);
                    i += 1;
                }
                "top" => {
                    self.emit_node(&nodes[i], ctx);
                    i += 1;
                    // TOP n / TOP (n) [PERCENT]
                    while i < nodes.len()
                        && (matches!(
                            &nodes[i],
                            Node::Tok(it) if matches!(it.tok.kind, TokenKind::Number(_))
                        ) || matches!(&nodes[i], Node::Paren { .. })
                            || nodes[i].is_word("percent"))
                    {
                        self.emit_node(&nodes[i], ctx);
                        i += 1;
                    }
                }
                _ => break,
            }
        }
        let list = &nodes[i..];
        if list.is_empty() {
            return;
        }

        let layout = select_layout(self.o.p.select_column_layout);
        self.emit_list(
            list,
            layout,
            ctx,
            ListOpts {
                break_before_first: self.o.p.break_after_select,
                align_aliases: self.o.p.align_column_aliases,
                align_assign: false,
            },
        );
    }

    // -- GROUP BY / ORDER BY -------------------------------------------------

    fn emit_order_group_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let mut i = 0;
        let mut seen_by = false;
        while i < nodes.len() {
            if nodes[i].is_comment() {
                self.emit_node(&nodes[i], ctx);
                i += 1;
                continue;
            }
            if let Node::Tok(item) = &nodes[i]
                && let Some(w) = word_text(&item.tok)
            {
                let lower = w.to_ascii_lowercase();
                if lower == "group" || lower == "order" || (!seen_by && lower == "by") {
                    if lower == "by" {
                        seen_by = true;
                    }
                    self.emit_node(&nodes[i], ctx);
                    i += 1;
                    continue;
                }
            }
            break;
        }
        let list = &nodes[i..];
        if list.is_empty() {
            return;
        }
        let layout = match self.o.p.order_group_layout {
            OrderGroupLayout::MatchSelectList => select_layout(self.o.p.select_column_layout),
            OrderGroupLayout::Inline => ListLayout::Wrap,
            OrderGroupLayout::Stacked => ListLayout::Stacked,
            OrderGroupLayout::PreserveExisting => ListLayout::PreserveSource,
        };
        self.emit_list(
            list,
            layout,
            ctx,
            ListOpts {
                break_before_first: self.o.p.break_after_select && layout == ListLayout::Stacked,
                align_aliases: false,
                align_assign: false,
            },
        );
    }

    // -- WHERE / HAVING / ON conditions ---------------------------------------

    fn emit_condition_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let new_line = self.o.p.where_condition_placement == ClauseBodyPlacement::NewLineIndented;
        let i = self.emit_clause_keyword(nodes, ctx, new_line);
        let body = &nodes[i..];
        let item_indent = ctx.cont_indent(self.o);
        // `where_condition_placement`: the first condition starts its own
        // line at the same indent as the AND/OR lines after it.
        if new_line && !body.is_empty() {
            self.w.newline(item_indent, ctx.clause_extra);
        }
        self.emit_condition_list(body, ctx, item_indent);
    }

    /// Emit a clause's single leading keyword plus the comments attached
    /// before it; returns the index of the first body node. With
    /// `keep_trailing_comments`, comments that trailed the keyword on its
    /// source line stay on the keyword's line (the body is about to move to
    /// a line of its own).
    fn emit_clause_keyword(
        &mut self,
        nodes: &[Node<'a>],
        ctx: &Ctx,
        keep_trailing_comments: bool,
    ) -> usize {
        let mut i = 0;
        while i < nodes.len() {
            let is_kw = matches!(&nodes[i], Node::Tok(it) if word_text(&it.tok).is_some());
            self.emit_node(&nodes[i], ctx);
            i += 1;
            if is_kw {
                break;
            }
        }
        if keep_trailing_comments {
            while i < nodes.len() && nodes[i].is_comment() && nodes[i].newlines_before() == 0 {
                self.emit_node(&nodes[i], ctx);
                i += 1;
            }
        }
        i
    }

    /// Split `cond` at top-level AND/OR (skipping BETWEEN…AND pairs and
    /// CASE…END spans) and lay the groups out per
    /// `boolean_operator_placement`.
    fn emit_condition_list(&mut self, cond: &[Node<'a>], ctx: &Ctx, item_indent: usize) {
        // Indices of splitting AND/OR operators.
        let mut ops: Vec<usize> = Vec::new();
        let mut between_pending = false;
        let mut case_depth = 0usize;
        for (idx, n) in cond.iter().enumerate() {
            if n.is_word("case") {
                case_depth += 1;
            } else if n.is_word("end") && case_depth > 0 {
                case_depth -= 1;
            } else if case_depth == 0 && n.is_word("between") {
                between_pending = true;
            } else if case_depth == 0 && (n.is_word("and") || n.is_word("or")) {
                if between_pending && n.is_word("and") {
                    between_pending = false;
                    continue;
                }
                ops.push(idx);
            }
        }
        // Groups: the leading run plus one per operator.
        let mut groups: Vec<(Option<usize>, std::ops::Range<usize>)> = Vec::new();
        let mut prev_end = 0usize;
        for &op in &ops {
            groups.push((None, prev_end..op));
            prev_end = op + 1;
        }
        groups.push((None, prev_end..cond.len()));
        for (k, g) in groups.iter_mut().enumerate() {
            if k > 0 {
                g.0 = Some(ops[k - 1]);
            }
        }
        if groups.len() == 1 {
            self.emit_condition_group(&cond[groups[0].1.clone()], ctx, item_indent);
            return;
        }

        // parenthesize_mixed_boolean_logic: with both AND and OR present at
        // this level, wrap maximal AND-runs in parens.
        let op_word = |i: Option<usize>| -> Option<String> {
            i.and_then(|i| match &cond[i] {
                Node::Tok(it) => word_text(&it.tok).map(|w| w.to_ascii_lowercase()),
                _ => None,
            })
        };
        let has_and = groups
            .iter()
            .any(|(op, _)| op_word(*op).as_deref() == Some("and"));
        let has_or = groups
            .iter()
            .any(|(op, _)| op_word(*op).as_deref() == Some("or"));
        let parenthesize = self.o.p.parenthesize_mixed_boolean_logic && has_and && has_or;

        let placement = self.o.p.boolean_operator_placement;
        self.w.new_scope();

        let mut k = 0usize;
        while k < groups.len() {
            let (op, range) = groups[k].clone();

            // Extent of the AND-run starting at k (for parenthesizing).
            let mut run_end = k + 1;
            while parenthesize
                && run_end < groups.len()
                && op_word(groups[run_end].0).as_deref() == Some("and")
            {
                run_end += 1;
            }
            let wrap_run = parenthesize && run_end - k > 1 && {
                // Wrap only AND-runs that are OR operands (i.e. the run is
                // bounded by OR or by the ends).
                op_word(op).as_deref() != Some("and")
            };

            if let Some(op_idx) = op {
                self.emit_boolean_op(&cond[op_idx], placement, item_indent, ctx);
            }

            if wrap_run {
                self.w.push("(");
                self.w.join();
                for (rk, (rop, rrange)) in groups[k..run_end].iter().cloned().enumerate() {
                    if rk > 0
                        && let Some(op_idx) = rop
                    {
                        self.emit_boolean_op(&cond[op_idx], placement, item_indent + 1, ctx);
                    }
                    self.emit_condition_group(&cond[rrange], ctx, item_indent + 1);
                }
                self.w.join();
                self.w.push(")");
                self.w.last = LastTok::Operand;
                k = run_end;
            } else {
                self.emit_condition_group(&cond[range], ctx, item_indent);
                k += 1;
            }
        }
    }

    fn emit_boolean_op(
        &mut self,
        op: &Node<'a>,
        placement: BooleanOperatorPlacement,
        item_indent: usize,
        ctx: &Ctx,
    ) {
        match placement {
            BooleanOperatorPlacement::LineLeading => {
                self.w.newline(item_indent, ctx.clause_extra);
                self.emit_node(op, ctx);
            }
            BooleanOperatorPlacement::LineTrailing => {
                self.emit_node(op, ctx);
                self.w.newline(item_indent, ctx.clause_extra);
            }
            BooleanOperatorPlacement::PreserveExisting => {
                if op.newlines_before() > 0 {
                    self.w.newline(item_indent, ctx.clause_extra);
                }
                self.emit_node(op, ctx);
            }
        }
    }

    fn emit_condition_group(&mut self, grp: &[Node<'a>], ctx: &Ctx, _item_indent: usize) {
        if self.o.p.align_comparison_operators {
            let mut tagged = false;
            let mut i = 0usize;
            while i < grp.len() {
                if grp[i].is_word("case") {
                    let end = find_case_end(grp, i);
                    self.emit_case(&grp[i..end], ctx);
                    i = end;
                    continue;
                }
                if !tagged
                    && let Node::Tok(item) = &grp[i]
                    && item.tok.kind == TokenKind::Operator
                    && matches!(
                        item.tok.text,
                        "=" | "<" | ">" | "<=" | ">=" | "<>" | "!=" | "!<" | "!>"
                    )
                {
                    self.w.mark(AlignTag::Comparison);
                    tagged = true;
                }
                self.emit_node(&grp[i], ctx);
                i += 1;
            }
        } else {
            self.emit_flow(grp, ctx);
        }
    }

    // -- FROM and JOINs -------------------------------------------------------

    fn emit_from_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        // Segment into FROM+first table, then join phrases.
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut seg_start = 0usize;
        for i in 0..nodes.len() {
            if i > 0 && self.join_phrase_start(nodes, i) {
                segments.push((seg_start, i));
                seg_start = i;
            }
        }
        segments.push((seg_start, nodes.len()));

        // `from_table_placement`: with the first table on its own indented
        // line, JOINs line up with it (and ON indents from there) so the
        // whole table list reads as one indented block under FROM.
        let table_on_new_line =
            self.o.p.from_table_placement == ClauseBodyPlacement::NewLineIndented;
        let join_indent = match self.o.p.join_layout {
            _ if table_on_new_line => ctx.cont_indent(self.o),
            JoinLayout::Aligned | JoinLayout::Compact => ctx.clause_indent,
            JoinLayout::Stacked => ctx.cont_indent(self.o),
        };
        let on_base = if table_on_new_line {
            join_indent
        } else {
            ctx.clause_indent
        };

        for (si, &(s, e)) in segments.iter().enumerate() {
            let seg = &nodes[s..e];
            if seg.is_empty() {
                continue;
            }
            if si == 0 && table_on_new_line {
                let k = self.emit_clause_keyword(seg, ctx, true);
                let rest = &seg[k..];
                if !rest.is_empty() {
                    self.w.newline(ctx.cont_indent(self.o), ctx.clause_extra);
                }
                self.emit_join_segment(rest, ctx, false, on_base);
                continue;
            }
            if si > 0 {
                let compact_ok = self.o.p.join_layout == JoinLayout::Compact
                    && !seg.iter().any(|n| n.contains_line_comment());
                if compact_ok {
                    let len = self.measure_nodes(seg);
                    if self.w.cur_width() + 1 + len <= self.o.max_line() {
                        self.w.space();
                    } else {
                        self.w.newline(join_indent, ctx.clause_extra);
                    }
                } else {
                    self.w.newline(join_indent, ctx.clause_extra);
                }
            }
            self.emit_join_segment(seg, ctx, si > 0, on_base);
        }
    }

    fn join_phrase_start(&self, nodes: &[Node<'a>], i: usize) -> bool {
        let Node::Tok(item) = &nodes[i] else {
            return false;
        };
        let Some(w) = word_text(&item.tok) else {
            return false;
        };
        match w.to_ascii_lowercase().as_str() {
            "join" => {
                let prev =
                    nodes[..i]
                        .iter()
                        .rev()
                        .find(|n| !n.is_comment())
                        .and_then(|n| match n {
                            Node::Tok(it) => word_text(&it.tok).map(|w| w.to_ascii_lowercase()),
                            _ => None,
                        });
                !matches!(
                    prev.as_deref(),
                    Some("inner")
                        | Some("left")
                        | Some("right")
                        | Some("full")
                        | Some("outer")
                        | Some("cross")
                        | Some("natural")
                )
            }
            "inner" | "left" | "right" | "full" | "cross" | "natural" => nodes[i + 1..]
                .iter()
                .filter(|n| !n.is_comment())
                .take(2)
                .any(|n| n.is_word("join") || n.is_word("outer")),
            "apply" => false,
            _ => false,
        }
    }

    /// `on_base` is the indent ON lines measure from (`indent_join_on` adds
    /// one continuation step to it).
    fn emit_join_segment(&mut self, seg: &[Node<'a>], ctx: &Ctx, is_join: bool, on_base: usize) {
        let on_pos = seg.iter().position(|n| n.is_word("on"));
        let (head, on_part) = match on_pos {
            Some(p) if is_join => (&seg[..p], Some(&seg[p..])),
            _ => (seg, None),
        };
        self.emit_flow(head, ctx);
        if let Some(on_part) = on_part {
            let on_indent = if self.o.p.indent_join_on {
                on_base + cont_units(self.o)
            } else {
                on_base
            };
            match self.o.p.on_clause_placement {
                OnClausePlacement::AfterJoin => {}
                OnClausePlacement::OnNewLine => {
                    self.w.newline(on_indent, ctx.clause_extra);
                }
                OnClausePlacement::PreserveExisting => {
                    if on_part[0].newlines_before() > 0 {
                        self.w.newline(on_indent, ctx.clause_extra);
                    }
                }
            }
            self.emit_node(&on_part[0], ctx);
            let cond = &on_part[1..];
            if self.o.p.multi_condition_on {
                self.emit_condition_list(cond, ctx, on_indent + cont_units(self.o));
            } else {
                self.emit_flow(cond, ctx);
            }
        }
    }

    // -- UPDATE … SET ---------------------------------------------------------

    fn emit_update_set_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let mut i = 0;
        while i < nodes.len() {
            let is_set = nodes[i].is_word("set");
            self.emit_node(&nodes[i], ctx);
            i += 1;
            if is_set {
                break;
            }
        }
        self.emit_list(
            &nodes[i..],
            ListLayout::Stacked,
            ctx,
            ListOpts {
                break_before_first: self.o.p.dml_style == DmlStyle::Expanded,
                align_aliases: false,
                align_assign: self.o.p.align_set_operators,
            },
        );
    }

    /// `WHEN [NOT] MATCHED [AND …] THEN UPDATE SET …` / `… THEN INSERT …` /
    /// `… THEN DELETE`. Expanded DML stacks the SET assignments one per line
    /// like an UPDATE statement's; otherwise the clause flows.
    fn emit_merge_when_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let set_pos = nodes
            .windows(2)
            .position(|pair| pair[0].is_word("update") && pair[1].is_word("set"));
        let stacked = self.o.p.dml_style == DmlStyle::Expanded
            && set_pos.is_some_and(|p| split_commas(&nodes[p + 2..]).len() > 1);
        let Some(set) = set_pos.filter(|_| stacked) else {
            self.emit_flow(nodes, ctx);
            return;
        };
        self.emit_flow(&nodes[..set + 2], ctx);
        self.emit_list(
            &nodes[set + 2..],
            ListLayout::Stacked,
            ctx,
            ListOpts {
                break_before_first: true,
                align_aliases: false,
                align_assign: self.o.p.align_set_operators,
            },
        );
    }

    // -- VALUES ---------------------------------------------------------------

    fn emit_values_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let mut i = 0;
        while i < nodes.len() && !matches!(&nodes[i], Node::Paren { .. }) {
            self.emit_node(&nodes[i], ctx);
            i += 1;
        }
        let tuples = &nodes[i..];
        if tuples.is_empty() {
            return;
        }

        let stacked = self.o.p.values_style == ValuesStyle::Stacked;
        // `dml_style: Expanded` puts each value of a tuple on its own line,
        // which supersedes row alignment.
        let expanded = self.o.p.dml_style == DmlStyle::Expanded;
        let widths = if self.o.p.align_values_rows && stacked && !expanded {
            self.values_column_widths(tuples)
        } else {
            None
        };

        let tuple_indent = ctx.cont_indent(self.o);
        let mut first_tuple = true;
        for n in tuples.iter() {
            match n {
                Node::Paren {
                    children,
                    has_close,
                    ..
                } => {
                    if first_tuple {
                        if stacked {
                            self.w.newline(tuple_indent, ctx.clause_extra);
                        } else {
                            self.w.space();
                        }
                        first_tuple = false;
                    }
                    if expanded && split_commas(children).len() > 1 {
                        self.emit_paren_one_per_line(children, *has_close, ctx);
                    } else if let (Some(widths), true) = (&widths, *has_close) {
                        let rendered = self.render_tuple_aligned(children, widths);
                        self.w.push(&rendered);
                        self.w.last = LastTok::Operand;
                    } else {
                        self.w.push("(");
                        self.w.join();
                        self.w.last = LastTok::Open;
                        self.emit_flow_compact(children, ctx);
                        if *has_close {
                            self.w.join();
                            self.w.push(")");
                            self.w.last = LastTok::Operand;
                        }
                    }
                }
                Node::Tok(item) if item.tok.kind == TokenKind::Comma => {
                    self.w.join();
                    self.w.push(",");
                    self.w.last = LastTok::Open;
                    if stacked {
                        self.w.newline(tuple_indent, ctx.clause_extra);
                    } else {
                        self.w.maybe_wrap(8, tuple_indent, ctx.clause_extra);
                        self.w.space();
                    }
                }
                other => self.emit_node(other, ctx),
            }
        }
    }

    fn values_column_widths(&mut self, tuples: &[Node<'a>]) -> Option<Vec<usize>> {
        let mut widths: Vec<usize> = Vec::new();
        let mut any = false;
        for n in tuples {
            if let Node::Paren { children, .. } = n {
                if children.iter().any(|c| c.contains_line_comment()) {
                    return None;
                }
                any = true;
                for (idx, (elem, _)) in split_commas(children).into_iter().enumerate() {
                    let text = self.render_compact(elem)?;
                    let w = text.chars().count();
                    if idx >= widths.len() {
                        widths.push(w);
                    } else if widths[idx] < w {
                        widths[idx] = w;
                    }
                }
            }
        }
        if any { Some(widths) } else { None }
    }

    fn render_tuple_aligned(&mut self, children: &[Node<'a>], widths: &[usize]) -> String {
        let mut out = String::from("(");
        let elems = split_commas(children);
        let count = elems.len();
        for (idx, (elem, _)) in elems.into_iter().enumerate() {
            let text = self.render_compact(elem).unwrap_or_default();
            let pad = widths
                .get(idx)
                .copied()
                .unwrap_or(0)
                .saturating_sub(text.chars().count());
            out.push_str(&text);
            if idx + 1 < count {
                out.push(',');
                out.push(' ');
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push(')');
        out
    }

    // -- WITH / CTEs ----------------------------------------------------------

    fn emit_with_clause(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let mut i = 0;
        while i < nodes.len() && (nodes[i].is_word("with") || nodes[i].is_word("recursive")) {
            self.emit_node(&nodes[i], ctx);
            i += 1;
        }
        let list = &nodes[i..];
        let cte_indent = if self.o.p.new_line_after_with {
            ctx.cont_indent(self.o)
        } else {
            ctx.clause_indent
        };
        if self.o.p.new_line_after_with {
            self.w.newline(cte_indent, ctx.clause_extra);
        }

        let elems = split_commas(list);
        for (ei, (elem, _)) in elems.into_iter().enumerate() {
            if ei > 0 {
                match self.o.p.comma_style {
                    CommaStyle::Leading => {
                        self.w.newline(cte_indent, ctx.clause_extra);
                        self.w.push(", ");
                        self.w.join();
                    }
                    _ => {
                        self.w.join();
                        self.w.push(",");
                        self.w.newline(cte_indent, ctx.clause_extra);
                    }
                }
            }
            if self.o.p.cte_style == CteStyle::Inline && self.emit_cte_inline(elem) {
                continue;
            }
            self.emit_flow(elem, ctx);
        }
    }

    /// `cte_style: Inline`: the whole `name AS (…)` on the current line when
    /// it has no line comment and fits the line length. False when it
    /// doesn't, so the caller falls back to the stacked layout.
    fn emit_cte_inline(&mut self, elem: &[Node<'a>]) -> bool {
        let Some(text) = self.render_compact(elem) else {
            return false;
        };
        let sep = usize::from(!self.w.cur.text.is_empty());
        if self.w.cur_width() + sep + text.chars().count() > self.o.max_line() {
            return false;
        }
        self.w.space();
        self.w.push(&text);
        self.w.space();
        self.w.last = LastTok::Operand;
        true
    }

    // -- generic flow ----------------------------------------------------------

    fn emit_flow(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let mut i = 0usize;
        while i < nodes.len() {
            if nodes[i].is_word("case") {
                let end = find_case_end(nodes, i);
                self.emit_case(&nodes[i..end], ctx);
                i = end;
                continue;
            }
            self.emit_node(&nodes[i], ctx);
            i += 1;
        }
    }

    /// Flow with no line breaks — used by compact rendering. Line comments
    /// must have been excluded by the caller.
    fn emit_flow_compact(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        for n in nodes {
            self.emit_node_compact(n, ctx);
        }
    }

    // -- CASE ------------------------------------------------------------------

    fn emit_case(&mut self, nodes: &[Node<'a>], ctx: &Ctx) {
        let style = self.o.p.case_expression_style;
        let compact_possible = !nodes.iter().any(|n| n.contains_line_comment());
        let try_compact = match style {
            CaseExpressionStyle::Compact => compact_possible,
            CaseExpressionStyle::Stacked => false,
            CaseExpressionStyle::PreserveExisting => {
                compact_possible && !nodes.iter().skip(1).any(|n| n.newlines_before() > 0)
            }
        };
        if try_compact && let Some(text) = self.render_compact(nodes) {
            let fits = style == CaseExpressionStyle::PreserveExisting
                || self.w.cur_width() + 1 + text.chars().count() <= self.o.max_line();
            if fits {
                self.w.push(&text);
                self.w.last = LastTok::Operand;
                return;
            }
        }

        // Stacked: WHEN/ELSE one continuation in from the CASE line, END back
        // at the CASE line's indent.
        let case_indent = self.w.cur.indent;
        let case_extra = self.w.cur.extra;
        let when_indent = case_indent + cont_units(self.o);

        let mut i = 0usize;
        while i < nodes.len() {
            let n = &nodes[i];
            if i > 0 && n.is_word("case") {
                let end = find_case_end(nodes, i);
                self.emit_case(&nodes[i..end], ctx);
                i = end;
                continue;
            }
            if n.is_word("when") || n.is_word("else") {
                self.w.newline(when_indent, case_extra);
            } else if n.is_word("end") {
                self.w.newline(case_indent, case_extra);
            }
            self.emit_node(n, ctx);
            i += 1;
        }
    }

    // -- node emission ----------------------------------------------------------

    fn emit_node(&mut self, node: &Node<'a>, ctx: &Ctx) {
        match node {
            Node::Tok(item) => self.emit_item(item, ctx, false),
            Node::Paren {
                kind,
                children,
                has_close,
                ..
            } => self.emit_paren(*kind, children, *has_close, ctx, false),
        }
    }

    fn emit_node_compact(&mut self, node: &Node<'a>, ctx: &Ctx) {
        match node {
            Node::Tok(item) => self.emit_item(item, ctx, true),
            Node::Paren {
                kind,
                children,
                has_close,
                ..
            } => self.emit_paren(*kind, children, *has_close, ctx, true),
        }
    }

    fn emit_paren(
        &mut self,
        kind: ParenKind,
        children: &[Node<'a>],
        has_close: bool,
        ctx: &Ctx,
        compact: bool,
    ) {
        // Separator before `(`: function calls follow the pref; everything
        // else keeps the natural space after a word/operator.
        if kind == ParenKind::FunctionArgs && !self.o.p.space_before_function_parentheses {
            self.w.join();
        }

        if compact {
            self.w.push("(");
            self.w.join();
            self.w.last = LastTok::Open;
            if kind == ParenKind::DdlBody {
                self.ddl_body_depth += 1;
            }
            self.emit_flow_compact(children, ctx);
            if kind == ParenKind::DdlBody {
                self.ddl_body_depth -= 1;
            }
            if has_close {
                self.w.join();
                self.w.push(")");
                self.w.last = LastTok::Operand;
            }
            return;
        }

        match kind {
            ParenKind::Subquery => self.emit_subquery(children, has_close, ctx),
            ParenKind::FunctionArgs => self.emit_function_args(children, has_close, ctx),
            ParenKind::InList => self.emit_in_list(children, has_close, ctx),
            ParenKind::DdlBody => self.emit_ddl_body(children, has_close, ctx),
            ParenKind::InsertColumns => self.emit_insert_columns(children, has_close, ctx),
            ParenKind::IndexColumns
                if self.o.p.multiline_index_columns && split_commas(children).len() > 1 =>
            {
                self.emit_paren_one_per_line(children, has_close, ctx)
            }
            _ => {
                self.w.push("(");
                self.w.join();
                self.w.last = LastTok::Open;
                self.emit_flow(children, ctx);
                if has_close {
                    self.w.join();
                    self.w.push(")");
                    self.w.last = LastTok::Operand;
                }
            }
        }
    }

    fn emit_subquery(&mut self, children: &[Node<'a>], has_close: bool, _ctx: &Ctx) {
        // Small subqueries may inline.
        if self.o.p.keep_short_queries_on_one_line
            && let Some(text) = self.render_compact(children)
        {
            let projected = self.w.cur_width() + 2 + text.chars().count();
            if text.chars().count() <= self.o.p.short_query_max_length().max(0) as usize
                && projected <= self.o.max_line()
            {
                self.w.push("(");
                self.w.join();
                self.w.push(&text);
                if has_close {
                    self.w.join();
                    self.w.push(")");
                }
                self.w.last = LastTok::Operand;
                return;
            }
        }

        let style = self.o.p.subquery_style;
        let outer_indent = self.w.cur.indent;
        let outer_extra = self.w.cur.extra;
        let (inner_indent, inner_extra) = match style {
            SubqueryStyle::Indented | SubqueryStyle::Expanded => {
                let bump = usize::from(self.o.p.indent_nested_queries);
                (outer_indent + bump, outer_extra)
            }
            SubqueryStyle::Aligned => {
                let sep = usize::from(self.w.pending_space && !self.w.force_join);
                let col = self.w.cur_width() + sep + 1;
                let unit = self.o.indent_unit_width().max(1);
                (col / unit, col % unit)
            }
        };
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        let inner_ctx = Ctx {
            clause_indent: inner_indent,
            clause_extra: inner_extra,
            top_level: false,
        };
        if style != SubqueryStyle::Aligned {
            self.w.newline(inner_indent, inner_extra);
        }
        self.emit_query_block(children, &inner_ctx);
        if has_close {
            match style {
                SubqueryStyle::Aligned => {
                    self.w.join();
                }
                _ => {
                    self.w.newline(outer_indent, outer_extra);
                }
            }
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    fn emit_function_args(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        let elems = split_commas(children);
        let stacked = self.o.p.function_arg_style == FunctionArgStyle::Stacked && elems.len() >= 2;
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        if !stacked {
            let cont = ctx.cont_indent(self.o);
            let wrap = self.o.p.function_arg_style == FunctionArgStyle::Wrap;
            for (ei, (elem, _)) in elems.into_iter().enumerate() {
                if ei > 0 {
                    self.w.join();
                    self.w.push(",");
                    self.w.last = LastTok::Open;
                    if wrap {
                        let len = self.measure_nodes(elem);
                        self.w.maybe_wrap(len + 1, cont, ctx.clause_extra);
                    }
                    self.w.space();
                }
                self.emit_flow(elem, ctx);
            }
            if has_close {
                self.w.join();
                self.w.push(")");
                self.w.last = LastTok::Operand;
            }
            return;
        }
        let outer_indent = self.w.cur.indent;
        let outer_extra = self.w.cur.extra;
        let arg_indent = outer_indent + cont_units(self.o);
        for (ei, (elem, _)) in elems.into_iter().enumerate() {
            if ei > 0 {
                self.w.join();
                self.w.push(",");
                self.w.last = LastTok::Open;
            }
            self.w.newline(arg_indent, outer_extra);
            self.emit_flow(elem, ctx);
        }
        if has_close {
            self.w.newline(outer_indent, outer_extra);
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    fn emit_in_list(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        let elems = split_commas(children);
        let cont = ctx.cont_indent(self.o);
        let style = self.o.p.in_list_style;
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        match style {
            InListStyle::Stacked if elems.len() > 1 => {
                let outer_indent = self.w.cur.indent;
                let outer_extra = self.w.cur.extra;
                let item_indent = outer_indent + cont_units(self.o);
                for (ei, (elem, _)) in elems.into_iter().enumerate() {
                    if ei > 0 {
                        self.w.join();
                        self.w.push(",");
                        self.w.last = LastTok::Open;
                    }
                    self.w.newline(item_indent, outer_extra);
                    self.emit_flow(elem, ctx);
                }
                if has_close {
                    self.w.newline(outer_indent, outer_extra);
                    self.w.push(")");
                    self.w.last = LastTok::Operand;
                }
                return;
            }
            InListStyle::PreserveExisting => {
                for (ei, (elem, _)) in elems.into_iter().enumerate() {
                    if ei > 0 {
                        self.w.join();
                        self.w.push(",");
                        self.w.last = LastTok::Open;
                        let broke = elem
                            .first()
                            .map(|n| n.newlines_before() > 0)
                            .unwrap_or(false);
                        if broke {
                            self.w.newline(cont, ctx.clause_extra);
                        } else {
                            self.w.space();
                        }
                    }
                    self.emit_flow(elem, ctx);
                }
            }
            _ => {
                let wrap = style == InListStyle::Wrap;
                for (ei, (elem, _)) in elems.into_iter().enumerate() {
                    if ei > 0 {
                        self.w.join();
                        self.w.push(",");
                        self.w.last = LastTok::Open;
                        if wrap {
                            let len = self.measure_nodes(elem);
                            self.w.maybe_wrap(len + 1, cont, ctx.clause_extra);
                        }
                        self.w.space();
                    }
                    self.emit_flow(elem, ctx);
                }
            }
        }
        if has_close {
            self.w.join();
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    fn emit_insert_columns(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        let elems = split_commas(children);
        let one_per_line =
            self.o.p.insert_one_column_per_line || self.o.p.dml_style == DmlStyle::Expanded;
        if one_per_line && elems.len() > 1 {
            self.emit_paren_one_per_line(children, has_close, ctx);
            return;
        }
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        let cont = ctx.cont_indent(self.o);
        for (ei, (elem, _)) in elems.into_iter().enumerate() {
            if ei > 0 {
                self.w.join();
                self.w.push(",");
                self.w.last = LastTok::Open;
                let len = self.measure_nodes(elem);
                self.w.maybe_wrap(len + 1, cont, ctx.clause_extra);
                self.w.space();
            }
            self.emit_flow(elem, ctx);
        }
        if has_close {
            self.w.join();
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    /// `(` then each comma-separated element on its own line one
    /// continuation in from the line holding the `(`, and `)` back at that
    /// line's indent.
    fn emit_paren_one_per_line(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        let outer_indent = self.w.cur.indent;
        let outer_extra = self.w.cur.extra;
        let item_indent = outer_indent + cont_units(self.o);
        for (ei, (elem, _)) in split_commas(children).into_iter().enumerate() {
            if ei > 0 {
                self.w.join();
                self.w.push(",");
                self.w.last = LastTok::Open;
            }
            self.w.newline(item_indent, outer_extra);
            self.emit_flow(elem, ctx);
        }
        if has_close {
            self.w.newline(outer_indent, outer_extra);
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    // -- DDL -------------------------------------------------------------------

    fn emit_ddl_body(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        self.ddl_body_depth += 1;
        self.emit_ddl_body_inner(children, has_close, ctx);
        self.ddl_body_depth -= 1;
    }

    fn emit_ddl_body_inner(&mut self, children: &[Node<'a>], has_close: bool, ctx: &Ctx) {
        if self.o.p.ddl_style == DdlStyle::Compact
            && let Some(text) = self.render_compact(children)
        {
            let projected = self.w.cur_width() + 2 + text.chars().count();
            if projected <= self.o.max_line() {
                self.w.push("(");
                self.w.join();
                self.w.push(&text);
                if has_close {
                    self.w.join();
                    self.w.push(")");
                }
                self.w.last = LastTok::Operand;
                return;
            }
        }

        let defs = split_commas(children);
        let outer_indent = self.w.cur.indent;
        let outer_extra = self.w.cur.extra;
        let def_indent = outer_indent + 1;
        self.w.push("(");
        self.w.join();
        self.w.last = LastTok::Open;
        self.w.new_scope();

        let mut seen_constraint_gap = false;
        for (di, (def, _)) in defs.into_iter().enumerate() {
            let is_table_constraint = def
                .iter()
                .find(|n| !n.is_comment())
                .and_then(|n| match n {
                    Node::Tok(i) => word_text(&i.tok).map(|w| w.to_ascii_lowercase()),
                    _ => None,
                })
                .map(|w| {
                    matches!(
                        w.as_str(),
                        "constraint"
                            | "primary"
                            | "foreign"
                            | "unique"
                            | "check"
                            | "key"
                            | "index"
                            | "exclude"
                    )
                })
                .unwrap_or(false);
            if di > 0 {
                match self.o.p.comma_style {
                    CommaStyle::Leading => {
                        self.w.newline(def_indent, outer_extra);
                        if self.o.p.constraints_after_columns
                            && is_table_constraint
                            && !seen_constraint_gap
                        {
                            self.w.blank_lines(1);
                            seen_constraint_gap = true;
                        }
                        self.w.push(", ");
                        self.w.join();
                    }
                    _ => {
                        self.w.join();
                        self.w.push(",");
                        self.w.newline(def_indent, outer_extra);
                        if self.o.p.constraints_after_columns
                            && is_table_constraint
                            && !seen_constraint_gap
                        {
                            self.w.blank_lines(1);
                            seen_constraint_gap = true;
                        }
                    }
                }
            } else {
                self.w.newline(def_indent, outer_extra);
            }
            self.emit_column_def(def, ctx, is_table_constraint);
        }
        if has_close {
            self.w.newline(outer_indent, outer_extra);
            self.w.push(")");
            self.w.last = LastTok::Operand;
        }
    }

    fn emit_column_def(&mut self, def: &[Node<'a>], ctx: &Ctx, is_table_constraint: bool) {
        if is_table_constraint {
            self.emit_flow(def, ctx);
            return;
        }
        let mut idx = 0;
        while idx < def.len() && def[idx].is_comment() {
            self.emit_node(&def[idx], ctx);
            idx += 1;
        }
        // Column name.
        if idx < def.len() {
            self.emit_node(&def[idx], ctx);
            idx += 1;
        }
        // Type word (+ optional `(n)` / `(p, s)`).
        if idx < def.len() {
            if self.o.p.align_data_types {
                self.w.mark(AlignTag::DataType);
            }
            if let Node::Tok(item) = &def[idx] {
                if word_text(&item.tok).is_some() {
                    self.emit_word_as(item, WordRole::DataType);
                    idx += 1;
                    if idx < def.len()
                        && let Node::Paren {
                            children,
                            has_close,
                            ..
                        } = &def[idx]
                    {
                        self.w.join();
                        self.w.push("(");
                        self.w.join();
                        self.w.last = LastTok::Open;
                        self.emit_flow_compact(children, ctx);
                        if *has_close {
                            self.w.join();
                            self.w.push(")");
                            self.w.last = LastTok::Operand;
                        }
                        idx += 1;
                    }
                } else {
                    self.emit_node(&def[idx], ctx);
                    idx += 1;
                }
            } else {
                self.emit_node(&def[idx], ctx);
                idx += 1;
            }
        }
        // Remaining modifiers; tag NULL / NOT NULL.
        let mut tagged_null = false;
        while idx < def.len() {
            let n = &def[idx];
            if !tagged_null && self.o.p.align_nullability && (n.is_word("null") || n.is_word("not"))
            {
                self.w.mark(AlignTag::Nullability);
                tagged_null = true;
            }
            self.emit_node(n, ctx);
            idx += 1;
        }
    }

    // -- item emission -----------------------------------------------------------

    fn emit_item(&mut self, item: &Item<'a>, ctx: &Ctx, compact: bool) {
        match item.tok.kind {
            TokenKind::Comment(shape) => {
                if compact {
                    // Line comments were excluded by render_compact; block
                    // comments inline with spaces.
                    self.w.space();
                    self.w.push(item.tok.text);
                    self.w.space();
                } else {
                    self.emit_comment(item, shape, ctx);
                }
            }
            _ => self.emit_plain_token(item),
        }
    }

    fn emit_comment(&mut self, item: &Item<'a>, shape: CommentShape, ctx: &Ctx) {
        // A comment is standalone when the source had it on its own line OR
        // the emitter just started a fresh line (a reformat pass would then
        // see it with a newline before it — treating it inline here would
        // break idempotency).
        let standalone = item.newlines_before > 0 || !self.w.started || self.w.cur.text.is_empty();
        // A standalone comment keeps the indent of the (empty) line the
        // emitter is already on — the list/clause emitters position that line
        // — falling back to the clause indent mid-line. Reusing the current
        // indent is what makes comment placement idempotent.
        let (ind, ext) = if self.w.cur.text.is_empty() {
            (self.w.cur.indent, self.w.cur.extra)
        } else {
            (ctx.clause_indent, ctx.clause_extra)
        };
        match shape {
            CommentShape::Line => {
                // `move_trailing_comments_with_expression`: a comment that
                // trailed code on its source line, where the formatter has
                // since broken the line, stays at the end of that code's line.
                if item.newlines_before == 0
                    && self.w.cur.text.is_empty()
                    && self.o.p.move_trailing_comments_with_expression
                    && self.o.p.keep_inline_comments_inline
                {
                    let tag = (self.o.p.comment_style == CommentStyle::Align)
                        .then_some(AlignTag::Comment);
                    if self
                        .w
                        .append_to_previous_line(item.tok.text.trim_end(), tag)
                    {
                        return;
                    }
                }
                if standalone {
                    let blanks = (item.newlines_before.saturating_sub(1) as usize)
                        .min(self.o.p.max_consecutive_blank_lines() as usize);
                    self.w.newline(ind, ext);
                    self.w.blank_lines(blanks);
                    self.w.push(item.tok.text.trim_end());
                    self.w.cur.ends_in_line_comment = true;
                    self.w.newline(ind, ext);
                } else if self.o.p.keep_inline_comments_inline {
                    if self.o.p.comment_style == CommentStyle::Align {
                        self.w.mark(AlignTag::Comment);
                    }
                    self.w.space();
                    self.w.push(item.tok.text.trim_end());
                    self.w.cur.ends_in_line_comment = true;
                    let indent = self.w.cur.indent;
                    let extra = self.w.cur.extra;
                    self.w.newline(indent, extra);
                } else {
                    // Own line, after the content it followed.
                    self.w.newline(ind, ext);
                    self.w.push(item.tok.text.trim_end());
                    self.w.cur.ends_in_line_comment = true;
                    self.w.newline(ind, ext);
                }
            }
            CommentShape::Block => {
                let text = if self.o.p.format_inside_block_comments && item.tok.text.contains('\n')
                {
                    reindent_block_comment(
                        item.tok.text,
                        self.w.cur.indent,
                        self.w.cur.extra,
                        self.o,
                    )
                } else {
                    item.tok.text.to_string()
                };
                if standalone {
                    let blanks = (item.newlines_before.saturating_sub(1) as usize)
                        .min(self.o.p.max_consecutive_blank_lines() as usize);
                    self.w.newline(ind, ext);
                    self.w.blank_lines(blanks);
                    push_multiline(&mut self.w, &text);
                    self.w.newline(ind, ext);
                } else {
                    self.w.space();
                    push_multiline(&mut self.w, &text);
                    self.w.space();
                }
            }
        }
    }

    fn emit_plain_token(&mut self, item: &Item<'a>) {
        let t = &item.tok;
        if item.attach {
            // Overrides the separator every branch below asks for.
            self.w.join();
        }
        match t.kind {
            TokenKind::Comma => {
                self.w.join();
                self.w.push(",");
                self.w.space();
                self.w.last = LastTok::Open;
            }
            TokenKind::Dot => {
                self.w.join();
                self.w.push(".");
                self.w.join();
                self.w.last = LastTok::Operator;
                self.after_cast_op = false;
                self.after_dot = true;
                return;
            }
            TokenKind::Semicolon => {
                self.w.join();
                self.w.push(";");
                self.w.space();
                self.w.last = LastTok::Open;
            }
            TokenKind::Operator => {
                let sym = t.text;
                let unary_candidate = matches!(sym, "-" | "+" | "~");
                if sym == "::" {
                    self.w.join();
                    self.w.push(sym);
                    self.w.join();
                    self.w.last = LastTok::Operator;
                    self.after_cast_op = true;
                    return;
                } else if unary_candidate
                    && matches!(
                        self.w.last,
                        LastTok::None | LastTok::Word | LastTok::Operator | LastTok::Open
                    )
                {
                    self.w.space();
                    self.w.push(sym);
                    self.w.join();
                    self.w.last = LastTok::Operator;
                } else if self.o.p.spaces_around_operators {
                    self.w.space();
                    self.w.push(sym);
                    self.w.space();
                    self.w.last = LastTok::Operator;
                } else {
                    self.w.join();
                    self.w.push(sym);
                    self.w.join();
                    self.w.last = LastTok::Operator;
                }
            }
            TokenKind::Keyword | TokenKind::Ident(_) => {
                let role = self.infer_role(item);
                let text = self.cased_word(item, role);
                self.w.space();
                self.w.push(&text);
                self.w.space();
                self.w.last = match role {
                    WordRole::Identifier | WordRole::Function => LastTok::Operand,
                    _ => {
                        if is_expression_tail_keyword(t.text) {
                            LastTok::Operand
                        } else {
                            LastTok::Word
                        }
                    }
                };
            }
            _ => {
                self.w.space();
                self.w.push(t.text);
                self.w.space();
                self.w.last = LastTok::Operand;
            }
        }
        self.after_cast_op = false;
        self.after_dot = false;
    }

    fn emit_word_as(&mut self, item: &Item<'a>, role: WordRole) {
        let text = self.cased_word(item, role);
        self.w.space();
        self.w.push(&text);
        self.w.space();
        self.w.last = LastTok::Operand;
    }

    fn infer_role(&self, item: &Item<'a>) -> WordRole {
        if item.is_function_name {
            return WordRole::Function;
        }
        if item.is_context_keyword {
            return WordRole::Keyword;
        }
        match item.tok.kind {
            // `o.type`, `t.key`: a keyword spelled behind a qualifier is a
            // column (or table) name.
            TokenKind::Keyword if self.after_dot => WordRole::Identifier,
            TokenKind::Keyword => {
                if word_in(TYPE_WORDS, item.tok.text) {
                    WordRole::DataType
                } else {
                    WordRole::Keyword
                }
            }
            TokenKind::Ident(IdentQuote::None)
                if (self.ddl_body_depth > 0 || self.after_cast_op)
                    && word_in(TYPE_WORDS, item.tok.text) =>
            {
                WordRole::DataType
            }
            _ => WordRole::Identifier,
        }
    }

    fn cased_word(&self, item: &Item<'a>, role: WordRole) -> String {
        let t = &item.tok;
        let g = &self.o.p.keyword_group_casing;
        match role {
            WordRole::Function => {
                let case = if g.enabled {
                    g.functions
                } else {
                    match self.o.p.function_case {
                        FunctionCase::Preserve => KeywordCase::Preserve,
                        FunctionCase::Lowercase => KeywordCase::Lowercase,
                        FunctionCase::Uppercase => KeywordCase::Uppercase,
                    }
                };
                match t.kind {
                    TokenKind::Ident(IdentQuote::None) | TokenKind::Keyword => {
                        apply_keyword_case(t.text, case, self.o.surrounding_upper)
                    }
                    _ => t.text.to_string(),
                }
            }
            WordRole::DataType => {
                let case = if g.enabled {
                    g.data_types
                } else {
                    self.o.p.keyword_case
                };
                match t.kind {
                    TokenKind::Ident(IdentQuote::None) | TokenKind::Keyword => {
                        apply_keyword_case(t.text, case, self.o.surrounding_upper)
                    }
                    _ => t.text.to_string(),
                }
            }
            WordRole::Keyword => {
                let w = t.text;
                let case = if g.enabled {
                    if word_in(CAT_JOINS, w) {
                        g.joins
                    } else if word_in(CAT_SET_OPS, w) {
                        g.set_operations
                    } else if word_in(CAT_DML, w) {
                        g.dml
                    } else if word_in(CAT_DDL, w) {
                        g.ddl
                    } else if word_in(CAT_TRANSACTION, w) {
                        g.transaction
                    } else if word_in(CAT_WORD_OPERATORS, w) {
                        g.operators
                    } else if word_in(CAT_CLAUSES, w) {
                        g.clauses
                    } else {
                        g.keywords
                    }
                } else {
                    self.o.p.keyword_case
                };
                apply_keyword_case(w, case, self.o.surrounding_upper)
            }
            WordRole::Identifier => self.identifier_text(item),
        }
    }

    fn identifier_text(&self, item: &Item<'a>) -> String {
        let t = &item.tok;
        let quoted = t.ident_quoted();
        let inner = t.ident_text();
        let inner: &str = &inner;

        // Quotes are only ever removed when the bare name still parses and
        // resolves to the same object (never off `[User]`), and only ever
        // added when they change nothing but the spelling.
        match self.o.p.identifier_quoting {
            IdentifierQuoting::PreserveExisting => {
                if quoted {
                    t.text.to_string()
                } else {
                    self.cased_plain_identifier(inner)
                }
            }
            IdentifierQuoting::QuoteOnlyWhenRequired
            | IdentifierQuoting::RemoveUnnecessaryQuotes => {
                if quoted && safe_to_unquote(inner, self.o.dialect) {
                    self.cased_plain_identifier(inner)
                } else if quoted {
                    t.text.to_string()
                } else if self.o.p.identifier_quoting == IdentifierQuoting::QuoteOnlyWhenRequired
                    && self.after_dot
                    && reserved_needs_quotes(inner, self.o.dialect)
                {
                    // `Shared.User`: a reserved word as the member of a
                    // qualified name is always a name, and SQL Server (among
                    // others) rejects it bare. A lone `USER` is left alone —
                    // it may be the niladic function.
                    quote_plain(&self.cased_plain_identifier(inner), self.o.dialect)
                } else {
                    self.cased_plain_identifier(inner)
                }
            }
            IdentifierQuoting::AlwaysQuote => {
                if quoted {
                    t.text.to_string()
                } else if plain_identifier_ok(inner)
                    && (self.after_dot
                        || !(matches!(t.kind, TokenKind::Keyword)
                            || reserved_needs_quotes(inner, self.o.dialect)))
                {
                    // A bare reserved word outside a qualified name is a
                    // keyword or niladic function (`USER`, `CURRENT_USER`),
                    // which quoting would turn into a column reference.
                    quote_plain(inner, self.o.dialect)
                } else {
                    t.text.to_string()
                }
            }
        }
    }

    fn cased_plain_identifier(&self, text: &str) -> String {
        match self.o.p.identifier_case {
            IdentifierCase::Preserve => text.to_string(),
            IdentifierCase::Lowercase => text.to_ascii_lowercase(),
            IdentifierCase::Uppercase => text.to_ascii_uppercase(),
            IdentifierCase::PascalCase => pascal_case_identifier(text),
        }
    }

    fn measure_nodes(&mut self, nodes: &[Node<'a>]) -> usize {
        self.render_compact(nodes)
            .map(|s| s.chars().count())
            .unwrap_or(usize::MAX / 4)
    }
}

/// The statement is INSERT / UPDATE / DELETE / MERGE (by its first word).
fn is_dml_statement(nodes: &[Node]) -> bool {
    nodes.iter().find(|n| !n.is_comment()).is_some_and(|n| {
        ["insert", "update", "delete", "merge"]
            .iter()
            .any(|w| n.is_word(w))
    })
}

/// Keywords that terminate an expression (a following `-` is binary).
fn is_expression_tail_keyword(w: &str) -> bool {
    word_in(&["null", "true", "false", "end", "rownum", "dual"], w)
}

// ---------------------------------------------------------------------------
// List emission
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ListLayout {
    Stacked,
    Wrap,
    InlineWhenShort,
    PreserveSource,
}

fn select_layout(style: SelectListStyle) -> ListLayout {
    match style {
        SelectListStyle::PreserveExisting => ListLayout::PreserveSource,
        SelectListStyle::OneLineWhenShort => ListLayout::InlineWhenShort,
        SelectListStyle::Stacked => ListLayout::Stacked,
        SelectListStyle::Wrap => ListLayout::Wrap,
    }
}

struct ListOpts {
    break_before_first: bool,
    align_aliases: bool,
    align_assign: bool,
}

impl<'a, 'o, 'p> Formatter<'a, 'o, 'p> {
    fn emit_list(&mut self, list: &[Node<'a>], layout: ListLayout, ctx: &Ctx, opts: ListOpts) {
        let elems = split_commas(list);
        let count = elems.len();
        let item_indent = ctx.cont_indent(self.o);

        let layout = if layout == ListLayout::InlineWhenShort {
            let threshold = self.o.p.inline_column_threshold() as usize;
            let fits_count = threshold == 0 || count <= threshold;
            let fits_line = self
                .render_compact(list)
                .map(|s| self.w.cur_width() + 1 + s.chars().count() <= self.o.max_line())
                .unwrap_or(false);
            if fits_count && fits_line {
                ListLayout::Wrap // fits on one line, so Wrap won't break it
            } else {
                ListLayout::Stacked
            }
        } else {
            layout
        };

        self.w.new_scope();
        for (ei, (elem, _)) in elems.into_iter().enumerate() {
            let break_before = match layout {
                ListLayout::Stacked => ei > 0 || opts.break_before_first,
                ListLayout::PreserveSource => {
                    let broke = elem
                        .first()
                        .map(|n| n.newlines_before() > 0)
                        .unwrap_or(false);
                    broke && (ei > 0 || opts.break_before_first)
                }
                _ => false,
            };

            if ei > 0 {
                match self.o.p.comma_style {
                    CommaStyle::Leading if break_before => {
                        self.w.newline(item_indent, ctx.clause_extra);
                        self.w.push(", ");
                        self.w.join();
                    }
                    _ => {
                        self.w.join();
                        self.w.push(",");
                        self.w.last = LastTok::Open;
                        if break_before {
                            self.w.newline(item_indent, ctx.clause_extra);
                        } else {
                            if layout == ListLayout::Wrap {
                                let len = self.measure_nodes(elem);
                                self.w.maybe_wrap(len + 1, item_indent, ctx.clause_extra);
                            }
                            self.w.space();
                        }
                    }
                }
            } else if break_before {
                self.w.newline(item_indent, ctx.clause_extra);
            }

            self.emit_list_element(elem, ctx, &opts);
        }
    }

    fn emit_list_element(&mut self, elem: &[Node<'a>], ctx: &Ctx, opts: &ListOpts) {
        if opts.align_assign {
            let mut tagged = false;
            for n in elem {
                if !tagged
                    && let Node::Tok(item) = n
                    && item.tok.kind == TokenKind::Operator
                    && item.tok.text == "="
                {
                    self.w.mark(AlignTag::SetAssign);
                    tagged = true;
                }
                self.emit_node(n, ctx);
            }
            return;
        }
        if opts.align_aliases {
            let alias_pos = find_alias_pos(elem);
            let mut i = 0usize;
            while i < elem.len() {
                if elem[i].is_word("case") {
                    let end = find_case_end(elem, i);
                    self.emit_case(&elem[i..end], ctx);
                    i = end;
                    continue;
                }
                if Some(i) == alias_pos {
                    self.w.mark(AlignTag::Alias);
                }
                self.emit_node(&elem[i], ctx);
                i += 1;
            }
            return;
        }
        self.emit_flow(elem, ctx);
    }
}
