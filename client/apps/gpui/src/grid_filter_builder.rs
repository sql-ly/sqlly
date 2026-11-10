//! Visual filter-builder model: maps UI rows onto the datatable's
//! [`ColumnFilter`]s and onto a dialect-correct WHERE clause.
//!
//! GPUI-free so the operator mapping, SQL rendering, and client-side
//! predicate can be unit-tested without a window. Flat AND models still
//! write `GridState.filters`; trees use [`FilterBuilderModel::row_passes`].
//! `column_filters_to_where` remains the "Add Active Filters to WHERE"
//! renderer and is unchanged.

use serde::{Deserialize, Serialize};
use sqlly_core::ResultColumn;
use sqlly_datatable::filter::{cell_passes_filter, parse_ymd_to_unix, uses_number_ops};
use sqlly_datatable::format::format_cell;
use sqlly_datatable::{
    CellValue, Column, ColumnFilter, ColumnKind, FilterPredicate, NumberOp, ResolvedColumnFormat,
    TextOp,
};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::sql_literal::sql_identifier_for;

/// Operators offered by the visual builder. Text vs number sets are chosen
/// with [`FilterBuilderModel::ops_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuilderOp {
    Contains,
    NotContains,
    BeginsWith,
    EndsWith,
    Is,
    IsNot,
    IsNull,
    IsNotNull,
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Between,
    In,
    NotIn,
    IsEmpty,
    IsNotEmpty,
}

impl BuilderOp {
    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::NotContains => "does not contain",
            Self::BeginsWith => "begins with",
            Self::EndsWith => "ends with",
            Self::Is => "is",
            Self::IsNot => "is not",
            Self::IsNull => "is null",
            Self::IsNotNull => "is not null",
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Between => "between",
            Self::In => "is one of",
            Self::NotIn => "is not one of",
            Self::IsEmpty => "is empty",
            Self::IsNotEmpty => "is not empty",
        }
    }

    pub fn needs_a(self) -> bool {
        !matches!(
            self,
            Self::IsNull | Self::IsNotNull | Self::IsEmpty | Self::IsNotEmpty
        )
    }

    pub fn needs_b(self) -> bool {
        matches!(self, Self::Between)
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            Self::Contains
                | Self::NotContains
                | Self::BeginsWith
                | Self::EndsWith
                | Self::Is
                | Self::IsNot
                | Self::In
                | Self::NotIn
                | Self::IsEmpty
                | Self::IsNotEmpty
        )
    }

    pub(crate) fn is_number(self) -> bool {
        matches!(
            self,
            Self::Eq
                | Self::Ne
                | Self::Gt
                | Self::Ge
                | Self::Lt
                | Self::Le
                | Self::Between
                | Self::In
                | Self::NotIn
        )
    }

    /// Operators the datatable `ColumnFilter` fast path can represent.
    fn is_legacy(self) -> bool {
        !matches!(
            self,
            Self::In | Self::NotIn | Self::IsEmpty | Self::IsNotEmpty
        )
    }
}

/// Every operator, in declaration order. The visual query builder offers this
/// full set per row because the completion catalog does not carry column types.
pub const ALL_OPS: &[BuilderOp] = &[
    BuilderOp::Contains,
    BuilderOp::NotContains,
    BuilderOp::BeginsWith,
    BuilderOp::EndsWith,
    BuilderOp::Is,
    BuilderOp::IsNot,
    BuilderOp::IsNull,
    BuilderOp::IsNotNull,
    BuilderOp::Eq,
    BuilderOp::Ne,
    BuilderOp::Gt,
    BuilderOp::Ge,
    BuilderOp::Lt,
    BuilderOp::Le,
    BuilderOp::Between,
    BuilderOp::In,
    BuilderOp::NotIn,
    BuilderOp::IsEmpty,
    BuilderOp::IsNotEmpty,
];

const TEXT_OPS: &[BuilderOp] = &[
    BuilderOp::Contains,
    BuilderOp::NotContains,
    BuilderOp::BeginsWith,
    BuilderOp::EndsWith,
    BuilderOp::Is,
    BuilderOp::IsNot,
    BuilderOp::IsNull,
    BuilderOp::IsNotNull,
    BuilderOp::In,
    BuilderOp::NotIn,
    BuilderOp::IsEmpty,
    BuilderOp::IsNotEmpty,
];

const NUMBER_OPS: &[BuilderOp] = &[
    BuilderOp::Eq,
    BuilderOp::Ne,
    BuilderOp::Gt,
    BuilderOp::Ge,
    BuilderOp::Lt,
    BuilderOp::Le,
    BuilderOp::Between,
    BuilderOp::IsNull,
    BuilderOp::IsNotNull,
    BuilderOp::In,
    BuilderOp::NotIn,
];

/// Column a condition applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnTarget {
    Index(usize),
    /// Every text-kind column, OR-ed. Only text ops are offered.
    AnyText,
}

/// One condition in the builder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterRow {
    pub column: ColumnTarget,
    pub op: BuilderOp,
    pub a: String,
    pub b: String,
    /// Disabled rows stay in the model (and in saved filters) but are
    /// skipped by every application path: grid filters, WHERE rendering,
    /// and the client-side predicate.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl FilterRow {
    pub fn new(column: usize, op: BuilderOp) -> Self {
        Self {
            column: ColumnTarget::Index(column),
            op,
            a: String::new(),
            b: String::new(),
            enabled: true,
        }
    }

    #[allow(dead_code)]
    pub fn any_text(op: BuilderOp) -> Self {
        Self {
            column: ColumnTarget::AnyText,
            op,
            a: String::new(),
            b: String::new(),
            enabled: true,
        }
    }
}

/// How a group's children are combined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Conjunction {
    #[default]
    And,
    Or,
}

/// A condition or a nested group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterItem {
    Row(FilterRow),
    Group(FilterGroup),
}

/// AND/OR group of conditions. The model is recursive and the builder
/// renders every level (see [`FilterBuilderModel::layout`]).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FilterGroup {
    pub conjunction: Conjunction,
    pub items: Vec<FilterItem>,
}

/// The builder's editable filter tree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterBuilderModel {
    pub root: FilterGroup,
}

/// One event of [`FilterBuilderModel::layout`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutItem {
    /// A nested group opens; `path` addresses it for the group actions.
    GroupStart { path: Vec<usize> },
    /// A condition; `leaf` is its pre-order leaf index.
    Row { leaf: usize, row: FilterRow },
    /// The group opened by the matching `GroupStart` closes.
    GroupEnd { path: Vec<usize> },
}

