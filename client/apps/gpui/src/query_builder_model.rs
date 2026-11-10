//! Pure visual-query-builder state and SQL generation.
//!
//! No gpui: table/join/column/filter state maps to one SELECT per dialect.
//! The overlay view formats the one-line result; this module never executes SQL.

use sqlly_core::ObjectKind;
use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject, FkRelation};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::grid_filter_builder::{BuilderOp, predicate_sql};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inner => "INNER",
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
            Self::Full => "FULL",
        }
    }

    fn cycle(self) -> Self {
        match self {
            Self::Inner => Self::Left,
            Self::Left => Self::Right,
            Self::Right => Self::Full,
            Self::Full => Self::Inner,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    None,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Aggregate {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::None => Self::Count,
            Self::Count => Self::Sum,
            Self::Sum => Self::Avg,
            Self::Avg => Self::Min,
            Self::Min => Self::Max,
            Self::Max => Self::None,
        }
    }

    fn wrap(self, expr: &str) -> String {
        match self {
            Self::None => expr.to_string(),
            Self::Count => format!("COUNT({expr})"),
            Self::Sum => format!("SUM({expr})"),
            Self::Avg => format!("AVG({expr})"),
            Self::Min => format!("MIN({expr})"),
            Self::Max => format!("MAX({expr})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub alias: String,
    pub columns: Vec<String>,
    pub position: (f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub from: usize,
    pub from_column: String,
    pub to: usize,
    pub to_column: String,
    pub kind: JoinKind,
    pub from_fk: bool,
    /// `Some(probability)` when this join was inferred by the decision model
    /// rather than declared (FK) or drawn by the user. `from_fk: false` alone
    /// already means "user-drawn", so inferred joins carry their own marker.
    pub inferred: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedColumn {
    pub table: usize,
    pub column: String,
    pub aggregate: Aggregate,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub table: usize,
    pub column: String,
    pub op: BuilderOp,
    pub a: String,
    pub b: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub table: usize,
    pub column: String,
    pub dir: SortDir,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryBuilderState {
    pub tables: Vec<TableRef>,
    pub joins: Vec<Join>,
    pub columns: Vec<SelectedColumn>,
    pub conditions: Vec<Condition>,
    pub group_by: Vec<(usize, String)>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u64>,
    pub distinct: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    NoTables,
    NoColumns,
    DisconnectedTable(String),
    EmptyGroupByWithAggregates,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTables => {
                f.write_str(&crate::localization::text("query.builder.error_no_tables"))
            }
            Self::NoColumns => {
                f.write_str(&crate::localization::text("query.builder.error_no_columns"))
            }
            Self::DisconnectedTable(alias) => f.write_str(&crate::localization::format(
                "query.builder.error_disconnected_table",
                &[("alias", alias)],
            )),
            Self::EmptyGroupByWithAggregates => {
                f.write_str(&crate::localization::text("query.builder.error_group_by"))
            }
        }
    }
}

impl QueryBuilderState {
    /// Add a catalog table (dedupe by schema+name → returns existing index);
    /// alias `t{n}`; positions cascade (x += 260, wrap every 4).
    pub fn add_table(&mut self, obj: &CompletionObject) -> usize {
        let schema = obj.schema.clone().unwrap_or_default();
        if let Some(i) = self
            .tables
            .iter()
            .position(|t| t.schema == schema && t.name == obj.name)
        {
            return i;
        }
        let n = self.tables.len();
        let col = n % 4;
        let row = n / 4;
        let alias = next_alias(&self.tables);
        self.tables.push(TableRef {
            schema,
            name: obj.name.clone(),
            alias,
            columns: obj.columns.clone(),
            position: (col as f32 * 260.0, row as f32 * 280.0),
        });
        n
    }

    pub fn remove_table(&mut self, index: usize) {
        if index >= self.tables.len() {
            return;
        }
        self.tables.remove(index);
        self.joins.retain(|j| j.from != index && j.to != index);
        self.columns.retain(|c| c.table != index);
        self.conditions.retain(|c| c.table != index);
        self.group_by.retain(|(t, _)| *t != index);
        self.order_by.retain(|o| o.table != index);
        let reindex = |t: &mut usize| {
            if *t > index {
                *t -= 1;
            }
        };
        for j in &mut self.joins {
            reindex(&mut j.from);
            reindex(&mut j.to);
        }
        for c in &mut self.columns {
            reindex(&mut c.table);
        }
        for c in &mut self.conditions {
            reindex(&mut c.table);
        }
        for (t, _) in &mut self.group_by {
            reindex(t);
        }
        for o in &mut self.order_by {
            reindex(&mut o.table);
        }
    }

    pub fn toggle_column(&mut self, table: usize, column: &str) {
        if let Some(i) = self
            .columns
            .iter()
            .position(|c| c.table == table && c.column == column && c.aggregate == Aggregate::None)
        {
            let removed = self.columns.remove(i);
            self.group_by
                .retain(|(t, col)| !(*t == removed.table && col == &removed.column));
            return;
        }
        self.columns.push(SelectedColumn {
            table,
            column: column.to_string(),
            aggregate: Aggregate::None,
            alias: None,
        });
    }

    pub fn column_selected(&self, table: usize, column: &str) -> bool {
        self.columns
            .iter()
            .any(|c| c.table == table && c.column == column)
    }

    #[allow(dead_code)]
    pub fn set_join_kind(&mut self, join: usize, kind: JoinKind) {
        if let Some(j) = self.joins.get_mut(join) {
            j.kind = kind;
        }
    }

    pub fn cycle_join_kind(&mut self, join: usize) {
        if let Some(j) = self.joins.get_mut(join) {
            j.kind = j.kind.cycle();
        }
    }

    pub fn to_sql(&self, db: DatabaseType) -> Result<String, BuildError> {
        if self.tables.is_empty() {
            return Err(BuildError::NoTables);
        }
        if self.columns.is_empty() {
            return Err(BuildError::NoColumns);
        }
        if let Some(alias) = disconnected_alias(self) {
            return Err(BuildError::DisconnectedTable(alias));
        }
        if has_incomplete_group_by(self) {
            return Err(BuildError::EmptyGroupByWithAggregates);
        }

        let mut sql = String::from("SELECT ");
        if self.distinct {
            sql.push_str("DISTINCT ");
        }
        if let Some(n) = self.limit
            && uses_top(db)
        {
            sql.push_str(&format!("TOP ({n}) "));
        }
        let col_sql: Vec<String> = self
            .columns
            .iter()
            .map(|c| render_selected(db, self, c))
            .collect();
        sql.push_str(&col_sql.join(", "));

        let t0 = &self.tables[0];
        sql.push_str(" FROM ");
        sql.push_str(&table_source(db, t0));

        let mysql_full = db.connection_kind() == ConnectionKind::MySqlWire;
        for join in &self.joins {
            let (kind_sql, comment) = if join.kind == JoinKind::Full && mysql_full {
                ("LEFT JOIN", " /* FULL JOIN is not supported on MySQL */")
            } else {
                (join_keyword(join.kind), "")
            };
            let Some(from_t) = self.tables.get(join.from) else {
                continue;
            };
            let Some(to_t) = self.tables.get(join.to) else {
                continue;
            };
            // FROM is always table 0. JOIN the other endpoint of this edge.
            let join_table = if join.from == 0 || (join.to != 0 && join.from <= join.to) {
                to_t
            } else {
                from_t
            };
            sql.push(' ');
            sql.push_str(kind_sql);
            sql.push(' ');
            sql.push_str(&table_source(db, join_table));
            sql.push_str(" ON ");
            sql.push_str(&qualify_col(db, from_t, &join.from_column));
            sql.push_str(" = ");
            sql.push_str(&qualify_col(db, to_t, &join.to_column));
            sql.push_str(comment);
        }

        let mut where_parts = Vec::new();
        for cond in &self.conditions {
            let Some(table) = self.tables.get(cond.table) else {
                continue;
            };
            let expr = qualify_col(db, table, &cond.column);
            if let Some(pred) = predicate_sql(db, &expr, cond.op, &cond.a, &cond.b) {
                where_parts.push(pred);
            }
        }
        if !where_parts.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_parts.join(" AND "));
        }

        if !self.group_by.is_empty() {
            let groups: Vec<String> = self
                .group_by
                .iter()
                .filter_map(|(t, col)| self.tables.get(*t).map(|table| qualify_col(db, table, col)))
                .collect();
            if !groups.is_empty() {
                sql.push_str(" GROUP BY ");
                sql.push_str(&groups.join(", "));
            }
        }

        if !self.order_by.is_empty() {
            let orders: Vec<String> = self
                .order_by
                .iter()
                .filter_map(|o| {
                    let table = self.tables.get(o.table)?;
                    let expr = order_expr(db, self, table, o);
                    Some(format!("{expr} {}", o.dir.sql()))
                })
                .collect();
            if !orders.is_empty() {
                sql.push_str(" ORDER BY ");
                sql.push_str(&orders.join(", "));
            }
        }

        if let Some(n) = self.limit
            && !uses_top(db)
        {
            sql.push_str(&format!(" LIMIT {n}"));
        }

        Ok(sql)
    }
}

fn uses_top(db: DatabaseType) -> bool {
    db.connection_kind() == ConnectionKind::Tds
}

fn join_keyword(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "INNER JOIN",
        JoinKind::Left => "LEFT JOIN",
        JoinKind::Right => "RIGHT JOIN",
        JoinKind::Full => "FULL JOIN",
    }
}

fn table_source(db: DatabaseType, table: &TableRef) -> String {
    let name = if table.schema.is_empty() {
        db.quote_identifier(&table.name)
    } else {
        format!(
            "{}.{}",
            db.quote_identifier(&table.schema),
            db.quote_identifier(&table.name)
        )
    };
    format!("{} AS {}", name, db.quote_identifier(&table.alias))
}

fn qualify_col(db: DatabaseType, table: &TableRef, column: &str) -> String {
    format!(
        "{}.{}",
        db.quote_identifier(&table.alias),
        db.quote_identifier(column)
    )
}

fn render_selected(db: DatabaseType, state: &QueryBuilderState, col: &SelectedColumn) -> String {
    let Some(table) = state.tables.get(col.table) else {
        return String::new();
    };
    let inner = if col.aggregate == Aggregate::Count && col.column == "*" {
        "*".to_string()
    } else {
        qualify_col(db, table, &col.column)
    };
    let mut expr = col.aggregate.wrap(&inner);
    if let Some(alias) = col.alias.as_deref().filter(|a| !a.is_empty()) {
        expr.push_str(" AS ");
        expr.push_str(&db.quote_identifier(alias));
    }
    expr
}

fn order_expr(
    db: DatabaseType,
    state: &QueryBuilderState,
    table: &TableRef,
    order: &OrderBy,
) -> String {
    if let Some(sel) = state
        .columns
        .iter()
        .find(|c| c.table == order.table && c.column == order.column)
        && let Some(alias) = sel.alias.as_deref().filter(|a| !a.is_empty())
    {
        return db.quote_identifier(alias);
    }
    qualify_col(db, table, &order.column)
}

fn has_incomplete_group_by(state: &QueryBuilderState) -> bool {
    let any_agg = state.columns.iter().any(|c| c.aggregate != Aggregate::None);
    if !any_agg {
        return false;
    }
    state.columns.iter().any(|c| {
        c.aggregate == Aggregate::None
            && !state
                .group_by
                .iter()
                .any(|(t, col)| *t == c.table && col == &c.column)
    })
}

fn disconnected_alias(state: &QueryBuilderState) -> Option<String> {
    if state.tables.len() <= 1 {
        return None;
    }
    let n = state.tables.len();
    let mut seen = vec![false; n];
    let mut stack = vec![0usize];
    seen[0] = true;
    while let Some(i) = stack.pop() {
        for j in &state.joins {
            let other = if j.from == i {
                Some(j.to)
            } else if j.to == i {
                Some(j.from)
            } else {
                None
            };
            if let Some(o) = other
                && o < n
                && !seen[o]
            {
                seen[o] = true;
                stack.push(o);
            }
        }
    }
    seen.iter()
        .position(|s| !s)
        .map(|i| state.tables[i].alias.clone())
}

fn next_alias(tables: &[TableRef]) -> String {
    let mut n = 1usize;
    loop {
        let candidate = format!("t{n}");
        if tables.iter().all(|t| t.alias != candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn object_schema(obj: &CompletionObject) -> String {
    obj.schema.clone().unwrap_or_default()
}

fn resolve_fk_target(fk: &FkRelation, referencing_schema: &str) -> (String, String) {
    let schema = if fk.target_schema.is_empty() {
        referencing_schema.to_string()
    } else {
        fk.target_schema.clone()
    };
    (schema, fk.target_table.clone())
}

fn same_table(obj: &CompletionObject, schema: &str, name: &str) -> bool {
    object_schema(obj) == schema && obj.name == name
}

fn find_table_index(state: &QueryBuilderState, schema: &str, name: &str) -> Option<usize> {
    state
        .tables
        .iter()
        .position(|t| t.schema == schema && t.name == name)
}

pub(crate) fn join_dup(existing: &[Join], join: &Join) -> bool {
    existing.iter().any(|j| {
        (j.from == join.from
            && j.to == join.to
            && j.from_column == join.from_column
            && j.to_column == join.to_column)
            || (j.from == join.to
                && j.to == join.from
                && j.from_column == join.to_column
                && j.to_column == join.from_column)
    })
}

/// Every FK relation between `new_table` and the tables already in `state`,
/// both directions, as ready `Join`s (kind Inner, from_fk true). An empty
/// `target_schema` resolves to the referencing object's schema.
pub fn suggest_joins(
    catalog: &CompletionCatalog,
    state: &QueryBuilderState,
    new_table: usize,
) -> Vec<Join> {
    let Some(added) = state.tables.get(new_table) else {
        return Vec::new();
    };
    let Some(added_obj) = catalog
        .objects
        .iter()
        .find(|o| same_table(o, &added.schema, &added.name))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Outbound FKs on the newly added table.
    for fk in &added_obj.foreign_keys {
        let (ts, tn) = resolve_fk_target(fk, &added.schema);
        if let Some(to) = find_table_index(state, &ts, &tn)
            && to != new_table
        {
            let join = Join {
                from: new_table,
                from_column: fk.column.clone(),
                to,
                to_column: fk.target_column.clone(),
                kind: JoinKind::Inner,
                from_fk: true,
                inferred: None,
            };
            if !join_dup(&out, &join) && !join_dup(&state.joins, &join) {
                out.push(join);
            }
        }
    }
    // Inbound FKs: existing tables pointing at the new table.
    for (i, table) in state.tables.iter().enumerate() {
        if i == new_table {
            continue;
        }
        let Some(obj) = catalog
            .objects
            .iter()
            .find(|o| same_table(o, &table.schema, &table.name))
        else {
            continue;
        };
        for fk in &obj.foreign_keys {
            let (ts, tn) = resolve_fk_target(fk, &table.schema);
            if ts == added.schema && tn == added.name {
                let join = Join {
                    from: i,
                    from_column: fk.column.clone(),
                    to: new_table,
                    to_column: fk.target_column.clone(),
                    kind: JoinKind::Inner,
                    from_fk: true,
                    inferred: None,
                };
                if !join_dup(&out, &join) && !join_dup(&state.joins, &join) {
                    out.push(join);
                }
            }
        }
    }
    out
}

/// Tables/views from the catalog, sorted by schema then name, filtered by a
/// case-insensitive substring on `schema.name`.
pub fn list_tables<'a>(catalog: &'a CompletionCatalog, filter: &str) -> Vec<&'a CompletionObject> {
    let needle = filter.trim().to_lowercase();
    let mut out: Vec<&CompletionObject> = catalog
        .objects
        .iter()
        .filter(|o| matches!(o.kind, ObjectKind::Table | ObjectKind::View))
        .filter(|o| {
            if needle.is_empty() {
                return true;
            }
            let schema = o.schema.as_deref().unwrap_or("");
            let key = format!("{schema}.{}", o.name);
            key.to_lowercase().contains(&needle)
        })
        .collect();
    out.sort_by(|a, b| {
        let sa = a.schema.as_deref().unwrap_or("");
        let sb = b.schema.as_deref().unwrap_or("");
        sa.cmp(sb).then_with(|| a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customers() -> CompletionObject {
        CompletionObject::table(
            "dbo",
            "Customers",
            vec!["Id".into(), "CustomerId".into(), "Name".into()],
        )
    }

    fn orders() -> CompletionObject {
        let mut obj = CompletionObject::table(
            "dbo",
            "Orders",
            vec!["Id".into(), "CustomerId".into(), "Total".into()],
        );
        obj.foreign_keys.push(FkRelation {
            column: "CustomerId".into(),
            target_schema: "dbo".into(),
            target_table: "Customers".into(),
            target_column: "Id".into(),
            constraint: None,
        });
        obj
    }

    fn products() -> CompletionObject {
        CompletionObject::table("dbo", "Products", vec!["Id".into(), "Sku".into()])
    }

    fn catalog_two() -> CompletionCatalog {
        CompletionCatalog {
            objects: vec![customers(), orders()],
            ..Default::default()
        }
    }

    fn catalog_three() -> CompletionCatalog {
        CompletionCatalog {
            objects: vec![customers(), orders(), products()],
            ..Default::default()
        }
    }

    fn golden_state() -> QueryBuilderState {
        let catalog = catalog_two();
        let mut state = QueryBuilderState::default();
        state.add_table(&catalog.objects[0]);
        let i = state.add_table(&catalog.objects[1]);
        for join in suggest_joins(&catalog, &state, i) {
            state.joins.push(join);
        }
        state.set_join_kind(0, JoinKind::Left);
        state.toggle_column(0, "CustomerId");
        state.toggle_column(0, "Name");
        state.columns.push(SelectedColumn {
            table: 1,
            column: "*".into(),
            aggregate: Aggregate::Count,
            alias: Some("OrderCount".into()),
        });
        state.conditions.push(Condition {
            table: 0,
            column: "Name".into(),
            op: BuilderOp::Contains,
            a: "ann".into(),
            b: String::new(),
        });
        state.conditions.push(Condition {
            table: 1,
            column: "Total".into(),
            op: BuilderOp::Ge,
            a: "10".into(),
            b: String::new(),
        });
        state.group_by = vec![(0, "CustomerId".into()), (0, "Name".into())];
        state.order_by.push(OrderBy {
            table: 1,
            column: "*".into(),
            dir: SortDir::Desc,
        });
        state.limit = Some(50);
        state
    }

    #[test]
    fn add_table_dedupes_and_aliases() {
        let mut state = QueryBuilderState::default();
        let c = customers();
        let i0 = state.add_table(&c);
        let i1 = state.add_table(&c);
        assert_eq!(i0, 0);
        assert_eq!(i1, 0);
        assert_eq!(state.tables.len(), 1);
        assert_eq!(state.tables[0].alias, "t1");
        let i2 = state.add_table(&orders());
        assert_eq!(i2, 1);
        assert_eq!(state.tables[1].alias, "t2");
    }

    #[test]
    fn suggest_joins_outbound_and_inbound_fk() {
        let catalog = catalog_two();
        let mut state = QueryBuilderState::default();
        state.add_table(&catalog.objects[0]);
        let i = state.add_table(&catalog.objects[1]);
        let joins = suggest_joins(&catalog, &state, i);
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].from, 1);
        assert_eq!(joins[0].from_column, "CustomerId");
        assert_eq!(joins[0].to, 0);
        assert_eq!(joins[0].to_column, "Id");
        assert!(joins[0].from_fk);
        assert_eq!(joins[0].kind, JoinKind::Inner);

        let mut inbound = QueryBuilderState::default();
        inbound.add_table(&catalog.objects[1]);
        let i = inbound.add_table(&catalog.objects[0]);
        let joins = suggest_joins(&catalog, &inbound, i);
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].from, 0);
        assert_eq!(joins[0].to, 1);
    }

    #[test]
    fn suggest_joins_resolves_empty_target_schema() {
        let mut orders =
            CompletionObject::table("dbo", "Orders", vec!["Id".into(), "CustomerId".into()]);
        orders.foreign_keys.push(FkRelation {
            column: "CustomerId".into(),
            target_schema: String::new(),
            target_table: "Customers".into(),
            target_column: "Id".into(),
            constraint: None,
        });
        let catalog = CompletionCatalog {
            objects: vec![customers(), orders],
            ..Default::default()
        };
        let mut state = QueryBuilderState::default();
        state.add_table(&catalog.objects[0]);
        let i = state.add_table(&catalog.objects[1]);
        let joins = suggest_joins(&catalog, &state, i);
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].to, 0);
        assert_eq!(joins[0].to_column, "Id");
    }

    #[test]
    fn remove_table_reindexes_everything() {
        let catalog = catalog_three();
        let mut state = QueryBuilderState::default();
        state.add_table(&catalog.objects[0]);
        let i1 = state.add_table(&catalog.objects[1]);
        let i2 = state.add_table(&catalog.objects[2]);
        state.joins.push(Join {
            from: i1,
            from_column: "CustomerId".into(),
            to: 0,
            to_column: "Id".into(),
            kind: JoinKind::Inner,
            from_fk: true,
            inferred: None,
        });
        state.joins.push(Join {
            from: i2,
            from_column: "Id".into(),
            to: 0,
            to_column: "Id".into(),
            kind: JoinKind::Inner,
            from_fk: false,
            inferred: None,
        });
        state.toggle_column(0, "Name");
        state.toggle_column(i1, "Total");
        state.toggle_column(i2, "Sku");
        state.conditions.push(Condition {
            table: i1,
            column: "Total".into(),
            op: BuilderOp::Ge,
            a: "1".into(),
            b: String::new(),
        });
        state.group_by.push((i2, "Sku".into()));
        state.order_by.push(OrderBy {
            table: i1,
            column: "Total".into(),
            dir: SortDir::Asc,
        });
        state.remove_table(i1);
        assert_eq!(state.tables.len(), 2);
        assert_eq!(state.tables[0].name, "Customers");
        assert_eq!(state.tables[1].name, "Products");
        assert_eq!(state.joins.len(), 1);
        assert_eq!(state.joins[0].from, 1);
        assert_eq!(state.joins[0].to, 0);
        assert_eq!(state.columns.len(), 2);
        assert!(state.columns.iter().all(|c| c.table < 2));
        assert!(state.conditions.is_empty());
        assert_eq!(state.group_by, vec![(1, "Sku".into())]);
        assert!(state.order_by.is_empty());
    }

    #[test]
    fn to_sql_requires_tables_and_columns() {
        let state = QueryBuilderState::default();
        assert_eq!(
            state.to_sql(DatabaseType::Postgres),
            Err(BuildError::NoTables)
        );
        let mut state = QueryBuilderState::default();
        state.add_table(&customers());
        assert_eq!(
            state.to_sql(DatabaseType::Postgres),
            Err(BuildError::NoColumns)
        );
    }

    #[test]
    fn to_sql_disconnected_table_errors() {
        let mut state = QueryBuilderState::default();
        state.add_table(&customers());
        state.add_table(&products());
        state.toggle_column(0, "Name");
        match state.to_sql(DatabaseType::Postgres) {
            Err(BuildError::DisconnectedTable(alias)) => assert_eq!(alias, "t2"),
            other => panic!("expected disconnected, got {other:?}"),
        }
    }

    #[test]
    fn to_sql_group_by_validation() {
        let mut state = QueryBuilderState::default();
        state.add_table(&customers());
        state.toggle_column(0, "Name");
        state.columns.push(SelectedColumn {
            table: 0,
            column: "*".into(),
            aggregate: Aggregate::Count,
            alias: None,
        });
        assert_eq!(
            state.to_sql(DatabaseType::Postgres),
            Err(BuildError::EmptyGroupByWithAggregates)
        );
        state.group_by.push((0, "Name".into()));
        assert!(state.to_sql(DatabaseType::Postgres).is_ok());
    }

    #[test]
    fn golden_sqlserver() {
        let sql = golden_state().to_sql(DatabaseType::SqlServer).expect("sql");
        assert_eq!(
            sql,
            "SELECT TOP (50) [t1].[CustomerId], [t1].[Name], COUNT(*) AS [OrderCount] FROM [dbo].[Customers] AS [t1] LEFT JOIN [dbo].[Orders] AS [t2] ON [t2].[CustomerId] = [t1].[Id] WHERE [t1].[Name] LIKE N'%ann%' AND [t2].[Total] >= 10 GROUP BY [t1].[CustomerId], [t1].[Name] ORDER BY [OrderCount] DESC"
        );
    }

    #[test]
    fn golden_postgres() {
        let sql = golden_state().to_sql(DatabaseType::Postgres).expect("sql");
        assert_eq!(
            sql,
            "SELECT \"t1\".\"CustomerId\", \"t1\".\"Name\", COUNT(*) AS \"OrderCount\" FROM \"dbo\".\"Customers\" AS \"t1\" LEFT JOIN \"dbo\".\"Orders\" AS \"t2\" ON \"t2\".\"CustomerId\" = \"t1\".\"Id\" WHERE \"t1\".\"Name\" LIKE '%ann%' AND \"t2\".\"Total\" >= 10 GROUP BY \"t1\".\"CustomerId\", \"t1\".\"Name\" ORDER BY \"OrderCount\" DESC LIMIT 50"
        );
    }

    #[test]
    fn golden_mysql() {
        let sql = golden_state().to_sql(DatabaseType::MySql).expect("sql");
        assert_eq!(
            sql,
            "SELECT `t1`.`CustomerId`, `t1`.`Name`, COUNT(*) AS `OrderCount` FROM `dbo`.`Customers` AS `t1` LEFT JOIN `dbo`.`Orders` AS `t2` ON `t2`.`CustomerId` = `t1`.`Id` WHERE `t1`.`Name` LIKE '%ann%' AND `t2`.`Total` >= 10 GROUP BY `t1`.`CustomerId`, `t1`.`Name` ORDER BY `OrderCount` DESC LIMIT 50"
        );
    }

    #[test]
    fn golden_sqlite() {
        let sql = golden_state().to_sql(DatabaseType::Sqlite).expect("sql");
        assert_eq!(
            sql,
            "SELECT \"t1\".\"CustomerId\", \"t1\".\"Name\", COUNT(*) AS \"OrderCount\" FROM \"dbo\".\"Customers\" AS \"t1\" LEFT JOIN \"dbo\".\"Orders\" AS \"t2\" ON \"t2\".\"CustomerId\" = \"t1\".\"Id\" WHERE \"t1\".\"Name\" LIKE '%ann%' AND \"t2\".\"Total\" >= 10 GROUP BY \"t1\".\"CustomerId\", \"t1\".\"Name\" ORDER BY \"OrderCount\" DESC LIMIT 50"
        );
    }

    #[test]
    fn golden_mysql_full_join_falls_back() {
        let mut state = golden_state();
        state.set_join_kind(0, JoinKind::Full);
        let sql = state.to_sql(DatabaseType::MySql).expect("sql");
        assert!(sql.contains("LEFT JOIN"), "{sql}");
        assert!(
            sql.contains("/* FULL JOIN is not supported on MySQL */"),
            "{sql}"
        );
        assert!(!sql.contains("FULL JOIN `"), "{sql}");
    }
}