impl FilterBuilderModel {
    pub fn ops_for(kind: ColumnKind) -> &'static [BuilderOp] {
        if uses_number_ops(kind) {
            NUMBER_OPS
        } else {
            TEXT_OPS
        }
    }

    /// Operators for the Any text column target.
    pub fn ops_for_any_text() -> &'static [BuilderOp] {
        TEXT_OPS
    }

    pub fn ops_for_target(target: ColumnTarget, kinds: &[ColumnKind]) -> &'static [BuilderOp] {
        match target {
            ColumnTarget::AnyText => Self::ops_for_any_text(),
            ColumnTarget::Index(i) => {
                Self::ops_for(kinds.get(i).copied().unwrap_or(ColumnKind::Text))
            }
        }
    }

    /// Every leaf row in pre-order (root, then nested groups).
    pub fn flat_rows(&self) -> Vec<&FilterRow> {
        let mut out = Vec::new();
        collect_rows(&self.root, &mut out);
        out
    }

    /// Root is AND, every child is a row on an indexed column with a
    /// pre-existing (datatable) operator. Disabled rows are skipped by every
    /// application path, so they never block the fast path.
    pub fn is_flat_and(&self) -> bool {
        self.root.conjunction == Conjunction::And
            && self.root.items.iter().all(|item| match item {
                FilterItem::Row(row) => {
                    !row.enabled
                        || (matches!(row.column, ColumnTarget::Index(_)) && row.op.is_legacy())
                }
                FilterItem::Group(_) => false,
            })
    }

    pub fn push_row(&mut self, row: FilterRow) {
        self.root.items.push(FilterItem::Row(row));
    }

    /// Add `row` so it *narrows* the current filter: pushed onto an AND
    /// root, or — when the root is OR — the existing tree is wrapped as one
    /// group of a new AND root. Pushing onto an OR root would widen the
    /// filter instead (a quick filter must never show more rows).
    pub fn and_row(&mut self, row: FilterRow) {
        if self.root.conjunction == Conjunction::Or && !self.root.items.is_empty() {
            let previous = std::mem::take(&mut self.root);
            self.root = FilterGroup {
                conjunction: Conjunction::And,
                items: vec![FilterItem::Group(previous)],
            };
        }
        self.root.conjunction = Conjunction::And;
        self.push_row(row);
    }

    pub fn clear_items(&mut self) {
        self.root.items.clear();
    }

    pub fn leaf_count(&self) -> usize {
        count_rows(&self.root)
    }

    pub fn leaf_row_mut(&mut self, index: usize) -> Option<&mut FilterRow> {
        leaf_row_mut(&mut self.root, index)
    }

    /// The nested group at `path` (child indices from the root; an empty
    /// path is the root itself).
    pub fn group_at(&self, path: &[usize]) -> Option<&FilterGroup> {
        let mut group = &self.root;
        for &i in path {
            match group.items.get(i) {
                Some(FilterItem::Group(inner)) => group = inner,
                _ => return None,
            }
        }
        Some(group)
    }

    fn group_at_mut(&mut self, path: &[usize]) -> Option<&mut FilterGroup> {
        let mut group = &mut self.root;
        for &i in path {
            match group.items.get_mut(i) {
                Some(FilterItem::Group(inner)) => group = inner,
                _ => return None,
            }
        }
        Some(group)
    }

    /// Append a group (holding one default row) to the root. Its
    /// conjunction is the opposite of the root's, which is the only reason
    /// to nest.
    pub fn add_group(&mut self, kinds: &[ColumnKind]) {
        let conjunction = match self.root.conjunction {
            Conjunction::And => Conjunction::Or,
            Conjunction::Or => Conjunction::And,
        };
        self.root.items.push(FilterItem::Group(FilterGroup {
            conjunction,
            items: vec![FilterItem::Row(default_filter_row(kinds))],
        }));
    }

    pub fn add_row_at_root(&mut self, kinds: &[ColumnKind]) {
        self.root
            .items
            .push(FilterItem::Row(default_filter_row(kinds)));
    }

    /// Append a default row to the nested group at `path` (non-empty).
    pub fn add_row_in_group(&mut self, path: &[usize], kinds: &[ColumnKind]) {
        if path.is_empty() {
            return;
        }
        if let Some(group) = self.group_at_mut(path) {
            group.items.push(FilterItem::Row(default_filter_row(kinds)));
        }
    }

    /// Remove the nested group at `path` (never the root).
    pub fn remove_group(&mut self, path: &[usize]) {
        let Some((&last, parent)) = path.split_last() else {
            return;
        };
        if let Some(parent) = self.group_at_mut(parent)
            && matches!(parent.items.get(last), Some(FilterItem::Group(_)))
        {
            parent.items.remove(last);
        }
    }

    pub fn set_group_conjunction(&mut self, path: &[usize], conjunction: Conjunction) {
        if let Some(group) = self.group_at_mut(path) {
            group.conjunction = conjunction;
        }
    }

    /// The builder's render plan: the tree in pre-order as open/row/close
    /// events. Row events carry the leaf index that [`Self::leaf_row_mut`],
    /// [`Self::remove_leaf`], and the per-row inputs use, so the UI can
    /// render any nesting depth without its indices drifting from the
    /// model's.
    pub fn layout(&self) -> Vec<LayoutItem> {
        let mut out = Vec::new();
        let mut leaf = 0usize;
        layout_group(&self.root, &mut Vec::new(), &mut leaf, &mut out);
        out
    }

    /// Remove the `index`th leaf in pre-order. Empty nested groups are pruned.
    pub fn remove_leaf(&mut self, index: usize) {
        let _ = remove_leaf_from(&mut self.root, index);
    }

    /// Reverse-map committed grid filters into a flat AND root. Value-checklist
    /// halves are ignored (the builder is operator-only). `null_text` is the
    /// formatted null placeholder used to detect Is/IsNot-null predicates.
    pub fn from_column_filters(
        filters: &[ColumnFilter],
        kinds: &[ColumnKind],
        null_texts: &[String],
    ) -> Self {
        let mut items = Vec::new();
        for (column, filter) in filters.iter().enumerate() {
            let kind = kinds.get(column).copied().unwrap_or(ColumnKind::Text);
            let null_text = null_texts.get(column).map(|s| s.as_str()).unwrap_or("NULL");
            if let Some(row) = filter_row_from_predicate(column, kind, &filter.predicate, null_text)
            {
                items.push(FilterItem::Row(row));
            }
        }
        Self {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items,
            },
        }
    }

    /// One [`ColumnFilter`] per column when this is a flat AND of legacy
    /// operators. `None` for trees, Any-text targets, or the new operators.
    ///
    /// Multiple rows on the same column: the last valid operator wins for
    /// `predicate`. Null handling: the datatable has no dedicated null
    /// operator, so [`BuilderOp::IsNull`] / [`BuilderOp::IsNotNull`] become
    /// `FilterPredicate::Text { op: Is / IsNot, operand: null_text }`.
    ///
    /// Number predicates on date-kind columns are also `None`: the datatable
    /// evaluates them against `CellValue::Date` cells, but the app carries
    /// dates as formatted text, so the fast path would match nothing. Those
    /// models fall through to the tree path, whose [`Self::row_passes`]
    /// compares calendar values itself.
    pub fn to_column_filters(
        &self,
        columns: &[Column],
        null_texts: &[String],
    ) -> Option<Vec<ColumnFilter>> {
        if !self.is_flat_and() {
            return None;
        }
        let mut filters = vec![ColumnFilter::default(); columns.len()];
        for row in self.flat_rows() {
            if !row.enabled {
                continue;
            }
            let ColumnTarget::Index(idx) = row.column else {
                continue;
            };
            let Some(column) = columns.get(idx) else {
                continue;
            };
            if column.kind == ColumnKind::Date && row.op.is_number() {
                return None;
            }
            // The datatable compares numbers as f64; integers past 2^53
            // must take the tree path, which compares them exactly.
            if row.op.is_number() && (is_inexact_integer(&row.a) || is_inexact_integer(&row.b)) {
                return None;
            }
            let null_text = null_texts.get(idx).map(|s| s.as_str()).unwrap_or("NULL");
            let Some(predicate) = predicate_from_row(row, column.kind, null_text) else {
                continue;
            };
            filters[idx].predicate = predicate;
        }
        Some(filters)
    }

    /// AND/OR-joined WHERE fragment, or `None` when nothing can be rendered.
    pub fn to_where_clause(
        &self,
        db: DatabaseType,
        columns: &[ResultColumn],
        kinds: &[ColumnKind],
    ) -> Option<String> {
        render_group(&self.root, db, columns, kinds, false)
    }

    /// Client-side predicate with the same truth table as SQL except NULL
    /// cells follow the grid: a NULL fails every non-null test; `IsNull`
    /// passes.
    pub fn row_passes(
        &self,
        row: &[CellValue],
        kinds: &[ColumnKind],
        fmts: &[ResolvedColumnFormat],
        null_texts: &[String],
    ) -> bool {
        eval_group(&self.root, row, kinds, fmts, null_texts).unwrap_or(true)
    }
}

fn collect_rows<'a>(group: &'a FilterGroup, out: &mut Vec<&'a FilterRow>) {
    for item in &group.items {
        match item {
            FilterItem::Row(row) => out.push(row),
            FilterItem::Group(inner) => collect_rows(inner, out),
        }
    }
}

fn count_rows(group: &FilterGroup) -> usize {
    group
        .items
        .iter()
        .map(|item| match item {
            FilterItem::Row(_) => 1,
            FilterItem::Group(inner) => count_rows(inner),
        })
        .sum()
}

fn layout_group(
    group: &FilterGroup,
    path: &mut Vec<usize>,
    leaf: &mut usize,
    out: &mut Vec<LayoutItem>,
) {
    for (i, item) in group.items.iter().enumerate() {
        match item {
            FilterItem::Row(row) => {
                out.push(LayoutItem::Row {
                    leaf: *leaf,
                    row: row.clone(),
                });
                *leaf += 1;
            }
            FilterItem::Group(inner) => {
                path.push(i);
                out.push(LayoutItem::GroupStart { path: path.clone() });
                layout_group(inner, path, leaf, out);
                out.push(LayoutItem::GroupEnd { path: path.clone() });
                path.pop();
            }
        }
    }
}

fn default_filter_row(kinds: &[ColumnKind]) -> FilterRow {
    let kind = kinds.first().copied().unwrap_or(ColumnKind::Text);
    let op = FilterBuilderModel::ops_for(kind)
        .first()
        .copied()
        .unwrap_or(BuilderOp::Contains);
    FilterRow::new(0, op)
}

fn remove_leaf_from(group: &mut FilterGroup, mut remaining: usize) -> Result<(), usize> {
    let mut i = 0;
    while i < group.items.len() {
        match &mut group.items[i] {
            FilterItem::Row(_) => {
                if remaining == 0 {
                    group.items.remove(i);
                    return Ok(());
                }
                remaining -= 1;
                i += 1;
            }
            FilterItem::Group(inner) => {
                let count = count_rows(inner);
                if remaining < count {
                    let _ = remove_leaf_from(inner, remaining);
                    if inner.items.is_empty() {
                        group.items.remove(i);
                    }
                    return Ok(());
                }
                remaining -= count;
                i += 1;
            }
        }
    }
    Err(remaining)
}

fn leaf_row_mut(group: &mut FilterGroup, mut remaining: usize) -> Option<&mut FilterRow> {
    for item in &mut group.items {
        match item {
            FilterItem::Row(row) => {
                if remaining == 0 {
                    return Some(row);
                }
                remaining -= 1;
            }
            FilterItem::Group(inner) => {
                let count = count_rows(inner);
                if remaining < count {
                    return leaf_row_mut(inner, remaining);
                }
                remaining -= count;
            }
        }
    }
    None
}

fn filter_row_from_predicate(
    column: usize,
    kind: ColumnKind,
    predicate: &FilterPredicate,
    null_text: &str,
) -> Option<FilterRow> {
    let ops = FilterBuilderModel::ops_for(kind);
    let (op, a, b) = match predicate {
        FilterPredicate::None => return None,
        FilterPredicate::Text {
            op: TextOp::Contains,
            operand,
        } => (BuilderOp::Contains, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::DoesNotContain,
            operand,
        } => (BuilderOp::NotContains, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::BeginsWith,
            operand,
        } => (BuilderOp::BeginsWith, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::EndsWith,
            operand,
        } => (BuilderOp::EndsWith, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::Is,
            operand,
        } if operand == null_text => (BuilderOp::IsNull, String::new(), String::new()),
        FilterPredicate::Text {
            op: TextOp::IsNot,
            operand,
        } if operand == null_text => (BuilderOp::IsNotNull, String::new(), String::new()),
        FilterPredicate::Text {
            op: TextOp::Is,
            operand,
        } => (BuilderOp::Is, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::IsNot,
            operand,
        } => (BuilderOp::IsNot, operand.clone(), String::new()),
        FilterPredicate::Text {
            op: TextOp::Matches,
            ..
        } => return None,
        FilterPredicate::Number {
            op: NumberOp::Eq,
            a,
            ..
        } => (BuilderOp::Eq, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Ne,
            a,
            ..
        } => (BuilderOp::Ne, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Gt,
            a,
            ..
        } => (BuilderOp::Gt, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Ge,
            a,
            ..
        } => (BuilderOp::Ge, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Lt,
            a,
            ..
        } => (BuilderOp::Lt, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Le,
            a,
            ..
        } => (BuilderOp::Le, format_number(*a), String::new()),
        FilterPredicate::Number {
            op: NumberOp::Between,
            a,
            b,
        } => (BuilderOp::Between, format_number(*a), format_number(*b)),
        FilterPredicate::Number {
            op: NumberOp::NotBetween,
            ..
        } => return None,
    };
    if !ops.contains(&op) {
        return None;
    }
    Some(FilterRow {
        column: ColumnTarget::Index(column),
        op,
        a,
        b,
        enabled: true,
    })
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Numeric operand text → `f64`. Date-kind columns also accept a literal
/// `YYYY-MM-DD` calendar date (the datatable's own date-operand format),
/// converted to the Unix-seconds value the grid's date cells carry.
fn parse_operand_number(s: &str, kind: ColumnKind) -> Option<f64> {
    if let Some(n) = parse_number(s) {
        return Some(n);
    }
    if kind == ColumnKind::Date {
        return parse_ymd_to_unix(s).map(|unix| unix as f64);
    }
    None
}

fn predicate_from_row(
    row: &FilterRow,
    kind: ColumnKind,
    null_text: &str,
) -> Option<FilterPredicate> {
    match row.op {
        BuilderOp::IsNull => Some(FilterPredicate::Text {
            op: TextOp::Is,
            operand: null_text.to_string(),
        }),
        BuilderOp::IsNotNull => Some(FilterPredicate::Text {
            op: TextOp::IsNot,
            operand: null_text.to_string(),
        }),
        BuilderOp::In | BuilderOp::NotIn | BuilderOp::IsEmpty | BuilderOp::IsNotEmpty => None,
        op if op.is_text() => {
            let text_op = match op {
                BuilderOp::Contains => TextOp::Contains,
                BuilderOp::NotContains => TextOp::DoesNotContain,
                BuilderOp::BeginsWith => TextOp::BeginsWith,
                BuilderOp::EndsWith => TextOp::EndsWith,
                BuilderOp::Is => TextOp::Is,
                BuilderOp::IsNot => TextOp::IsNot,
                _ => return None,
            };
            Some(FilterPredicate::Text {
                op: text_op,
                operand: row.a.clone(),
            })
        }
        op if op.is_number() => {
            let a = parse_operand_number(&row.a, kind)?;
            let b = if op == BuilderOp::Between {
                parse_operand_number(&row.b, kind)?
            } else {
                0.0
            };
            let number_op = match op {
                BuilderOp::Eq => NumberOp::Eq,
                BuilderOp::Ne => NumberOp::Ne,
                BuilderOp::Gt => NumberOp::Gt,
                BuilderOp::Ge => NumberOp::Ge,
                BuilderOp::Lt => NumberOp::Lt,
                BuilderOp::Le => NumberOp::Le,
                BuilderOp::Between => NumberOp::Between,
                _ => return None,
            };
            Some(FilterPredicate::Number {
                op: number_op,
                a,
                b,
            })
        }
        _ => None,
    }
}

fn parse_number(s: &str) -> Option<f64> {
    number_literal(s)?.parse::<f64>().ok()
}

/// The operand as a SQL numeric literal: the user's own digits (trimmed),
/// never a round-trip through `f64`, so `1234567890123456789` stays exact in
/// the generated SQL. Only plain decimal notation (`-12`, `3.5`, `.5`,
/// `1e6`) is accepted, which also keeps anything but a number out of the
/// SQL text.
pub(crate) fn number_literal(s: &str) -> Option<String> {
    let t = s.trim();
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(int_part)
        && frac_part.is_none_or(digits)
        && (!int_part.is_empty() || frac_part.is_some_and(|f| !f.is_empty()));
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    if !mantissa_ok || !exponent_ok {
        return None;
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())?;
    Some(t.strip_prefix('+').unwrap_or(t).to_string())
}

/// An integer operand that `f64` cannot represent exactly (beyond 2^53).
/// Such operands must compare as integers, so they never take the
/// datatable's `f64` fast path.
fn is_inexact_integer(s: &str) -> bool {
    s.trim()
        .parse::<i128>()
        .is_ok_and(|n| n.unsigned_abs() > (1u128 << 53))
}

/// IN-list items, split on commas and newlines. A pasted SQL list (the
/// "Copy for IN" output `[Id] IN (1, 2)`, or a bare `(1, 2)`) is unwrapped
/// and its quoted literals read as values, so the first item does not keep
/// the `(` and a comma inside `'a, b'` does not split it.
fn split_list(s: &str) -> Vec<String> {
    if let Some(body) = pasted_in_list_body(s) {
        return split_sql_literals(body);
    }
    s.split([',', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The text between the parentheses of `<column> IN ( … )` or of a bare
/// `( … )` list. A bare parenthesized single value such as `(none)` is left
/// alone: only a list (a separator or a quoted literal inside) is unwrapped.
fn pasted_in_list_body(s: &str) -> Option<&str> {
    let t = s.trim();
    let open = t.find('(')?;
    if !t.ends_with(')') || open + 1 > t.len() - 1 {
        return None;
    }
    let body = &t[open + 1..t.len() - 1];
    let head = t[..open].trim_end();
    if head.is_empty() {
        let inner = body.trim_start();
        let is_list = body.contains([',', '\n'])
            || inner.starts_with('\'')
            || inner.starts_with("N'")
            || inner.starts_with("n'");
        return is_list.then_some(body);
    }
    let lower = head.to_ascii_lowercase();
    let before_in = lower.strip_suffix("in")?;
    let separated = before_in
        .chars()
        .last()
        .is_some_and(|c| c.is_whitespace() || matches!(c, ']' | '"' | '`'));
    separated.then_some(body)
}

/// Splits a SQL value list: `'…'` / `N'…'` literals (with `''` escapes)
/// become their text, unquoted items are trimmed, empty items dropped.
fn split_sql_literals(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted: Option<String> = None;
    let mut in_quote = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            if c != '\'' {
                quoted.get_or_insert_with(String::new).push(c);
            } else if chars.peek() == Some(&'\'') {
                chars.next();
                quoted.get_or_insert_with(String::new).push('\'');
            } else {
                in_quote = false;
            }
            continue;
        }
        match c {
            ',' | '\n' => push_list_item(&mut items, &mut current, &mut quoted),
            '\'' if quoted.is_none() && matches!(current.trim(), "" | "N" | "n") => {
                current.clear();
                quoted = Some(String::new());
                in_quote = true;
            }
            _ => current.push(c),
        }
    }
    push_list_item(&mut items, &mut current, &mut quoted);
    items
}

fn push_list_item(items: &mut Vec<String>, current: &mut String, quoted: &mut Option<String>) {
    match quoted.take() {
        Some(text) => items.push(text),
        None => {
            let text = current.trim();
            if !text.is_empty() {
                items.push(text.to_string());
            }
        }
    }
    current.clear();
}

fn render_group(
    group: &FilterGroup,
    db: DatabaseType,
    columns: &[ResultColumn],
    kinds: &[ColumnKind],
    nested: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    for item in &group.items {
        match item {
            FilterItem::Row(row) => {
                if row.enabled
                    && let Some(sql) = row_to_sql(db, columns, kinds, row)
                {
                    parts.push(sql);
                }
            }
            FilterItem::Group(inner) => {
                if let Some(sql) = render_group(inner, db, columns, kinds, true) {
                    parts.push(sql);
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let sep = match group.conjunction {
        Conjunction::And => " AND ",
        Conjunction::Or => " OR ",
    };
    let joined = parts.join(sep);
    if nested && parts.len() >= 2 {
        Some(format!("({joined})"))
    } else {
        Some(joined)
    }
}

fn row_to_sql(
    db: DatabaseType,
    columns: &[ResultColumn],
    kinds: &[ColumnKind],
    row: &FilterRow,
) -> Option<String> {
    match row.column {
        ColumnTarget::Index(idx) => {
            let col = columns.get(idx)?;
            let kind = kinds.get(idx).copied().unwrap_or(ColumnKind::Text);
            leaf_to_sql(db, col, kind, row)
        }
        ColumnTarget::AnyText => {
            let mut parts = Vec::new();
            for (idx, col) in columns.iter().enumerate() {
                if kinds.get(idx).copied().unwrap_or(ColumnKind::Text) != ColumnKind::Text {
                    continue;
                }
                if let Some(sql) = leaf_to_sql(db, col, ColumnKind::Text, row) {
                    parts.push(sql);
                }
            }
            if parts.is_empty() {
                None
            } else if parts.len() == 1 {
                parts.pop()
            } else {
                Some(format!("({})", parts.join(" OR ")))
            }
        }
    }
}

/// Render one predicate for an already-quoted column expression. Shared by the
/// grid filter builder and the visual query builder.
pub fn predicate_sql(
    db: DatabaseType,
    col_expr: &str,
    op: BuilderOp,
    a: &str,
    b: &str,
) -> Option<String> {
    match op {
        BuilderOp::IsNull => Some(format!("{col_expr} IS NULL")),
        BuilderOp::IsNotNull => Some(format!("{col_expr} IS NOT NULL")),
        BuilderOp::Contains => Some(format!(
            "{col_expr} LIKE {}",
            db.string_literal(&format!("%{a}%"))
        )),
        BuilderOp::NotContains => Some(format!(
            "{col_expr} NOT LIKE {}",
            db.string_literal(&format!("%{a}%"))
        )),
        BuilderOp::BeginsWith => Some(format!(
            "{col_expr} LIKE {}",
            db.string_literal(&format!("{a}%"))
        )),
        BuilderOp::EndsWith => Some(format!(
            "{col_expr} LIKE {}",
            db.string_literal(&format!("%{a}"))
        )),
        BuilderOp::Is => Some(format!("{col_expr} = {}", db.string_literal(a))),
        BuilderOp::IsNot => Some(format!("{col_expr} <> {}", db.string_literal(a))),
        BuilderOp::IsEmpty => Some(format!("{col_expr} = {}", db.string_literal(""))),
        BuilderOp::IsNotEmpty => Some(format!("{col_expr} <> {}", db.string_literal(""))),
        BuilderOp::In | BuilderOp::NotIn => in_not_in_sql(db, col_expr, op, a, None),
        BuilderOp::Eq => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} = {a}"))
        }
        BuilderOp::Ne => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} <> {a}"))
        }
        BuilderOp::Gt => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} > {a}"))
        }
        BuilderOp::Ge => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} >= {a}"))
        }
        BuilderOp::Lt => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} < {a}"))
        }
        BuilderOp::Le => {
            let a = number_literal(a)?;
            Some(format!("{col_expr} <= {a}"))
        }
        BuilderOp::Between => {
            let a = number_literal(a)?;
            let b = number_literal(b)?;
            Some(format!("{col_expr} BETWEEN {a} AND {b}"))
        }
    }
}

fn in_not_in_sql(
    db: DatabaseType,
    col_expr: &str,
    op: BuilderOp,
    a: &str,
    numeric: Option<bool>,
) -> Option<String> {
    let items = split_list(a);
    if items.is_empty() {
        return None;
    }
    let rendered = match numeric {
        Some(true) => items
            .iter()
            .map(|item| number_literal(item))
            .collect::<Option<Vec<_>>>()?,
        Some(false) => items.iter().map(|item| db.string_literal(item)).collect(),
        None => match items
            .iter()
            .map(|item| number_literal(item))
            .collect::<Option<Vec<_>>>()
        {
            Some(nums) => nums,
            None => items.iter().map(|item| db.string_literal(item)).collect(),
        },
    };
    let kw = if op == BuilderOp::In { "IN" } else { "NOT IN" };
    Some(format!("{col_expr} {kw} ({})", rendered.join(", ")))
}

fn leaf_to_sql(
    db: DatabaseType,
    col: &ResultColumn,
    kind: ColumnKind,
    row: &FilterRow,
) -> Option<String> {
    let col_name = sql_identifier_for(db, &col.name);
    if let Some(sql) = binary_id_predicate_sql(db, col, &col_name, row) {
        return Some(sql);
    }
    match row.op {
        BuilderOp::In | BuilderOp::NotIn => {
            in_not_in_sql(db, &col_name, row.op, &row.a, Some(uses_number_ops(kind)))
        }
        op if kind == ColumnKind::Date && op.is_number() => date_predicate_sql(db, &col_name, row),
        op => predicate_sql(db, &col_name, op, &row.a, &row.b),
    }
}

/// The bytes literal for a UUID or ULID typed against a binary column (one
/// shown with a `binary=uuid` / `binary=ulid` format), so the query compares
/// the stored bytes rather than the text. `None` for any other column or
/// text.
fn binary_id_literal(db: DatabaseType, col: &ResultColumn, text: &str) -> Option<String> {
    if crate::where_actions::kind_for_type_name(Some(db), &col.data_type)
        != Some(crate::where_actions::WhereValueKind::Bytes)
    {
        return None;
    }
    crate::binary_ids::parse_binary_id(text).map(|bytes| db.bytes_literal(&bytes))
}

/// `=`, `<>`, `IN` and `NOT IN` on a binary column whose operands are all
/// UUID or ULID text, rendered against the bytes; `None` leaves the row to
/// the ordinary text rendering.
fn binary_id_predicate_sql(
    db: DatabaseType,
    col: &ResultColumn,
    col_expr: &str,
    row: &FilterRow,
) -> Option<String> {
    match row.op {
        BuilderOp::Is | BuilderOp::Eq => Some(format!(
            "{col_expr} = {}",
            binary_id_literal(db, col, &row.a)?
        )),
        BuilderOp::IsNot | BuilderOp::Ne => Some(format!(
            "{col_expr} <> {}",
            binary_id_literal(db, col, &row.a)?
        )),
        BuilderOp::In | BuilderOp::NotIn => {
            let items = split_list(&row.a);
            if items.is_empty() {
                return None;
            }
            let literals = items
                .iter()
                .map(|item| binary_id_literal(db, col, item))
                .collect::<Option<Vec<_>>>()?;
            let kw = if row.op == BuilderOp::In {
                "IN"
            } else {
                "NOT IN"
            };
            Some(format!("{col_expr} {kw} ({})", literals.join(", ")))
        }
        _ => None,
    }
}

/// Date comparisons render their operand as a dialect-correct calendar
/// literal (see [`calendar_literal`]), never as the Unix-seconds number the
/// grid compares on. Operands arrive either as `YYYY-MM-DD` (builder, filter
/// language) or as Unix-seconds text (a quick filter seeded from a date
/// cell).
fn date_predicate_sql(db: DatabaseType, col_expr: &str, row: &FilterRow) -> Option<String> {
    let a = date_literal_for_sql(db, &row.a)?;
    match row.op {
        BuilderOp::Eq => Some(format!("{col_expr} = {a}")),
        BuilderOp::Ne => Some(format!("{col_expr} <> {a}")),
        BuilderOp::Gt => Some(format!("{col_expr} > {a}")),
        BuilderOp::Ge => Some(format!("{col_expr} >= {a}")),
        BuilderOp::Lt => Some(format!("{col_expr} < {a}")),
        BuilderOp::Le => Some(format!("{col_expr} <= {a}")),
        BuilderOp::Between => {
            let b = date_literal_for_sql(db, &row.b)?;
            Some(format!("{col_expr} BETWEEN {a} AND {b}"))
        }
        _ => None,
    }
}

/// A calendar operand as a dialect-correct date/time literal. The operand is
/// a validated `YYYY-MM-DD` date or Unix-seconds text, so the rendered text
/// is fixed-shape digits and never needs escaping.
fn date_literal_for_sql(db: DatabaseType, operand: &str) -> Option<String> {
    let t = operand.trim();
    if parse_ymd_to_unix(t).is_some() {
        let date = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?;
        return Some(calendar_literal(db, date, None));
    }
    let secs = parse_number(t)? as i64;
    // 1970-01-01..2100-01-01 keeps stray numbers from becoming dates.
    if !(0..4_102_444_800).contains(&secs) {
        return None;
    }
    let dt = chrono::DateTime::from_timestamp(secs, 0)?.naive_utc();
    let time = (secs % 86_400 != 0).then(|| dt.time());
    Some(calendar_literal(db, dt.date(), time))
}

/// Render a date (with an optional time of day) in the literal form each
/// engine parses the same way regardless of session settings:
///
/// * SQL Server / Azure SQL: `'YYYYMMDD'` for dates and ISO 8601
///   `'YYYY-MM-DDTHH:MM:SS'` for datetimes. Both are independent of
///   `SET LANGUAGE`/`DATEFORMAT`, unlike `'YYYY-MM-DD'`, which `datetime`
///   reads as year-day-month under a DMY language.
/// * Access: `#YYYY-MM-DD#` / `#YYYY-MM-DD HH:MM:SS#`.
/// * Informix: `DATETIME(…) YEAR TO DAY|SECOND` (its string conversion
///   follows `DBDATE`).
/// * SQLite / libSQL / ClickHouse: the ISO text itself — SQLite stores dates
///   as ISO text and ClickHouse converts string operands against date
///   columns; neither accepts a typed `DATE '…'` literal.
/// * Everything else (Oracle, PostgreSQL, MySQL, Db2, Snowflake, …): the
///   ANSI typed literals `DATE '…'` / `TIMESTAMP '…'`. Oracle needs them —
///   a bare string converts through `NLS_DATE_FORMAT` (ORA-01861).
fn calendar_literal(
    db: DatabaseType,
    date: chrono::NaiveDate,
    time: Option<chrono::NaiveTime>,
) -> String {
    let ymd = date.format("%Y-%m-%d").to_string();
    let hms = time.map(|t| t.format("%H:%M:%S").to_string());
    match db {
        DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => match hms {
            None => format!("'{}'", date.format("%Y%m%d")),
            Some(hms) => format!("'{ymd}T{hms}'"),
        },
        DatabaseType::Access => match hms {
            None => format!("#{ymd}#"),
            Some(hms) => format!("#{ymd} {hms}#"),
        },
        DatabaseType::Informix => match hms {
            None => format!("DATETIME({ymd}) YEAR TO DAY"),
            Some(hms) => format!("DATETIME({ymd} {hms}) YEAR TO SECOND"),
        },
        DatabaseType::Sqlite | DatabaseType::LibSql | DatabaseType::ClickHouse => match hms {
            None => format!("'{ymd}'"),
            Some(hms) => format!("'{ymd} {hms}'"),
        },
        _ => match hms {
            None => format!("DATE '{ymd}'"),
            Some(hms) => format!("TIMESTAMP '{ymd} {hms}'"),
        },
    }
}

fn eval_group(
    group: &FilterGroup,
    row: &[CellValue],
    kinds: &[ColumnKind],
    fmts: &[ResolvedColumnFormat],
    null_texts: &[String],
) -> Option<bool> {
    let mut values = Vec::new();
    for item in &group.items {
        match item {
            FilterItem::Row(leaf) => {
                if leaf.enabled
                    && let Some(pass) = eval_row(leaf, row, kinds, fmts, null_texts)
                {
                    values.push(pass);
                }
            }
            FilterItem::Group(inner) => {
                if let Some(pass) = eval_group(inner, row, kinds, fmts, null_texts) {
                    values.push(pass);
                }
            }
        }
    }
    if values.is_empty() {
        return None;
    }
    Some(match group.conjunction {
        Conjunction::And => values.iter().all(|v| *v),
        Conjunction::Or => values.iter().any(|v| *v),
    })
}

fn eval_row(
    leaf: &FilterRow,
    row: &[CellValue],
    kinds: &[ColumnKind],
    fmts: &[ResolvedColumnFormat],
    null_texts: &[String],
) -> Option<bool> {
    match leaf.column {
        ColumnTarget::Index(idx) => {
            let cell = row.get(idx)?;
            let kind = kinds.get(idx).copied().unwrap_or(ColumnKind::Text);
            let fmt = fmts.get(idx)?;
            let null_text = null_texts.get(idx).map(|s| s.as_str()).unwrap_or("NULL");
            eval_leaf(leaf, cell, kind, fmt, null_text)
        }
        ColumnTarget::AnyText => {
            let mut any = false;
            for (idx, cell) in row.iter().enumerate() {
                if kinds.get(idx).copied() != Some(ColumnKind::Text) {
                    continue;
                }
                let Some(fmt) = fmts.get(idx) else {
                    continue;
                };
                let null_text = null_texts.get(idx).map(|s| s.as_str()).unwrap_or("NULL");
                if let Some(pass) = eval_leaf(leaf, cell, ColumnKind::Text, fmt, null_text) {
                    any = true;
                    if pass {
                        return Some(true);
                    }
                }
            }
            if any { Some(false) } else { None }
        }
    }
}

fn eval_leaf(
    leaf: &FilterRow,
    cell: &CellValue,
    kind: ColumnKind,
    fmt: &ResolvedColumnFormat,
    null_text: &str,
) -> Option<bool> {
    let is_null = matches!(cell, CellValue::None);
    match leaf.op {
        BuilderOp::IsNull => Some(is_null),
        BuilderOp::IsNotNull => Some(!is_null),
        _ if is_null => Some(false),
        BuilderOp::IsEmpty => {
            let (text, _) = format_cell(cell, fmt);
            Some(text.is_empty())
        }
        BuilderOp::IsNotEmpty => {
            let (text, _) = format_cell(cell, fmt);
            Some(!text.is_empty())
        }
        BuilderOp::In | BuilderOp::NotIn => {
            let items = split_list(&leaf.a);
            if items.is_empty() {
                return None;
            }
            let (text, _) = format_cell(cell, fmt);
            let member = if let (CellValue::Integer(v), Some(ints)) = (
                cell,
                items
                    .iter()
                    .map(|item| item.parse::<i128>().ok())
                    .collect::<Option<Vec<i128>>>(),
            ) {
                ints.contains(&i128::from(*v))
            } else if uses_number_ops(kind) {
                let parsed: Option<Vec<f64>> =
                    items.iter().map(|item| parse_number(item)).collect();
                let parsed = parsed?;
                parse_number(&text).is_some_and(|n| parsed.contains(&n))
            } else {
                let hay = text.to_lowercase();
                items.iter().any(|item| item.to_lowercase() == hay)
            };
            Some(if leaf.op == BuilderOp::In {
                member
            } else {
                !member
            })
        }
        _ => {
            if kind == ColumnKind::Date && leaf.op.is_number() {
                return eval_date_number_leaf(leaf, cell, fmt);
            }
            if let Some(pass) = eval_exact_integer_leaf(leaf, cell) {
                return Some(pass);
            }
            let predicate = predicate_from_row(leaf, kind, null_text)?;
            let filter = ColumnFilter {
                predicate,
                values: None,
            };
            Some(cell_passes_filter(cell, fmt, &filter))
        }
    }
}

/// Integer cells against integer operands compare exactly (as `i128`), so
/// `id = 1234567890123456789` does not also match its `f64` neighbours.
/// `None` when either side is not an integer; the caller falls back to the
/// datatable's `f64` predicate.
fn eval_exact_integer_leaf(leaf: &FilterRow, cell: &CellValue) -> Option<bool> {
    use std::cmp::Ordering;
    let CellValue::Integer(v) = cell else {
        return None;
    };
    if !leaf.op.is_number() {
        return None;
    }
    let v = i128::from(*v);
    let a = leaf.a.trim().parse::<i128>().ok()?;
    let ord = v.cmp(&a);
    Some(match leaf.op {
        BuilderOp::Eq => ord == Ordering::Equal,
        BuilderOp::Ne => ord != Ordering::Equal,
        BuilderOp::Gt => ord == Ordering::Greater,
        BuilderOp::Ge => ord != Ordering::Less,
        BuilderOp::Lt => ord == Ordering::Less,
        BuilderOp::Le => ord != Ordering::Greater,
        BuilderOp::Between => {
            let b = leaf.b.trim().parse::<i128>().ok()?;
            let (lo, hi) = if a > b { (b, a) } else { (a, b) };
            (lo..=hi).contains(&v)
        }
        _ => return None,
    })
}

/// Date-kind number comparisons in the tree path. The app carries dates as
/// formatted text cells, so the datatable's `Number` predicate (which only
/// reads `CellValue::Date`) would match nothing; compare calendar values
/// here instead. A cell whose text is not ISO-like date text fails the
/// predicate rather than silently passing.
fn eval_date_number_leaf(
    leaf: &FilterRow,
    cell: &CellValue,
    fmt: &ResolvedColumnFormat,
) -> Option<bool> {
    use std::cmp::Ordering;
    let a = parse_operand_number(&leaf.a, ColumnKind::Date)?;
    let b = if leaf.op == BuilderOp::Between {
        parse_operand_number(&leaf.b, ColumnKind::Date)?
    } else {
        0.0
    };
    // An operand that parses but a cell that does not (NULL, free text)
    // fails the comparison, like SQL's NULL semantics would.
    let Some(v) = cell_date_unix(cell, fmt) else {
        return Some(false);
    };
    let ord = v.total_cmp(&a);
    Some(match leaf.op {
        BuilderOp::Eq => ord == Ordering::Equal,
        BuilderOp::Ne => ord != Ordering::Equal,
        BuilderOp::Gt => ord == Ordering::Greater,
        BuilderOp::Ge => ord != Ordering::Less,
        BuilderOp::Lt => ord == Ordering::Less,
        BuilderOp::Le => ord != Ordering::Greater,
        BuilderOp::Between => {
            let (lo, hi) = if a.total_cmp(&b) == Ordering::Greater {
                (b, a)
            } else {
                (a, b)
            };
            v.total_cmp(&lo) != Ordering::Less && v.total_cmp(&hi) != Ordering::Greater
        }
        _ => return None,
    })
}

/// Unix seconds for a date cell: native `Date` cells carry them; the app's
/// usual text cells parse as ISO `YYYY-MM-DD` with an optional time.
fn cell_date_unix(cell: &CellValue, fmt: &ResolvedColumnFormat) -> Option<f64> {
    match cell {
        CellValue::Date(t) => Some(*t as f64),
        CellValue::Text(_) => {
            let (text, _) = format_cell(cell, fmt);
            parse_iso_datetime_unix(&text).map(|t| t as f64)
        }
        _ => None,
    }
}

fn parse_iso_datetime_unix(text: &str) -> Option<i64> {
    let t = text.trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(t, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    let date = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

/// Render active [`ColumnFilter`]s as an AND-joined WHERE fragment. This is
/// the single renderer used by both "Add Active Filters to WHERE" and the
/// filter builder's Apply to Query path.
pub fn column_filters_to_where(
    db: DatabaseType,
    columns: &[ResultColumn],
    filters: &[ColumnFilter],
    sql: Option<&str>,
) -> Option<String> {
    // Parse the statement once; every filtered column resolves against the
    // same parse.
    let provenance =
        sql.and_then(sqlly_intel::query::column_provenance::StatementProvenance::parse);
    let predicates: Vec<String> = filters
        .iter()
        .enumerate()
        .filter(|(_, f)| f.is_active())
        .filter_map(|(col_idx, filter)| {
            let col = columns.get(col_idx)?;
            // Same provenance rule as the context-menu WHERE actions: write
            // the predicate against the resolved source column when the
            // originating statement attributes the result column to one.
            let occurrences = columns
                .iter()
                .filter(|c| c.name.eq_ignore_ascii_case(&col.name))
                .count();
            let source = provenance
                .as_ref()
                .and_then(|p| p.resolve(&col.name, col_idx, occurrences));
            let col_name =
                crate::where_actions::qualified_identifier_for(db, source.as_ref(), &col.name);
            let mut parts: Vec<String> = Vec::new();
            match &filter.predicate {
                FilterPredicate::Text { op, operand } => {
                    let lit = |s: String| db.string_literal(&s);
                    let clause = match op {
                        TextOp::Contains => {
                            format!("{col_name} LIKE {}", lit(format!("%{operand}%")))
                        }
                        TextOp::DoesNotContain => {
                            format!("{col_name} NOT LIKE {}", lit(format!("%{operand}%")))
                        }
                        TextOp::BeginsWith => {
                            format!("{col_name} LIKE {}", lit(format!("{operand}%")))
                        }
                        TextOp::EndsWith => {
                            format!("{col_name} LIKE {}", lit(format!("%{operand}")))
                        }
                        TextOp::Is => format!(
                            "{col_name} = {}",
                            binary_id_literal(db, col, operand)
                                .unwrap_or_else(|| lit(operand.clone()))
                        ),
                        TextOp::IsNot => format!(
                            "{col_name} <> {}",
                            binary_id_literal(db, col, operand)
                                .unwrap_or_else(|| lit(operand.clone()))
                        ),
                        TextOp::Matches => format!("{col_name} LIKE {}", lit(operand.clone())),
                    };
                    parts.push(clause);
                }
                FilterPredicate::Number { op, a, b } => {
                    let clause = match op {
                        NumberOp::Eq => format!("{col_name} = {a}"),
                        NumberOp::Ne => format!("{col_name} <> {a}"),
                        NumberOp::Gt => format!("{col_name} > {a}"),
                        NumberOp::Ge => format!("{col_name} >= {a}"),
                        NumberOp::Lt => format!("{col_name} < {a}"),
                        NumberOp::Le => format!("{col_name} <= {a}"),
                        NumberOp::Between => {
                            format!("{col_name} BETWEEN {a} AND {b}")
                        }
                        NumberOp::NotBetween => {
                            format!("{col_name} NOT BETWEEN {a} AND {b}")
                        }
                    };
                    parts.push(clause);
                }
                FilterPredicate::None => {}
            }
            if let Some(allowed) = &filter.values {
                let vals: Vec<String> = allowed.iter().map(|v| db.string_literal(v)).collect();
                if !vals.is_empty() {
                    parts.push(format!("{} IN ({})", col_name, vals.join(", ")));
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" AND "))
            }
        })
        .collect();
    if predicates.is_empty() {
        None
    } else {
        Some(predicates.join(" AND "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_datatable::GridConfig;

    fn col(name: &str, kind: ColumnKind) -> Column {
        Column::new(name, kind, 80.0)
    }

    fn result_col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    fn row(column: usize, op: BuilderOp, a: &str, b: &str) -> FilterRow {
        FilterRow {
            column: ColumnTarget::Index(column),
            op,
            a: a.into(),
            b: b.into(),
            enabled: true,
        }
    }

    fn and_model(rows: Vec<FilterRow>) -> FilterBuilderModel {
        FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items: rows.into_iter().map(FilterItem::Row).collect(),
            },
        }
    }

    fn kinds_text(n: usize) -> Vec<ColumnKind> {
        vec![ColumnKind::Text; n]
    }

    #[test]
    fn ops_for_text_excludes_numeric_ops() {
        let ops = FilterBuilderModel::ops_for(ColumnKind::Text);
        assert!(ops.contains(&BuilderOp::Contains));
        assert!(ops.contains(&BuilderOp::IsNull));
        assert!(ops.contains(&BuilderOp::In));
        assert!(ops.contains(&BuilderOp::IsEmpty));
        for numeric in [
            BuilderOp::Eq,
            BuilderOp::Ne,
            BuilderOp::Gt,
            BuilderOp::Ge,
            BuilderOp::Lt,
            BuilderOp::Le,
            BuilderOp::Between,
        ] {
            assert!(!ops.contains(&numeric), "{numeric:?} must not be a text op");
        }
    }

    #[test]
    fn ops_for_integer_excludes_text_ops() {
        let ops = FilterBuilderModel::ops_for(ColumnKind::Integer);
        assert!(ops.contains(&BuilderOp::Eq));
        assert!(ops.contains(&BuilderOp::Between));
        assert!(ops.contains(&BuilderOp::IsNull));
        assert!(ops.contains(&BuilderOp::In));
        for text in [
            BuilderOp::Contains,
            BuilderOp::NotContains,
            BuilderOp::BeginsWith,
            BuilderOp::EndsWith,
            BuilderOp::Is,
            BuilderOp::IsNot,
            BuilderOp::IsEmpty,
            BuilderOp::IsNotEmpty,
        ] {
            assert!(!ops.contains(&text), "{text:?} must not be a number op");
        }
    }

    #[test]
    fn to_column_filters_maps_contains_and_between() {
        let columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
        ];
        let nulls = vec!["NULL".into(), "NULL".into()];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            row(1, BuilderOp::Between, "1", "5"),
        ]);
        let filters = model.to_column_filters(&columns, &nulls).expect("flat and");
        assert_eq!(
            filters[0].predicate,
            FilterPredicate::Text {
                op: TextOp::Contains,
                operand: "ab".into(),
            }
        );
        assert_eq!(
            filters[1].predicate,
            FilterPredicate::Number {
                op: NumberOp::Between,
                a: 1.0,
                b: 5.0,
            }
        );
    }

    #[test]
    fn to_column_filters_ignores_non_numeric_operand() {
        let columns = vec![col("qty", ColumnKind::Integer)];
        let nulls = vec!["NULL".into()];
        let model = and_model(vec![row(0, BuilderOp::Gt, "nope", "")]);
        let filters = model.to_column_filters(&columns, &nulls).expect("flat and");
        assert_eq!(filters[0].predicate, FilterPredicate::None);
    }

    #[test]
    fn is_null_maps_to_text_is_with_null_placeholder() {
        let columns = vec![col("name", ColumnKind::Text)];
        let nulls = vec!["(null)".into()];
        let model = and_model(vec![row(0, BuilderOp::IsNull, "", "")]);
        let filters = model.to_column_filters(&columns, &nulls).expect("flat and");
        assert_eq!(
            filters[0].predicate,
            FilterPredicate::Text {
                op: TextOp::Is,
                operand: "(null)".into(),
            }
        );
    }

    #[test]
    fn predicate_evaluation_keeps_matching_rows() {
        let columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
            col("flag", ColumnKind::Text),
        ];
        let rows = [
            vec![
                CellValue::Text("alpha".into()),
                CellValue::Integer(1),
                CellValue::Text("x".into()),
            ],
            vec![
                CellValue::Text("cab".into()),
                CellValue::Integer(3),
                CellValue::Text("y".into()),
            ],
            vec![
                CellValue::Text("ab".into()),
                CellValue::Integer(10),
                CellValue::Text("z".into()),
            ],
            vec![
                CellValue::Text("no".into()),
                CellValue::Integer(4),
                CellValue::Text("w".into()),
            ],
            vec![
                CellValue::Text("nab".into()),
                CellValue::Integer(5),
                CellValue::Text("v".into()),
            ],
        ];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            row(1, BuilderOp::Between, "1", "5"),
        ]);
        let nulls = vec!["NULL".into(), "NULL".into(), "NULL".into()];
        let filters = model.to_column_filters(&columns, &nulls).expect("flat and");
        let formats = GridConfig::default().resolve_all(&columns);
        let surviving: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.iter()
                    .enumerate()
                    .all(|(ci, cell)| cell_passes_filter(cell, &formats[ci], &filters[ci]))
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(surviving, vec![1, 4]);
    }

    /// Sequel Ace #2148: a UUID or ULID typed against a binary(16) column
    /// filters by the bytes it stands for, in each engine's bytes literal;
    /// other text keeps the text rendering.
    #[test]
    fn binary_id_text_filters_binary_columns_by_bytes() {
        let columns = vec![
            result_col("id", "binary(16)"),
            result_col("label", "varchar"),
        ];
        let kinds = vec![ColumnKind::Text, ColumnKind::Text];
        let uuid = "017f22e2-79b0-7cc3-98c4-dc0c0c07398f";
        let ulid = "01FWHE4YDGFK1SHH6W1G60EECF";
        let hex = "017F22E279B07CC398C4DC0C0C07398F";
        let model = and_model(vec![row(0, BuilderOp::Is, uuid, "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::MySql, &columns, &kinds)
                .unwrap(),
            format!("`id` = 0x{hex}")
        );
        let model = and_model(vec![row(0, BuilderOp::IsNot, ulid, "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .unwrap(),
            format!("[id] <> 0x{hex}")
        );
        let model = and_model(vec![row(0, BuilderOp::In, &format!("{uuid}, {ulid}"), "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
                .unwrap(),
            format!("\"id\" IN ('\\x{hex}', '\\x{hex}')")
        );
        // Not an id: the text rendering stays.
        let model = and_model(vec![row(0, BuilderOp::Is, "abc", "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::MySql, &columns, &kinds)
                .unwrap(),
            "`id` = 'abc'"
        );
        // A text column never turns an id-shaped value into bytes.
        let model = and_model(vec![row(1, BuilderOp::Is, uuid, "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::MySql, &columns, &kinds)
                .unwrap(),
            format!("`label` = '{uuid}'")
        );
    }

    #[test]
    fn to_where_clause_sql_server_and_postgres() {
        let columns = vec![result_col("name", "nvarchar"), result_col("qty", "int")];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            row(1, BuilderOp::Between, "1", "5"),
        ]);
        let sql = model
            .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
            .expect("clause");
        assert_eq!(sql, "[name] LIKE N'%ab%' AND [qty] BETWEEN 1 AND 5");
        let pg = model
            .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
            .expect("clause");
        assert_eq!(pg, "\"name\" LIKE '%ab%' AND \"qty\" BETWEEN 1 AND 5");
    }

    #[test]
    fn to_where_clause_doubles_quotes_and_empty_is_none() {
        let columns = vec![result_col("name", "nvarchar")];
        let kinds = kinds_text(1);
        let model = and_model(vec![row(0, BuilderOp::Contains, "a'b", "")]);
        let sql = model
            .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
            .unwrap();
        assert_eq!(sql, "[name] LIKE N'%a''b%'");
        let empty = FilterBuilderModel::default();
        assert!(
            empty
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .is_none()
        );
    }

    #[test]
    fn to_where_clause_is_null() {
        let columns = vec![result_col("name", "nvarchar")];
        let kinds = kinds_text(1);
        let model = and_model(vec![row(0, BuilderOp::IsNull, "", "")]);
        assert_eq!(
            model.to_where_clause(DatabaseType::SqlServer, &columns, &kinds),
            Some("[name] IS NULL".into())
        );
    }

    #[test]
    fn column_filters_to_where_round_trips_contains_and_between() {
        let grid_columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
        ];
        let result_columns = vec![result_col("name", "nvarchar"), result_col("qty", "int")];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            row(1, BuilderOp::Between, "1", "5"),
        ]);
        let nulls = vec!["NULL".into(), "NULL".into()];
        let filters = model
            .to_column_filters(&grid_columns, &nulls)
            .expect("flat and");
        let via_filters =
            column_filters_to_where(DatabaseType::SqlServer, &result_columns, &filters, None);
        let via_model = model.to_where_clause(DatabaseType::SqlServer, &result_columns, &kinds);
        assert_eq!(via_filters, via_model);
        assert_eq!(
            via_model.as_deref(),
            Some("[name] LIKE N'%ab%' AND [qty] BETWEEN 1 AND 5")
        );
    }

    fn nested_or_model() -> FilterBuilderModel {
        FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items: vec![
                    FilterItem::Row(row(0, BuilderOp::Is, "x", "")),
                    FilterItem::Group(FilterGroup {
                        conjunction: Conjunction::Or,
                        items: vec![
                            FilterItem::Row(row(1, BuilderOp::Contains, "y", "")),
                            FilterItem::Row(row(2, BuilderOp::IsNull, "", "")),
                        ],
                    }),
                ],
            },
        }
    }

    fn abc_columns() -> (Vec<ResultColumn>, Vec<ColumnKind>) {
        (
            vec![
                result_col("a", "nvarchar"),
                result_col("b", "nvarchar"),
                result_col("c", "nvarchar"),
            ],
            kinds_text(3),
        )
    }

    #[test]
    fn nested_or_group_is_parenthesised() {
        let model = nested_or_model();
        let (columns, kinds) = abc_columns();
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[a] = N'x' AND ([b] LIKE N'%y%' OR [c] IS NULL)")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
                .as_deref(),
            Some("\"a\" = 'x' AND (\"b\" LIKE '%y%' OR \"c\" IS NULL)")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::MySql, &columns, &kinds)
                .as_deref(),
            Some("`a` = 'x' AND (`b` LIKE '%y%' OR `c` IS NULL)")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::Sqlite, &columns, &kinds)
                .as_deref(),
            Some("\"a\" = 'x' AND (\"b\" LIKE '%y%' OR \"c\" IS NULL)")
        );
    }

    #[test]
    fn single_child_group_is_bare() {
        let model = FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items: vec![
                    FilterItem::Row(row(0, BuilderOp::Is, "x", "")),
                    FilterItem::Group(FilterGroup {
                        conjunction: Conjunction::Or,
                        items: vec![FilterItem::Row(row(1, BuilderOp::Contains, "y", ""))],
                    }),
                ],
            },
        };
        let (columns, kinds) = abc_columns();
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[a] = N'x' AND [b] LIKE N'%y%'")
        );
    }

    #[test]
    fn empty_group_is_dropped() {
        let model = FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::And,
                items: vec![
                    FilterItem::Row(row(0, BuilderOp::Is, "x", "")),
                    FilterItem::Group(FilterGroup {
                        conjunction: Conjunction::Or,
                        items: vec![],
                    }),
                ],
            },
        };
        let (columns, kinds) = abc_columns();
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[a] = N'x'")
        );
    }

    #[test]
    fn in_list_quotes_per_dialect() {
        let columns = vec![result_col("a", "nvarchar")];
        let kinds = kinds_text(1);
        let model = and_model(vec![row(0, BuilderOp::In, "it's, x", "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[a] IN (N'it''s', N'x')")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
                .as_deref(),
            Some("\"a\" IN ('it''s', 'x')")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::MySql, &columns, &kinds)
                .as_deref(),
            Some("`a` IN ('it''s', 'x')")
        );
        assert_eq!(
            model
                .to_where_clause(DatabaseType::Sqlite, &columns, &kinds)
                .as_deref(),
            Some("\"a\" IN ('it''s', 'x')")
        );
    }

    #[test]
    fn in_list_accepts_pasted_copy_for_in_output() {
        // "Copy for IN" writes a whole predicate; pasted into the IN box it
        // must filter on every value, the first one included.
        let columns = vec![result_col("id", "int")];
        let kinds = vec![ColumnKind::Integer];
        for pasted in [
            "[Id] IN (1, 2, 3)",
            "(1, 2, 3)",
            "(\n1,\n2,\n3\n)",
            "id in (1,2,3)",
        ] {
            let model = and_model(vec![row(0, BuilderOp::In, pasted, "")]);
            assert_eq!(
                model
                    .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                    .as_deref(),
                Some("[id] IN (1, 2, 3)"),
                "pasted {pasted:?}"
            );
        }
        let columns = vec![result_col("name", "nvarchar")];
        let kinds = kinds_text(1);
        for pasted in [
            "[name] IN (N'it''s', N'a, b', N'x')",
            "\"name\" IN ('it''s', 'a, b', 'x')",
            "('it''s', 'a, b', 'x')",
        ] {
            let model = and_model(vec![row(0, BuilderOp::In, pasted, "")]);
            assert_eq!(
                model
                    .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                    .as_deref(),
                Some("[name] IN (N'it''s', N'a, b', N'x')"),
                "pasted {pasted:?}"
            );
        }
        // A plain list keeps its apostrophes and parentheses as typed.
        let model = and_model(vec![row(0, BuilderOp::In, "O'Brien, (none)", "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[name] IN (N'O''Brien', N'(none)')")
        );
    }

    #[test]
    fn in_list_numeric_rejects_text() {
        let columns = vec![result_col("qty", "int")];
        let kinds = vec![ColumnKind::Integer];
        let model = and_model(vec![row(0, BuilderOp::In, "1, nope", "")]);
        assert!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .is_none()
        );
        let ok = and_model(vec![row(0, BuilderOp::In, "1, 3", "")]);
        assert_eq!(
            ok.to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[qty] IN (1, 3)")
        );
    }

    #[test]
    fn any_text_expands_over_text_columns_only() {
        let columns = vec![
            result_col("name", "nvarchar"),
            result_col("qty", "int"),
            result_col("flag", "nvarchar"),
        ];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer, ColumnKind::Text];
        let mut leaf = FilterRow::any_text(BuilderOp::Contains);
        leaf.a = "ab".into();
        let model = and_model(vec![leaf]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("([name] LIKE N'%ab%' OR [flag] LIKE N'%ab%')")
        );
    }

    #[test]
    fn is_empty_renders_equals_blank() {
        let columns = vec![result_col("name", "nvarchar")];
        let kinds = kinds_text(1);
        let model = and_model(vec![row(0, BuilderOp::IsEmpty, "", "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[name] = N''")
        );
        let not_empty = and_model(vec![row(0, BuilderOp::IsNotEmpty, "", "")]);
        assert_eq!(
            not_empty
                .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
                .as_deref(),
            Some("\"name\" <> ''")
        );
    }

    #[test]
    fn flat_and_model_still_maps_to_column_filters() {
        let columns = vec![col("name", ColumnKind::Text)];
        let nulls = vec!["NULL".into()];
        let model = and_model(vec![row(0, BuilderOp::Contains, "ab", "")]);
        assert!(model.is_flat_and());
        assert!(model.to_column_filters(&columns, &nulls).is_some());
    }

    #[test]
    fn tree_model_has_no_column_filters() {
        let columns = vec![col("a", ColumnKind::Text), col("b", ColumnKind::Text)];
        let nulls = vec!["NULL".into(), "NULL".into()];
        let model = nested_or_model();
        assert!(!model.is_flat_and());
        assert!(model.to_column_filters(&columns, &nulls).is_none());
    }

    #[test]
    fn row_passes_matches_sql_semantics_for_or_group() {
        let grid_columns = vec![
            col("a", ColumnKind::Text),
            col("b", ColumnKind::Text),
            col("c", ColumnKind::Text),
        ];
        let kinds = kinds_text(3);
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".into(), "NULL".into(), "NULL".into()];
        let model = nested_or_model();
        let rows = [
            (
                vec![
                    CellValue::Text("x".into()),
                    CellValue::Text("yy".into()),
                    CellValue::Text("z".into()),
                ],
                true,
            ),
            (
                vec![
                    CellValue::Text("x".into()),
                    CellValue::Text("no".into()),
                    CellValue::None,
                ],
                true,
            ),
            (
                vec![
                    CellValue::Text("x".into()),
                    CellValue::Text("no".into()),
                    CellValue::Text("z".into()),
                ],
                false,
            ),
            (
                vec![
                    CellValue::Text("other".into()),
                    CellValue::Text("yy".into()),
                    CellValue::None,
                ],
                false,
            ),
        ];
        for (row, expected) in rows {
            assert_eq!(
                model.row_passes(&row, &kinds, &fmts, &nulls),
                expected,
                "row {row:?}"
            );
        }
    }

    #[test]
    fn row_passes_any_text() {
        let grid_columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
            col("flag", ColumnKind::Text),
        ];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer, ColumnKind::Text];
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".into(), "NULL".into(), "NULL".into()];
        let mut leaf = FilterRow::any_text(BuilderOp::Contains);
        leaf.a = "ab".into();
        let model = and_model(vec![leaf]);
        assert!(model.row_passes(
            &[
                CellValue::Text("xx".into()),
                CellValue::Integer(1),
                CellValue::Text("cab".into()),
            ],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!model.row_passes(
            &[
                CellValue::Text("xx".into()),
                CellValue::Integer(1),
                CellValue::Text("no".into()),
            ],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!model.row_passes(
            &[
                CellValue::Text("xx".into()),
                CellValue::Integer(0),
                CellValue::Text("no".into()),
            ],
            &kinds,
            &fmts,
            &nulls
        ));
    }

    #[test]
    fn row_passes_in_and_empty() {
        let grid_columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
        ];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer];
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".into(), "NULL".into()];
        let in_model = and_model(vec![row(0, BuilderOp::In, "alpha, beta", "")]);
        assert!(in_model.row_passes(
            &[CellValue::Text("alpha".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!in_model.row_passes(
            &[CellValue::Text("gamma".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!in_model.row_passes(
            &[CellValue::None, CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        let empty_model = and_model(vec![row(0, BuilderOp::IsEmpty, "", "")]);
        assert!(empty_model.row_passes(
            &[CellValue::Text("".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!empty_model.row_passes(
            &[CellValue::None, CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        let not_empty = and_model(vec![row(0, BuilderOp::IsNotEmpty, "", "")]);
        assert!(not_empty.row_passes(
            &[CellValue::Text("x".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!not_empty.row_passes(
            &[CellValue::None, CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        let num_in = and_model(vec![row(1, BuilderOp::In, "1, 3", "")]);
        assert!(num_in.row_passes(
            &[CellValue::Text("x".into()), CellValue::Integer(3)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!num_in.row_passes(
            &[CellValue::Text("x".into()), CellValue::Integer(2)],
            &kinds,
            &fmts,
            &nulls
        ));
    }

    fn disabled_row(column: usize, op: BuilderOp, a: &str, b: &str) -> FilterRow {
        FilterRow {
            enabled: false,
            ..row(column, op, a, b)
        }
    }

    #[test]
    fn disabled_rows_are_skipped_by_column_filters() {
        let columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
        ];
        let nulls = vec!["NULL".into(), "NULL".into()];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            disabled_row(1, BuilderOp::Between, "1", "5"),
        ]);
        assert!(model.is_flat_and(), "disabled rows keep the fast path");
        let filters = model.to_column_filters(&columns, &nulls).expect("flat and");
        assert_eq!(
            filters[0].predicate,
            FilterPredicate::Text {
                op: TextOp::Contains,
                operand: "ab".into(),
            }
        );
        assert_eq!(filters[1].predicate, FilterPredicate::None);
    }

    #[test]
    fn disabled_non_legacy_row_keeps_flat_and() {
        let columns = vec![col("name", ColumnKind::Text)];
        let nulls = vec!["NULL".into()];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            disabled_row(0, BuilderOp::In, "x, y", ""),
        ]);
        assert!(model.is_flat_and());
        assert!(model.to_column_filters(&columns, &nulls).is_some());
    }

    #[test]
    fn disabled_rows_are_skipped_by_where_clause() {
        let columns = vec![result_col("name", "nvarchar"), result_col("qty", "int")];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer];
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            disabled_row(1, BuilderOp::Between, "1", "5"),
        ]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[name] LIKE N'%ab%'")
        );
        let all_disabled = and_model(vec![disabled_row(0, BuilderOp::Contains, "ab", "")]);
        assert!(
            all_disabled
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .is_none()
        );
    }

    #[test]
    fn disabled_rows_are_skipped_by_row_passes() {
        let grid_columns = vec![
            col("name", ColumnKind::Text),
            col("qty", ColumnKind::Integer),
        ];
        let kinds = vec![ColumnKind::Text, ColumnKind::Integer];
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".into(), "NULL".into()];
        let model = and_model(vec![disabled_row(0, BuilderOp::Contains, "zz", "")]);
        assert!(model.row_passes(
            &[CellValue::Text("ab".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        let mixed = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            disabled_row(1, BuilderOp::Eq, "99", ""),
        ]);
        assert!(mixed.row_passes(
            &[CellValue::Text("ab".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!mixed.row_passes(
            &[CellValue::Text("no".into()), CellValue::Integer(1)],
            &kinds,
            &fmts,
            &nulls
        ));
    }

    #[test]
    fn date_number_rows_route_to_the_tree_path() {
        // The datatable's Number predicate only reads CellValue::Date, but
        // the app carries dates as text — so flat date comparisons must not
        // become ColumnFilters.
        let columns = vec![col("created", ColumnKind::Date)];
        let nulls = vec!["NULL".into()];
        let model = and_model(vec![row(0, BuilderOp::Ge, "2026-09-01", "")]);
        assert!(model.to_column_filters(&columns, &nulls).is_none());
        let mixed = and_model(vec![
            row(0, BuilderOp::Ge, "2026-09-01", ""),
            row(0, BuilderOp::IsNull, "", ""),
        ]);
        assert!(mixed.to_column_filters(&columns, &nulls).is_none());
        // Disabled date rows do not force the tree path.
        let disabled = and_model(vec![disabled_row(0, BuilderOp::Ge, "2026-09-01", "")]);
        assert!(disabled.to_column_filters(&columns, &nulls).is_some());
    }

    #[test]
    fn row_passes_compares_text_date_cells_by_calendar_value() {
        let grid_columns = vec![col("created", ColumnKind::Date)];
        let kinds = vec![ColumnKind::Date];
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".into()];
        let model = and_model(vec![
            row(0, BuilderOp::Ge, "2026-09-01", ""),
            row(0, BuilderOp::Lt, "2026-10-01", ""),
        ]);
        let passes =
            |text: &str| model.row_passes(&[CellValue::Text(text.into())], &kinds, &fmts, &nulls);
        assert!(passes("2026-09-01"));
        assert!(passes("2026-09-15"));
        assert!(passes("2026-09-30 14:30:00"));
        assert!(passes("2026-09-30T23:59:59"));
        assert!(!passes("2026-08-31"));
        assert!(!passes("2026-10-01"));
        assert!(!passes("not a date"));
        assert!(!model.row_passes(&[CellValue::None], &kinds, &fmts, &nulls));
        let between = and_model(vec![row(0, BuilderOp::Between, "2026-09-01", "2026-09-30")]);
        assert!(between.row_passes(
            &[CellValue::Text("2026-09-15".into())],
            &kinds,
            &fmts,
            &nulls
        ));
        let eq = and_model(vec![row(0, BuilderOp::Eq, "2026-09-15", "")]);
        assert!(eq.row_passes(
            &[CellValue::Text("2026-09-15".into())],
            &kinds,
            &fmts,
            &nulls
        ));
        assert!(!eq.row_passes(
            &[CellValue::Text("2026-09-15 10:00:00".into())],
            &kinds,
            &fmts,
            &nulls
        ));
        // Native Date cells (unix seconds) compare directly.
        let unix = parse_ymd_to_unix("2026-09-15").unwrap();
        assert!(eq.row_passes(&[CellValue::Date(unix)], &kinds, &fmts, &nulls));
    }

    #[test]
    fn date_where_clause_renders_calendar_literals() {
        let columns = vec![result_col("created", "datetime")];
        let kinds = vec![ColumnKind::Date];
        let model = and_model(vec![
            row(0, BuilderOp::Ge, "2026-09-01", ""),
            row(0, BuilderOp::Lt, "2026-10-01", ""),
        ]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[created] >= '20260901' AND [created] < '20261001'")
        );
        // Unix-seconds operands (quick filter from a date cell) render as
        // calendar text too.
        let unix = parse_ymd_to_unix("2026-09-15").unwrap().to_string();
        let quick = and_model(vec![row(0, BuilderOp::Eq, &unix, "")]);
        assert_eq!(
            quick
                .to_where_clause(DatabaseType::Postgres, &columns, &kinds)
                .as_deref(),
            Some("\"created\" = DATE '2026-09-15'")
        );
    }

    #[test]
    fn date_literals_are_dialect_correct() {
        let columns = vec![result_col("created", "datetime")];
        let kinds = vec![ColumnKind::Date];
        let day = and_model(vec![row(0, BuilderOp::Ge, "2026-09-01", "")]);
        let render = |db: DatabaseType, model: &FilterBuilderModel| {
            model.to_where_clause(db, &columns, &kinds).unwrap()
        };
        // SQL Server: YYYYMMDD, which datetime reads the same under every
        // SET LANGUAGE / DATEFORMAT (YYYY-MM-DD flips to YDM under DMY).
        assert_eq!(
            render(DatabaseType::SqlServer, &day),
            "[created] >= '20260901'"
        );
        assert_eq!(
            render(DatabaseType::AzureSqlDatabase, &day),
            "[created] >= '20260901'"
        );
        // Oracle needs a typed literal; a bare string goes through NLS.
        assert_eq!(
            render(DatabaseType::Oracle, &day),
            "\"created\" >= DATE '2026-09-01'"
        );
        assert_eq!(
            render(DatabaseType::MySql, &day),
            "`created` >= DATE '2026-09-01'"
        );
        assert_eq!(
            render(DatabaseType::Sqlite, &day),
            "\"created\" >= '2026-09-01'"
        );
        assert!(render(DatabaseType::Access, &day).ends_with(">= #2026-09-01#"));
        assert!(
            render(DatabaseType::Informix, &day).ends_with(">= DATETIME(2026-09-01) YEAR TO DAY")
        );
        // A time-of-day operand keeps its time in every form.
        let secs = parse_ymd_to_unix("2026-09-15").unwrap() + 10 * 3600 + 30 * 60;
        let timed = and_model(vec![row(0, BuilderOp::Lt, &secs.to_string(), "")]);
        assert_eq!(
            render(DatabaseType::SqlServer, &timed),
            "[created] < '2026-09-15T10:30:00'"
        );
        assert_eq!(
            render(DatabaseType::Oracle, &timed),
            "\"created\" < TIMESTAMP '2026-09-15 10:30:00'"
        );
    }

    #[test]
    fn large_integer_operands_stay_exact_in_sql_and_in_the_grid() {
        let big = "1234567890123456789";
        let columns = vec![result_col("id", "bigint")];
        let kinds = vec![ColumnKind::Integer];
        let model = and_model(vec![row(0, BuilderOp::Eq, big, "")]);
        assert_eq!(
            model
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[id] = 1234567890123456789"),
            "no f64 round trip in the SQL"
        );
        let in_list = and_model(vec![row(0, BuilderOp::In, "1234567890123456789, 7", "")]);
        assert_eq!(
            in_list
                .to_where_clause(DatabaseType::SqlServer, &columns, &kinds)
                .as_deref(),
            Some("[id] IN (1234567890123456789, 7)")
        );
        // The f64 fast path cannot tell neighbours apart: route to the tree.
        let grid_columns = vec![col("id", ColumnKind::Integer)];
        let nulls = vec!["NULL".to_string()];
        assert!(model.to_column_filters(&grid_columns, &nulls).is_none());
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let exact = CellValue::Integer(1_234_567_890_123_456_789);
        let neighbour = CellValue::Integer(1_234_567_890_123_456_788);
        assert!(model.row_passes(std::slice::from_ref(&exact), &kinds, &fmts, &nulls));
        assert!(
            !model.row_passes(std::slice::from_ref(&neighbour), &kinds, &fmts, &nulls),
            "an f64 comparison would match the neighbour too"
        );
        assert!(in_list.row_passes(std::slice::from_ref(&exact), &kinds, &fmts, &nulls));
        assert!(!in_list.row_passes(std::slice::from_ref(&neighbour), &kinds, &fmts, &nulls));
        // Small integers keep the fast path.
        let small = and_model(vec![row(0, BuilderOp::Eq, "42", "")]);
        assert!(small.to_column_filters(&grid_columns, &nulls).is_some());
    }

    #[test]
    fn number_literal_accepts_plain_decimals_only() {
        assert_eq!(number_literal(" 42 ").as_deref(), Some("42"));
        assert_eq!(number_literal("+5").as_deref(), Some("5"));
        assert_eq!(number_literal("-3.25").as_deref(), Some("-3.25"));
        assert_eq!(number_literal(".5").as_deref(), Some(".5"));
        assert_eq!(number_literal("1e6").as_deref(), Some("1e6"));
        for bad in [
            "", "abc", "inf", "NaN", "1;DROP", "0x10", "1e", ".", "--1", "1 2",
        ] {
            assert_eq!(number_literal(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn and_row_narrows_an_or_root_instead_of_widening_it() {
        let mut model = FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    FilterItem::Row(row(0, BuilderOp::Is, "a", "")),
                    FilterItem::Row(row(0, BuilderOp::Is, "b", "")),
                ],
            },
        };
        model.and_row(row(1, BuilderOp::Is, "x", ""));
        assert_eq!(model.root.conjunction, Conjunction::And);
        assert_eq!(model.root.items.len(), 2);
        assert!(matches!(
            &model.root.items[0],
            FilterItem::Group(g) if g.conjunction == Conjunction::Or && g.items.len() == 2
        ));
        let kinds = kinds_text(2);
        let grid_columns = vec![col("c0", ColumnKind::Text), col("c1", ColumnKind::Text)];
        let fmts = GridConfig::default().resolve_all(&grid_columns);
        let nulls = vec!["NULL".to_string(); 2];
        let cells =
            |c0: &str, c1: &str| vec![CellValue::Text(c0.into()), CellValue::Text(c1.into())];
        assert!(model.row_passes(&cells("a", "x"), &kinds, &fmts, &nulls));
        assert!(
            !model.row_passes(&cells("a", "y"), &kinds, &fmts, &nulls),
            "the quick filter narrows"
        );
        assert!(!model.row_passes(&cells("c", "x"), &kinds, &fmts, &nulls));
        // On an AND root it is a plain push.
        let mut flat = and_model(vec![row(0, BuilderOp::Is, "a", "")]);
        flat.and_row(row(1, BuilderOp::Is, "x", ""));
        assert_eq!(flat.root.items.len(), 2);
        assert!(flat.is_flat_and());
    }

    /// A tree nested two levels deep, as the expression parser builds for
    /// `status = a created != 2026-09-28, qty = 1`.
    fn deep_model() -> FilterBuilderModel {
        FilterBuilderModel {
            root: FilterGroup {
                conjunction: Conjunction::Or,
                items: vec![
                    FilterItem::Group(FilterGroup {
                        conjunction: Conjunction::And,
                        items: vec![
                            FilterItem::Row(row(0, BuilderOp::Is, "a", "")),
                            FilterItem::Group(FilterGroup {
                                conjunction: Conjunction::Or,
                                items: vec![
                                    FilterItem::Row(row(1, BuilderOp::Lt, "2026-09-28", "")),
                                    FilterItem::Row(row(1, BuilderOp::Ge, "2026-09-29", "")),
                                ],
                            }),
                        ],
                    }),
                    FilterItem::Row(row(2, BuilderOp::Eq, "1", "")),
                ],
            },
        }
    }

    #[test]
    fn layout_renders_every_level_with_aligned_leaf_indices() {
        let model = deep_model();
        let layout = model.layout();
        let rows: Vec<(usize, FilterRow)> = layout
            .iter()
            .filter_map(|item| match item {
                LayoutItem::Row { leaf, row } => Some((*leaf, row.clone())),
                _ => None,
            })
            .collect();
        // Every leaf is rendered — including the doubly nested ones — and
        // each rendered row carries the index the model edits by.
        assert_eq!(rows.len(), model.leaf_count());
        let flat: Vec<FilterRow> = model.flat_rows().into_iter().cloned().collect();
        for (leaf, rendered) in &rows {
            assert_eq!(&flat[*leaf], rendered, "leaf {leaf} is the row shown there");
        }
        let starts: Vec<Vec<usize>> = layout
            .iter()
            .filter_map(|item| match item {
                LayoutItem::GroupStart { path } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![vec![0], vec![0, 1]]);
        assert!(model.group_at(&[0, 1]).is_some());

        // Editing / deleting through a rendered index hits the row shown.
        let mut edited = model.clone();
        let (last_nested, _) = rows[2].clone();
        edited.leaf_row_mut(last_nested).unwrap().a = "2026-10-01".into();
        assert_eq!(edited.flat_rows()[2].a, "2026-10-01");
        assert_eq!(edited.flat_rows()[3].a, "1", "the root row is untouched");
        edited.remove_leaf(rows[3].0);
        assert_eq!(edited.leaf_count(), 3);
        assert!(edited.flat_rows().iter().all(|r| r.a != "1"));
    }

    #[test]
    fn nested_group_actions_address_groups_by_path() {
        let mut model = deep_model();
        let kinds = kinds_text(3);
        model.add_row_in_group(&[0, 1], &kinds);
        assert_eq!(model.group_at(&[0, 1]).unwrap().items.len(), 3);
        model.set_group_conjunction(&[0, 1], Conjunction::And);
        assert_eq!(
            model.group_at(&[0, 1]).unwrap().conjunction,
            Conjunction::And
        );
        model.remove_group(&[0, 1]);
        assert_eq!(model.group_at(&[0]).unwrap().items.len(), 1);
        assert_eq!(model.leaf_count(), 2);
        // Paths that do not name a group are ignored.
        model.remove_group(&[1]);
        model.add_row_in_group(&[], &kinds);
        assert_eq!(model.leaf_count(), 2);
    }

    #[test]
    fn serde_round_trip_preserves_enabled_flag() {
        let model = and_model(vec![
            row(0, BuilderOp::Contains, "ab", ""),
            disabled_row(1, BuilderOp::Between, "1", "5"),
        ]);
        let json = serde_json::to_string(&model).unwrap();
        let back: FilterBuilderModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model, back);
        assert!(!back.flat_rows()[1].enabled);
    }

    #[test]
    fn serde_missing_enabled_defaults_to_true() {
        let json = r#"{"root":{"conjunction":"And","items":[{"Row":{"column":{"Index":0},"op":"Contains","a":"x","b":""}}]}}"#;
        let model: FilterBuilderModel = serde_json::from_str(json).unwrap();
        assert!(model.flat_rows()[0].enabled);
    }
}
